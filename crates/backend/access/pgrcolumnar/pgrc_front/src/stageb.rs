//! Stage B: the tombstone that deletes a row, or moves it to its UPDATE's new version, is the row's write lock. A dirty
//! probe of the UNIQUE index on `rowident` decides; an uncontested row gets its tombstone by a speculative insert,
//! whose loser is dead to everyone at once and probes again. Lock order: parent → segment → tombstone.

use std::rc::Rc;

use mcx::Mcx;
use pgrc_format::dml::Tombstone;
use pgrc_format::rowid::{SUCC_MOVED, SUCC_NONE};
use pgrc_front_seams::{TombInsert, TombState};
use tableam_vocab::{LockTupleMode, LockWaitPolicy, Snapshot, TM_FailureData, TM_Result};
use types_core::{CommandId, TransactionId};
use types_error::{PgError, PgResult};
use types_rel::Relation;
use types_slot::SlotData;
use types_snapshot::{SnapshotData, SnapshotType};
use types_storage::lock::XLTW_Oper;
use types_tuple::{ItemPointerData, ItemPointerSetMovedPartitions};

use crate::position::{self, ViewRef};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    Clear,
    Own { cmin: CommandId },
    Updated { succ: u64, xmax: TransactionId },
    Deleted { xmax: TransactionId },
    Moved { xmax: TransactionId },
    Busy { xmax: TransactionId },
    Locked { heap: usize, tid: ItemPointerData, mine: Option<TransactionId> },
    /// KEY SHARE probes only: an in-progress no-key update to `succ`, which KEY SHARE does not wait for. The lock goes
    /// on the successor (heap's `heap_lock_updated_tuple` walks the chain the same way); the old version's tombstone is
    /// the updater's uncommitted row, so it is not locked itself (residual: if the update aborts, the row is not KEY
    /// SHARE locked).
    Passed { succ: u64 },
}

/// Under `SnapshotDirty`: an in-progress tombstone is waited out (its speculative token while it decides, else its
/// transaction) and probed again. A speculative token is waited for even when `wait` is false: it is held only for
/// the inserter's unique check. `key_share`: an in-progress update is [`Probe::Passed`], not waited for.
fn probe_heap<'mcx>(mcx: Mcx<'mcx>, tomb: &Relation<'mcx>, r: u64, wait: bool, key_share: bool) -> PgResult<Probe> {
    loop {
        let dirty: Snapshot<'mcx> = Some(Rc::new(SnapshotData::sentinel(mcx, SnapshotType::SNAPSHOT_DIRTY)));
        let Some(hit) = pgrc_front_seams::tomb_probe::call(mcx, tomb, r, &dirty)? else { return Ok(Probe::Clear) };
        if hit.spec_token != 0 && hit.state != TombState::Own {
            lmgr::SpeculativeInsertionWait(hit.xmin, hit.spec_token)?;
            continue;
        }
        // a LOCK row is frozen once won, so only a crash-era leftover could read in progress: waited out
        if hit.is_lock() && hit.state != TombState::InProgress {
            return Ok(Probe::Locked { heap: 0, tid: hit.tid, mine: hit.own_unfrozen.then_some(hit.xmin) });
        }
        match hit.state {
            TombState::Own => return Ok(Probe::Own { cmin: hit.cmin }),
            TombState::Committed => {
                return Ok(match hit.succ {
                    SUCC_NONE => Probe::Deleted { xmax: hit.xmin },
                    SUCC_MOVED => Probe::Moved { xmax: hit.xmin },
                    succ => Probe::Updated { succ, xmax: hit.xmin },
                })
            }
            TombState::InProgress if key_share && !hit.is_lock() && key_share_compatible(hit.succ) => return Ok(Probe::Passed { succ: hit.succ }),
            TombState::InProgress if !wait => return Ok(Probe::Busy { xmax: hit.xmin }),
            TombState::InProgress => {
                let oper = if key_share { XLTW_Oper::Lock } else { XLTW_Oper::Update };
                lmgr::XactLockTableWait(hit.xmin, Some(tomb), Some(&hit.tid), oper)?
            }
        }
    }
}

pub fn probe<'mcx>(mcx: Mcx<'mcx>, tomb: &Relation<'mcx>, r: u64, wait: bool) -> PgResult<Probe> {
    probe_heap(mcx, tomb, r, wait, false)
}

pub fn probe_all_key_share<'mcx>(mcx: Mcx<'mcx>, heaps: &[&Relation<'mcx>], r: u64, wait: bool) -> PgResult<Probe> {
    probe_set(mcx, heaps, r, wait, true)
}

fn key_share_compatible(succ: u64) -> bool {
    succ != SUCC_NONE && succ != SUCC_MOVED && succ != pgrc_format::rowid::SUCC_LOCK
}

/// [`probe`] over a writer's tombstone heaps: a RowIdent has at most one live tombstone
/// among them, so the first heap that answers decides. A tombstone in any heap outranks a LOCK row (a writer
/// that met a LOCK row in the retired heap put its tombstone in the current one).
pub fn probe_all<'mcx>(mcx: Mcx<'mcx>, heaps: &[&Relation<'mcx>], r: u64, wait: bool) -> PgResult<Probe> {
    probe_set(mcx, heaps, r, wait, false)
}

fn probe_set<'mcx>(mcx: Mcx<'mcx>, heaps: &[&Relation<'mcx>], r: u64, wait: bool, key_share: bool) -> PgResult<Probe> {
    let mut lock = None;
    for (i, tomb) in heaps.iter().enumerate() {
        match probe_heap(mcx, tomb, r, wait, key_share)? {
            Probe::Clear => {}
            Probe::Locked { tid, mine, .. } => {
                lock.get_or_insert(Probe::Locked { heap: i, tid, mine });
            }
            p => return Ok(p),
        }
    }
    Ok(lock.unwrap_or(Probe::Clear))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Locking {
    Held,
    Answer(Probe),
    WouldBlock { xmax: TransactionId },
}

pub fn lock_row<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, r: u64, cid: CommandId, mode: LockTupleMode, policy: LockWaitPolicy) -> PgResult<Locking> {
    let mut tombs = crate::tombs::Writer::open(mcx, rel)?;
    let out = lock_loop(mcx, rel, &mut tombs, r, cid, mode, policy);
    tombs.close()?;
    out
}

fn lock_loop<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, tombs: &mut crate::tombs::Writer<'_, 'mcx>, r: u64, cid: CommandId, mode: LockTupleMode, policy: LockWaitPolicy) -> PgResult<Locking> {
    let wait = policy == LockWaitPolicy::LockWaitBlock;
    loop {
        let probed = probe_set(mcx, &tombs.heaps(), r, wait, mode == LockTupleMode::LockTupleKeyShare)?;
        match probed {
            Probe::Clear => match tombs.insert_lock(r, cid, mode)? {
                TombInsert::Won(tid) => {
                    crate::maint::tombstoned(init_small::globals::MyDatabaseId(), rel.rd_id, types_tuple::ItemPointerGetBlockNumberNoCheck(&tid));
                    return Ok(Locking::Held);
                }
                TombInsert::Lost(_) | TombInsert::Retry => {}
            },
            // the successor's chain is locked in turn (waiting out an in-progress delete of it); once it answers
            // otherwise (that delete committed), the row is probed again: its update has committed meanwhile
            Probe::Passed { succ } => match lock_loop(mcx, rel, tombs, succ, cid, mode, policy)? {
                Locking::Held => return Ok(Locking::Held),
                w @ Locking::WouldBlock { .. } => return Ok(w),
                Locking::Answer(_) => {}
            },
            // a committed no-key update does not conflict with KEY SHARE either: heap locks the version it was asked
            // for and the chain after it (heap_lock_updated_tuple), and answers TM_Ok, the old version (no EPQ, no
            // 40001); the successor is locked here the same way. Its chain ending in a delete answers as before.
            p @ Probe::Updated { succ, .. } if mode == LockTupleMode::LockTupleKeyShare && key_share_compatible(succ) => {
                match lock_loop(mcx, rel, tombs, succ, cid, mode, policy)? {
                    Locking::Answer(_) => return Ok(Locking::Answer(p)),
                    l => return Ok(l),
                }
            }
            Probe::Busy { xmax } => match policy {
                LockWaitPolicy::LockWaitBlock => unreachable!("a waiting probe is never Busy"),
                LockWaitPolicy::LockWaitSkip if !lmgr::ConditionalXactLockTableWait(xmax, false)? => return Ok(Locking::WouldBlock { xmax }),
                LockWaitPolicy::LockWaitError if !lmgr::ConditionalXactLockTableWait(xmax, heapam::dml::log_lock_failures())? => return Err(could_not_lock(rel)),
                _ => {}
            },
            // this transaction's unfrozen LOCK row is an exclusive lock it already holds, the strongest
            Probe::Locked { mine: Some(_), .. } => return Ok(Locking::Held),
            Probe::Locked { heap, tid, mine: None } => {
                if !tombs.hold(heap)? {
                    tombs.reopen()?;
                    continue;
                }
                let mut tmfd = TM_FailureData::default();
                let (res, pin) = match heapam::heap_lock_tuple(tombs.heap(heap), &tid, cid, mode, policy, false, &mut tmfd) {
                    Ok(x) => x,
                    Err(e) if e.sqlstate() == types_error::ERRCODE_LOCK_NOT_AVAILABLE => return Err(could_not_lock(rel)),
                    Err(e) => return Err(e),
                };
                pin.release();
                match res {
                    TM_Result::TM_Ok => return Ok(Locking::Held),
                    TM_Result::TM_WouldBlock => return Ok(Locking::WouldBlock { xmax: tmfd.xmax }),
                    TM_Result::TM_Invisible => {
                        return Err(Box::new(PgError::error(format!("pgrcolumnar: LOCK row {r} of \"{}\" is invisible to its locker", rel.name())).with_sqlstate(types_error::ERRCODE_DATA_CORRUPTED)))
                    }
                    _ => {}
                }
            }
            p => return Ok(Locking::Answer(p)),
        }
    }
}

/// heap's NOWAIT failure, naming the parent rather than its tombstone heap.
fn could_not_lock(rel: &Relation<'_>) -> Box<PgError> {
    Box::new(PgError::error(format!("could not obtain lock on row in relation \"{}\"", rel.name())).with_sqlstate(types_error::ERRCODE_LOCK_NOT_AVAILABLE))
}

fn tombstoned(rel: &Relation<'_>, tomb: types_core::Oid, tid: &ItemPointerData) -> PgResult<()> {
    crate::maint::tombstoned(init_small::globals::MyDatabaseId(), rel.rd_id, types_tuple::ItemPointerGetBlockNumberNoCheck(tid));
    // a sorted_by table's cached plans; the heap's first position means it was empty
    let first = types_tuple::ItemPointerGetBlockNumberNoCheck(tid) == 0 && types_tuple::ItemPointerGetOffsetNumberNoCheck(tid) == 1;
    pgrc_am::front_claim::front_write(rel, tomb, first)
}

/// `tmfd.ctid` comes in as the row's own position.
#[allow(clippy::too_many_arguments)]
pub fn write<'mcx>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    r: u64,
    new: Option<&mut SlotData<'mcx>>,
    cid: CommandId,
    wait: bool,
    moved: bool,
    tmfd: &mut TM_FailureData,
) -> PgResult<TM_Result> {
    // the active segment before the tombstone heap (lock order)
    let active = match new.is_some() {
        true => Some(crate::route::segment(mcx, rel)?),
        false => None,
    };
    let mut tombs = crate::tombs::Writer::open(mcx, rel)?;
    let mut rows = Rel { mcx, rel, tombs: &mut tombs, new, active };
    let out = run(&mut rows, r, cid, wait, moved, tmfd);
    tombs.close()?;
    out
}

trait Rows {
    fn probe(&mut self, r: u64, wait: bool) -> PgResult<Probe>;
    fn updating(&self) -> bool;
    fn insert_new(&mut self, cid: CommandId) -> PgResult<u64>;
    fn tomb_insert(&mut self, t: Tombstone, cid: CommandId) -> PgResult<TombInsert>;
    fn lock_row(&mut self, heap: usize, tid: ItemPointerData, cid: CommandId, mode: LockTupleMode, wait: bool) -> PgResult<LockRow>;
    /// Write tombstone `t` over the LOCK row `tid` of probed heap `heap`, which this transaction holds locked:
    /// the current heap's row is heap-updated to it; a retired heap's stays (nothing new enters a retired heap) and
    /// `t` goes to the current heap, where it outranks it.
    fn write_locked(&mut self, heap: usize, tid: ItemPointerData, t: Tombstone, cid: CommandId) -> PgResult<TombInsert>;
    /// Overwrite the current heap's LOCK row `tid`, inserted (unfrozen) by the current subtransaction, with
    /// tombstone `t` in place.
    fn overwrite(&mut self, tid: ItemPointerData, t: Tombstone, cid: CommandId) -> PgResult<TombInsert>;
    fn current_xid(&self) -> TransactionId {
        xact::GetCurrentTransactionIdIfAny()
    }
    fn conflict_in(&mut self) -> PgResult<()> {
        Ok(())
    }
    fn position_of(&mut self, succ: u64) -> PgResult<Option<ItemPointerData>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockRow {
    Held,
    Again,
    Busy(TransactionId),
}

struct Rel<'a, 'b, 'mcx> {
    mcx: Mcx<'mcx>,
    rel: &'a Relation<'mcx>,
    tombs: &'b mut crate::tombs::Writer<'a, 'mcx>,
    new: Option<&'a mut SlotData<'mcx>>,
    active: Option<crate::ring::ActiveSeg>,
}

impl Rows for Rel<'_, '_, '_> {
    fn probe(&mut self, r: u64, wait: bool) -> PgResult<Probe> {
        probe_all(self.mcx, &self.tombs.heaps(), r, wait)
    }

    fn updating(&self) -> bool {
        self.new.is_some()
    }

    fn insert_new(&mut self, cid: CommandId) -> PgResult<u64> {
        let slot = self.new.as_deref_mut().expect("an UPDATE carries its new version");
        // the version rides ROW_UPDATE once the tombstone won, so a lost race's orphan decodes as nothing.
        crate::route::insert_row_in(self.mcx, self.rel, slot, cid, heapam::hio::HEAP_INSERT_NO_LOGICAL, self.active)
    }

    fn tomb_insert(&mut self, t: Tombstone, cid: CommandId) -> PgResult<TombInsert> {
        let out = self.tombs.insert(t, cid)?;
        if let TombInsert::Won(tid) = &out {
            tombstoned(self.rel, self.tombs.cur.1.rd_id, tid)?;
        }
        Ok(out)
    }

    fn lock_row(&mut self, heap: usize, tid: ItemPointerData, cid: CommandId, mode: LockTupleMode, wait: bool) -> PgResult<LockRow> {
        if !self.tombs.hold(heap)? {
            self.tombs.reopen()?;
            return Ok(LockRow::Again);
        }
        let policy = if wait { LockWaitPolicy::LockWaitBlock } else { LockWaitPolicy::LockWaitSkip };
        let mut tmfd = TM_FailureData::default();
        let (res, pin) = heapam::heap_lock_tuple(self.tombs.heap(heap), &tid, cid, mode, policy, false, &mut tmfd)?;
        pin.release();
        Ok(match res {
            TM_Result::TM_Ok => LockRow::Held,
            TM_Result::TM_WouldBlock => LockRow::Busy(tmfd.xmax),
            TM_Result::TM_Invisible => {
                return Err(Box::new(PgError::error(format!("pgrcolumnar: a LOCK row of \"{}\" is invisible to its writer", self.rel.name())).with_sqlstate(types_error::ERRCODE_DATA_CORRUPTED)))
            }
            _ => LockRow::Again,
        })
    }

    fn write_locked(&mut self, heap: usize, tid: ItemPointerData, t: Tombstone, cid: CommandId) -> PgResult<TombInsert> {
        if heap != 0 {
            return self.tomb_insert(t, cid);
        }
        let tomb = &self.tombs.cur.1;
        let new = pgrc_front_seams::tomb_update::call(self.mcx, tomb, tid, t, cid)?;
        tombstoned(self.rel, tomb.rd_id, &new)?;
        Ok(TombInsert::Won(new))
    }

    fn overwrite(&mut self, tid: ItemPointerData, t: Tombstone, cid: CommandId) -> PgResult<TombInsert> {
        let tomb = &self.tombs.cur.1;
        pgrc_front_seams::tomb_overwrite::call(self.mcx, tomb, tid, t, cid)?;
        tombstoned(self.rel, tomb.rd_id, &tid)?;
        Ok(TombInsert::Won(tid))
    }

    fn conflict_in(&mut self) -> PgResult<()> {
        crate::filter::conflict_in(self.rel)
    }

    fn position_of(&mut self, succ: u64) -> PgResult<Option<ItemPointerData>> {
        let mut view = ViewRef::default();
        match position::find(self.mcx, self.rel, succ, &mut view)? {
            Some(at) => position::stamp(self.rel, &at).map(Some),
            None => Ok(None),
        }
    }
}

fn run(rows: &mut dyn Rows, r: u64, cid: CommandId, wait: bool, moved: bool, tmfd: &mut TM_FailureData) -> PgResult<TM_Result> {
    loop {
        let locked = match rows.probe(r, wait)? {
            Probe::Clear => None,
            Probe::Own { cmin } if cmin >= cid => {
                tmfd.xmax = xact::GetCurrentTransactionIdIfAny();
                tmfd.cmax = cmin;
                return Ok(TM_Result::TM_SelfModified);
            }
            Probe::Own { .. } => return Ok(TM_Result::TM_Invisible),
            Probe::Busy { xmax } => return Ok(failed(tmfd, xmax, TM_Result::TM_BeingModified)),
            Probe::Deleted { xmax } => return Ok(failed(tmfd, xmax, TM_Result::TM_Deleted)),
            Probe::Moved { xmax } => {
                ItemPointerSetMovedPartitions(&mut tmfd.ctid);
                return Ok(failed(tmfd, xmax, TM_Result::TM_Updated));
            }
            Probe::Updated { succ, xmax } => {
                if let Some(at) = rows.position_of(succ)? {
                    tmfd.ctid = at;
                }
                return Ok(failed(tmfd, xmax, TM_Result::TM_Updated));
            }
            Probe::Passed { .. } => unreachable!("only KEY SHARE probes pass"),
            // heap's tuple lock of the write's strength first (an UPDATE is a no-key update: no unique index)
            Probe::Locked { heap, tid, mine: Some(x) } => Some((heap, tid, Some(x))),
            Probe::Locked { heap, tid, mine: None } => {
                let mode = if rows.updating() { LockTupleMode::LockTupleNoKeyExclusive } else { LockTupleMode::LockTupleExclusive };
                match rows.lock_row(heap, tid, cid, mode, wait)? {
                    LockRow::Held => Some((heap, tid, None)),
                    LockRow::Again => continue,
                    LockRow::Busy(xmax) => return Ok(failed(tmfd, xmax, TM_Result::TM_BeingModified)),
                }
            }
        };
        let succ = match (rows.updating(), moved) {
            (true, _) => rows.insert_new(cid)?,
            (false, true) => SUCC_MOVED,
            (false, false) => SUCC_NONE,
        };
        rows.conflict_in()?;
        let t = Tombstone { rowident: r, succ };
        let out = match locked {
            None => rows.tomb_insert(t, cid)?,
            // the current subtransaction's own LOCK row in the current heap becomes the tombstone in place
            Some((0, tid, Some(x))) if x == rows.current_xid() => rows.overwrite(tid, t, cid)?,
            Some((heap, tid, _)) => rows.write_locked(heap, tid, t, cid)?,
        };
        match out {
            TombInsert::Won(_) => return Ok(TM_Result::TM_Ok),
            // The new version is ours alone: its tombstone cannot conflict. The next probe waits for the winner.
            TombInsert::Lost(_) | TombInsert::Retry if rows.updating() => {
                rows.tomb_insert(Tombstone { rowident: succ, succ: SUCC_NONE }, cid)?;
            }
            TombInsert::Lost(_) | TombInsert::Retry => {}
        }
    }
}

fn failed(tmfd: &mut TM_FailureData, xmax: TransactionId, res: TM_Result) -> TM_Result {
    tmfd.xmax = xmax;
    tmfd.cmax = types_core::InvalidCommandId;
    res
}

#[cfg(test)]
mod tests {
    #[test]
    fn key_share_waits_for_deletes_and_moves_only() {
        assert!(!super::key_share_compatible(SUCC_NONE));
        assert!(!super::key_share_compatible(SUCC_MOVED));
        assert!(super::key_share_compatible(42));
        assert!(!super::key_share_compatible(pgrc_format::rowid::SUCC_LOCK), "a LOCK row is no update");
    }

    use super::*;
    use std::collections::VecDeque;

    #[derive(Default)]
    struct Double {
        update: bool,
        script: VecDeque<Probe>,
        live: Vec<Tombstone>,
        rival: Option<TransactionId>,
        next_new: u64,
        locks: VecDeque<LockRow>,
        asked: Vec<(usize, LockTupleMode)>,
        over: Vec<(usize, Tombstone)>,
        inplace: Vec<Tombstone>,
        xid: TransactionId,
    }

    impl Rows for Double {
        fn probe(&mut self, _r: u64, _wait: bool) -> PgResult<Probe> {
            Ok(self.script.pop_front().unwrap_or(Probe::Clear))
        }
        fn updating(&self) -> bool {
            self.update
        }
        fn insert_new(&mut self, _cid: CommandId) -> PgResult<u64> {
            self.next_new += 1;
            Ok(self.next_new)
        }
        fn tomb_insert(&mut self, t: Tombstone, _cid: CommandId) -> PgResult<TombInsert> {
            if self.live.iter().any(|l| l.rowident == t.rowident) {
                return Ok(TombInsert::Lost(1));
            }
            if let Some(x) = self.rival.take().filter(|_| t.rowident == 7) {
                self.live.push(Tombstone { rowident: t.rowident, succ: 900 });
                return Ok(TombInsert::Lost(x));
            }
            self.live.push(t);
            Ok(TombInsert::Won(ItemPointerData::default()))
        }
        fn lock_row(&mut self, heap: usize, _tid: ItemPointerData, _cid: CommandId, mode: LockTupleMode, _wait: bool) -> PgResult<LockRow> {
            self.asked.push((heap, mode));
            Ok(self.locks.pop_front().unwrap_or(LockRow::Held))
        }
        fn write_locked(&mut self, heap: usize, _tid: ItemPointerData, t: Tombstone, cid: CommandId) -> PgResult<TombInsert> {
            if heap != 0 {
                return self.tomb_insert(t, cid);
            }
            self.over.push((heap, t));
            Ok(TombInsert::Won(ItemPointerData::default()))
        }
        fn overwrite(&mut self, _tid: ItemPointerData, t: Tombstone, _cid: CommandId) -> PgResult<TombInsert> {
            self.inplace.push(t);
            Ok(TombInsert::Won(ItemPointerData::default()))
        }
        fn current_xid(&self) -> TransactionId {
            self.xid
        }
        fn position_of(&mut self, succ: u64) -> PgResult<Option<ItemPointerData>> {
            let mut t = ItemPointerData::default();
            types_tuple::ItemPointerSet(&mut t, succ as u32, 1);
            Ok(Some(t))
        }
    }

    fn go(d: &mut Double, cid: CommandId, moved: bool) -> (TM_Result, TM_FailureData) {
        let mut tmfd = TM_FailureData::default();
        types_tuple::ItemPointerSet(&mut tmfd.ctid, 3, 3);
        (run(d, 7, cid, true, moved, &mut tmfd).expect("stage b"), tmfd)
    }

    #[test]
    fn each_probe_maps_to_heaps_result() {
        let at = |b: u32| {
            let mut t = ItemPointerData::default();
            types_tuple::ItemPointerSet(&mut t, b, if b == 3 { 3 } else { 1 });
            t
        };
        for (p, want, xmax, ctid) in [
            (Probe::Own { cmin: 5 }, TM_Result::TM_SelfModified, None, Some(at(3))),
            (Probe::Own { cmin: 9 }, TM_Result::TM_SelfModified, None, Some(at(3))),
            (Probe::Own { cmin: 4 }, TM_Result::TM_Invisible, None, Some(at(3))),
            (Probe::Updated { succ: 44, xmax: 12 }, TM_Result::TM_Updated, Some(12), Some(at(44))),
            (Probe::Deleted { xmax: 13 }, TM_Result::TM_Deleted, Some(13), Some(at(3))),
            (Probe::Moved { xmax: 14 }, TM_Result::TM_Updated, Some(14), None),
            (Probe::Busy { xmax: 15 }, TM_Result::TM_BeingModified, Some(15), Some(at(3))),
        ] {
            let mut d = Double { script: [p].into(), ..Default::default() };
            let (res, tmfd) = go(&mut d, 5, false);
            assert_eq!(res, want, "{p:?}");
            assert!(d.live.is_empty(), "{p:?} writes nothing");
            if let Some(x) = xmax {
                assert_eq!(tmfd.xmax, x);
            }
            match ctid {
                Some(c) => assert_eq!(tmfd.ctid, c, "{p:?}"),
                None => assert!(types_tuple::ItemPointerIndicatesMovedPartitions(&tmfd.ctid)),
            }
            if let Probe::Own { cmin } = p {
                if res == TM_Result::TM_SelfModified {
                    assert_eq!(tmfd.cmax, cmin);
                }
            }
        }
    }

    #[test]
    fn clear_rows_get_their_tombstone() {
        for (update, moved, succ) in [(false, false, SUCC_NONE), (false, true, SUCC_MOVED), (true, false, 1)] {
            let mut d = Double { update, ..Default::default() };
            assert_eq!(go(&mut d, 5, moved).0, TM_Result::TM_Ok);
            assert_eq!(d.live, vec![Tombstone { rowident: 7, succ }]);
        }
    }

    #[test]
    fn a_lost_update_leaves_one_live_tombstone_and_an_orphan() {
        let mut d = Double { update: true, rival: Some(77), ..Default::default() };
        d.script = [Probe::Clear, Probe::Updated { succ: 900, xmax: 77 }].into();
        let (res, tmfd) = go(&mut d, 5, false);
        assert_eq!((res, tmfd.xmax), (TM_Result::TM_Updated, 77));
        assert_eq!(d.live, vec![Tombstone { rowident: 7, succ: 900 }, Tombstone { rowident: 1, succ: SUCC_NONE }]);
        let mut d = Double { update: true, rival: Some(77), ..Default::default() };
        let mut tmfd = TM_FailureData::default();
        let res = run(&mut Aborting(&mut d), 7, 5, true, false, &mut tmfd).unwrap();
        assert_eq!(res, TM_Result::TM_Ok);
        assert_eq!(d.live, vec![Tombstone { rowident: 1, succ: SUCC_NONE }, Tombstone { rowident: 7, succ: 2 }]);
    }

    struct Aborting<'a>(&'a mut Double);

    impl Rows for Aborting<'_> {
        fn probe(&mut self, r: u64, wait: bool) -> PgResult<Probe> {
            self.0.live.retain(|t| t.succ != 900);
            self.0.probe(r, wait)
        }
        fn updating(&self) -> bool {
            self.0.updating()
        }
        fn insert_new(&mut self, cid: CommandId) -> PgResult<u64> {
            self.0.insert_new(cid)
        }
        fn tomb_insert(&mut self, t: Tombstone, cid: CommandId) -> PgResult<TombInsert> {
            self.0.tomb_insert(t, cid)
        }
        fn lock_row(&mut self, heap: usize, tid: ItemPointerData, cid: CommandId, mode: LockTupleMode, wait: bool) -> PgResult<LockRow> {
            self.0.lock_row(heap, tid, cid, mode, wait)
        }
        fn write_locked(&mut self, heap: usize, tid: ItemPointerData, t: Tombstone, cid: CommandId) -> PgResult<TombInsert> {
            self.0.write_locked(heap, tid, t, cid)
        }
        fn overwrite(&mut self, tid: ItemPointerData, t: Tombstone, cid: CommandId) -> PgResult<TombInsert> {
            self.0.overwrite(tid, t, cid)
        }
        fn position_of(&mut self, succ: u64) -> PgResult<Option<ItemPointerData>> {
            self.0.position_of(succ)
        }
    }

    #[test]
    fn a_locked_row_is_locked_then_written_over() {
        let tid = ItemPointerData::default();
        for (update, heap, mode) in [
            (false, 0, LockTupleMode::LockTupleExclusive),
            (true, 0, LockTupleMode::LockTupleNoKeyExclusive),
            (false, 1, LockTupleMode::LockTupleExclusive),
            (true, 1, LockTupleMode::LockTupleNoKeyExclusive),
        ] {
            let mut d = Double { update, script: [Probe::Locked { heap, tid, mine: None }].into(), ..Default::default() };
            assert_eq!(go(&mut d, 5, false).0, TM_Result::TM_Ok);
            assert_eq!(d.asked, vec![(heap, mode)]);
            let t = Tombstone { rowident: 7, succ: if update { 1 } else { SUCC_NONE } };
            match heap {
                0 => assert_eq!((d.over, d.live), (vec![(0, t)], vec![]), "the current heap's LOCK row is updated"),
                _ => assert_eq!((d.over, d.live), (vec![], vec![t]), "a retired heap's LOCK row stays"),
            }
        }
    }

    #[test]
    fn a_lost_lock_probes_again_and_a_busy_one_fails() {
        let tid = ItemPointerData::default();
        let mut d = Double { script: [Probe::Locked { heap: 0, tid, mine: None }, Probe::Deleted { xmax: 31 }].into(), locks: [LockRow::Again].into(), ..Default::default() };
        let (res, tmfd) = go(&mut d, 5, false);
        assert_eq!((res, tmfd.xmax), (TM_Result::TM_Deleted, 31));
        assert!(d.over.is_empty() && d.live.is_empty());
        let mut d = Double { update: true, script: [Probe::Locked { heap: 0, tid, mine: None }].into(), locks: [LockRow::Busy(32)].into(), ..Default::default() };
        let mut tmfd = TM_FailureData::default();
        assert_eq!(run(&mut d, 7, 5, false, false, &mut tmfd).unwrap(), TM_Result::TM_BeingModified);
        assert_eq!((tmfd.xmax, d.next_new), (32, 0), "no new version for a write that did not get its lock");
    }

    #[test]
    fn an_own_unfrozen_lock_is_held_and_written_in_place() {
        let tid = ItemPointerData::default();
        for (update, heap, inserter, want) in [(false, 0, 40, "inplace"), (true, 0, 40, "inplace"), (false, 0, 39, "over"), (true, 1, 40, "live")] {
            let mut d = Double { update, xid: 40, script: [Probe::Locked { heap, tid, mine: Some(inserter) }].into(), ..Default::default() };
            assert_eq!(go(&mut d, 5, false).0, TM_Result::TM_Ok);
            assert!(d.asked.is_empty(), "no heap lock asked");
            let t = Tombstone { rowident: 7, succ: if update { 1 } else { SUCC_NONE } };
            let got = (d.inplace, d.over, d.live);
            match want {
                "inplace" => assert_eq!(got, (vec![t], vec![], vec![])),
                "over" => assert_eq!(got, (vec![], vec![(0, t)], vec![])),
                _ => assert_eq!(got, (vec![], vec![], vec![t])),
            }
        }
    }

    #[test]
    fn no_wait_returns_being_modified() {
        let mut d = Double { script: [Probe::Busy { xmax: 21 }].into(), ..Default::default() };
        let mut tmfd = TM_FailureData::default();
        assert_eq!(run(&mut d, 7, 5, false, false, &mut tmfd).unwrap(), TM_Result::TM_BeingModified);
        assert_eq!(tmfd.xmax, 21);
    }
}
