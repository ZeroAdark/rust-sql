//! D411 (M2-HEAP): which recipe templates bind from the admitting statement's graph (`BindCx.graph`), so the V4R-18
//! front key (a hit bound without the graph) never files their statements. Every kind answers exhaustively: a new
//! `RecipeTpl` variant or M2 rung recipe must say before it compiles. Owner M2-HEAP (Owner-Ack main).

use gravity_hosts::ops::scan::am_rows::AmRowsRecipe;
use gravity_hosts::ops::scan::tid_sample::{SampleRecipe, TidRecipe};

use crate::ops::scan::sites_heap::{Local, MorselTpl, TreeTpl};
use crate::admit::RecipeTpl;

/// A recipe template's kind as far as the front key cares (D411). Exhaustive on purpose: a new `RecipeTpl` variant or
/// rung recipe must say whether it binds from the admitting statement's graph before it compiles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TplKind {
    Index,
    Bitmap,
    /// D2754: a one-index bitmap whose recheck a front-key hit reads from its own plan (`RecipeTpl::Bitmap`'s fifth field),
    /// a plain recheck or (part 4) one over executor parameters with the words its sixth field records.
    BitmapPlan,
    Range,
    Granule,
    Fetch,
    RungTree,
    /// D2754: a bitmap tree whose recheck a front-key hit reads from its own plan (`TreeTpl::heap`).
    RungTreePlan,
    RungTid,
    RungSample,
    /// D3104: the bitmap morsel form's template (sites_heap.rs `MorselTpl`): its leaves are the serial bitmap's (the same
    /// `leaf_recipes`), its claims' keys are this execution's words (`param_slots`), and its recheck is the program's own
    /// filter (lowerv4 step/filter.rs `guard`): nothing of it binds from the admitting statement's graph.
    RungMorsel,
    RungAmRows,
    /// A rung recipe this module does not know: answered as binding from the graph (never filed under the front key).
    RungOther,
}

pub(crate) fn tpl_kind(t: &RecipeTpl) -> TplKind {
    match t {
        RecipeTpl::Index(_) => TplKind::Index,
        RecipeTpl::Bitmap(_, _, _, _, Some(_), _) => TplKind::BitmapPlan,
        RecipeTpl::Bitmap(..) => TplKind::Bitmap,
        RecipeTpl::Range(_) => TplKind::Range,
        RecipeTpl::Granule(_) => TplKind::Granule,
        RecipeTpl::Fetch(_) | RecipeTpl::HeapFetch(_) => TplKind::Fetch,
        RecipeTpl::Rung(any) if any.downcast_ref::<Local<TreeTpl>>().is_some_and(|t| t.0.heap.is_some()) => TplKind::RungTreePlan,
        RecipeTpl::Rung(any) if any.downcast_ref::<Local<TreeTpl>>().is_some() => TplKind::RungTree,
        RecipeTpl::Rung(any) if any.downcast_ref::<TidRecipe>().is_some() => TplKind::RungTid,
        RecipeTpl::Rung(any) if any.downcast_ref::<Local<SampleRecipe>>().is_some() => TplKind::RungSample,
        RecipeTpl::Rung(any) if any.downcast_ref::<Local<MorselTpl>>().is_some() => TplKind::RungMorsel,
        RecipeTpl::Rung(any) if any.downcast_ref::<AmRowsRecipe>().is_some() => TplKind::RungAmRows,
        RecipeTpl::Rung(_) => TplKind::RungOther,
    }
}

/// Whether a recipe of this kind binds from the admitting statement's graph (`BindCx.graph`): a bitmap walk's or a
/// bitmap tree's recheck qual (`bitmapqualorig`). A front-key hit binds without the graph, so such a statement is never
/// filed there (D411: the tree's rung recipe was missed, and its front hit refused). D2754: a bitmap whose recheck the
/// admission found as its plan node's own (`BitmapPlan`, `RungTreePlan`) is bound on a front-key hit from that node of
/// the hit's own plan (`BindCx.front`, sites_heap.rs `recheck`).
pub(crate) fn graph_use(k: TplKind) -> bool {
    match k {
        TplKind::Bitmap | TplKind::RungTree | TplKind::RungOther => true,
        TplKind::Index | TplKind::BitmapPlan | TplKind::Range | TplKind::Granule | TplKind::Fetch | TplKind::RungTreePlan | TplKind::RungTid | TplKind::RungSample | TplKind::RungMorsel | TplKind::RungAmRows => false,
    }
}

pub(crate) fn binds_from_graph(t: &RecipeTpl) -> bool {
    graph_use(tpl_kind(t))
}

/// D411's test fixtures (admit.rs `tests`): a bitmap tree's template and an unknown rung recipe's.
#[cfg(test)]
pub(crate) fn fixture_tpls() -> (RecipeTpl, RecipeTpl) {
    use gravity_hosts::ops::scan::bitmap_window::{Shape, TreeRecipe};
    use std::sync::Arc;
    let tree = RecipeTpl::Rung(Arc::new(Local(TreeTpl { tree: TreeRecipe { leaves: Vec::new(), shape: Shape::Leaf(0) }, relid: 1, root: 2, heap: None }))); // alloc: Cold
    let other = RecipeTpl::Rung(Arc::new(Local(7u8))); // alloc: Cold
    (tree, other)
}
