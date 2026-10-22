//! D3156: a verbatim part's code table over the granules one execution keeps.
//!
//! A part that stored a text column verbatim publishes its code lane as the byte ranks of its rows (text_hosts
//! `row_codes`): the whole part decoded, sorted and fingerprinted, about one code per row of a near-unique column. A
//! statement whose zone verdicts keep a few granules of the part (a sorted key's value, a date window) read the whole part
//! to code those few: a sort-key value and a date window over a 30M-row table kept 29 of 3634 granules and built 29
//! whole-part images (524 MiB, 2.1 s cold at W8).
//!
//! KEPT MODE: a site whose execution has bind-time zone predicates decides per verbatim part and code lane where its
//! table lives (`SiteKept::home`, in the statement's `Resolved` by registry index). The part's kept granules are a pure
//! function of the bind's zone predicates (the first `nzone_bind`: no seek's key predicate) and the part's Stats records,
//! read whole first (`kept_of`, the records' verdicts alone: no code zone or zone map), so every source of the site
//! decides alike. When they are at most 1 / `KEPT_GRANULES_DIV` of the part's, the table is theirs alone
//! (`text_hosts::Kept`, cached under its own key: a hot repeat of the statement hits it) in the site's own cells (HOME_OWN,
//! never the list's shared cells: a racing build's entry loses); else the part's whole table opens in the list's cells as
//! before (HOME_LIST). A granule the kept table does not cover holds no row the bind's zone predicates pass (their verdict
//! there is NONE, each predicate one of the scan's quals), so a source that meets one skips it before any code lane of it
//! is read (`kept_skips`, at `decode`): the table covers every granule a source decodes, whatever verdicts it holds. A
//! coded part's dictionary (and a shred lane's) opens in the shared cells as before.
//!
//! Off: a statement that releases its dictionaries (D2985's own cells; D3268's uses-only release keeps it on), an entries window, a sample, no bind-time zone
//! predicate (or the zone verdicts off), `PGRUST_GRAVITY_KEPT_DICTS=0`. Fires `dict.kept` (a kept-granule table opened)
//! and `dict.kept_whole` (kept mode, the whole part's).

use super::*;
use pgsync::atomic::{AtomicPtr, AtomicU8};

/// `SiteKept::home`: not decided yet, the site's own kept table, the list's cells (a coded part, or the whole table).
const HOME_UNKNOWN: u8 = 0;
const HOME_OWN: u8 = 1;
const HOME_LIST: u8 = 2;

/// One code lane's cells of a site in kept mode: per part its home and its kept table.
struct LaneKept {
    homes: StmtSlice<AtomicU8>,
    cells: StmtSlice<AtomicPtr<DictEntry>>,
}

/// One site's kept-mode code tables in this execution, per code lane and part.
pub(super) struct SiteKept {
    lanes: StmtSlice<OnceLock<LaneKept>>,
    nparts: usize,
}

impl SiteKept {
    pub(super) fn new(natts: usize, nparts: usize) -> SiteKept {
        SiteKept { lanes: StmtSlice::collect((0..natts).map(|_| OnceLock::new())), nparts } // alloc: Exec charged gravity_driver::stmt_context()
    }

    fn lane(&self, k: usize) -> &LaneKept {
        let n = self.nparts;
        self.lanes[k].get_or_init(|| LaneKept { homes: StmtSlice::collect((0..n).map(|_| AtomicU8::new(HOME_UNKNOWN))), cells: StmtSlice::collect((0..n).map(|_| AtomicPtr::new(core::ptr::null_mut()))) }) // alloc: Exec charged gravity_driver::stmt_context()
    }

    #[inline]
    fn home(&self, p: usize, k: usize) -> u8 {
        self.lanes.get(k).and_then(OnceLock::get).and_then(|l| l.homes.get(p)).map_or(HOME_UNKNOWN, |h| h.load(Ordering::Acquire))
    }

    fn set_home(&self, p: usize, k: usize, home: u8) {
        self.lane(k).homes[p].store(home, Ordering::Release);
    }

    #[inline]
    pub(super) fn get(&self, p: usize, k: usize) -> Option<&DictEntry> {
        raw::dict_cell_get(self.lanes.get(k)?.get()?.cells.get(p)?)
    }

    /// Publish `entry` as code lane `k`'s kept table of part `p` (a racing open's entry wins; this one drops).
    fn set(&self, p: usize, k: usize, entry: DictEntry) -> &DictEntry {
        raw::dict_cell_publish(&self.lane(k).cells[p], entry).0
    }
}

impl Drop for SiteKept {
    fn drop(&mut self) {
        // bounded("other: the execution's end drops every table it published, one free each; a teardown runs to its end (a Drop cannot stop), as PG's MemoryContextDelete at the executor's end polls no deeper")
        for l in self.lanes.iter().filter_map(OnceLock::get) {
            // bounded("other: the execution's end drops every table it published, one free each; a teardown runs to its end (a Drop cannot stop), as PG's MemoryContextDelete at the executor's end polls no deeper")
            for c in l.cells.iter() {
                // the execution is over: no source reads its tables
                drop(raw::dict_cell_take(c));
            }
        }
    }
}

impl GranuleRangeSource {
    /// Kept mode for this execution of the site (set at bind; the same in every source of the site).
    pub(super) fn kept_mode_of(&self) -> bool {
        crate::inputs::kept_dicts_enabled()
            && self.rel_unbound().stmt.as_deref().is_none_or(|s| !s.releases_dicts())
            && self.recipe.entries.is_none()
            && self.recipe.sample.is_none()
            && self.nzone_bind > 0
            && self.bound_ok
            && self.zone_on()
            && self.resolved_unbound().kept.get(self.site).is_some()
    }

    /// D3038 (A): lens mode for this execution of the site (set at bind; the same in every source of the site): a code
    /// lane it reads only through `OCTET_LENGTH` tables and the emptiness test (recipe `lens_only`) takes its table of a
    /// verbatim part that sealed the column's octet-length lane from the site's own cells, coded by length (`open_lens`).
    /// Off as kept mode is: a statement that releases its dictionaries, an entries window, a sample.
    pub(super) fn lens_mode_of(&self) -> bool {
        self.recipe.lens_only.words().iter().any(|&w| w != 0)
            && self.rel_unbound().stmt.as_deref().is_none_or(|s| !s.releases_dicts())
            && self.recipe.entries.is_none()
            && self.recipe.sample.is_none()
            && self.resolved_unbound().kept.get(self.site).is_some()
    }

    /// The site's kept cells, in kept mode or lens mode.
    #[inline]
    pub(super) fn kept_cells<'r>(&self) -> Option<&'r SiteKept> {
        if !self.kept_mode && !self.lens_mode {
            return None;
        }
        let nparts = self.list.as_ref().map_or(0, |l| l.len());
        let natts = self.recipe.natts;
        let sk: &'r Box<SiteKept> = self.resolved_unbound().kept.get(self.site)?.get_or_init(|| Box::new(SiteKept::new(natts, nparts))); // alloc: Exec
        Some(&**sk)
    }

    /// Whether code lane `k` of part `p` is a kept-mode table's: a column lane (no shred) whose part stored it verbatim
    /// (this source's cursor over it; `None` while the cursor is not open).
    #[inline]
    fn kept_lane(&self, p: usize, k: usize) -> Option<bool> {
        if self.recipe.kinds[k] != lane::KIND_CODES || self.recipe.shred_of(k).is_some() {
            return Some(false);
        }
        let cur = self.cursors.get(p * self.recipe.natts + k)?.as_deref()?;
        Some(cur.entry().encoding != EncodingId::DictCodes as u16)
    }

    /// The dictionary of code lane `k` in part `p` this source reads: in kept mode a verbatim part's kept table from the
    /// site's own cells (none until the site opens it there, never the list's), any other from the statement's cells or
    /// the list's warm cell (D4410: then the statement's too).
    #[inline]
    pub(super) fn dict_at<'r>(&self, p: usize, k: usize) -> Option<&'r DictEntry> {
        // lens mode alone decides the homes of its length-only lanes; kept mode every code lane's
        if let Some(sk) = self.kept_cells().filter(|_| self.kept_mode || self.recipe.lens_only.has(k)) {
            match sk.home(p, k) {
                HOME_OWN => return sk.get(p, k),
                HOME_LIST => {}
                // undecided: a coded part's dictionary is the list's; a verbatim one's waits for this site's open
                _ if self.kept_lane(p, k) == Some(false) => {}
                _ => return None,
            }
        }
        // D4410: the statement's own, else the list's warm entry, which the statement holds from here on
        let (rp, j) = (self.rel_unbound(), self.face(k));
        rp.dict_if_open(p, j).or_else(|| rp.adopt_warm(p, j, self.renders_derived(k)))
    }

    /// D3038 (A): lens mode at the open of code lane `k` of part `p` (this source's cursor over it open). A length-only
    /// lane (recipe `lens_only`) of a column lane whose part stored it verbatim and sealed its octet-length lane opens the
    /// site's own table coded by length from that lane (`text_hosts::lane_codes`: the lane's words, never the column's
    /// text), HOME_OWN: true. Any other length-only lane's part is false, its home the list's cells unless kept mode
    /// decides it next (`open_kept`); any other lane is false and left alone.
    pub(super) fn open_lens(&mut self, p: usize, k: usize, part: &Arc<OpenPart>) -> PgResult<bool> {
        if !self.recipe.lens_only.has(k) || self.recipe.shred_of(k).is_some() {
            return Ok(false);
        }
        let Some(sk) = self.kept_cells() else { return Ok(false) };
        match sk.home(p, k) {
            HOME_LIST => return Ok(false),
            // the site's own table (this lens open's, or kept mode's for a part with no octet lane): open once published
            HOME_OWN if sk.get(p, k).is_some() => return Ok(true),
            _ => {}
        }
        let lane = match (self.kept_lane(p, k), self.parts().list.faces.locate(self.face(k), p, part)?) {
            (Some(true), Some((attno, 0))) => octet_lane_ord(part, attno)?.map(|o| (attno, o)),
            _ => None,
        };
        let Some((attno, ord)) = lane else {
            if !self.kept_mode && sk.home(p, k) == HOME_UNKNOWN {
                sk.set_home(p, k, HOME_LIST);
            }
            return Ok(false);
        };
        sk.set_home(p, k, HOME_OWN);
        if sk.get(p, k).is_some() {
            return Ok(true);
        }
        if let Some(e) = mcx::governor::refusal("pgrc part cache") {
            return Err(Box::<types_error::PgError>::new(e)); // alloc: Cold
        }
        fires::fire_shared("octet.lens_coded"); // shapecheck fire signal (D3038 A): a verbatim part's code lane coded by length
        let binding = pgrc_am::scan::codec_binding();
        let mut cur = StreamCursor::open(Arc::clone(part), binding, attno, ord).map_err(read_err)?;
        cur.set_owner(self.held.owner());
        let table = crate::dict::text_hosts::lane_codes(part, attno, &mut cur).map_err(table_err)?;
        let ranks = table.ranks().expect("ranks");
        // its codes are not the part's byte ranks: its own epoch, so no memo or once-cell keyed by the part's reads it
        let epoch = part.dict_epoch(attno, 0) ^ ranks.salt().rotate_left(29);
        sk.set(p, k, DictEntry::new(Dict::Ranked { table: crate::dict::text_hosts::FpPin::of(&table), epoch }));
        Ok(true)
    }

    /// D3038 (A): code lane `k` of part `p` is (or opens) coded by length (`open_lens`), so a claim reads none of its column
    /// stream's bytes (read.claim and read.advise skip the lane): its codes are the length table's, and only its validity
    /// is read, on demand. The lanes' cursors are open. A lane the open then refuses (the governor) reads its stream on
    /// demand instead, so the answer is never this guess's.
    pub(super) fn lens_unread(&self, p: usize, k: usize) -> bool {
        if !self.lens_mode || !self.recipe.lens_only.has(k) || self.recipe.shred_of(k).is_some() {
            return false;
        }
        let Some(sk) = self.kept_cells() else { return false };
        match sk.home(p, k) {
            HOME_LIST => return false,
            HOME_OWN => {
                return sk.get(p, k).is_some_and(|e| matches!(&e.dict, Dict::Ranked { table, .. } if table.ranks().is_some_and(|r| r.by_length().is_some())));
            }
            _ => {}
        }
        let Some(part) = self.parts().part_if_open(p) else { return false };
        match (self.kept_lane(p, k), self.parts().list.faces.locate(self.face(k), p, part)) {
            (Some(true), Ok(Some((attno, 0)))) => octet_lane_ord(part, attno).ok().flatten().is_some(),
            _ => false,
        }
    }

    /// Kept mode at the open of code lane `k` of part `p` (this source's cursor over it open): decide its home; true when
    /// the site's own kept table is now open (HOME_OWN), false for the list's cells (the caller opens the dictionary there).
    /// `real` is part `p` itself: the zone lanes' Stats read from it; lane `k` reads its stand-in when the part lacks its
    /// column (FASTADD, `RelParts::lane`).
    pub(super) fn open_kept(&mut self, p: usize, k: usize, real: &Arc<OpenPart>) -> PgResult<bool> {
        let Some(sk) = self.kept_cells() else { return Ok(false) };
        if sk.home(p, k) == HOME_LIST {
            return Ok(false);
        }
        let part = &Arc::clone(self.parts().lane(p, self.face(k), real)?);
        let Some((attno, ord)) = self.parts().list.faces.locate(self.face(k), p, part)? else { return Ok(false) };
        let kept = match self.kept_lane(p, k) {
            Some(true) if ord == 0 => {
                let kept = self.kept_of(p, real)?;
                if kept.is_none() {
                    fires::fire_shared("dict.kept_whole"); // shapecheck fire signal (D3156): kept mode, a verbatim part's whole table
                }
                kept
            }
            _ => None,
        };
        let Some(kept) = kept else {
            sk.set_home(p, k, HOME_LIST);
            return Ok(false);
        };
        sk.set_home(p, k, HOME_OWN);
        if sk.get(p, k).is_some() {
            return Ok(true);
        }
        if let Some(e) = mcx::governor::refusal("pgrc part cache") {
            return Err(Box::<types_error::PgError>::new(e)); // alloc: Cold
        }
        fires::fire_shared("dict.kept"); // shapecheck fire signal (D3156): a verbatim part's code table over its kept granules
        let cur = self.cursors[p * self.recipe.natts + k].as_deref_mut().expect("cursor");
        let table = crate::dict::text_hosts::row_codes(part, cur, stream_key(attno, ord), &mut self.scratch, Some(&kept)).map_err(table_err)?;
        let ranks = table.ranks().expect("ranks");
        // a kept table's codes are not the whole part's: its own epoch, so no memo or once-cell keyed by the part's reads it
        let epoch = part.dict_epoch(attno, ord) ^ ranks.salt().rotate_left(29);
        sk.set(p, k, DictEntry::new(Dict::Ranked { table: crate::dict::text_hosts::FpPin::of(&table), epoch }));
        Ok(true)
    }

    /// Part `p`'s granules the bind's zone predicates keep by the part's Stats records alone (read whole here first, so
    /// every source of the site computes the same set), when they are at most 1 / `KEPT_GRANULES_DIV` of its granules;
    /// `None`: the whole part's table.
    fn kept_of(&self, p: usize, part: &Arc<OpenPart>) -> PgResult<Option<crate::dict::text_hosts::Kept>> {
        let (n, granules) = (self.nzone_bind.min(self.zone.len()), part.footer().granule_count);
        if n == 0 || granules == 0 {
            return Ok(None);
        }
        let rp = self.parts();
        // a tick per Stats body read and per granule tested
        let mut step = cancelpoll::step("granule_source: a part's kept granules");
        for z in &self.zone[..n] {
            step.tick().map_err(|_| err(WALK_CANCELED))?;
            rp.body(p, self.face(z.lane as usize), part)?;
        }
        let mut bits = vec![0u64; granules.div_ceil(64) as usize].into_boxed_slice(); // alloc: Exec
        let mut kept = 0u64;
        for lg in 0..granules {
            step.tick().map_err(|_| err(WALK_CANCELED))?;
            let rows = self.granule_rows(p, lg);
            let none = self.zone[..n].iter().enumerate().any(|(i, z)| self.facts_of(p, z.lane as usize, lg).is_some_and(|rec| self.text_verdict(i, &rec, rows, z) == V_NONE));
            if !none {
                bits[lg as usize / 64] |= 1 << (lg % 64);
                kept += 1;
            }
        }
        Ok((kept * gravity_program::cost::pgrc::KEPT_GRANULES_DIV <= granules as u64).then(|| crate::dict::text_hosts::Kept::new(bits, granules)))
    }

    /// Granule `lg` of part `p` lies outside a kept table of one of the site's code lanes: no row of it passes the bind's
    /// zone predicates, so the window is skipped before its code lanes are read.
    pub(super) fn kept_skips(&self, p: usize, lg: u32) -> bool {
        let Some(sk) = self.kept_cells() else { return false };
        (0..self.recipe.natts).any(|k| match sk.get(p, k).map(|e| &e.dict) {
            Some(Dict::Ranked { table, .. }) => !table.ranks().expect("ranks").covers(lg),
            _ => false,
        })
    }
}
