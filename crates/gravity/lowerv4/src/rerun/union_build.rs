//! D1486 (material, with#112): the build-first half of rerun/union_arms.rs `hashed`, which calls it (the HOTSPOT rule keeps
//! new helpers of that file here). Registers nothing. A settled union that is a hash join's build on PostgreSQL's
//! build-first path (nodeHashjoin.c HJ_BUILD_HASHTABLE without the outer-first fetch: HJ_FILL_INNER, a RIGHT / FULL / RIGHT
//! ANTI join, or the planner's costs) is read whole by MultiExecHash at the join's first call, arm after arm, before any
//! outer row: the arms filled whole in arm order before the build reads the record are PostgreSQL's run, provided the join
//! is called at all. It is when the join is the statement's root node under filters and projections alone: ExecutePlan
//! pulls the root until it ends, so the join's first call (and its build) comes before the statement's first row whatever
//! the outer holds. In the graph that is the probe's pipeline being the root with only filters and projections after the
//! probe, or, for a join that drains its build's unmatched rows (decompose join.rs `form.drains`), the probe's pipeline
//! spooled into the root, whose first op is the drain and whose others are filters and projections. What runs before the
//! build in pipeline order on the outer's side (its producers) is the probe's (hashjoin/probe.rs `outer`: quiet, else
//! licensed or refused by name); the emit's own ops and keys are `hashed`'s quiet test.
use gravity_decompose::join::JoinDesc;
use gravity_decompose::Op;
use gravity_program::LifeId;

use crate::graph::Gx;

/// Probe `j` of pipeline `q` reads the union's build on the build-first path where PostgreSQL always calls the join (the
/// module doc).
pub(crate) fn build_first(gx: &Gx<'_, '_>, q: usize, j: usize) -> bool {
    let ps = gx.pipelines();
    q < gx.graph.pipelines.len() && ps[q].ops[j + 1..].iter().all(plain) && (q == usize::from(gx.graph.root) || drains(gx, q))
}

/// Build pipeline `emit`'s probe pipeline when the build has two readers, the probe's pipeline and the root draining it
/// (`drains`), as a draining join's build does (the drain pipeline's source is the build).
pub(crate) fn drained_reader(gx: &Gx<'_, '_>, emit: usize) -> Option<usize> {
    let ps = gx.pipelines();
    let n = gx.graph.pipelines.len();
    let root = usize::from(gx.graph.root);
    let mut readers = (0..n).filter(|&c| c != emit && ps[c].desc.consumes.iter().any(|&s| gx.life_of(c, s) == Some(LifeId(emit as u16))));
    let (Some(a), Some(b), None) = (readers.next(), readers.next(), readers.next()) else { return None };
    let q = match (a == root, b == root) {
        (false, true) => a,
        (true, false) => b,
        _ => return None,
    };
    drains(gx, q).then_some(q)
}

/// The graph's root drains pipeline `q`'s product alone: its first op is the drain over `q`'s spool, the others filters and
/// projections, and no other pipeline reads `q`.
fn drains(gx: &Gx<'_, '_>, q: usize) -> bool {
    let ps = gx.pipelines();
    let (n, root) = (gx.graph.pipelines.len(), usize::from(gx.graph.root));
    let Some((Op::Join(JoinDesc::Drain { matches, .. }), rest)) = ps[root].ops.split_first() else { return false };
    let life = Some(LifeId(q as u16));
    let only = (0..n).all(|c| c == root || c == q || !ps[c].desc.consumes.iter().any(|&s| gx.life_of(c, s) == life));
    q < n && q != root && gx.life_of(root, *matches) == life && rest.iter().all(plain) && only
}

fn plain(op: &Op<'_>) -> bool {
    matches!(op, Op::Filter(_) | Op::Project(_))
}
