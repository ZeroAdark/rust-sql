//! A CTE's plan read through the interface home: the InitPlan family (ops/subplan/initplan) asks whether a member it
//! does not see re-run lies in a CTE's plan that a rerun set holds nothing of (D1262).
use crate::graph::Gx;

/// D1262: pipeline `q` lies in a CTE's plan (the CTE's pipeline and every pipeline feeding it, ops/ctescan `fed`) of
/// which the rerun set `held` holds no pipeline. The CTE's readers pull that plan (a pulled set is never an on-demand
/// member: gravity_program protos.rs `producers_in`), and its store is kept for the statement, so a re-arm of the set
/// never runs it again, as PG's ExecReScanCteScan only rewinds a CTE that no changed parameter reaches.
pub(crate) fn in_cte_plan_apart(gx: &Gx<'_, '_>, q: usize, held: &[u16]) -> bool {
    (0..gx.pipelines().len()).filter(|&c| gx.cte_write(c)).any(|c| {
        let plan = crate::ops::ctescan::lower::fed(gx, c);
        plan.contains(&q) && !plan.iter().any(|&x| held.contains(&(x as u16)))
    })
}
