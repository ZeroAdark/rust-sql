//! The rerun family's visible decisions in the effects home (moved from rerun/inner_probe.rs, rerun/spool/form.rs and
//! rerun/params_key.rs): a union arm calm by its raises (`arm_calm`, D1018), a write whose per-row work is ModifyTable's
//! alone (`write_calm`, rowsecurity#343), and a computed key that may run user code (`key_user_code`, D2071).
use gravity_decompose::{Op, PipelineNode, Source};
use types_nodes::Node;

use super::nestloop::{bitmap_quiet, quiet_ops};
use crate::graph::Gx;

/// D1018 (heldout rnd-0426): a union arm (a feed pipeline) whose scan and steps up to its feed raise nothing (`calm`): PG
/// pulls the arms of the Append on its outer-first path before it builds an empty table; the count-free ruling reads them
/// by their raises only, as the inner's own source.
pub(crate) fn arm_calm(p: &PipelineNode<'_>, calm: &dyn Fn(Node<'_>) -> bool) -> bool {
    let src = match &p.source { Source::Range { .. } => true, Source::Index { keys, recheck, .. } => keys.iter().all(|k| calm(k.value)) && recheck.iter().all(calm), Source::Bitmap { plan, recheck, .. } => bitmap_quiet(plan, recheck, calm), _ => false };
    src && p.ops.iter().all(|op| matches!(op, Op::Append(gravity_decompose::ops::shared::append::AppendDesc::Feed { .. })) || quiet_ops(std::slice::from_ref(op), calm))
}

/// rowsecurity#343: write `w`'s per-row work is ModifyTable's alone: an UPDATE or DELETE with no ON CONFLICT or MERGE, every
/// result relation (and a moved row's leaf) a heap or partitioned table with no BEFORE or INSTEAD OF row trigger for the event,
/// its check options and RETURNING lists making no call `calls` names (union_arms.rs `write_still`'s reading, raises allowed).
pub(crate) fn write_calm(gx: &Gx<'_, '_>, w: &gravity_decompose::ops::shared::modifytable::WriteDesc<'_>, calls: &dyn Fn(Node<'_>) -> bool) -> bool {
    use gravity_decompose::ops::modifytable::trig_facts::{TrigExt, TRIG_DELETE, TRIG_UPDATE};
    use gravity_decompose::ops::shared::modifytable::TargetKind;
    use gravity_program::Cmd;
    let events = match w.cmd {
        Cmd::Update => TRIG_UPDATE,
        Cmd::Delete => TRIG_DELETE,
        _ => return false,
    };
    let quiet_trig = |t: TrigExt| t.known && (t.before_row | t.instead_row) & events == 0;
    let list = |l: Option<&types_nodes::list::NodeList<'_>>| l.is_none_or(|l| !l.iter().any(calls));
    let targets = w.targets.iter().all(|t| matches!(t.kind, TargetKind::Heap | TargetKind::Partitioned) && quiet_trig(gx.facts.trig_ext(t.relid)) && list(t.returning));
    // a route moves no row unless the UPDATE sets partition key columns (`cross_partition`, partColsUpdated): an inheritance
    // root's or a partition tree's in-place write routes nothing to a leaf
    let leaves = w.route.as_ref().is_none_or(|r| !r.cross_partition) || w.leaves_known && w.leaves.iter().all(|l| quiet_trig(l.trig));
    w.on_conflict.is_none() && w.merge.is_none() && targets && leaves && !w.checks.iter().any(|c| calls(c.qual)) && list(w.returning) && quiet_trig(gx.facts.trig_ext(w.plan.target_rel))
}

/// D2071 (D439, as srf/store_read.rs declares for a re-opened store): computed key `v` may run user code (a user function
/// or operator, a node not named), the host's at each re-open in PG's executor state; so a pull (a cursor's FETCH n,
/// Execute's max_rows) asks for the rows it still owes, and no outer row PG never pulls re-opens the walk.
pub(crate) fn key_user_code(gx: &Gx<'_, '_>, v: Node<'_>) -> bool {
    gx.fx(v).user_by(&crate::effects::user_fn::Reading::params_key())
}
