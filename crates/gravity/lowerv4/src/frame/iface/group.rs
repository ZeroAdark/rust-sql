//! A grouped DISTINCT group's key order, read through the interface home: whether the plan reads the order
//! the group's owner emits in, so the emit sorts its groups by key (table/emit_group.rs). It reads the draining
//! pipeline's ops and end, the lowered output a family must not read raw.
use gravity_decompose::ops::shared::agg::{GroupDecl, GroupForm};
use gravity_decompose::{Op, Source};
use gravity_program::End;

use crate::graph::Gx;

/// D530: whether the plan reads grouped DISTINCT group `g`'s key order (a sorted GroupAggregate's `ORDER_KEY`): its
/// breaker declares it and the draining pipeline reaches no full sort before an op that reads order, nor ends in a
/// plain or hashed aggregate. The owner emits groups in partition order, so the emit (emit_group.rs) sorts them by key.
pub fn order_read(gx: &Gx<'_, '_>, g: &GroupDecl<'_>) -> bool {
    let end = g.producer.0 as usize;
    if g.form != GroupForm::Distinct || !crate::graph::grouped(g) || !gx.graph.breakers.iter().any(|d| d.producer as usize == end && d.breaker.slot == g.slot && d.order.is_known()) {
        return false;
    }
    let Some(q) = (end + 1..gx.pipelines().len()).find(|&q| matches!(gx.pipelines()[q].source, Source::Breaker { slot } if slot == g.slot)) else { return true };
    for op in gx.pipelines()[q].ops.iter() {
        match op {
            Op::Filter(_) | Op::Gate(_) | Op::Project(_) => {}
            Op::Sort(d) => return d.presorted > 0,
            _ => return true,
        }
    }
    let unordered = |h: &GroupDecl<'_>| matches!(h.form, GroupForm::Plain | GroupForm::Hashed);
    !matches!(gx.pipelines()[q].desc.ends_in, End::Breaker(_) if gx.group_at(q).is_some_and(unordered))
}

/// D3644: whether sorted group `g` (runs.group / runs.unique) declares its key order (`ORDER_KEY`: its breaker's order
/// names the keys). decompose declares none for a group whose rows may come in any order (agg/mod.rs `key_order`: no
/// reader above reads them in order), so its emit hands the groups unsorted (table/emit_group.rs).
pub fn key_ordered(gx: &Gx<'_, '_>, g: &GroupDecl<'_>) -> bool {
    let end = g.producer.0 as usize;
    gx.graph.breakers.iter().any(|d| d.producer as usize == end && d.breaker.slot == g.slot && d.order.n > 0)
}

/// D1596 (D3375): whether full Sort pipeline `k` feeds a plain group whose aggpresorted DISTINCT the group family admits only
/// in PostgreSQL's arrival order and tie order (group/regroup.rs `exact_feed`): sort/exact_feed.rs then orders its run by
/// PG's qsort over its rows in arrival order, serially.
pub(crate) fn presorted_exact(gx: &Gx<'_, '_>, arena: gravity_program::ArenaRef, k: usize) -> bool {
    gx.graph.groups.iter().any(|g| crate::ops::group::regroup::exact_feed(arena, gx.graph, gx.facts, g) == Some(k))
}
