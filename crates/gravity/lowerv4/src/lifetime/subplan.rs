//! subplan's lifetime decisions, which the family asks here: when a hashed SubPlan's set is built again, the point where
//! a rebuilt set is emptied first, and which pipelines a correlated SubPlan's inner reads kept across its calls
//! (`kept_feeder`, D1337).
//! PG builds a hashed SubPlan's table once per SubPlanState and builds it again when a rescan changed a parameter its plan
//! reads (nodeSubplan.c `ExecHashSubPlan` -> `buildSubPlanHash` under `chgParam`, which resets the table first). Lowering
//! runs that rescan as a rerun set's run, so a set whose build pipeline a rerun set holds is rebuilt at each of its runs
//! (`rebuilt`). A set built in program order is emptied at its build's open (`reset_at_rebuild`); a LAZY one is re-armed
//! by exec at the set's re-open (exec subplan/raw.rs `rearm_sets`) and built again at its next probe.
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntCond, Type, Value};
use gravity_kernels::ops::shared::object::entries as m8;
use mcx::{CollectIn, GrowIn, PgVec};

use crate::frame::env::Env;
use crate::graph::Gx;

/// Whether a rerun set holds build pipeline `kb`, so the set is built again at each run of that set.
pub(crate) fn rebuilt(gx: &Gx<'_, '_>, kb: usize) -> bool {
    gx.graph.reruns.iter().any(|r| r.pipelines.iter().any(|&q| q as usize == kb))
}

/// A rebuilt set built in program order: its build's open empties the set its previous run sealed (`m8_subhash_reset` on
/// the set object `o`, PG's ResetTupleHashTable in `buildSubPlanHash`). The answer is true when the reset failed; the
/// caller raises the host's stashed error.
pub(crate) fn reset_at_rebuild(b: &mut FunctionBuilder<'_>, env: &Env, o: Value) -> Value {
    let f = crate::ops::subplan::hashed::host(b, "m8_subhash_reset", &[Type::Ptr, Type::Ptr], gravity_abi::host_words!(m8::m8_subhash_reset(ctx, h)));
    let r = b.call_host(f, &[env.ctx, o], Some(Type::I64)).unwrap_or(env.zero);
    b.icmp(IntCond::Slt, r, env.zero)
}

/// D1337: whether the embedded inner `ik` of pipeline `k` reads, through the slots it consumes, a pipeline outside the rerun
/// set `set` (built once, its rows kept across calls). `ctes`: a CTE's pipeline no rerun set re-runs is not such a feeder,
/// nor is anything feeding it: PG keeps the CTE's store across a CteScan's rescan too (ExecReScanCteScan re-runs the CTE's
/// plan only when a parameter it reads changed, which makes the pipeline a rerun set member), so a VOLATILE call in its
/// plan runs once per stored row in both. Nor (`ctes`) is a hash build, nor what feeds it: PG's ExecReScanHashJoin keeps a
/// one-batch table across rescans, and a kept table whose build side calls a user or VOLATILE function is hashjoin's to
/// serve or refuse (D1906, hashjoin.rs `kept_road`: served only while PG's table is certainly one batch) (gen rr-0242).
/// Nor (decision 18) is a pipeline the keep rule answers for at the set's `owner` (`lifetime::kept_input`) as kept by
/// PG's rescan too with no match flags to clear (`Pg`) or run again by it to the same rows (`Quiet`: nothing in its fill
/// acts): the rows the inner reads are PG's either way. Any other answer, or none (an embedded inner, a pipeline the set
/// runs), keeps the refusal.
pub(crate) fn kept_feeder(gx: &Gx<'_, '_>, k: usize, ik: usize, set: &[u16], ctes: bool, owner: Option<(i32, i32)>) -> bool {
    let ps = gx.pipelines();
    let mut todo: PgVec<'_, (usize, u8)> = ps[ik].desc.consumes.iter().map(|&s| (k, s)).collect_in(gx.mcx);
    let mut seen: PgVec<'_, usize> = PgVec::new_in(gx.mcx);
    while let Some((c, s)) = todo.pop() {
        let Some(p) = (0..c).rev().find(|&p| ps[p].desc.produces() == Some(s)) else { continue };
        if seen.contains(&p) {
            continue;
        }
        seen.push_in(p);
        if ctes && gx.cte_write(p) && !gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(p as u16))) {
            continue;
        }
        if ctes && !set.contains(&(p as u16)) && matches!(ps[p].ops.last(), Some(gravity_decompose::Op::Join(gravity_decompose::join::JoinDesc::Build { .. }))) {
            continue;
        }
        if !set.contains(&(p as u16)) {
            use gravity_decompose::lifetime::KeptBy;
            if owner.is_some_and(|(node, plan)| matches!(super::kept_input(gx.graph, node, plan, p), Some(KeptBy::Pg { marks: false } | KeptBy::Quiet))) {
                continue;
            }
            return true;
        }
        todo.extend_in(ps[p].desc.consumes.iter().map(|&s| (p, s)));
    }
    false
}
