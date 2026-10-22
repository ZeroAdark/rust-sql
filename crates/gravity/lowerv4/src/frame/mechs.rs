//! A graph's pipelines' mechanisms, each made by its kind's maker through the registry (plan §2.1): `Mechs::new` /
//! `Mechs::shaped` and `resolve`. Moved unchanged from frame.rs (lowerv4 plan §4.1 P1, frame.rs's budget: main 15:09Z
//! 09-27), which keeps the types (`Mechs`, the makers) and re-exports `resolve`.
use gravity_program::protos::ProtoKind;
use gravity_program::Pipeline;
use mcx::{CollectIn, GrowIn, Mcx, PgBox, PgVec};

use crate::face::end::EndMech;
use crate::ops::hashjoin::drain::arms::ArmRun;
use crate::ops::hashjoin::face::SideImpl;
use crate::face::kinds::KindId;
use crate::face::strategy::JOIN_BUILDSIDE;
use crate::face::source::SourceMech;
use crate::face::step::StepMech;
use crate::frame::diag;
use crate::frame::instr;
use crate::graph::Gx;
use crate::frame::frame::{EndMaker, Mechs, SourceMaker, StepMaker};
use crate::frame::px::Px;
use crate::admit::refusal::check_interrupts;
use crate::ops as registry;

/// D1716: a step kind's feeder gate for graph pipeline `k` (a second face row of step.limit's and step.gate's kinds): the
/// step a pipeline that feeds such a step's input alone runs first, `None` when `k` feeds none.
pub struct FeederMaker(pub for<'g, 'p> fn(&mut Px<'g, 'p>, usize) -> Option<PgBox<'g, dyn StepMech + 'g>>);

impl<'g> Mechs<'g> {
    /// Pipeline `k`'s mechanisms: the source's site numbered in `px`, the descriptor's driving site set to it.
    pub fn new(px: &mut Px<'g, '_>, k: usize, source: PgBox<'g, dyn SourceMech + 'g>, steps: PgVec<'g, PgBox<'g, dyn StepMech + 'g>>, end: PgBox<'g, dyn EndMech + 'g>) -> Mechs<'g> {
        let shape = px.after_of(k);
        Mechs::shaped(px, shape, k, source, steps, end)
    }

    /// `new` with the source's `k` arguments the graph pipeline whose source it is (`src`; a swap's other side), the
    /// end's the graph pipeline `k` follows (itself, or a pass's `after`), and every site / protocol keyed by the program
    /// pipeline `k` (`Px::ident`). A swapped pipeline's descriptor takes its source-side fields (driving site, parallel,
    /// rows, claims) from `src`'s and the rest (end, slots, lives) from its own graph pipeline's.
    pub(crate) fn shaped(px: &mut Px<'g, '_>, src: usize, k: usize, mut source: PgBox<'g, dyn SourceMech + 'g>, steps: PgVec<'g, PgBox<'g, dyn StepMech + 'g>>, end: PgBox<'g, dyn EndMech + 'g>) -> Mechs<'g> {
        let shape = px.after_of(k);
        if px.lazy.iter().any(|l| l.1 == k) {
            source.in_order();
        }
        let (site, from) = px.making(src, k, |px| {
            let mut draft = px.take_site(k).or_else(|| source.site_desc(px.arena));
            if let Some(d) = draft.as_mut() {
                source.seal(px, src, d);
            }
            (draft.map(|d| crate::pipes::place_site(px, k, d)), source.pipeline_desc(px, src))
        });
        px.set_pipeline_site(k, site);
        // D3585: only the protocols still at their drafted site are visited (`Px::draft_protos`); one given its site here or
        // elsewhere (material, rerun inner) leaves the list
        let (protos, s) = (&mut px.protos, site.map_or(-1, |i| i as i16));
        px.draft_protos.retain(|&i| match protos.get_mut(i as usize) {
            Some(p) if p.site == crate::frame::px::DRAFT_SITE => {
                if p.pipeline as usize == k {
                    p.site = s;
                }
                p.site == crate::frame::px::DRAFT_SITE
            }
            _ => false,
        });
        let base = match src == shape {
            true => from,
            false => Pipeline { driving_site: from.driving_site, parallel: from.parallel, predicted_rows: from.predicted_rows, claims: from.claims, ..px.gx.pipelines()[shape].desc },
        };
        let mut desc = px.making(shape, k, |px| end.pipeline_desc(px, shape, base));
        if desc.driving_site >= 0 || site.is_some() {
            desc.driving_site = site.or_else(|| source.nominal_site(px)).map_or(-1, |i| i as i16);
        }
        // a nominal site is the claims' (`Px::claims_site`): a group emit over this pipeline's end takes it as its own
        px.set_nominal_site(k, usize::try_from(desc.driving_site).ok().filter(|_| site.is_none()));
        // D824 / D1005: a lazy build's probe pipeline runs on the leader, as a pool-unsafe host site's does: the build runs
        // once, when the first row of the whole outer reaches the probe; M8: so does a step that drives an embedded inner
        let lazy = px.lazy.iter().any(|&(_, q)| q == k) && gravity_program::step_serial(&gravity_program::StepShape::HashProbe { lazy: true, clears_marks: false, ordered: false }).is_some();
        if desc.is_eligible() && (lazy || px.serial.contains(&k) || steps.iter().any(|s| s.serial())) {
            desc.parallel = gravity_program::Parallel::Ineligible { site: desc.driving_site, reason: gravity_program::Reason::Host };
        }
        // KEY-PREDICTED-ROWS road (b): the graph pipeline whose estimates the descriptor carries (the source side's rows, the
        // end's build rows and, D4047, its rows at the end), so an admission that takes this program for another statement
        // binds that statement's own
        let g = px.gx.pipelines();
        desc.rows_of = [src, shape].into_iter().find(|&j| g[j].desc.predicted_rows == desc.predicted_rows).map_or(u16::MAX, |j| j as u16);
        desc.build_of = [shape, src].into_iter().find(|&j| desc.build_rows != 0 && g[j].desc.build_rows == desc.build_rows).map_or(u16::MAX, |j| j as u16);
        desc.end_of = [shape, src].into_iter().find(|&j| desc.end_rows != 0 && g[j].desc.end_rows == desc.end_rows).map_or(u16::MAX, |j| j as u16);
        desc.source_of = site.map_or(u16::MAX, |_| src as u16); // D3588: the registry binds the driving site to `src`'s source
        Mechs { k, source, steps, end, site, desc, words: None, state_word: None }
    }
}

/// Every pipeline's mechanisms, each graph pipeline followed by the passes its makers added (`Px::add_pass`): program
/// order, graph order but for the arms `arm_order` moves. Admission (lib.rs) has refused every kind no table provides, so
/// `need_kind` refusing here is the registry disagreeing with admission (a `NotBuilt` all the same, never a wrong program).
pub fn resolve<'g>(px: &mut Px<'g, '_>) -> Vec<Mechs<'g>> {
    let gx = px.gx;
    let mcx = px.mcx();
    // join.buildside (H18 c): the swaps its mechanism takes, when the road applies, is on and has one
    let side = px.strategies.has(JOIN_BUILDSIDE).then(|| registry::strategy(JOIN_BUILDSIDE).and_then(|m| m.downcast_ref::<SideImpl>())).flatten().map(|s| s.0);
    let swaps = side.map_or_else(|| PgVec::new_in(mcx), |s| s.swaps(px));
    let runs = gx.arm_runs();
    let mut out = Vec::with_capacity(gx.pipelines().len()); // alloc: Program
    for k in arm_order(gx.pipelines().len(), &runs, mcx) {
        let p = &gx.pipelines()[k];
        check_interrupts(p.node_id);
        let first = out.len();
        let (_, serial) = gravity_fn::resolved::pipeline_scope(|| {
            let (src, source, mut steps, end) = match (side, swaps.iter().find(|s| s.build == k || s.probe == k)) {
                // the build: the probe side's source and its ops before the probe, into the swapped build end
                (Some(side), Some(s)) if s.build == k => {
                    uncounted(gx, &[s.probe, k]);
                    let (source, steps) = px.making(s.probe, k, |px| (make_source(px, s.probe), (0..s.at).map(|j| make_step(px, s.probe, j)).collect_in(mcx)));
                    (s.probe, source, steps, px.making(k, k, |px| side.make_build(px, s)))
                }
                // the probe: the build side's source and ops, the swapped probe, this pipeline's ops after it and its end
                (Some(side), Some(s)) => {
                    uncounted(gx, &[s.build, k]);
                    let (source, mut steps) = px.making(s.build, k, |px| (make_source(px, s.build), (0..gx.pipelines()[s.build].ops.len()).map(|j| make_step(px, s.build, j)).collect_in(mcx)));
                    let end = px.making(k, k, |px| {
                        steps.push_in(side.make_probe(px, s));
                        steps.extend_in((s.at + 1..p.ops.len()).map(|j| make_step(px, k, j)));
                        make_end(px, k)
                    });
                    (s.build, source, steps, end)
                }
                _ => {
                    plan_bounds(gx, k);
                    px.making(k, k, |px| (k, make_source(px, k), (0..p.ops.len()).map(|j| make_step(px, k, j)).collect_in(mcx), make_end(px, k)))
                }
            };
            // D1716: a pipeline that feeds a Limit or a one-time gate alone checks it first and pulls nothing when it would
            let gate = [KindId::StepLimit, KindId::StepGate].into_iter().filter_map(registry::kind_as::<FeederMaker>).find_map(|f| px.making(k, k, |px| (f.0)(px, k)));
            steps.extend_in(gate);
            if let Some(t) = px.tails.iter().find(|t| t.ident == k && t.shape != k) {
                uncounted(gx, &[t.shape]); // its ops run here, after this pipeline's
            }
            steps.extend_in(px.take_tail(k)); // M7-DRAIN (V4R-378): a fused drain's tail, made by this pipeline's end (`open_tail`)
            steps.iter_mut().for_each(|s| s.end_is(&*end));
            // D732-rest: a COPY-marked subquery's pipeline enters its stored row (copy_row.rs)
            let source = match if src == k { crate::rerun::epq::copy_row::entry(px, k) } else { None } {
                Some(e) => crate::rerun::epq::copy_row::wrap(mcx, source, e),
                None => source,
            };
            out.push(Mechs::shaped(px, src, k, source, steps, end)); // alloc: Program
            for (v, mut pass) in px.take_passes(k) {
                if !instr::planned_pipe(v) {
                    uncounted(gx, &[k, px.after_of(v)]); // a pass that runs the rows again
                }
                pass.steps.extend_in(px.take_tail(v)); // B4c-2: a spill pair's fused drain tail (the end remade under the pass's id)
                pass.steps.iter_mut().for_each(|s| s.end_is(&*pass.end));
                out.push(Mechs::new(px, v, pass.source, pass.steps, pass.end)); // alloc: Program
            }
        });
        // M6-HOSTED (main 09-28, wp1): a comparator its makers bound that a pool worker cannot call (it fills its state from
        // the thread-local type cache: an array's, a range's, a record's, an enum's, a user type's) runs on the leader
        if serial {
            for m in out[first..].iter_mut().filter(|m| m.desc.is_eligible()) {
                m.desc.parallel = gravity_program::Parallel::Ineligible { site: m.desc.driving_site, reason: gravity_program::Reason::Host };
            }
        }
    }
    arms_span(px, &runs, &out);
    // EXPLAIN ANALYZE: each quiet InitPlan (the effects home's `init_unseen`: it neither acts nor may raise; a CTE's plan is
    // its CTE Scans' to run) is marked, so EXPLAIN may print it never executed where PostgreSQL never read its parameters
    // (exec instrument.rs `init_ran`)
    for i in gx.graph.init_plans.iter().filter(|i| !matches!(i.link, gravity_decompose::ops::shared::subplan::SubLink::Cte) && i.param >= 0) {
        if crate::effects::visible::pgrc::init_unseen(gx, i.param) {
            instr::init_quiet(i.plan_id);
        }
    }
    instr::seal_plan(&out.iter().map(|m| m.k).collect_in(mcx)[..], px.mcx());
    diag::stop(px);
    if px.protos.iter().any(|p| p.site == crate::frame::px::DRAFT_SITE) {
        crate::refuse!(gx.root_node(), crate::admit::refusal::Reason::Bug { payload: "a protocol names a drafted site no pipeline numbered" });
    }
    out
}

/// EXPLAIN ANALYZE's node boundaries of graph pipeline `k`, whose function runs its ops in order (`instr::Bounds`): a
/// write's node counts its own rows (RETURNING's) and loops.
fn plan_bounds(gx: &Gx<'_, '_>, k: usize) {
    if !instr::on() {
        return;
    }
    // a union arm a nest inner streams (or a Material's reader pulls) runs inside its reader: this pipeline runs nothing
    // (rerun/inner.rs `instr_plan` counts the arm where it runs)
    if gx.source_kind(k) == KindId::NestLoop {
        return;
    }
    let p = &gx.pipelines()[k];
    let entries: PgVec<'_, (i32, u16, bool)> = p.node_ops.iter().map(|n| (n.node_id, n.first_op, n.producer_top)).collect_in(gx.mcx);
    let owner = |j: usize| entries.iter().rposition(|e| e.1 as usize <= j).map(|i| entries[i].0);
    let writes: PgVec<'_, i32> = p.ops.iter().enumerate().filter(|(_, op)| matches!(op, gravity_decompose::Op::Write(_))).filter_map(|(j, _)| owner(j)).collect_in(gx.mcx);
    let hash = matches!(gx.end_kind(k), crate::graph::EndKind::Breaker(gravity_abi::breaker::BreakerKind::HashBuild));
    // D3815: a nest's catalog / keyed build (decompose join.rs `map_catalog`) is no Hash node of PostgreSQL's: its entry is
    // the nest's, whose rows are the probe's; and its inner's nodes ran once here where PostgreSQL rescans them per outer row
    let catalog = hash && catalog_build(gx, k);
    if catalog {
        instr::uncounted(entries.iter().filter(|e| !e.2).map(|e| e.0));
    }
    let lead = p.ops.iter().take_while(|op| matches!(op, gravity_decompose::Op::Filter(_))).count();
    instr::plan_pipeline(k, &entries, p.ops.len(), hash && !catalog, lead, &|n| writes.contains(&n));
}

/// Graph pipeline `k` builds the table a nest's catalog probe reads (decompose join.rs `map_catalog`: nest.catalog,
/// nest.eqkey).
fn catalog_build(gx: &Gx<'_, '_>, k: usize) -> bool {
    use gravity_decompose::join::JoinDesc;
    let ps = gx.pipelines();
    let life = Some(gravity_program::LifeId(k as u16));
    (0..ps.len() + ps.inners().len()).any(|j| ps[j].ops.iter().any(|op| matches!(op, gravity_decompose::Op::Join(JoinDesc::Probe { slot, catalog: true, .. }) if gx.life_of(j, *slot) == life)))
}

/// EXPLAIN ANALYZE: the nodes of graph pipelines `ks` run out of their order somewhere; no boundary counts them.
fn uncounted(gx: &Gx<'_, '_>, ks: &[usize]) {
    for &k in ks.iter().filter(|&&k| k < gx.pipelines().len()) {
        instr::uncounted(gx.pipelines()[k].node_ops.iter().map(|n| n.node_id));
    }
}

/// D1752 (drain/arms.rs `arm_runs`): graph order, but each run's arm `i` right before child `i + 1`'s first pipeline (the
/// last arm before the first pipeline decompose pushed after the children), as PG's Append runs child `i` whole first.
pub(crate) fn arm_order(n: usize, runs: &[ArmRun], mcx: Mcx<'_>) -> Vec<usize> {
    let mut before: PgVec<'_, PgVec<'_, usize>> = (0..n + 1).map(|_| PgVec::new_in(mcx)).collect_in(mcx);
    let mut moved = mcx::pg_vec![in mcx; false; n];
    for r in runs {
        for (i, q) in r.arms.clone().enumerate() {
            before[r.starts[i + 1]].push_in(q);
            moved[q] = true;
        }
    }
    let mut order = Vec::with_capacity(n); // alloc: Program
    for p in 0..=n {
        order.extend(before[p].iter().copied());
        if p < n && !moved[p] {
            order.push(p);
        }
    }
    order
}

/// D1752: a moved run's `Arms` (append/lower.rs declares it over the consecutive graph arms) counts the program pipelines
/// from its first arm to its last, the children's between (no arm: they produce other slots, `arm_run`'s end-slot rule).
/// D3437: a pass its maker spliced after an arm and counted into the run (spill/pair.rs `grows`: a spill pair's `P'`, the
/// run's last arm) stays in it: the passes the run's `arg` holds past its arms are the last arm's first ones, after it.
fn arms_span(px: &mut Px<'_, '_>, runs: &[ArmRun], out: &[Mechs<'_>]) {
    for r in runs {
        let at = |k: usize| out.iter().position(|m| m.k == k);
        let (Some(a), Some(z)) = (at(r.arms.start), at(r.arms.end - 1)) else { continue };
        let passes = out.iter().filter(|m| !r.arms.contains(&m.k) && r.arms.contains(&px.after_of(m.k))).count();
        let tail = out[z + 1..].iter().take_while(|m| px.after_of(m.k) == r.arms.end - 1).count();
        for p in px.protos.iter_mut().filter(|p| p.kind == ProtoKind::Arms && p.pipeline as usize == r.arms.start) {
            match (p.arg as usize).checked_sub(r.arms.len()) {
                Some(0) => p.arg = (z - a + 1) as u64,
                Some(g) if g == passes && g == tail => p.arg = (z + g - a + 1) as u64,
                _ => {}
            }
        }
    }
}

/// Graph pipeline `k`'s source / op `j` / end, made by the maker of the kind admission admitted for it.
/// Under the all-blockers mode (diag.rs) a maker's refusal is recorded and leaves a placeholder.
fn make_source<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    let (gx, node) = (px.gx, px.gx.pipelines()[k].node_id);
    diag::make(px, k, node, |px| (crate::frame::iface::kind_impl::<SourceMaker>(&node, gx.source_kind(k)).0)(px, k), diag::source)
}

fn make_step<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let (gx, node) = (px.gx, px.gx.pipelines()[k].node_id);
    diag::make(px, k, node, |px| (crate::frame::iface::kind_impl::<StepMaker>(&node, gx.step_need(k, j)).0)(px, k, j), diag::step)
}

fn make_end<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    let (gx, node) = (px.gx, px.gx.pipelines()[k].node_id);
    // D1292: the end feeding a raising Sort's breaker raises at its open (sort_raise::wrap_feeder).
    diag::make(px, k, node, |px| crate::ops::sort::sort_raise::wrap_feeder(gx, k, (crate::frame::iface::kind_impl::<EndMaker>(&node, gx.end_maker(k)).0)(px, k)), diag::end)
}
