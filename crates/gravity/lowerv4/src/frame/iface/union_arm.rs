//! A streamed union's later arm under a hash join that its empty table ends, read through the interface home: the hash
//! probe (ops/hashjoin) closes such an arm's call, and the recursive union (ops/setop) asks whether every later arm
//! reading its store does.
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::append::AppendDesc;
use gravity_decompose::ops::shared::setop::SetDesc;
use gravity_decompose::{Op, Source};
use types_nodes::jointype::JoinType;

use crate::graph::Gx;

/// D3373: op `j` of pipeline `q` is an INNER or SEMI hash probe (not the catalog form) that a streamed union's later arm
/// carries (an `AppendDesc::Arm` marker with `i > 0` before it, decompose append.rs `push_union`), in a pipeline no rerun
/// set holds, whose source is the store of a recursive union decompose marked `IterDesc::arms_later` (only that form; no
/// other Append's arm changes). PostgreSQL's Append starts that arm only after the earlier ones ended, and a join whose
/// table is empty ends before it, so the probe closes the arm's call at the open over a built EMPTY table, before its
/// source opens.
pub(crate) fn later_arm_probe(gx: &Gx<'_, '_>, q: usize, j: usize) -> bool {
    let p = &gx.pipelines()[q];
    let Some(Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_INNER | JoinType::JOIN_SEMI, catalog: false, .. })) = p.ops.get(j) else { return false };
    p.ops[..j].iter().any(|op| matches!(op, Op::Append(AppendDesc::Arm { i, .. }) if *i > 0)) && !gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(q as u16))) && reads_arms_later(gx, q)
}

/// Pipeline `q`'s source is the store of a recursive union whose `IterDesc::arms_later` holds (its pipeline's Iter op).
fn reads_arms_later(gx: &Gx<'_, '_>, q: usize) -> bool {
    let Source::Breaker { slot } = gx.pipelines()[q].source else { return false };
    gx.life_of(q, slot).is_some_and(|l| gx.pipelines()[l.0 as usize].ops.iter().any(|op| matches!(op, Op::SetOp(SetDesc::Iter(d)) if d.arms_later)))
}

/// D3373: whether pipeline `q` carries a later arm's probe that closes (`later_arm_probe`).
pub(crate) fn later_arm_closes(gx: &Gx<'_, '_>, q: usize) -> bool {
    (0..gx.pipelines()[q].ops.len()).any(|j| later_arm_probe(gx, q, j))
}

/// The arm index of a streamed union's arm pipeline `q` (its `AppendDesc::Arm` marker), if it is one.
pub(crate) fn arm_index(gx: &Gx<'_, '_>, q: usize) -> Option<u16> {
    gx.pipelines()[q].ops.iter().find_map(|op| if let Op::Append(AppendDesc::Arm { i, .. }) = op { Some(*i) } else { None })
}
