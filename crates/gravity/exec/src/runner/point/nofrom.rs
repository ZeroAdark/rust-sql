//! S0: a SELECT without FROM. Hook A (`plan_skip`, tcop's exec_simple_query) builds the plan the planner builds for it: a
//! childless Result whose target list is folded as the planner folds it, at the planner's point. Hook B (`serve`,
//! `simple_at`) runs a childless Result in ExecutorStart / ExecutorRun's order: every entry initialized, the
//! RowDescription, the entries evaluated in order by execexpr, the DataRow.
use gravity_program::{Cmd, OutCol};
use mcx::{Mcx, PgVec};
use tcop_dest::DestReceiver;
use types_dest::CommandDest;
use types_error::PgResult;
use types_nodes::list::NodeList;
use types_nodes::nodes_enums::CmdType;
use types_nodes::parsenodes::{Query, RTEKind, RangeTblEntry};
use types_nodes::plannodes::{PlannedStmt, Result as ResultPlan};
use types_nodes::primnodes::{Alias, TargetEntry};
use types_nodes::Node;
use types_portal::QueryCompletion;

use super::out::{cstring, out_col};
use super::run::{exec_in, with_run};
use super::{env_once, report, reporting, runtime_refusal, strict_error, BUILT, PLANCHECK};
use crate::inputs::knobs::seam::{point_path, POINT_PATH_OFF, POINT_PATH_STRICT};

/// The first feature of a SELECT without FROM that keeps it on the planner (PL/pgSQL's simple-expression rules without
/// the one-column limit); `None` when it is one hook A builds.
fn nofrom_refusal(q: &Query<'_>) -> Option<&'static str> {
    let jt = q.jointree.map_or((true, true), |j| (j.fromlist.is_nil(), j.quals.is_none()));
    let checks: [(bool, &'static str); 19] = [
        (q.utilityStmt.is_some() || !q.canSetTag, "utility or rule"),
        (!jt.1, "where"),
        (q.hasAggs || !q.groupClause.is_nil() || !q.groupingSets.is_nil() || q.havingQual.is_some(), "aggregate or group"),
        (q.hasWindowFuncs || !q.windowClause.is_nil(), "window"),
        (q.hasTargetSRFs, "srf"),
        (q.hasSubLinks, "sublink"),
        (!q.cteList.is_nil() || q.hasModifyingCTE, "cte"),
        (!q.distinctClause.is_nil(), "distinct"),
        (!q.sortClause.is_nil(), "order by"),
        (q.limitOffset.is_some() || q.limitCount.is_some(), "limit"),
        (q.setOperations.is_some(), "set operation"),
        (q.hasForUpdate || !q.rowMarks.is_nil(), "row marks"),
        (q.hasRowSecurity || !q.withCheckOptions.is_nil(), "row security"),
        (!q.returningList.is_nil() || q.onConflict.is_some() || !q.mergeActionList.is_nil(), "write clause"),
        (q.isReturn, "return"),
        (q.targetList.iter().any(|t| t.as_target_entry().is_none_or(|te| te.resjunk)), "junk entry"),
        (q.targetList.iter().any(|t| t.as_target_entry().is_some_and(|te| te.resname.is_some_and(|n| n.len() >= gravity_program::program::NAME_LEN))), "long name"),
        (q.targetList.is_nil(), "no columns"),
        (q.targetList.iter().any(|t| t.as_target_entry().is_some_and(cstring)), "cstring column"),
    ];
    // the walk last, over a target list the checks above passed (no aggregate, window, SRF or sublink)
    checks.iter().find(|(hit, _)| *hit).map(|(_, why)| *why).or_else(|| runs_sql(&q.targetList).then_some(RUNS_SQL))
}

/// D4547: a target list that may run SQL keeps its statement on the planner and Gravity, so a statement its code runs is
/// nested in Gravity's run (a served statement's level), as with the point path off. An entry may when its code may run
/// user code (decompose `effects::code_may_write`, read with no catalog facts so an initdb SQL function counts too: a
/// user's function, operator, type I/O or domain CHECK, a builtin that runs code named at run time or a query of its own,
/// a function the builtin table lacks, a node the walk cannot see), when its type's output is not a builtin's
/// (`effects::builtin_type_io`: an extension's or a user's type, a domain), or when it calls a builtin that runs a
/// statement (`runs_statements`).
pub(super) const RUNS_SQL: &str = "function may run SQL";

pub(super) fn runs_sql(tlist: &NodeList<'_>) -> bool {
    use gravity_decompose::{catalog::expr_type::type_of, effects};
    tlist.iter().any(|t| {
        t.as_target_entry().is_none_or(|te| {
            type_of(te.expr).and_then(effects::builtin_type_io).is_none() || effects::code_may_write(&gravity_decompose::NoFacts, te.expr) || runs_statements(te.expr)
        })
    })
}

/// `n` or a node under it (decompose `expr::kids`) calls a builtin that runs a statement of its own though the effects home
/// reads no user code in it: a LANGUAGE sql function (`effects::builtin_sql_body`: functions.c runs its body unless the
/// planner inlines it) or one that queries the catalogs through SPI (`effects::runs_catalog_query`: schema_ /
/// database_to_xmlschema, pg_get_viewdef / pg_get_ruledef); a node the walk cannot see counts.
fn runs_statements(n: Node<'_>) -> bool {
    stack_depth_core::check_stack_depth_or_panic();
    let call = |f: u32| gravity_decompose::effects::builtin_sql_body(f) || gravity_decompose::effects::runs_catalog_query(f);
    let mut any = n.as_func_expr().is_some_and(|x| call(x.funcid)) || n.as_op_expr().is_some_and(|o| call(o.opfuncid));
    let known = gravity_decompose::expr::kids(n, &mut |c| any = any || runs_statements(c));
    any || !known
}

fn nofrom(q: &Query<'_>) -> bool {
    q.commandType == CmdType::CMD_SELECT && q.rtable.is_nil() && q.jointree.is_none_or(|j| j.fromlist.is_nil())
}

/// Hook A: the statement's plan built here, or its queries handed back to the planner.
pub fn plan_skip<'mcx>(mcx: Mcx<'mcx>, querytrees: PgVec<'mcx, Query<'mcx>>, dest: CommandDest) -> PgResult<Result<PgVec<'mcx, PlannedStmt<'mcx>>, PgVec<'mcx, Query<'mcx>>>> {
    BUILT.with(|b| b.set(false));
    if point_path() == POINT_PATH_OFF || querytrees.len() != 1 || !nofrom(&querytrees[0]) {
        return Ok(Err(querytrees));
    }
    let observed = || crate::inputs::knobs::seam::planning_observed().then_some("planning observed");
    let why = nofrom_refusal(&querytrees[0]).or_else(|| runtime_refusal(dest)).or_else(observed);
    if let Some(why) = why {
        if reporting() {
            report(&format!("declined nofrom ({why})")); // alloc: Cold
        }
        return Ok(Err(querytrees));
    }
    let stmt = result_plan(mcx, &querytrees[0])?;
    if PLANCHECK.with(|c| env_once(c, "PGRUST_GRAVITY_POINT_PLANCHECK")) {
        plancheck(mcx, querytrees, &stmt)?;
    }
    let mut out: PgVec<'mcx, PlannedStmt<'mcx>> = PgVec::new_in(mcx);
    out.try_reserve_exact(1).map_err(|_| mcx.oom(core::mem::size_of::<PlannedStmt<'_>>()))?;
    out.push(stmt);
    BUILT.with(|b| b.set(true));
    Ok(Ok(out))
}

/// The planner's plan of a SELECT without FROM: subquery_planner's replace_empty_jointree RTE_RESULT, preprocess_expression's
/// eval_const_expressions over the target list (entry by entry, in order), setrefs' opfuncids, a Result over no child.
fn result_plan<'mcx>(mcx: Mcx<'mcx>, q: &Query<'mcx>) -> PgResult<PlannedStmt<'mcx>> {
    let mut tlist = NodeList::nil();
    for t in q.targetList.iter() {
        let te = t.as_target_entry().expect("checked by nofrom_refusal");
        let expr = clauses::fold::eval_const_expressions(mcx, te.expr)?;
        nodes_core::fix_opfuncids(expr)?;
        tlist.lappend(mcx, Node::mk(mcx, TargetEntry { expr, ..*te })?)?;
    }
    let mut res = Node::build::<ResultPlan<'mcx>>(mcx)?;
    res.plan.targetlist = tlist;
    res.plan.plan_rows = 1.0;
    res.plan.total_cost = 0.01;
    let eref = mcx::alloc_leak_in(mcx, Alias { aliasname: Some("*RESULT*"), colnames: NodeList::nil() })?;
    let mut rte = Node::build::<RangeTblEntry<'mcx>>(mcx)?;
    rte.rtekind = RTEKind::RTE_RESULT;
    rte.eref = Some(eref);
    Ok(PlannedStmt {
        commandType: CmdType::CMD_SELECT,
        queryId: types_nodes::SyncCell::new(q.queryId),
        canSetTag: q.canSetTag,
        planTree: Some(res.seal()),
        rtable: NodeList::make1(mcx, rte.seal())?,
        stmt_location: q.stmt_location,
        stmt_len: q.stmt_len,
        ..PlannedStmt::default()
    })
}

/// PGRUST_GRAVITY_POINT_PLANCHECK (proof runs): the planner plans the statement too and its plan is compared with hook A's;
/// a difference is one LOG line. The planner's folding runs a second time (an immutable function's NOTICE shows twice).
#[cold]
fn plancheck<'mcx>(mcx: Mcx<'mcx>, querytrees: PgVec<'mcx, Query<'mcx>>, ours: &PlannedStmt<'mcx>) -> PgResult<()> {
    let qs: &'mcx mut [Query<'mcx>] = querytrees.leak();
    let q0: &'mcx mut Query<'mcx> = &mut qs[0];
    let theirs = planner_seams::planner::call(mcx, q0, "", types_nodes::parsenodes::CURSOR_OPT_PARALLEL_OK, types_portal::ParamListHandle::NULL)?;
    let tl = |p: &PlannedStmt<'mcx>| p.planTree.and_then(|n| n.as_result()).filter(|r| r.plan.lefttree.is_none() && r.resconstantqual.is_none() && r.plan.initPlan.is_nil()).map(|r| &r.plan.targetlist);
    let why = match (tl(ours), tl(&theirs)) {
        (_, None) => Some("the planner's plan is not a childless Result"),
        (Some(a), Some(b)) if a.len() != b.len() || a.iter().zip(b.iter()).any(|(x, y)| !types_nodes::equal::equal(x, y)) => Some("target lists differ"),
        _ if ours.rtable.len() != theirs.rtable.len() || !theirs.subplans.is_nil() || !theirs.paramExecTypes.is_nil() => Some("range table or subplans differ"),
        _ => None,
    };
    if let Some(why) = why {
        report(&format!("plancheck mismatch ({why})")); // alloc: Cold
    }
    Ok(())
}

/// The target list of a plan hook B serves: a SELECT whose plan is a childless Result over at most an RTE_RESULT.
pub(super) fn bare_result<'p>(p: &PlannedStmt<'p>) -> Option<&'p NodeList<'p>> {
    let plain = p.commandType == CmdType::CMD_SELECT && p.utilityStmt.is_none() && p.canSetTag && !p.hasReturning && !p.hasModifyingCTE;
    let empty = p.rowMarks.is_nil() && p.subplans.is_nil() && p.paramExecTypes.is_nil() && p.resultRelations.is_nil();
    let rtable = p.rtable.len() <= 1 && p.rtable.iter().all(|r| r.as_range_tbl_entry().is_some_and(|e| e.rtekind == RTEKind::RTE_RESULT));
    if !(plain && empty && rtable) {
        return None;
    }
    let r = p.planTree?.as_result()?;
    let childless = r.plan.lefttree.is_none() && r.plan.righttree.is_none() && r.resconstantqual.is_none() && r.plan.initPlan.is_nil() && r.plan.qual.is_nil();
    let entries = !r.plan.targetlist.is_nil()
        && r.plan.targetlist.iter().all(|t| t.as_target_entry().is_some_and(|te| !te.resjunk && !cstring(te) && te.resname.is_none_or(|n| n.len() < gravity_program::program::NAME_LEN)));
    (childless && entries).then_some(&r.plan.targetlist)
}

/// Hook B for a FROM-less plan (`built`: hook A built it; `mode`: the session's point_path, not off): `None` leaves the
/// statement to Gravity, unchanged.
pub(super) fn serve(pstmt: &PlannedStmt<'_>, dest: &mut DestReceiver<'_>, built: bool, mode: i32) -> Option<PgResult<Option<QueryCompletion>>> {
    let tlist = bare_result(pstmt);
    let why = match tlist {
        None => Some("plan"),
        // hook A's own plan passed `nofrom_refusal` (its query's functions, before folding)
        Some(tl) if !built && runs_sql(tl) => Some(RUNS_SQL),
        Some(_) => runtime_refusal(dest.mydest()),
    };
    if let Some(why) = why {
        if built && mode == POINT_PATH_STRICT {
            return Some(Err(strict_error(why)));
        }
        if built && reporting() {
            report(&format!("declined nofrom ({why})")); // alloc: Cold
        }
        return None;
    }
    let r = run(pstmt, tlist.expect("checked above"));
    if reporting() {
        report(match (built, r.is_ok()) {
            (true, true) => "served nofrom rows=1",
            (true, false) => "served nofrom error",
            (false, true) => "served planned result rows=1",
            (false, false) => "served planned result error",
        });
    }
    Some(r.map(Some))
}

/// The FROM-less run (`run::with_run`, `run::exec_in`): the projection of a childless Result.
fn run(pstmt: &PlannedStmt<'_>, tlist: &NodeList<'_>) -> PgResult<QueryCompletion> {
    with_run(|mcx, wire| exec_in(pstmt, mcx, |_| project(tlist, mcx, wire)))
}

/// ExecInitResult's projection (every entry initialized, an EXECUTE privilege error before any message), then
/// ExecutorRun's: the RowDescription, the entries in order, their output.
fn project<'p: 'm, 'm>(tlist: &NodeList<'p>, mcx: Mcx<'m>, wire: &mut gravity_wire::WireNode) -> PgResult<QueryCompletion> {
    let n = tlist.len();
    let mut cols: PgVec<'m, OutCol> = PgVec::new_in(mcx);
    let mut states: PgVec<'m, mcx::PgBox<'m, execexpr::ExprState<'m>>> = PgVec::new_in(mcx);
    cols.try_reserve_exact(n).map_err(|_| mcx.oom(n * core::mem::size_of::<OutCol>()))?;
    states.try_reserve_exact(n).map_err(|_| mcx.oom(n * core::mem::size_of::<usize>()))?;
    for t in tlist.iter() {
        let te = t.as_target_entry().expect("checked by bare_result");
        cols.push(out_col(te)?);
        states.push(execexpr::exec_init_expr(mcx, Some(te.expr), execexpr::ParamBind::NONE)?.expect("a target entry has an expression"));
    }
    wire.prepare_cols(&cols, None)?;
    wire.reset();
    wire.send_rowdesc()?;
    // ExecResult's CHECK_FOR_INTERRUPTS at its first call (a cancel, statement_timeout or terminate pending)
    cancelpoll::interrupts::check()?;
    let mut values: PgVec<'m, u64> = PgVec::new_in(mcx);
    let mut isnull: PgVec<'m, u8> = PgVec::new_in(mcx);
    values.try_reserve_exact(n).map_err(|_| mcx.oom(n * 8))?;
    isnull.try_reserve_exact(n).map_err(|_| mcx.oom(n))?;
    let mut slots = execexpr::EvalSlots { scan: None, inner: None, outer: None };
    for st in states.iter_mut() {
        st.arm_result_mcx(mcx);
        let d = execexpr::exec_eval_expr(st, &mut slots)?;
        values.push(d.value.as_u64());
        isnull.push(u8::from(d.isnull));
    }
    // the DataRow in the run context, reset at the run's end: a wide value is not kept in the node's buf (send_row)
    wire.send_row_in(&values, &isnull, mcx)?;
    // D4639: ExecutePlan calls ExecResult again after the row, and its CHECK_FOR_INTERRUPTS comes first: an interrupt the
    // entries raised (pg_terminate_backend(pg_backend_pid()): its own SIGTERM) fires there, before CommandComplete
    cancelpoll::interrupts::check()?;
    Ok(gravity_wire::completion(Cmd::Select, 1))
}
