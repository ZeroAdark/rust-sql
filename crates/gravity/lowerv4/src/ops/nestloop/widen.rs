//! D3014O (nest.parallel): whether a nest step's outer may widen over its inner, recorded as the inner site's `worker`
//! bit (gravity_program `SiteKind::IndexSource`): a marked step is not serial (`StepMech::serial`), each worker probes
//! the site through its own frozen-image btree probe (hosts index_probe.rs, bound by driver bind), and an execution
//! whose image cannot be built runs the pipeline on the leader (exec runner plan.rs, D3014O P3). The outer may widen
//! when every worker can run its own copy of the inner with nothing shared but the pages it reads:
//! - the inner is a bare probe of one site (`Inner::probe_site`: no rerun set, LIMIT, hash probe, COPY row, streamed
//!   group or one-time gate) whose pipeline is filters and projections only, perhaps under a Memoize (D3014 M2: its
//!   table is one per worker ordinal, exec ops/memoize/proto.rs), and the step fills no lazy Material (D2157);
//! - the inner makes no VOLATILE call (effects::visible::calls_visible), and the outer's rows reach an end no row order
//!   changes (effects::visible::wide_visible: a fold, a sort or a top-k with no LIMIT, lock, visible call or error;
//!   which equal member a group key or min / max shows is free at any width, D3085), FACTS F4 / F1;
//! - the site is a btree over plain columns of a heap relation, every key `column op value` over a bare executor parameter
//!   (not an InitPlan's) or a statement parameter or constant (no computed key: exec param_key.rs keeps those on the leader).
//!
//! D4532 (`spool`): or the inner reads a plan Material's eager spool (nest.spool's eager form) that is whole before the
//! outer's pipeline runs and only read after, so every worker walks the one record from its own position word (a
//! per-open frame word; the record's handle and count are words the fill wrote, read only). The guards: the nest has no
//! parameters; the inner is the spool's one reader with nothing above it but filters and projections; the fill takes
//! the quiet eager arm (raise-free, no VOLATILE call, the same rows at every rescan: `quiet_fill_licence`), held by no
//! error, refilled by no rerun set and not pulling a loud CTE, so it runs in pipeline order ahead of the outer (never
//! D2157's first-outer-row fill, which `lazy.rs` would make it: the step does not declare it, so the outer's call never
//! waits; an outer empty at run time then costs the fill, as D2157's eager fills over parallel-eligible scans do); the
//! outer's pipeline is parallel-eligible by the graph's reading; and the outer's rows reach `wide_visible`'s order-free
//! end or a hash build (`wide_build_visible`). Not under EXPLAIN ANALYZE (the lazy fill's counts are PostgreSQL's), and
//! not with road nest.parallel switched off (`gravity_road_off`), which keeps such a nest on the leader as before.
use gravity_decompose::join::JoinDesc;
use gravity_decompose::{Am, Op, Source};
use types_nodes::primnodes::ParamKind;
use gravity_program::{NestProbe, NestShape, SiteKind, StepShape};

use crate::face::kinds::KindId;
use crate::frame::px::Px;
use crate::rerun::inner::Inner;

/// Marks the site of the inner op `j` of pipeline `k` embeds as one a worker may probe, when the nest may widen; true
/// when it did. The facts are this frame's; the verdict is gravity_program's `step_serial` (the one serial predicate the
/// planner's width costing reads too). `lazy`: the step fills a lazy Material; `regroup`: a streamed group follows the
/// nest; `keep`: the inner holds an owner of its own (lower.rs).
pub(super) fn mark(px: &mut Px<'_, '_>, k: usize, j: usize, inner: &Inner<'_>, lazy: bool, regroup: bool, keep: bool) -> bool {
    let gx = px.gx;
    let Some(s) = inner.probe_site() else { return false };
    let Some(ik) = gx.pipelines().inner_of(k, j) else { return false };
    let Some(probe) = probe_facts(px, s, ik) else { return false };
    let shape = NestShape {
        probe: Some(probe),
        inner_plain: gx.pipelines()[ik].ops.iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_) | Op::Join(JoinDesc::Memo { .. }))),
        inner_volatile: crate::effects::visible::calls_visible(gx, ik).is_some(),
        end_order_visible: crate::effects::visible::wide_visible(gx, k).is_some(),
        lazy_material: lazy,
        regroup,
        keeps_owner: keep,
        spool: false,
    };
    if gravity_program::step_serial(&StepShape::Nest(shape)).is_some() {
        return false;
    }
    let SiteKind::IndexSource { nkeys, natts, leaves: false, ranges: false, .. } = px.sites[s].kind else { return false };
    px.sites[s].kind = SiteKind::IndexSource { nkeys, natts, worker: true, leaves: false, ranges: false };
    if inner.memoized() {
        fires::fire("nest.probe.memo"); // D3014 M2: a widened nest whose inner keeps its Memoize
    }
    true
}

/// D4532: whether the nest at op `j` of pipeline `k`, whose inner is `inner`, widens over an eager spool its fill made
/// whole ahead of the outer (the module doc); true when it does, and then the caller declares no D2157 lazy fill for it.
/// `regroup` and `keep` as `mark`'s. The verdict is `step_serial`'s over the frame's facts (`NestShape::spool`).
pub(super) fn spool(px: &Px<'_, '_>, k: usize, j: usize, inner: &Inner<'_>, regroup: bool, keep: bool) -> bool {
    let gx = px.gx;
    let ps = gx.pipelines();
    // the outer's pipeline may run on many workers by the graph's reading (a driving site, no other reason it keeps): a nest
    // whose outer never widens keeps D2157's wait, which skips the fill under an outer empty at run time
    if k >= ps.len() || !ps[k].desc.is_eligible() || crate::frame::instr::on() || crate::face::strategy::off().has(crate::face::strategy::NEST_PARALLEL) || !inner.bare_siteless() {
        return false;
    }
    if !matches!(ps[k].ops.get(j), Some(Op::Join(JoinDesc::Nest { params, .. })) if params.is_nil()) {
        return false;
    }
    let Some(ik) = ps.inner_of(k, j) else { return false };
    let ip = &ps[ik];
    let Source::Breaker { slot } = ip.source else { return false };
    if gx.source_kind(ik) != KindId::NestSpool || crate::rerun::union_arms::union_of(ip).is_some() {
        return false;
    }
    let Some(kf) = ip.desc.consumes.iter().position(|&s| s == slot).and_then(|i| ip.desc.consumes_from.get(i)).map(|l| l.0 as usize) else { return false };
    // the eager form, read by this inner alone, its fill quiet and run once in pipeline order ahead of the outer
    // D4569: or one of a SELECT's union nest copies (readers.rs `copies` / `union_nest`), each arm's copy over the one record
    let copy = || crate::rerun::spool::readers::union_nest(gx, k, j) && crate::rerun::spool::readers::copies(gx, kf).is_some_and(|c| c.contains(&ik));
    let eager = match (crate::lifetime::material::spool_fill(gx, kf), crate::rerun::spool::readers::reader_or_copies(gx, kf)) {
        (Ok((r, false)), Ok((_, false))) => r == ik,
        (Ok((_, false)), Ok((_, true))) => copy(),
        _ => false,
    };
    let quiet = crate::lifetime::material::quiet_fill_licence(gx, kf).is_some() && !crate::lifetime::material::refilled_ahead(gx, kf) && !crate::rerun::spool::form::deferred(gx, kf) && crate::rerun::spool::form::held(gx, kf).is_none() && !crate::effects::visible::material::pulls_loud_cte(gx, kf);
    let once = !gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(kf as u16)) || r.pipelines.contains(&(k as u16))) && crate::rerun::union_arms::union_of(&ps[kf]).is_none() && !px.held_sort.contains(&kf) && !px.lazy.iter().any(|&(x, q)| x == kf || x == k || q == k);
    if !(eager && quiet && once) {
        return false;
    }
    let shape = NestShape {
        probe: None,
        inner_plain: ip.ops.iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_))),
        inner_volatile: crate::effects::visible::calls_visible(gx, ik).is_some(),
        end_order_visible: crate::effects::visible::wide_visible(gx, k).is_some() && crate::effects::visible::wide_build_visible(gx, k).is_some(),
        lazy_material: false,
        regroup,
        keeps_owner: keep,
        spool: true,
    };
    if gravity_program::step_serial(&StepShape::Nest(shape)).is_some() {
        return false;
    }
    fires::fire("nest.spool.wide"); // shapecheck fire signal (D4532): a nest over a filled eager spool runs on every worker
    true
}

/// The probe facts of site `s`, the inner pipeline `ik`'s source: `None` when the site is not an index source.
fn probe_facts(px: &Px<'_, '_>, s: usize, ik: usize) -> Option<NestProbe> {
    let gx = px.gx;
    let d = px.sites.get(s)?;
    let SiteKind::IndexSource { leaves: false, ranges: false, .. } = d.kind else { return None };
    let btree = gx.facts.index(d.index).is_some_and(|i| i.btree && i.keys().iter().all(|&a| a > 0));
    let heap = gx.facts.rel(d.relation).is_some_and(|r| r.am == Am::Heap);
    // the probe's heap read deforms table columns only (no system lane: hosts RangeRecipe), and publishes the site's
    // RowId lane (each row's heap tid) as the leader's batched walk does
    let plain_cols = d.out_attnos.iter().all(|&a| a > 0) && d.lanes().iter().all(|l| matches!(l.id, gravity_program::LaneId::Attr(_) | gravity_program::LaneId::RowId));
    // every key a bare executor parameter (the outer row's, which the frame stores into the key cells at each re-open) or
    // a statement parameter word; never an InitPlan's (its InitLoad runs on the leader) nor a computed key (the host's)
    let bare_keys = match &gx.pipelines()[ik].source {
        Source::Index { keys, .. } => keys.iter().all(|k| match k.value.as_param() {
            Some(p) if matches!(p.paramkind, ParamKind::PARAM_EXEC) => crate::rerun::params::producer(gx, p.paramid) != Some(crate::face::kinds::KindId::NestInitplan),
            Some(_) => true,
            None => k.value.as_const().is_some(),
        } && matches!(k.form, gravity_decompose::graph::KeyForm::Op)),
        _ => false,
    };
    Some(NestProbe { btree, heap, plain_cols, bare_keys })
}
