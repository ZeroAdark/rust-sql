//! pglpoint S3a: a plain point DELETE after planning, under pgrust.point_write (default off). The planner's ModifyTable
//! over an IndexScan of a unique btree of a heap table, equality on every key column against a Const, no RETURNING: run
//! in ExecutorStart / ExecutorRun / ExecutorFinish's order through the Gravity program's own hosts with no program
//! (`hosts::delete`: the write host's bind and heap_delete, the index walk, READ COMMITTED's EvalPlanQual through the same
//! two). Anything else is `Verdict::Not` / `Declined`: Gravity serves it unchanged.
use gravity_program::Cmd;
use mcx::Mcx;
use tcop_dest::DestReceiver;
use types_error::PgResult;
use types_nodes::nodes_enums::CmdType;
use types_nodes::parsenodes::RTEKind;
use types_nodes::plannodes::PlannedStmt;
use types_portal::QueryCompletion;
use types_rel::{NoLock, Relation};

use super::keep::{self, Kept, KeptBox, Made, WriteKept};
use super::run::{exec_cmd_in, with_run};
use super::select::{self, Verdict};
use super::{hosts, probe, rel, skip};

/// The statement-level and plan-level test, pointer checks only (the relations' own are `rel::write_refusal` and S1's
/// `probe::index_refusal`). The scan is S1's shape (`select::Shape`), under a ModifyTable that deletes from it alone.
pub(super) fn shape<'p>(p: &PlannedStmt<'p>) -> Verdict<'p> {
    let Some(mt) = p.planTree.and_then(|n| n.as_modify_table()) else {
        return Verdict::Not;
    };
    if p.commandType != CmdType::CMD_DELETE || mt.operation != CmdType::CMD_DELETE {
        return Verdict::Not;
    }
    let plain = p.utilityStmt.is_none() && p.canSetTag && mt.canSetTag && !p.hasReturning && !p.hasModifyingCTE;
    // the one PARAM_EXEC a plain DELETE has is its ModifyTable's epqParam (make_modifytable's SS_assign_special_param)
    let empty = p.rowMarks.is_nil() && p.subplans.is_nil() && p.paramExecTypes.len() <= 1 && p.resultRelations.len() == 1 && !p.parallelModeNeeded;
    if !(plain && empty) {
        return Verdict::Declined("statement");
    }
    // one result relation, the scan's (rti 1), with no RETURNING, check option, row mark, ON CONFLICT (0: ONCONFLICT_NONE),
    // MERGE action or direct foreign modify, and nothing above the scan but the ModifyTable
    let one = mt.resultRelations.len() == 1 && mt.resultRelations.nth(0) == 1 && mt.nominalRelation == 1 && mt.rootRelation == 0 && !mt.partColsUpdated;
    let bare = mt.withCheckOptionLists.is_nil() && mt.returningLists.is_nil() && mt.rowMarks.is_nil() && mt.onConflictAction == 0 && mt.mergeActionLists.is_nil() && mt.fdwDirectModifyPlans.is_empty() && mt.updateColnosLists.is_nil();
    let top = &mt.plan;
    let alone = top.righttree.is_none() && top.initPlan.is_nil() && top.qual.is_nil() && top.targetlist.is_nil();
    let Some(scan) = top.lefttree.and_then(|n| n.as_index_scan()).filter(|_| one && bare && alone) else {
        return Verdict::Declined("plan");
    };
    let Some(rte) = (p.rtable.len() == 1).then(|| p.rtable.nth(0).as_range_tbl_entry()).flatten() else {
        return Verdict::Declined("range table");
    };
    if rte.rtekind != RTEKind::RTE_RELATION || rte.inh || !rte.securityQuals.is_nil() || scan.scan.scanrelid != 1 {
        return Verdict::Declined("range table");
    }
    let plan = &scan.scan.plan;
    let bare = plan.lefttree.is_none() && plan.righttree.is_none() && plan.initPlan.is_nil() && plan.qual.is_nil() && scan.indexorderby.is_nil();
    if !bare || scan.indexqual.is_nil() {
        return Verdict::Declined("plan");
    }
    // the scan's row identity (ctid) alone: junk entries the write reads as the walk's position
    if !plan.targetlist.iter().all(|t| t.as_target_entry().is_some_and(|te| te.resjunk)) {
        return Verdict::Declined("target list");
    }
    Verdict::Take(select::Shape { scan, relid: rte.relid, lockmode: rte.rellockmode })
}

/// Hook B for a DELETE: `None` leaves it to Gravity, unchanged. `hit`: S5 served the plan from its memo slot, which keeps
/// the run's pieces (keep.rs).
pub(super) fn serve(pstmt: &PlannedStmt<'_>, dest: &mut DestReceiver<'_>, hit: Option<skip::Hit>) -> Option<PgResult<Option<QueryCompletion>>> {
    let s = match shape(pstmt) {
        Verdict::Not => return None,
        Verdict::Declined(why) => return declined(why),
        Verdict::Take(s) => s,
    };
    // as S1: the planner's own plan, so a decline is a word in the log in every mode. SERIALIZABLE (its predicate locks and
    // conflict checks) and logical decoding's WAL are later widenings with their own proof
    let refusal = super::runtime_refusal(dest.mydest())
        .or_else(|| xact::IsolationIsSerializable().then_some("serializable"))
        .or_else(|| crate::inputs::knobs::seam::logical_wal().then_some("wal_level logical"));
    if let Some(why) = refusal {
        return declined(why);
    }
    // D4572: a slot whose pieces found no room runs as with pgrust.point_keep off (`skip::Taken::NoRoom`: no hit)
    let (hit, kept) = match hit.map(|h| (h, skip::take_kept(&h))) {
        Some((h, skip::Taken::Kept(kb))) => (Some(h), Some((h, kb))),
        Some((h, skip::Taken::Make)) => (Some(h), None),
        Some((_, skip::Taken::NoRoom)) | None => (None, None),
    };
    let r = match with_run(|mcx, _| match kept {
        Some((h, kb)) if keep::write_ids(&kb, Cmd::Delete) == Some((s.relid, s.scan.indexid)) => run_kept(pstmt, &s, mcx, h, kb, None).map(Ok),
        _ => run_in(pstmt, &s, mcx, hit),
    }) {
        Ok(Err(why)) => return declined(why),
        Ok(Ok(qc)) => Ok(qc),
        Err(e) => Err(e),
    };
    if super::reporting() {
        super::report(&match &r {
            Ok(qc) => format!("served delete rows={}", qc.nprocessed), // alloc: Cold
            Err(_) => "served delete error".to_string(),                // alloc: Cold
        });
    }
    Some(r.map(Some))
}

#[cold]
fn declined(why: &str) -> Option<PgResult<Option<QueryCompletion>>> {
    if super::reporting() {
        super::report(&format!("declined delete ({why})")); // alloc: Cold
    }
    None
}

/// The table opened as ExecInitModifyTable's result relation (the parser's lock held) and the index as ExecInitIndexScan
/// opens it (`select::index_lockmode`), their tests (`Err` with the reason: nothing ran, Gravity serves it), then
/// ExecutorStart's checks for a DELETE and the statement's snapshot (`run::exec_cmd_in`) around the hosts' run, DELETE n.
/// With `hit` the hosts are made for the memo slot and kept there after the run (`run_kept`), what their making read
/// recorded into the slot's reads.
fn run_in(pstmt: &PlannedStmt<'_>, s: &select::Shape<'_>, mcx: Mcx<'_>, hit: Option<skip::Hit>) -> PgResult<Result<QueryCompletion, &'static str>> {
    if let Some(h) = hit {
        let (made, deps) = gravity_deps::recording(|| make(s, mcx));
        return match made? {
            Err(why) => Ok(Err(why)),
            Ok(k) => run_kept(pstmt, s, mcx, h, keep::boxed(Made::Write(k), skip::MEMO_OWNER)?, Some(&deps)).map(Ok),
        };
    }
    let heap = table::table_open(mcx, s.relid, NoLock)?;
    let index = indexam::index_open(mcx, s.scan.indexid, select::index_lockmode(s))?;
    let r = match refusal(s, &heap, &index) {
        Ok(Some(why)) => Ok(Err(why)),
        Ok(None) => {
            let p = hosts::Probe { heap: &heap, index: &index, indexqual: &s.scan.indexqual, cols: &[] };
            p.recipe().and_then(|recipe| {
                let mut h = hosts::Hosts::new(heap.rd_id, Cmd::Delete, None, recipe);
                exec_cmd_in(pstmt, Cmd::Delete, mcx, |snap| hosts::delete(&mut h, &hosts::EpqRecipe::Plan(&p), &s.scan.indexqual, snap)).map(|n| Ok(gravity_wire::completion(Cmd::Delete, n)))
            })
        }
        Err(e) => Err(e),
    };
    let closed = indexam::index_close(index, NoLock).and_then(|()| table::table_close(heap, NoLock));
    let r = r?;
    closed?;
    Ok(r)
}

/// The relation tests of a point DELETE: the table's as a write's (`rel::write_refusal`), the index's as S1's.
fn refusal(s: &select::Shape<'_>, heap: &Relation<'_>, index: &Relation<'_>) -> PgResult<Option<&'static str>> {
    if let Some(why) = rel::write_refusal(heap) {
        return Ok(Some(why));
    }
    let mut ops: probe::KeyOps = [(0, 0); types_core::fmgr::INDEX_MAX_KEYS as usize];
    select::relation_refusal(heap, index, s, &[], &mut ops)
}

/// The pieces a memo slot keeps for its DELETE (`run_in`'s opens and tests, then the walk's recipe and the hosts), the
/// relations closed again (the hosts re-open them at each bind).
fn make(s: &select::Shape<'_>, mcx: Mcx<'_>) -> PgResult<Result<WriteKept, &'static str>> {
    let heap = table::table_open(mcx, s.relid, NoLock)?;
    let index = indexam::index_open(mcx, s.scan.indexid, select::index_lockmode(s))?;
    let made = refusal(s, &heap, &index).and_then(|why| match why {
        Some(why) => Ok(Err(why)),
        None => {
            let p = hosts::Probe { heap: &heap, index: &index, indexqual: &s.scan.indexqual, cols: &[] };
            let recipe = p.recipe()?;
            let hosts = hosts::Hosts::new_held(heap.rd_id, Cmd::Delete, None, p.recipe()?, skip::MEMO_OWNER);
            Ok(Ok(WriteKept { cmd: Cmd::Delete, relid: s.relid, indexid: s.scan.indexid, recipe: core::cell::Cell::new(Some(recipe)), hosts, set: None }))
        }
    });
    let closed = indexam::index_close(index, NoLock).and_then(|()| table::table_close(heap, NoLock));
    let made = made?;
    closed?;
    Ok(made)
}

/// A DELETE over the memo slot's kept pieces: ExecInitIndexScan's index lock where hook A's left none, then ExecutorStart's
/// checks and the snapshot around the hosts' run; the pieces back into the slot when it returns, dropped when it raises.
fn run_kept(pstmt: &PlannedStmt<'_>, s: &select::Shape<'_>, mcx: Mcx<'_>, hit: skip::Hit, mut kb: KeptBox, deps: Option<&gravity_deps::Deps>) -> PgResult<QueryCompletion> {
    if super::reporting() {
        super::report(if deps.is_some() { "keep made" } else { "keep used" });
    }
    let mode = select::index_lockmode(s);
    if mode != NoLock {
        lmgr_seams::lock_relation_oid::call(s.scan.indexid, mode)?;
    }
    let n = kb.with_mut(|kin| -> PgResult<u64> {
        let Kept::Write(k) = &mut kin.kept else {
            return Err(keep::internal("a point DELETE's memo entry keeps another statement's pieces"));
        };
        let k: &mut WriteKept = k; // the pieces' fields borrowed apart, not through the box each time
        exec_cmd_in(pstmt, Cmd::Delete, mcx, |snap| hosts::delete(&mut k.hosts, &hosts::EpqRecipe::Kept(&k.recipe), &s.scan.indexqual, snap))
    })?;
    skip::put_kept(&hit, kb, deps);
    Ok(gravity_wire::completion(Cmd::Delete, n))
}
