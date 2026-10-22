//! SORTRUNS-E0 L2 (COLIDX2 §17.1, pgrc-plans/SORTRUNS-E0-STEP1.md D2/D3): the run engine's I/O half, one run job.
//!
//! A job merges sorted inputs of one generation (loose parts, and every live member of each run it takes) into one
//! sorted run that homes their row IDs:
//! - **cursors:** each input's survivors (its DV-marked rows dropped) in `(key, ID)` order: a part whose SortKey
//!   section attests the table's `sorted_by` key is read a granule at a time; any other is sorted in memory first. A
//!   cursor holds its window's key cells (decoded through the writer's own conversion, as K1's sorted merge does, so
//!   the seal's verify agrees) and IDs.
//! - **the merge:** a heap over the cursors ([`pgrc_write::runs::Heap`], `(key, ID)` with the seal's comparator)
//!   streams the rows in order; every [`cut`] rows they are a member: its column passes (K1-MEM, the compaction's
//!   [`ColumnFeed`]) seal it as a temp of the job's writer (debris until the publish), its map is a run member's
//!   (`pgrc_format::rowmap::encode_run_member`), and the reverse builder spills its rows' (rank, frame).
//! - **the reverse:** written once the members are (`RevPlan::write`, through the writer's WAL as its parts are, a
//!   staged temp), page 0 first, so the file is sequential; the run's ID is a generation number reserved before
//!   ([`crate::catalog::reserve_gen`]), since the file and its page 0 name it.
//! - **the publish:** one `PUBLISH` retires the inputs, installs the members, renames the reverse and records the
//!   run ([`SealWriter::publish_runs`]); DV carry routes the rows the inputs' DVs marked since the job read them to
//!   their members, by ID through the new reverse.
//!
//! L3 runs a job in steps (a transaction each) with suspend and resume; here a job is one transaction.

use std::cmp::Ordering;
use std::sync::Arc;

use pgrc_compact::runs::{cut_rows, kept_vstarts, Force, RevBuilder, RevStats, RunPolicy, Spill};
use pgrc_compact::{Job, JobKind, Reason};
use pgrc_format::class::ColSchema;
use pgrc_write::runs::Heap;
use pgrc_format::manifest::{IdRanges, Manifest};
use pgrc_format::rowmap::{encode_run_member, GranMap};
use pgrc_format::runrev::{Hit, PAGE};
use pgrc_format::sortrun::{RankForm, RunRevForm, SortRun};
use pgrc_format::FormatError;
use pgrc_write::ingest::ColBuffer;
use pgrc_write::seal::cmp::{key_slot_at, slot_cmp, KeySlot};
use pgrc_write::seal::ColumnPool;
use pgrc_write::structural::ClusterKeyDecl;
use pgrc_write::writer::PartCutPolicy;
use types_error::{PgError, PgResult};
use types_rel::Relation;

use crate::catalog::Generation;
use crate::compact::{inputs, row_of, ColumnFeed, Granules, In};
use crate::ingest::SealWriter;
use crate::scan::read_residency;

/// What a run job did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOut {
    pub run_id: u64,
    pub level: u8,
    pub members: Vec<u32>,
    pub rows: u64,
    pub dropped: u64,
    pub input_bytes: u64,
    pub output_bytes: u64,
    pub rev: RunRevForm,
    pub rank: RankForm,
    pub rev_bytes: u64,
    pub stats: Option<RevStats>,
}

fn contract(what: String) -> Box<PgError> {
    Box::new(PgError::error(format!("pgrcolumnar: run merge {what}")).with_sqlstate(types_error::ERRCODE_INTERNAL_ERROR))
}

/// Ordinal `o` of input `i` is DV-marked (the job drops it).
fn is_dead(i: &In, o: u32) -> bool {
    i.dead[o as usize / 64] >> (o % 64) & 1 == 1
}

fn format_error(e: FormatError) -> Box<PgError> {
    crate::write_error(e.into())
}

/// The gates' knobs (never set in production), read once per process from `PGRUST_PGRC_TEST_RUN`, a comma list of
/// `key=value`: `cut` (a member's rows), `bucket` (the reverse build's ranks per spill bucket), `rev` (`labels`,
/// `frameix`) and `rank` (`dense`, `set`) forcing the reverse's form; L3a's run pass: `fanin`, `cap` (rows) and
/// `capbytes` ([`run_policy`]), and `target` (the compaction policy's full-part rows, [`test_target_rows`]).
#[derive(Debug, Clone, Copy, Default)]
struct Knobs {
    /// A member's rows (a small cut gives a run of many members).
    cut_rows: Option<u64>,
    /// The reverse build's ranks per spill bucket (0: the default).
    bucket: u64,
    force: Force,
    fanin: Option<u64>,
    cap_rows: Option<u64>,
    cap_bytes: Option<u64>,
    target_rows: Option<u64>,
}

fn parse_knobs(v: &str) -> Knobs {
    let mut k = Knobs::default();
    for kv in v.split(',') {
        let (key, val) = kv.split_once('=').map_or((kv.trim(), ""), |(a, b)| (a.trim(), b.trim()));
        let num = val.parse::<u64>().ok().filter(|&n| n > 0);
        match key {
            "cut" => k.cut_rows = num,
            "bucket" => k.bucket = num.unwrap_or(0),
            "rev" => k.force.form = [RunRevForm::Labels, RunRevForm::FrameIx].into_iter().find(|f| f.name() == val),
            "rank" => k.force.rank = [RankForm::Dense, RankForm::Set].into_iter().find(|r| r.name() == val),
            "fanin" => k.fanin = num,
            "cap" => k.cap_rows = num,
            "capbytes" => k.cap_bytes = num,
            "target" => k.target_rows = num,
            _ => {}
        }
    }
    k
}

fn knobs() -> Knobs {
    static K: pgsync::OnceLock<Knobs> = pgsync::OnceLock::new();
    *K.get_or_init(|| std::env::var("PGRUST_PGRC_TEST_RUN").map_or_else(|_| Knobs::default(), |v| parse_knobs(&v)))
}

/// SORTRUNS-E0 L3a: the run pass's tiered policy ([`RunPolicy::default`], or the gates' knobs).
pub fn run_policy() -> RunPolicy {
    let (k, p) = (knobs(), RunPolicy::default());
    RunPolicy {
        fanin: k.fanin.map_or(p.fanin, |n| usize::try_from(n).unwrap_or(usize::MAX)),
        cap_rows: k.cap_rows.unwrap_or(p.cap_rows),
        cap_bytes: k.cap_bytes.unwrap_or(p.cap_bytes),
    }
}

/// SORTRUNS-E0 L3a: the gates' compaction target in rows (`target=`; `None` in production): small COPY parts read as
/// full, so the run pass sees them and the L0 pass leaves them.
pub fn test_target_rows() -> Option<u64> {
    knobs().target_rows
}

/// SORTRUNS-E0 L3a (main 10-08): the table sort key the run engine can merge by, THE one function the run pass's
/// sort-key decisions go through: the writer's `cluster_key` (the declared `sorted_by` columns, ASC NULLS LAST); `None`
/// when undeclared, void (a name no longer resolves), empty, or a key the run merge cannot order by, so the run pass
/// skips such a table. [`run_merge`] reads `cluster_key` itself.
pub fn table_sort_key(rel: &Relation<'_>) -> Option<Vec<ClusterKeyDecl>> {
    let decls = crate::reloptions::cluster_key(rel);
    // a jsonb path key orders by its value (`seal::decl_slots`), which the run merge does not compare
    (!decls.is_empty() && decls.iter().all(|d| d.path.is_empty())).then_some(decls)
}

/// A member's rows: the base consumer's cut (`PartCutPolicy::default()`: rows, and bytes at the inputs' average row
/// width), or the gates' knob.
pub fn cut(input_rows: u64, input_bytes: u64) -> u64 {
    let p = PartCutPolicy::default();
    knobs().cut_rows.unwrap_or_else(|| cut_rows(p.max_rows, p.max_bytes, input_rows, input_bytes))
}

// ------------------------------------------------------------------------------------------------------------ spill

/// The reverse build's spill: one `pgsql_tmp` file per bucket (PG's temporary files: closed and removed at the
/// transaction's end, and at startup after a crash).
#[derive(Default)]
struct TempSpill {
    files: Vec<Option<(types_storage::File, u64)>>,
}

impl Spill for TempSpill {
    type Error = Box<PgError>;

    fn append(&mut self, bucket: usize, bytes: &[u8]) -> PgResult<()> {
        if self.files.len() <= bucket {
            crate::alloc::try_resize(&mut self.files, bucket + 1, None, "runmerge")?;
        }
        if self.files[bucket].is_none() {
            self.files[bucket] = Some((fd::OpenTemporaryFile(false)?, 0));
        }
        let (f, len) = self.files[bucket].as_mut().expect("opened");
        let n = fd::FileWrite(*f, bytes, *len as i64, fd::wait_event::WAIT_EVENT_BUFFILE_WRITE)?;
        if n != bytes.len() as isize {
            return Err(crate::corrupt(&format!("run merge spill: wrote {n} of {} bytes", bytes.len())));
        }
        *len += bytes.len() as u64;
        Ok(())
    }

    fn read(&mut self, bucket: usize, out: &mut Vec<u8>) -> PgResult<()> {
        out.clear();
        let Some(Some((f, len))) = self.files.get(bucket) else { return Ok(()) };
        crate::alloc::try_resize(out, *len as usize, 0, "runmerge")?;
        let mut at = 0usize;
        while at < out.len() {
            let n = fd::FileRead(*f, &mut out[at..], at as i64, fd::wait_event::WAIT_EVENT_BUFFILE_READ)?;
            if n <= 0 {
                return Err(crate::corrupt(&format!("run merge spill: read {at} of {len} bytes")));
            }
            at += n as usize;
        }
        Ok(())
    }

    fn format(e: FormatError) -> Box<PgError> {
        format_error(e)
    }
}

impl Drop for TempSpill {
    fn drop(&mut self) {
        for (f, _) in self.files.drain(..).flatten() {
            let _ = fd::FileClose(f);
        }
    }
}

// ---------------------------------------------------------------------------------------------------------- cursors

/// A key cell a window owns (a byte cell is a slice of the window's arena).
#[derive(Debug, Clone, Copy)]
enum Cell {
    Null,
    I(i64),
    U(u64),
    F(f64),
    B(u32, u32),
}

/// Rows of one input in `(key, ID)` order: their ordinals, IDs and key cells (`nk` a row).
#[derive(Debug, Default)]
struct Window {
    ords: Vec<u32>,
    ids: Vec<u64>,
    cells: Vec<Cell>,
    arena: Vec<u8>,
}

impl Window {
    fn slot(&self, row: usize, k: usize, nk: usize) -> KeySlot<'_> {
        match self.cells[row * nk + k] {
            Cell::Null => KeySlot::Null,
            Cell::I(x) => KeySlot::I(x),
            Cell::U(x) => KeySlot::U(x),
            Cell::F(x) => KeySlot::F(x),
            Cell::B(at, len) => KeySlot::B(&self.arena[at as usize..(at + len) as usize]),
        }
    }

    fn clear(&mut self) {
        self.ords.clear();
        self.ids.clear();
        self.cells.clear();
        self.arena.clear();
    }

    fn push_cell(&mut self, s: KeySlot<'_>) -> PgResult<()> {
        let c = match s {
            KeySlot::Null => Cell::Null,
            KeySlot::I(x) => Cell::I(x),
            KeySlot::U(x) => Cell::U(x),
            KeySlot::F(x) => Cell::F(x),
            KeySlot::B(b) => {
                let at = u32::try_from(self.arena.len()).ok().filter(|a| a.checked_add(b.len() as u32).is_some()).ok_or_else(|| contract("key window past 4 GiB".into()))?;
                self.arena.extend_from_slice(b);
                Cell::B(at, b.len() as u32)
            }
        };
        self.cells.push(c);
        Ok(())
    }
}

/// One input's cursor.
struct Cursor {
    n: u16,
    /// Read a granule at a time (an attested part); else the whole input, sorted, is the one window.
    sorted: bool,
    next_g: u32,
    win: Window,
    at: usize,
}

/// What every window load reads with.
struct Ctx<'a, 'r> {
    rel: &'a Relation<'r>,
    gen: &'a Generation,
    ins: &'a [In],
    fills: &'a [crate::fastadd::Fill],
    schemas: &'a [ColSchema],
    /// Per declared key, its column's index in `schemas`.
    keys: Vec<usize>,
    decls: &'a [ClusterKeyDecl],
    interrupt: &'a dyn Fn(bool) -> PgResult<()>,
}

impl Ctx<'_, '_> {
    /// The key columns of input `n`'s rows `ords`, through the writer's conversion (the compaction's column feed).
    fn key_columns(&self, n: u16, ords: &[u32]) -> PgResult<Vec<ColBuffer>> {
        let order: Vec<(u16, u32)> = ords.iter().map(|&o| (n, o)).collect();
        let feed = ColumnFeed::new(self.gen, self.ins, self.fills, &order)?;
        let mut out = crate::alloc::try_vec(self.keys.len(), "runmerge")?;
        for &k in &self.keys {
            let mut b = ColBuffer::new(self.schemas[k]);
            feed.fill(self.rel, self.schemas, k, &mut b, true, self.interrupt)?;
            out.push(b);
        }
        Ok(out)
    }

    /// The IDs of input `n`'s ordinals `a..b`.
    fn ids(&self, n: u16, a: u32, b: u32) -> PgResult<Vec<u64>> {
        let mut v = crate::alloc::try_filled((b - a) as usize, 0u64, "runmerge")?;
        self.ins[n as usize].map.view().decode(a, &mut v);
        Ok(v)
    }

    /// Fill `win` with an input's survivors `ords` (ascending; their IDs `ids`, their key columns `cols`) in the given
    /// `perm` order of them (`None`: as they are).
    fn fill(&self, win: &mut Window, ords: &[u32], ids: &[u64], cols: &[ColBuffer], perm: Option<&[u32]>) -> PgResult<()> {
        win.clear();
        let nk = self.keys.len();
        crate::alloc::try_reserve_more(&mut win.cells, ords.len() * nk, "runmerge")?;
        let put = |win: &mut Window, i: usize| -> PgResult<()> {
            win.ords.push(ords[i]);
            win.ids.push(ids[i]);
            for c in cols {
                win.push_cell(key_slot_at(c, i as u64).map_err(crate::write_error)?)?;
            }
            Ok(())
        };
        match perm {
            Some(p) => p.iter().try_for_each(|&i| put(win, i as usize)),
            None => (0..ords.len()).try_for_each(|i| put(win, i)),
        }
    }

    /// Input `n`'s next granule with a survivor, from `cur.next_g` (a sorted input).
    fn load_granule(&self, cur: &mut Cursor) -> PgResult<bool> {
        let i = &self.ins[cur.n as usize];
        let rows = i.rec.rows as u32;
        while u64::from(cur.next_g) * u64::from(i.grain) < u64::from(rows) {
            let a = cur.next_g * i.grain;
            let b = (a + i.grain).min(rows);
            cur.next_g += 1;
            let ords: Vec<u32> = (a..b).filter(|&o| !is_dead(i, o)).collect();
            if ords.is_empty() {
                continue;
            }
            let all = self.ids(cur.n, a, b)?;
            let ids: Vec<u64> = ords.iter().map(|&o| all[(o - a) as usize]).collect();
            let cols = self.key_columns(cur.n, &ords)?;
            self.fill(&mut cur.win, &ords, &ids, &cols, None)?;
            cur.at = 0;
            return Ok(true);
        }
        cur.win.clear();
        Ok(false)
    }

    /// Input `n` whole, sorted by `(key, ID)` in memory (an input no SortKey section attests).
    fn load_sorted(&self, cur: &mut Cursor) -> PgResult<bool> {
        let i = &self.ins[cur.n as usize];
        let rows = i.rec.rows as u32;
        let ords: Vec<u32> = (0..rows).filter(|&o| !is_dead(i, o)).collect();
        if ords.is_empty() {
            return Ok(false);
        }
        let all = self.ids(cur.n, 0, rows)?;
        let ids: Vec<u64> = ords.iter().map(|&o| all[o as usize]).collect();
        drop(all);
        let cols = self.key_columns(cur.n, &ords)?;
        let slots = cols.iter().map(pgrc_write::seal::cmp::key_slots).collect::<Result<Vec<_>, _>>().map_err(crate::write_error)?;
        let mut perm: Vec<u32> = (0..ords.len() as u32).collect();
        perm.sort_unstable_by(|&a, &b| {
            let (a, b) = (a as usize, b as usize);
            slots.iter().zip(self.decls).map(|(s, d)| slot_cmp(&s[a], &s[b], d)).find(|o| o.is_ne()).unwrap_or(ids[a].cmp(&ids[b]))
        });
        drop(slots);
        self.fill(&mut cur.win, &ords, &ids, &cols, Some(&perm))?;
        cur.at = 0;
        Ok(true)
    }

    /// The cursor's next row (`false`: the input is done).
    fn advance(&self, cur: &mut Cursor) -> PgResult<bool> {
        cur.at += 1;
        if cur.at < cur.win.ords.len() {
            return Ok(true);
        }
        match cur.sorted {
            true => self.load_granule(cur),
            false => {
                cur.win.clear();
                Ok(false)
            }
        }
    }
}

/// Cursors `a` and `b`'s current rows by `(key, ID)`.
fn cmp_rows(a: &Cursor, b: &Cursor, decls: &[ClusterKeyDecl]) -> Ordering {
    let nk = decls.len();
    for (k, d) in decls.iter().enumerate() {
        let o = slot_cmp(&a.win.slot(a.at, k, nk), &b.win.slot(b.at, k, nk), d);
        if o.is_ne() {
            return o;
        }
    }
    a.win.ids[a.at].cmp(&b.win.ids[b.at])
}

// --------------------------------------------------------------------------------------------------------------- job

/// Run a run job over `gen` (the caller holds the table's maintenance lock): merge `parts` (loose sorted parts, and
/// every live member of each run it takes) into one sorted run and publish it in the caller's transaction.
/// `interrupt` as [`crate::compact::run_job`]'s. A job whose every row is dropped retires its inputs only.
pub fn run_merge(rel: &Relation<'_>, gen: &Arc<Generation>, parts: &[u32], pool: Option<Arc<dyn ColumnPool>>, interrupt: &dyn Fn(bool) -> PgResult<()>) -> PgResult<RunOut> {
    let out = merge_body(rel, gen, parts, pool, interrupt);
    mcx::release_retained();
    out
}

fn merge_body(rel: &Relation<'_>, gen: &Arc<Generation>, parts: &[u32], pool: Option<Arc<dyn ColumnPool>>, interrupt: &dyn Fn(bool) -> PgResult<()>) -> PgResult<RunOut> {
    let decls = crate::reloptions::cluster_key(rel);
    if decls.is_empty() {
        return Err(Box::new(PgError::error(format!("pgrcolumnar: a run merge of \"{}\", which has no sorted_by key", rel.name())).with_sqlstate(types_error::ERRCODE_INVALID_PARAMETER_VALUE)));
    }
    // A jsonb path key in any position: the seal orders it by the path's value (`seal::decl_slots`), while this merge
    // compares each key column's stored form (`key_slot_at`) and takes a part's SortKey (attnos only) as sorted, so its
    // members would follow neither the seal's order nor the planner's. Refused before any work.
    if decls.iter().any(|d| !d.path.is_empty()) {
        return Err(Box::new(
            PgError::error(format!("pgrcolumnar: run merge does not support path sort keys yet (the sorted_by key of \"{}\" has one)", rel.name()))
                .with_sqlstate(types_error::ERRCODE_FEATURE_NOT_SUPPORTED),
        ));
    }
    if parts.is_empty() || parts.len() > usize::from(u16::MAX) {
        return Err(contract(format!("of {} inputs", parts.len())));
    }
    let dir = crate::dirpath::table_dir_path(rel.rd_locator.get(), rel.rd_backend);
    let schemas = crate::schema::col_schemas(rel)?;
    let mut presences = crate::fastadd::Presences::new(&schemas, pgrc_format::ident::schema_fingerprint(&schemas));
    let job = Job { kind: JobKind::SortedMerge, reason: Reason::Forced, inputs: crate::alloc::try_to_vec(parts, "runmerge")? };
    let ins = inputs(rel, gen, &dir, &job, &mut presences, false, true)?;
    let fills = match presences.any_absent() {
        true => crate::fastadd::fills(&rel.rd_att, &schemas)?,
        false => Vec::new(),
    };
    // the run homes every input's ranges: a loose part's own, and each run's once (every live member of it an input)
    let mut level = 1u8;
    let mut taken: Vec<usize> = Vec::new();
    for i in &ins {
        if let Some((r, _)) = gen.run_member(i.idx) {
            if !taken.contains(&r) {
                let run = &gen.runs()[r].run;
                if let Some(no) = run.live().find(|no| !parts.contains(no)) {
                    return Err(contract(format!("takes part {} of sorted run {} but not its member {no}", i.rec.part_no, run.run_id)));
                }
                level = level.max(run.level.saturating_add(1));
                taken.push(r);
            }
        }
    }
    // L3b1: a self-homed run's members (VACUUM FULL's renumbered run) come with their own ranges; the run's level counts
    for i in &ins {
        if let Some(h) = gen.runs().iter().find(|h| !h.run.homes() && h.slots.contains(&Some(i.idx))) {
            level = level.max(h.run.level.saturating_add(1));
        }
    }
    let ranges: IdRanges = pgrc_compact::ident::union_ranges(ins.iter().map(|i| &i.ids[..]).chain(taken.iter().map(|&r| &gen.runs()[r].run.ranges[..])));
    let in_rows: u64 = ins.iter().map(|i| i.rec.rows).sum();
    let in_bytes: u64 = ins.iter().map(|i| i.rec.file_len).sum();
    let attnos: Vec<u32> = decls.iter().map(|d| d.attno).collect();
    let keys = decls.iter().map(|d| schemas.iter().position(|s| s.attno == d.attno).ok_or_else(|| contract("key column is not a table column".into()))).collect::<PgResult<Vec<_>>>()?;
    let ctx = Ctx { rel, gen, ins: &ins, fills: &fills, schemas: &schemas, keys, decls: &decls, interrupt };
    let mut cursors: Vec<Cursor> = crate::alloc::try_vec(ins.len(), "runmerge")?;
    for (n, i) in ins.iter().enumerate() {
        let keys = crate::footer::part_sort_key(&gen.parts[i.idx].part)?.keys;
        let sorted = crate::reloptions::attested_prefix(&attnos, &keys) == attnos.len();
        cursors.push(Cursor { n: n as u16, sorted, next_g: 0, win: Window::default(), at: 0 });
    }
    // r15 (sortmerge T1, the ONE merge engine): the heap is trunk's `pgrc_write::runs::Heap` over u32 sources in slots
    // this job lends (its live cursors)
    let mut live: Vec<u32> = Vec::new();
    for c in cursors.iter_mut() {
        let has = if c.sorted { ctx.load_granule(c)? } else { ctx.load_sorted(c)? };
        if has {
            live.push(u32::from(c.n));
        }
    }
    let cut = cut(in_rows, in_bytes);
    let mut w = SealWriter::open_compact(rel, pool, true)?;
    #[allow(clippy::type_complexity)]
    let fed = (|| -> PgResult<Option<(SortRun, Vec<Vec<u8>>, Vec<u64>, Vec<(String, String)>, RevStats)>> {
        let n_live = live.len();
        let mut heap = Heap::new(&mut live, n_live, &mut |a, b| cmp_rows(&cursors[a as usize], &cursors[b as usize], &decls).is_lt());
        if heap.is_empty() {
            return Ok(None);
        }
        let run_id = crate::catalog::reserve_gen(&dir, gen.gen);
        let k = knobs();
        let mut rev = RevBuilder::new(run_id, ranges.clone(), k.bucket, TempSpill::default(), k.force)?;
        let (mut maps, mut member_rows): (Vec<Vec<u8>>, Vec<u64>) = (Vec::new(), Vec::new());
        let natts = rel.rd_att.natts as usize;
        while !heap.is_empty() {
            let mut order: Vec<(u16, u32)> = crate::alloc::try_vec(cut.min(in_rows) as usize, "runmerge")?;
            let mut ids: Vec<u64> = crate::alloc::try_vec(order.capacity(), "runmerge")?;
            while (order.len() as u64) < cut {
                let Some(c) = heap.top() else { break };
                let cur = &mut cursors[c as usize];
                order.push((cur.n, cur.win.ords[cur.at]));
                ids.push(cur.win.ids[cur.at]);
                let more = ctx.advance(cur)?;
                let mut less = |a: u32, b: u32| cmp_rows(&cursors[a as usize], &cursors[b as usize], &decls).is_lt();
                match more {
                    true => heap.fix_top(&mut less),
                    false => heap.pop_top(&mut less),
                }
            }
            // the member: its column passes, or (a finer grain) the row feed and its seal; the member's first pass is
            // the one `interrupt` is told decodes granules first (the pause knob: once a granule a member reads)
            let feed = ColumnFeed::new(gen, &ins, &fills, &order)?;
            if !w.seal_columns(rel, order.len() as u64, &mut |c, out| feed.fill(rel, &schemas, c, out, c == 0, interrupt))? {
                let _ = elog::elog(types_error::LOG, format!("pgrcolumnar: run merge of \"{}\": a member of {} rows needs a finer grain; the job holds it", rel.name(), order.len())); // alloc: Cold
                let residency = read_residency(in_bytes);
                let cursors = ins.iter().map(|i| crate::scan::PartCursors::new(&gen.parts[i.idx].part, residency)).collect();
                let mut g = Granules { schemas: &schemas, ins: &ins, fills: &fills, hold_all: true, held: Vec::new(), cursors };
                let (mut values, mut isnull) = (crate::alloc::try_filled(natts, datum::Datum::null(), "runmerge")?, crate::alloc::try_filled(natts, true, "runmerge")?);
                for &(n, o) in &order {
                    row_of(&mut g, &ins, n, o, &mut values, &mut isnull, interrupt)?;
                    w.append(rel, &values, &isnull)?;
                }
                drop(g);
                w.seal_appended(rel)?;
            }
            let frames = rev.member(&ids)?;
            maps.push(encode_run_member(&ids, &frames.vstarts, Some(&frames.vmin[..])).map_err(format_error)?);
            member_rows.push(ids.len() as u64);
        }
        // the reverse: a dry pass gives page 0 (the counts its directory holds), so the file is written in order
        let mut plan = rev.finish()?;
        let spec = plan.spec();
        let (fin, stats) = plan.write(&mut |_, _| Ok(()))?;
        let mut failed: Option<Box<PgError>> = None;
        let tmp = w.write_file(0, &mut |write| {
            write(&fin.page0)?;
            let mut next = PAGE as u64;
            let wrote = plan.write(&mut |at, bytes| {
                if at == 0 {
                    return if bytes == &fin.page0[..] { Ok(()) } else { Err(contract("reverse page 0 differs from its dry pass".into())) };
                }
                if at != next {
                    return Err(contract(format!("reverse write at {at}, not {next}")));
                }
                next += bytes.len() as u64;
                write(bytes).map_err(crate::write_error)
            });
            wrote.map(|_| ()).map_err(|e| {
                failed = Some(e);
                pgrc_write::WriteError::Refused { what: "a run merge's reverse write failed" }
            })
        });
        let tmp = match (tmp, failed) {
            (Err(_), Some(e)) => return Err(e),
            (t, _) => t?,
        };
        let run = SortRun { run_id, level, rev: spec.form, rank: spec.rank, rows: spec.rows, rev_len: fin.rev_len, rev_crc: fin.rev_crc, members: Vec::new(), ranges: ranges.clone() };
        let staged = vec![(tmp, pgrc_format::dirlayout::runrev_file_name(run_id))];
        Ok(Some((run, maps, member_rows, staged, stats)))
    })();
    let built = match fed {
        Ok(b) => b,
        Err(e) => {
            w.abort()?;
            return Err(e);
        }
    };
    let retire: Vec<u32> = ins.iter().map(|i| i.rec.part_no).collect();
    let dropped: u64 = ins.iter().map(|i| (0..i.rec.rows as u32).filter(|&o| is_dead(i, o)).count() as u64).sum();
    let Some((run, maps, member_rows, staged, stats)) = built else {
        // every row dropped: the inputs retire, nothing is installed
        let pairs: Vec<(u32, u64)> = ins.iter().map(|i| (i.rec.part_no, i.rec.dv_gen)).collect();
        w.publish_compact(rel, &pairs, (pgrc_format::manifest::PART_ROWMAP_GRANULES, &[][..]), &[])?;
        return Ok(RunOut { run_id: 0, level, members: Vec::new(), rows: 0, dropped, input_bytes: in_bytes, output_bytes: 0, rev: RunRevForm::None, rank: RankForm::Dense, rev_bytes: 0, stats: None });
    };
    let path = format!("{dir}/{}", staged[0].0);
    let mut carry = |base: &Manifest| carry_rows(&dir, &ins, &run, &path, &maps, &member_rows, base);
    let (out_gen, members) = w.publish_runs(rel, &maps, &retire, run.clone(), gen.gen, &staged, &mut carry)?;
    let output_bytes = out_gen.manifest.parts.iter().filter(|p| members.contains(&p.part_no)).map(|p| p.file_len).sum();
    Ok(RunOut { run_id: run.run_id, level, members, rows: run.rows, dropped, input_bytes: in_bytes, output_bytes, rev: run.rev, rank: run.rank, rev_bytes: run.rev_len, stats: Some(stats) })
}

/// DV carry (COLIDX2 §17.1.4), under the publish lock: the rows the inputs' DVs mark in `base` that the job read as
/// live, routed by ID through the run's reverse (its staged temp `path`) and its members' maps: per member slot, its
/// ordinals. An input's own DV is never rewritten: it retires.
fn carry_rows(dir: &str, ins: &[In], run: &SortRun, path: &str, maps: &[Vec<u8>], rows: &[u64], base: &Manifest) -> PgResult<Vec<(usize, Vec<u32>)>> {
    let mut ids: Vec<u64> = Vec::new();
    for i in ins {
        let now = base.parts.iter().find(|p| p.part_no == i.rec.part_no).ok_or_else(|| contract(format!("input part {} left the base", i.rec.part_no)))?;
        if now.dv_gen == i.rec.dv_gen {
            continue;
        }
        let Some(d) = crate::scan::load_part_deletes(dir, now)? else { continue };
        let view = i.map.view();
        for g in 0..now.granule_count {
            let mut mask = [0u64; pgrc_scan::DV_GRANULE_WORDS];
            d.union_into(g, &mut mask);
            for r in (0..i.grain).filter(|&r| mask[r as usize / 64] >> (r % 64) & 1 == 1) {
                let o = g * i.grain + r;
                if u64::from(o) < i.rec.rows && !is_dead(i, o) {
                    ids.push(view.ident(o));
                }
            }
        }
    }
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    route(ids, &crate::runcache::RunCell::open_path(path, run), run, maps, rows)
}

/// The member slot and ordinal of each of `ids` in `run` (its reverse `cell`, its members' maps `maps` of `rows` rows
/// each), grouped by slot: an ID the run does not hold is a contract error.
pub(crate) fn route(ids: Vec<u64>, cell: &crate::runcache::RunCell, run: &SortRun, maps: &[Vec<u8>], rows: &[u64]) -> PgResult<Vec<(usize, Vec<u32>)>> {
    let mut decoded: Vec<Option<GranMap>> = (0..maps.len()).map(|_| None).collect();
    let mut by_slot: std::collections::BTreeMap<usize, Vec<u32>> = std::collections::BTreeMap::new();
    for id in ids {
        let (slot, found) = match cell.lookup(run, id)? {
            Hit::Absent => return Err(contract(format!("DV carry: ID {id} is not in the run"))),
            Hit::Group { slot, va, vb } => (slot as usize, (va, vb, true)),
            Hit::Frame { slot, vframe } => (slot as usize, (vframe, 0, false)),
        };
        let (Some(map), Some(&n)) = (maps.get(slot), rows.get(slot)) else {
            return Err(contract(format!("DV carry: no member {slot}")));
        };
        let m = match &mut decoded[slot] {
            Some(m) => m,
            d => d.insert(GranMap::decode(map, n).map_err(format_error)?),
        };
        let o = match found {
            (va, vb, true) => m.run_find(va, vb, id),
            (v, _, false) => m.run_find_in(v, id),
        }
        .map_err(format_error)?
        .ok_or_else(|| contract(format!("DV carry: ID {id} is not in member {slot}")))?;
        by_slot.entry(slot).or_default().push(o);
    }
    Ok(by_slot.into_iter().collect())
}

// ------------------------------------------------------------------------------------------- member maintenance (L3b1)

/// What a member job did (SORTRUNS-E0 L3b1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberOut {
    /// The members that left: a swap's one, or the dead ones.
    pub retired: Vec<u32>,
    /// A swap's new member (`None`: dead members retired).
    pub to: Option<u32>,
    pub rows: u64,
    pub dropped: u64,
    pub input_bytes: u64,
    pub output_bytes: u64,
}

/// SORTRUNS-E0 L3b1: retire `parts`, members of runs that home their IDs every row of which is deleted, in the
/// caller's transaction under the maintenance lock: their slots empty (a run left with no live member drops).
pub fn retire_members(rel: &Relation<'_>, gen: &Arc<Generation>, parts: &[u32]) -> PgResult<MemberOut> {
    let (mut pairs, mut dropped, mut bytes) = (crate::alloc::try_vec(parts.len(), "runmerge")?, 0u64, 0u64);
    for &no in parts {
        let idx = gen.manifest.parts.iter().position(|p| p.part_no == no).ok_or_else(|| contract(format!("member retire: part {no} is not in generation {}", gen.gen)))?;
        let rec = gen.manifest.parts[idx];
        if gen.run_member(idx).is_none() {
            return Err(contract(format!("member retire: part {no} is no member of a run that homes its IDs")));
        }
        if gen.parts[idx].deletes.as_ref().map_or(0, |d| d.deleted_rows) < rec.rows {
            return Err(contract(format!("member retire: part {no} has a live row")));
        }
        pairs.push((no, rec.dv_gen));
        (dropped, bytes) = (dropped + rec.rows, bytes + rec.file_len);
    }
    let w = SealWriter::open_compact(rel, None, true)?;
    w.publish_compact(rel, &pairs, (pgrc_format::manifest::PART_ROWMAP_GRANULES, &[][..]), &[])?;
    Ok(MemberOut { retired: crate::alloc::try_to_vec(parts, "runmerge")?, to: None, rows: 0, dropped, input_bytes: bytes, output_bytes: 0 })
}

/// SORTRUNS-E0 L3b1 (`policy.rs` §6 item 9): member `part_no` of a run that homes its IDs, rewritten alone with its
/// survivors in ordinal order (the run's `(key, ID)` order) into one part that takes its slot. Its RUN map keeps the
/// member's virtual frames (each one's first row among the survivors, its fence minimum unchanged), so the run's record
/// and reverse stay as they are and every ID the reverse leads to is found again, or is gone. DV carry: the rows the
/// member's DV marked since the job read it, at their new ordinals. A member with no survivor retires
/// ([`retire_members`]). In the caller's transaction under the maintenance lock; `interrupt` as [`run_merge`]'s.
pub fn rewrite_member(rel: &Relation<'_>, gen: &Arc<Generation>, part_no: u32, pool: Option<Arc<dyn ColumnPool>>, interrupt: &dyn Fn(bool) -> PgResult<()>) -> PgResult<MemberOut> {
    let out = member_body(rel, gen, part_no, pool, interrupt);
    mcx::release_retained();
    out
}

fn member_body(rel: &Relation<'_>, gen: &Arc<Generation>, part_no: u32, pool: Option<Arc<dyn ColumnPool>>, interrupt: &dyn Fn(bool) -> PgResult<()>) -> PgResult<MemberOut> {
    let dir = crate::dirpath::table_dir_path(rel.rd_locator.get(), rel.rd_backend);
    let schemas = crate::schema::col_schemas(rel)?;
    let mut presences = crate::fastadd::Presences::new(&schemas, pgrc_format::ident::schema_fingerprint(&schemas));
    let job = Job { kind: JobKind::DvRewrite, reason: Reason::DvFloor, inputs: vec![part_no] };
    let ins = inputs(rel, gen, &dir, &job, &mut presences, false, true)?;
    let i = &ins[0];
    if gen.run_member(i.idx).is_none() {
        return Err(contract(format!("member swap: part {part_no} is no member of a run that homes its IDs")));
    }
    let kept: Vec<u32> = (0..i.rec.rows as u32).filter(|&o| !is_dead(i, o)).collect();
    if kept.is_empty() {
        return retire_members(rel, gen, &[part_no]);
    }
    let crate::catalog::RowMap::Granules(gm) = &*i.map else { return Err(contract(format!("member swap: part {part_no} has no RUN map"))) };
    let (vstarts, vmin) = gm.map().run_vframes().map_err(format_error)?;
    let mut all = crate::alloc::try_filled(i.rec.rows as usize, 0u64, "runmerge")?;
    i.map.view().decode(0, &mut all);
    let ids: Vec<u64> = kept.iter().map(|&o| all[o as usize]).collect();
    drop(all);
    let map = encode_run_member(&ids, &kept_vstarts(&vstarts, &kept), vmin.as_deref()).map_err(format_error)?;
    let fills = match presences.any_absent() {
        true => crate::fastadd::fills(&rel.rd_att, &schemas)?,
        false => Vec::new(),
    };
    let order: Vec<(u16, u32)> = kept.iter().map(|&o| (0u16, o)).collect();
    let mut w = SealWriter::open_compact(rel, pool, true)?;
    let sealed = (|| -> PgResult<()> {
        let feed = ColumnFeed::new(gen, &ins, &fills, &order)?;
        if !w.seal_columns(rel, order.len() as u64, &mut |c, out| feed.fill(rel, &schemas, c, out, c == 0, interrupt))? {
            let _ = elog::elog(types_error::LOG, format!("pgrcolumnar: member swap of \"{}\": a member of {} rows needs a finer grain; the job holds it", rel.name(), order.len())); // alloc: Cold
            let cursors = vec![crate::scan::PartCursors::new(&gen.parts[i.idx].part, read_residency(i.rec.file_len))];
            let mut g = Granules { schemas: &schemas, ins: &ins, fills: &fills, hold_all: true, held: Vec::new(), cursors };
            let natts = rel.rd_att.natts as usize;
            let (mut values, mut isnull) = (crate::alloc::try_filled(natts, datum::Datum::null(), "runmerge")?, crate::alloc::try_filled(natts, true, "runmerge")?);
            for &(n, o) in &order {
                row_of(&mut g, &ins, n, o, &mut values, &mut isnull, interrupt)?;
                w.append(rel, &values, &isnull)?;
            }
            drop(g);
            w.seal_appended(rel)?;
        }
        Ok(())
    })();
    if let Err(e) = sealed {
        w.abort()?;
        return Err(e);
    }
    let mut carry = |base: &Manifest| swap_carry(&dir, i, &kept, base);
    let (out_gen, to) = w.publish_member_swap(rel, &map, part_no, &mut carry)?;
    let output_bytes = out_gen.manifest.parts.iter().find(|p| p.part_no == to).map_or(0, |p| p.file_len);
    Ok(MemberOut { retired: vec![part_no], to: Some(to), rows: kept.len() as u64, dropped: i.rec.rows - kept.len() as u64, input_bytes: i.rec.file_len, output_bytes })
}

/// What VACUUM FULL's renumber of a run did (SORTRUNS-E0 L3b1, [`renumber_run`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenumberOut {
    /// The self-homed run's members (none when no row survived).
    pub members: Vec<u32>,
    pub rows: u64,
    pub dropped: u64,
    pub input_bytes: u64,
    pub output_bytes: u64,
    /// The first ID past the run's: `base` plus each member's aligned rows.
    pub next: u64,
}

/// The aligned ID width a run renumbered from `base` takes: each of its live members' survivors, aligned
/// ([`pgrc_format::rowid::align_up`]), in slot order.
pub fn run_width(gen: &Generation, r: usize) -> u64 {
    let h = &gen.runs()[r];
    h.slots.iter().flatten().map(|&i| pgrc_format::rowid::align_up(gen.manifest.parts[i].rows - gen.parts[i].deletes.as_ref().map_or(0, |d| d.deleted_rows))).sum()
}

/// SORTRUNS-E0 L3b1: VACUUM FULL's renumber of run `r` of `gen` (one that homes its IDs, or a self-homed one), under
/// the caller's AccessExclusiveLock and maintenance lock. Each live member, in slot order (the run's key order), is
/// rewritten with its survivors under dense IDs: member `k` homes `[b_k, b_k + align_up(n_k))`, `b_0 = base`,
/// `b_(k+1) = b_k + align_up(n_k)`; the outputs are published as one self-homed run of the run's level (form NONE: the
/// members keep their own `EXT_RANGES`, so no reverse is read or written) and the old run retires with its members
/// (its reverse goes at the next reclaim). No DML runs under the lock: nothing is carried. The caller chose `base` clear
/// of every other part's and run's IDs. On the merged tree (post-switch) this is also the natural place to clear
/// `sorted_by_partial` (pgrc-plans/SORTRUNS-E0-STEP1.md §7 item 1, FOLLOWUP SORTEDBY-PARTIAL-CLEAR).
pub fn renumber_run(rel: &Relation<'_>, gen: &Arc<Generation>, r: usize, base: u64, pool: Option<Arc<dyn ColumnPool>>, interrupt: &dyn Fn(bool) -> PgResult<()>) -> PgResult<RenumberOut> {
    let out = renumber_body(rel, gen, r, base, pool, interrupt);
    mcx::release_retained();
    out
}

fn renumber_body(rel: &Relation<'_>, gen: &Arc<Generation>, r: usize, base: u64, pool: Option<Arc<dyn ColumnPool>>, interrupt: &dyn Fn(bool) -> PgResult<()>) -> PgResult<RenumberOut> {
    let run = gen.runs().get(r).map(|h| h.run.clone()).ok_or_else(|| contract(format!("run renumber: no run {r}")))?;
    let parts: Vec<u32> = run.live().collect();
    if parts.is_empty() {
        return Err(contract(format!("run renumber: run {} has no live member", run.run_id)));
    }
    let dir = crate::dirpath::table_dir_path(rel.rd_locator.get(), rel.rd_backend);
    let schemas = crate::schema::col_schemas(rel)?;
    let mut presences = crate::fastadd::Presences::new(&schemas, pgrc_format::ident::schema_fingerprint(&schemas));
    let job = Job { kind: JobKind::DvRewrite, reason: Reason::Forced, inputs: parts.clone() };
    let ins = inputs(rel, gen, &dir, &job, &mut presences, true, true)?;
    let fills = match presences.any_absent() {
        true => crate::fastadd::fills(&rel.rd_att, &schemas)?,
        false => Vec::new(),
    };
    let retire: Vec<(u32, u64)> = ins.iter().map(|i| (i.rec.part_no, i.rec.dv_gen)).collect();
    let (rows_in, input_bytes): (u64, u64) = (ins.iter().map(|i| i.rec.rows).sum(), ins.iter().map(|i| i.rec.file_len).sum());
    let mut w = SealWriter::open_compact(rel, pool, true)?;
    let (mut maps, mut ranges, mut next, mut rows) = (Vec::new(), Vec::new(), base, 0u64);
    let fed = (|| -> PgResult<()> {
        for (n, i) in ins.iter().enumerate() {
            let order: Vec<(u16, u32)> = (0..i.rec.rows as u32).filter(|&o| !is_dead(i, o)).map(|o| (n as u16, o)).collect();
            if order.is_empty() {
                continue;
            }
            let k = order.len() as u64;
            let map = pgrc_compact::ident::renumbered(next, k).map_err(format_error)?;
            let feed = ColumnFeed::new(gen, &ins, &fills, &order)?;
            if !w.seal_columns(rel, k, &mut |c, out| feed.fill(rel, &schemas, c, out, c == 0, interrupt))? {
                let _ = elog::elog(types_error::LOG, format!("pgrcolumnar: run renumber of \"{}\": a member of {k} rows needs a finer grain; the job holds it", rel.name())); // alloc: Cold
                let cursors = ins.iter().map(|x| crate::scan::PartCursors::new(&gen.parts[x.idx].part, read_residency(x.rec.file_len))).collect();
                let mut g = Granules { schemas: &schemas, ins: &ins, fills: &fills, hold_all: true, held: Vec::new(), cursors };
                let natts = rel.rd_att.natts as usize;
                let (mut values, mut isnull) = (crate::alloc::try_filled(natts, datum::Datum::null(), "runmerge")?, crate::alloc::try_filled(natts, true, "runmerge")?);
                for &(n, o) in &order {
                    row_of(&mut g, &ins, n, o, &mut values, &mut isnull, interrupt)?;
                    w.append(rel, &values, &isnull)?;
                }
                drop(g);
                w.seal_appended(rel)?;
            }
            maps.push((map.form(), map.encode()));
            ranges.push(vec![(next, next + pgrc_format::rowid::align_up(k))]);
            (next, rows) = (next + pgrc_format::rowid::align_up(k), rows + k);
        }
        Ok(())
    })();
    if let Err(e) = fed {
        w.abort()?;
        return Err(e);
    }
    if maps.is_empty() {
        // every row deleted: the members retire, the run drops
        w.publish_compact(rel, &retire, (pgrc_format::manifest::PART_ROWMAP_GRANULES, &[][..]), &[])?;
        return Ok(RenumberOut { members: Vec::new(), rows: 0, dropped: rows_in, input_bytes, output_bytes: 0, next: base });
    }
    let (out_gen, members) = w.publish_self_run(rel, &maps, &ranges, &retire, run.level)?;
    let output_bytes = out_gen.manifest.parts.iter().filter(|p| members.contains(&p.part_no)).map(|p| p.file_len).sum();
    Ok(RenumberOut { members, rows, dropped: rows_in - rows, input_bytes, output_bytes, next })
}

/// DV carry of a member's slot swap, under the publish lock: the rows member `i`'s DV marks in `base` that the job read
/// as live, as their ordinals among the survivors `kept` (the new member's ordinals). Its own DV is never rewritten: it
/// retires.
fn swap_carry(dir: &str, i: &In, kept: &[u32], base: &Manifest) -> PgResult<Vec<u32>> {
    let now = base.parts.iter().find(|p| p.part_no == i.rec.part_no).ok_or_else(|| contract(format!("member {} left the base", i.rec.part_no)))?;
    if now.dv_gen == i.rec.dv_gen {
        return Ok(Vec::new());
    }
    let Some(d) = crate::scan::load_part_deletes(dir, now)? else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    for g in 0..now.granule_count {
        let mut mask = [0u64; pgrc_scan::DV_GRANULE_WORDS];
        d.union_into(g, &mut mask);
        for r in (0..i.grain).filter(|&r| mask[r as usize / 64] >> (r % 64) & 1 == 1) {
            let o = g * i.grain + r;
            if let Ok(n) = kept.binary_search(&o) {
                out.push(n as u32);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gates_knob_parses_its_keys() {
        let k = parse_knobs("cut=1000, bucket=2000,rev=frameix,rank=dense");
        assert_eq!((k.cut_rows, k.bucket, k.force.form, k.force.rank), (Some(1000), 2000, Some(RunRevForm::FrameIx), Some(RankForm::Dense)));
        let k = parse_knobs("rev=labels,cut=0,bucket=x,other=1");
        assert_eq!((k.cut_rows, k.bucket, k.force.form, k.force.rank), (None, 0, Some(RunRevForm::Labels), None));
        let k = parse_knobs("");
        assert_eq!((k.cut_rows, k.bucket, k.force), (None, 0, Force::default()));
    }
}
