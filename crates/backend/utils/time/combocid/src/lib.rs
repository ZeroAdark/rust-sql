#![allow(non_snake_case)]
#![allow(clippy::result_large_err)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use types_core::CommandId;
use types_error::PgResult;
use types_tuple::{HeapTupleHeaderData, HEAP_COMBOCID, HEAP_MOVED};

#[cfg(test)]
mod tests;

#[derive(Default)]
struct ComboCidState {
    comboCids: Vec<(CommandId, CommandId)>,
    comboHash: HashMap<(CommandId, CommandId), CommandId>,
}

thread_local! {
    static STATE: RefCell<ComboCidState> = RefCell::new(ComboCidState::default());
}

// GetCmin/GetCmax are only valid from the originating transaction; other
// readers must use raw_command_id() directly.
pub fn HeapTupleHeaderGetCmin(tup: &HeapTupleHeaderData) -> CommandId {
    let cid = tup.raw_command_id();

    debug_assert!((tup.t_infomask & HEAP_MOVED) == 0);
    debug_assert!(xact_seams::transaction_id_is_current_transaction_id::call(tup.xmin()));

    if (tup.t_infomask & HEAP_COMBOCID) != 0 {
        GetRealCmin(cid)
    } else {
        cid
    }
}

pub fn HeapTupleHeaderGetCmax(tup: &HeapTupleHeaderData) -> CommandId {
    let cid = tup.raw_command_id();

    debug_assert!((tup.t_infomask & HEAP_MOVED) == 0);
    // Skipped inside critical sections: GetUpdateXid may allocate (multixact).
    debug_assert!(
        init_small::globals::CritSectionCount() > 0
            || xact_seams::transaction_id_is_current_transaction_id::call(
                heapam::HeapTupleHeaderGetUpdateXid(tup).expect("HeapTupleHeaderGetUpdateXid")
            )
    );

    if (tup.t_infomask & HEAP_COMBOCID) != 0 {
        GetRealCmax(cid)
    } else {
        cid
    }
}

pub fn HeapTupleHeaderAdjustCmax(
    tup: &HeapTupleHeaderData,
    cmax: CommandId,
) -> PgResult<(CommandId, bool)> {
    if !tup.xmin_committed()
        && xact_seams::transaction_id_is_current_transaction_id::call(tup.xmin_raw())
    {
        let cmin = HeapTupleHeaderGetCmin(tup);
        Ok((GetComboCommandId(cmin, cmax)?, true))
    } else {
        Ok((cmax, false))
    }
}

/// How many combo CIDs this backend's transaction has made. The map lives in the
/// originating thread alone, so a reader on another thread (a Gravity pool worker)
/// cannot resolve a `HEAP_COMBOCID` tuple: a non-zero count means such a tuple may
/// be met and the scan belongs on the leader.
pub fn combo_cid_count() -> usize {
    STATE.with(|s| s.borrow().comboCids.len())
}

pub fn AtEOXact_ComboCid() {
    STATE.with(|s| {
        // Drop the capacity too rather than pinning the high-water mark for the session.
        *s.borrow_mut() = ComboCidState::default();
    });
}

fn GetComboCommandId(cmin: CommandId, cmax: CommandId) -> PgResult<CommandId> {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let ComboCidState { comboCids, comboHash } = &mut *s;
        let next = comboCids.len() as CommandId;
        if let Some(&existing) = comboHash.get(&(cmin, cmax)) {
            return Ok(existing);
        }
        // Reserve both BEFORE inserting so the pair stays consistent.
        comboCids
            .try_reserve(1)
            .map_err(|_| ::mcx::oom_named("TopTransactionContext", core::mem::size_of::<(CommandId, CommandId)>()))?;
        comboHash
            .try_reserve(1)
            .map_err(|_| ::mcx::oom_named("TopTransactionContext", core::mem::size_of::<((CommandId, CommandId), CommandId)>()))?;
        comboHash.insert((cmin, cmax), next);
        comboCids.push((cmin, cmax));
        Ok(next)
    })
}

fn GetRealCmin(combocid: CommandId) -> CommandId {
    STATE.with(|s| {
        let s = s.borrow();
        debug_assert!((combocid as usize) < s.comboCids.len());
        s.comboCids[combocid as usize].0
    })
}

fn GetRealCmax(combocid: CommandId) -> CommandId {
    STATE.with(|s| {
        let s = s.borrow();
        debug_assert!((combocid as usize) < s.comboCids.len());
        s.comboCids[combocid as usize].1
    })
}

// Arc'd snapshot instead of C's byte image: immutable while parallel mode's
// fences hold (no writer can create combo CIDs during a parallel operation).
pub fn SerializeComboCIDState() -> Arc<[(CommandId, CommandId)]> {
    STATE.with(|s| Arc::from(s.borrow().comboCids.as_slice()))
}

// Only valid in a worker with no combo CIDs yet.
pub fn RestoreComboCIDState(state: &Arc<[(CommandId, CommandId)]>) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        debug_assert!(s.comboCids.is_empty() && s.comboHash.is_empty());
        s.comboCids.extend_from_slice(state);
        s.comboHash
            .extend(state.iter().enumerate().map(|(i, &key)| (key, i as CommandId)));
    });
}

pub fn init_seams() {
    combocid_seams::at_eoxact_combocid::set(AtEOXact_ComboCid);
    combocid_seams::heap_tuple_header_get_cmin::set(HeapTupleHeaderGetCmin);
    combocid_seams::heap_tuple_header_get_cmax::set(HeapTupleHeaderGetCmax);
    combocid_seams::heap_tuple_header_adjust_cmax::set(HeapTupleHeaderAdjustCmax);
    combocid_seams::combo_cid_count::set(combo_cid_count);
}
