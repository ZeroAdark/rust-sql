//! EXPLAIN without ANALYZE, and EXPLAIN (GENERIC_PLAN), answered by the entry: nothing runs and the incumbent's
//! ExecutorStart is never called. The printer walks the Plan (commands/explain node.rs) and reads only the initial pruning
//! from the desc (instrument.rs's getters over `QueryDesc::prune`). What standard_ExecutorStart does under
//! EXEC_FLAG_EXPLAIN_ONLY is done here, in its order: no read-only guard and no AfterTriggerBeginQuery; the output
//! command id a write, a row lock or a data-modifying CTE marks used (`runner::start_gates::mark_output_cid`); InitPlan's
//! ExecCheckPermissions; ExecDoInitialPruning (none under GENERIC_PLAN, whose parameters are unbound); the relations
//! InitPlan and ExecInitNode open, with CheckValidRowMarkRel and CheckValidResultRel (`runner::held::explain_plan`); the
//! EXECUTE gates of the expressions each node's init compiles (the admitted program's list, else `runner::start_gates`)
//! and the comparison procs those inits look up (`runner::start_gates::init_lookups`).

use gravity_driver::mem::root::Root;
use types_error::PgResult;
use types_slot::EXEC_FLAG_EXPLAIN_GENERIC;

use super::{complete, keyed, raw, ExplainOnly, PlannedStmt, QueryDesc, State, VIA_EXPLAIN};
use crate::admit::reentry;

/// The relations an EXPLAIN's start opened, held to its ExecutorEnd (ExecCloseRangeTableRelations); the context first,
/// the handles last (as `run::Serve`'s).
pub(super) struct Explained {
    _ctx: Root,
    _held: crate::runner::held::Held,
}

/// EXPLAIN (GENERIC_PLAN)'s verdict: no admission binds a plan whose parameters are unbound.
const GENERIC_WHY: &str = "explain only: a generic plan whose parameters are unbound is never admitted";

/// The start of a desc under EXEC_FLAG_EXPLAIN_ONLY (`start_ours`).
pub(super) fn start(qd: &mut QueryDesc, eflags: i32) -> PgResult<()> {
    let pstmt = qd.plannedstmt();
    // a Foreign Scan's, a foreign write's or a Custom Scan's EXPLAIN lines were its provider's (ExplainForeignScan /
    // ExplainForeignModify / ExplainCustomScan): those node families are removed, so the EXPLAIN raises their 0A000
    crate::admit::removed::check(pstmt)?;
    crate::runner::start_gates::mark_output_cid(pstmt)?;
    let generic = eflags & EXEC_FLAG_EXPLAIN_GENERIC != 0;
    // D2834: a PL list that leaves a slot to evaluation time is admitted as its execution is (its program fetches the slot
    // where it evaluates it)
    let admitted = if generic {
        refused(GENERIC_WHY);
        None
    } else {
        admit(qd, eflags)?
    };
    let checked = checks(qd, pstmt, generic, admitted.as_ref().map(|(a, ..)| &**a));
    if let Some((a, home, key)) = admitted {
        keyed::release(home, key, qd.cplan, pstmt, a);
    }
    qd.explained = Some(checked?);
    qd.state = State::Explained;
    Ok(())
}

/// The admission, for its verdict only (`via=entry:explain`), as an execution's: the plan memo's take, its program filed
/// back after the checks (`keyed::release`; a parallel program or one a counted pull refuses never is: D2230, D2732,
/// D2805), its refusal remembered for the execution. `None`: refused, its line written.
#[allow(clippy::type_complexity)]
fn admit(qd: &QueryDesc, eflags: i32) -> PgResult<Option<(Box<crate::Admitted>, usize, Option<gravity_program::PlanKey>)>> {
    let (pstmt, params, cplan) = (qd.plannedstmt(), qd.params, qd.cplan);
    let _explain = ExplainOnly::mark(true);
    let completion = complete::eligible(qd.dest, eflags);
    let plan_order = crate::runner::in_plan_order();
    let mut home = 0;
    let admitted = reentry::enter(Some(reentry::built), None, false, |b, depth| -> PgResult<Result<(Box<crate::Admitted>, Option<gravity_program::PlanKey>), String>> {
        home = depth;
        if plan_order {
            return Ok(crate::admit::unfiled(|| crate::runner::admit_unkeyed(b, pstmt, params, completion, true))?.map(|a| (Box::new(a), None)).ok_or_else(crate::runner::last_refusal_line)); // alloc: Program
        }
        if let Some((key, a)) = keyed::take(b, cplan, pstmt, params, false)? {
            return Ok(Ok((a, Some(key))));
        }
        // C3 S4c: a plan without a plan key is filed nowhere: its admission records no reads (`admit::unfiled`)
        let admitted = if keyed::key_of(cplan, pstmt).is_some() { crate::runner::admit_unkeyed(b, pstmt, params, completion, true) } else { crate::admit::unfiled(|| crate::runner::admit_unkeyed(b, pstmt, params, completion, true)) };
        match admitted? {
            None => {
                crate::runner::remember_entry_refusal(b, cplan, pstmt);
                Ok(Err(crate::runner::last_refusal_line()))
            }
            Some(a) if a.is_parallel() || crate::runner::counted_refused(&a) => Ok(Ok((Box::new(a), None))), // alloc: Program
            Some(a) => Ok(Ok((Box::new(a), keyed::key_of(cplan, pstmt)))), // alloc: Program
        }
    });
    let why = match admitted {
        Ok(None) | Err(_) => return Err(crate::glue::internal("gravity_entry: no backend level for the statement")),
        Ok(Some(Err(e))) => return Err(e),
        Ok(Some(Ok(Err(why)))) => why,
        Ok(Some(Ok(Ok((a, key))))) => {
            let admit = if a.graph.is_some() { "graph" } else { "shape" };
            let _ = crate::with_backend(|b| b.note_served_via(a.shape, 0, admit, a.program.code_of(0).kind(), VIA_EXPLAIN));
            return Ok(Some((a, home, key)));
        }
    };
    refused(&why);
    Ok(None)
}

/// The verdict of an EXPLAIN whose plan has no program: a report line, never a fallthrough (nothing runs).
fn refused(why: &str) {
    let _ = crate::with_backend(|b| b.note_explain_refused(why));
}

/// ExecutorStart's checks under EXEC_FLAG_EXPLAIN_ONLY, in its order (the module comment); `a`: the admitted program,
/// whose EXECUTE gates are filed with it.
fn checks(qd: &mut QueryDesc, pstmt: &'static PlannedStmt<'static>, generic: bool, a: Option<&crate::Admitted>) -> PgResult<Box<Explained>> {
    crate::runner::permitted(pstmt)?;
    let ctx = Root::new("GravityEntryExplain");
    let mcx = raw::serve_mcx(&ctx);
    let held = crate::runner::held::take(mcx, pstmt, &crate::runner::held::explain_plan(pstmt, generic), qd.params, None)?;
    // the prune getters' answer ("Subplans Removed"): the decision the held relations were taken under
    qd.prune = if generic { None } else { crate::ops::append::prune::decided_initial(pstmt, qd.params).map(Box::new) }; // alloc: Cold
    match a {
        Some(a) => {
            crate::runner::check_gates(a, mcx)?;
            crate::runner::start_gates::init_lookups(pstmt, qd.prune.as_deref())?;
        }
        None => crate::runner::start_gates::plan_gates(pstmt, qd.prune.as_deref())?,
    }
    Ok(Box::new(Explained { _ctx: ctx, _held: held })) // alloc: Cold
}
