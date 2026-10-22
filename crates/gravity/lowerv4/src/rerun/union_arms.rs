//! A settled union (decompose append.rs `settle_arms`: each arm's pipeline ends `Feed { slot }` in one Materialize slot, the
//! emit's source reads it and its first op is `Union`), nest.spool's (M8-NEST S2a, D1483; a helper of nest/spool.rs, which
//! source/append.rs's `src.append` makers hand these ops, ends and sources to by kind). The eager form: each feed's end
//! appends its rows to one record (nest/spool_rec.rs, declared at the first feed; the feeds are serial pipelines run in
//! arm order, so the record holds PostgreSQL's Append order) and the emit reads it once (nest/spool.rs's eager reader).
//! Filling every arm up front is PostgreSQL's Append when PostgreSQL reads every arm to its end too (`unconditional`: the
//! emit feeds a sort, top-k, aggregate or distinct, and so on up to the statement's root, with nothing that could keep it
//! from fetching), or when no arm can show whether it ran (`quiet_arm`: a scan, a Result or a VALUES list under filters and
//! projections that raise nothing and call no user or VOLATILE function, join/rtfilter.rs `raise_free`'s rule without its
//! extensions), so a skip PostgreSQL makes (an empty outer before a hash build, a LIMIT above the union) is unobservable.
//! An arm that is a hash join of quiet scans is quiet too (nest/union_join_arms.rs). So is it when PostgreSQL drains
//! every arm into a root INSERT whose per-row work acts nowhere and no arm acts (`drained`, material item 9: an arm that
//! only raises then raises where PostgreSQL does too). An initial pruning (decided at the executor start) runs each pruned child's feed empty (its skip bit, `step`). Refused by name: other arms, or a run-time (exec) pruned union (D1486); a union in a rerun set, whose arms PostgreSQL re-runs per rescan (D1483). A union a nest
//! inner embeds streams its arms per re-open (S2b, nest/inner_arms.rs's feeds, ends, source), or, when the nest's set
//! refills it (S2r, `refilled`: arms over the set's breakers), is filled by the set at every re-open and read as here, as is a union a member of the set emits below the inner (a hash
//! build's Append, `member_refill`), its quiet exec pruning run as no pruning (D2013). A union that fills a plan
//! Material the eager form refuses is pulled arm after arm by the Material's reader, as PostgreSQL's Material pulls its
//! Append (G2(e), S1u: rerun/spool/form.rs `union_pull`, `pulled`): its feeds and emit read nothing. Registers nothing. Owner M8-NEST.
use gravity_abi::breaker::BreakerKind;
use gravity_decompose::ops::shared::append::{AppendDesc, PruneDesc};
use gravity_decompose::graph::{slot_has, Bound};
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::sort::{SortDesc, SortForm};
use gravity_decompose::{Op, PipelineNode, Source};
use gravity_ir::builder::FunctionBuilder;
use gravity_decompose::effects;
use gravity_decompose::ops::modifytable::trig_facts::TRIG_INSERT;
use gravity_decompose::ops::shared::modifytable::WriteDesc;
use gravity_program::{ArenaRef, Cmd, LifeId};
use types_nodes::jointype::JoinType;
use types_nodes::{Node, NodeList};
use mcx::{CollectIn, GrowIn, PgBox, PgVec};

use crate::rerun::spool::rec::Rec;
use crate::face::end::EndMech;
use crate::face::kinds::KindId;
use crate::face::source::SourceMech;
use crate::face::step::StepMech;
use crate::frame::env::Env;
use crate::frame::frame::Next;
use crate::frame::iface;
use crate::frame::px::{Px, WordKey};
use crate::frame::row::{Carry, RowVals};
use crate::graph::{EndKind, Gx};
use crate::effects::visible::union::refill_calls;
use crate::effects::visible::material::quiet_nest;
use crate::effects::visible::nestloop::acts;
// moved to the effects home; the union strategies' callers keep their `rerun::union_arms` path
pub(crate) use crate::effects::visible::nestloop::{quiet_arm, quiet_arm_in, quiet_build, quiet_in, quiet_prune};
use crate::rerun::spool::form::{KEPT_UNION, LOUD_UNION};
use crate::admit::refusal::{Reason, ShapeWhy};

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

/// D3373 (`refill_arms`): a held union's arm that may raise outside a whole sort, refilled ahead of PostgreSQL's pulls
/// where the unchecked licence is denied.
pub(crate) const HELD_UNION_AHEAD: &str = "a union a rerun set refills at every re-open over an arm that may raise where PostgreSQL may stop before it, in a statement that writes, locks rows or calls VOLATILE code, or with no unchecked licence for it (D1483)";

/// D3373 (`refill_arms`): a held union's arm that S2q's refill would take but for a user or VOLATILE call (`refill_calls`),
/// which never runs ahead of PostgreSQL's pulls.
pub(crate) const HELD_UNION_CALLS: &str = "a union a rerun set refills at every re-open over an arm that calls a user or VOLATILE function, or one hidden in another node, where PostgreSQL may stop before it (D1483)";

/// G2(c) part 2 (merge/route.rs `union_targets`): the feeds by target index (`targets` order) of the settled union
/// pipeline `k` reads first (its Union op, an initial pruning or none), each a heap range or index scan of one of write
/// `w`'s result relations under filters and projections into the union's slot; else `None`.
pub(crate) fn target_feeds<'g>(gx: &Gx<'g, '_>, k: usize, w: &WriteDesc<'_>) -> Option<PgVec<'g, usize>> {
    use gravity_decompose::Am;
    let ps = gx.pipelines();
    let p = ps.get(k)?;
    let Source::Breaker { slot } = p.source else { return None };
    // its first op the union's (an initial pruning or none: union_arms.rs refuses an exec one by D1486 first)
    let first = p.ops.first().is_some_and(|o| crate::graph::step_kind(o) == KindId::SrcAppend && !crate::graph::is_arm(o));
    if !first || union_of(p).map(|u| u.0) != Some(slot) || feed_of(p).is_some() || w.targets.len() < 2 || w.tableoid_col <= 0 {
        return None;
    }
    let feeds = crate::rerun::union_join_arms::feeds(gx, k, slot);
    if feeds.len() != w.targets.len() {
        return None;
    }
    let mut by = mcx::pg_vec![in gx.mcx; usize::MAX; w.targets.len()];
    for &f in feeds.iter() {
        let q = &ps[f];
        let (rti, relid) = match &q.source {
            Source::Range { am: Am::Heap, scanrelid, relid, .. } | Source::Index { am: Am::Heap, scanrelid, relid, only: false, .. } => (u32::from(*scanrelid), *relid),
            _ => return None,
        };
        let Some((last, body)) = q.ops.split_last() else { return None };
        if feed_of(q) != Some(slot) || matches!(last, Op::Filter(_) | Op::Project(_)) || !q.desc.consumes.is_empty() || !body.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_))) {
            return None;
        }
        let i = w.targets.iter().position(|t| t.rti == rti && t.relid == relid)?;
        if by[i] != usize::MAX {
            return None;
        }
        by[i] = f;
    }
    Some(by)
}

/// Pipeline `p`'s Feed slot (a settled union's arm).
pub(crate) fn feed_of(p: &PipelineNode<'_>) -> Option<u8> {
    p.ops.iter().find_map(|op| if let Op::Append(AppendDesc::Feed { slot, .. }) = op { Some(*slot) } else { None })
}

/// Pipeline `p`'s Union op (a settled union's emit): its slot and whether it prunes.
pub(crate) fn union_of(p: &PipelineNode<'_>) -> Option<(u8, bool)> {
    p.ops.iter().find_map(|op| if let Op::Append(AppendDesc::Union { slot, prune, .. }) = op { Some((*slot, prune.is_some())) } else { None })
}

/// `o` is a settled union's unpruned Union op (the first op of the pipeline reading its spool).
pub(crate) fn union_op(o: &Op<'_>) -> bool {
    matches!(o, Op::Append(AppendDesc::Union { prune: None, .. }))
}

/// lockrows portals#244: `o` is a union arm's or feed's Append marker (no work of its own).
pub(crate) fn feed_op(o: &Op<'_>) -> bool {
    matches!(o, Op::Append(AppendDesc::Feed { .. } | AppendDesc::Arm { .. }))
}

/// Pipeline `p` reads a settled union's spool (its first op an unpruned Union, `union_op`).
pub(crate) fn union_reader(p: &PipelineNode<'_>) -> bool {
    p.ops.first().is_some_and(union_op)
}

/// Pipeline `p`'s Union op's pruning and child count (a settled union's emit that prunes).
fn union_prune<'g, 'p>(p: &'g PipelineNode<'p>) -> Option<(&'g PruneDesc<'p>, u16)> {
    p.ops.iter().find_map(|op| if let Op::Append(AppendDesc::Union { prune: Some(d), children, .. }) = op { Some((d, *children)) } else { None })
}

/// G2(e), S1u: emit `e`'s union fills a plan Material whose reader pulls it arm after arm (rerun/spool/form.rs
/// `union_pull`, taken where the eager form refuses): its feeds and the emit read nothing and the reader makes the arms.
pub(crate) fn pulled(gx: &Gx<'_, '_>, e: usize) -> bool {
    e < gx.pipelines().len() && union_of(&gx.pipelines()[e]).is_some() && matches!(crate::pipes::spool_fill(gx, e), Ok((_, true)))
}

/// S2r: the settled union `(emit, slot, feeds)` a nest inner emits is refilled by the nest's rerun set at every re-open
/// and read as the eager form's record: an arm reads a breaker (S2b streams scan arms only, D2014), every arm and every
/// breaker an arm reads is the set's (a LATERAL grouping-sets inner: each arm a group's emit, decompose gsets.rs), the slot
/// is the set's, the union prunes nothing, and either the inner raises nothing and calls nothing VOLATILE (FACTS-1b
/// F5, effects.rs `refill_quiet`), so the arms PostgreSQL's Append skips past an early stop (a SEMI match, a LIMIT met) show
/// nothing, or PostgreSQL reads every arm whole at every rescan and nothing runs between them (regress subselect#224 /
/// #225: a Sort of random() in an arm, sorted again per rescan without EXEC_FLAG_REWIND): an INNER or LEFT nest that
/// stops at no match in the statement's root (its sink), only raise-free filters and projections past the union and the
/// nest (no LIMIT) and a raise-free join qual,
/// and every member but the arms a sort's or top-k's fill run just before the arm that reads it (a Sort reads its child
/// once per sort). ExecReScanAgg re-reads a
/// sorted, mixed or plain Agg's input at every rescan, as the set's run re-runs the fills; the first feed's open empties
/// the record (`end`), as D1107's eager refill.
pub(crate) fn refilled(gx: &Gx<'_, '_>, (e, slot, feeds): &(usize, u8, Vec<usize>)) -> bool {
    let ps = gx.pipelines();
    let Some(((k, j), ip)) = e.checked_sub(ps.len()).and_then(|i| ps.inners().get(i).copied()) else { return false };
    let Some(Op::Join(JoinDesc::Nest { facts, jointype, unique, qual, .. })) = ps[k].ops.get(j) else { return false };
    let Some(r) = crate::rerun::sets::nest_set_index(gx, k, j).map(|i| &gx.graph.reruns[i]) else { return false };
    let member = |q: usize| r.pipelines.contains(&(q as u16));
    let reads = |f: usize| match ps[f].source {
        Source::Breaker { slot } => Some(slot),
        _ => None,
    };
    // PG reads every arm whole at every rescan, the arms one after another: the refill's calls and errors are PG's
    let whole = || {
        let first = |m: usize| feeds.iter().copied().filter(|&q| q > m).min().filter(|&q| reads(q).is_some_and(|s| gx.life_of(q, s).is_some_and(|l| l.0 as usize == m)));
        matches!(jointype, JoinType::JOIN_INNER | JoinType::JOIN_LEFT)
            && !unique
            && k == gx.graph.root as usize
            && crate::effects::ops_raise_free(gx, &ip.ops[1..])
            && crate::effects::ops_raise_free(gx, &ps[k].ops[j + 1..])
            && qual.iter().all(|n| crate::effects::raise_free(gx, n))
            && r.pipelines.iter().map(|&m| m as usize).filter(|m| !feeds.contains(m)).all(|m| first(m).is_some_and(|q| matches!(gx.source_kind(q), KindId::RunsSort | KindId::RunsTopkRows)))
    };
    union_of(ip).is_some_and(|u| !u.1)
        && slot_has(&r.slots, *slot)
        && feeds.iter().any(|&f| reads(f).is_some())
        && feeds.iter().all(|&f| member(f) && reads(f).is_none_or(|s| gx.life_of(f, s).is_some_and(|l| member(l.0 as usize))))
        && (crate::effects::refill_quiet(facts.raise_free, facts.volatility) || whole())
}

/// D3782 (S2k): the settled union `(emit, slot, feeds)` a nest inner emits under a nest with no NestLoopParam, unpruned,
/// with an arm over a breaker (one S2b refuses, D2014; a union of scan arms keeps S2b's streamed form), that no rerun set
/// re-runs: none holds a feed, a pipeline feeding one (build_class `feed`: a Sort's or an aggregate's fill under a
/// DISTINCT arm, a join's build) or the slot. decompose leaves such a union's fills out of every set where a node
/// PostgreSQL keeps sits under an arm (rerun.rs `declare_breaker_rerun`: a Sort rewound under EXEC_FLAG_REWIND, which
/// ExecInitNestLoop gives an inner with no parameters), so S2r never refills it. PostgreSQL's Append re-reads every arm
/// at each rescan over the same rows in the same order (no parameter changes, one snapshot, nothing acts: F1's same
/// rows); the feeds fill the union's record once, in program order ahead of the owner (the eager form, `end`), and the
/// inner reads it from its start at every re-open (ops/material/lower.rs `eager`, its position a per-open word), as a
/// plan Materialize over the Append would. The record is the Material eager form's spool (rerun/spool/rec.rs: its
/// regions the statement's grant road, dropped at the execution's end). Ahead of PostgreSQL's first rescan: `admit`
/// takes it in a statement that acts on nothing when no arm can raise or call (`kept_quiet`), else under the unchecked
/// licence, which a user or VOLATILE call or an acting statement denies (LOUD_UNION, by name).
pub(crate) fn kept(gx: &Gx<'_, '_>, (e, slot, feeds): &(usize, u8, Vec<usize>)) -> bool {
    let ps = gx.pipelines();
    let Some(((k, j), ip)) = e.checked_sub(ps.len()).and_then(|i| ps.inners().get(i).copied()) else { return false };
    let Some(Op::Join(JoinDesc::Nest { params, .. })) = ps[k].ops.get(j) else { return false };
    // a CteScan arm S2b streams (D3789, inner_arms.rs `cte_streams`) streams as a scan arm does: read at the open PG's
    // Append reaches, never filling its CTE ahead of an outer with no row
    let breaker_arm = feeds.iter().any(|&f| matches!(ps[f].source, Source::Breaker { .. }) && !crate::rerun::inner_arms::cte_streams(gx, f));
    if params.iter().next().is_some() || !union_of(ip).is_some_and(|u| !u.1) || !breaker_arm {
        return false;
    }
    let mut parts: PgVec<'_, usize> = mcx::to_vec_in(&feeds[..], gx.mcx);
    for &f in feeds.iter() {
        let src = if let Source::Breaker { slot } = ps[f].source { Some(slot) } else { None };
        parts.extend_in(crate::ops::hashjoin::build_class::feed(gx, f, ps[f].desc.consumes.iter().copied().chain(src)));
    }
    gx.graph.reruns.iter().all(|r| !slot_has(&r.slots, *slot) && !parts.iter().any(|&q| r.pipelines.contains(&(q as u16))))
}

/// D3782 (S2k): the union pipeline `k` feeds or emits when it is a kept one (`kept`).
pub(crate) fn kept_of(gx: &Gx<'_, '_>, k: usize) -> Option<(usize, u8, Vec<usize>)> {
    crate::pipes::union_in_inner(gx, k).filter(|u| kept(gx, u))
}

/// D3773 (SQLAlchemy's polymorphic_union: `companies LEFT OUTER JOIN (people JOIN engineers UNION ALL people JOIN
/// managers)`, a Nested Loop Left Join over an Append of two Hash Joins): the settled union `(emit, slot, feeds)` an
/// unparameterized nest's inner emits (no parameter and no rerun set: nothing in it reads the outer row), the nest in a
/// graph pipeline, its feeds graph pipelines before it, no set re-running the union (`held`: a SubPlan's or LATERAL's
/// run would append to the record again), pruning nothing, an arm a join S2b does not stream (D2015), and every arm
/// D1486's (union_join_arms.rs `arms_take`: quiet scans, joins of quiet scans), is filled once, its feeds run in arm order
/// before the nest's pipeline, and read as the eager form's record from its start at every re-open. PostgreSQL re-runs
/// the Append at each rescan, each Hash Join keeping its table (ExecReScanHashJoin: no parameter changed) and re-reading
/// its outer: the same rows in the same order under the statement's snapshot, and nothing in them raises or calls, so
/// running them once, or where PostgreSQL never runs them (an empty outer, a SEMI match), shows nothing.
pub(crate) fn filled_once(gx: &Gx<'_, '_>, (e, slot, feeds): &(usize, u8, Vec<usize>)) -> bool {
    let ps = gx.pipelines();
    let Some(((k, j), ip)) = e.checked_sub(ps.len()).and_then(|i| ps.inners().get(i).copied()) else { return false };
    let Some(Op::Join(JoinDesc::Nest { params, .. })) = ps[k].ops.get(j) else { return false };
    let probes = |f: usize| ps[f].ops.iter().any(|o| matches!(o, Op::Join(JoinDesc::Probe { .. })));
    k < ps.len()
        && params.is_nil()
        && crate::rerun::sets::nest_set_index(gx, k, j).is_none()
        && union_of(ip).is_some_and(|u| !u.1)
        && feeds.iter().all(|&f| f < k)
        && feeds.iter().any(|&f| probes(f))
        && !held(gx, *slot, *e, feeds)
        && crate::rerun::union_join_arms::arms_take(gx, feeds).is_some()
}

/// S2r: the union pipeline `k` feeds or emits, when the nest's set refills it (`refilled`, or `member_refill`).
pub(crate) fn refill_of(gx: &Gx<'_, '_>, k: usize) -> Option<(usize, u8, Vec<usize>)> {
    crate::pipes::union_in_inner(gx, k).filter(|u| refilled(gx, u)).or_else(|| member_refill(gx, k))
}

/// S2r's member form (D2013 slice 1, regress join#809 / #810): the settled union whose emit is a member pipeline of a
/// nest's rerun set other than its inner (a hash build's Append in the nest inner's join), its feeds members too, is
/// refilled by the set at every re-open and read as the eager form's record. The set holds the slot; the arms and the
/// emit's ops are quiet (`quiet_arm_in` over the nest's parameters: a scan, a Result or VALUES under quiet filters and
/// projections), so a build PostgreSQL skips (an empty probe side) or rewinds shows nothing;
/// a pruning union has one feed per child and quiet steps (`quiet_prune`): the pruning PostgreSQL's ExecReScanAppend
/// redoes per re-open shows nothing, so every arm runs (`step`), and one it prunes passes no row by its own filters.
fn member_refill(gx: &Gx<'_, '_>, k: usize) -> Option<(usize, u8, Vec<usize>)> {
    let ps = gx.pipelines();
    let p = ps.get(k).filter(|_| k < ps.len())?;
    let slot = union_of(p).map(|u| u.0).or_else(|| feed_of(p))?;
    let emit = (k..ps.len()).find(|&q| union_of(&ps[q]).is_some_and(|u| u.0 == slot))?;
    let feeds = crate::rerun::union_join_arms::feeds(gx, emit, slot);
    if feeds.is_empty() || (emit != k && !feeds.contains(&k)) {
        return None;
    }
    let member = |r: &gravity_decompose::graph::Rerun, q: usize| r.pipelines.contains(&(q as u16));
    let r = gx.graph.reruns.iter().find(|r| r.plan_id == 0 && slot_has(&r.slots, slot) && member(r, emit) && feeds.iter().all(|&f| member(r, f)))?;
    // the set is a nest's (its node a NestLoop), not a SubPlan's
    if !ps.inners().iter().any(|&((n, j), _)| matches!(ps[n].ops.get(j), Some(Op::Join(JoinDesc::Nest { .. }))) && crate::rerun::inner::op_node(&ps[n], j) == r.node_id) {
        return None;
    }
    // the arms and the emit's own ops show nothing (`quiet_arm_in`), so a build PostgreSQL skips shows nothing either
    let emit_quiet = || {
        ps[emit].ops.iter().all(|op| match op {
            Op::Append(AppendDesc::Union { .. }) => true,
            Op::Filter(n) => quiet_in(gx, *n, true),
            Op::Project(l) => l.iter().all(|n| quiet_in(gx, n, true)),
            Op::Join(JoinDesc::Build { keys, .. }) => quiet_build(gx, emit, keys),
            _ => false,
        })
    };
    let quiet = feeds.iter().all(|&f| quiet_arm_in(gx, &ps[f], true, false)) && emit_quiet();
    let pruned = |(d, children): (&PruneDesc<'_>, u16)| feeds.len() == children as usize && quiet_prune(gx, d);
    (quiet && union_prune(&ps[emit]).is_none_or(pruned)).then_some((emit, slot, feeds))
}

/// Pipeline `k` is a feed of a union its Material's reader pulls (`pulled`) or its emit pulls (`arms_read`): graph.rs names
/// its source nest.loop's `Empty`.
pub(crate) fn pulled_arm(gx: &Gx<'_, '_>, k: usize) -> bool {
    let ps = gx.pipelines();
    let Some(slot) = ps.get(k).filter(|_| k < ps.len()).and_then(feed_of) else { return false };
    (k..ps.len()).find(|&q| union_of(&ps[q]).is_some_and(|u| u.0 == slot)).is_some_and(|e| pulled(gx, e) || arms_read(gx, e))
}

/// D3377 road (3): under a named cursor's portal (`graph.partial`: each FETCH takes a counted part and may stop there), a
/// settled union the sink rule (a) or rule C (b) alone admits (`admit`'s `full` by `union_reads::sink_read` or `capped`:
/// its record runs every arm before the first row, PostgreSQL's run only for a reader that takes every row, so the
/// program was marked `Px::whole_read`) is read as PostgreSQL's Append hands rows on. The emit's source makes the arms
/// (rerun/inner_arms.rs `pulled`, S1u's `Arms`): arm 0 to its end, then arm 1, each opened at its first pull, a row made
/// only when the emit asks for one, so an arm's call or error happens where PostgreSQL's does. The feeds read nothing
/// (`pulled_arm`) and leave at their open (`end`); no mark. The emit is the root (each FETCH resumes it), fills the store
/// of a CTE the root reads (the CTE's pull runs it row by row: ops/ctescan, D3450), or fills the partition buffer of rule
/// C's top window, whose spool pulls it (ops/window/lower.rs `streaming_fill`; the arms' row caps dropped: inner_arms.rs
/// `arm_cap`). Every arm is `Arms`' (`inner_arms::arm_fits`: a heap range, a btree walk or a btree-leaved bitmap scan, a
/// Result or VALUES, under filters, gates, projections and target-list set functions; one column type list), the
/// pruning none or an initial one (each child's skip bit read before its open, `pull_skips`), no rerun set re-runs the
/// emit or the window, and nothing in the statement writes or locks rows. Any other union keeps the record and the mark.
/// Decided from the graph alone (graph.rs `source_kind` asks it): `admit`'s rule A is read in its widest form (every
/// LIMIT's check held), so a union it may take keeps its form.
pub(crate) fn arms_read(gx: &Gx<'_, '_>, e: usize) -> bool {
    let ps = gx.pipelines();
    if !gx.graph.partial || e >= ps.len() || crate::pipes::inner_union(gx, e).is_some() || pulled(gx, e) || refill_of(gx, e).is_some() {
        return false;
    }
    let Some((slot, _)) = union_of(&ps[e]) else { return false };
    let feeds = crate::rerun::union_join_arms::feeds(gx, e, slot);
    let rerun = gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(e as u16)));
    if feeds.is_empty() || rerun || held(gx, slot, e, &feeds) || union_prune(&ps[e]).is_some_and(|(d, children)| d.exec || feeds.len() != children as usize) {
        return false;
    }
    // the emit's rows leave as they are made: at the root, through a CTE's store its pull fills, or (b) into the partition
    // buffer of rule C's top window, whose fill the window's spool pulls (ops/window/lower.rs `streaming_fill`)
    let window = reader(gx, e).filter(|&c| matches!(ps[c].ops.first(), Some(Op::Window(_))) && !gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(c as u16))));
    let (pulls, rule) = match gx.end_kind(e) {
        EndKind::Sink => (e == gx.graph.root as usize, crate::rerun::union_reads::sink_read(gx, e)),
        EndKind::Breaker(BreakerKind::Materialize) if gx.spool_kind(e) == KindId::NestCte => (true, crate::rerun::union_reads::sink_read(gx, e)),
        EndKind::Breaker(BreakerKind::Materialize) => (window.is_some(), capped(gx, e, &feeds)),
        _ => (false, false),
    };
    let others = unconditional(gx, e, &mut |_: Ask<'_>| true) || hashed(gx, e) || crate::rerun::union_join_arms::arms_take(gx, &feeds).is_some()
        || crate::rerun::union_reads::spool_read(gx, e) || crate::rerun::spool::arm_fill::whole_union(gx, e);
    if !pulls || others || !rule {
        return false;
    }
    let mut types = feeds.iter().map(|&f| crate::rerun::inner_arms::arm_fits(gx, f));
    let Some(Some(first)) = types.next() else { return false };
    types.all(|t| t.as_ref() == Some(&first))
}

/// Feed `f`'s union fills the partition buffer of a top window that capped every arm (decompose `WindowDesc::arms_capped`):
/// a constant Limit in it is that cap (`cap_union`'s test, whichever form makes the arm).
pub(crate) fn capped_feed(gx: &Gx<'_, '_>, f: usize) -> bool {
    let ps = gx.pipelines();
    let Some(slot) = ps.get(f).filter(|_| f < ps.len()).and_then(feed_of) else { return false };
    let Some(e) = (f..ps.len()).find(|&q| union_of(&ps[q]).is_some_and(|u| u.0 == slot)) else { return false };
    reader(gx, e).and_then(|c| ps[c].ops.first()).is_some_and(|op| matches!(op, Op::Window(d) if d.arms_capped))
}

/// G2(e), S1u: the skip bit of each child of emit `e`'s initial pruning (append/prune.rs `declare`, in arm order over
/// `feeds`), none where the union prunes nothing; the Material's reader reads it before the arm's open.
pub(crate) fn pull_skips<'g>(px: &mut Px<'g, '_>, e: usize, feeds: &[usize], node: i32) -> PgVec<'g, Option<(u16, u32)>> {
    let (gx, mcx) = (px.gx, px.mcx());
    // D3612: a pruning decided at the first pull declares no skip bits (rerun/inner_arms.rs `pulled`'s mask)
    let prune = union_prune(&gx.pipelines()[e]).filter(|_| !crate::rerun::spool::form::pull_late(gx, e));
    feeds.iter().enumerate().map(|(i, &f)| prune.map(|(d, n)| crate::ops::append::prune::declare(px, f, i as u16, n, d, node))).collect_in(mcx)
}

/// The Feed or Union op: the row as it is (the feed's end appends it, the emit's ops read the record's).
struct Pass;

impl StepMech for Pass {
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        next(b, env, row)
    }
}

pub(crate) fn step<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    if crate::pipes::inner_union(px.gx, k).is_none() {
        let slot = match &px.gx.pipelines()[k].ops[j] {
            Op::Append(AppendDesc::Feed { slot, .. } | AppendDesc::Union { slot, .. }) => Some(*slot),
            _ => None,
        };
        let (gx, arena, node) = (px.gx, px.arena, crate::rerun::inner::op_node(&px.gx.pipelines()[k], j)); // the Append's node, as src.append's arm refuses
        let (emit, feeds, _) = admit(gx, arena, k, slot, node, &mut |a| answer(px, a));
        if pulled(gx, emit) || arms_read(gx, emit) {
            return mcx::pg_box_dyn!(px.mcx(), Pass => dyn StepMech + 'g); // S1u: the reader makes the arm again, its skip bit with it; road (3): the emit
        }
        // S2r's member form (`member_refill`): its quiet pruning shows nothing, so every arm runs and one PostgreSQL prunes
        // passes no row by its own filters (the planner keeps the pruning quals in each child), as a quiet S2b union's
        if refill_of(gx, k).is_some() {
            return mcx::pg_box_dyn!(px.mcx(), Pass => dyn StepMech + 'g);
        }
        // a feed of a union PostgreSQL prunes at its executor start: the feed is its child `i` (`admit`: one feed per
        // child, in arm order), so it takes that child's skip bit as src.append's arm marker does (append/prune.rs
        // declares it; exec's `pruned_whole` then runs the pipeline empty, never walking the arm nor calling its filters)
        let feed = matches!(gx.pipelines()[k].ops[j], Op::Append(AppendDesc::Feed { .. }));
        if let (true, Some((d, children)), Some(i)) = (feed, union_prune(&gx.pipelines()[emit]), feeds.iter().position(|&f| f == k)) {
            let skip = crate::ops::append::prune::declare(px, k, i as u16, children, d, node);
            return mcx::pg_box_dyn!(px.mcx(), Skip { skip, pass: core::cell::Cell::new(None), node } => dyn StepMech + 'g);
        }
    }
    mcx::pg_box_dyn!(px.mcx(), Pass => dyn StepMech + 'g)
}

/// A pruning union's feed: its child's skip bit `(word, bit)`, read once per call (src.append's arm marker's form): a
/// pruned child passes no row.
struct Skip {
    skip: (u16, u32),
    pass: core::cell::Cell<Option<gravity_ir::Value>>,
    node: i32,
}

impl StepMech for Skip {
    fn prologue(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _x: &crate::frame::stop::Exits) {
        use gravity_ir::{IntBinOp, IntCond, Type};
        let (word, bit) = self.skip;
        let w = b.load(Type::I64, env.params, word as i32 * 8);
        let sh = b.iconst(Type::I64, bit as i64);
        let s = b.int_bin(IntBinOp::Ushr, w, sh);
        let s1 = b.int_bin(IntBinOp::And, s, env.one);
        self.pass.set(Some(b.icmp(IntCond::Eq, s1, env.zero)));
    }

    fn block(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _w: &crate::frame::frame::WordWin, _base: gravity_ir::Value, m: crate::frame::row::Mask) -> crate::frame::row::Mask {
        match self.pass.get() {
            Some(p) => crate::frame::row::Mask(b.select(p, m.0, env.zero)),
            None => m,
        }
    }

    fn block_reads(&self, _env: &Env, _w: &crate::frame::frame::WordWin) -> u64 {
        0
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        let Some(pass) = self.pass.get() else { crate::refuse!(self.node, Reason::Bug { payload: "a pruning union's feed lowered before its prologue" }) };
        let types: PgVec<'_, gravity_ir::Type> = row.carry.params().iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
        let l_join = b.create_block(&types);
        let l_yes = b.create_block(&[]);
        b.cond_br(pass, (l_yes, &[]), (l_join, &row.carry.params()));
        b.switch_to(l_yes);
        let c = next(b, env, row);
        b.br(l_join, &c.params());
        b.switch_to(l_join);
        Carry::from_params(b.block_params(l_join), env.mcx)
    }
}

/// Whether Limit op `j` of pipeline `q`, at a zero count or a negative bound, stops each of `ps` before it pulls a row: the
/// feeder makes its check there (result/gate.rs `checks_limit`, D1716), as PG's Limit evaluates its bounds first. Also
/// setop's (ops/setop/recursive.rs `zero_checked`: a recursive union read whole under a LIMIT that may count 0, K11.rec.l0,
/// or under a one-time gate, result/gate.rs `checks_gate`).
pub(crate) fn stops(px: &mut Px<'_, '_>, ps: &[usize], q: usize, j: usize) -> bool {
    let gate = matches!(px.gx.pipelines()[q].ops[j], Op::Gate(_));
    ps.iter().all(|&x| if gate { crate::ops::result::gate::checks_gate(px, x, q, j) } else { crate::ops::result::gate::checks_limit(px, x, q, j) })
}

/// The union pipeline `k` belongs to through `slot`: its emit, feeds in arm order, else the refusal's name at `node`. The
/// slot is the role's (D2341, D2368): a feed's end its Feed's, the emit's source its Union's; a union settled inside
/// another union's arm has an emit that feeds the outer slot too, so its own pipeline is in two unions. The flag: admitted by
/// rule C (`capped`), rule W (`full_read`) or the sink rule (`sink_read`) alone, whose program must run to its end.
fn admit(gx: &Gx<'_, '_>, arena: ArenaRef, k: usize, slot: Option<u8>, node: i32, stops: Stops<'_>) -> (usize, Vec<usize>, bool) {
    let ps = gx.pipelines();
    let Some(slot) = slot else { shape(node, KEPT_UNION) };
    if let Some((emit, _, feeds)) = refill_of(gx, k) {
        return (emit, feeds, false); // S2r: the nest's set refills it at every re-open
    }
    // D3782 (S2k): filled once ahead of the owner, read from its start at every re-open (`kept`); arms that may raise run
    // ahead of PostgreSQL's first rescan under the unchecked licence (tagged), one that calls a user or VOLATILE function,
    // or any arm in a statement that acts, refuses
    if let Some((emit, _, feeds)) = kept_of(gx, k) {
        if gx.graph.acts || !crate::effects::visible::nestloop::kept_quiet(gx, &feeds) && !stops(Ask::Ahead(&feeds)) {
            shape(node, LOUD_UNION);
        }
        return (emit, feeds, false);
    }
    // D3773: an unparameterized nest inner's union of quiet arms, filled once (`filled_once`), GUARD-G1's USER_FN first
    if let Some((emit, _, feeds)) = crate::pipes::union_in_inner(gx, k).filter(|u| filled_once(gx, u)) {
        if crate::rerun::union_join_arms::arms(gx, &feeds, node) {
            stops(Ask::Quiet(&feeds));
        }
        return (emit, feeds, false);
    }
    let n = gx.graph.pipelines.len();
    // the emit: `k`, or the first later pipeline whose Union op reads the slot (a slot is reused after its life, so the
    // feeds are those after the slot's previous emit); an inner's union is S2b's
    let reads = |q: usize| union_of(&ps[q]).is_some_and(|u| u.0 == slot);
    let Some(emit) = (k..ps.len()).find(|&q| reads(q)) else { shape(node, KEPT_UNION) };
    if emit >= n {
        shape(node, KEPT_UNION);
    }
    let feeds = crate::rerun::union_join_arms::feeds(gx, emit, slot);
    // G2(e), S1u: the Material's reader pulls the arms as PostgreSQL's Material pulls its Append (no arm runs ahead)
    if pulled(gx, emit) {
        return (emit, feeds, false);
    }
    // an initial pruning (PostgreSQL's ExecInitAppend, decided once per execution) leaves whole children out: with one
    // feed per child each feed takes its child's skip bit (`step`), so a pruned one runs empty as PostgreSQL never opens
    // it; a run-time (exec) pruning decided per call, or feeds that are not the children one to one, refuse by name
    // (one over InitPlans' outputs only is decided once per execution too: `init_pruned`). D3614: one a rerun set decides
    // at every run of every feed (append/prune.rs `exec_per_rerun`: an InitPlan re-run per SubPlan call over the outer
    // row's column, as PostgreSQL's ExecReScanAppend re-prunes after ExecReScanSetParamPlan) is the set's, refilled whole
    // below (`held`, `refills_whole`) or refused there
    let per_rerun = |d: &PruneDesc<'_>| !feeds.is_empty() && feeds.iter().all(|&f| crate::ops::append::prune::exec_per_rerun(gx, f, d)) && held(gx, slot, emit, &feeds) && refills_whole(gx, slot, emit, &feeds) && crate::effects::visible::nestloop::quiet_prune_bound(gx, d, &|id| crate::effects::visible::nestloop::refill_bound(gx, id));
    if union_prune(&ps[emit]).is_some_and(|(d, children)| (d.exec && !crate::rerun::union_reads::init_pruned(gx, d, &feeds) && !per_rerun(d)) || feeds.len() != children as usize) {
        shape(node, LOUD_UNION);
    }
    if feeds.is_empty() {
        shape(node, KEPT_UNION);
    }
    // a union a rerun set re-runs (`holders`): S2q refills it at every run (`refills_whole`, `end`), else PG re-runs arms we
    // would not
    if held(gx, slot, emit, &feeds) {
        // D3373: arms PostgreSQL's Append may stop before that may raise run ahead unchecked (tagged); one that calls a
        // user or VOLATILE function, or a statement that acts, refuses by its own name
        match refill_arms(gx, slot, emit, &feeds, &|n| !refill_calls(gx, n)) {
            Some(ahead) => {
                if !ahead.is_empty() && !stops(Ask::Ahead(&ahead)) {
                    shape(node, HELD_UNION_AHEAD);
                }
            }
            None if refill_arms(gx, slot, emit, &feeds, &|_| true).is_some() => shape(node, HELD_UNION_CALLS),
            None => shape(node, KEPT_UNION),
        }
        return (emit, feeds, false);
    }
    // rules A, B and the arms rule first: a union they admit is served to any reader; rule C (`capped`) and rule W
    // (`full_read`) only to one that takes every row
    // Construct C2: a union the arms rule takes (no earlier rule did) fills ahead under the no-effects licence (`Ask::Quiet`)
    let mut by_arms = false;
    let any = unconditional(gx, emit, stops) || hashed(gx, emit) || drained(gx, arena, emit, &feeds) || {
        by_arms = crate::rerun::union_join_arms::arms(gx, &feeds, node);
        by_arms
    } || crate::rerun::union_reads::spool_read(gx, emit);
    if by_arms {
        stops(Ask::Quiet(&feeds));
    }
    let any = any || crate::rerun::spool::arm_fill::whole_union(gx, emit); // rowsecurity#343: run whole at the first arm's nest
    let any = any || crate::rerun::union_reads::every_read(gx, emit); // D3261: every CTE reader reads it whole
    let any = any || crate::rerun::union_reads::drained_write(gx, emit, &feeds); // D3499: a data-modifying CTE drains it
    let any = any || crate::rerun::union_reads::written_arms(gx, &feeds); // D4202: every arm reads a data-modifying CTE's store
    let full = !any && (capped(gx, emit, &feeds) || crate::rerun::union_reads::full_read(gx, emit) || crate::rerun::union_reads::sink_read(gx, emit));
    // `DECISIONS.md` 4 (D) b: no rule admits arms that may raise; where none acts or calls user code, they fill the union
    // ahead of PostgreSQL's pulls unchecked (`Px::ahead_unchecked`, tagged), served to any reader. D1486 (v0.4 refusals):
    // or the accepted class (`Ask::Accept`): arms whose only acts are extra call-local VOLATILE calls (AGENTS.md "Errors
    // do not constrain evaluation order": call counts free), the arms filled in PostgreSQL's arm order, so every row PG's
    // Append would hand on carries the value PG's call gives it and what PG never pulls is a gap
    if !any && !full && !stops(Ask::Ahead(&feeds)) && !stops(Ask::Accept(&feeds)) {
        shape(node, LOUD_UNION);
    }
    (emit, feeds, full)
}

/// D1486 (material, psql's \d "Referenced by": confrelid IN (SELECT pg_partition_ancestors(..) UNION ALL VALUES (..))): the
/// emit is the build of a hash join PostgreSQL runs on its outer-first path (nodeHashjoin.c HJ_BUILD_HASHTABLE fetches
/// the first outer row, then MultiExecHash reads the Append to its end, arm after arm, or never for an empty outer). The
/// emit's own ops and its hash keys are quiet (PG computes them row by row between the arms' rows), so the arms filled
/// whole in arm order before the build reads the record are PG's run whenever the build runs. Whether and when it runs,
/// with the arms' calls and errors, is the probe's (hashjoin/probe.rs over the side of the build and every feed,
/// build_class.rs `feed`): D824's lazy build defers the arms with the emit to the first row that reaches the probe for a
/// calling side, the recheck licence a raising one (re-run in plan order on an error before any row left), else the
/// probe refuses by name. Never op 14's eager build with its held error (build_class.rs `defers`, which never holds a
/// build whose arm may raise or call: `loud_arms`): it holds the build's own error, not a feed's (m8_d1486_joinarms: an
/// arm's division by zero under an outer empty at run time raised there).
fn hashed(gx: &Gx<'_, '_>, emit: usize) -> bool {
    let ps = gx.pipelines();
    let Some((Op::Join(JoinDesc::Build { keys, .. }), body)) = ps[emit].ops.split_last() else { return false };
    let quiet = matches!(gx.end_kind(emit), crate::graph::EndKind::Breaker(BreakerKind::HashBuild))
        && keys.iter().all(|x| quiet_in(gx, x, false))
        && body.iter().all(|op| match op {
            Op::Append(AppendDesc::Union { .. }) => true,
            Op::Filter(x) => quiet_in(gx, *x, false),
            Op::Project(l) => l.iter().all(|x| quiet_in(gx, x, false)),
            _ => false,
        });
    let Some(q) = reader(gx, emit).or_else(|| crate::rerun::union_build::drained_reader(gx, emit)).filter(|_| quiet) else { return false };
    // its one reader probes it (the probe's slot holds the emit's life) on PG's outer-first path (build_class.rs
    // `outer_first`'s join types)
    let life = Some(LifeId(emit as u16));
    let probe = |op: &Op<'_>| match op {
        Op::Join(JoinDesc::Probe { slot, outer_first: true, catalog: false, jointype, .. }) if gx.life_of(q, *slot) == life => {
            matches!(jointype, JoinType::JOIN_INNER | JoinType::JOIN_LEFT | JoinType::JOIN_SEMI | JoinType::JOIN_ANTI | JoinType::JOIN_RIGHT_SEMI)
        }
        Op::Join(JoinDesc::Probe { slot, outer_first: false, catalog: false, .. }) => gx.life_of(q, *slot) == life,
        _ => false,
    };
    // or (with#112) PG's build-first path where the join is always called (union_build.rs)
    let first = |j: usize| matches!(ps[q].ops[j], Op::Join(JoinDesc::Probe { outer_first: true, .. })) || crate::rerun::union_build::build_first(gx, q, j);
    ps[q].ops.iter().position(probe).is_some_and(|j| first(j) && !crate::ops::hashjoin::build_class::defers(gx, emit, q, j))
}

/// The rerun sets that re-run the settled union `(slot, emit, feeds)`: a set with the emit or a feed as a member, or one
/// holding the union's slot in the union's life (a member between the first feed and the emit). Decompose reuses a slot's
/// number after its last reader, so a set holding the number in another life (a correlated SubPlan's own union,
/// updatable_views#858) is not this union's.
fn holders<'a>(gx: &'a Gx<'_, '_>, slot: u8, emit: usize, feeds: &'a [usize]) -> impl Iterator<Item = &'a gravity_decompose::graph::Rerun> + 'a {
    let life = move |x: u16| feeds.first().is_some_and(|&f| (f..=emit).contains(&(x as usize)));
    let member = move |x: u16| x as usize == emit || feeds.contains(&(x as usize));
    gx.graph.reruns.iter().filter(move |r| slot_has(&r.slots, slot) && r.pipelines.iter().any(|&x| life(x)) || r.pipelines.iter().any(|&x| member(x)))
}

/// Whether a rerun set re-runs the settled union `(slot, emit, feeds)` (`holders`).
fn held(gx: &Gx<'_, '_>, slot: u8, emit: usize, feeds: &[usize]) -> bool {
    holders(gx, slot, emit, feeds).next().is_some()
}

/// S2q (D1483, heldout rnd-0645's class: a LATERAL or SubPlan `Limit -> Sort -> Append` of parameterized arms over a
/// partitioned table): a settled union a rerun set re-runs is refilled at every run of the set, the first feed's open
/// emptying the record (`end`, as S2r and D1107's eager refill), so the emit reads the rows PG's Append yields at that
/// rescan, in arm order. Taken when every set that re-runs the union runs the emit and every feed (no run refills part of
/// the record), every arm is quiet (`quiet_arm_in` with `exec`: a scan, an index or bitmap walk, a Result or VALUES under
/// quiet filters and projections, a parameter the nest or SubPlan binds read quietly) and the emit's own ops are the Union,
/// quiet filters and projections and its sort: then an arm PG does not run at a rescan (a LIMIT met, a SEMI match, an
/// unfetched inner) shows nothing, and the rows are the same. D3373: an arm whose filters, gates and projections may raise
/// but call no user or VOLATILE function (`refill_calls`) is taken too, outside a whole sort as well, ahead of
/// PostgreSQL's pulls: `admit` asks the unchecked licence for it (`refill_arms`).
fn refills_whole(gx: &Gx<'_, '_>, slot: u8, emit: usize, feeds: &[usize]) -> bool {
    refill_arms(gx, slot, emit, feeds, &|n| !refill_calls(gx, n)).is_some()
}

/// `refills_whole`, arm by arm: `None` when S2q's refill cannot take the union `(slot, emit, feeds)`; else the feeds it
/// takes only ahead of PostgreSQL's pulls (D3373): arms whose filters, gates and projections (read by `body`) may raise
/// and that neither run under a whole sort (`raising`, PostgreSQL's Sort reads them to their end too) nor are quiet or
/// joined. PostgreSQL's Append under a SEMI match, an IN's first equal row or a LIMIT met may stop before such an arm,
/// which the refill runs whole at every run of the set: its rows are the same, and an error it raises where PostgreSQL
/// returns rows is the unchecked licence's tagged one (`DECISIONS.md` 4 (D) b, `admit`'s `Ask::Ahead`), denied in a
/// statement that acts. Empty when every arm is taken as PostgreSQL runs it.
fn refill_arms<'g>(gx: &Gx<'g, '_>, slot: u8, emit: usize, feeds: &[usize], body: &dyn Fn(Node<'_>) -> bool) -> Option<PgVec<'g, usize>> {
    let ps = gx.pipelines();
    let all = |r: &gravity_decompose::graph::Rerun| std::iter::once(emit).chain(feeds.iter().copied()).all(|q| r.pipelines.contains(&(q as u16)));
    // D3133: a hash build over the union (a SubPlan's or LATERAL inner's Hash over an Append, PostgreSQL rebuilding it at
    // every rescan) is the emit's last op when it shows nothing (`quiet_build`); so is the plan Material over it that a
    // nest in the set reads (the generic plan's Nested Loop Semi Join over Materialize -> Append) when the spool takes
    // D1107's eager refill (lifetime/material.rs `spool_fill`'s eager form, a member of the set: form.rs
    // `refill_member`): ExecReScanMaterial drops the store at every call (the child's changed parameter) and the nest's
    // rescans inside one call rewind it, as each run's refill and the reads from 0 do. D3879 (planagg's min / max of a
    // constant in a LATERAL or SubPlan over a partitioned table: `Limit -> Result -> Append` of parameterized arms): a
    // Limit of a constant count of one or more (`const_bound`: no bound to evaluate, no WITH TIES) in the emit reads the
    // refilled record from its start, its counters per open (ops/limit's scope), so each run takes the first rows in arm
    // order, PostgreSQL's Limit's rows at that rescan; the arms PostgreSQL's Append never reaches past it are quiet (exact)
    // or run under the unchecked licence (`ahead`), as for a LIMIT above the emit
    let emit_quiet = ps[emit].ops.iter().all(|op| crate::ops::limit::lower::const_bound(op) || match op {
        Op::Append(AppendDesc::Union { .. }) | Op::Sort(_) => true,
        Op::Filter(x) => quiet_in(gx, *x, true),
        Op::Project(l) => l.iter().all(|x| quiet_in(gx, x, true)),
        Op::Join(JoinDesc::Build { keys, .. }) => quiet_build(gx, emit, keys),
        Op::Join(JoinDesc::Spool) => matches!(crate::lifetime::material::spool_fill(gx, emit), Ok((_, false))),
        // D3614: a nest over a plan Material the same sets refill (a correlated SubPlan's Nested Loop over two pruned
        // unions), the Material in D1107's eager refill
        Op::Join(JoinDesc::Nest { .. }) => crate::effects::visible::nestloop::refill_nest(gx, emit, op).is_some_and(|f| matches!(crate::lifetime::material::spool_fill(gx, f), Ok((_, false)))),
        _ => false,
    });
    // no row cap in an arm (the only Limit `quiet_arm_in` takes): a capped union's count words are the union's, counted
    // across the feeds (`cap_union`), and a set's run does not re-arm them (D2121's class)
    let uncapped = |f: usize| !ps[f].ops.iter().any(crate::ops::limit::lower::const_bound);
    // D1483 (dict S2q): an emit whose full Sort reads its whole input before its first row (no presorted prefix, no walk)
    // over a Union that prunes nothing at run time reads every arm to its end at each rescan, as PostgreSQL's Sort reads
    // its Append: an arm whose filters, gates and projections may raise but call no user or VOLATILE function (and hold no
    // node the side class cannot read) raises where PostgreSQL's does, in arm order, before any row of that rescan leaves
    let whole = ps[emit].ops.iter().any(|op| matches!(op, Op::Sort(SortDesc { presorted: 0, form: SortForm::Runs | SortForm::TopK { .. }, .. })))
        && !ps[emit].ops.iter().any(|op| matches!(op, Op::Append(AppendDesc::Union { prune: Some(_), .. })));
    let raising = |f: usize| whole && crate::effects::visible::nestloop::arm_in(gx, &ps[f], true, true, &|n| !refill_calls(gx, n));
    // D1483: an arm that is a hash join of quiet parameterized scans (`joined_exec`) whose builds every set re-running the
    // union runs too, so each run rebuilds them, as PostgreSQL's rescan of a Hash whose parameter changed does (one whose
    // parameter did not change keeps its table there: the same entries)
    let joined = |f: usize| crate::rerun::union_join_arms::joined_exec(gx, f);
    let builds = |f: usize| crate::ops::hashjoin::build_class::feed(gx, f, ps[f].desc.consumes.iter().copied());
    let rebuilt = |r: &gravity_decompose::graph::Rerun| feeds.iter().filter(|&&f| joined(f)).all(|&f| builds(f).iter().all(|&b| r.pipelines.contains(&(b as u16))));
    // D3614: an arm may read an unseen InitPlan's output (nestloop.rs `refill_bound`), the set's run re-arming it as PostgreSQL's
    // ExecReScanSetParamPlan does at each call of the SubPlan holding it
    let quiet = |f: usize| crate::effects::visible::nestloop::quiet_arm_by(gx, &ps[f], true, true, &|id| crate::effects::visible::pgrc::init_unseen(gx, id));
    let exact = |f: usize| quiet(f) || raising(f) || joined(f);
    let ahead = |f: usize| crate::effects::visible::nestloop::arm_in(gx, &ps[f], true, true, body);
    let ok = holders(gx, slot, emit, feeds).all(|r| all(r) && rebuilt(r)) && emit_quiet && feeds.iter().all(|&f| (exact(f) || ahead(f)) && uncapped(f));
    ok.then(|| feeds.iter().copied().filter(|&f| !exact(f)).collect_in(gx.mcx)) // (none when every arm is exact)
}

/// S2q as a rerun set member (rerun/spool/form.rs `refill_member`, read by rerun/sets.rs `members_close`): pipeline `m` is
/// a feed of a settled union every set re-running it refills whole (`refills_whole`), so each run of the set empties the
/// record at the first feed's open and fills it again, and the emit, a member of the same sets, reads it from 0.
pub(crate) fn refed(gx: &Gx<'_, '_>, m: usize) -> bool {
    let (ps, n) = (gx.pipelines(), gx.graph.pipelines.len());
    let Some(slot) = ps.get(m).filter(|_| m < n).and_then(feed_of) else { return false };
    let Some(emit) = (m..n).find(|&q| union_of(&ps[q]).is_some_and(|u| u.0 == slot)) else { return false };
    let feeds = crate::rerun::union_join_arms::feeds(gx, emit, slot);
    feeds.contains(&m) && held(gx, slot, emit, &feeds) && refills_whole(gx, slot, emit, &feeds)
}

/// D3133: pipeline `e` is the emit of a settled union every set re-running it refills whole (`refills_whole`, S2q): a plan
/// Material's fill over it (rerun/spool/lazy.rs) runs in pipeline order at each run of the set, its feeds just before it.
pub(crate) fn refilled_emit(gx: &Gx<'_, '_>, e: usize) -> bool {
    let ps = gx.pipelines();
    let Some((slot, _)) = ps.get(e).filter(|_| e < gx.graph.pipelines.len()).and_then(union_of) else { return false };
    let feeds = crate::rerun::union_join_arms::feeds(gx, e, slot);
    !feeds.is_empty() && held(gx, slot, e, &feeds) && refills_whole(gx, slot, e, &feeds)
}

/// What `admit` asks of the lowering (`Px`): whether Limit op `j` of pipeline `q` stops each of the pipelines given before
/// it pulls a row (`Ask::Stops`: `stops`), or whether the feeds run ahead unchecked (`Ask::Ahead`: `Px::ahead_unchecked`).
type Stops<'s> = &'s mut dyn FnMut(Ask<'_>) -> bool;

enum Ask<'a> {
    Stops(&'a [usize], usize, usize),
    Ahead(&'a [usize]),
    /// D1486 (v0.4 refusals): the feeds run ahead under the accepted class (`Px::accept` over each feed's effects).
    Accept(&'a [usize]),
    /// Construct C2: the feeds fill the union ahead of PostgreSQL's pulls and are quiet by the arms rule
    /// (union_join_arms.rs `arms`): they hold the no-effects licence (`quiet_fill`). Always answered true.
    Quiet(&'a [usize]),
}

/// The lowering's answer to `admit`'s `Ask`.
fn answer(px: &mut Px<'_, '_>, a: Ask<'_>) -> bool {
    match a {
        Ask::Stops(ps, q, j) => stops(px, ps, q, j),
        Ask::Ahead(feeds) => feeds.iter().all(|&f| px.ahead_unchecked(f, gravity_decompose::effects::Effects::RAISES).is_some()),
        Ask::Accept(feeds) => {
            let (gx, arena) = (px.gx, px.arena);
            feeds.iter().all(|&f| px.accept(f, crate::effects::pipeline_effects(gx, arena, f, true)).is_some())
        }
        Ask::Quiet(feeds) => {
            quiet_fill(px, feeds);
            true
        }
    }
}

/// Construct C2: union feeds `feeds` are quiet by the arms rule, which reads each arm with what feeds it (a joined arm's
/// builds, a merged arm's children: union_join_arms.rs `pipe`, `merged`), so their fill ahead of PostgreSQL's pulls
/// departs from nothing: each arm and feeder holds the no-effects licence for the tracer (`Px::hold`; nothing while
/// the tracer is off).
fn quiet_fill(px: &mut Px<'_, '_>, feeds: &[usize]) {
    if let Some(l) = effects::unobservable(effects::Effects::NONE).filter(|_| crate::frame::program::stamps_on()) {
        hold_arms(px, feeds, &l);
    }
}

/// Construct C2 (b): licence `l`, asked with what union feeds `feeds` and their feeders run, stamped on each of them for
/// the tracer (`Px::hold`; build_class.rs `feed`).
fn hold_arms(px: &mut Px<'_, '_>, feeds: &[usize], l: &effects::Licence) {
    let ps = px.gx.pipelines();
    for &f in feeds {
        px.hold(f, l);
        let src = if let Source::Breaker { slot } = ps[f].source { Some(slot) } else { None };
        for q in crate::ops::hashjoin::build_class::feed(px.gx, f, ps[f].desc.consumes.iter().copied().chain(src)) {
            px.hold(q, l);
        }
    }
}

/// PostgreSQL reads every arm to its end whatever the arms hold, and nothing it computes over the union's rows interleaves
/// with them (rule A): the emit's own ops are quiet filters and projections and its sort, it ends in a sort,
/// top-k or a distinct with no aggregate (reads its whole input before its first row; an aggregate's transitions could
/// raise or call between two arms' rows, so a plain or hashed aggregate only when they cannot: `plain_group`), and each pipeline reading it in turn, one reader each up to the statement's root, reads at least one row
/// (filters, projections, a sort, a LIMIT: `bounds_fetch`) and ends in a breaker (a sort, top-k, aggregate or distinct)
/// or is the root; a gate, a join other than a quiet nest in the emit (`quiet_nest`), SubPlan, set function, write or lock
/// returns false. The fill runs at the emit's first pull (the feeds are pipelines of the execution, none at executor start).
/// D1486 (errorhold): a LIMIT whose count may be zero or a run-time one (and a top under it, which owes the next reader's
/// LIMIT: decompose's map_limit gives both its bounds) reads every arm or none when its check stops every pipeline below it
/// before any row (`stops`: the emit, every pipeline feeding it, every arm, and the readers on the way), as PG's Limit
/// evaluates its bounds before its first pull: a zero count reads nothing in both, any other count reads the top's whole
/// input. with#114: so does a quiet recursive seed's (`recursive_seed`).
fn unconditional(gx: &Gx<'_, '_>, emit: usize, stops: Stops<'_>) -> bool {
    crate::stack_check();
    if recursive_seed(gx, emit) {
        return true;
    }
    let ps = gx.pipelines();
    let mut owed = false;
    let quiet_ops = ps[emit].ops.iter().all(|op| match op {
        Op::Append(AppendDesc::Union { .. }) => true,
        Op::Sort(d) => sort_reads(d, &mut owed),
        Op::Filter(x) => quiet_in(gx, *x, false),
        Op::Project(l) => l.iter().all(|x| quiet_in(gx, x, false)),
        // D1486: or one whose per-row work may only raise (union_reads.rs `calm_nest`)
        Op::Join(JoinDesc::Nest { params, qual, inner, .. }) => quiet_nest(gx, Some(emit), params, qual, inner) || crate::rerun::union_reads::calm_nest(gx, emit, op),
        Op::Append(AppendDesc::Feed { .. }) => true,
        _ => false,
    });
    let Some(b) = iface::breaker_of(&gx.pipelines()[emit].desc) else { return false };
    // D1486 (window, heldout rnd-0846 / rnd-2444 / rnd-3420): a union settled inside an outer union's arm (its emit ends in
    // the outer union's Feed): the outer Append reads this arm to its end whenever it reads every arm to its end, so the
    // inner union is unconditional when the outer one is; what runs between two of its rows is its emit's quiet ops and
    // then the outer emit's, which the outer emit's own test reads (the same stops)
    if let Some(Op::Append(AppendDesc::Feed { slot, .. })) = ps[emit].ops.last() {
        let outer = (emit + 1..gx.graph.pipelines.len()).find(|&q| union_of(&ps[q]).is_some_and(|u| u.0 == *slot));
        return quiet_ops && !owed && b.kind == BreakerKind::Materialize && b.slot == *slot && outer.is_some_and(|o| unconditional(gx, o, stops));
    }
    // a distinct with no aggregate (DISTINCT, or a Unique whose key sort the group absorbed: decompose agg `map_set`,
    // D612) reads its whole input before its first row too, and its per-row work is the key image of columns the emit's
    // quiet projection made
    let distinct = b.kind == BreakerKind::Distinct && gx.graph.groups.iter().any(|g| g.producer == LifeId(emit as u16) && g.aggs.is_empty());
    // D1486 (window): a WindowAgg straight over the union, its partition buffer the emit's end, that PostgreSQL reads whole
    // before its first row (`whole_window`)
    let window = b.kind == BreakerKind::Materialize && reader(gx, emit).is_some_and(|c| whole_window(gx, c));
    // D1486 (window, heldout rnd-0098 / rnd-2022): a plain or hashed aggregate reads its whole input before its first row
    // too; what it runs between two input rows is its keys' images and its transitions, so they must raise and call
    // nothing (`plain_group`: builtin plain aggregates whose transitions and final raise on no input, `effects::agg_quiet`, no FILTER,
    // DISTINCT, ORDER BY or direct argument, over quiet arguments and keys)
    let quiet_agg = |n: Node<'_>| {
        n.as_aggref().is_some_and(|a| {
            effects::agg_quiet(a) && a.aggfilter.is_none() && a.aggdistinct.is_nil() && a.aggorder.is_nil() && a.aggdirectargs.is_nil() && a.args.iter().all(|x| quiet_in(gx, x, false))
        })
    };
    let agg = matches!(b.kind, BreakerKind::PlainAgg | BreakerKind::HashAgg)
        && plain_group(gx, emit).is_some_and(|g| g.keys.iter().all(|key| quiet_in(gx, key.expr.node(), false)) && g.aggs.iter().all(|a| quiet_agg(a.node)));
    if !quiet_ops || !(matches!(b.kind, BreakerKind::Sort | BreakerKind::TopK) || distinct || window || agg) {
        return false;
    }
    let mut below = below(gx, emit);
    let mut q = emit;
    loop {
        let Some(c) = reader(gx, q) else { return false };
        let mut next = false;
        let fetches = c > q
            && ps[c].ops.iter().enumerate().all(|(j, op)| match op {
                Op::Filter(_) | Op::Project(_) => true,
                // a WindowAgg reads its first input row when asked for its first row (begin_partition)
                Op::Window(_) => j == 0,
                Op::Sort(d) => sort_reads(d, &mut next),
                _ => iface::limit_bounds(op).is_some_and(|(count, offset)| {
                    bounds_fetch(count, offset) || {
                        let held = stops(Ask::Stops(&below, c, j));
                        owed &= !held;
                        held
                    }
                }),
            });
        if !fetches || owed {
            return false;
        }
        if c == gx.graph.root as usize {
            return !next;
        }
        let Some(b) = iface::breaker_of(&gx.pipelines()[c].desc) else { return false };
        // a stacked WindowAgg's partition buffer: its reader's window reads its first row (the `fetches` check)
        let buffer = b.kind == BreakerKind::Materialize && reader(gx, c).is_some_and(|r| matches!(ps[r].ops.first(), Some(Op::Window(_))));
        if !(matches!(b.kind, BreakerKind::Sort | BreakerKind::TopK | BreakerKind::PlainAgg | BreakerKind::HashAgg | BreakerKind::Distinct) || buffer) {
            return false;
        }
        below.push_in(c);
        (q, owed) = (c, next);
    }
}

/// `unconditional`'s aggregate end: the one group emit `k` folds, when it is a plain one (no grouping set, no two-pass form).
fn plain_group<'g, 'p>(gx: &'g Gx<'_, 'p>, k: usize) -> Option<&'g gravity_decompose::ops::shared::agg::GroupDecl<'p>> {
    let mut gs = gx.graph.groups.iter().filter(|g| g.producer == LifeId(k as u16));
    match (gs.next(), gs.next()) {
        (Some(g), None) if g.set.is_none() && g.two_pass.is_none() => Some(g),
        _ => None,
    }
}

/// Rule C (D1486's LIMIT forms): PostgreSQL's top WindowAgg straight over the union reads exactly the union's first `n` rows
/// under its LIMIT or its run condition (decompose `WindowDesc::capped`: a constant count and offset right above it, or a
/// row_number() run condition under a constant bound that ends the window at its own row, the least of the two; no qual,
/// ranking functions and quiet ROWS aggregates, the row past a frame ending at the current row counted), and every feed
/// carries the union's cap of `n` rows (decompose window.rs `cap_arms`, counted across the feeds in arm order:
/// `cap_union`), so the record holds the rows PostgreSQL's Append returns, in its order, their filters and calls made on
/// the same scan rows, and a feed past the count reads no row (the cap's window check runs before the source's first
/// window). Its source opens all the same: a heap range, or a heap index scan whose keys and recheck are quiet
/// (`quiet_in`), shows nothing at its open (a columnar scan's window loop is not taken). What PostgreSQL runs between two
/// of those rows shows nothing either: the emit's own ops are quiet filters and projections, the window's arguments and
/// FILTERs are quiet, and its pipeline is the statement's root of quiet filters and projections and the LIMIT. That holds
/// only when the statement runs to its LIMIT: a cursor's FETCH, a protocol portal's row limit or an SPI caller may stop it
/// sooner, where PostgreSQL reads fewer rows; so the program is served only to a reader that takes every row (the emit's
/// `source`: `Px::whole_read`, D1922) and the entry delegates it to the row executor elsewhere.
fn capped(gx: &Gx<'_, '_>, emit: usize, feeds: &[usize]) -> bool {
    let ps = gx.pipelines();
    let q = |x: Node<'_>| quiet_in(gx, x, false);
    // the window's own pipeline reads the emit's product: its partition buffer (decompose closes one under every window)
    let Some(c) = reader(gx, emit).filter(|&c| c == gx.graph.root as usize) else { return false };
    let Some((Op::Window(d), rest)) = ps[c].ops.split_first() else { return false };
    let Some(n) = d.capped else { return false };
    let window = d.functions.iter().all(|f| f.func.args.iter().all(q) && f.func.aggfilter.is_none_or(q));
    let root = rest.iter().all(|op| match op {
        Op::Filter(x) => q(*x),
        Op::Project(l) => l.iter().all(q),
        // the LIMIT `d.capped` was counted under (decompose `first_rows`: constant bounds, no WITH TIES)
        _ => crate::graph::step_kind(op) == KindId::StepLimit,
    });
    let emit_ops = ps[emit].ops.iter().all(|op| match op {
        Op::Append(AppendDesc::Union { .. }) => true,
        Op::Filter(x) => q(*x),
        Op::Project(l) => l.iter().all(q),
        _ => false,
    });
    let opens = |s: &Source<'_>| match s {
        Source::Range { am: gravity_decompose::Am::Heap, .. } => true,
        Source::Index { am: gravity_decompose::Am::Heap, keys, recheck, .. } => keys.iter().all(|x| q(x.value)) && recheck.iter().all(q),
        _ => false,
    };
    // `d.capped` holds only when decompose capped every arm (no arm of a capped union holds another Limit: `cap_arms`)
    let cap = |f: usize| opens(&ps[f].source) && ps[f].ops.iter().any(crate::ops::limit::lower::const_bound);
    n >= 1 && window && root && emit_ops && feeds.iter().all(|&f| cap(f))
}

/// wlim-w02 (window; ops/limit's `make`): op `j` of pipeline `k` is a settled union feed's row cap: a constant Limit
/// (ops/limit `const_bound`) before the feed's end, in a union whose emit fills the buffer of a top window that capped
/// every arm (decompose `WindowDesc::arms_capped`: window.rs `cap_arms` put the window's cap in each plain arm, and no arm
/// holds another Limit; such arms are quiet, `quiet_arm_in`, or rule C's, `capped`). Its count words are the union's, keyed by the Append's plan
/// node (the Feed op's), the same in every feed, so they count across the feeds in arm order: the record holds the
/// union's first rows and a feed that starts at the count reads no row (the Limit's window check runs before the source's
/// first window). None for any other op or union, a feed its Material's reader or a nest inner makes again (S1u, S2b).
pub(crate) fn cap_union(gx: &Gx<'_, '_>, k: usize, j: usize) -> Option<(u32, usize)> {
    let ps = gx.pipelines();
    let p = ps.get(k).filter(|_| k < ps.len())?;
    let (f, slot) = p.ops.iter().enumerate().find_map(|(i, op)| if let Op::Append(AppendDesc::Feed { slot, .. }) = op { Some((i, *slot)) } else { None })?;
    if j >= f || !crate::ops::limit::lower::const_bound(&p.ops[j]) || crate::pipes::inner_union(gx, k).is_some() || pulled_arm(gx, k) {
        return None;
    }
    // the union's emit (the first later pipeline whose Union op reads the slot) fills the window's partition buffer: the
    // window's own pipeline, its one reader, opens with the window op
    let e = (k..ps.len()).find(|&q| union_of(&ps[q]).is_some_and(|u| u.0 == slot))?;
    let window = reader(gx, e).and_then(|c| ps[c].ops.first()).is_some_and(|op| matches!(op, Op::Window(d) if d.arms_capped));
    window.then(|| (crate::rerun::inner::op_node(p, f) as u32, j))
}

/// The one pipeline that reads pipeline `q`'s product (its life), none when it has another count of readers.
pub(crate) fn reader(gx: &Gx<'_, '_>, q: usize) -> Option<usize> {
    let ps = gx.pipelines();
    let life = Some(LifeId(q as u16));
    let mut readers = (0..gx.graph.pipelines.len()).filter(|&c| c != q && ps[c].desc.consumes.iter().any(|&s| gx.life_of(c, s) == life));
    match (readers.next(), readers.next()) {
        (Some(c), None) => Some(c),
        _ => None,
    }
}

/// D1486 (window): reader `c`'s first op is a WindowAgg PostgreSQL reads whole before its first row (decompose
/// `WindowDesc::whole`: one partition, every row a peer, a frame or row count reaching the partition's end, functions
/// that raise on no input) whose arguments and FILTERs are quiet (`quiet_in`) or only raise (`union_reads::raise_only`).
/// PostgreSQL's WindowAgg then pulls every arm to its end at its first row (eval_windowaggregates reads row after row,
/// evaluating only those arguments and the quiet transitions between two of them), so the arms' rows and calls are the
/// fill's, and an argument's raise only ends PostgreSQL's read sooner, with an error where the fill raises one too
/// (D1486 b07); what follows runs after every arm in both.
fn whole_window(gx: &Gx<'_, '_>, c: usize) -> bool {
    let Some(Op::Window(d)) = gx.pipelines()[c].ops.first() else { return false };
    let q = |n: Node<'_>| quiet_in(gx, n, false) || crate::rerun::union_reads::raise_only(gx, n);
    d.whole && d.functions.iter().all(|f| f.func.args.iter().all(q) && f.func.aggfilter.is_none_or(q))
}

/// with#114: emit `e` fills a recursive union's working table, its seed (decompose ops/setop.rs `map_recursive_union`
/// settles a UNION ALL seed). setop.recursive runs the seed whole before the union's first row, where PostgreSQL reads it
/// row by row as the union's readers pull, so the fill runs the arms first only where PostgreSQL reads them all as well:
/// every reader reads the union to its end (`IterDesc::read_whole`: PostgreSQL then reads each seed arm to its end, in arm
/// order, the fill's order), or the seed can neither raise nor call a user or VOLATILE function (`IterDesc::early_quiet`,
/// its arms included: a skip PostgreSQL makes shows nothing). A loud seed under a reader that may stop inside it keeps
/// refusing (D2732: the one-pipeline seed raises there where PostgreSQL returns rows).
fn recursive_seed(gx: &Gx<'_, '_>, e: usize) -> bool {
    use gravity_decompose::ops::shared::setop::SetDesc;
    let ps = gx.pipelines();
    (e + 1..gx.graph.pipelines.len()).any(|j| ps[j].ops.iter().any(|op| matches!(op, Op::SetOp(SetDesc::Iter(d)) if (d.read_whole || d.early_quiet) && gx.life_of(j, d.work) == Some(LifeId(e as u16)))))
}

/// The pipelines a stop must reach below emit `k`: `k` and every pipeline feeding it, each settled union's arms included
/// (`with_arms`).
fn below<'g>(gx: &Gx<'g, '_>, k: usize) -> PgVec<'g, usize> {
    let p = &gx.pipelines()[k];
    let src = if let Source::Breaker { slot } = p.source { Some(slot) } else { None };
    let mut out = crate::ops::hashjoin::build_class::feed(gx, k, p.desc.consumes.iter().copied().chain(src));
    out.push_in(k);
    with_arms(gx, &mut out);
    out
}

/// `group` (pipelines, each with every pipeline feeding it: join/build_class.rs `feed`) with every arm of each settled union
/// emitted in it and what feeds the arm, transitively: `feed` reads a union's arms (`Gx::union_feeds`), and a stop above
/// the union must reach them all (D1486; result/gate.rs `feeds_alone`).
pub(crate) fn with_arms(gx: &Gx<'_, '_>, group: &mut PgVec<'_, usize>) {
    let mut i = 0;
    while let Some(&e) = group.get(i) {
        i += 1;
        let Some((slot, _)) = union_of(&gx.pipelines()[e]) else { continue };
        for f in crate::rerun::union_join_arms::feeds(gx, e, slot) {
            let p = &gx.pipelines()[f];
            let src = if let Source::Breaker { slot } = p.source { Some(slot) } else { None };
            for x in core::iter::once(f).chain(crate::ops::hashjoin::build_class::feed(gx, f, p.desc.consumes.iter().copied().chain(src))) {
                if !group.contains(&x) {
                    group.push_in(x);
                }
            }
        }
    }
}

/// D2115 (kit K18.x09; result/gate.rs `feeds_alone`): `group` with every other arm of the streamed union pipeline `q` is an
/// arm of (`Gx::stream_arms`: a union into the statement's sink has no slot for `feed` to follow) and what feeds the arm,
/// transitively, each settled union's arms included (`with_arms`): the arms carry copies of one Limit with one count, and
/// PG's Limit at a zero count pulls no arm.
pub(crate) fn with_stream_arms(gx: &Gx<'_, '_>, q: usize, group: &mut PgVec<'_, usize>) {
    for f in gx.stream_arms(q) {
        let p = &gx.pipelines()[f];
        let src = if let Source::Breaker { slot } = p.source { Some(slot) } else { None };
        for x in core::iter::once(f).chain(crate::ops::hashjoin::build_class::feed(gx, f, p.desc.consumes.iter().copied().chain(src))) {
            if !group.contains(&x) {
                group.push_in(x);
            }
        }
    }
    with_arms(gx, group);
}

/// Whether graph pipeline `k` feeds pipeline `q`'s input below op `j` alone: through `q`'s breaker source or a join of
/// `q`'s before `j` (another op's slot is not counted), transitively, every pipeline on the way read by no pipeline outside
/// them and `q`. D1921: a Drain's `matches` (the probe pipeline's spool) is the join's too: the build is read by the probe
/// and the drain, both below the node, so both are fed alone. `arms` (a Limit's check, D1486): every arm of a settled union
/// on the way too, as PG's Limit at a zero count pulls no arm of a union below it; and when op `j` is past `q`'s `Arm`
/// marker (the
/// union's parent's, copied into every arm with one count: decompose append.rs `push_union`, limit/lower.rs), every arm of
/// `q`'s union and what feeds it (`with_stream_arms`: a union into the sink has no slot to follow; D2115, kit K18.x09: two
/// empty grouping sets' arms over one spool walk under a bare LIMIT). The feeder checks' one predicate (result/gate.rs
/// `limit_feeder`; `full_feed`, `full_fed`: one home for them).
pub(crate) fn feeds_alone(gx: &Gx<'_, '_>, k: usize, q: usize, j: usize, arms: bool) -> bool {
    let (qp, n) = (&gx.pipelines()[q], gx.pipelines().len());
    let src = if let Source::Breaker { slot } = qp.source { Some(slot) } else { None };
    let joins = qp.ops[..j].iter().filter_map(|op| match op {
        Op::Join(JoinDesc::Probe { slot, .. } | JoinDesc::Merge { slot, .. } | JoinDesc::Drain { matches: slot, .. }) => Some(*slot),
        _ => None,
    });
    let mut group = crate::ops::hashjoin::build_class::feed(gx, q, src.into_iter().chain(joins));
    if arms {
        with_arms(gx, &mut group);
    }
    let reads = |c: usize, x: usize| gx.pipelines()[c].desc.consumes.iter().any(|&s| gx.life_of(c, s).is_some_and(|l| l.0 as usize == x));
    let alone = |group: &[usize]| group.contains(&k) && group.iter().all(|&x| (0..n).all(|c| c == q || group.contains(&c) || !reads(c, x)));
    let first = alone(&group);
    if first || !arms || !qp.ops[..j].iter().any(crate::graph::is_arm) {
        return first;
    }
    // the streamed union's other arms (one count: a zero one pulls none) and their feeders, tried only when `q`'s own do
    // not stand alone, so a feeder another arm shares with a pipeline outside the union keeps the verdict it had
    with_stream_arms(gx, q, &mut group);
    alone(&group)
}

/// D1753 / D1752: the check result/gate.rs's `Check::Full` makes (`full_feed`): the union's plan node, the LIMIT op's
/// place past the union's arm marker (its words' key, limit/lower.rs) and the rows it reads (count plus offset).
pub(crate) struct FullFeed {
    pub(crate) union: i32,
    pub(crate) after: u64,
    pub(crate) rows: i64,
}

/// D1753 / D1752 (result/gate.rs `Check::Full`): PG's Append starts an arm only when its reader pulls
/// past the previous one, so once a LIMIT above the union has read its rows it never runs a later arm's children (a later
/// partition's Hash, a HashAggregate's fill). Graph pipeline `k` that feeds alone (`feeds_alone`) a later pipeline `q`,
/// whose op `j` is a LIMIT of the union's parent (copied past the nearest arm marker: decompose append.rs `push_union`; no
/// Drain op between) with a constant count that cannot be zero and a constant offset, no ties (frame/iface.rs asks the
/// node it reads, the union or a projection or filter over it, `Bound::Rows`: count plus offset), neither in a rerun set:
/// `k`'s feeder reads the union's words (the LIMIT's skipped and emitted counts under the union's flag) and stops `k`
/// when the flag is set and the rows are read: that LIMIT then pulls nothing more, so PG pulls no arm. A zero count is the
/// zero check's (`stopping`, D1716).
pub(crate) fn full_feed(px: &Px<'_, '_>, k: usize, q: usize, j: usize) -> Option<FullFeed> {
    let gx = px.gx;
    let p = &gx.pipelines()[q];
    let limit = crate::graph::step_kind(&p.ops[j]) == KindId::StepLimit && crate::frame::iface::limit_stopping(&p.ops[j]).is_none();
    if !limit || k >= q || [k, q].iter().any(|&x| gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(x as u16)))) {
        return None;
    }
    let marker = p.ops[..j].iter().rposition(crate::graph::is_arm)?;
    if p.ops[marker + 1..j].iter().any(|o| matches!(o, Op::Join(JoinDesc::Drain { .. }))) || !feeds_alone(gx, k, q, j, false) {
        return None;
    }
    let at = |o: usize| p.node_ops.iter().rposition(|n| n.first_op as usize <= o);
    let (u, l) = (at(marker)?, at(j)?);
    let below = l.checked_sub(1).filter(|&b| b >= u)?;
    let crate::frame::iface::Bound::Rows(rows @ 1..) = px.request(p.node_ops[below].node_id).bound else { return None };
    Some(FullFeed { union: p.node_ops[u].node_id, after: crate::frame::px::key_j(j - marker), rows: i64::try_from(rows).ok()? })
}

/// D1753 / D1752: whether graph pipeline `k` runs only where PG starts the arm of streamed union `union` it feeds, so a
/// fence on what `k` shows (drain/arms.rs `arm_order_unseen`, LATER_ARMS; append/later_arms.rs `fenced`, LATER_FEEDERS)
/// does not apply: drain/arms.rs `arm_runs` takes the union, `k` is one of later arm `a`'s child's pipelines (frame/mechs.rs
/// `arm_order` runs them right after arm `a - 1`, right before arm `a`), past the union's marker arm `a`'s pipeline only
/// passes rows up (projections, filters) to LIMITs, `k`'s feeder makes every one's `Check::Full` (`full_feed`, where the
/// feeder reaches it: result/gate.rs `reaches`), and the arm ends where every row it emits is read: a plain aggregate, or
/// the statement's sink, read whole or by a named cursor's FETCHes (`graph.partial`: a FETCH that stops inside an earlier
/// arm suspends the program before arm order reaches `k`, as PG's union has not started arm `a`, D3377). SPI's and a SQL
/// function's lazy row are not partial: they may stop inside an earlier arm and never resume, so `k` is marked whole-read
/// (`Px::whole_read`, the portal-kind fact) and delegated. Any other stop above the union (a count read
/// at bind, a lazily read spool, a sort or hashed end that may read fewer rows: an incremental sort, a mixed grouping-sets
/// phase) keeps the fences.
pub(crate) fn full_fed(px: &mut Px<'_, '_>, k: usize, union: i32) -> bool {
    let gx = px.gx;
    let Some(r) = gx.arm_runs().into_iter().find(|r| gx.arm_append(r.arms.start) == Some(union)) else { return false };
    let Some(a) = arm_child(gx, union, k).filter(|&a| a >= 1) else { return false };
    let q = r.arms.start + a;
    let p = &gx.pipelines()[q];
    let sink = gx.end_kind(q) == crate::graph::EndKind::Sink;
    let whole = sink || gx.end_kind(q) == crate::graph::EndKind::Breaker(BreakerKind::PlainAgg);
    let Some(marker) = p.ops.iter().rposition(crate::graph::is_arm) else { return false };
    let limit = |j: usize| crate::graph::step_kind(&p.ops[j]) == KindId::StepLimit;
    let passes = (marker + 1..p.ops.len()).all(|j| limit(j) || matches!(p.ops[j], Op::Project(_) | Op::Filter(_)));
    let checked = |j: usize| full_feed(px, k, q, j).is_some_and(|f| f.union == union) && crate::ops::result::gate::reaches(gx, k, q, j);
    let fed = whole && passes && (marker + 1..p.ops.len()).any(limit) && (marker + 1..p.ops.len()).filter(|&j| limit(j)).all(checked);
    if fed && sink && !gx.graph.partial {
        px.whole_read(k);
    }
    fed
}

/// D1752 / D2697: the arm of streamed union `union` whose child's pipelines hold graph pipeline `k`, where drain/arms.rs
/// `arm_runs` takes the union: frame/mechs.rs `arm_order` runs them right after the previous arm and right before that
/// arm, as PG's Append runs child `i` whole first. `None` for any other pipeline or union.
pub(crate) fn arm_child(gx: &Gx<'_, '_>, union: i32, k: usize) -> Option<usize> {
    let r = gx.arm_runs().into_iter().find(|r| gx.arm_append(r.arms.start) == Some(union))?;
    (0..r.arms.len()).find(|&a| (r.starts[a]..r.starts[a + 1]).contains(&k))
}

/// D1753 / D2693: whether a LIMIT past op `j` of pipeline `q` has a count or offset read at bind or one that may stop its
/// call (ops/limit `stopping`): frame/iface.rs asks no bound for it, yet PG may stop inside an earlier arm under it.
pub(crate) fn late_limit(gx: &Gx<'_, '_>, q: usize, j: usize) -> bool {
    gx.pipelines()[q].ops.get(j + 1..).is_some_and(|ops| ops.iter().any(|o| crate::frame::iface::limit_stopping(o).is_some()))
}

/// A sort op that reads its whole input once started (`sort_fetches`), or a top whose bounds may read nothing: then it
/// is `owed` the next reader's LIMIT (its own bounds), which must stop everything below (`unconditional`).
pub(crate) fn sort_reads(d: &SortDesc<'_>, owed: &mut bool) -> bool {
    match &d.form {
        SortForm::TopK { count, offset, .. } if !bounds_fetch(count, offset) => {
            *owed = true;
            true
        }
        _ => sort_fetches(d),
    }
}

/// A LIMIT PostgreSQL's ExecLimit reads its input under: a plan-time constant count above zero (or none) and no computed
/// offset. LIMIT 0 reads nothing, and a Param or expression count or offset is evaluated (and may raise) before the first pull
/// (recompute_limits), so either could leave every arm unrun (such a union refuses by D1486).
fn bounds_fetch(count: &Bound<'_>, offset: &Bound<'_>) -> bool {
    (matches!(count, Bound::All) || matches!(count, Bound::Const(x) if *x > 0)) && !matches!(offset, Bound::Expr(_))
}

/// A sort op: a full sort reads its whole input; a top-k or walk only under a LIMIT that reads (`bounds_fetch`).
fn sort_fetches(d: &SortDesc<'_>) -> bool {
    match &d.form {
        SortForm::Runs => true,
        SortForm::TopK { count, offset, .. } | SortForm::Walk { count, offset, .. } => bounds_fetch(count, offset),
    }
}

/// Rule B (material item 9: VF2's half that needs no batch re-run). PostgreSQL reads every arm to its end, the arms may
/// raise but act nowhere, and what runs between two arms' rows acts nowhere: the emit is the statement's root (or fills a
/// top WindowAgg's buffer the root reads, `window_write`) and ends in an INSERT (ExecModifyTable loops to the end of its
/// subplan) whose filters, gates and projections act nowhere, with no ON
/// CONFLICT, MERGE or check option, a RETURNING list that acts nowhere and a target (every leaf of a routed one) with no
/// BEFORE or INSTEAD OF row trigger (an AFTER row trigger fires at the statement's end only); every feed is `still`. An
/// arm's error is then one PostgreSQL meets too: it reads that arm's row as well, unless an earlier error ended the
/// statement (any genuine error passes, `DECISIONS.md` 4 (a)), and the rows written before it leave with the statement's
/// rollback; with no error the record holds the arms' rows in Append order. Filling the arms first shows nothing
/// PostgreSQL's interleaving does. An arm joins by a calm nest only (`still_ops`), and a SubPlan in an arm or the emit
/// counts as acting. G2(b): a MERGE / UPDATE / DELETE whose input is the union takes the same rule (`write_still`), a
/// source join's nest between them included.
fn drained(gx: &Gx<'_, '_>, arena: ArenaRef, emit: usize, feeds: &[usize]) -> bool {
    let ps = gx.pipelines();
    let root = gx.graph.root as usize;
    let calm = |n: Node<'_>| !acts(gx, arena, n);
    // D1486 (D3373: topn_new46's setup, INSERT .. SELECT row_number() OVER (), k FROM (.. UNION ALL ..)): or a top
    // WindowAgg between the union and the write (`window_write`)
    let windowed = emit != root && window_write(gx, emit, root, &calm);
    if emit != root && !windowed {
        return false;
    }
    // RETURNING: decompose modifytable projects the returning list after the write (`insert_still` reads that list)
    let ops = match ps[root].ops.split_last() {
        Some((Op::Project(_), rest)) if matches!(rest.last(), Some(Op::Write(w)) if w.returning.is_some()) => rest,
        _ => &ps[root].ops[..],
    };
    let Some((Op::Write(w), body)) = ops.split_last() else { return false };
    let insert = w.cmd == Cmd::Insert;
    let ops = body.iter().enumerate().all(|(j, op)| match op {
        Op::Append(AppendDesc::Union { .. }) => !windowed,
        // `window_write` read its functions
        Op::Window(_) => windowed && j == 0,
        Op::Filter(x) | Op::Gate(x) => calm(*x),
        Op::Project(l) => l.iter().all(calm),
        // G2(b): a MERGE's source join (an UPDATE / DELETE .. FROM's), a nest over the union's rows: its parameters and
        // join qual act nowhere and nothing in its inner acts (F5: not VOLATILE). The inner keeps its own form (D1108:
        // filled ahead only when raise-free, else run per outer row), so it runs only where PostgreSQL's does
        Op::Join(JoinDesc::Nest { params, qual, facts, .. }) if !insert => {
            crate::effects::nest_inner_calm(facts.volatility) && params.iter().all(|p| p.as_nest_loop_param().is_some_and(|p| calm(p.paramval))) && qual.iter().all(calm)
        }
        _ => false,
    });
    let write = if insert { insert_still(gx, w, &calm) } else { write_still(gx, w, &calm) };
    ops && write && feeds.iter().all(|&f| still(gx, &ps[f], &calm))
}

/// D1486 (D3373: topn_new46's setup INSERTs): rule B over a top WindowAgg with no run condition straight over the union,
/// its partition buffer the emit's end and the root, the write's pipeline, its one reader: PostgreSQL's WindowAgg hands
/// the union's rows on (reading ahead inside the partition as its frames need) while ExecModifyTable drains it to its end,
/// so it reads every arm to its end too, and what it runs between two arms' rows is its functions over the rows read so
/// far, which act nowhere here (each may raise); the emit's own ops act nowhere either. A run condition may end the read
/// early (the window rows stay D1486's).
fn window_write(gx: &Gx<'_, '_>, emit: usize, root: usize, calm: &dyn Fn(Node<'_>) -> bool) -> bool {
    let ps = gx.pipelines();
    let buffer = iface::breaker_of(&ps[emit].desc).is_some_and(|b| b.kind == BreakerKind::Materialize && matches!(ps[root].source, Source::Breaker { slot } if slot == b.slot));
    let emit_ops = ps[emit].ops.iter().all(|op| match op {
        Op::Append(AppendDesc::Union { .. }) => true,
        Op::Filter(x) | Op::Gate(x) => calm(*x),
        Op::Project(l) => l.iter().all(calm),
        _ => false,
    });
    let window = matches!(ps[root].ops.first(), Some(Op::Window(d)) if d.run_condition.is_nil() && d.functions.iter().all(|f| calm(f.node)));
    buffer && emit_ops && window && reader(gx, emit) == Some(root)
}

/// G2(b): rule B's MERGE / UPDATE / DELETE, the union its input. The
/// arms scan under the command's snapshot, so the write's own changes stay invisible to them whenever they run, and
/// ExecModifyTable reads its whole input. Its per-row work acts nowhere: no ON CONFLICT, every MERGE action's quals and
/// target list, every check option's qual (WITH CHECK OPTION, a policy's) and RETURNING list calm (each may raise), every
/// result relation a heap or partitioned table with no BEFORE or INSTEAD OF row trigger for the command's events, and
/// so every routed leaf. Only the order of two genuine errors can then differ from PostgreSQL's; with no error the
/// written rows and RETURNING rows are PostgreSQL's.
fn write_still(gx: &Gx<'_, '_>, w: &WriteDesc<'_>, calm: &dyn Fn(Node<'_>) -> bool) -> bool {
    use gravity_decompose::ops::modifytable::trig_facts::{TrigExt, TRIG_DELETE, TRIG_UPDATE};
    use gravity_decompose::ops::shared::modifytable::{MergeAction, TargetKind};
    let events = match w.cmd {
        Cmd::Update => TRIG_UPDATE,
        Cmd::Delete => TRIG_DELETE,
        Cmd::Merge => TRIG_INSERT | TRIG_UPDATE | TRIG_DELETE,
        _ => return false,
    };
    let quiet_trig = |t: TrigExt| t.known && (t.before_row | t.instead_row) & events == 0;
    let list = |l: Option<&NodeList<'_>>| l.is_none_or(|l| l.iter().all(calm));
    let action = |a: &MergeAction<'_>| list(a.quals) && a.tlist.iter().all(calm);
    let merge = w.merge.as_ref().is_none_or(|m| list(m.join_cond) && m.actions.iter().all(action));
    let targets = w.targets.iter().all(|t| {
        matches!(t.kind, TargetKind::Heap | TargetKind::Partitioned)
            && quiet_trig(gx.facts.trig_ext(t.relid))
            && list(t.returning)
            && t.merge.as_ref().is_none_or(|m| list(m.join_cond) && m.actions.iter().all(action))
    });
    let leaves = w.route.is_none() || w.leaves_known && w.leaves.iter().all(|l| quiet_trig(l.trig));
    w.on_conflict.is_none()
        && merge
        && targets
        && leaves
        && w.checks.iter().all(|c| calm(c.qual))
        && list(w.returning)
        && quiet_trig(gx.facts.trig_ext(w.plan.target_rel))
}

/// Rule B's INSERT: its per-row work acts nowhere (`drained`).
fn insert_still(gx: &Gx<'_, '_>, w: &WriteDesc<'_>, calm: &dyn Fn(Node<'_>) -> bool) -> bool {
    let quiet_trig = |t: gravity_decompose::ops::modifytable::trig_facts::TrigExt| t.known && (t.before_row | t.instead_row) & TRIG_INSERT == 0;
    let leaves = w.route.is_none() || w.leaves_known && w.leaves.iter().all(|l| quiet_trig(l.trig));
    w.cmd == Cmd::Insert
        && w.on_conflict.is_none()
        && w.merge.is_none()
        && w.checks.is_empty()
        && w.returning.is_none_or(|l| l.iter().all(calm))
        && quiet_trig(gx.facts.trig_ext(w.plan.target_rel))
        && leaves
}

/// Rule B's arm: a heap or index scan, a bitmap heap scan, a Result or VALUES source, read by no other pipeline's product,
/// under filters, gates, projections, FROM-clause set functions and calm nests, none of which acts (each may raise); or a
/// window over such an arm's rows (rerun/spool/window_arm.rs), a group's or a top-k's (`group_arm`, `topk_arm`), a
/// CteScan over a CTE that is such an arm itself (`cte_arm`), or the emit of a union of its own whose feeds are such arms
/// (`union_arm`).
fn still(gx: &Gx<'_, '_>, p: &PipelineNode<'_>, calm: &dyn Fn(Node<'_>) -> bool) -> bool {
    let Some((Op::Append(AppendDesc::Feed { .. }), body)) = p.ops.split_last() else { return false };
    still_ops(gx, p, body, calm)
}

/// `still` over pipeline `p` running ops `body` (a feed's but its Feed, or a window's fill's every op).
fn still_ops(gx: &Gx<'_, '_>, p: &PipelineNode<'_>, body: &[Op<'_>], calm: &dyn Fn(Node<'_>) -> bool) -> bool {
    crate::stack_check(); // D3832
    let src = match &p.source {
        Source::Range { .. } | Source::Const => true,
        Source::Index { keys, recheck, .. } => keys.iter().all(|x| calm(x.value)) && recheck.iter().all(calm),
        Source::Bitmap { plan, recheck, .. } => crate::rerun::union_join_arms::leaves(plan, &|x| calm(x.value)) && recheck.iter().all(calm),
        Source::Values { lists, .. } => lists.iter().all(|l| l.as_list().is_some_and(|l| l.iter().all(calm))),
        Source::Breaker { slot } => return group_arm(gx, p, *slot, body, calm) || topk_arm(gx, p, *slot, body, calm) || cte_arm(gx, p, *slot, body, calm) || crate::rerun::spool::window_arm::still(gx, p, *slot, body, calm, &|q: &PipelineNode<'_>, ops: &[Op<'_>]| still_ops(gx, q, ops, calm)) || union_arm(gx, p, *slot, body, calm),
        _ => false,
    };
    let call = |f: u32| !effects::of_call(gx.facts, f).acts;
    src && p.desc.consumes.is_empty()
        && body.iter().all(|op| match op {
            Op::Filter(n) | Op::Gate(n) => calm(*n),
            Op::Project(l) => l.iter().all(calm),
            Op::Srf(d) => matches!(d.place, gravity_decompose::ops::shared::srf::SrfPlace::From { .. }) && d.functions.iter().all(|f| call(f.funcid) && f.args.iter().all(calm)) && d.tlist.iter().all(calm),
            // D1486 (D3373: topn_new46's `FROM generate_series(..) j, generate_series(..) r WHERE r <= (j + 1) * 10` arm):
            // a nest whose parameters and join qual act nowhere and whose inner acts nowhere (F5: not VOLATILE), as
            // `drained`'s source join: the inner keeps its own form (D1108: filled ahead only when raise-free, else run
            // per outer row), so it runs only where PostgreSQL's does, and the arm's rows come in the nest's order
            Op::Join(JoinDesc::Nest { params, qual, facts, .. }) => {
                crate::effects::nest_inner_calm(facts.volatility) && params.iter().all(|p| p.as_nest_loop_param().is_some_and(|p| calm(p.paramval))) && qual.iter().all(calm)
            }
            _ => false,
        })
}

/// Rule B's arm over a group (agg_core-0320: decompose gsets `map_sets`' sets, each an aggregate over the input): `p`
/// drains the group of breaker `slot`, whose one producer `p` alone reads and which is rule B's itself (`still_ops` over
/// its ops); the group's keys and aggregates act nowhere (each may raise); then `body`'s filters, gates and projections.
fn group_arm(gx: &Gx<'_, '_>, p: &PipelineNode<'_>, slot: u8, body: &[Op<'_>], calm: &dyn Fn(Node<'_>) -> bool) -> bool {
    let ps = gx.pipelines();
    let n = gx.graph.pipelines.len();
    let Some(j) = (0..n).find(|&j| core::ptr::addr_eq(&ps[j], p)) else { return false };
    let (Some(g), Some(LifeId(q))) = (gx.drained_group(j), gx.life_of(j, slot).filter(|_| p.desc.consumes[..] == [slot])) else { return false };
    let q = q as usize;
    let of = |k: usize| k < n && gx.group_at(k).is_some_and(|h| core::ptr::eq(h, g));
    let readers = (0..n).filter(|&c| ps[c].desc.consumes.iter().any(|&s| gx.life_of(c, s) == Some(LifeId(q as u16)))).count();
    of(q) && !of(q + 1) && readers == 1
        && still_ops(gx, &ps[q], &ps[q].ops, calm)
        && g.keys.iter().all(|k| calm(k.expr.node()))
        && g.aggs.iter().all(|a| calm(a.node))
        && body.iter().all(|op| match op {
            Op::Filter(x) | Op::Gate(x) => calm(*x),
            Op::Project(l) => l.iter().all(calm),
            _ => false,
        })
}

/// D3379 (UNION-DRAINED, producerchain_union_copy's `SELECT a + 1 FROM some_tab WHERE a > 0` arm): rule B's arm over a
/// union of its own. PostgreSQL keeps such an arm as a Subquery Scan over a Result over an Append (a WHERE over an
/// inheritance parent); decompose settles that Append too, its emit `p` this arm: `body` is the inner union's Union op
/// (reading `slot`, no pruning) and then filters, gates and projections that act nowhere (each may raise), and every feed
/// of the inner union is rule B's own arm (`still`). PostgreSQL's outer Append reaches the Subquery Scan after the arms
/// before it and reads it to its end, its own Append arm after arm; the record's rows come in that order, and an error
/// is one PostgreSQL meets too.
fn union_arm(gx: &Gx<'_, '_>, p: &PipelineNode<'_>, slot: u8, body: &[Op<'_>], calm: &dyn Fn(Node<'_>) -> bool) -> bool {
    let ps = gx.pipelines();
    let n = gx.graph.pipelines.len();
    let Some(e) = (0..n).find(|&j| core::ptr::addr_eq(&ps[j], p)) else { return false };
    let Some((Op::Append(AppendDesc::Union { slot: u, prune: None, .. }), rest)) = body.split_first() else { return false };
    if *u != slot || p.desc.consumes[..] != [slot] {
        return false;
    }
    let feeds = crate::rerun::union_join_arms::feeds(gx, e, slot);
    let steps = rest.iter().all(|op| match op {
        Op::Filter(x) | Op::Gate(x) => calm(*x),
        Op::Project(l) => l.iter().all(calm),
        _ => false,
    });
    let ok = steps && !feeds.is_empty() && feeds.iter().all(|&f| f < e && still(gx, &ps[f], calm));
    if ok {
        fires::fire("union.arm.nested"); // shapecheck fire signal (D1486 rule B's arm over a union of its own)
    }
    ok
}

/// D1486 (rxshape1; the R-KEEP c-app/jobx loader, INSERT INTO movie_info WITH mm AS (..) SELECT .. FROM (.. FROM mm UNION
/// ALL .. FROM mm ..)): rule B's arm over a CTE. `p` reads CTE `slot` through a CteScan (kind nest.cte: its reader pulls the
/// CTE's plan only past the rows stored, as nodeCtescan.c does), the CTE is rule B's own arm (`still_ops` over its ops: a
/// scan, a Result, VALUES or a FROM-clause set function under filters and projections that act nowhere, each may raise; no
/// write, lock or feeder), and `body` is filters, gates and projections that act nowhere. The feeds run in arm order and
/// each CteScan pulls as PostgreSQL's does, so the first arm that reads the CTE computes it row by row in PostgreSQL's
/// interleaving; an error (the CTE's or an arm's) is one PostgreSQL meets too, and with none the record holds PostgreSQL's
/// rows in Append order.
fn cte_arm(gx: &Gx<'_, '_>, p: &PipelineNode<'_>, slot: u8, body: &[Op<'_>], calm: &dyn Fn(Node<'_>) -> bool) -> bool {
    let ps = gx.pipelines();
    let n = gx.graph.pipelines.len();
    let Some(j) = (0..n).find(|&j| core::ptr::addr_eq(&ps[j], p)) else { return false };
    let Some(LifeId(q)) = gx.life_of(j, slot).filter(|_| p.desc.consumes[..] == [slot]) else { return false };
    let q = q as usize;
    if q >= n || gx.spool_kind(q) != KindId::NestCte {
        return false;
    }
    let cte = match ps[q].ops.split_last() {
        Some((Op::Join(JoinDesc::Spool), rest)) => rest,
        _ => &ps[q].ops[..],
    };
    let ok = still_ops(gx, &ps[q], cte, calm)
        && body.iter().all(|op| match op {
            Op::Filter(x) | Op::Gate(x) => calm(*x),
            Op::Project(l) => l.iter().all(calm),
            _ => false,
        });
    if ok {
        fires::fire("union.arm.cte"); // shapecheck fire signal (D1486 rule B's CTE arm, rxshape1)
    }
    ok
}

/// D1486 (window, heldout tw-core-030): rule B's arm over a top-k (a subquery's ORDER BY .. LIMIT n, PostgreSQL's Limit
/// over a Sort): `p` reads the run of TopK breaker `slot`, whose one producer `q` alone `p` reads; `q` is rule B's own arm
/// up to its sort (`still_ops` over its ops before it: a scan's or a group's rows), sorted by keys whose comparisons call no
/// user code (no CompareFn key) and kept to a constant count; `p` takes that count again (the plan's Limit) and then
/// `body`'s filters, gates and projections. PostgreSQL's Sort reads the arm's whole input before its first row, as the
/// record's fill does, so the arm acts nowhere and raises only where PostgreSQL does too.
fn topk_arm(gx: &Gx<'_, '_>, p: &PipelineNode<'_>, slot: u8, body: &[Op<'_>], calm: &dyn Fn(Node<'_>) -> bool) -> bool {
    let ps = gx.pipelines();
    let n = gx.graph.pipelines.len();
    let Some(j) = (0..n).find(|&j| core::ptr::addr_eq(&ps[j], p)) else { return false };
    let Some(LifeId(q)) = gx.life_of(j, slot).filter(|_| p.desc.consumes[..] == [slot]) else { return false };
    let q = q as usize;
    let readers = (0..n).filter(|&c| ps[c].desc.consumes.iter().any(|&s| gx.life_of(c, s) == Some(LifeId(q as u16)))).count();
    let Some((Op::Sort(d), pre)) = ps.get(q).filter(|_| q < j).and_then(|x| x.ops.split_last()) else { return false };
    let bound = |x: &Bound<'_>| matches!(x, Bound::All | Bound::Const(_));
    let sorted = matches!(&d.form, SortForm::TopK { count: Bound::Const(_), offset, ties: false } if bound(offset)) && d.keys.iter().all(|k| !k.compare_fn());
    readers == 1
        && gx.end_kind(q) == EndKind::Breaker(BreakerKind::TopK)
        && sorted
        && still_ops(gx, &ps[q], pre, calm)
        && body.iter().all(|op| match op {
            Op::Filter(x) | Op::Gate(x) => calm(*x),
            Op::Project(l) => l.iter().all(calm),
            _ => crate::ops::limit::lower::const_bound(op),
        })
}

/// A feed's end: its rows appended to the union's record (declared at the first feed, found by the others).
pub(crate) fn end<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    if crate::pipes::inner_union(px.gx, k).is_some() {
        return crate::rerun::inner_arms::fused(px.mcx()); // S2b: the inner makes this arm again and pulls it
    }
    let (gx, arena) = (px.gx, px.arena);
    let p = &gx.pipelines()[k];
    let (emit, feeds, _) = admit(gx, arena, k, feed_of(p), p.node_id, &mut |a| answer(px, a));
    if pulled(gx, emit) || arms_read(gx, emit) {
        return crate::rerun::inner_arms::fused(px.mcx()); // S1u: the Material's reader makes this arm again and pulls it; road (3): the emit
    }
    // S2r: the first feed of a union the nest's set refills empties the record at its open, at every run; S2q: the same
    // for a union a rerun set re-runs (`refills_whole`, which `admit` checked); D3614: under a pruning decided at every run
    // the first feed to open does (`fresh`)
    let first = feeds[0] == k && (refill_of(gx, k).is_some() || feed_of(p).is_some_and(|s| held(gx, s, emit, &feeds)));
    let rec = record(px, feeds[0], k);
    if let Some(w) = fresh(px, emit, &feeds) {
        return crate::ops::material::lower::fill_fresh(px.mcx(), rec, w);
    }
    crate::ops::material::lower::fill(px.mcx(), rec, first)
}

/// D3614: a held union (S2q) whose rerun sets prune its arms at every run (`admit`'s `per_rerun`): a pruned arm's pipeline
/// is never opened (D2105: exec rerun/mod.rs `pruned_whole`), so the first feed may not run and cannot empty the record.
/// Its fresh word instead: the first feed to open in a run empties the record and sets it (material lower.rs
/// `fill_fresh`), and the emit's open empties the record when no feed opened, clearing it (`eager_fresh`), as
/// PostgreSQL's ExecReScanAppend re-prunes and its Append yields only the valid arms' rows (none when every arm is pruned).
/// None for any other union.
fn fresh(px: &mut Px<'_, '_>, emit: usize, feeds: &[usize]) -> Option<i32> {
    let gx = px.gx;
    let p = &gx.pipelines()[emit];
    let (slot, _) = union_of(p)?;
    let (d, _) = union_prune(p)?;
    let per = refill_of(gx, emit).is_none()
        && d.exec
        && !feeds.is_empty()
        && !crate::rerun::union_reads::init_pruned(gx, d, feeds)
        && feeds.iter().all(|&f| crate::ops::append::prune::exec_per_rerun(gx, f, d))
        && held(gx, slot, emit, feeds);
    // the union's word keyed by the emit's plan node (as the arm markers' words are), past every op position
    per.then(|| px.frame_word(WordKey::Union { union: p.node_id as u32, after: u64::MAX - 2, i: 0 }) as i32 * 8)
}

/// The emit's source: the union's record, read once.
pub(crate) fn source<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    let node = px.gx.pipelines()[k].node_id;
    if crate::pipes::inner_union(px.gx, k).is_some_and(|u| u.0 == k) {
        return crate::rerun::inner_arms::source(px, k); // S2b: a nest inner's union, its arms streamed per re-open
    }
    let (gx, arena) = (px.gx, px.arena);
    let (_, feeds, full) = admit(gx, arena, k, union_of(&gx.pipelines()[k]).map(|u| u.0), node, &mut |a| answer(px, a));
    // D3377 road (3): under a named cursor's portal the sink rule's union is pulled arm after arm by its emit (`arms_read`)
    if arms_read(gx, k) {
        let skips = pull_skips(px, k, &feeds, node);
        // D3612: a pruning over InitPlans that may raise or act decides at the first pull
        let late = match gx.pipelines()[k].ops.first() { Some(Op::Append(AppendDesc::Union { prune: Some(d), .. })) if crate::rerun::spool::form::pull_late(gx, k) => Some(d), _ => None };
        return crate::rerun::inner_arms::pulled(px, k, &feeds, skips, late, node);
    }
    // rule C (`capped`) and rule W (`full_read`): PostgreSQL's only when the statement runs to its LIMIT or its end, so the
    // program is served only to a reader that takes every row (D1922's portal-kind fact, `Px::whole_read`): the entry
    // delegates it under a named cursor's FETCH, SPI, a SQL function's lazy row or a bounded protocol Execute
    if full {
        // Construct C2 (b): the arms fill the record ahead of PostgreSQL's pulls for that reader alone, under the whole-read
        // licence asked with what they and their feeders run (`shared::whole::read`, which marks the program as
        // `Px::whole_read` does), stamped on each of them, while the tracer is on. Denied (an act past the call-local
        // allowlist), or with the tracer off, the program is marked alone
        if crate::frame::program::stamps_on() {
            let own = feeds.iter().fold(effects::Effects::NONE, |e, &f| e.and(crate::effects::pipeline_effects(gx, arena, f, true)));
            if let Some(l) = crate::ops::shared::whole::read(px, k, own) {
                hold_arms(px, &feeds, &l);
            }
        } else {
            px.whole_read(k);
        }
    }
    if pulled(gx, k) {
        return crate::ops::material::lower::union_empty(px, k, feeds[0]); // S1u: the reader pulls the arms; this reads nothing
    }
    let rec = record(px, feeds[0], k);
    if let Some(w) = fresh(px, k, &feeds) {
        return crate::ops::material::lower::eager_fresh(px, k, rec, node, w);
    }
    crate::ops::material::lower::eager(px, k, rec, node)
}

/// The row settled union emit `k` reads from the record: its first feed's projection, as `record` declares it. An op over
/// the union takes it where the emit projects nothing itself (a grouping-sets union: each arm projects its own set's row,
/// decompose gsets.rs); none for any other pipeline or a nest inner's union (S2b streams its arms).
pub(crate) fn record_cols<'p>(gx: &Gx<'_, 'p>, k: usize) -> Option<&'p NodeList<'p>> {
    let (slot, _) = union_of(&gx.pipelines()[k]).filter(|_| crate::pipes::inner_union(gx, k).is_none())?;
    let f0 = *crate::rerun::union_join_arms::feeds(gx, k, slot).first()?;
    match gx.pipelines()[f0].ops.iter().rev().nth(1) {
        Some(Op::Project(cols)) => Some(*cols),
        _ => None,
    }
}

/// The record the first feed `f0` declares (at its end) and every other pipeline `k` of the union finds.
fn record<'g>(px: &mut Px<'g, '_>, f0: usize, k: usize) -> Rec<'g> {
    let gx = px.gx;
    let p = &gx.pipelines()[f0];
    let Some(Op::Project(cols)) = p.ops.iter().rev().nth(1) else { shape(p.node_id, LOUD_UNION) };
    let node = gx.pipelines()[k].node_id;
    match k == f0 {
        true => Rec::declare(px, f0, f0, node, cols, 0).0,
        false => Rec::find(px, f0, node, cols),
    }
}

#[cfg(test)]
mod tests {
    use gravity_abi::breaker::{Breaker, BreakerKind};
    use gravity_decompose::graph::Bound;
    use gravity_decompose::ops::shared::append::AppendDesc;
    use gravity_decompose::ops::shared::sort::{SortDesc, SortForm};
    use gravity_decompose::{NoFacts, Op, PipelineGraph, PipelineNode, Source};
    use gravity_program::{Arena, ArenaVec, End, Pipeline, Reason};
    use types_nodes::list::NodeList;

    use super::{admit, feed_of, unconditional, union_of, Ask};
    use crate::graph::Gx;

    /// Rule A's witnesses where SQL plans no settled union (main 09-29 00:39Z): p0 is the emit (the union's reader, its own
    /// ops `emit_ops`) ending in a `end` breaker in slot 1; p1, the root, reads slot 1 under `root_ops`.
    fn chain(a: gravity_program::ArenaRef, end: BreakerKind, emit_ops: Vec<Op<'static>>, root_ops: Vec<Op<'static>>) -> PipelineGraph<'static> {
        let mut g = PipelineGraph::new(a);
        let d = Pipeline::serial(-1, End::Breaker(Breaker::sized(end, 64).in_slot(1)), Reason::NoDrivingSite);
        let mut emit = PipelineNode::new(a, 3, Source::Breaker { slot: 0 }, d);
        emit_ops.into_iter().for_each(|op| emit.push(op));
        g.pipelines.push(emit);
        let mut root = PipelineNode::new(a, 1, Source::Breaker { slot: 1 }, Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming_one(1));
        root_ops.into_iter().for_each(|op| root.push(op));
        g.pipelines.push(root);
        g.root = 1;
        g
    }

    fn limit(n: i64) -> Op<'static> {
        Op::Limit { count: Bound::Const(n), offset: Bound::All, ties: None }
    }

    fn reads_all(end: BreakerKind, emit_ops: Vec<Op<'static>>, root_ops: Vec<Op<'static>>) -> bool {
        let arena = Arena::new(1 << 16);
        reads(arena.handle(), end, emit_ops, root_ops, false)
    }

    /// `unconditional` over `chain`, a LIMIT's check stopping the emit (the only pipeline below the root) when `held`.
    fn reads(a: gravity_program::ArenaRef, end: BreakerKind, emit_ops: Vec<Op<'static>>, root_ops: Vec<Op<'static>>, held: bool) -> bool {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let g = chain(a, end, emit_ops, root_ops);
        let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
        unconditional(&gx, 0, &mut |a| {
            let Ask::Stops(ps, q, _) = a else { panic!("rule A asks only for its LIMIT's checks") };
            assert_eq!((ps, q), (&[0usize][..], 1), "the root's LIMIT asked to stop the emit");
            held
        })
    }

    #[test]
    fn a_sort_ended_emit_read_by_a_fetching_root_is_unconditional() {
        assert!(reads_all(BreakerKind::Sort, vec![], vec![]), "a full sort read to the root");
        assert!(reads_all(BreakerKind::Sort, vec![], vec![limit(3)]), "a LIMIT 3 over the sort reads it");
    }

    #[test]
    fn an_aggregate_ended_emit_is_not_unconditional() {
        // an aggregate's transitions run between two arms' rows (one could raise before a later arm's VOLATILE call): D1486
        assert!(!reads_all(BreakerKind::PlainAgg, vec![], vec![]), "a plain aggregate over the union");
        assert!(!reads_all(BreakerKind::HashAgg, vec![], vec![]), "a hashed aggregate over the union");
    }

    #[test]
    fn a_limit_on_the_emit_is_not_unconditional() {
        // PostgreSQL's Limit over the Append stops inside an arm, later arms unrun: D1486
        assert!(!reads_all(BreakerKind::Sort, vec![limit(2)], vec![]), "a LIMIT on the emit itself");
    }

    #[test]
    fn a_reader_limit_that_reads_nothing_is_not_unconditional() {
        assert!(!reads_all(BreakerKind::Sort, vec![], vec![limit(0)]), "LIMIT 0 over the sort reads nothing");
    }

    #[test]
    fn a_zero_limit_whose_check_stops_everything_below_reads_all_or_none() {
        // D1486 (errorhold): the feeders check the count before any arm runs, as PG's Limit before its first pull
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        assert!(reads(a, BreakerKind::Sort, vec![], vec![limit(0)], true), "LIMIT 0 whose check stops the emit");
        let nil: &'static NodeList<'static> = Box::leak(Box::new(NodeList::nil()));
        let top = |n: i64| Op::Sort(SortDesc { keys: ArenaVec::new_in(a), presorted: 0, form: SortForm::TopK { count: Bound::Const(n), offset: Bound::All, ties: false }, tlist: nil });
        assert!(reads(a, BreakerKind::TopK, vec![top(0)], vec![limit(0)], true), "a top of 0 rows owed the root's stopping LIMIT");
        assert!(!reads(a, BreakerKind::TopK, vec![top(0)], vec![limit(0)], false), "a top of 0 rows whose LIMIT stops nothing below");
        assert!(!reads(a, BreakerKind::TopK, vec![top(0)], vec![], true), "a top of 0 rows with no LIMIT above it");
        assert!(reads(a, BreakerKind::TopK, vec![top(2)], vec![limit(2)], false), "a top of 2 rows reads its whole input");
    }

    /// D2341 / D2368: a union settled in another union's arm. Its emit reads its own slot and feeds the outer one, so the
    /// emit's source belongs to the inner union and its end to the outer union. `inner_first`: the nested arm is arm 0.
    fn nested(a: gravity_program::ArenaRef, inner_first: bool) -> PipelineGraph<'static> {
        let mut g = PipelineGraph::new(a);
        let (outer, inner) = if inner_first { (1, 0) } else { (0, 1) };
        let mat = |s: u8| Pipeline::serial(-1, End::Breaker(Breaker::sized(BreakerKind::Materialize, 0).in_slot(s)), Reason::NoDrivingSite);
        let feed = |s: u8| Op::Append(AppendDesc::Feed { slot: s, ordered: false });
        let union = |s: u8| Op::Append(AppendDesc::Union { slot: s, children: 2, prune: None });
        let mut push = |src: Source<'static>, d: Pipeline, ops: Vec<Op<'static>>| {
            let mut p = PipelineNode::new(a, 1, src, d);
            ops.into_iter().for_each(|op| p.push(op));
            g.pipelines.push(p);
        };
        if !inner_first {
            push(Source::Const, mat(outer), vec![feed(outer)]);
        }
        push(Source::Const, mat(inner), vec![feed(inner)]);
        push(Source::Const, mat(inner), vec![feed(inner)]);
        push(Source::Breaker { slot: inner }, mat(outer).consuming_one(inner), vec![union(inner), feed(outer)]);
        if inner_first {
            push(Source::Const, mat(outer), vec![feed(outer)]);
        }
        let sort = Pipeline::serial(-1, End::Breaker(Breaker::sized(BreakerKind::Sort, 64).in_slot(2)), Reason::NoDrivingSite);
        push(Source::Breaker { slot: outer }, sort.consuming_one(outer), vec![union(outer)]);
        push(Source::Breaker { slot: 2 }, Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming_one(2), vec![]);
        g.root = 5;
        g
    }

    #[test]
    fn a_union_settled_in_an_arm_reads_its_own_slot_and_feeds_the_outer_one() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let arena = Arena::new(1 << 17); // D3588: Pipeline 416 -> 424 bytes (source_of) passed 64 KiB
        for inner_first in [false, true] {
            let g = nested(arena.handle(), inner_first);
            let gx = Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
            let ps = gx.pipelines();
            let (e, f, o) = if inner_first { (2, vec![0, 1], vec![2, 3]) } else { (3, vec![1, 2], vec![0, 3]) };
            assert_eq!(admit(&gx, arena.handle(), e, union_of(&ps[e]).map(|u| u.0), 0, &mut |_: Ask<'_>| false), (e, f, false), "the inner emit's source reads its own union ({inner_first})");
            assert_eq!(admit(&gx, arena.handle(), e, feed_of(&ps[e]), 0, &mut |_: Ask<'_>| false), (4, o.clone(), false), "the inner emit's end feeds the outer union ({inner_first})");
            assert_eq!(admit(&gx, arena.handle(), o[0], feed_of(&ps[o[0]]), 0, &mut |_: Ask<'_>| false), (4, o, false), "the outer union's first feed ({inner_first})");
        }
    }
}
