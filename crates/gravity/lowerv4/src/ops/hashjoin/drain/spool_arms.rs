//! D2132 narrowed (partition_join#79 / #80): the spooled drain's union-arm tests spool_admit.rs's ARMS refusal and
//! `every_row` call (spool_admit.rs's helpers live beside it). A helper file (no rows). Owner
//! M7-DRAIN.
use gravity_abi::breaker::BreakerKind;
use gravity_decompose::Op;

use crate::face::kinds::KindId;
use crate::graph::{EndKind, Gx};

/// Op `o` is a bare arm marker: its pipeline is one arm of a union and hands every row on to the union's end as it is.
pub(crate) fn bare_arm(o: &Op<'_>) -> bool {
    crate::graph::is_arm(o) && crate::graph::append_without_exprs(o)
}

/// Drain `d` is an arm of a union (op `o` of its tail a bare arm marker: a RIGHT JOIN per partition under one Append)
/// whose end takes every row before anything reads one (a sort, a top-N, a fold) with no LIMIT in the tail. PostgreSQL's
/// Append runs each arm's join whole, its drain included, before the next arm's probe; the spooled form runs every arm's
/// probe first (decompose numbers the later arms' builds and probes before this drain). Under such an end every arm runs
/// whole in either order and nothing reads a row before the last one: the order shows only through what a probe or a
/// pipeline between does, which N1, USER and TAILVOL judge as for any spooled drain.
pub(crate) fn arm_whole(gx: &Gx<'_, '_>, d: usize, o: &Op<'_>, tail: &[Op<'_>]) -> bool {
    feed_whole(gx, o)
        || (bare_arm(o)
            && !tail.iter().any(|o| crate::graph::step_kind(o) == KindId::StepLimit)
            && matches!(gx.end_kind(d), EndKind::Breaker(BreakerKind::Sort | BreakerKind::TopK | BreakerKind::PlainAgg | BreakerKind::HashAgg)))
}

/// drain (regress domain#507): op `o` is a Feed into a settled union's record (decompose `settle_arms` / the child-by-child
/// form close each arm's last pipeline with it, in arm order; graph.rs `append_without_exprs` without the arm marker: a
/// feed, or a union with no pruning, which only an emit's first op is) and no Append of the statement prunes (an initial
/// pruning runs a pruned arm's feed empty, but its probe and build would run where PostgreSQL never opens the arm) or
/// merges. The drain pipeline is its arm's one feed and appends after the arm's probe ran, as PostgreSQL's Append returns
/// the arm's join whole, its unmatched build rows last, before the next arm's first row; the record reaches its emit in
/// arm order either way. What a later arm's own pipelines run before the drain is N1's (the pipelines between).
pub(crate) fn feed_whole(gx: &Gx<'_, '_>, o: &Op<'_>) -> bool {
    let ps = gx.pipelines();
    let plain = |x: &Op<'_>| crate::graph::step_kind(x) == KindId::SrcAppend && crate::graph::append_without_exprs(x);
    plain(o) && !crate::graph::is_arm(o) && (0..ps.end()).flat_map(|k| ps[k].ops.iter()).all(|x| !matches!(x, Op::Append(_)) || plain(x))
}

/// drain (regress domain#507: information_schema.check_constraints' arm, a Hash Right Join's unmatched pg_constraint rows
/// up nest left joins into the union's record): drain `d`'s end is a settled union's record (src.append, one Feed per
/// arm). No partial form takes it; the spooled form (drain/lower.rs `end`) keeps the drain pipeline the arm's one feed,
/// the probe's joined rows spooled then the drained records, each through the tail into the record in PostgreSQL's order
/// (the arm's probe-phase rows, then its unmatched build rows). The union kept in its Materialize form runs every arm
/// whole before its emit reads a row (D1486 judges an arm that could show it ran where PostgreSQL's Append would stop),
/// so no reader stops the drain's arm early: N1's `every_row` (a pipeline between shows its order only through a call
/// that acts).
pub(crate) fn union_record(gx: &Gx<'_, '_>, d: usize) -> bool {
    gx.spool_kind(d) == KindId::SrcAppend
}
