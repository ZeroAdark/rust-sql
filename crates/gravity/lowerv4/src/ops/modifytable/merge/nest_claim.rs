//! NEST-CLAIM (gen lk-0116, the partitioned claim `UPDATE lp .. WHERE (d, id) IN (SELECT d, id FROM lp .. ORDER BY
//! id LIMIT n FOR UPDATE SKIP LOCKED) RETURNING id`): an UPDATE / DELETE of several result relations over a nested loop
//! whose inner is the Append of one parameterized index scan per result relation, pruned at run time by the outer row's
//! parameters. PostgreSQL 18.6 plans the IN subquery's HashAggregate the outer; decompose embeds the Append as a union the
//! nest's inner reads first, each arm a feed (M8-NEST S2c), the union pruning at each re-open. A row's result relation is
//! its junk tableoid and its position its junk ctid; EvalPlanQual substitutes that relation's scan with the newest version
//! and blocks the others (relsubs_blocked), over the same outer row, so the same arms survive the pruning and the re-run
//! inner yields that version alone, through its arm's keys (IndexRecheck).
//!
//! write/update_built.rs `nest_union` takes this union's feeds from `embedded_feeds` (the union's own arms, pipes.rs
//! `inner_union`), so the union form serves it (write/update.rs, write/delete_built.rs, RETURNING through update_built.rs
//! `ret_cells` / returning.rs `make_delete`); rerun/inner_arms_index.rs's write rule asks `covers` (an index arm of this
//! form is rechecked through its keys at the substituted open). Helper file of the union form: no rows of its own.
use gravity_decompose::{Op, PipelineNode};
use gravity_program::Cmd;

use crate::graph::Gx;

/// The feeds, in arm order, of the run-time-pruned union whose emit `emit` is nest inner `ik` itself (pipes.rs
/// `inner_union`: the union's own arms), else none.
pub(crate) fn embedded_feeds(gx: &Gx<'_, '_>, ik: usize, emit: usize) -> Option<Vec<usize>> {
    let (e, _, feeds) = crate::pipes::inner_union(gx, ik)?;
    (e == emit && emit == ik).then_some(feeds)
}

/// rerun/inner_arms_index.rs's write rule: whether arm pipeline `arm` is a feed of the union form's nest inner
/// (write/update_built.rs `nest_union`, no flattened UNION ALL outer) of the statement's one write, an UPDATE or DELETE of
/// several result relations, every other pipeline that writes holding row locks only (an IN subquery's LockRows: its own
/// recheck reruns its own subtree, never this nest's inner).
pub(crate) fn covers(gx: &Gx<'_, '_>, arm: &PipelineNode<'_>) -> bool {
    let ps = gx.pipelines();
    let at = arm as *const PipelineNode<'_> as *const u8;
    let Some(a) = (0..ps.len()).find(|&q| core::ptr::eq(&ps[q] as *const PipelineNode<'_> as *const u8, at)) else { return false };
    let mut writers = (0..ps.len()).filter(|&q| crate::rerun::inner_arms_index::writes(&ps[q]) && !locks_only(&ps[q]));
    let (Some(k), None) = (writers.next(), writers.next()) else { return false };
    let mut ws = ps[k].ops.iter().filter_map(|o| if let Op::Write(w) = o { Some(w) } else { None });
    let (Some(w), None) = (ws.next(), ws.next()) else { return false };
    matches!(w.cmd, Cmd::Update | Cmd::Delete) && ps[k].ops.iter().all(|o| !matches!(o, Op::Lock(_))) && !super::super::update_built::copied_union(gx, k, w) && super::super::update_built::nest_union(gx, k, w).is_some_and(|f| f.contains(&a))
}

/// Pipeline `p` writes no row: its ops and its nests' inners hold row locks only (a SubPlan step counts as a writer).
fn locks_only(p: &PipelineNode<'_>) -> bool {
    crate::stack_check(); // D3832
    use gravity_decompose::join::JoinDesc;
    p.ops.iter().all(|op| match op {
        Op::Write(_) | Op::SubPlan(_) => false,
        Op::Join(JoinDesc::Nest { inner, .. }) => locks_only(inner),
        _ => true,
    })
}
