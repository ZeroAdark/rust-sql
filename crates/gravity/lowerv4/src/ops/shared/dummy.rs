//! The planner's dummy relation (D717): a Result (`Source::Const`) under a one-time gate that is constant false or NULL
//! (every leaf pruned, a constant-false qual). No row ever comes, so its target list (the scan Vars a dummy rel keeps,
//! system columns too) is never evaluated and no row reaches a write or a row lock above it: the one test step/expr.rs
//! and the write and row-lock kinds admit a Const source's scan rows by.
use gravity_decompose::{Op, Source};
use types_nodes::Node;

use crate::graph::Gx;

/// Pipeline `k` is the planner's dummy relation: a Const source with a gate among `ops[..upto]` that `never` passes.
pub(crate) fn dummy(gx: &Gx<'_, '_>, k: usize, upto: usize) -> bool {
    gx.pipelines().get(k).is_some_and(|p| matches!(p.source, Source::Const) && p.ops[..upto.min(p.ops.len())].iter().any(|o| matches!(o, Op::Gate(g) if never(*g))))
}

/// A one-time gate constant false or NULL (an implicit-AND list holding one such constant): `ExecQual` over it passes no row.
pub(crate) fn never(n: Node<'_>) -> bool {
    crate::stack_check();
    if let Some(c) = n.as_const() {
        return c.constisnull || !c.constvalue.as_bool();
    }
    n.as_list().is_some_and(|l| l.iter().any(never))
}

/// D829: an UPDATE / DELETE of a partitioned root that every leaf was pruned from (a WHERE false, a key no partition
/// holds, RI's nested action for such a key): the planner makes the root itself the one result relation over its dummy
/// Result (ExecInitModifyTable opens it; CheckValidResultRel admits a partitioned table), so the write has no storage to
/// reach and no row reaches it. The kinds admit it by this test instead of the target's AM, and plan it under the root.
pub(crate) fn pruned_root(gx: &Gx<'_, '_>, k: usize, w: &gravity_decompose::ops::shared::modifytable::WriteDesc<'_>) -> bool {
    use gravity_decompose::ops::shared::modifytable::{Member, TargetKind};
    let written = gx.pipelines().get(k).map_or(0, |p| p.ops.iter().position(|o| matches!(o, Op::Write(_))).unwrap_or(p.ops.len()));
    w.targets.len() == 1
        && w.targets[0].kind == TargetKind::Partitioned
        && w.targets[0].relid == w.plan.target_rel
        && w.members.has(Member::Route)
        && w.route.as_ref().is_some_and(|r| r.root_relid == w.plan.target_rel)
        && dummy(gx, k, written)
}
