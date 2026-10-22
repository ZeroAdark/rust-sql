//! D2985: a streaming statement's dictionaries, released part by part.
//!
//! A relation's dictionaries live in its `PartList`'s face cells, shared by every statement that holds the list and kept
//! for the list's generation (`Faces`): a scan that renders large text columns held each part's dictionary images (~65 MB
//! per wide text column per part) for every part it read, tens of GB over a `COPY (SELECT * ...) TO STDOUT` of a wide
//! table. A statement whose only pipeline streams rows into its sink (`Resolved::set_part_release`: one site, no
//! breaker, the sink takes each row before the next window: a worker's leg copies every by-reference image at push, a serial
//! run's receiver writes the row out) never reads a part again once every claim left it. Such a statement opens its
//! dictionaries in its own cells here instead; each source credits the granules of a part as its claim leaves the part, and
//! the claim that completes the part's granules drops the part's dictionaries and its `PartUse`, so the process part cache
//! (D3003, `pgrcolumnar_read::partcache`) may give the images back under its budget. The shared cells are never touched.
//!
//! The list's shared cells stay the first home: while the process part cache (D3003, whose census counts the dictionary
//! images) is under its budget, a streaming statement opens its dictionaries there as any statement does, so a hot repeat
//! finds them; only past the budget does it open them in its own cells and release them.
//!
//! D3268: an aggregate's scan (`Resolved::set_part_uses_release`: its one site feeds a PlainAgg / HashAgg breaker, every
//! later pipeline reads breaker slots only) releases its parts' USES the same way, never its dictionaries: they stay in
//! the list's cells, where its kept code words render at the emit and a hot repeat finds them, and kept / lens mode tables
//! (D3156, D3038) stay on. Only the part's segment and part-grain images may then go back under the budget once every claim
//! left the part; before, an aggregate kept every part it read in use to its end (one statement held 22 GB of part cache
//! past a 3.6 GB budget). An images pointer an aggregate keeps past the part (a render, a text group's key) is a
//! dictionary's, owned by the list's cell, as across statements today.
//!
//! D3823: every breaker over one site releases the same way (any breaker kind: `drive::part_release`). What a breaker
//! keeps by reference past its window is a dictionary's (the list's cell owns it through the statement; a render after
//! the release reads it from its handle, never holding the part again: `dict_value_pg`), all else is copied at the
//! retain. A source's cursors over a released part close at its unbind (`close_released`), so a parked Node does not keep
//! the part in use.
//!
//! A part opened again after its release (a rescan's second pass) is counted (`reopened`): a plain scan opens none.
//! Debug builds (and `PGRUST_GRAVITY_RELEASE_POISON=1`) poison a released block-lazy image the statement held alone, so a
//! datum read after its part's release reads 0xDB bytes, not the dictionary's; a uses-only release (D3823) poisons the
//! part's segment images no holder shares once no use of the part is left.

use super::*;
use pgsync::atomic::{AtomicBool, AtomicPtr, AtomicU32};

static PARTS_RELEASED: AtomicU64 = AtomicU64::new(0);
static DICTS_REOPENED: AtomicU64 = AtomicU64::new(0);

/// `(parts released, dictionaries re-opened after their part's release)` since process start (the PHASES census).
pub fn part_release_census() -> (u64, u64) {
    (PARTS_RELEASED.load(Ordering::Relaxed), DICTS_REOPENED.load(Ordering::Relaxed))
}

/// The process part cache's `(bytes, budget, held)` in MiB (the PHASES line).
pub fn part_cache_mib() -> (u64, u64, u64) {
    (pgrc_read::partcache::bytes() >> 20, pgrc_read::partcache::budget() >> 20, part_cache_held() >> 20)
}

/// The part cache's images the governor's account still charges past the resident census: images a holder keeps after
/// their eviction (the pgrc line's held class). 0 here, where the account never exceeds the resident bytes (D3113).
pub fn part_cache_held() -> u64 {
    (mcx::governor::cache_bytes() as u64).saturating_sub(pgrc_read::partcache::bytes())
}

/// One statement's dictionary cells over one relation's parts (`RelParts::stmt`).
pub(super) struct StmtDicts {
    /// Per face (allocated on the face's first open), per part: the entry, or null (never opened, or released).
    faces: StmtSlice<OnceLock<StmtSlice<AtomicPtr<DictEntry>>>>,
    /// One bit per allocated face, set before the face publishes: a part's release walks these, never every face slot.
    touched: StmtSlice<AtomicU64>,
    nparts: usize,
    /// Each part's granules times the relation's readers (D3823 R3b: every site over the relation reads each granule once),
    /// and those the statement's claims left so far.
    granules: StmtSlice<u64>,
    done: StmtSlice<AtomicU64>,
    /// Each part's releases: a source that finds it moved since it last entered the part starts its memos over.
    epoch: StmtSlice<AtomicU32>,
    /// The statement's use of each part (D3003 `PartUse`), dropped at the part's release.
    uses: StmtSlice<Mutex<Option<pgrc_read::PartUse>>>,
    held: StmtSlice<AtomicBool>,
    released: AtomicU64,
    reopened: AtomicU64,
    /// Dictionaries this statement opened in its own cells (past the part cache's budget).
    private: AtomicU64,
    /// The part-cache level under which dictionaries go to the list's cells (`None`: the process budget).
    share: Option<u64>,
    /// The statement's part re-use token (`Resolved::token`).
    token: u64,
    /// D2985: the statement's dictionaries go to its own cells past `share` and are released with the part; false
    /// (D3268, an aggregate's scan): the list's cells always, only the parts' uses are released.
    dicts: bool,
    /// D3823: a uses-only release whose breaker keeps no dictionary image by reference (`mempolicy::release::
    /// dicts_go_with_part`): its dictionaries go to its own cells past `share` too and go with their part's release, which
    /// keeps the uses-only rules (the gate, kept mode).
    own_dicts: bool,
    /// D3823 gate: the part-cache level past which a uses-only release gives its part back (`release_due`); `auto`: the
    /// cache's budget at each check (a portal and tests set a level: 0 always, `u64::MAX` never).
    release_at: AtomicU64,
    release_auto: AtomicBool,
    /// D3823 gate: the parts whose claims are done that a check under the level kept (`defer`), and how many.
    deferred: StmtSlice<AtomicBool>,
    ndeferred: AtomicU32,
}

impl StmtDicts {
    pub(super) fn new(list: &PartList, share: Option<u64>, token: u64, dicts: bool, own_dicts: bool, readers: u32, release_at: Option<u64>) -> StmtDicts {
        let n = list.len();
        StmtDicts {
            faces: StmtSlice::collect((0..MAX_FACE_ATTNO + MAX_SHRED_FACES).map(|_| OnceLock::new())), // alloc: Exec charged gravity_driver::stmt_context()
            touched: StmtSlice::collect((0..(MAX_FACE_ATTNO + MAX_SHRED_FACES).div_ceil(64)).map(|_| AtomicU64::new(0))), // alloc: Exec charged gravity_driver::stmt_context()
            nparts: n,
            granules: StmtSlice::collect(list.parts().iter().map(|p| p.granules as u64 * readers.max(1) as u64)), // alloc: Exec charged gravity_driver::stmt_context()
            done: StmtSlice::collect((0..n).map(|_| AtomicU64::new(0))), // alloc: Exec charged gravity_driver::stmt_context()
            epoch: StmtSlice::collect((0..n).map(|_| AtomicU32::new(0))), // alloc: Exec charged gravity_driver::stmt_context()
            uses: StmtSlice::collect((0..n).map(|_| Mutex::new(None))), // alloc: Exec charged gravity_driver::stmt_context()
            held: StmtSlice::collect((0..n).map(|_| AtomicBool::new(false))), // alloc: Exec charged gravity_driver::stmt_context()
            released: AtomicU64::new(0),
            reopened: AtomicU64::new(0),
            private: AtomicU64::new(0),
            share,
            token,
            dicts,
            own_dicts: dicts || own_dicts,
            release_at: AtomicU64::new(release_at.unwrap_or(0)),
            release_auto: AtomicBool::new(release_at.is_none()),
            deferred: StmtSlice::collect((0..n).map(|_| AtomicBool::new(false))), // alloc: Exec charged gravity_driver::stmt_context()
            ndeferred: AtomicU32::new(0),
        }
    }

    /// D3823 gate: the level `release_due` reads from now on (`None`: the cache's budget).
    pub(super) fn set_release_at(&self, level: Option<u64>) {
        self.release_at.store(level.unwrap_or(0), Ordering::Relaxed);
        self.release_auto.store(level.is_none(), Ordering::Relaxed);
    }

    /// D3823 gate: a part every claim left goes back now (`mempolicy::release::part_release_due`: the part cache is past
    /// its budget, so the release lets the trim evict it), or stays held while the cache has room: a release there evicts
    /// nothing (a trim runs only past the budget) and costs the statement's next run the cursors and memos it opens again.
    /// D3994 (level gate): at the cache's own level (`release_auto`) the policy's short signal is due too (`residency::release_short`):
    /// the deferred parts were the cache's floor, so inside its budget they never came due (a set level is the caller's).
    /// D2985's own dictionaries (`dicts`) keep their rule (`shares`): due always.
    pub(super) fn release_due(&self) -> bool {
        if self.dicts {
            return true;
        }
        let bytes = pgrc_read::partcache::bytes();
        if self.release_auto.load(Ordering::Relaxed) {
            let level = pgrc_read::partcache::budget();
            // the short signal only when the level alone keeps the part: one load past the budget
            return mempolicy::release::part_release_due(bytes, level, bytes <= level && crate::ops::scan::pgrc::residency::release_short());
        }
        mempolicy::release::part_release_due(bytes, self.release_at.load(Ordering::Relaxed), false)
    }

    /// D3823 gate: every claim left part `p` while the cache had room: it stays held until a later check finds the cache
    /// past the level (`take_deferred`) or the statement ends. D4315: the statement's use marks the part deferred, so the
    /// part cache's floor leaves it out (`OpenPart::live_claimed`): in the floor it lifted the broker's target, the level
    /// `release_due` reads, and so held itself to the statement's end.
    pub(super) fn defer(&self, p: usize) {
        if !self.deferred[p].swap(true, Ordering::AcqRel) {
            self.ndeferred.fetch_add(1, Ordering::AcqRel);
        }
        if let Some(u) = self.uses[p].lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            u.defer();
        }
    }

    /// D4315: a source enters part `p` again (a later pass): a part this statement deferred is in a live claim's read, so
    /// back in the cache's floor until its claims leave it again (`defer`). The gate's own hold (`take_deferred`) is
    /// unchanged. One load while the statement deferred nothing.
    pub(super) fn reclaim_entered(&self, p: usize) {
        if self.ndeferred.load(Ordering::Acquire) == 0 || !self.deferred[p].load(Ordering::Acquire) {
            return;
        }
        if let Some(u) = self.uses[p].lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            u.undefer();
        }
    }

    /// D3823 gate: the parts `defer` kept, each handed out once (the caller releases them); one load while none is kept.
    pub(super) fn take_deferred(&self) -> impl Iterator<Item = usize> + '_ {
        let n = if self.ndeferred.load(Ordering::Acquire) > 0 { self.nparts } else { 0 };
        (0..n).filter(move |&p| {
            let took = self.deferred[p].swap(false, Ordering::AcqRel);
            if took {
                self.ndeferred.fetch_sub(1, Ordering::AcqRel);
            }
            took
        })
    }

    /// The statement's dictionaries are its own past the level and released with their part (D2985); false: only the
    /// parts' uses are (D3268).
    pub(super) fn releases_dicts(&self) -> bool {
        self.dicts
    }

    /// D2985 / D3823: the statement's dictionaries go with their part (a streaming statement's, or a code-word breaker's
    /// but for a lane that renders through a derived table). D4410: at any level; under it the list keeps them warm too.
    pub(super) fn owns_dicts(&self) -> bool {
        self.own_dicts
    }

    /// A dictionary opened now goes to the list's cells: the process part cache (whose census counts its image) is under
    /// the level (D3003's budget).
    pub(super) fn shares(&self) -> bool {
        if !self.own_dicts {
            return true;
        }
        // the governor's account keeps evicted images a holder still keeps (the pgrc line's held class) until their last drop
        pgrc_read::partcache::bytes().max(mcx::governor::cache_bytes() as u64) < self.share.unwrap_or_else(pgrc_read::partcache::budget)
    }

    fn cells(&self, face: u32) -> &[AtomicPtr<DictEntry>] {
        self.faces[face as usize].get_or_init(|| {
            self.touched[face as usize / 64].fetch_or(1 << (face % 64), Ordering::Release);
            StmtSlice::collect((0..self.nparts).map(|_| AtomicPtr::new(core::ptr::null_mut()))) // alloc: Exec charged gravity_driver::stmt_context()
        })
    }

    fn touched_faces(&self) -> impl Iterator<Item = &[AtomicPtr<DictEntry>]> + '_ {
        self.touched.iter().enumerate().flat_map(|(w, word)| {
            let mut bits = word.load(Ordering::Acquire);
            core::iter::from_fn(move || {
                if bits == 0 {
                    return None;
                }
                let b = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                Some(w * 64 + b)
            })
        })
        .filter_map(|f| self.faces[f].get().map(|c| &**c))
    }

    pub(super) fn get(&self, p: usize, face: u32) -> Option<&DictEntry> {
        raw::arc_cell_get(&self.faces[face as usize].get()?[p])
    }

    /// Hold `entry` as part `p`'s face `face` until the part's release (a racing reader's entry wins; this clone drops).
    /// D4410: the cells hold an `Arc` each; `private`: the list's warm cell keeps no clone (opened past the level);
    /// `opened`: this statement opened the dictionary (a clone of the warm cell's is no re-open).
    pub(super) fn set(&self, p: usize, face: u32, entry: Arc<DictEntry>, private: bool, opened: bool) -> &DictEntry {
        let (e, published) = raw::arc_cell_publish(&self.cells(face)[p], entry);
        self.private.fetch_add((published && private) as u64, Ordering::Relaxed);
        if published && opened && self.epoch[p].load(Ordering::Relaxed) > 0 {
            self.reopened.fetch_add(1, Ordering::Relaxed);
            DICTS_REOPENED.fetch_add(1, Ordering::Relaxed);
        }
        e
    }

    /// The statement uses part `p` (its contents are not evicted while it does).
    #[inline]
    pub(super) fn hold(&self, p: usize, part: &Arc<OpenPart>) {
        if self.held[p].load(Ordering::Acquire) {
            return;
        }
        let mut u = self.uses[p].lock().unwrap_or_else(|e| e.into_inner());
        if u.is_none() {
            *u = Some(part.use_part_by_row(self.token));
        }
        self.held[p].store(true, Ordering::Release);
    }

    pub(super) fn epoch(&self, p: usize) -> u32 {
        self.epoch[p].load(Ordering::Acquire)
    }

    /// Credit `n` granules of part `p` that a claim left; true when they complete the part (a pass's last claim over it, of
    /// every reader's pass: D3823 R3b).
    pub(super) fn credit(&self, p: usize, n: u64) -> bool {
        let g = self.granules[p];
        let now = self.done[p].fetch_add(n, Ordering::AcqRel) + n;
        g > 0 && now % g == 0
    }

    /// Drop part `p`'s dictionaries and the statement's use of it: every claim of the pass left the part.
    pub(super) fn release(&self, p: usize) {
        let poison = crate::inputs::release_poison();
        // bounded("key-columns: the dictionary faces the statement touched, one per scanned column's lane or shred face, at most MAX_FACE_ATTNO + MAX_SHRED_FACES (1665); each drops one part's dictionary")
        for f in self.touched_faces() {
            // no claim of the pass reads part p any more
            let Some(e) = raw::arc_cell_take(&f[p]) else { continue };
            // D4410: an entry the list's warm cell (or another holder) keeps too is no released image: never poisoned
            if poison && Arc::strong_count(&e) == 1 {
                if let Dict::Part(d) = &e.dict {
                    d.debug_poison();
                }
            }
            drop(e);
        }
        self.held[p].store(false, Ordering::Release);
        let u = self.uses[p].lock().unwrap_or_else(|e| e.into_inner()).take();
        // D3823: a uses-only release poisons the part's segment images no holder shares once no use of the part is left, so a
        // by-reference datum a breaker kept past its window (D3003: its retain must have copied it) reads 0xDB bytes
        let part = (poison && !self.dicts).then(|| u.as_ref().map(|u| Arc::clone(u.part()))).flatten();
        drop(u);
        if let Some(part) = part.filter(|part| part.users() == 0) {
            part.debug_poison_contents();
        }
        self.epoch[p].fetch_add(1, Ordering::AcqRel);
        self.released.fetch_add(1, Ordering::Relaxed);
        PARTS_RELEASED.fetch_add(1, Ordering::Relaxed);
    }

    /// `(parts released, dictionaries re-opened)` in this statement.
    pub(super) fn private_opens(&self) -> u64 {
        self.private.load(Ordering::Relaxed)
    }

    pub(super) fn counts(&self) -> (u64, u64) {
        (self.released.load(Ordering::Relaxed), self.reopened.load(Ordering::Relaxed))
    }
}

impl Drop for StmtDicts {
    fn drop(&mut self) {
        // bounded("other: the statement's end drops every dictionary it published, one free per face and part; a teardown runs to its end (a Drop cannot stop), as PG's MemoryContextDelete at the executor's end polls no deeper")
        for f in self.touched_faces() {
            // bounded("other: the statement's end drops every dictionary it published, one free per face and part; a teardown runs to its end (a Drop cannot stop), as PG's MemoryContextDelete at the executor's end polls no deeper")
            for c in f.iter() {
                // the statement is over
                drop(raw::arc_cell_take(c));
            }
        }
    }
}

impl GranuleRangeSource {
    /// D2985: this source's claim leaves part `q`, the part it was in (it entered another part, or the claim ended): credit
    /// the claim's granules in `q`; the claim that completes them releases the part's dictionaries.
    pub(super) fn part_leave(&mut self, walk: &mut cancelpoll::Step, q: usize) {
        if q == usize::MAX || self.order.is_some() || self.recipe.sample.is_some() {
            return;
        }
        let rp = self.rel_unbound();
        let Some(sd) = rp.stmt.as_deref() else { return };
        let list = &*rp.list;
        if q >= list.len() {
            return;
        }
        let from = self.credit_from.max(list.starts()[q]);
        let until = self.hi.min(list.end_of(q));
        if until <= from {
            return;
        }
        self.credit_from = until;
        // D3823 gate: while the part cache has room a part whose claims are done stays held, its cursors open for the
        // statement's next run; past the level it goes back, and so do the parts kept before
        let due = sd.release_due();
        if due && until == list.end_of(q) {
            // this claim ran to the part's end: no later claim of the pass reads it, so this source's cursors close now
            self.close_cursors(q);
        }
        let mut gave = false;
        if sd.credit(q, until - from) {
            if due {
                sd.release(q);
                gave = true;
            } else {
                sd.defer(q);
            }
        }
        if due {
            // a tick per part released; a cancel leaves the rest deferred (the statement's end releases them)
            for p in sd.take_deferred() {
                self.close_cursors(p);
                sd.release(p);
                gave = true;
                if walk.tick().is_err() {
                    break;
                }
            }
        }
        if gave {
            self.recipe.registry.trim_if_over();
        }
    }

    /// D2985: this source enters part `p`. Claims are cut in ascending granules, so the parts below `p` this source still
    /// holds cursors over are behind every later claim of the pass: their cursors (each a `PartUse`) close. A part released
    /// since this source last entered it (a second pass) has its per-part memos started over.
    pub(super) fn part_enter(&mut self, walk: &mut cancelpoll::Step, p: usize) {
        let rp = self.rel_unbound();
        let Some(sd) = rp.stmt.as_deref() else { return };
        if p < self.cursor_low {
            self.cursor_low = p;
        }
        // D3823 gate: the parts behind the claim keep their cursors while the cache has room (the next run reads them
        // again); a later entry past the level closes them all
        if self.cursor_low < p && sd.release_due() {
            // a tick per part closed; a cancel leaves the rest to a later entry or the statement's end
            while self.cursor_low < p {
                self.close_cursors(self.cursor_low);
                self.cursor_low += 1;
                if walk.tick().is_err() {
                    break;
                }
            }
        }
        sd.reclaim_entered(p);
        if self.seen_epoch.len() < sd.nparts {
            self.seen_epoch.resize(sd.nparts, 0); // alloc: Node
        }
        let e = sd.epoch(p);
        if self.seen_epoch[p] != e {
            self.seen_epoch[p] = e;
            self.renders.clear();
            self.sparse_tab = None;
            self.cond_entry = None;
            self.tables.pinned.clear();
            self.tables.part = usize::MAX;
        }
    }

    /// This source's cursors over part `q` close (each holds a `PartUse`, its character-count cursors too); the part's lanes
    /// open again on a later entry.
    pub(super) fn close_cursors(&mut self, q: usize) {
        let natts = self.recipe.natts;
        if q < self.lanes_open.len() {
            self.lanes_open[q] = false;
        }
        let mut closed = 0;
        // bounded("key-columns: one part's cursors, one per scanned column, at most MaxTupleAttributeNumber (1664) columns")
        for c in self.cursors.iter_mut().skip(q * natts).take(natts) {
            closed += c.take().is_some() as usize;
        }
        // bounded("key-columns: one part's character sources, one per scanned column, at most MaxTupleAttributeNumber (1664) columns")
        for c in self.char_src.iter_mut().skip(q * natts).take(natts) {
            closed += c.take().is_some() as usize;
        }
        self.cursors_held -= closed * super::CURSOR_BYTES;
    }

    /// D3823: the statement's end (unbind): this source's cursors over the parts its statement released close. A claim that
    /// ended inside a part kept them (a later claim of this source could have read on), and a parked Node's cursors (D1507)
    /// would keep a released part in use with no statement running, past the release that let it go. The parts the
    /// statement kept (a LIMIT stopped its claims) keep their cursors for the next bind.
    pub(super) fn close_released(&mut self) {
        if !self.bound {
            return;
        }
        let rp = self.rel_unbound();
        let Some(sd) = rp.stmt.as_deref() else { return };
        // D4554: the parts this source holds cursors over (`held_parts`), not every part of the list
        // bounded("other: the statement's end closes this source's cursors over the parts it released, one check per part it holds cursors over; it runs to its end (a stop would keep a released part in use, D3823), as PG's ExecEndNode polls no deeper")
        for i in 0..self.held_parts.len() {
            let q = self.held_parts[i] as usize;
            if q < sd.nparts && sd.epoch(q) > 0 && self.holds_part(q) {
                self.close_cursors(q);
            }
        }
    }

    /// `(parts released, dictionaries re-opened)` in this statement over this source's relation.
    pub fn part_release_counts(&self) -> (u64, u64) {
        if !self.bound {
            return (0, 0);
        }
        self.rel_unbound().stmt.as_deref().map_or((0, 0), StmtDicts::counts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CONVERT wave 2: the statement's per-part tables are charged to the statement (or portal) from their allocation to
    /// their drop (`StmtSlice`): a part table, and StmtDicts' face table and bitmap at its construction; uncharged with
    /// no statement.
    #[test]
    fn a_statements_part_tables_are_charged_to_it_until_they_drop() {
        std::thread::spawn(|| {
            let none = StmtSlice::collect((0..64).map(|_| AtomicU64::new(0)));
            assert_eq!(none.len(), 64, "no statement: uncharged, still the slice");
            memledger::bind_backend(3995, 0);
            memledger::statement_begin();
            let (stmt, _) = memledger::charge::context();
            let c = || memledger::charge::owner_row(stmt).committed;
            let cells = StmtSlice::collect((0..1000).map(|_| AtomicPtr::<DictEntry>::new(core::ptr::null_mut())));
            assert_eq!(c(), 8000, "a part table's bytes on the statement");
            let d = StmtDicts::new(&PartList::new(Vec::new()), None, pgrc_read::partcache::next_stmt_token(), true, false, 1, None); // alloc: Cold
            let fixed = (core::mem::size_of_val(&*d.faces) + core::mem::size_of_val(&*d.touched)) as i64;
            assert!(fixed > 0);
            assert_eq!(c(), 8000 + fixed, "the statement's dictionary cells: the face table and its bitmap");
            drop(d);
            drop(cells);
            assert_eq!(c(), 0, "left at the drops");
            memledger::statement_end();
            memledger::unbind_backend();
            drop(none);
        })
        .join()
        .unwrap();
    }

    /// An image the governor's account still charges after it left the resident census (the pgrc line's held class)
    /// counts against the share level. The margins dwarf any concurrent test's part cache; nothing else in the hosts lib
    /// reads the level.
    #[test]
    fn shares_counts_the_held_images_beside_the_resident_ones() {
        let level = pgrc_read::partcache::bytes().max(mcx::governor::cache_bytes() as u64) + (64 << 30);
        let d = StmtDicts::new(&PartList::new(Vec::new()), Some(level), pgrc_read::partcache::next_stmt_token(), true, false, 1, None); // alloc: Cold
        assert!(d.shares(), "under the level");
        let mut rec = mcx::governor::CacheCharge::default();
        mcx::governor::charge_cache(128 << 30, "pgrc part cache", &mut rec);
        assert!(!d.shares(), "held images count against the level");
        assert!(part_cache_mib().2 >= 64 << 10, "the PHASES line reports them");
        mcx::governor::release_cache(128 << 30, &mut rec);
        assert!(d.shares(), "released");
    }

    /// D3755: a part's release walks only the faces the statement allocated (an aggregate's D3268 release allocates
    /// none), not all `MAX_FACE_ATTNO + MAX_SHRED_FACES` slots per part.
    #[test]
    fn release_walks_only_the_faces_the_statement_allocated() {
        let d = StmtDicts::new(&PartList::new(Vec::new()), None, pgrc_read::partcache::next_stmt_token(), false, false, 1, None); // alloc: Cold
        assert_eq!(d.touched_faces().count(), 0, "a uses-only release opens no face of its own");
        let shred = (MAX_FACE_ATTNO + 2) as u32;
        let last = (MAX_FACE_ATTNO + MAX_SHRED_FACES - 1) as u32;
        for f in [3, 3, 64, shred, last] {
            d.cells(f);
        }
        assert_eq!(d.touched_faces().count(), 4, "each allocated face once");
        assert_eq!(d.touched[0].load(Ordering::Relaxed), 1 << 3);
        assert_eq!(d.touched[1].load(Ordering::Relaxed), 1);
        assert_ne!(d.touched[shred as usize / 64].load(Ordering::Relaxed) & (1 << (shred % 64)), 0);
        assert_ne!(d.touched[last as usize / 64].load(Ordering::Relaxed) & (1 << (last % 64)), 0);
        assert_eq!(d.touched.iter().map(|w| w.load(Ordering::Relaxed).count_ones()).sum::<u32>(), 4);
    }
}
