//! D3507 (graphile-worker's get_job: `WITH j AS (SELECT .. LIMIT 1 FOR UPDATE SKIP LOCKED), q AS (UPDATE job_queues ..
//! FROM j ..) UPDATE jobs .. FROM j ..`): CTELOCK's claim beside the statement's UPDATE / DELETE and data-modifying
//! CTEs' UPDATE / DELETEs that each recheck in their own IR, the answers both families read (lockrows lock_init.rs
//! `write_pair`, modifytable rowop.rs `two_plans_served`).
use gravity_decompose::Op;
use mcx::{CollectIn, PgVec};

use crate::ops::lockrows::plan::Census;
use crate::graph::Gx;

/// Modifytable rowop.rs `two_plans_served`: pipeline `k`'s lock is the program's one, a CTE claim under a LIMIT /
/// OFFSET that lockrows plan.rs `Plan::write_pair` lays beside the statement's write and data-modifying CTEs' writes
/// (plan.rs `Census::cte_whole`, frame/px.rs `Gx::ctes_beside`): a Program.locks entry rechecked in its own IR (a product-form lock), so
/// it never stops for the runner's EvalPlanQual. D3601 (4): or a claim with no LIMIT / OFFSET beside data-modifying
/// CTEs' writes alone.
pub(crate) fn cte_claim_beside(gx: &Gx<'_, '_>, k: usize) -> bool {
    let ps = gx.pipelines();
    let limit_op = |o: &Op<'_>| crate::frame::iface::limit_bounds(o).is_some();
    // D3506 SETLOCK: a lock in a hashed SubPlan's plan is no claim (set_lock.rs)
    let set_locks = super::set_lock::locks(gx);
    let mut locks = ps.iter().enumerate().flat_map(|(q, p)| p.ops.iter().enumerate().filter(|(_, o)| matches!(o, Op::Lock(_))).map(move |(j, _)| (q, j))).filter(|&(q, j)| !set_locks.iter().any(|x| x.0 == q && x.1 == j));
    let (Some((q, j)), None) = (locks.next(), locks.next()) else { return false };
    let writes = |c: usize| ps[c].ops.iter().any(|o| matches!(o, Op::Write(w) if w.cmd != gravity_program::Cmd::Select));
    let ctes: PgVec<'_, usize> = (0..ps.len()).filter(|&c| gx.cte_write(c) && writes(c)).collect_in(gx.mcx);
    // D3601 (4) (River's completer): or no LIMIT / OFFSET when every write is a data-modifying CTE's (no statement
    // write)
    let ctes_only = (0..ps.len()).filter(|&c| writes(c)).all(|c| gx.cte_write(c));
    q == k && (ps[k].ops[j + 1..].iter().any(limit_op) || ctes_only) && Census(gx).cte_whole(k, limit_op) && gx.ctes_beside(&ctes[..]) && ps.inners().iter().all(|x| !x.1.ops.iter().any(|o| matches!(o, Op::Lock(_))))
}
