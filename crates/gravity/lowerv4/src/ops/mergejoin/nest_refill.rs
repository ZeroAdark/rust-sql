//! merge.join as a nest loop's embedded inner over a run the nest's own rerun set refills (regress rangefuncs#223, D1109 /
//! D1018). PG 18.6 rescans the merge at every outer row (ExecReScanMergeJoin: both children rescanned); an inner Material
//! whose child reads the nest's parameter re-reads it (ExecReScanMaterial with chgParam), so the merge's inner rows are
//! those of the outer row's parameter. Here every pipeline filling the merge's run is a member of the inner's own set
//! (rerun/sets.rs `owner_set`: the merge reads a slot of the set), which the re-open runs before the inner opens, and the
//! merge reads the run from its first record with its machine words re-opened, as nest_inner.rs's run filled once ahead.
//! The fill runs whole before the re-open's first row; PG runs it as far as the merge fetches, so the fill may only raise
//! where PG surely fetches the whole inner at every re-open (`refill_raises`): a join that fills its inner (RIGHT / FULL /
//! RIGHT ANTI: ExecMergeJoin's ENDOUTER reads the inner to its end, an empty outer included), nothing after the merge that
//! stops it, a nest that takes every inner row (an INNER or LEFT nest, not inner-unique), its rows reaching the client
//! whole (lazy_outer.rs `reached`; a cursor delegates: `Px::whole_read`), and the fill reading no other pipeline's rows
//! and calling no user or VOLATILE function (the effects home's readers) nor sorting by a user comparator. A helper file
//! (ROWS = NONE). Owner mergejoin.
use gravity_decompose::join::JoinDesc;
use gravity_decompose::{Op, Source};
use types_nodes::jointype::JoinType;
use types_nodes::Node;

use crate::graph::Gx;

/// Op `i` of embedded inner `ik` is a merge join whose run's fillers are all members of the inner's own rerun set:
/// `Some(quiet)` (markable.rs `inner_may_act`), else `None`.
fn refilled(gx: &Gx<'_, '_>, ik: usize, i: usize) -> Option<bool> {
    let Some(Op::Join(JoinDesc::Merge { slot, .. })) = gx.pipelines()[ik].ops.get(i) else { return None };
    let fillers = crate::ops::mergejoin::lazy_outer::fillers(gx, ik, *slot);
    let set = &gx.graph.reruns[crate::rerun::sets::owner_set_at(gx, ik)?];
    if fillers.is_empty() || !fillers.iter().all(|&x| set.pipelines.contains(&(x as u16))) {
        return None;
    }
    Some(fillers.iter().all(|&x| !crate::effects::visible::merge::inner_may_act(gx, x, usize::MAX, None)))
}

/// Fill pipeline `x` reads no other pipeline's rows (a scan, a function scan or VALUES, its ops filters, projections, set
/// functions and the run's Sort) and calls no user or VOLATILE function nor sorts by a user comparator: what it raises is
/// its own rows'.
fn plain_fill(gx: &Gx<'_, '_>, x: usize) -> bool {
    let p = &gx.pipelines()[x];
    let calls = |n: Node<'_>| crate::effects::user_or_volatile(gx, n);
    let src = matches!(p.source, Source::Range { .. } | Source::Const | Source::Values { .. });
    let ops = p.ops.iter().all(|op| match op {
        Op::Filter(n) => !calls(*n),
        Op::Project(l) => !l.iter().any(calls),
        Op::Srf(d) => !d.functions.iter().any(|f| calls(f.node)),
        Op::Join(JoinDesc::Spool) | Op::Sort(_) => true,
        _ => false,
    });
    p.desc.consumes.is_empty() && src && ops && !crate::effects::visible::merge::user_sort(gx, p)
}

/// Whether op `i` of embedded inner `ik` is a merge join the inner serves over a run its own set refills (the module's rule).
pub(crate) fn ok(gx: &Gx<'_, '_>, ik: usize, i: usize) -> bool {
    match refilled(gx, ik, i) {
        Some(quiet) => quiet || refill_raises(gx, ik, i).is_some(),
        None => false,
    }
}

/// A refilled run's fill that may raise (D1236's case, whose recheck licence a rerun set never takes): the graph pipeline
/// whose nest embeds inner `ik` when PG surely fetches the merge's whole inner at every re-open (the module's rule).
pub(crate) fn refill_raises(gx: &Gx<'_, '_>, ik: usize, i: usize) -> Option<usize> {
    let ps = gx.pipelines();
    let ((k, j), ip) = *ps.inners().get(ik.checked_sub(ps.len())?)?;
    let Some(Op::Join(JoinDesc::Merge { jointype, slot, .. })) = ip.ops.get(i) else { return None };
    let after = ip.ops[i + 1..].iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_)));
    let nest = matches!(ps[k].ops.get(j), Some(Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER | JoinType::JOIN_LEFT, unique: false, .. })));
    let whole = k < ps.len() && crate::ops::mergejoin::lazy_outer::reached(gx, k, j + 1);
    let plain = crate::ops::mergejoin::lazy_outer::fillers(gx, ik, *slot).iter().all(|&x| plain_fill(gx, x));
    (refilled(gx, ik, i).is_some() && plain && crate::ops::mergejoin::fill_inner::fills_inner(*jointype) && after && nest && whole).then_some(k)
}
