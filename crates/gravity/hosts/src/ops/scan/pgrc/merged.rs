//! sortmerge T2 (D4469 road 2): a merged range site's source. A pgrc part holds its rows in its table's `sorted_by` key
//! order (the seal's cluster sort), but only inside itself: a site the plan relies on for that order (`SiteDesc::merge`)
//! k-way merges the sorted streams of the statement snapshot's parts, so its rows come in key order at any part count.
//! - Streams: one per part of the bound list (the statement's `Resolved`). Each part's sort witness must lead with the
//!   merge key (`part_sorted_on`): a part without one is the statement's error, never rows out of order.
//! - Live windows: a stream's current granule is the window of a `Member`, a granule source over this source's recipe
//!   (`GranuleRangeSource::member`) seeked to the rest of the stream's part. At most `live_max` members exist
//!   (MERGE_LIVE_BYTES cut into windows of the site's lanes, MERGE_LIVE_MAX at most), reserved in the Node's arena at its
//!   build and kept across statements as any source's buffers are; a stream without one keeps its head key and opens its
//!   granule again when it reaches the top.
//! - Runs: the ONE engine's heap (`pgrc_write::runs::Heap`) orders the streams by head key under the seal's comparator
//!   (`seal::cmp::slot_cmp`, ASC NULLS LAST), ties to the lower part. The top stream's rows before the first one that sorts
//!   after the runner-up's head are one run, published zero-copy: the member's window cut to `[cursor, end)`, or the slice
//!   of its selection. The next window call moves that stream on (to its next granule when the run ended its window).
//! - The merge state (the streams, their head cells, the heap's slots, an evicted head's key bytes) is the scan operator's:
//!   an `OpArena` owner each execution, so its bytes show under that owner. The members' windows are granule sources'
//!   buffers, held as every granule source's are (`kept_bytes`).
//! - Shape: the site is serial (one merge on the leader), whole-relation (a claim of part of it is an error), and its key
//!   lanes are values; a verdict that changes a window's rows or leaves a lane undecoded (late, packed, unread, count-only,
//!   code tables), an answer, an entries window or a walk order is refused at the bind, and the members get no filter-only
//!   key lane and no answer lane, so a key lane is always decoded.

use core::cmp::Ordering;

use gravity_abi::ctx::lane;
use gravity_abi::site::Site;
use gravity_abi::{Datum as Word, Lane};
use gravity_driver::{blank_site, ArenaRef, ArenaVec, BindingRef, Cursor, Error, Residency, Result, Source, Window, WindowBase};
use gravity_fn::arena::{AVec, Kind, OpArena, Refused};
use gravity_program::cost::pgrc::{MERGE_LANE_BYTES, MERGE_LIVE_BYTES, MERGE_LIVE_MAX};
use gravity_program::protos::{VERDICT_CODE_SPARSE, VERDICT_CODE_TABLE, VERDICT_CONST_KEY, VERDICT_COUNT_ONLY, VERDICT_COUNT_SELECT, VERDICT_ELIDE, VERDICT_ELIDE_HIT, VERDICT_GUARD_SEL, VERDICT_LATE, VERDICT_PACKED, VERDICT_PACKED_ZONE, VERDICT_UNREAD};
use gravity_program::{SiteDesc, ZonePred};
use pgrc_format::sortkey::{NullsOrder, SortDir};
use pgrc_write::runs::Heap;
use pgrc_write::seal::cmp::{slot_cmp, KeySlot};
use pgrc_write::structural::ClusterKeyDecl;

use super::granule_source::{part_sorted_on, GranuleRangeSource, GranuleRecipe, Resolved, VerdictCell};

mod raw;

/// The most merge keys (a `sorted_by` key's columns; PostgreSQL's INDEX_MAX_KEYS).
pub const MAX_KEYS: usize = gravity_program::limits::KEYS;

/// No stream / no member.
const NONE: u32 = u32::MAX;

/// The verdicts whose windows a merge cannot cut into runs: a lane left undecoded or filled late (late, packed, unread,
/// elision, count-only, guarded selections), rows that stand for codes (code tables) or a window's const-key hint. The
/// zone, bloom, PSMA and condition-cache verdicts only skip granules and select rows, which a run keeps.
const SHAPED: u64 = VERDICT_LATE | VERDICT_PACKED | VERDICT_UNREAD | VERDICT_PACKED_ZONE | VERDICT_ELIDE | VERDICT_ELIDE_HIT | VERDICT_CONST_KEY | VERDICT_GUARD_SEL | VERDICT_COUNT_ONLY | VERDICT_COUNT_SELECT | VERDICT_CODE_TABLE | VERDICT_CODE_SPARSE;

/// One key cell of a stream's head (or of a row): NULL, a by-value word, or a by-reference value's payload bytes.
#[derive(Clone, Copy)]
struct Cell {
    null: bool,
    w: u64,
    p: *const u8,
    n: u32,
}

impl Cell {
    const NULL: Cell = Cell { null: true, w: 0, p: core::ptr::null(), n: 0 };
}

/// One part's sorted stream.
struct Stream {
    /// Its part's first and end granules (global), and the granule its window is at (or its member seeks from).
    start: u64,
    end: u64,
    g: u64,
    /// The head: its row in the window's granule, and its index among the window's rows (its selection's when it has one).
    row: u32,
    at: u32,
    member: u32,
    done: bool,
    /// The head's by-reference key bytes while the stream has no member (`evict`).
    own: AVec<u8>,
}

/// A live window: its granule source and the site it fills.
struct Member {
    src: GranuleRangeSource,
    site: Site,
    stream: u32,
    /// The run clock at its last run: the least is evicted first.
    used: u64,
}

/// One execution's merge state, in the scan operator's arena.
struct Exec {
    arena: OpArena,
    streams: AVec<Stream>,
    /// `nk` cells per stream: its head's key.
    heads: AVec<Cell>,
    /// The heap's slots, the first `live` its streams with a row.
    slots: AVec<u32>,
    live: usize,
    /// The site's key cells as `seek` was handed them, the claim's two replaced per member seek.
    keys: AVec<Word>,
    nulls: AVec<u8>,
    /// The published run: its stream and the end of its rows (`settle` moves the stream there).
    run: Option<(u32, u32)>,
    clock: u64,
}

pub struct MergedRangeSource {
    /// The live windows (the first `nlive` bound this execution), reserved at build: `live_max` of them at most.
    members: ArenaVec<Member>,
    nlive: usize,
    /// The members' window lanes, `lp` each, and the row protocol's (`next`).
    lanes: ArenaVec<Lane>,
    scratch: ArenaVec<Lane>,
    lp: usize,
    live_max: usize,
    /// What `zone` / `zone_any` handed over at the Node's build, for each member: the predicates, the filter-only lanes
    /// less the key lanes (a filter-only lane may be left undecoded), no answer lane.
    zone: ArenaVec<ZonePred>,
    filter: ArenaVec<u64>,
    zoned: bool,
    any: ArenaVec<ZonePred>,
    any_ends: ArenaVec<u16>,
    any_base: usize,
    anyd: bool,
    /// The merge key: its attnos, their lanes (`u16::MAX`: not a lane of the site, refused at the bind), and whether each
    /// is a by-reference value (its payload bytes compare).
    keys: ArenaVec<i16>,
    key_lanes: ArenaVec<u16>,
    byref: ArenaVec<bool>,
    decl: ClusterKeyDecl,
    exec: Option<Exec>,
    cur: Cursor,
    site: usize,
    bound: bool,
    /// The members read it (`GranuleRangeSource::member`): declared last, so it drops after them.
    recipe: GranuleRecipe,
}

/// The live windows a merged site keeps at most: B_live cut into windows of its lanes, MERGE_LIVE_MAX at most, 2 at least.
pub fn live_max(desc: &SiteDesc) -> usize {
    (MERGE_LIVE_BYTES / (MERGE_LANE_BYTES * desc.natts().max(1))).clamp(2, MERGE_LIVE_MAX)
}

/// The lanes one member's window may publish (the site's lanes, its attribute lanes and an offset lane above them).
fn lanes_per(desc: &SiteDesc) -> usize {
    desc.nlanes() + desc.natts() + 2
}

/// The Node arena bytes a merged site's source takes at its build (`HostRegistry::size_hint`): the source, its members
/// and their lanes, the row protocol's lanes, the key and the zone lists, with alignment slack.
pub fn footprint(desc: &SiteDesc) -> usize {
    let (n, lp) = (live_max(desc), lanes_per(desc));
    let lane = core::mem::size_of::<Lane>();
    let zone = (desc.zone_preds().len() + desc.zone_any.len()) * core::mem::size_of::<ZonePred>() + desc.zone_any_ends.len() * 2 + desc.filter_lanes.words().len() * 8;
    core::mem::size_of::<MergedRangeSource>() + n * core::mem::size_of::<Member>() + (n + 1) * lp * lane + desc.merge.len() * 8 + zone + 256
}

fn refused(r: Refused) -> Error {
    Error::out_of_memory(r.bytes)
}

impl MergedRangeSource {
    /// The merged source of range site `site` (its descriptor `desc`, whose `merge` is the key) over `recipe`, its members'
    /// room in the Node's arena `a`.
    pub fn new(recipe: GranuleRecipe, site: usize, desc: &SiteDesc, a: ArenaRef) -> MergedRangeSource {
        let (live_max, lp) = (live_max(desc), lanes_per(desc));
        let natts = recipe.natts;
        let hint = (natts + 1 + recipe.tables.len()).max(recipe.verdict_lane.map_or(0, |k| k + 1));
        assert!(hint <= lp, "gravity: a merged site's recipe publishes {hint} lanes, its descriptor {lp}");
        let mut lanes = ArenaVec::with_capacity_in(live_max * lp, a);
        lanes.resize(live_max * lp, Lane::NONE);
        let mut scratch = ArenaVec::with_capacity_in(lp, a);
        scratch.resize(lp, Lane::NONE);
        let nk = desc.merge.len();
        let mut keys = ArenaVec::with_capacity_in(nk, a);
        keys.extend_from_slice(&desc.merge);
        let mut key_lanes = ArenaVec::with_capacity_in(nk, a);
        let mut byref = ArenaVec::with_capacity_in(nk, a);
        // bounded("key-columns: one lane per merge key column, at most MAX_KEYS, found among the site's lanes")
        for &attno in desc.merge.iter() {
            let k = recipe.attnos[..natts].iter().position(|&x| x == attno);
            key_lanes.push(k.map_or(u16::MAX, |k| k as u16));
            byref.push(k.is_some_and(|k| recipe.kinds[k] == lane::KIND_BYREF));
        }
        MergedRangeSource {
            members: ArenaVec::with_capacity_in(live_max, a),
            nlive: 0,
            lanes,
            scratch,
            lp,
            live_max,
            zone: ArenaVec::new_in(a),
            filter: ArenaVec::new_in(a),
            zoned: false,
            any: ArenaVec::new_in(a),
            any_ends: ArenaVec::new_in(a),
            any_base: 0,
            anyd: false,
            keys,
            key_lanes,
            byref,
            decl: ClusterKeyDecl { attno: 0, dir: SortDir::Asc, nulls: NullsOrder::Last, path: Vec::new() }, // alloc: Node
            exec: None,
            cur: Cursor::new(),
            site,
            bound: false,
            recipe,
        }
    }

    pub fn recipe(&self) -> &GranuleRecipe {
        &self.recipe
    }

    /// At most `n` live windows (at least one, at most the room the build reserved): a test's small pool.
    pub fn with_live(mut self, n: usize) -> MergedRangeSource {
        self.live_max = n.clamp(1, self.members.capacity());
        self
    }

    /// The live windows this execution binds (`min(parts, live_max)`).
    pub fn live(&self) -> usize {
        self.nlive
    }

    /// The granules this execution's members decoded (an evicted stream's granule decodes again when it reaches the top).
    pub fn granules_decoded(&self) -> u64 {
        self.members[..self.nlive].iter().map(|m| m.src.granules_decoded()).sum()
    }

    /// The refusals of the site's shape, before any member binds.
    fn shape(&self, resolved: &Resolved) -> Result<()> {
        let r = &self.recipe;
        let (enables, _) = r.verdicts.get();
        if enables != VerdictCell::LEGACY && enables & SHAPED != 0 {
            return Err(Error::host("gravity: a merged site's verdicts change its windows' rows (late, packed, unread, count-only or code-table verdicts)"));
        }
        if resolved.order_of(self.site).is_some() {
            return Err(Error::host("gravity: a merged site under a walk order"));
        }
        if r.entries.is_some() || r.entries_every || !r.tables.is_empty() || r.sample.is_some() || !r.shred.is_empty() {
            return Err(Error::host("gravity: a merged site with entries, tables, a sample or shred lanes"));
        }
        if self.keys.is_empty() || self.keys.len() > MAX_KEYS {
            return Err(Error::host("gravity: a merged site's key is empty or longer than INDEX_MAX_KEYS"));
        }
        // bounded("key-columns: one check per merge key column, at most MAX_KEYS")
        for &k in self.key_lanes.iter() {
            let ok = (k as usize) < r.natts && match r.kinds[k as usize] {
                lane::KIND_BYVAL => matches!(r.widths[k as usize], 2 | 4 | 8),
                lane::KIND_BYREF => true,
                _ => false,
            };
            if !ok {
                return Err(Error::host("gravity: a merged site's key is not a by-value integer or a by-reference value lane of the site"));
            }
        }
        Ok(())
    }

    fn bind_merge(&mut self, b: &BindingRef<'_>) -> Result<()> {
        let resolved = Resolved::of(b).map_err(crate::stash)?;
        self.shape(resolved)?;
        let list = resolved.list(self.recipe.rel).ok_or_else(|| Error::host("gravity: a merged site bound without its part list"))?;
        let k = list.len();
        // the witness: every part's rows are in the key's order (the seal sorted or verified them)
        // bounded("other: one sort witness per part of the statement's list, each a small section of an open part")
        for p in (0..k).filter(|&p| list.parts()[p].granules > 0) {
            if !part_sorted_on(resolved, &self.recipe, p, &self.keys).map_err(crate::stash)? {
                return Err(Error::host("gravity: a pgrcolumnar part holds no sort witness for its merged scan's key (D4469)"));
            }
        }
        let live = k.min(self.live_max);
        // the members made for an earlier execution read the recipe where it is now
        // bounded("other: the members the build reserved room for, at most MERGE_LIVE_MAX of them")
        for m in self.members.iter_mut() {
            m.src.relend(raw::lent(&self.recipe));
        }
        // the members this execution needs, each made once in the room the build reserved
        // bounded("other: at most live_max (MERGE_LIVE_MAX) members, made once per Node")
        while self.members.len() < live {
            let m = self.members.len();
            let lanes = raw::lanes_at(&mut self.lanes, m * self.lp);
            let mut src = GranuleRangeSource::member(raw::lent(&self.recipe)).at_site(self.site);
            if self.zoned {
                src.zone(&self.zone, &self.filter, &[]);
            }
            if self.anyd {
                src.zone_any(&self.any, &self.any_ends, self.any_base);
            }
            self.members.push(Member { src, site: blank_site(lanes), stream: NONE, used: 0 });
        }
        // bounded("other: one bind per member this execution needs, at most MERGE_LIVE_MAX of them")
        for i in 0..live {
            if let Err(e) = self.members[i].src.bind(b) {
                // bounded("other: the members bound before the failing one, at most live_max")
                for m in self.members[..i].iter_mut() {
                    m.src.unbind();
                }
                return Err(e);
            }
        }
        self.nlive = live;
        let nk = self.keys.len();
        let arena = OpArena::new(core::ptr::null_mut(), Kind::Sort, MERGE_LIVE_BYTES as u64);
        let state = (|| -> core::result::Result<(AVec<Stream>, AVec<Cell>, AVec<u32>), Refused> {
            let mut streams = AVec::with_capacity(&arena, k)?;
            // bounded("other: one stream per part of the statement's list")
            for p in 0..k {
                let (start, end) = (list.starts()[p], list.end_of(p));
                streams.push(&arena, Stream { start, end, g: start, row: 0, at: 0, member: NONE, done: start >= end, own: AVec::new() })?; // alloc: Exec
            }
            let mut heads = AVec::with_capacity(&arena, k * nk)?;
            heads.resize(&arena, k * nk, Cell::NULL)?;
            let mut slots = AVec::with_capacity(&arena, k)?;
            slots.resize(&arena, k, 0)?;
            Ok((streams, heads, slots))
        })();
        match state {
            Ok((streams, heads, slots)) => self.exec = Some(Exec { streams, heads, slots, live: 0, keys: AVec::new(), nulls: AVec::new(), run: None, clock: 0, arena }), // alloc: Exec
            Err(r) => {
                self.unbind_members();
                return Err(refused(r));
            }
        }
        self.cur.clear();
        self.bound = true;
        Ok(())
    }

    fn unbind_members(&mut self) {
        // bounded("other: the bound members, at most live_max (MERGE_LIVE_MAX)")
        for m in self.members[..self.nlive].iter_mut() {
            m.src.unbind();
            // a member keeps no part open across statements (nothing for `release_rel` to hand back later)
            drop(m.src.release_rel(0));
            m.stream = NONE;
        }
        self.nlive = 0;
    }

    /// Stream `s`'s head cells (`nk` of them).
    fn head(ex: &Exec, nk: usize, s: u32) -> &[Cell] {
        let h = ex.heads.as_slice(&ex.arena);
        &h[s as usize * nk..(s as usize + 1) * nk]
    }

    /// Row `row` of member `m`'s window as key cells into `out`; an error for a by-reference image of no plain form.
    fn row_cells(&self, m: usize, row: usize, out: &mut [Cell]) -> Result<()> {
        let site = &self.members[m].site;
        // bounded("key-columns: one cell per merge key column, at most MAX_KEYS")
        for (i, o) in out.iter_mut().enumerate() {
            let l = raw::site_lane(site, self.key_lanes[i] as usize);
            *o = raw::cell_at(&l, row, self.byref[i]).ok_or_else(|| Error::host("gravity: a merged key's value is not a plain varlena image"))?;
        }
        Ok(())
    }

    /// The order of two key cell lists under the seal's law.
    fn cmp(byref: &[bool], decl: &ClusterKeyDecl, a: &[Cell], b: &[Cell]) -> Ordering {
        // bounded("key-columns: one compare per merge key column, at most MAX_KEYS")
        for i in 0..a.len() {
            let (x, y) = (slot(&a[i], byref[i]), slot(&b[i], byref[i]));
            let o = slot_cmp(&x, &y, decl);
            if o != Ordering::Equal {
                return o;
            }
        }
        Ordering::Equal
    }

    /// The window rows of member `m`: its selection's length, else its rows.
    fn window_end(&self, m: usize) -> u32 {
        self.members[m].site.end()
    }

    /// The granule row behind index `at` of member `m`'s window.
    fn row_of(&self, m: usize, at: u32) -> u32 {
        raw::row_at(&self.members[m].site, at)
    }

    /// Stream `s`'s head from its member's window at index `at`, its cells loaded.
    fn load_head(&mut self, s: u32, at: u32) -> Result<()> {
        let nk = self.keys.len();
        let m = self.exec.as_ref().expect("bound").streams.as_slice(&self.exec.as_ref().expect("bound").arena)[s as usize].member as usize;
        let row = self.row_of(m, at);
        let mut cells = [Cell::NULL; MAX_KEYS];
        self.row_cells(m, row as usize, &mut cells[..nk])?;
        let ex = self.exec.as_mut().expect("bound");
        let st = &mut ex.streams.as_mut_slice(&ex.arena)[s as usize];
        st.at = at;
        st.row = row;
        ex.heads.as_mut_slice(&ex.arena)[s as usize * nk..(s as usize + 1) * nk].copy_from_slice(&cells[..nk]);
        Ok(())
    }

    /// Stream `s`'s head moved to its member's next window (`true`), or the stream ended (`false`).
    fn next_window(&mut self, s: u32) -> Result<bool> {
        let m = self.stream_member(s);
        let n = {
            let mem = &mut self.members[m];
            let mut w = Window::over(&mut mem.site);
            mem.src.window(&mut w)?
        };
        if n == 0 {
            self.end_stream(s);
            return Ok(false);
        }
        self.check_window(m)?;
        let g = self.members[m].site.pos.block as u64;
        let ex = self.exec.as_mut().expect("bound");
        let st = &mut ex.streams.as_mut_slice(&ex.arena)[s as usize];
        st.g = g;
        st.row = 0;
        self.load_head(s, 0)?;
        Ok(true)
    }

    /// A member's window as a merge reads it: rows, lanes decoded, never answered.
    fn check_window(&self, m: usize) -> Result<()> {
        let site = &self.members[m].site;
        if site.answered != 0 || site.end() == 0 {
            return Err(Error::host("gravity: a merged site's member published an answered or empty window"));
        }
        // bounded("key-columns: one check per merge key column, at most MAX_KEYS")
        for &k in self.key_lanes.iter() {
            if (k as u16) >= site.nlanes || gravity_abi::unword(raw::site_lane(site, k as usize).ptr).is_null() {
                return Err(Error::host("gravity: a merged site's key lane is undecoded"));
            }
        }
        Ok(())
    }

    fn stream_member(&self, s: u32) -> usize {
        let ex = self.exec.as_ref().expect("bound");
        ex.streams.as_slice(&ex.arena)[s as usize].member as usize
    }

    /// Stream `s` has no row left: its member is free.
    fn end_stream(&mut self, s: u32) {
        let ex = self.exec.as_mut().expect("bound");
        let st = &mut ex.streams.as_mut_slice(&ex.arena)[s as usize];
        st.done = true;
        let m = core::mem::replace(&mut st.member, NONE);
        if m != NONE {
            self.members[m as usize].stream = NONE;
        }
    }

    /// Stream `s` gives up its member: its head's by-reference bytes copied into the stream's own (in the operator's arena).
    fn evict(&mut self, s: u32) -> Result<()> {
        let nk = self.keys.len();
        let ex = self.exec.as_mut().expect("bound");
        let total: usize = {
            let h = &ex.heads.as_slice(&ex.arena)[s as usize * nk..(s as usize + 1) * nk];
            h.iter().zip(self.byref.iter()).filter(|(c, &br)| br && !c.null).map(|(c, _)| c.n as usize).sum()
        };
        let streams = ex.streams.as_mut_slice(&ex.arena);
        let st = &mut streams[s as usize];
        st.own.clear();
        st.own.reserve(&ex.arena, total).map_err(refused)?;
        let heads = ex.heads.as_mut_slice(&ex.arena);
        // bounded("key-columns: one copy per by-reference merge key column, at most MAX_KEYS")
        for (c, &br) in heads[s as usize * nk..(s as usize + 1) * nk].iter_mut().zip(self.byref.iter()) {
            if br && !c.null {
                let at = st.own.len();
                st.own.extend_from_slice(&ex.arena, raw::bytes(c.p, c.n)).map_err(refused)?;
                c.p = raw::own_at(&st.own, &ex.arena, at);
            }
        }
        let m = core::mem::replace(&mut st.member, NONE);
        if m != NONE {
            self.members[m as usize].stream = NONE;
        }
        Ok(())
    }

    /// Stream `s` (no member) on a member: a free one, else the least recently run one's stream evicted. Its member seeks
    /// the rest of its part from its granule; its head stays where it was when that granule comes back (its rows a
    /// runtime filter dropped since are passed), else it moves to the first row of the window that does.
    fn open(&mut self, s: u32) -> Result<()> {
        let m = match self.members[..self.nlive].iter().position(|m| m.stream == NONE) {
            Some(m) => m,
            None => {
                let m = (0..self.nlive).min_by_key(|&m| self.members[m].used).expect("a member");
                self.evict(self.members[m].stream)?;
                m
            }
        };
        let (g, end, row) = {
            let ex = self.exec.as_mut().expect("bound");
            let st = &mut ex.streams.as_mut_slice(&ex.arena)[s as usize];
            st.member = m as u32;
            (st.g, st.end, st.row)
        };
        self.members[m].stream = s;
        {
            let ex = self.exec.as_mut().expect("bound");
            let keys = ex.keys.as_mut_slice(&ex.arena);
            (keys[0], keys[1]) = (g, end);
            let (keys, nulls) = (ex.keys.as_slice(&ex.arena), ex.nulls.as_slice(&ex.arena));
            self.members[m].src.seek(keys, nulls)?;
        }
        let mut want = (g, row);
        // bounded("other: one member window per pass, whose granule walk polls the statement's cancel")
        loop {
            let n = {
                let mem = &mut self.members[m];
                let mut w = Window::over(&mut mem.site);
                mem.src.window(&mut w)?
            };
            if n == 0 {
                self.end_stream(s);
                return Ok(());
            }
            self.check_window(m)?;
            let wg = self.members[m].site.pos.block as u64;
            let from = if wg == want.0 { want.1 } else { 0 };
            let at = raw::first_at_or_after(&self.members[m].site, from);
            {
                let ex = self.exec.as_mut().expect("bound");
                ex.streams.as_mut_slice(&ex.arena)[s as usize].g = wg;
            }
            if at < self.window_end(m) {
                return self.load_head(s, at);
            }
            want = (wg + 1, 0);
        }
    }

    /// The published run's stream moved past it: to the run's end in its window, or to its next window.
    fn settle(&mut self, s: u32, e: u32) -> Result<()> {
        let m = self.stream_member(s);
        let alive = if e < self.window_end(m) {
            self.load_head(s, e)?;
            true
        } else {
            self.next_window(s)?
        };
        let nk = self.keys.len();
        let ex = self.exec.as_mut().expect("bound");
        let (byref, decl) = (&self.byref[..], &self.decl);
        let (heads, slots) = (ex.heads.as_slice(&ex.arena), ex.slots.as_mut_slice(&ex.arena));
        let mut less = |a: u32, b: u32| stream_less(byref, decl, heads, nk, a, b);
        let mut h = Heap::resume(slots, ex.live);
        debug_assert_eq!(h.top(), Some(s), "the run's stream is the top until it settles");
        if alive {
            h.fix_top(&mut less);
        } else {
            h.pop_top(&mut less);
        }
        ex.live = h.len();
        Ok(())
    }

    /// The end of the top stream `t`'s run: the first index of its window (from its head) whose row sorts after the
    /// runner-up `r`'s head (a tie goes to the lower part), by a gallop and a binary search; its window's end without one.
    fn cut(&self, t: u32, r: Option<u32>) -> Result<u32> {
        let m = self.stream_member(t);
        let end = self.window_end(m);
        let ex = self.exec.as_ref().expect("bound");
        let at = ex.streams.as_slice(&ex.arena)[t as usize].at;
        let Some(r) = r else { return Ok(end) };
        let nk = self.keys.len();
        let hr = Self::head(ex, nk, r);
        let mut cells = [Cell::NULL; MAX_KEYS];
        let before = |j: u32, cells: &mut [Cell]| -> Result<bool> {
            self.row_cells(m, self.row_of(m, j) as usize, cells)?;
            Ok(match Self::cmp(&self.byref, &self.decl, cells, hr) {
                Ordering::Less => true,
                Ordering::Equal => t < r,
                Ordering::Greater => false,
            })
        };
        // the gallop: `lo` is a row of the run, `hi` the first probe past it that is not (or the end)
        let (mut lo, mut step) = (at, 1u32);
        // bounded("small-fixed: the probe doubles each pass, so log2 of a granule's 8192 rows at most")
        let mut hi = loop {
            let probe = at.saturating_add(step).min(end);
            if probe >= end || !before(probe, &mut cells[..nk])? {
                break probe;
            }
            lo = probe;
            step = step.saturating_mul(2);
        };
        // the first index in (lo, hi] whose row is not before the runner-up's head
        // bounded("small-fixed: a binary search halves the span each pass, log2 of 8192 rows at most")
        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            if before(mid, &mut cells[..nk])? {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        Ok(hi)
    }

    /// Member `m`'s window cut to its indices `[at, e)`, published zero-copy into `w`.
    fn publish(&self, m: usize, at: u32, e: u32, w: &mut Window<'_>) {
        let site = &self.members[m].site;
        // bounded("key-columns: one lane per site lane the member published, at most the site's lanes")
        for k in 0..site.nlanes as usize {
            w.set(k, raw::site_lane(site, k));
        }
        if raw::has_sel(site) {
            w.select(&raw::sel(site)[at as usize..e as usize]);
            w.rows = site.rows;
        } else {
            w.rows = e;
            w.cursor = at;
        }
        w.set_base(WindowBase { pos: site.pos, part_id: site.part_id, dict_epoch: site.dict_epoch, granule: site.granule, claim_id: site.claim_id });
        w.set_part_facts(site.part_facts);
    }

    fn seek_merge(&mut self, keys: &[Word], nulls: &[u8]) -> Result<()> {
        let k = {
            let ex = self.exec.as_mut().ok_or_else(|| Error::host("gravity: seek on an unbound merged source"))?;
            ex.run = None;
            ex.live = 0;
            ex.keys.clear();
            ex.keys.extend_from_slice(&ex.arena, keys).map_err(refused)?;
            ex.nulls.clear();
            ex.nulls.extend_from_slice(&ex.arena, nulls).map_err(refused)?;
            ex.streams.len()
        };
        self.cur.clear();
        // bounded("other: the bound members, at most live_max (MERGE_LIVE_MAX)")
        for m in self.members[..self.nlive].iter_mut() {
            m.stream = NONE;
            m.used = 0;
        }
        if keys.len() < 2 || nulls[0] != 0 || nulls[1] != 0 {
            return Ok(());
        }
        let total = {
            let ex = self.exec.as_mut().expect("bound");
            let streams = ex.streams.as_mut_slice(&ex.arena);
            // bounded("other: one stream per part of the statement's list")
            for st in streams.iter_mut() {
                st.member = NONE;
                st.g = st.start;
                st.row = 0;
                st.at = 0;
                st.done = st.start >= st.end;
                st.own.clear();
            }
            streams.last().map_or(0, |s| s.end)
        };
        // a merged site is read whole, in one claim (its pipeline is serial): a claim of part of it has no order to keep
        if keys[0] > 0 || keys[1] < total {
            return Err(Error::host("gravity: a merged site read by a claim of part of its relation"));
        }
        // every stream's first head: its part's first window that keeps a row
        // bounded("other: one stream per part of the statement's list, a granule decode each")
        for s in 0..k as u32 {
            let done = {
                let ex = self.exec.as_ref().expect("bound");
                ex.streams.as_slice(&ex.arena)[s as usize].done
            };
            if !done {
                self.open(s)?;
            }
        }
        let nk = self.keys.len();
        let ex = self.exec.as_mut().expect("bound");
        let live = {
            let (streams, slots) = (ex.streams.as_slice(&ex.arena), ex.slots.as_mut_slice(&ex.arena));
            let mut n = 0;
            // bounded("other: one stream per part of the statement's list")
            for (s, st) in streams.iter().enumerate() {
                if !st.done {
                    slots[n] = s as u32;
                    n += 1;
                }
            }
            n
        };
        let (byref, decl) = (&self.byref[..], &self.decl);
        let (heads, slots) = (ex.heads.as_slice(&ex.arena), ex.slots.as_mut_slice(&ex.arena));
        let mut less = |a: u32, b: u32| stream_less(byref, decl, heads, nk, a, b);
        ex.live = Heap::new(slots, live, &mut less).len();
        Ok(())
    }

    fn window_merge(&mut self, w: &mut Window<'_>) -> Result<u32> {
        let run = self.exec.as_mut().ok_or_else(|| Error::host("gravity: a window of an unbound merged source"))?.run.take();
        if let Some((s, e)) = run {
            self.settle(s, e)?;
        }
        let nk = self.keys.len();
        // bounded("other: each pass publishes a run or reopens one stream's granule through a polled member window")
        loop {
            let (top, runner) = {
                let ex = self.exec.as_mut().expect("bound");
                let (byref, decl) = (&self.byref[..], &self.decl);
                let (heads, slots) = (ex.heads.as_slice(&ex.arena), ex.slots.as_mut_slice(&ex.arena));
                let mut less = |a: u32, b: u32| stream_less(byref, decl, heads, nk, a, b);
                let h = Heap::resume(slots, ex.live);
                let Some(top) = h.top() else { return Ok(0) };
                (top, h.runner_up(&mut less))
            };
            if self.stream_member(top) == NONE as usize {
                // the top's granule opens again; its head may have moved on (a runtime filter's rows), or the stream ended
                self.open(top)?;
                let done = {
                    let ex = self.exec.as_ref().expect("bound");
                    ex.streams.as_slice(&ex.arena)[top as usize].done
                };
                let ex = self.exec.as_mut().expect("bound");
                let (byref, decl) = (&self.byref[..], &self.decl);
                let (heads, slots) = (ex.heads.as_slice(&ex.arena), ex.slots.as_mut_slice(&ex.arena));
                let mut less = |a: u32, b: u32| stream_less(byref, decl, heads, nk, a, b);
                let mut h = Heap::resume(slots, ex.live);
                if done {
                    h.pop_top(&mut less);
                } else {
                    h.fix_top(&mut less);
                }
                ex.live = h.len();
                continue;
            }
            let e = self.cut(top, runner)?;
            let m = self.stream_member(top);
            let at = {
                let ex = self.exec.as_ref().expect("bound");
                ex.streams.as_slice(&ex.arena)[top as usize].at
            };
            self.publish(m, at, e, w);
            let ex = self.exec.as_mut().expect("bound");
            ex.clock += 1;
            ex.run = Some((top, e));
            self.members[m].used = ex.clock;
            return Ok(e - at);
        }
    }

    /// The member whose window the program reads now (the published run's), else the first.
    fn current(&self) -> Option<usize> {
        let ex = self.exec.as_ref()?;
        match ex.run {
            Some((s, _)) => Some(ex.streams.as_slice(&ex.arena)[s as usize].member as usize),
            None => (self.nlive > 0).then_some(0),
        }
    }
}

/// Stream `a`'s head sorts before stream `b`'s: by key, a tie to the lower part.
fn stream_less(byref: &[bool], decl: &ClusterKeyDecl, heads: &[Cell], nk: usize, a: u32, b: u32) -> bool {
    let (x, y) = (&heads[a as usize * nk..(a as usize + 1) * nk], &heads[b as usize * nk..(b as usize + 1) * nk]);
    match MergedRangeSource::cmp(byref, decl, x, y) {
        Ordering::Less => true,
        Ordering::Equal => a < b,
        Ordering::Greater => false,
    }
}

/// A key cell as the seal's comparator reads it: a signed word (an integer, date or timestamp lane, sign-extended), or the
/// payload bytes of a by-reference value (C-collated text compares as its bytes).
fn slot(c: &Cell, byref: bool) -> KeySlot<'_> {
    match (c.null, byref) {
        (true, _) => KeySlot::Null,
        (false, true) => KeySlot::B(raw::bytes(c.p, c.n)),
        (false, false) => KeySlot::I(c.w as i64),
    }
}

impl Source for MergedRangeSource {
    fn bind(&mut self, b: &BindingRef<'_>) -> Result<()> {
        self.bind_merge(b)
    }

    fn seek(&mut self, keys: &[Word], keys_null: &[u8]) -> Result<()> {
        self.seek_merge(keys, keys_null)
    }

    fn zone(&mut self, preds: &[ZonePred], filter_lanes: &[u64], _answer_lanes: &[u64]) {
        self.zone.clear();
        self.zone.extend_from_slice(preds);
        self.filter.clear();
        self.filter.extend_from_slice(filter_lanes);
        // a key lane is never filter-only to a member: an AllMatch window may leave a filter-only lane undecoded
        let bits = 64 * self.filter.len();
        // bounded("key-columns: one bit per merge key column, at most MAX_KEYS")
        for &k in self.key_lanes.iter().filter(|&&k| (k as usize) < bits) {
            self.filter[k as usize / 64] &= !(1u64 << (k % 64));
        }
        self.zoned = true;
    }

    fn zone_any(&mut self, preds: &[ZonePred], ends: &[u16], base: usize) {
        self.any.clear();
        self.any.extend_from_slice(preds);
        self.any_ends.clear();
        self.any_ends.extend_from_slice(ends);
        self.any_base = base;
        self.anyd = true;
    }

    fn window(&mut self, w: &mut Window<'_>) -> Result<u32> {
        self.window_merge(w)
    }

    /// The row protocol over the runs (tests and tooling): each run's rows from its cursor.
    fn next(&mut self, values: &mut [Word], isnull: &mut [u8]) -> Result<bool> {
        // bounded("small-fixed: a run holds a row, so a served row follows at most one window call")
        loop {
            if !self.cur.spent() {
                self.cur.serve(values, isnull);
                return Ok(true);
            }
            let mut site = blank_site(self.scratch.as_mut_ptr());
            let mut w = Window::over(&mut site);
            let n = self.window(&mut w)?;
            self.cur.take(&w);
            self.cur.row = w.cursor;
            if n == 0 {
                return Ok(false);
            }
        }
    }

    fn lanes_hint(&self) -> usize {
        self.lp
    }

    fn dict_value(&mut self, part_id: u32, lane: u16, code: u32) -> Result<(Word, u32)> {
        let ex = self.exec.as_ref().ok_or_else(|| Error::host("gravity: a dictionary value of an unbound merged source"))?;
        let streams = ex.streams.as_slice(&ex.arena);
        let m = match streams.get(part_id as usize).map(|s| s.member).filter(|&m| m != NONE) {
            Some(m) => m as usize,
            None => self.current().ok_or_else(|| Error::host("gravity: a merged source with no member"))?,
        };
        self.members[m].src.dict_value(part_id, lane, code)
    }

    fn lane(&mut self, k: u16, sel: &[u32], out: &mut Lane) -> Result<u32> {
        match self.current() {
            Some(m) => self.members[m].src.lane(k, sel, out),
            None => Ok(0),
        }
    }

    fn cursor(&self) -> &Cursor {
        &self.cur
    }

    fn cursor_mut(&mut self) -> &mut Cursor {
        &mut self.cur
    }

    fn refetch(&mut self, _values: &mut [Word], _isnull: &mut [u8]) -> Result<bool> {
        Err(crate::unsupported())
    }

    fn substitute(&mut self, _pos: gravity_abi::ScanPos) {}

    fn unbind(&mut self) {
        self.unbind_members();
        self.exec = None;
        self.cur.clear();
        self.bound = false;
    }

    fn rearm(&mut self) -> Result<()> {
        // bounded("other: the bound members, at most live_max (MERGE_LIVE_MAX)")
        for m in self.members[..self.nlive].iter_mut() {
            m.src.rearm()?;
        }
        Ok(())
    }

    fn pins(&self) -> u64 {
        self.members[..self.nlive].iter().map(|m| m.src.pins()).sum()
    }

    fn release(&mut self) {
        // bounded("other: the bound members, at most live_max (MERGE_LIVE_MAX)")
        for m in self.members[..self.nlive].iter_mut() {
            m.src.release();
        }
    }

    /// The members' bind-sized buffers, kept at their high water between statements as a granule source's are.
    fn kept_bytes(&self) -> usize {
        self.members.iter().map(|m| m.src.kept_bytes()).sum()
    }

    // D4631: the park protocol reaches every member, bound by any statement since (each keeps its buffers, charges and
    // kept cursors as a granule source does); without it a parked merged source's members stayed on the statement that
    // last ran them, and the trim that dropped the set freed their whole kept heap on another backend's account

    fn transfer_held(&mut self, to: memledger::Owner, release: memledger::Release) {
        // bounded("other: the source's members, at most live_max (MERGE_LIVE_MAX)")
        for m in self.members.iter_mut() {
            m.src.transfer_held(to, release);
        }
    }

    fn kept_uses_to(&mut self, to: memledger::Owner, release: memledger::Release) {
        // bounded("other: the source's members, at most live_max (MERGE_LIVE_MAX)")
        for m in self.members.iter_mut() {
            m.src.kept_uses_to(to, release);
        }
    }

    fn settle_kept(&mut self) {
        // bounded("other: the source's members, at most live_max (MERGE_LIVE_MAX)")
        for m in self.members.iter_mut() {
            m.src.settle_kept();
        }
    }

    fn park(&mut self) {
        // bounded("other: the source's members, at most live_max (MERGE_LIVE_MAX)")
        for m in self.members.iter_mut() {
            m.src.park();
        }
    }

    fn park_bytes(&self) -> usize {
        self.members.iter().map(|m| m.src.park_bytes()).sum()
    }

    fn residency(&self) -> Residency {
        match (self.bound, self.members.first()) {
            (true, Some(m)) if self.nlive > 0 => m.src.residency(),
            _ => Residency::default(),
        }
    }
}
