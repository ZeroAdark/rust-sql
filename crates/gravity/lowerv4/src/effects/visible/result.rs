//! The result family's visible decisions, in the effects home (lint rule RV; the EFFECTS-DEBT row of ops/result/gate.rs
//! `quiet`, moved unchanged): whether running a pipeline whole shows nothing (gate.rs `Reach::of`, the feeder check's
//! masks).
use gravity_abi::breaker::BreakerKind;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::Op;

use crate::graph::Gx;

/// Whether running pipeline `k` whole shows nothing: its source and ops raise and call nothing (join_build_class::own_side,
/// which leaves a trailing Build to its probe) and neither does its END, which runs per input row: a spool; a hash build
/// whose keys are raise-free; a plain or hashed aggregate or distinct whose every aggregate is (join/rtfilter.rs
/// `raise_free`: its class, arguments, ORDER BY / DISTINCT and FILTER). Any other end (a sort, a top-k) is not quiet.
/// `end`: the kind of `k`'s breaker end, None for a sink (ops/result/gate.rs `breaker`).
pub(crate) fn quiet(gx: &Gx<'_, '_>, k: usize, end: Option<BreakerKind>) -> bool {
    let (p, rf) = (&gx.pipelines()[k], |n| gx.fx(n).quiet());
    let end = match end {
        Some(b) if b == BreakerKind::Materialize => true,
        Some(b) if b == BreakerKind::HashBuild => matches!(p.ops.last(), Some(Op::Join(JoinDesc::Build { keys, .. })) if keys.iter().all(rf)),
        Some(b) if matches!(b, BreakerKind::PlainAgg | BreakerKind::HashAgg | BreakerKind::Distinct) => gx.group_at(k).is_some_and(|g| g.aggs.iter().all(|a| rf(a.node) && a.filter.is_none_or(rf))),
        _ => false,
    };
    end && crate::ops::hashjoin::build_class::own_side(gx, k) == crate::ops::hashjoin::build_class::Side::Safe
}

/// D2571 (ops/shared/limit.rs, ops/result/gate.rs): whether evaluating gate qual `n` (a list: each member) once more would
/// show: it calls a user function or a VOLATILE one in whole scope (`subplan::calls_whole`, D4138: a SubPlan's plan,
/// test and args too), or holds a row-reading xml builtin or a node the walk cannot see (`Fx::user_typed`).
pub(crate) fn qual_calls(gx: &Gx<'_, '_>, n: types_nodes::Node<'_>) -> bool {
    let one = |c: types_nodes::Node<'_>| super::subplan::calls_whole(gx, c) || gx.fx(c).user_typed();
    n.as_list().map_or_else(|| one(n), |l| l.iter().any(one))
}
