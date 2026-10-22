//! The tableam DML arms of pgrcolumnar: a position resolves to its RowIdent and Stage B writes the tombstone. A write's
//! row lock is its tombstone; a lock alone (`SELECT ... FOR <strength>`, EPQ, a BEFORE ROW trigger's row) is a LOCK row
//! in the tombstone heap, locked as a heap row.

use mcx::Mcx;
use tableam_vocab::{LockTupleMode, LockWaitPolicy, Snapshot, TM_FailureData, TM_Result, TUPLE_LOCK_FLAG_FIND_LAST_VERSION};
use types_core::CommandId;
use types_error::{PgError, PgResult};
use types_nodes::CmdType;
use types_rel::Relation;
use types_slot::SlotData;
use types_tuple::{ItemPointerData, ItemPointerSetMovedPartitions};

use crate::position::{self, ViewRef};
use crate::stageb::{self, Locking, Probe};

pub use crate::fetch::fetch_row_version as fetch;

pub fn admit(op: CmdType, on_conflict: bool) -> PgResult<()> {
    match op {
        CmdType::CMD_INSERT => crate::route::admit_insert(on_conflict),
        CmdType::CMD_UPDATE | CmdType::CMD_DELETE => Ok(()),
        CmdType::CMD_MERGE => Err(pgrc_am::dml::trickle_unsupported(pgrc_am::dml::TrickleOp::Merge)),
        _ => Err(pgrc_am::unsupported("this DML operation")),
    }
}

/// The RowIdent at a position this statement minted (a row trigger event's tuple, kept past the statement by a
/// deferred event: the RowIdent survives seals and compactions, the position does not).
pub fn rowident_of<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, tid: &ItemPointerData) -> PgResult<u64> {
    rowident_at(mcx, rel, tid)
}

#[allow(clippy::too_many_arguments)]
pub fn delete<'mcx>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    tid: &ItemPointerData,
    cid: CommandId,
    _snapshot: &Snapshot<'mcx>,
    wait: bool,
    tmfd: &mut TM_FailureData,
    changing_part: bool,
) -> PgResult<TM_Result> {
    let r = rowident_at(mcx, rel, tid)?;
    tmfd.ctid = *tid;
    let res = visible(stageb::write(mcx, rel, r, None, cid, wait, changing_part, tmfd)?, "delete")?;
    if res == TM_Result::TM_Ok {
        crate::stats::deleted(rel);
        if crate::logical::active(rel) {
            crate::logical::deleted(mcx, rel, tid)?;
        }
    }
    Ok(res)
}

#[allow(clippy::too_many_arguments)]
pub fn update<'mcx>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    otid: &ItemPointerData,
    slot: &mut SlotData<'mcx>,
    cid: CommandId,
    _snapshot: &Snapshot<'mcx>,
    wait: bool,
    tmfd: &mut TM_FailureData,
    lockmode: &mut LockTupleMode,
) -> PgResult<TM_Result> {
    // a no-key update (a pgrc table has no unique index): EPQ's lock and a KEY SHARE holder agree with heap's
    *lockmode = LockTupleMode::LockTupleNoKeyExclusive;
    let r = rowident_at(mcx, rel, otid)?;
    tmfd.ctid = *otid;
    let res = visible(stageb::write(mcx, rel, r, Some(&mut *slot), cid, wait, false, tmfd)?, "update")?;
    if res == TM_Result::TM_Ok {
        crate::stats::updated(rel);
        if crate::logical::active(rel) {
            crate::logical::updated(mcx, rel, otid, slot)?;
        }
    }
    Ok(res)
}

#[allow(clippy::too_many_arguments)]
pub fn lock<'mcx>(
    mcx: Mcx<'mcx>,
    rel: &Relation<'mcx>,
    tid: &ItemPointerData,
    _snapshot: &Snapshot<'mcx>,
    slot: &mut SlotData<'mcx>,
    cid: CommandId,
    mode: LockTupleMode,
    policy: LockWaitPolicy,
    flags: u8,
    tmfd: &mut TM_FailureData,
) -> PgResult<TM_Result> {
    let follow = flags & TUPLE_LOCK_FLAG_FIND_LAST_VERSION != 0;
    tmfd.traversed = false;
    let (mut r, mut at) = (rowident_at(mcx, rel, tid)?, *tid);
    loop {
        let (xmax, res) = match stageb::lock_row(mcx, rel, r, cid, mode, policy)? {
            Locking::Held => break,
            Locking::WouldBlock { xmax } => (xmax, TM_Result::TM_WouldBlock),
            Locking::Answer(Probe::Own { cmin }) if cmin >= cid => {
                tmfd.xmax = xact::GetCurrentTransactionIdIfAny();
                tmfd.cmax = cmin;
                return Ok(TM_Result::TM_SelfModified);
            }
            Locking::Answer(Probe::Own { .. }) => return Ok(TM_Result::TM_Invisible),
            Locking::Answer(Probe::Deleted { xmax }) => (xmax, TM_Result::TM_Deleted),
            Locking::Answer(Probe::Moved { .. }) if follow => return Err(moved()),
            Locking::Answer(Probe::Moved { xmax }) => {
                ItemPointerSetMovedPartitions(&mut at);
                (xmax, TM_Result::TM_Updated)
            }
            Locking::Answer(Probe::Updated { succ, xmax }) => {
                let mut view = ViewRef::default();
                match position::find(mcx, rel, succ, &mut view)? {
                    None => (xmax, TM_Result::TM_Deleted),
                    Some(found) => {
                        (at, r) = (position::stamp(rel, &found)?, succ);
                        if !follow {
                            (xmax, TM_Result::TM_Updated)
                        } else {
                            tmfd.traversed = true;
                            continue;
                        }
                    }
                }
            }
            Locking::Answer(p) => {
                return Err(Box::new(PgError::error(format!("pgrcolumnar: unexpected row lock answer {p:?}")).with_sqlstate(types_error::ERRCODE_INTERNAL_ERROR)))
            }
        };
        tmfd.xmax = xmax;
        tmfd.cmax = types_core::InvalidCommandId;
        tmfd.ctid = at;
        return Ok(res);
    }
    match fetch(mcx, rel, &at, &None, slot)? {
        true => Ok(TM_Result::TM_Ok),
        false => Ok(TM_Result::TM_Deleted),
    }
}

fn rowident_at<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>, tid: &ItemPointerData) -> PgResult<u64> {
    if crate::pathcache::was_located(rel, types_tuple::ItemPointerGetBlockNumberNoCheck(tid), types_tuple::ItemPointerGetOffsetNumberNoCheck(tid))? {
        return position::id_at(rel, tid);
    }
    let mut view = ViewRef::default();
    position::rowident(&position::locate(mcx, rel, tid, &mut view)?)
}

/// heap_delete / heap_update raise on a row the command cannot see.
fn visible(res: TM_Result, what: &str) -> PgResult<TM_Result> {
    match res {
        TM_Result::TM_Invisible => {
            Err(Box::new(PgError::error(format!("attempted to {what} invisible tuple")).with_sqlstate(types_error::ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE)))
        }
        r => Ok(r),
    }
}

fn moved() -> Box<PgError> {
    Box::new(
        PgError::error("tuple to be locked was already moved to another partition due to concurrent update")
            .with_sqlstate(types_error::ERRCODE_T_R_SERIALIZATION_FAILURE),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_and_on_conflict_refuse_typed_update_and_delete_are_admitted() {
        for (op, on_conflict, ok) in [
            (CmdType::CMD_INSERT, false, true),
            (CmdType::CMD_UPDATE, false, true),
            (CmdType::CMD_DELETE, false, true),
            (CmdType::CMD_INSERT, true, false),
            (CmdType::CMD_MERGE, false, false),
        ] {
            match admit(op, on_conflict) {
                Ok(()) => assert!(ok, "{op:?} refused"),
                Err(e) => assert!(!ok && e.sqlstate() == types_error::ERRCODE_FEATURE_NOT_SUPPORTED, "{op:?}: {}", e.message()),
            }
        }
    }

    #[test]
    fn row_triggers_never_refuse() {
        assert!(admit(CmdType::CMD_UPDATE, false).is_ok());
        assert!(admit(CmdType::CMD_DELETE, false).is_ok());
    }
}
