//! A table's two tombstone heaps (`pg_toast.pgrc_tomb_<p>` and `pg_toast.pgrc_tomb_<p>_1`). New
//! tombstones go to the current one. A switch retires it; one publish folds a retired heap whole into the parts'
//! deletion vectors; once no snapshot can need it, compile resets it to a new, empty relfilenode, and it waits to be
//! current again. No tombstone is ever heap-deleted.
//!
//! - Writers probe the newest generation's current heap and its retired one while unfolded, and insert into the
//!   current one (`tomb_insert` confirms only while it is still current).
//! - Readers read their view's current heap, its retired one unless the view has it folded, and the newest current
//!   heap (the transaction's own tombstones after a switch the view predates).
//! - Probes and scans release their lock when done: only the reset's AccessExclusiveLock waits for them.

use std::cell::{Cell, RefCell};

use mcx::Mcx;
use pgrc_am::catalog::Generation;
use pgrc_format::dml::Tombstone;
use pgrc_format::manifest::{tombs as decode, TombsRecord, TOMB_SLOTS};
use pgrc_front_seams::TombInsert;
use types_core::{CommandId, ForkNumber, Oid};
use types_error::{PgError, PgResult};
use types_rel::{AccessShareLock, NoLock, Relation, RowExclusiveLock, ShareUpdateExclusiveLock};

pub use pgrc_format::manifest::RetiredTombs;

pub fn state(gen: Option<&Generation>) -> PgResult<TombsRecord> {
    match gen {
        None => Ok(TombsRecord::default()),
        Some(g) => decode(&g.manifest.ext).map_err(|e| {
            Box::new(PgError::error(format!("pgrcolumnar: generation {}: {e}", g.gen)).with_sqlstate(types_error::ERRCODE_DATA_CORRUPTED))
        }),
    }
}

pub fn newest_state(rel: &Relation<'_>) -> PgResult<TombsRecord> {
    state(crate::ring::newest(rel)?.as_deref())
}

/// The slots a writer probes: the current heap, and the retired one while unfolded. A folded heap is skipped: its
/// tombstones' inserters were all below the horizon when it folded, so every snapshot that can run sees their rows
/// deleted, and no writer targets them.
pub fn probe_slots(st: &TombsRecord) -> Vec<u8> {
    let mut out = vec![st.cur];
    out.extend(st.ret.filter(|r| r.folded == 0).map(|r| r.slot));
    out
}

/// The newest current heap past a switch the view predates holds only tombstones inserted after that switch: another
/// transaction's is invisible to the snapshot (had its inserter committed before the snapshot, so had the switch), the
/// reader's own are visible.
pub fn read_slots(view: &TombsRecord, newest: &TombsRecord) -> Vec<u8> {
    let mut out = probe_slots(view);
    out.push(newest.cur);
    out.sort_unstable();
    out.dedup();
    out
}

pub fn all_slots() -> Vec<u8> {
    (0..TOMB_SLOTS).collect()
}

/// The slot's heap, if the table has it. A miss accepts pending invalidations and looks again: a statement that holds
/// every lock it takes already accepts none, so a heap made since (slot 1 by the first switch) can be a stale miss.
pub fn relid(parent: Oid, slot: u8) -> PgResult<Option<Oid>> {
    if let Some(r) = pgrc_front_seams::tomb_relid::call(parent, slot)? {
        return Ok(Some(r));
    }
    inval_seams::accept_invalidation_messages::call()?;
    pgrc_front_seams::tomb_relid::call(parent, slot)
}

const NAMED_WAIT_MS: u32 = 10_000;

/// The heap of a slot a generation's state names. Slot 1 exists once a state names it: the first switch made it and
/// published in one transaction. That transaction is visible a moment before it sends its invalidations
/// (CommitTransaction: ProcArrayEndTransaction, then AtEOXact_Inval), so a miss waits for them, then errors: a named
/// heap is never skipped. Slot 0 is named before the first tombstone makes it.
fn named(parent: Oid, slot: u8) -> PgResult<Option<Oid>> {
    let mut waited = 0;
    loop {
        match relid(parent, slot)? {
            Some(r) => return Ok(Some(r)),
            None if slot == 0 => return Ok(None),
            None if waited >= NAMED_WAIT_MS => {
                return Err(Box::new(
                    PgError::error(format!("pgrcolumnar: relation {parent} has no tombstone relation {slot}")).with_sqlstate(types_error::ERRCODE_DATA_CORRUPTED),
                ))
            }
            None => {
                postgres_seams::check_for_interrupts::call()?;
                waiter::sleep(std::time::Duration::from_millis(1));
                waited += 1;
            }
        }
    }
}

pub fn all(parent: Oid) -> PgResult<Vec<(u8, Oid)>> {
    (0..TOMB_SLOTS).filter_map(|s| relid(parent, s).map(|r| r.map(|r| (s, r))).transpose()).collect()
}

pub struct Opened<'mcx> {
    pub slot: u8,
    pub rel: Relation<'mcx>,
}

impl Opened<'_> {
    pub fn close(self) -> PgResult<()> {
        self.rel.close(AccessShareLock)
    }
}

pub fn open_read<'mcx>(mcx: Mcx<'mcx>, parent: Oid, slots: &[u8]) -> PgResult<Vec<Opened<'mcx>>> {
    open_slots(mcx, parent, slots, named)
}

pub fn open_existing<'mcx>(mcx: Mcx<'mcx>, parent: Oid) -> PgResult<Vec<Opened<'mcx>>> {
    open_slots(mcx, parent, &all_slots(), relid)
}

fn open_slots<'mcx>(mcx: Mcx<'mcx>, parent: Oid, slots: &[u8], find: fn(Oid, u8) -> PgResult<Option<Oid>>) -> PgResult<Vec<Opened<'mcx>>> {
    let mut out = ::mcx::stdvec::try_vec(slots.len(), "pgrc tombstone heaps")?;
    for &slot in slots {
        let r = match find(parent, slot) {
            Ok(Some(r)) => r,
            Ok(None) => continue,
            Err(e) => {
                close_all(out)?;
                return Err(e);
            }
        };
        match table::table_open(mcx, r, AccessShareLock) {
            Ok(rel) => out.push(Opened { slot, rel }),
            Err(e) => {
                close_all(out)?;
                return Err(e);
            }
        }
    }
    Ok(out)
}


/// `cache_syscache::cacheinfo::RELNAMENSP`: `inval` takes the id, and `cache_syscache` depends on
/// this crate (`the_relnamensp_cache_id_is_cacheinfos` checks the number).
const RELNAMENSP_CACHE_ID: i32 = 56;
const TOMBS_STALE_POINT: &str = "pgrc-ixfetch-tombs-stale";
const RELIDS_SLOTS: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Filed {
    db: Oid,
    parent: Oid,
    epoch: u64,
    heaps: Vec<(u8, Oid)>,
}

thread_local! {
    /// Bumped by every pg_class name invalidation this backend executes: another transaction's when it accepts them,
    /// its own at command end and abort (CommandEndInvalidationMessages, AtEOXact_Inval), a cache reset's too.
    static RELIDS_EPOCH: Cell<u64> = const { Cell::new(0) };
    static RELIDS_HOOKED: Cell<Option<bool>> = const { Cell::new(None) };
    static RELIDS: RefCell<Vec<Filed>> = const { RefCell::new(Vec::new()) };
}

fn relnamensp_changed(_arg: datum::Datum, _cacheid: i32, _hashvalue: u32) {
    RELIDS_EPOCH.with(|e| e.set(e.get().wrapping_add(1)));
}

fn epoch() -> u64 {
    RELIDS_EPOCH.with(Cell::get)
}

fn hooked() -> bool {
    RELIDS_HOOKED.with(|h| match h.get() {
        Some(on) => on,
        None => {
            let on = inval::invalidate::CacheRegisterSyscacheCallback(RELNAMENSP_CACHE_ID, relnamensp_changed, datum::Datum::null()).is_ok();
            h.set(Some(on));
            on
        }
    })
}

fn filed(parent: Oid, stale: bool, accept: &mut dyn FnMut() -> PgResult<()>) -> PgResult<Option<Vec<(u8, Oid)>>> {
    let db = init_small::globals::MyDatabaseId();
    let Some(f) = RELIDS.with(|c| c.borrow().iter().find(|f| f.db == db && f.parent == parent).cloned()) else { return Ok(None) };
    if stale {
        return Ok(Some(f.heaps));
    }
    if f.heaps.len() < usize::from(TOMB_SLOTS) {
        accept()?;
    }
    Ok((epoch() == f.epoch).then_some(f.heaps))
}

/// Files `heaps` for `parent` in this database, found at epoch `at`, unless an invalidation came while they were looked up.
fn file(parent: Oid, at: u64, heaps: &[(u8, Oid)]) {
    if epoch() != at {
        return;
    }
    let db = init_small::globals::MyDatabaseId();
    let mut kept = Vec::with_capacity(TOMB_SLOTS as usize);
    kept.extend_from_slice(heaps);
    RELIDS.with(|c| {
        let mut c = c.borrow_mut();
        c.retain(|f| f.db != db || f.parent != parent);
        if c.len() >= RELIDS_SLOTS {
            c.remove(0);
        }
        c.push(Filed { db, parent, epoch: at, heaps: kept }); // alloc: Cold
    });
}

pub fn all_cached(parent: Oid) -> PgResult<Vec<(u8, Oid)>> {
    if !hooked() {
        crate::ixfetch::stats(|s| s.tomb_unhooked += 1);
        return all(parent);
    }
    let stale = injection_point::is_attached(TOMBS_STALE_POINT);
    if let Some(heaps) = filed(parent, stale, &mut || inval_seams::accept_invalidation_messages::call())? {
        crate::ixfetch::stats(|s| s.tomb_hits += 1);
        return Ok(heaps);
    }
    crate::ixfetch::stats(|s| s.tomb_lookups += 1);
    let at = epoch();
    let heaps = all(parent)?;
    file(parent, at, &heaps);
    Ok(heaps)
}

pub fn open_existing_cached<'mcx>(mcx: Mcx<'mcx>, parent: Oid) -> PgResult<Vec<Opened<'mcx>>> {
    let heaps = all_cached(parent)?;
    let mut out = ::mcx::stdvec::try_vec(heaps.len(), "pgrc tombstone heaps")?;
    for (slot, r) in heaps {
        match table::table_open(mcx, r, AccessShareLock) {
            Ok(rel) => out.push(Opened { slot, rel }),
            Err(e) => {
                close_all(out)?;
                return Err(e);
            }
        }
    }
    Ok(out)
}

pub fn close_all(heaps: Vec<Opened<'_>>) -> PgResult<()> {
    let mut first = Ok(());
    for h in heaps {
        let r = h.close();
        if first.is_ok() {
            first = r;
        }
    }
    first
}

pub fn blocks<'mcx>(mcx: Mcx<'mcx>, parent: Oid) -> PgResult<u32> {
    let heaps = open_existing(mcx, parent)?;
    let n = heaps.iter().try_fold(0u32, |n, h| Ok::<_, Box<PgError>>(n + bufmgr::RelationGetNumberOfBlocksInFork(&h.rel, ForkNumber::MAIN_FORKNUM)?));
    close_all(heaps)?;
    n
}

/// D4597: the heaps a read can need now: [`read_slots`] for the statement's view and for the newest generation.
pub fn reader_slots(view: Option<&TombsRecord>, newest: &TombsRecord) -> Vec<u8> {
    let mut out = read_slots(newest, newest);
    out.extend(view.map(|v| read_slots(v, newest)).unwrap_or_default());
    out.sort_unstable();
    out.dedup();
    out
}

/// LOCK F2: whether a tombstone heap of the table deletes rows ([`front_bearing_reads`](crate::front_bearing_reads)): one with
/// blocks, unless its colcache image says it holds LOCK rows only (row locks delete nothing). A heap without an image
/// (the cap 0 or over it, the seam not installed) counts as bearing, as every heap with blocks did before LOCK.
pub fn bearing<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, lock_rows_bear: bool) -> PgResult<bool> {
    let view = crate::ring::statement_view(rel)?.map(|g| state(Some(&*g))).transpose()?;
    let slots = reader_slots(view.as_ref(), &newest_state(rel)?);
    let heaps = open_existing(mcx, rel.rd_id)?;
    let out = (|| {
        for h in heaps.iter().filter(|h| slots.contains(&h.slot)) {
            if bufmgr::RelationGetNumberOfBlocksInFork(&h.rel, ForkNumber::MAIN_FORKNUM)? == 0 {
                continue;
            }
            if lock_rows_bear || !pgrc_front_seams::tomb_lock_only::is_installed() || pgrc_front_seams::tomb_lock_only::call(&h.rel)? != Some(true) {
                return Ok(true);
            }
        }
        Ok(false)
    })();
    close_all(heaps)?;
    out
}

pub struct Writer<'a, 'mcx> {
    mcx: Mcx<'mcx>,
    rel: &'a Relation<'mcx>,
    pub cur: (u8, Relation<'mcx>),
    pub ret: Option<Opened<'mcx>>,
}

impl<'a, 'mcx> Writer<'a, 'mcx> {
    pub fn open(mcx: Mcx<'mcx>, rel: &'a Relation<'mcx>) -> PgResult<Writer<'a, 'mcx>> {
        let st = newest_state(rel)?;
        let cur = open_cur(mcx, rel, st.cur)?;
        let ret = match st.ret.filter(|r| r.folded == 0) {
            Some(r) => match open_read(mcx, rel.rd_id, &[r.slot]) {
                Ok(mut v) => v.pop(),
                Err(e) => {
                    cur.close(NoLock)?;
                    return Err(e);
                }
            },
            None => None,
        };
        Ok(Writer { mcx, rel, cur: (st.cur, cur), ret })
    }

    pub fn heaps(&self) -> Vec<&Relation<'mcx>> {
        std::iter::once(&self.cur.1).chain(self.ret.as_ref().map(|o| &o.rel)).collect()
    }

    pub fn insert_lock(&mut self, rowident: u64, cid: CommandId, mode: tableam_vocab::LockTupleMode) -> PgResult<TombInsert> {
        loop {
            let (slot, rel) = (self.cur.0, self.rel);
            let mut current = || newest_state(rel).map(|s| s.cur == slot);
            match pgrc_front_seams::tomb_insert_lock::call(self.mcx, &self.cur.1, rowident, cid, mode, &mut current)? {
                TombInsert::Retry => self.refresh()?,
                out => return Ok(out),
            }
        }
    }

    /// Probed heap `i` ([`Writer::heaps`] order) held under RowExclusiveLock to transaction end, the lock every
    /// row locker and writer of a LOCK row holds on its heap: a fold of a retired heap needs a ShareLock on it, so it
    /// never drops a held row lock. The current heap is held since [`Writer::open`]; the retired one is locked, then
    /// rechecked (lock-then-recheck, as segments): `false` if a fold committed meanwhile (its LOCK rows are void, the
    /// caller probes again on a fresh writer).
    pub fn hold(&mut self, i: usize) -> PgResult<bool> {
        let Some(ret) = self.ret.as_ref().filter(|_| i == 1) else { return Ok(true) };
        lmgr::LockRelationOid(ret.rel.rd_id, RowExclusiveLock)?;
        Ok(probe_slots(&newest_state(self.rel)?).contains(&ret.slot))
    }

    pub fn heap(&self, i: usize) -> &Relation<'mcx> {
        match (i, &self.ret) {
            (1, Some(r)) => &r.rel,
            _ => &self.cur.1,
        }
    }

    pub fn reopen(&mut self) -> PgResult<()> {
        self.refresh()
    }

    /// Insert into the current heap; a switch that committed since this writer opened it moves the insert to the new
    /// current heap (the retired one joins the probe set). Never [`TombInsert::Retry`].
    pub fn insert(&mut self, t: Tombstone, cid: CommandId) -> PgResult<TombInsert> {
        injection_point::injection_point("pgrc-tomb-before-insert")?;
        loop {
            let (slot, rel) = (self.cur.0, self.rel);
            let mut current = || newest_state(rel).map(|s| s.cur == slot);
            match pgrc_front_seams::tomb_insert::call(self.mcx, &self.cur.1, t, cid, &mut current)? {
                TombInsert::Retry => self.refresh()?,
                out => return Ok(out),
            }
        }
    }

    fn refresh(&mut self) -> PgResult<()> {
        let next = Writer::open(self.mcx, self.rel)?;
        let old = std::mem::replace(self, next);
        // the old current heap keeps its RowExclusiveLock: this transaction may have tombstones there
        old.cur.1.close(NoLock)?;
        old.ret.map_or(Ok(()), Opened::close)
    }

    pub fn close(self) -> PgResult<()> {
        let out = self.cur.1.close(NoLock);
        let r = self.ret.map_or(Ok(()), Opened::close);
        out.and(r)
    }
}

fn open_cur<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, slot: u8) -> PgResult<Relation<'mcx>> {
    let r = match named(rel.rd_id, slot)? {
        Some(r) => r,
        None if slot == 0 => {
            lmgr::LockRelationOid(rel.rd_id, ShareUpdateExclusiveLock)?;
            pgrc_front_seams::create_tomb::call(mcx, rel.rd_id, 0)?
        }
        None => {
            return Err(Box::new(
                PgError::error(format!("pgrcolumnar: relation {} has no tombstone relation {slot}", rel.rd_id)).with_sqlstate(types_error::ERRCODE_DATA_CORRUPTED),
            ))
        }
    };
    table::table_open(mcx, r, RowExclusiveLock)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(cur: u8, ret: Option<(u8, u64)>) -> TombsRecord {
        TombsRecord { cur, ret: ret.map(|(slot, folded)| RetiredTombs { slot, folded }) }
    }

    #[test]
    fn writers_skip_a_folded_heap_and_readers_add_the_newest_current_one() {
        assert_eq!(probe_slots(&TombsRecord::default()), vec![0]);
        assert_eq!(probe_slots(&st(1, Some((0, 0)))), vec![1, 0], "unfolded: probed");
        assert_eq!(probe_slots(&st(1, Some((0, 77)))), vec![1], "folded: skipped");
        assert_eq!(read_slots(&TombsRecord::default(), &st(1, Some((0, 0)))), vec![0, 1]);
        assert_eq!(read_slots(&st(1, Some((0, 77))), &st(1, Some((0, 77)))), vec![1]);
        assert_eq!(read_slots(&st(1, Some((0, 0))), &st(1, Some((0, 77)))), vec![0, 1]);
        assert_eq!(read_slots(&st(0, Some((1, 0))), &st(0, Some((1, 90)))), vec![0, 1]);
        assert_eq!(read_slots(&st(0, Some((1, 90))), &st(0, Some((1, 90)))), vec![0]);
        assert_eq!(read_slots(&st(1, Some((0, 77))), &st(0, Some((1, 0)))), vec![0, 1]);
    }

    #[test]
    fn front_bearing_skips_a_retired_heap_only_once_both_the_view_and_the_newest_have_it_folded() {
        assert_eq!(reader_slots(Some(&st(1, Some((0, 77)))), &st(1, Some((0, 77)))), vec![1]);
        assert_eq!(reader_slots(None, &st(1, Some((0, 77)))), vec![1]);
        assert_eq!(reader_slots(Some(&st(1, Some((0, 0)))), &st(1, Some((0, 77)))), vec![0, 1]);
        assert_eq!(reader_slots(Some(&TombsRecord::default()), &st(1, Some((0, 77)))), vec![0, 1]);
        assert_eq!(reader_slots(None, &st(1, Some((0, 0)))), vec![0, 1]);
        assert_eq!(reader_slots(None, &TombsRecord::default()), vec![0]);
    }

    fn reset() {
        RELIDS.with(|c| c.borrow_mut().clear());
    }

    fn quiet() -> PgResult<()> {
        Ok(())
    }

    fn unasked() -> PgResult<()> {
        panic!("the teeth accept nothing")
    }

    #[test]
    fn a_filed_answer_holds_until_a_pg_class_name_invalidation() {
        reset();
        let mut accepts = 0;
        let mut none = || -> PgResult<()> {
            accepts += 1;
            Ok(())
        };
        assert_eq!(filed(7, false, &mut none).unwrap(), None, "nothing filed");
        file(7, epoch(), &[]);
        file(8, epoch(), &[(0, 900), (1, 901)]);
        assert_eq!(filed(7, false, &mut none).unwrap(), Some(vec![]));
        assert_eq!(filed(8, false, &mut none).unwrap(), Some(vec![(0, 900), (1, 901)]));
        assert_eq!(accepts, 1, "only the answer lacking a slot accepts");
        let db = init_small::globals::MyDatabaseId();
        init_small::globals::SetMyDatabaseId(0x7e57_d000);
        assert_eq!(filed(8, false, &mut unasked).unwrap(), None, "another database");
        init_small::globals::SetMyDatabaseId(db);
        let mut made = || -> PgResult<()> {
            relnamensp_changed(datum::Datum::null(), RELNAMENSP_CACHE_ID, 1);
            Ok(())
        };
        assert_eq!(filed(7, false, &mut made).unwrap(), None);
        assert_eq!(filed(8, false, &mut quiet).unwrap(), None, "every answer goes stale at once");
        assert_eq!(filed(8, true, &mut unasked).unwrap(), Some(vec![(0, 900), (1, 901)]));
    }

    #[test]
    fn answers_found_across_an_invalidation_are_not_filed() {
        reset();
        let at = epoch();
        relnamensp_changed(datum::Datum::null(), RELNAMENSP_CACHE_ID, 1);
        file(7, at, &[(0, 900)]);
        assert_eq!(filed(7, false, &mut quiet).unwrap(), None);
        file(7, epoch(), &[(0, 900)]);
        file(7, epoch(), &[(0, 900), (1, 901)]);
        assert_eq!(RELIDS.with(|c| c.borrow().len()), 1);
        for p in 100..100 + RELIDS_SLOTS as Oid {
            file(p, epoch(), &[]);
        }
        assert_eq!(filed(7, false, &mut quiet).unwrap(), None, "the oldest left");
        assert_eq!(RELIDS.with(|c| c.borrow().len()), RELIDS_SLOTS);
    }

    #[test]
    fn the_relnamensp_cache_id_is_cacheinfos() {
        let src = std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../utils/cache/cache_syscache/src/cacheinfo.rs")).unwrap();
        assert!(src.contains(&format!("pub const RELNAMENSP: i32 = {RELNAMENSP_CACHE_ID};")));
    }
}
