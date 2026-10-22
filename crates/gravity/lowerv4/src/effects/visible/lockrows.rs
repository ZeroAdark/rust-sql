//! The effects home's visible half for the lockrows family's IR rechecks over REFERENCE rows (ops/lockrows/lower.rs):
//! may the user see that a REFERENCE side's part reruns over its refetched row? Its calls (`calm_call`) and a hash
//! build's or merge inner's part (`quiet_ref_build`).
use gravity_decompose::{Op, PipelineNode, Source};
use types_nodes::Node;

use crate::graph::Gx;

/// REFMERGE / D3379 REFCALM (ops/lockrows/lower.rs): expression `n` calls only builtin, non-VOLATILE functions over
/// builtin types. Redone over a REFERENCE row the recheck refetches at its junk ctid (SnapshotAny: the very version the
/// join read), such calls give what they gave there, so a qual that passed passes again and nothing raises that did not
/// raise then, whichever of PostgreSQL's outer-first or build-first orders its EvalPlanQual takes.
pub(crate) fn calm_call(gx: &Gx<'_, '_>, n: Node<'_>) -> bool {
    !gx.fx(n).user_calls() && !gx.fx(n).volatile_any() && !gx.fx(n).user_typed()
}

/// REFFETCH's build test (lower.rs `ref_limit`; D3567 REFCHAIN, `ref_chain`): whether a REFERENCE build's part, build
/// pipeline `pb`'s ops `below` its build op (or a merge inner's whole pipeline), reruns quietly over the refetched row:
/// filters, gates and projections that cannot raise (nor its source's recheck quals), or whose calls are `calm_call`
/// ones.
pub(crate) fn quiet_ref_build(gx: &Gx<'_, '_>, pb: &PipelineNode<'_>, below: &[Op<'_>]) -> bool {
    let plain = |o: &Op<'_>| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_));
    let recheck_free = match &pb.source {
        Source::Index { recheck, .. } | Source::Bitmap { recheck, .. } => recheck.iter().all(|n| gx.fx(n).quiet()),
        _ => true,
    };
    // REFMERGE: the merge inner's ops rerun over its refetched row, the very version the merge read (SnapshotAny at the junk
    // ctid), so they pass again as they passed: builtin, non-VOLATILE calls over builtin types (lock-0009's numeric amt = 99,
    // not leakproof) can neither fail nor raise there, and PostgreSQL's EvalPlanQual runs the same calls over the same row.
    // D3379 REFCALM: a hash build's ops alike (the build row refetched the same way)
    let calm = |n: Node<'_>| calm_call(gx, n);
    let same = |o: &Op<'_>| match o {
        Op::Filter(n) | Op::Gate(n) => calm(*n),
        Op::Project(l) => l.iter().all(calm),
        _ => false,
    };
    let recheck_calm = match &pb.source {
        Source::Index { recheck, .. } | Source::Bitmap { recheck, .. } => recheck.iter().all(calm),
        _ => true,
    };
    // AGA-35: a counted-out InitPlan's parameter reads as a bound word in the build part's ops (`lock_init::init_param`:
    // the recheck copies its value; make() asks `admit_ir_at` that the build part reads it unconditionally)
    let bound = |id: i32| crate::ops::lockrows::lock_init::init_param(gx, id);
    let rf = |n: Node<'_>| gx.fx(n).quiet_by(&bound, false);
    let op_rf = |o: &Op<'_>| match o {
        Op::Filter(n) | Op::Gate(n) => rf(*n),
        Op::Project(l) => l.iter().all(rf),
        _ => false,
    };
    below.iter().all(plain) && (below.iter().all(op_rf) && recheck_free || below.iter().all(same) && recheck_calm)
}
