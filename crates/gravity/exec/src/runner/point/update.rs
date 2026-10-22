//! pglpoint S3b: a plain point UPDATE after planning, under pgrust.point_write (default off). The planner's ModifyTable
//! over an IndexScan of a unique btree of a heap table, equality on every key column against a Const, no RETURNING, SET
//! values an expression of the row's own columns: run in ExecutorStart / ExecutorRun / ExecutorFinish's order through the
//! Gravity program's own hosts with no program (`hosts::update`: the write host's bind, constraint check, heap_update and
//! index maintenance, the index walk over the old row's columns, READ COMMITTED's EvalPlanQual through the same two), the
//! new row as ExecGetUpdateNewTuple makes it (keep.rs `SetKept`: the SET values computed by execexpr over the scanned
//! version, its other columns kept). Anything else is `Verdict::Not` / `Declined`: Gravity serves it unchanged. S3b serves only a plan
//! S5 (pgrust.point_noplan) served from its memo or just filed there, over the pieces the memo entry keeps (keep.rs): the
//! hosts, and the SET values compiled once with their constants as parameters (`keep::SetKept`). Built per statement,
//! they cost more than the admission S3b skips (measured: more instructions per UPDATE than Gravity's), so without a memo
//! entry, or with pgrust.point_keep off, the UPDATE is Gravity's.
use gravity_program::Cmd;
use mcx::{Mcx, PgVec};
use tcop_dest::DestReceiver;
use types_core::Oid;
use types_error::PgResult;
use types_nodes::list::{IntList, NodeList};
use types_nodes::nodes_enums::CmdType;
use types_nodes::parsenodes::RTEKind;
use types_nodes::plannodes::PlannedStmt;
use types_nodes::{Node, NodeTag};
use types_portal::QueryCompletion;
use types_rel::{NoLock, Relation};

use super::keep::{self, Kept, KeptBox, Made, WriteKept};
use super::run::{exec_cmd_in, with_run};
use super::select::{self, Verdict};
use super::{hosts, nofrom, probe, rel, skip};

/// The statement-level and plan-level test, pointer checks only (the relations' own are `rel::write_refusal`,
/// `rel::update_refusal` and S1's `probe::index_refusal`). The scan is S1's shape (`select::Shape`) under a ModifyTable
/// that updates it alone; its target list is the planner's for an UPDATE (`set_list`). `kept`: the table and index of the
/// memo slot's kept pieces (keep.rs), made for a plan of the slot that passed every test here; S5 serves each later
/// statement of the slot that plan but for its constants, so for the same table and index the SET values' walks
/// (`set_expr`, `nofrom::runs_sql`) answer as they did and are not run again.
pub(super) fn shape<'p>(p: &PlannedStmt<'p>, kept: Option<(Oid, Oid)>) -> Verdict<'p> {
    let Some(mt) = p.planTree.and_then(|n| n.as_modify_table()) else {
        return Verdict::Not;
    };
    if p.commandType != CmdType::CMD_UPDATE || mt.operation != CmdType::CMD_UPDATE {
        return Verdict::Not;
    }
    let plain = p.utilityStmt.is_none() && p.canSetTag && mt.canSetTag && !p.hasReturning && !p.hasModifyingCTE;
    // the one PARAM_EXEC a plain UPDATE has is its ModifyTable's epqParam (make_modifytable's SS_assign_special_param)
    let empty = p.rowMarks.is_nil() && p.subplans.is_nil() && p.paramExecTypes.len() <= 1 && p.resultRelations.len() == 1 && !p.parallelModeNeeded;
    if !(plain && empty) {
        return Verdict::Declined("statement");
    }
    // one result relation, the scan's (rti 1), with no RETURNING, check option, row mark, ON CONFLICT (0: ONCONFLICT_NONE),
    // MERGE action or direct foreign modify, one list of SET columns, and nothing above the scan but the ModifyTable
    let one = mt.resultRelations.len() == 1 && mt.resultRelations.nth(0) == 1 && mt.nominalRelation == 1 && mt.rootRelation == 0 && !mt.partColsUpdated;
    let bare = mt.withCheckOptionLists.is_nil() && mt.returningLists.is_nil() && mt.rowMarks.is_nil() && mt.onConflictAction == 0 && mt.mergeActionLists.is_nil() && mt.fdwDirectModifyPlans.is_empty() && mt.updateColnosLists.len() == 1;
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
    let Some(cols) = set_cols(mt) else {
        return Verdict::Declined("plan");
    };
    let walked = kept == Some((rte.relid, scan.indexid));
    if !set_list(&plan.targetlist, cols, !walked) {
        return Verdict::Declined("target list");
    }
    // a SET value that calls code running a statement of its own (a user's function, a LANGUAGE sql builtin, a catalog
    // query through SPI) would run it inside the point statement: S0a's test (`nofrom::runs_sql`)
    if !walked && nofrom::runs_sql(&plan.targetlist) {
        return Verdict::Declined(nofrom::RUNS_SQL);
    }
    Verdict::Take(select::Shape { scan, relid: rte.relid, lockmode: rte.rellockmode })
}

/// The ModifyTable's one list of SET columns (updateColnosLists' first), attnos in the target list's order.
fn set_cols<'p>(mt: &'p types_nodes::plannodes::ModifyTable<'p>) -> Option<&'p IntList<'p>> {
    mt.updateColnosLists.nth(0).as_int_list().filter(|l| !l.is_nil())
}

/// The scan's target list as preprocess_targetlist makes an UPDATE's: one entry per SET column first, in `cols`' order
/// (resno 1..n), each value an expression `set_expr` takes (`exprs`), then the row identity's junk entries only.
fn set_list(tl: &NodeList<'_>, cols: &IntList<'_>, exprs: bool) -> bool {
    let n = cols.len();
    tl.len() > n
        && tl.iter().enumerate().all(|(i, t)| {
            t.as_target_entry().is_some_and(|te| {
                if i < n {
                    !te.resjunk && te.resno as usize == i + 1 && cols.nth(i) > 0 && (!exprs || set_expr(te.expr))
                } else {
                    te.resjunk
                }
            })
        })
}

/// A SET value S3b computes with execexpr over the scanned row alone: the table's own user columns (no system column, no
/// whole row, no outer reference), no parameter, sublink or subplan, aggregate, window or grouping function, set-returning
/// call, domain coercion (its constraints) or sequence step of an identity; a node the walk cannot see counts.
fn set_expr(n: Node<'_>) -> bool {
    stack_depth_core::check_stack_depth_or_panic();
    let mut ok = match n.node_tag() {
        NodeTag::T_Var => n.as_var().is_some_and(|v| v.varno == 1 && v.varattno > 0 && v.varlevelsup == 0),
        NodeTag::T_Param
        | NodeTag::T_SubLink
        | NodeTag::T_SubPlan
        | NodeTag::T_AlternativeSubPlan
        | NodeTag::T_Aggref
        | NodeTag::T_WindowFunc
        | NodeTag::T_GroupingFunc
        | NodeTag::T_MergeSupportFunc
        | NodeTag::T_CoerceToDomain
        | NodeTag::T_CoerceToDomainValue
        | NodeTag::T_NextValueExpr => false,
        NodeTag::T_FuncExpr => n.as_func_expr().is_some_and(|f| !f.funcretset),
        NodeTag::T_OpExpr => n.as_op_expr().is_some_and(|o| !o.opretset),
        _ => true,
    };
    let known = gravity_decompose::expr::kids(n, &mut |c| ok = ok && set_expr(c));
    ok && known
}

/// No memo entry for the statement (point_noplan off, or S5 did not file its plan): Gravity's.
const NO_ENTRY: &str = "no memo entry";
/// A SET value the kept compile does not take (`keep::keeps`): Gravity's.
const UNKEPT: &str = "SET value not kept";
/// D4572: the slot's pieces found no room in the memo (`skip::Taken::NoRoom`): Gravity's, as with pgrust.point_keep off.
const NO_ROOM: &str = "keep: no room";

/// Hook B for an UPDATE: `None` leaves it to Gravity, unchanged. `hit`: S5 served the plan from its memo slot, or filed it
/// there, and the slot keeps the run's pieces (keep.rs); without one the UPDATE is Gravity's (`NO_ENTRY`).
pub(super) fn serve(pstmt: &PlannedStmt<'_>, dest: &mut DestReceiver<'_>, hit: Option<skip::Hit>) -> Option<PgResult<Option<QueryCompletion>>> {
    let kept = match hit.map(|h| (h, skip::take_kept(&h))) {
        Some((h, skip::Taken::Kept(kb))) => Some((h, kb)),
        Some((_, skip::Taken::NoRoom)) => return declined(NO_ROOM),
        Some((_, skip::Taken::Make)) | None => None,
    };
    let ids = kept.as_ref().and_then(|(_, kb)| keep::write_ids(kb, Cmd::Update));
    let s = match shape(pstmt, ids) {
        Verdict::Take(s) => s,
        v => {
            give_back(kept);
            return match v {
                Verdict::Declined(why) => declined(why),
                _ => None,
            };
        }
    };
    // as S3a: SERIALIZABLE (its predicate locks and conflict checks) and logical decoding's WAL are later widenings
    let refusal = super::runtime_refusal(dest.mydest())
        .or_else(|| xact::IsolationIsSerializable().then_some("serializable"))
        .or_else(|| crate::inputs::knobs::seam::logical_wal().then_some("wal_level logical"));
    if let Some(why) = refusal {
        give_back(kept);
        return declined(why);
    }
    let Some(h) = hit else {
        return declined(NO_ENTRY);
    };
    let r = match with_run(|mcx, _| match kept {
        Some((h, kb)) if ids == Some((s.relid, s.scan.indexid)) => run_kept(pstmt, &s, mcx, h, kb, None),
        _ => run_in(pstmt, &s, mcx, h),
    }) {
        Ok(Err(why)) => return declined(why),
        Ok(Ok(qc)) => Ok(qc),
        Err(e) => Err(e),
    };
    if super::reporting() {
        super::report(&match &r {
            Ok(qc) => format!("served update rows={}", qc.nprocessed), // alloc: Cold
            Err(_) => "served update error".to_string(),                // alloc: Cold
        });
    }
    Some(r.map(Some))
}

/// A slot's kept pieces back, untouched, when the statement declines before it runs.
fn give_back(kept: Option<(skip::Hit, KeptBox)>) {
    if let Some((h, kb)) = kept {
        skip::put_kept(&h, kb, None);
    }
}

#[cold]
fn declined(why: &str) -> Option<PgResult<Option<QueryCompletion>>> {
    if super::reporting() {
        super::report(&format!("declined update ({why})")); // alloc: Cold
    }
    None
}

/// The memo slot's pieces made (`make`: the relations' tests, `Err` with the reason: nothing ran, Gravity serves it) when
/// the kept compile takes every SET value (`keep::keeps`), then the run over them (`run_kept`), what their making read
/// recorded into the slot's reads.
fn run_in(pstmt: &PlannedStmt<'_>, s: &select::Shape<'_>, mcx: Mcx<'_>, h: skip::Hit) -> PgResult<Result<QueryCompletion, &'static str>> {
    let mt = pstmt.planTree.and_then(|n| n.as_modify_table()).expect("checked by shape");
    let n = set_cols(mt).expect("checked by shape").len();
    let tl = &s.scan.scan.plan.targetlist;
    if !tl.iter().take(n).all(|t| t.as_target_entry().is_some_and(|te| keep::keeps(te.expr))) {
        return Ok(Err(UNKEPT));
    }
    let (made, deps) = gravity_deps::recording(|| make(pstmt, s, mcx));
    match made? {
        Err(why) => Ok(Err(why)),
        Ok(k) => run_kept(pstmt, s, mcx, h, keep::boxed(Made::Write(k), skip::MEMO_OWNER)?, Some(&deps)),
    }
}

/// The SET columns' attnos, in the target list's order.
fn targets_of<'m>(mcx: Mcx<'m>, pstmt: &PlannedStmt<'_>) -> PgResult<PgVec<'m, i16>> {
    let mt = pstmt.planTree.and_then(|n| n.as_modify_table()).expect("checked by shape");
    let cols = set_cols(mt).expect("checked by shape");
    let mut targets: PgVec<'m, i16> = PgVec::new_in(mcx);
    targets.try_reserve_exact(cols.len()).map_err(|_| mcx.oom(cols.len() * 2))?;
    for c in cols.iter() {
        targets.push(c as i16);
    }
    Ok(targets)
}

/// The relation tests of a point UPDATE, in order: a write's (`rel::write_refusal`), an UPDATE's over its SET columns
/// (`rel::update_refusal`), the index's as S1's.
fn refusal(mcx: Mcx<'_>, s: &select::Shape<'_>, heap: &Relation<'_>, index: &Relation<'_>, targets: &[i16]) -> PgResult<Option<&'static str>> {
    if let Some(why) = rel::write_refusal(heap) {
        return Ok(Some(why));
    }
    if let Some(why) = rel::update_refusal(mcx, heap, targets)? {
        return Ok(Some(why));
    }
    let mut ops: probe::KeyOps = [(0, 0); types_core::fmgr::INDEX_MAX_KEYS as usize];
    select::relation_refusal(heap, index, s, &[], &mut ops)
}

/// The pieces a memo slot keeps for its UPDATE: the table opened as ExecInitModifyTable's result relation and the index as
/// ExecInitIndexScan opens it and their tests (`refusal`), then the walk's recipe over the old row's columns, the hosts
/// (the statement's insertedCols / updatedCols to the write host: `admit::modified_cols`, over the current statement's
/// plan) and the kept compile of the SET values (`keep::SetKept`); the relations closed again (the hosts re-open them at
/// each bind).
fn make(pstmt: &PlannedStmt<'_>, s: &select::Shape<'_>, mcx: Mcx<'_>) -> PgResult<Result<WriteKept, &'static str>> {
    let heap = table::table_open(mcx, s.relid, NoLock)?;
    let index = indexam::index_open(mcx, s.scan.indexid, select::index_lockmode(s))?;
    let made = (|| -> PgResult<Result<WriteKept, &'static str>> {
        let targets = targets_of(mcx, pstmt)?;
        if let Some(why) = refusal(mcx, s, &heap, &index, &targets)? {
            return Ok(Err(why));
        }
        let live = live_cols(mcx, &heap)?;
        let p = hosts::Probe { heap: &heap, index: &index, indexqual: &s.scan.indexqual, cols: &live };
        let recipe = p.recipe()?;
        let modified = crate::admit::modified_cols(super::super::raw::current_stmt(pstmt));
        let hosts = hosts::Hosts::new_held(heap.rd_id, Cmd::Update, modified, p.recipe()?, skip::MEMO_OWNER);
        let set = keep::SetKept::build(s.relid, &live, &targets, &s.scan.scan.plan.targetlist, skip::MEMO_OWNER)?;
        Ok(Ok(WriteKept { cmd: Cmd::Update, relid: s.relid, indexid: s.scan.indexid, recipe: core::cell::Cell::new(Some(recipe)), hosts, set: Some(set) }))
    })();
    let closed = indexam::index_close(index, NoLock).and_then(|()| table::table_close(heap, NoLock));
    let made = made?;
    closed?;
    Ok(made)
}

/// An UPDATE over the memo slot's kept pieces: ExecInitIndexScan's index lock where hook A's left none, the statement's
/// SET constants into the kept compile's parameters, then ExecutorStart's checks and the snapshot around the hosts' run,
/// UPDATE n; the pieces back into the slot when it returns, dropped when it raises. Constants that do not line up with
/// the kept compile's (never seen: S5 serves the slot's plan but for its constants) drop the pieces, and Gravity serves.
fn run_kept(pstmt: &PlannedStmt<'_>, s: &select::Shape<'_>, mcx: Mcx<'_>, hit: skip::Hit, mut kb: KeptBox, deps: Option<&gravity_deps::Deps>) -> PgResult<Result<QueryCompletion, &'static str>> {
    if super::reporting() {
        super::report(if deps.is_some() { "keep made" } else { "keep used" });
    }
    let mode = select::index_lockmode(s);
    if mode != NoLock {
        lmgr_seams::lock_relation_oid::call(s.scan.indexid, mode)?;
    }
    let n = kb.with_mut(|kin| -> PgResult<Option<u64>> {
        let Kept::Write(k) = &mut kin.kept else {
            return Err(keep::internal("a point UPDATE's memo entry keeps another statement's pieces"));
        };
        let k: &mut WriteKept = k; // the pieces' fields borrowed apart, not through the box each time
        let Some(set) = k.set.as_mut() else {
            return Err(keep::internal("a kept point UPDATE without its SET values"));
        };
        if !set.bind(&s.scan.scan.plan.targetlist) {
            return Ok(None);
        }
        exec_cmd_in(pstmt, Cmd::Update, mcx, |snap| hosts::update(&mut k.hosts, &hosts::EpqRecipe::Kept(&k.recipe), &s.scan.indexqual, set, snap)).map(Some)
    })?;
    let Some(n) = n else {
        return Ok(Err("SET constants"));
    };
    skip::put_kept(&hit, kb, deps);
    Ok(Ok(gravity_wire::completion(Cmd::Update, n)))
}

/// The table's columns the walk reads for the old row: every column not dropped, in attno order.
fn live_cols<'m>(mcx: Mcx<'m>, heap: &Relation<'_>) -> PgResult<PgVec<'m, i16>> {
    let natts = heap.rd_att.attrs.len();
    let mut v: PgVec<'m, i16> = PgVec::new_in(mcx);
    v.try_reserve_exact(natts).map_err(|_| mcx.oom(natts * 2))?;
    for (i, a) in heap.rd_att.attrs.iter().enumerate() {
        if !a.attisdropped {
            v.push(i as i16 + 1);
        }
    }
    Ok(v)
}
