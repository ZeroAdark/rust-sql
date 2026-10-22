//! A full sort whose rows feed a sort keyed by a SQL / PL comparator, sorted in PostgreSQL's own
//! order. A Sort over another breaker's rows calls its comparator on the pairs its arrival order meets; PostgreSQL's
//! tuplesort meets the pairs its qsort meets over the rows in the order its child returned them. When that child is itself
//! a plain Sort over a heap scan, PostgreSQL's arrival order is that Sort's qsort (sort_template.h over the rows in heap
//! order, a key tie left as the qsort leaves it). The feeding sort's run here orders by the same qsort in arrival order
//! (the KFLAG_USER run, kernels rowsort.rs `sort_exact`), serially, so the consumer's comparator meets PostgreSQL's pairs.
use gravity_decompose::catalog::Am;
use gravity_decompose::ops::shared::sort::{SortForm, SortKey};
use gravity_decompose::{Op, Source};
use types_nodes::list::NodeList;

use crate::face::runs::Shape;
use crate::frame::px::Px;
use crate::graph::Gx;

/// Sort pipeline `k`'s run shape, one for its producer (lower.rs `RunEnd::over`) and its reader (`make_emit`):
/// `Shape::plan_exact`'s when its run feeds a user comparator's sort (`feeds_user_sort`), with whether it does.
pub(crate) fn shape(px: &Px<'_, '_>, k: usize, keys: &[SortKey], tlist: Option<&NodeList<'_>>, cols: &NodeList<'_>) -> (Shape, bool) {
    let node = px.gx.pipelines()[k].node_id;
    // D1596 (D3375): or the Sort PG put under a plain group whose presorted DISTINCT reads PG's comparator calls and tie order
    let exact = tlist.is_some() && (crate::effects::visible::sort::feeds_user_sort(px.gx, k) || crate::frame::iface::presorted_exact(px.gx, px.arena, k));
    (if exact { Shape::plan_exact(px, node, keys, tlist, cols) } else { Shape::plan(px, node, keys, tlist, cols) }, exact)
}

/// Pipeline `r`'s rows come from a feeding sort (`feeds_user_sort`), so they arrive in PostgreSQL's order.
pub(crate) fn fed_exact(gx: &Gx<'_, '_>, r: usize) -> bool {
    let Source::Breaker { slot } = gx.pipelines()[r].source else { return false };
    gx.life_of(r, slot).is_some_and(|l| crate::effects::visible::sort::feeds_user_sort(gx, l.0 as usize))
}

/// Pipeline `k` is a heap scan's rows, through filters and projections only, into one full sort (`SortForm::Runs`).
pub(crate) fn plain_sort(gx: &Gx<'_, '_>, k: usize) -> bool {
    let p = &gx.pipelines()[k];
    let Some((Op::Sort(d), before)) = p.ops.split_last() else { return false };
    matches!(p.source, Source::Range { am: Am::Heap, .. })
        && before.iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_)))
        && matches!(d.form, SortForm::Runs)
        && d.presorted == 0
        && d.keys.iter().all(|key| !key.own_family && key.no_proc.is_none())
        && crate::frame::iface::run_holds_rows(gx, k)
        && !crate::rerun::sets::in_owner_set(gx, k)
        && !crate::rerun::sets::in_term_set(gx, k)
}
