//! Props: code cells, code expressions and the small graph readers (props/mod.rs; split from props.rs, main 09-24).
use gravity_decompose::ops::shared::sort::KeyImage;
use gravity_decompose::{Op, PipelineNode};
use mcx::{GrowIn, Mcx, PgVec};

use types_nodes::list::NodeList;
use types_nodes::primnodes::{INNER_VAR, OUTER_VAR};
use types_nodes::Node;

use super::props::*;

impl<'p> Walk<'_, '_, 'p> {
    /// The key image a column of type `typid` takes (its type facts), when the type is known.
    pub(crate) fn image_of(&self, typid: u32) -> Option<KeyImage> {
        gravity_decompose::catalog::typefacts::facts_of(self.gx.facts, typid).map(|tf| gravity_decompose::catalog::keysem::image::class_image(tf.image))
    }
}

/// Pipeline `p`'s last projection (a group producer's: its keys and outputs read it through `OUTER_VAR`).
pub(crate) fn last_project<'p>(p: &PipelineNode<'p>) -> Option<&'p NodeList<'p>> {
    p.ops.iter().rev().find_map(|op| if let Op::Project(l) = op { Some(*l) } else { None })
}

/// The scan column a key reads (a hash probe's key, a merge clause's outer arg): through the projections before it (the
/// last one's entry for `OUTER_VAR`). Shared by hashjoin and mergejoin.
pub(crate) fn scan_attno<'a, 'p: 'a>(ops: impl DoubleEndedIterator<Item = &'a Op<'p>>, key: Node<'p>) -> Option<i16> {
    let mut n = strip_key(key);
    let mut projs = ops.filter_map(|op| if let Op::Project(l) = op { Some(*l) } else { None }).rev();
    loop {
        let v = n.as_var()?;
        match v.varno {
            OUTER_VAR => n = strip_key(projs.next()?.iter().nth(v.varattno as usize - 1)?),
            INNER_VAR => return None,
            _ if v.varattno > 0 => return Some(v.varattno),
            _ => return None,
        }
    }
}

/// D3494: `op` hands on each pipeline row it keeps with that row's columns as read, so `scan_attno` reads a scan column
/// through it: a filter, gate or projection, or a join that never NULL-extends this side (a hash probe, nest loop or merge
/// join, INNER / LEFT / SEMI / ANTI; its projection names this side's columns `OUTER_VAR`). RIGHT / FULL rows are not kept.
pub(crate) fn keeps_scan_row(op: &Op<'_>) -> bool {
    use gravity_decompose::join::JoinDesc::{Merge, Nest, Probe};
    use types_nodes::jointype::JoinType::*;
    match op {
        Op::Filter(_) | Op::Gate(_) | Op::Project(_) => true,
        Op::Join(Probe { jointype, .. } | Nest { jointype, .. } | Merge { jointype, .. }) => matches!(jointype, JOIN_INNER | JOIN_LEFT | JOIN_SEMI | JOIN_ANTI),
        _ => false,
    }
}

fn strip_key(n: Node<'_>) -> Node<'_> {
    let mut s = n.as_target_entry().map_or(n, |te| te.expr);
    while let Some(a) = gravity_decompose::expr::through_relabel(s) {
        s = a;
    }
    s
}

/// The projections an `OUTER_VAR` of `top` (one of `p`'s projections) descends through, oldest first, `top` last: each
/// earlier projection of `p` reached across filters and gates and the probe of a hash join whose probe (outer) rows are
/// never NULL-extended (inner, left, semi, anti), so an `OUTER_VAR` in the join's output reads the probe row. Any other
/// op between two projections ends the chain.
pub(crate) fn projections_under<'p, 'm>(p: &PipelineNode<'p>, top: &'p NodeList<'p>, mcx: Mcx<'m>) -> PgVec<'m, &'p NodeList<'p>> {
    use types_nodes::jointype::JoinType::*;
    let Some(at) = p.ops.iter().position(|op| matches!(op, Op::Project(l) if core::ptr::eq(*l, top))) else { return mcx::pg_vec![in mcx; top] };
    let mut out = mcx::pg_vec![in mcx; top];
    for op in p.ops[..at].iter().rev() {
        match op {
            Op::Project(l) => out.push_in(*l),
            Op::Filter(_) | Op::Gate(_) => {}
            Op::Join(gravity_decompose::join::JoinDesc::Probe { jointype: JOIN_INNER | JOIN_LEFT | JOIN_SEMI | JOIN_ANTI, .. }) => {}
            _ => break,
        }
    }
    out.reverse();
    out
}

/// A sort target entry read through the projection before the sort: an `OUTER_VAR` column is that projection's entry.
pub(super) fn through<'p>(e: Node<'p>, input: Option<&'p NodeList<'p>>) -> Node<'p> {
    match (e.as_var().filter(|v| v.varno == OUTER_VAR && v.varlevelsup == 0), input) {
        (Some(v), Some(l)) => entry(l, v.varattno).unwrap_or(e),
        _ => e,
    }
}

/// The expression of target entry `resno` in `tlist`.
pub(crate) fn entry<'p>(tlist: &NodeList<'p>, resno: i16) -> Option<Node<'p>> {
    tlist.iter().filter_map(|n| n.as_target_entry()).find(|t| t.resno == resno).map(|t| t.expr)
}
