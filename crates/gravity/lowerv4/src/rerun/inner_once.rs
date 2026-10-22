//! A nest inner's once rule (M8-NEST B1b; a helper of nest/inner.rs, split for its budget): exec admit binds an inner
//! walk's key words once (admit.rs `late_owner`), so a TABLESAMPLE argument bound at bind refuses D1630; an index, bitmap
//! or TID key bound at bind is the host's at every seek (lifetime/bound.rs `per_open`, `reopened`). A VOLATILE FROM function reading
//! none of the owner's parameters (D1085, retired) is read from PG's FunctionScan store, filled once and rewound per
//! re-open as ExecReScanFunctionScan does (srf/function_store.rs). A sort's run the inner reads with no rerun set is
//! filled once ahead of the owner and read again at every re-open (`kept_run`); under a nest, a fill that may raise is
//! filled at the nest's first outer row instead (`kept_run_lazy`, D3378). Registers nothing. Owner M8-NEST.
use gravity_decompose::join::JoinDesc;
use gravity_decompose::{Op, PipelineNode, Source};

use crate::admit::refusal::{Reason, ShapeWhy};
use crate::lifetime::bound::at_bind;

/// D1630: an inner's walk drives no program pipeline, so exec admit binds its key words once (admit.rs `late_owner`).
pub(crate) const INNER_WALK_AT_BIND: &str = "a nest inner's walk keyed by an expression over no executor parameter (bound once at bind; PG evaluates it at every rescan: D1630)";

/// D1630: inner `ip` (op `j` of pipeline `k`) refuses by name when its walk's key is bound at bind (a TABLESAMPLE
/// argument; an index, bitmap or TID walk's is the host's, params_key.rs `per_open`).
pub(crate) fn check(_gx: &crate::graph::Gx<'_, '_>, _k: usize, _j: usize, ip: &PipelineNode<'_>, node: i32) {
    let walk = match &ip.source {
        // D1643: a value evaluated at each open (decompose `SampleDesc::per_open`) is the host's at every seek, as an index key's
        Source::Sample { sample, .. } => sample.args.iter().chain(sample.repeatable).any(|a| at_bind(a) && !sample.opens(a)),
        _ => false,
    };
    if walk {
        crate::refuse!(node, Reason::Shape { why: ShapeWhy(INNER_WALK_AT_BIND) });
    }
}

/// D1018 (regress groupingsets#102 / #146 / #148: `array(select row(v.a, s1.*) from (select .. group by cube(..) order by
/// ..) s1)`): inner `ik` of op `j` of pipeline `k` reads a sort's run no rerun set refills (nothing under the owner reads
/// its parameters, so decompose keeps the run). The run is filled once ahead of the owner and the inner's emit reads it
/// from its start at every re-open (kernels rowsort.rs `begin_in`: a sealed run read in place again, else its merge laid
/// again over the runs). PostgreSQL 18.6 sorts the same rows again at every rescan (no EXEC_FLAG_REWIND under a correlated
/// SubPlan or a nest with parameters: ExecReScanSort, then ExecReScanAgg re-reads its input), so the reread is PG's rows
/// when the owner's inner raises nothing and calls nothing VOLATILE (effects.rs `refill_quiet` over the owner's facts; C1:
/// a STABLE call makes the same rows again): the fill ahead of the first outer row, or with none, shows nothing. A run
/// that holds rows (frame::iface `run_holds_rows`: not form G's records), not a set member's run (rerun/sets.rs
/// `emit_ok`), and the run consumed in the owner's region (`Pipes::consumed_at`).
/// The licence of the fill's run ahead comes back: its quiet code shows nothing there (`effects::unobservable`).
pub(crate) fn kept_run(gx: &crate::graph::Gx<'_, '_>, (k, j, ik): (usize, usize, usize)) -> Option<gravity_decompose::effects::Licence> {
    let ps = gx.pipelines();
    let Source::Breaker { slot } = ps[ik].source else { return None };
    let quiet = match ps[k].ops.get(j) {
        Some(Op::Join(JoinDesc::Nest { facts, .. })) => crate::effects::refill_quiet(facts.raise_free, facts.volatility),
        Some(Op::SubPlan(d)) => crate::effects::refill_quiet(d.raise_free, d.volatility),
        _ => false,
    };
    let producer = gx.life_of(ik, slot).map(|l| l.0 as usize);
    // D3378 (m8_d1018_kept_run K6): or the fill alone is quiet (effects.rs `fill_quiet`), whatever the owner's inner does
    // after the run: those ops run over the run's rows at every re-open as PG's do, so only the fill's placement could
    // show, and it shows nothing
    let kept = ps.consumed_at(ik, slot).is_some() && producer.is_some_and(|q| (quiet || crate::effects::fill_quiet(gx, q)) && crate::frame::iface::run_holds_rows(gx, q) && !crate::rerun::sets::in_owner_set(gx, q));
    kept.then(|| gravity_decompose::effects::unobservable(gravity_decompose::effects::Effects::NONE)).flatten()
}

/// D4101 (D1018's kept emit; S3 rkeep ra-hx3/g8-hagg-lat-exists): inner `ik` of op `j` of pipeline `k` reads a keyed
/// group's emit (a hashed aggregate's, a sorted group's or Unique's; the left group of a set operation, rerun/nestloop.rs
/// `setop_kept`) that no rerun set refills, under no Memoize. PostgreSQL 18.6 keeps a hashed aggregate's table at a
/// rescan no changed parameter reaches and re-reads it from its start (ExecReScanAgg; ExecReScanSetOp alike), and re-sorts
/// the same rows for a sorted group or Unique. Here the group's table is filled once ahead of the owner and the emit gathers
/// it again at every re-open (table/emit_group.rs: the fresh open's gather; its array a statement pin). That is PG's rows
/// when the owner's inner is quiet (effects.rs `owner_quiet`): the fill ahead shows nothing, and no raise or VOLATILE call after the
/// emit can depend on which record comes first (the gather's order is the table's, a multiset to PG's). The group's
/// producer is a graph pipeline in no owner's set, and the emit reads it at the owner's consumed region (rerun/sets.rs
/// `emit_ok`'s row C rule).
pub(crate) fn kept_emit(px: &crate::frame::px::Px<'_, '_>, (k, j, ik): (usize, usize, usize)) -> bool {
    use crate::face::kinds::KindId;
    let gx = px.gx;
    let ps = gx.pipelines();
    let Source::Breaker { slot } = ps[ik].source else { return false };
    let keyed = matches!(gx.source_kind(ik), KindId::KeyedEmitGroup | KindId::RunsGroup | KindId::RunsUnique);
    let producer = gx.life_of(ik, slot).map(|l| l.0 as usize).filter(|&q| q < ps.len());
    keyed
        && crate::effects::owner_quiet(gx, k, j)
        && crate::rerun::sets::owner_set(gx, ik).is_none()
        && producer.is_some_and(|q| !crate::rerun::sets::in_owner_set(gx, q))
        && ps.consumed_at(ik, slot).is_some_and(|at| px.consumed(ik, slot) == Some(at))
}

/// Construct C2 (b): `kept_run` or `kept_run_lazy` takes inner `ik` (op `j` of pipeline `k`). A run filled ahead of the
/// owner holds `kept_run`'s licence, stamped for the tracer (`Px::hold`) on what the reading that granted it covered, and
/// on what feeds each: where the owner declares its whole inner quiet (`effects::owner_quiet`, which implies `kept_run`'s
/// reading of the owner's facts) every pipeline that fills the run (a UNION ALL's arms, GROUPING SETS' phases:
/// mergejoin/lazy_outer.rs `fillers`), else the life's producer alone (`effects::fill_quiet`'s reading, or the owner's).
pub(crate) fn kept(px: &mut crate::frame::px::Px<'_, '_>, (k, j, ik): (usize, usize, usize)) -> bool {
    let gx = px.gx;
    let Some(l) = kept_run(gx, (k, j, ik)) else { return kept_run_lazy(gx, (k, j, ik)) };
    let ps = gx.pipelines();
    if let (true, &Source::Breaker { slot }) = (crate::frame::program::stamps_on(), &ps[ik].source) {
        let (all, one) = if crate::effects::owner_quiet(gx, k, j) {
            (Some(crate::ops::mergejoin::lazy_outer::fillers(gx, ik, slot)), None)
        } else {
            (None, gx.life_of(ik, slot).map(|l| l.0 as usize))
        };
        for q in all.into_iter().flatten().chain(one).filter(|&q| q < ps.len()) {
            let src = if let Source::Breaker { slot } = ps[q].source { Some(slot) } else { None };
            let fill = crate::ops::hashjoin::build_class::feed(gx, q, ps[q].desc.consumes.iter().copied().chain(src));
            fill.into_iter().chain(std::iter::once(q)).for_each(|x| px.hold(x, &l));
        }
    }
    true
}

/// D3378 (D1018's kept run, heldout rnd-3226 / m8_d1018_cte): `kept_run`'s run where the fill may raise (the nest's facts
/// not raise-free) but calls nothing VOLATILE, under a nest of a graph pipeline (op `j` of `k`; one level: a nest inside an
/// inner has no wait of its own, and a correlated SubPlan none). PostgreSQL sorts the run at the nest's first rescan, its
/// first outer row (a MATERIALIZED CTE's body runs once at its first fetch; a Sort with no REWIND re-sorts the same rows at
/// each later rescan and raises the same way), so the fill runs at that row instead of ahead of the owner
/// (rerun/spool/lazy.rs `declare`: D2157's wait over the run's slot): an outer that hands on no row never runs it, and a
/// fill that raises raises there. Not here: the outer's own source reading the same run (a CTE scanned on both sides of
/// the nest: the two readers' merges over one run, D1061), and a fill whose raise may come at the program's bind rather
/// than at a row (effects.rs `raises_at_rows`), which a deferred fill would still raise under an empty outer.
pub(crate) fn kept_run_lazy(gx: &crate::graph::Gx<'_, '_>, (k, j, ik): (usize, usize, usize)) -> bool {
    let ps = gx.pipelines();
    let Source::Breaker { slot } = ps[ik].source else { return false };
    if k >= ps.len() {
        return false;
    }
    let loud = matches!(ps[k].ops.get(j), Some(Op::Join(JoinDesc::Nest { facts, .. })) if crate::effects::waited_fill(facts.raise_free, facts.volatility));
    let Some(q) = gx.life_of(ik, slot).map(|l| l.0 as usize).filter(|&q| q < ps.len()) else { return false };
    let shared = matches!(ps[k].source, Source::Breaker { slot: s } if gx.life_of(k, s).is_some_and(|l| l.0 as usize == q));
    let src = if let Source::Breaker { slot } = ps[q].source { Some(slot) } else { None };
    let group = crate::ops::hashjoin::build_class::feed(gx, q, ps[q].desc.consumes.iter().copied().chain(src));
    loud && !shared && ps.consumed_at(ik, slot).is_some() && crate::frame::iface::run_holds_rows(gx, q) && !crate::rerun::sets::in_owner_set(gx, q) && group.iter().chain(std::iter::once(&q)).all(|&x| crate::effects::raises_at_rows(gx, x))
}
