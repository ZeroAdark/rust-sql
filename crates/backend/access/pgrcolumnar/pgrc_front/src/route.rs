
use std::cell::Cell;

use mcx::Mcx;
use types_core::{CommandId, ForkNumber, Oid};
use types_error::{PgError, PgResult};
use types_rel::{NoLock, Relation, RowExclusiveLock, ShareUpdateExclusiveLock};
use types_slot::SlotData;
use types_tuple::{ItemPointerData, ItemPointerGetBlockNumber, ItemPointerGetOffsetNumberNoCheck, ItemPointerSet};
use types_storage::waiteventset::{WL_EXIT_ON_PM_DEATH, WL_LATCH_SET, WL_TIMEOUT};

use pgrc_am::rowid_alloc;
use pgrc_format::rowid::{seg_local, tid_of, SEGEXT_SOFT_ENTRIES, SEG_EXTENT_IDS, SEG_SOFT_EXTENTS};

use crate::maint;
use crate::ring::{self, ActiveSeg};

pub const BULK_THRESHOLD: u64 = u64::MAX;
const BULK_GUC: &str = "pgrust.pgrc_bulk_rows";

const BACKPRESSURE_MS: u32 = 1000;
const NAP_MS: u32 = 10;
const WAIT_EVENT_PG_SLEEP: u32 = 0x0900_0000 | 2;

pub const L0_CAP: u64 = 4096;
const L0_CAP_GUC: &str = "pgrust.pgrc_l0_parts_cap";
const L0_CAP_EVENT: &str = "PgrcL0Cap";

thread_local! {
    static WAITED: Cell<(Oid, u32, u32)> = const { Cell::new((0, 0, 0)) };
    static BULK: Cell<Option<(u64, u64)>> = const { Cell::new(None) };
    static L0CAP: Cell<Option<(u64, u64)>> = const { Cell::new(None) };
    static L0_EVENT: Cell<u32> = const { Cell::new(0) };
}

pub fn l0_cap() -> u64 {
    L0CAP.with(|c| maint::per_guc_change(c, || maint::setting(L0_CAP_GUC).unwrap_or(L0_CAP)))
}

pub fn bulk_rows() -> u64 {
    BULK.with(|b| maint::per_guc_change(b, || maint::setting(BULK_GUC).unwrap_or(BULK_THRESHOLD)))
}

pub fn statement_end() {
    WAITED.with(|w| w.set((0, 0, 0)));
    maint::publish_pending();
}

pub fn backpressure(db: Oid, rel: &Relation<'_>, cid: u32) -> PgResult<()> {
    let parent = rel.rd_id;
    let ext_over = || crate::pathcache::extents_over(rel);
    let over = maint::over_cap(parent);
    if !over && !ext_over()? {
        return Ok(());
    }
    let spent = spent_on(parent, cid);
    if spent >= BACKPRESSURE_MS {
        return Ok(());
    }
    // A held lock on a sealing segment skips the wait: that seal's drain may wait for this transaction. The active
    // segment's lock does not (SEALSIZE, main 10-04: the exemption let a long writer pass the cap): its holder waits
    // while a seal is in progress, the one thing that lowers the backlog without draining it.
    let (sealing, holds_active) = held_segments(rel, parent)?;
    if sealing {
        exempt_rest(parent, cid);
        return Ok(());
    }
    maint::queue(db, parent);
    // the row cap waits only on a seal or a rotation in flight, the things that lower it; over it with neither, the
    // table's rotation starts (one a table) and the wait is on that, never on a rotation the wait itself would block.
    let ext_wait = || -> PgResult<bool> { Ok((!holds_active || maint::sealing_in_progress(parent)) && ext_over()?) };
    let rows_go = over && maint::start_stall_rotation(db, parent);
    if !rows_go && !ext_wait()? {
        return Ok(());
    }
    maint::note_bp_wait(parent);
    let (mut over, mut first) = (over, true);
    wait_while(parent, cid, spent, WAIT_EVENT_PG_SLEEP, &mut || {
        if !std::mem::take(&mut first) {
            // RBPROF: a rotation since the entry may have made the held active segment sealing: the exemption again
            if holds_active && held_segments(rel, parent)?.0 {
                exempt_rest(parent, cid);
                return Ok(false);
            }
            over = maint::over_cap(parent);
        }
        Ok((over && maint::in_flight(parent)) || ext_wait()?)
    })
}

/// Whether this transaction holds a sealing segment of `parent` (its seal's drain may wait for us), and the active one.
fn held_segments(rel: &Relation<'_>, parent: Oid) -> PgResult<(bool, bool)> {
    maint::note_bp_check(parent);
    let active = crate::ring::find_active(rel)?.map(|a| a.relid);
    let mut holds_active = false;
    for s in crate::segments(parent)? {
        if lmgr::CheckRelationOidLockedByMe(s, RowExclusiveLock, true) {
            if Some(s) != active {
                return Ok((true, holds_active));
            }
            holds_active = true;
        }
    }
    Ok((false, holds_active))
}

/// RBPROF: the exemption holds for the rest of the statement (lock held to xact end; sealing never turns active), so
/// its budget is spent: no [`held_segments`] walk a row (turn/checks). The L0 wait never runs with a segment lock held.
fn exempt_rest(parent: Oid, cid: u32) {
    WAITED.with(|w| w.set((parent, cid, BACKPRESSURE_MS)));
}

/// The statement's spent backpressure budget on `parent` (`cid`: its command).
fn spent_on(parent: Oid, cid: u32) -> u32 {
    let (r, c, spent) = WAITED.with(|w| w.get());
    if (r, c) == (parent, cid) { spent } else { 0 }
}

fn wait_while(parent: Oid, cid: u32, mut spent: u32, event: u32, more: &mut dyn FnMut() -> PgResult<bool>) -> PgResult<()> {
    let from = spent;
    let out = nap_while(parent, cid, &mut spent, event, more);
    maint::note_bp_ms(parent, u64::from(spent - from));
    out
}

fn nap_while(parent: Oid, cid: u32, spent: &mut u32, event: u32, more: &mut dyn FnMut() -> PgResult<bool>) -> PgResult<()> {
    while *spent < BACKPRESSURE_MS && more()? {
        if latch_seams::wait_latch_my_latch::call(WL_LATCH_SET | WL_TIMEOUT | WL_EXIT_ON_PM_DEATH, NAP_MS as i64, event) & WL_LATCH_SET != 0 {
            latch_seams::reset_latch_my_latch::call();
        }
        *spent += NAP_MS;
        WAITED.with(|w| w.set((parent, cid, *spent)));
        postgres_seams::check_for_interrupts::call()?;
    }
    Ok(())
}

/// Never while this transaction holds what a job takes or might wait for: ShareUpdateExclusiveLock or stronger on the
/// table, its maintenance lock, a segment's lock.
pub fn l0_backpressure(rel: &Relation<'_>) -> PgResult<()> {
    let cap = l0_cap();
    if cap == 0 || !maint::compactor_on() {
        return Ok(());
    }
    let l = rel.rd_locator.get();
    let slot = (l.spcOid, l.dbOid, l.relNumber as u64);
    let parent = rel.rd_id;
    let l0 = || pgrc_am::l0::newest(slot).unwrap_or(0);
    let now = l0();
    if u64::from(now) < cap || maint::futile(parent, now) {
        return Ok(());
    }
    let cid = xact::GetCurrentCommandId(false)?;
    let spent = spent_on(parent, cid);
    if spent >= BACKPRESSURE_MS || lmgr::CheckRelationOidLockedByMe(parent, ShareUpdateExclusiveLock, true) || maint::maint_held(parent) {
        return Ok(());
    }
    for s in crate::segments(parent)? {
        if lmgr::CheckRelationOidLockedByMe(s, RowExclusiveLock, true) {
            return Ok(());
        }
    }
    let db = init_small::globals::MyDatabaseId();
    if maint::sealed(db, parent) && pgrc_front_seams::wake_compaction::is_installed() {
        pgrc_front_seams::wake_compaction::call();
    }
    maint::note_l0_wait(parent);
    let low = cap - cap / 16;
    wait_while(parent, cid, spent, l0_event(), &mut || {
        let n = l0();
        Ok(u64::from(n) >= low && (maint::compact_queued(db, parent) || maint::compacting(parent)) && !maint::futile(parent, n))
    })
}

fn l0_event() -> u32 {
    L0_EVENT.with(|e| {
        if e.get() == 0 {
            e.set(waitevent::custom::WaitEventExtensionNew(L0_CAP_EVENT).unwrap_or(waitevent::PG_WAIT_EXTENSION));
        }
        e.get()
    })
}

pub(crate) fn extents_over(e: &rowid_alloc::Extents) -> bool {
    e.len() >= SEGEXT_SOFT_ENTRIES || e.widest() >= SEG_SOFT_EXTENTS
}

/// Every writer of segment rows asks here first (`insert_row`, the statement-end flush of buffered rows, UPDATE's
/// new version), so a transaction's first write to the segment is seen here: for a `sorted_by` table it may have to
/// invalidate cached plans holding the claim; once per transaction and segment.
pub fn segment<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>) -> PgResult<ActiveSeg> {
    let seg = active_segment(mcx, rel)?;
    if !pgrc_am::front_claim::front_checked(rel, seg.relid) {
        let segrel = crate::open_segment(mcx, seg.relid)?;
        let n = bufmgr::RelationGetNumberOfBlocksInFork(&segrel, ForkNumber::MAIN_FORKNUM);
        segrel.close(NoLock)?;
        pgrc_am::front_claim::front_write(rel, seg.relid, n? == 0)?;
    }
    Ok(seg)
}

fn active_segment<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>) -> PgResult<ActiveSeg> {
    if let Some(a) = ring::active(rel)? {
        return Ok(a);
    }
    if detached(rel) && pgrc_front_seams::activate_detached::call(rel.rd_id)? {
        if let Some(a) = ring::active(rel)? {
            return Ok(a);
        }
    }
    pgrc_am::refuse_v1_format(rel)?;
    lmgr::LockRelationOid(rel.rd_id, ShareUpdateExclusiveLock)?;
    crate::ensure_companions(mcx, rel.rd_id)?;
    ring::active(rel)?.ok_or_else(|| Box::new(PgError::error(format!("pgrcolumnar: relation {} has no active segment", rel.rd_id))))
}

/// Whether a worker may activate `rel` in its own transaction while this one waits for it. Not a temp table (another
/// backend cannot see its storage), not without the worker seam (single-user mode, unit tests), and not while this
/// transaction holds a lock on `rel` the worker's ShareUpdateExclusiveLock conflicts with (SUEL or stronger): the
/// worker would wait for this transaction, which waits for the worker.
fn detached(rel: &Relation<'_>) -> bool {
    rel.rd_rel.relpersistence != types_core::RELPERSISTENCE_TEMP
        && pgrc_front_seams::activate_detached::is_installed()
        && !lmgr::CheckRelationOidLockedByMe(rel.rd_id, ShareUpdateExclusiveLock, true)
}

pub fn row_id(rel: &Relation<'_>, seg: &ActiveSeg, block: u32, off: u16) -> PgResult<u64> {
    let Some(local) = seg_local(block, off) else {
        return Err(Box::new(PgError::error(format!("pgrcolumnar: segment {} is past its position range", seg.relid)).with_sqlstate(types_error::ERRCODE_PROGRAM_LIMIT_EXCEEDED)));
    };
    let idx = (u64::from(local) / SEG_EXTENT_IDS) as u32;
    Ok(crate::pathcache::extent_base(rel, seg.part_no, idx)? + u64::from(local) % SEG_EXTENT_IDS)
}

pub fn insert_row<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, slot: &mut SlotData<'mcx>, cid: CommandId, options: i32) -> PgResult<u64> {
    insert_row_in(mcx, rel, slot, cid, options, None)
}

pub fn insert_row_in<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, slot: &mut SlotData<'mcx>, cid: CommandId, options: i32, held: Option<ActiveSeg>) -> PgResult<u64> {
    let db = init_small::globals::MyDatabaseId();
    backpressure(db, rel, cid)?;
    let seg = match held {
        Some(h) if ring::find_active(rel)? == Some(h) => h,
        _ => segment(mcx, rel)?,
    };
    let segrel = crate::open_segment(mcx, seg.relid)?;
    let r = heap_insert_slot(mcx, &segrel, slot, cid, options);
    let segloc = segrel.rd_locator.get();
    segrel.close(NoLock)?;
    let tid = r?;
    let (block, off) = (ItemPointerGetBlockNumber(&tid), ItemPointerGetOffsetNumberNoCheck(&tid));
    pgrc_marks::written((segloc.spcOid, segloc.dbOid, segloc.relNumber), block);
    let id = row_id(rel, &seg, block, off)?;
    maint::inserted(db, rel.rd_id, seg.relid, block);
    let base = slot.base_mut();
    let (pblock, poff) = tid_of(id);
    ItemPointerSet(&mut base.tts_tid, pblock, poff);
    base.tts_tableOid = rel.rd_id;
    Ok(id)
}

fn heap_insert_slot<'mcx>(mcx: Mcx<'mcx>, seg: &Relation<'mcx>, slot: &mut SlotData<'mcx>, cid: CommandId, options: i32) -> PgResult<ItemPointerData> {
    exectuples::exec_materialize_slot(slot, mcx)?;
    let tuple = match slot {
        SlotData::Heap(h) => h.tuple.as_mut(),
        SlotData::BufferHeap(b) => b.base.tuple.as_mut(),
        _ => None,
    };
    if let Some(t) = tuple {
        t.t_tableOid = seg.rd_id;
        heapam::heap_insert(seg, t, cid, options, None)?;
        return Ok(t.t_self);
    }
    let mut t = exectuples::exec_copy_slot_heap_tuple(slot, mcx, mcx)?;
    t.t_tableOid = seg.rd_id;
    heapam::heap_insert(seg, &mut t, cid, options, None)?;
    Ok(t.t_self)
}

pub fn admit_insert(on_conflict: bool) -> PgResult<()> {
    match on_conflict {
        true => Err(pgrc_am::unsupported("INSERT ... ON CONFLICT")),
        false => Ok(()),
    }
}

/// An INSERT whose rows an AFTER ROW INSERT trigger or a NEW TABLE capture fetches back by position must not buffer
/// them (a buffered row has no position until the flush, and the bulk lane's parts none until publish): they take the
/// single-row segment path, as heap COPY turns multi-insert off for BEFORE / INSTEAD row triggers.
pub fn needs_position(trig: Option<&types_trigger::TriggerDesc<'_>>, new_table_capture: bool) -> bool {
    new_table_capture || trig.is_some_and(|t| t.trig_insert_after_row || t.trig_insert_new_table)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_conflict_refuses_and_typed_triggers_need_positions() {
        assert!(admit_insert(false).is_ok());
        assert_eq!(admit_insert(true).unwrap_err().sqlstate(), types_error::ERRCODE_FEATURE_NOT_SUPPORTED);
        assert!(!needs_position(None, false));
        assert!(needs_position(None, true));
    }

    #[test]
    fn extents_over_at_either_soft_cap() {
        use pgrc_format::rowid::{SegExtent, SEGEXT_MAX_ENTRIES};
        let ext = |segs: u32, wide: u32| {
            let mut t = Vec::new();
            for p in 0..segs {
                for idx in 0..if p == 0 { wide } else { 1 } {
                    t.push(SegExtent { part_no: p, idx, base: 291 + SEG_EXTENT_IDS * t.len() as u64 });
                }
            }
            rowid_alloc::Extents::new(t)
        };
        assert!(!extents_over(&ext(1, 1)));
        assert!(!extents_over(&ext(1, SEG_SOFT_EXTENTS - 1)), "one under the per-segment cap");
        assert!(extents_over(&ext(1, SEG_SOFT_EXTENTS)), "a segment at its soft cap waits for the rotation");
        assert!(!extents_over(&ext(SEGEXT_SOFT_ENTRIES as u32 - 1, 1)), "one under the table cap");
        assert!(extents_over(&ext(SEGEXT_SOFT_ENTRIES as u32, 1)), "a table at its soft cap waits for the seals");
        assert!(SEGEXT_SOFT_ENTRIES < SEGEXT_MAX_ENTRIES, "the soft cap is below the hard one");
    }
}
