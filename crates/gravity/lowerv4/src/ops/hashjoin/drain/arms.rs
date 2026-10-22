//! V4R-533 (D393's drain arms): the probe side of a right-family hash join (RIGHT, FULL, RIGHT SEMI, RIGHT ANTI: PG's
//! HJ_FILL_INNER or a Drain) streamed as an Append's arms. Decompose streams the union into the probe's ops (append.rs
//! `push_union`): one pipeline per arm, each ending in the join's spool `m`, the drain reading the last arm's life.
//! `drained_arms` finds that run: graph.rs `spool_kind` names every arm's spool join.drain's, join/drain.rs fuses (a sink)
//! or seeds (a fold / full sort / continuing end) each arm as it does a lone probe, and source/append.rs lets the arms
//! continue into it. `quiet_drain_arms` and `user_drain_arms` are the in-row refusals' tests: the build's skip, op 14
//! deferral and lazy form (build_class.rs `outer`, `defers`, `lazy`) are read at the first arm's probe alone, so every arm
//! must be one PG could run or not without a difference: PG runs no arm over an empty build (not HJ_FILL_OUTER,
//! nodeHashjoin.c:351) and its outer-first path fetches from the arms in turn. A helper file (no rows). Owner M8-NEST
//! (W3-D393-ARMS-ENDS).
use gravity_abi::breaker::BreakerKind;
use gravity_decompose::ops::shared::append::{AppendDesc, UnionRec};
use gravity_decompose::graph::Source;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::Op;
use gravity_program::LifeId;
use mcx::{CollectIn, PgVec};
use types_nodes::jointype::JoinType;
use types_nodes::Node;

use crate::effects::visible::errorhold::quiet_cmp;
use crate::effects::initdb_index;
use crate::effects::visible::drain::{source_still, steps, user_clause};
use crate::frame::px::Px;
use crate::face::kinds::KindId;
use crate::graph::{EndKind, Gx};
use crate::ops::hashjoin::build_class::Side;

/// D1752: a streamed union whose arms run in PG's Append order (frame/mechs.rs `resolve`): arm `i`, pipeline
/// `arms.start + i`, right after child `i`'s own pipelines `starts[i]..starts[i + 1]` (decompose append.rs `UnionRec`),
/// where decompose numbers every arm after the last child.
pub(crate) struct ArmRun<'g> {
    pub(crate) arms: core::ops::Range<usize>,
    pub(crate) starts: PgVec<'g, usize>,
}

impl<'g> Gx<'g, '_> {
    /// The first and last arm of the streamed union pipeline `k` is an arm of, when those arms are a drain's probe: pipelines
    /// `first..=last` one per arm, arm 0 to n - 1 under one outermost Append marker, each ending in the one spool the drain
    /// reads from the last arm's life: a graph pipeline after the arms, or a nest's embedded inner (D3915: a Hash Right /
    /// Full Join over a partitioned table as a Nested Loop's inner, drain/lower.rs `inner_spooled`). `None` when `k` is no
    /// such arm (a lone probe, an arm with an inner union's pipelines).
    pub(crate) fn drained_arms(&self, k: usize) -> Option<(usize, usize)> {
        let ps = self.pipelines();
        let top = |q: usize| {
            let p = ps.get(q)?;
            let Some(b) = crate::frame::iface::breaker_of(&p.desc) else { return None };
            let (j, i, n) = p.ops.iter().enumerate().rev().find_map(|(j, o)| if let Op::Append(AppendDesc::Arm { i, n, .. }) = o { Some((j, *i, *n)) } else { None })?;
            let node = p.node_ops.iter().rev().find(|x| x.first_op as usize <= j).map_or(p.node_id, |x| x.node_id);
            (b.kind == BreakerKind::Materialize).then_some((node, i as usize, n as usize, b.slot))
        };
        let (node, i, n, slot) = top(k)?;
        let first = k.checked_sub(i)?;
        let last = first + n - 1;
        if n < 2 || !(first..=last).all(|q| top(q) == Some((node, q - first, n, slot))) {
            return None;
        }
        let drains = |q: usize| ps[q].ops.iter().any(|o| matches!(o, Op::Join(JoinDesc::Drain { matches, .. }) if *matches == slot)) && self.life_of(q, slot) == Some(LifeId(last as u16));
        (last + 1..ps.end()).any(drains).then_some((first, last))
    }

    /// D393's drain arms, the first arm (regress foreign_key#611 / #613 / #615: `pg_partition_tree(..)` UNION ALL a Result,
    /// VOLATILE): arm `q` of a RIGHT SEMI probe union on PG's outer-first path. RIGHT SEMI has no inner fill, so
    /// ExecHashJoin fetches its first outer row before it builds (nodeHashjoin.c HJ_BUILD_HASHTABLE), from the first arm,
    /// whatever the build will hold; a FunctionScan materializes every function at that first fetch, as the step does
    /// (srf/function_scan.rs). So the arm's function calls (VOLATILE or raising ones too) run once in PG and once here,
    /// the build empty or not. Its other ops before the probe and its probe's keys and quals are `quiet`'s (PG pulls only
    /// up to the first row past them over an empty build), and the build side is `Side::Safe`: it raises and calls
    /// nothing, so running it before the arm (PG runs it after the arm's first fetch) shows nothing. The later arms stay
    /// quiet (join/drain.rs): PG runs them only over a non-empty build or past an empty first arm.
    pub(crate) fn fetched_arm(&self, q: usize) -> bool {
        let p = &self.pipelines()[q];
        let still = |n: Node<'_>| (crate::effects::raise_free(self, n) || quiet_cmp(self, n)) && !crate::effects::user_fn(self, n) && !crate::effects::volatile(self, n);
        let Some(j) = p.ops.iter().rposition(|o| matches!(o, Op::Join(JoinDesc::Probe { .. }))) else { return false };
        let Op::Join(JoinDesc::Probe { slot, jointype: JoinType::JOIN_RIGHT_SEMI, outer_first: true, catalog: false, keys, qual, .. }) = &p.ops[j] else {
            return false;
        };
        let safe = self.life_of(q, *slot).is_some_and(|l| crate::ops::hashjoin::build_class::side(self, l.0 as usize) == Side::Safe);
        // a FROM-clause set function (srf.function / srf.pl: a FunctionScan), its target list quiet
        let srf = |op: &Op<'_>| matches!(op, Op::Srf(d) if matches!(crate::graph::step_kind(op), KindId::SrfFunction | KindId::SrfPl) && d.tlist.iter().all(still));
        matches!(p.source, Source::Const)
            && safe
            && p.desc.consumes.iter().all(|s| s == slot)
            && keys.iter().chain(qual.iter()).all(still)
            && p.ops[..j].iter().filter(|op| srf(op)).count() == 1
            && p.ops[..j].iter().all(|op| match op {
                Op::Filter(n) | Op::Gate(n) => still(*n),
                Op::Project(l) => l.iter().all(still),
                Op::Append(AppendDesc::Arm { prune: None, .. }) => true,
                op => srf(op),
            })
    }

    /// D1752 where `arm_runs` does not take the union: whether every pipeline numbered between probe `k` and its drain `d`
    /// (an Append's arm) runs unseen (`unseen`). PG's Append runs its arms one after another (ExecAppend), each arm's join
    /// whole, its drain included, before the next arm's first row, where decompose numbers a later arm's builds and probes
    /// before this arm's drain (append.rs `push_union` pushes each arm's last pipeline there; an earlier arm's one is
    /// between too, and PG ran it already). A later drain arm's fused probe hands its rows on through its tail before this
    /// drain's: under a stop that picks other rows (an aggregate over the LIMIT sums them), so it is never unseen.
    pub(crate) fn between_unseen(&self, k: usize, d: usize) -> bool {
        let Some((node, i, arms)) = arm_at(self, d) else { return false };
        let probes: PgVec<'_, usize> = (0..self.pipelines().len()).filter(|&x| arm_at(self, x).is_some_and(|(at, _, m)| at == node && m == arms)).filter_map(|x| fused(self, x).map(|f| f.0)).collect_in(self.mcx);
        (k + 1..d).all(|q| arm_at(self, q).is_some_and(|(at, j, m)| at == node && m == arms && j < i) || (!probes.contains(&q) && unseen(self, q)))
    }

    /// D1752: every union whose arms run in PG's Append order (`ArmRun`, `arm_run`).
    pub(crate) fn arm_runs(&self) -> PgVec<'g, ArmRun<'g>> {
        self.graph.unions.iter().filter_map(|u| arm_run(self, u)).collect_in(self.mcx)
    }

    /// D1752 where union `r` runs its arms in PG's order, under a stop: probe `k`'s drain `d` has only its own child's
    /// pipelines past the probe before it, each unseen. Under a LIMIT the later children's pipelines run after the drain even
    /// when its rows fill the count: each must run unseen too, but a later drain arm's fused probe, whose tail holds that
    /// LIMIT's copy (step/limit.rs `window`: a full count pulls no row; a cursor's stop runs nothing past it), and a
    /// pipeline the LIMIT's rows already read stop (rerun/union_arms.rs `full_fed`: its feeder's `Check::Full`, D1753; a
    /// later partition's build that raises).
    pub(crate) fn arm_order_unseen(&self, px: &mut Px<'_, '_>, r: &ArmRun<'_>, k: usize, d: usize) -> bool {
        let own = r.starts[d - r.arms.start + 1];
        let probes: PgVec<'_, usize> = r.arms.clone().filter_map(|q| fused(self, q).map(|f| f.0)).collect_in(px.mcx());
        let union = arm_at(self, r.arms.start).map_or(-1, |a| a.0);
        let mut stopped = |q: usize| crate::rerun::union_arms::full_fed(px, q, union);
        (k + 1..own).all(|q| unseen(self, q)) && (own..r.arms.start).all(|q| probes.contains(&q) || unseen(self, q) || stopped(q))
    }

    /// D1752: the plan node of the Append whose arm pipeline `d` is (its outermost arm marker), if any.
    pub(crate) fn arm_append(&self, d: usize) -> Option<i32> {
        arm_at(self, d).map(|a| a.0)
    }
}

/// D1752: pipeline `q` runs unseen and hands no row on: a source `quiet` takes (or a breaker's read, or a Result's row),
/// filters, gates, projections, an Append's arm marker with no pruning, a hash build's keys, a hash probe's keys, hash
/// clauses and quals, each raising nothing and calling no user or VOLATILE function, ending in a hash build, a spool, or a
/// plain aggregate whose aggregates and FILTERs are as quiet (another arm's fold: D1758's form).
fn unseen(gx: &Gx<'_, '_>, q: usize) -> bool {
    let p = &gx.pipelines()[q];
    let still = |n: Node<'_>| (crate::effects::raise_free(gx, n) || quiet_cmp(gx, n)) && !crate::effects::user_fn(gx, n) && !crate::effects::volatile(gx, n);
    let src = matches!(p.source, Source::Const | Source::Breaker { .. }) || source_still(gx, p, &still);
    let end = match gx.end_kind(q) {
        EndKind::Breaker(BreakerKind::HashBuild | BreakerKind::Materialize) => true,
        EndKind::Breaker(BreakerKind::PlainAgg) => gx.group_at(q).is_some_and(|g| g.aggs.iter().all(|a| still(a.node) && a.filter.is_none_or(still))),
        _ => false,
    };
    src && end
        && p.ops.iter().all(|op| match op {
            Op::Filter(n) | Op::Gate(n) => still(*n),
            Op::Project(l) => l.iter().all(still),
            Op::Append(AppendDesc::Arm { prune: None, .. }) => true,
            Op::Join(JoinDesc::Build { keys, .. }) => keys.iter().all(still),
            Op::Join(JoinDesc::Probe { keys, clauses, qual, catalog: false, .. }) => keys.iter().chain(clauses.iter()).chain(qual.iter()).all(still),
            _ => false,
        })
}

/// D1752: arm pipeline `q`'s probe and Drain op when `q` is a drain in the fused or seeded form (lower.rs's `m` rule: its
/// ops past the Drain op are all tail ops, so the probe hands its joined rows on through the drain's tail).
fn fused(gx: &Gx<'_, '_>, q: usize) -> Option<(usize, usize)> {
    let p = &gx.pipelines()[q];
    let (jd, m) = p.ops.iter().enumerate().find_map(|(j, op)| if let Op::Join(JoinDesc::Drain { matches, .. }) = op { Some((j, *matches)) } else { None })?;
    let k = gx.life_of(q, m)?.0 as usize;
    let spool = gx.pipelines().get(k)?.desc.produces() == Some(m) && matches!(gx.end_kind(k), EndKind::Breaker(BreakerKind::Materialize));
    (spool && k < q && p.ops[jd + 1..].iter().all(super::lower::tail_op)).then_some((k, jd))
}

/// D1752 / D1753: union `u` as an `ArmRun` when its arms take PG's order and it shows: a drain arm in the fused or seeded
/// form (its probe's rows come from its child's pipelines), or a later child's pipeline that may raise or call user or
/// VOLATILE code (neither `unseen` nor effects.rs `span_quiet` with quiet aggregates) under an end a stop reaches inside an
/// arm (a sink, which a LIMIT or a cursor stops; a plain aggregate over a LIMIT past the marker), or (D4124) under any
/// end, one that makes a sequence, clock or user call (effects/visible.rs `order_shown`) after an earlier arm that makes one;
/// or (D3437) when it frees memory and shows nothing: two or more arms read breakers their own children filled;
/// every arm reading only lives its own child or an earlier one made, no rerun set holding any of its pipelines
/// (nest.rerun re-runs members in program order by its own rules), and no pipeline among the children using the arms' end
/// slot (it would lie inside that end's life once the arms run between the children).
fn arm_run<'g>(gx: &Gx<'g, '_>, u: &UnionRec<'_>) -> Option<ArmRun<'g>> {
    let ps = gx.pipelines();
    // a grouping-sets node's arms (`UnionRec::sets`) keep graph order: they are one Agg node's sets over one input, not
    // an Append's children, and the Append order below is not theirs
    if u.sets {
        return None;
    }
    let n = u.starts.len().checked_sub(1)?;
    let arms = u.pushed.start as usize..u.pushed.end as usize;
    if n < 2 || arms.len() != n || arms.end > ps.len() {
        return None;
    }
    let node = arm_at(gx, arms.start)?.0;
    if !arms.clone().all(|q| arm_at(gx, q) == Some((node, (q - arms.start) as u16, n as u16))) {
        return None;
    }
    let starts: PgVec<'g, usize> = u.starts.iter().map(|&s| s as usize).collect_in(gx.mcx);
    if starts.windows(2).any(|w| w[0] > w[1]) || starts[n] > arms.start {
        return None;
    }
    let early = |i: usize, q: usize| ps[q].desc.consumes.iter().all(|&s| gx.life_of(q, s).is_some_and(|l| (l.0 as usize) < starts[i + 1]));
    if !arms.clone().enumerate().all(|(i, q)| early(i, q)) {
        return None;
    }
    let held = |p: u16| (starts[0]..arms.end).contains(&(p as usize));
    if gx.graph.reruns.iter().any(|r| r.pipelines.iter().any(|&p| held(p))) {
        return None;
    }
    if let Some(e) = ps[arms.start].desc.produces() {
        if (starts[0]..arms.start).any(|q| ps[q].desc.produces() == Some(e) || ps[q].desc.consumes.contains(&e)) {
            return None;
        }
    }
    // where neither shows, every arm runs whole in either order and graph order keeps the Arms run one span of arms (a full
    // sort merges that many runs; program.rs `to_program` reads lives inside it as the last arm's)
    let marker = ps[arms.start].ops.iter().rposition(crate::graph::is_arm)?;
    let limit = ps[arms.start].ops[marker + 1..].iter().any(|o| crate::graph::step_kind(o) == crate::face::kinds::KindId::StepLimit);
    let stops = match gx.end_kind(arms.start) {
        EndKind::Sink => true,
        EndKind::Breaker(BreakerKind::PlainAgg) => limit,
        _ => false,
    };
    let quiet = |q: usize| {
        let calm = |n: Node<'_>| crate::effects::expr_quiet(gx, n);
        let aggs = gx.group_at(q).is_none_or(|g| g.aggs.iter().all(|a| calm(a.node) && a.filter.is_none_or(calm)));
        unseen(gx, q) || (aggs && crate::effects::span_quiet(gx, &ps[q], 0, ps[q].ops.len(), false))
    };
    // D4124: graph order runs every child before the arms, so a later child's pipeline that makes a call whose place shows
    // (effects/visible.rs `order_shown`: a sequence, clock or user call) would make it before an earlier arm's own pipeline
    // makes one, an order PG's Append fixes whatever the end above it (each arm whole in turn): such a run takes PG's
    // order under a sort or a build end too. The children are read first (a partitioned table's arms have none between
    // them), the arms only up to the first such child.
    let shown = |q: usize| crate::effects::visible::order_shown(gx, q);
    let order = || (1..n).find(|&i| (starts[i]..starts[i + 1]).any(shown)).is_some_and(|i| (0..i).any(|a| shown(arms.start + a)));
    let shows = (stops && !(starts[1]..arms.start).all(quiet)) || order();
    // D3437: where two or more arms each read a breaker their own child filled, graph order keeps every child's finished
    // breaker resident until the first arm reads it; PG's order frees each child's slot after its arm (the driver frees a
    // slot after its last reader), one child's at a time. Taken where the move shows nothing: no held range keeps the
    // union's slots for the statement (D3646), and every later child's pipeline is quiet, or the arms end in a breaker
    // and their own work raises nothing and calls nothing visible (effects.rs `arm_quiet`). Aggregates read in the fold
    // form (effects.rs `fold_quiet`).
    let child = |q: usize| {
        let aggs = gx.group_at(q).is_none_or(|g| g.aggs.iter().all(|a| crate::effects::fold_quiet(gx, a.node) && a.filter.is_none_or(|f| crate::effects::fold_quiet(gx, f))));
        unseen(gx, q) || (aggs && crate::effects::span_quiet(gx, &ps[q], 0, ps[q].ops.len(), false))
    };
    let own = |i: usize, q: usize| ps[q].desc.consumes.iter().any(|&s| gx.life_of(q, s).is_some_and(|l| (starts[i]..starts[i + 1]).contains(&(l.0 as usize))));
    let leased = gx.graph.held.iter().any(|&(a, z)| (a as usize) < arms.end && starts[0] < z as usize);
    let frees = || {
        let fed = arms.clone().enumerate().filter(|&(i, q)| own(i, q)).count() >= 2;
        let ended = matches!(gx.end_kind(arms.start), EndKind::Breaker(_));
        fed && !leased && ((starts[1]..arms.start).all(child) || (ended && arms.clone().all(|q| crate::effects::arm_quiet(gx, q))))
    };
    (shows || arms.clone().any(|q| fused(gx, q).is_some()) || frees()).then_some(ArmRun { arms, starts })
}

/// The outermost Append arm marker of pipeline `q`: (the Append's node, arm, arms), as `drained_arms` reads it.
fn arm_at(gx: &Gx<'_, '_>, q: usize) -> Option<(i32, u16, u16)> {
    let p = &gx.pipelines()[q];
    let (j, i, n) = p.ops.iter().enumerate().rev().find_map(|(j, o)| if let Op::Append(AppendDesc::Arm { i, n, .. }) = o { Some((j, *i, *n)) } else { None })?;
    let node = p.node_ops.iter().rev().find(|x| x.first_op as usize <= j).map_or(p.node_id, |x| x.node_id);
    Some((node, i, n))
}
