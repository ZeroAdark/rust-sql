//! The program (lowerv4 plan §2.1): the mechanisms resolved in one compile context, every pipeline's function emitted and
//! verified (IR, cancel back edges), and the descriptors `assemble` takes: the write plan a row-op end declares (a SELECT
//! otherwise), the program's v4 protocols. The lowering stops between pipelines on a pending interrupt (D231).
use gravity_ir::builder::ProgramBuilder;
use gravity_ir::{Block, Func};
use gravity_lower::{Descs, LowerOpts, Lowered, WordPlan};
use gravity_program::protos::{ProtoKind, V4Descs};
use gravity_program::protos::NO_SLOT;
use gravity_program::{ArenaRef, ArenaVec, Layout, LifeId, Pipeline, WritePlan};

use crate::face::policy::{Width, WidthDecl, WidthImpl};
use crate::face::strategy::{Strategy, WIDTH_WORK, WIDTH_WRITE};
use crate::frame::env;
use crate::frame::env::Core;
use crate::face::source::SourceMech;
use crate::face::step::StepMech;
use crate::frame::frame::{self, Mechs, SourceMaker, StepMaker};
use crate::frame::px::{ExtraPass, Out, Px, OUT_ROWS};
use crate::frame::{cancel, instr, stop};
use crate::graph::Gx;
use crate::hir::{Ex, Hosted, Ir};
use crate::props::Props;
use crate::admit::refusal::{check_interrupts, Reason};
use mcx::{CollectIn, GrowIn, PgBox, PgVec};

/// The graph lowered once admission passed (lib.rs `lower_graph`): its mechanisms resolved, then emitted; an EXPLAIN
/// ANALYZE's lowering (`opts.instrument`) carries its counter cells to `assemble` in `Descs::instr`.
/// Roads width.work / width.write (H15): a pipeline over a claimable source declares its width inputs, when either road
/// applies and is on.
fn width(px: &mut Px<'_, '_>, m: &Mechs<'_>) {
    let on = |w: Width, r: Strategy| px.strategies.has(r) && w.on();
    let d = WidthDecl { work: on(Width::Work, WIDTH_WORK), leader_only: on(Width::Write, WIDTH_WRITE), walk: false };
    if m.desc.driving_site < 0 || !(d.work || d.leader_only) {
        return;
    }
    let node = px.gx.pipelines()[px.after_of(m.k)].node_id;
    let road = if d.work { Width::Work } else { Width::Write };
    road.need::<WidthImpl>(&node).0.declare(px, m.k, d);
}

/// `diag`: the all-blockers mode's record (`frame/diag.rs`), `None` off.
pub fn lower<'p>(arena: ArenaRef, gx: &Gx<'_, 'p>, props: &Props, plan: &mut dyn WordPlan<'p>, opts: LowerOpts, diag: Option<&core::cell::RefCell<super::diag::Log>>) -> Lowered {
    let ((mut l, cells), used) = crate::mechs_used::used_scope(|| instr::scope(opts.instrument, || {
        let mut px = Px::new(arena, gx, plan);
        px.diag = diag;
        px.shape = opts.shape;
        px.strategies = crate::applies::applicable(props);
        // D3233 (road case.select): the expressions this lowering emits run a CASE arm that computes anything only where taken
        let cs = crate::face::strategy::CASE_SELECT;
        let _case_select = gravity_expr::emit::scalar::CaseSelect::scope(px.strategies.has(cs) && crate::face::source::on(crate::admit::refusal::Variant::Strategy(cs)));
        // D4822 (road text.substr.ascii): the hosted substrings this lowering binds take their ASCII twin
        let _substr_ascii = gravity_expr::nodes::func::SubstrAscii::scope(crate::rep::varlena::substr_ascii::on());
        crate::rerun::params::reserve(&mut px); // M8 L0-c: the executor parameters' words first (params.rs `word`)
        let mut mechs = frame::resolve(&mut px);
        crate::frame::publish::insert(&mut px, &mut mechs, opts.publish); // D2798: an identity cursor's row, before its quals
        // D3610: a cancel point per pipeline here too, as in `resolve` and `functions` (charged to the root: a mechanism
        // `publish::insert` added has no graph pipeline)
        mechs.iter_mut().for_each(|m| {
            check_interrupts(gx.root_node());
            crate::face::consts::walk(&mut px, m.k, m.site, &mut m.desc) // topk.walk (V4R-148)
        });
        mechs.iter().for_each(|m| {
            check_interrupts(gx.root_node());
            width(&mut px, m)
        });
        emit(&mut px, mechs)
    }));
    l.descs.instr = cells.iter().map(|c| (c.node, c.ctr as u8)).collect(); // alloc: Program
    l.descs.used = used;
    l
}

/// One function per pipeline, `p<k>`, each after an interrupt check (D231): a cancel lands between two pipelines' emits,
/// never inside one. The rig drives this with its own mechanisms (a long synthetic compile, cancelled part way).
/// D4193: each emit runs in `pipeline_scope` too, so a site its code binds for the leader alone (keysem `user_key_site`: a
/// user key's classes, user code through fmgr) keeps that pipeline off the pool, as a maker's mark does in `resolve`.
/// D3168 S0: a pipeline `split` allows (by its graph index) and whose probe stages lowers a staged function beside its own
/// (frame.rs `pipeline_fns_in`) in a module `side` makes, or none where its memo tag (`tag`, by graph index) says no call
/// chose it; the third list holds each such plain function with its staged one (`emit` checks a lowered one).
#[allow(clippy::type_complexity)]
pub fn functions<'m>(pb: &mut ProgramBuilder, core: Core, hir: &Ir, exs: &'m [Ex<'_>], mechs: &mut [Mechs<'_>], node_of: &dyn Fn(usize) -> i32, split: &dyn Fn(usize) -> bool, (side, tag): (frame::SideModule<'_>, &dyn Fn(usize) -> Option<u64>), mcx: mcx::Mcx<'m>) -> (Vec<Func>, PgVec<'m, PgVec<'m, Block>>, PgVec<'m, (Func, frame::StagedFn<'m>)>) {
    let mut funcs = Vec::with_capacity(mechs.len()); // alloc: Program
    let mut bounded = mcx::reserved_in(mechs.len(), mcx);
    let mut callees = PgVec::new_in(mcx);
    let claimed = crate::face::ext::claimed(exs);
    for m in mechs.iter_mut() {
        let node = node_of(m.k);
        check_interrupts(node);
        let split = split(m.k);
        let (((f, b), staged), serial) = gravity_fn::resolved::pipeline_scope(|| frame::pipeline_fns_in(pb, core, hir, exs, &claimed, &mcx::pg_format!(mcx, "p{}", m.k), node, m, split, (side, tag(m.k)), mcx));
        if serial && m.desc.is_eligible() {
            m.desc.parallel = gravity_program::Parallel::Ineligible { site: m.desc.driving_site, reason: gravity_program::Reason::Host };
        }
        funcs.push(f); // alloc: Program
        bounded.push_in(b);
        callees.extend_in(staged.map(|c| (f, c)));
    }
    (funcs, bounded, callees)
}

/// D3168 S0: the graph pipelines of the program's arm runs (`ProtoKind::Arms`, still in graph order here: a run from its
/// first arm's `k` over `arg` pipelines of `mechs`) with a twin candidate in their run: another pipeline of the same source
/// kind and op kinds, which twin.rs may fold with it into one body by their code (D3585: a union's arms over partitions of
/// one shape); a staged callee per arm would defeat that, so they keep one function. A spill pair's P and P' (other
/// sources) split.
fn arm_runs<'m>(gx: &Gx<'_, '_>, protos: &[gravity_program::protos::Proto], mechs: &[Mechs<'_>], mcx: mcx::Mcx<'m>) -> PgVec<'m, bool> {
    let n = mechs.iter().map(|m| m.k + 1).max().unwrap_or(0);
    let mut arm = mcx::pg_vec![in mcx; false; n];
    let shape = |k: usize| (k < gx.pipelines().len()).then(|| gx.source_kind(k));
    let same = |a: usize, b: usize| {
        let (pa, pb) = (&gx.pipelines()[a].ops, &gx.pipelines()[b].ops);
        shape(a) == shape(b) && pa.len() == pb.len() && pa.iter().zip(pb.iter()).all(|(x, y)| core::mem::discriminant(x) == core::mem::discriminant(y))
    };
    for p in protos.iter().filter(|p| p.kind == ProtoKind::Arms) {
        let at = mechs.iter().position(|m| m.k == p.pipeline as usize).unwrap_or(mechs.len());
        let run: PgVec<'_, usize> = mechs.iter().skip(at).take(p.arg as usize).map(|m| m.k).filter(|&k| k < gx.pipelines().len()).collect_in(mcx);
        for &a in run.iter() {
            if run.iter().any(|&b| b != a && same(a, b)) {
                arm[a] = true;
            }
        }
    }
    arm
}

/// Verify once (main 09-24): `verify`'s verdict is a function of the IR and the bounded loops alone, so an IR whose key
/// (a 128-bit FNV of its derived `Hash`, the functions and their bounded blocks mixed in) verified before in this
/// process is not verified again; direct-mapped, an evicted key verifies again. Under `pgrust.gravity_front_check` (the
/// harnesses' shadow checks: the gate's regress, gravity-e2e.sh, the rig) every lowering verifies.
const VERIFIED_SLOTS: usize = 1024;
static VERIFIED: pgsync::Mutex<[u128; VERIFIED_SLOTS]> = pgsync::Mutex::new([0; VERIFIED_SLOTS]);

/// FNV-1a over 128 bits, a word per `write_*`: the key's hasher. The one hash here outside KeyHash / gravity_dict's
/// Fp128 (main 09-24, measured on box 39): a non-adversarial 128-bit key over IR this process emitted, where a collision
/// can only skip a verifier pass, never change an answer; Fp128 over the Hash byte stream cost 2/3 of the verify it
/// skipped, this 1/5 to 1/4.
struct Fnv128(u128);

impl std::hash::Hasher for Fnv128 {
    fn finish(&self) -> u64 {
        (self.0 >> 64) as u64
    }
    fn write(&mut self, bytes: &[u8]) {
        for c in bytes.chunks(8) {
            let mut w = [0u8; 8];
            w[..c.len()].copy_from_slice(c);
            self.write_u64(u64::from_le_bytes(w) ^ ((c.len() as u64) << 56));
        }
    }
    fn write_u64(&mut self, w: u64) {
        self.0 = (self.0 ^ w as u128).wrapping_mul(0x0000_0000_0100_0000_0000_0000_0000_013b);
    }
    fn write_u32(&mut self, w: u32) {
        self.write_u64(w as u64)
    }
    fn write_usize(&mut self, w: usize) {
        self.write_u64(w as u64)
    }
}

/// The key: the IR's code key (`code`: gravity_lower `code_key`, the backend's content key of the whole IR, taken once per
/// lowering; D3886: this memo hashed the IR's derived `Hash` itself, ~6.5% of a lowering, and the backend hashed it again),
/// or `carried` in its place (the pre-pass program's key `instr::carry` returns: the IR it leaves is a function of that
/// key, so an EXPLAIN ANALYZE's lowering walks its IR once, 5.4 (1)), then each function and its bounded blocks.
fn verified_key(code: u128, carried: Option<u128>, funcs: &[Func], bounded: &[PgVec<'_, Block>]) -> u128 {
    use std::hash::Hasher;
    let mut h = Fnv128(0x6c62_272e_07bb_0142_62b8_2175_6295_c58d);
    let (tag, k) = match carried {
        Some(k) => (u64::MAX, k),
        None => (u64::MAX - 1, code),
    };
    [tag, k as u64, (k >> 64) as u64].into_iter().for_each(|w| h.write_u64(w));
    for (f, b) in funcs.iter().zip(bounded) {
        [f.0, u32::MAX].into_iter().chain(b.iter().map(|b| b.0)).for_each(|w| h.write_u32(w));
    }
    h.0 | 1
}

/// The finished IR verified: every function well formed, every back edge checked for cancel (or bounded by a checked
/// loop), once per IR (`VERIFIED`). An interrupt check per function keeps a long verify cancellable. Returns the IR's code
/// key (`Lowered::key`), taken here once; `None` for a carried key's IR (an instrumented program: the backend keys it).
/// The verifier's tables live in a child of `m`, the lowering's planning context, dropped when the verify ends.
pub fn verify(ir: &gravity_ir::Program, carried: Option<u128>, funcs: &[Func], bounded: &[PgVec<'_, Block>], node: i32, m: mcx::Mcx<'_>) -> Option<u128> {
    let code = carried.is_none().then(|| gravity_lower::code_key(ir));
    let key = verified_key(code.unwrap_or(0), carried, funcs, bounded);
    let slot = (key >> 118) as usize % VERIFIED_SLOTS;
    if VERIFIED.lock().unwrap_or_else(|e| e.into_inner())[slot] == key && !crate::inputs::verify_always() {
        return code;
    }
    #[cfg(test)]
    tests::VERIFY_RUNS.with(|c| c.set(c.get() + 1));
    let scratch = m.context().new_child("GravityVerify");
    let m = scratch.mcx();
    if let Err(e) = gravity_ir::verify::verify_in(ir, m) {
        eprintln!("gravity lowerv4: IR verify failed: {e:?}");
        crate::refuse!(node, Reason::Bug { payload: "the frame emitted IR that does not verify" });
    }
    for (i, &f) in funcs.iter().enumerate() {
        check_interrupts(node);
        if cancel::verify(ir.func(f), &bounded[i], m).is_err() {
            crate::refuse!(node, Reason::Bug { payload: "a loop without a cancel check on its back edge" });
        }
    }
    VERIFIED.lock().unwrap_or_else(|e| e.into_inner())[slot] = key;
    code
}

/// A top drain whose group's emit cannot keep its array past its call, in a program that runs a nested pipeline (D2561).
pub(crate) const DRAIN_ARRAY_NOT_KEPT: &str = "a top drain over a group in a program with a nested run, whose emit cannot keep its array (D2561)";

/// D2561: whether the program runs a pipeline nested inside another pipeline's call (D1036's on-demand groups and a pulled
/// CTE), known once every pipeline is made: a lazy build (D824), an InitPlan (at its first read, or an eager one at its
/// reader's start), a rerun set, a LAZY hashed SubPlan's set, a pulled set (a CTE's, a window's fill).
fn runs_nested(px: &Px<'_, '_>) -> bool {
    let lazy_set = |p: &gravity_program::protos::Proto| p.kind == ProtoKind::SubHash && p.arg & gravity_kernels::ops::shared::object::entries::SUBHASH_LAZY != 0;
    !px.lazy.is_empty() || px.protos.iter().any(|p| p.kind == ProtoKind::Init || p.kind == ProtoKind::Rerun || gravity_program::protos::pulled(p) || lazy_set(p))
}

/// D2561: the top pipeline graph pipeline `k` drains, when `k` drains a top over a group's records (form G): its source is
/// a Sort breaker whose own source is a group's breaker (topn/lower.rs `make_drain`'s shape, whichever of its makers takes it).
fn top_drain(gx: &Gx<'_, '_>, k: usize) -> Option<usize> {
    let gravity_decompose::Source::Breaker { slot } = gx.pipelines()[k].source else { return None };
    let t = gx.life_of(k, slot)?.0 as usize;
    (gx.pipelines()[t].desc.breaker().is_some_and(|b| b.kind == gravity_abi::breaker::BreakerKind::Sort) && gx.drained_group(t).is_some()).then_some(t)
}

/// C2 (b): lowerings stamp the tracer's gates (`gate_stamp`) only while the tracer is on. Off until the executor sets
/// it where it registers the lowerer (exec knobs/lowerer.rs, from knobs/diag.rs's `PGRUST_GRAVITY_AHEAD_TRACE`), so a
/// lowering with the tracer off pays one load (the lowering's instruction budget: tests/lower_insns_guard.rs).
static GATE_STAMPS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Set whether lowerings stamp the tracer's gates: the executor, once, where it registers the lowerer.
pub fn set_gate_stamps(on: bool) {
    GATE_STAMPS.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Whether lowerings stamp the tracer's gates and licences now (`set_gate_stamps`).
#[inline]
pub(crate) fn stamps_on() -> bool {
    GATE_STAMPS.load(std::sync::atomic::Ordering::Relaxed)
}

/// C2 R3: the licence stamp of graph pipeline `q` (program `Pipeline::licence`, read by the tracer only): the licence the
/// lowering held for it (`Px::held_kind`). A pipeline the lowering marked for the plan-order re-run (`Px::licensed`) whose
/// code no recheck hold names: `Recheck` over a `Plain` or `Accepted` hold (both licence the run; the recheck says more of
/// it), `Rests` with none (an InitPlan run at its open, no licence for its own run), so `Recheck-after-send` reads it. The
/// root pipeline's mark for mapping's licence (`PipelineGraph::licensed`) names no code of its own; the pipeline that
/// runs a node that licence was taken for (`PipelineGraph::licensed_nodes`: a group folded whole, a window's fill, a
/// sort's input read whole, a CTE Scan inner built whole) is stamped `Rests` instead. A breaker's node runs in its producer (`NodeOps::producer_top`): its readers
/// hand on rows it made. Tracer off: no stamp.
fn licence_stamp(px: &Px<'_, '_>, q: usize) -> gravity_program::LicenceStamp {
    use gravity_program::LicenceStamp as S;
    match px.held_kind(q) {
        S::None if stamps_on() && (px.recheck.contains(&q) || runs_licensed_node(px.gx, q)) => S::Rests,
        S::Plain | S::Accepted if px.recheck.contains(&q) => S::Recheck,
        held => held,
    }
}

/// C2 R3: whether graph pipeline `q` runs a node mapping took a recheck licence for (`licence_stamp`).
fn runs_licensed_node(gx: &Gx<'_, '_>, q: usize) -> bool {
    let (ids, ps) = (&gx.graph.licensed_nodes, gx.pipelines());
    if ids.is_empty() || gravity_decompose::effects::denying() || q >= ps.end() {
        return false;
    }
    let top = |id: i32| (0..ps.end()).any(|y| ps[y].node_ops.iter().any(|o| o.node_id == id && o.producer_top));
    let p = &ps[q];
    p.node_ops.iter().any(|o| ids.contains(&o.node_id) && (o.producer_top || !top(o.node_id))) || p.node_ops.is_empty() && ids.contains(&p.node_id)
}

/// Construct C2 (b), the run-ahead tracer's stamp of graph pipeline `q` (program `Pipeline::pg_gate` / `pg_regions` /
/// `quiet` / `gate_tags`, the gated node's kind and its parent's, a diagnosis for the hit lines): its source plan node when
/// PostgreSQL 18.6 may never run it (`regions`: decompose `pg_demand::regions`, whose nodes are `never_run`'s; else -1),
/// the gate regions holding that node, innermost first (D4191: a demand runs its own region; the four innermost, -1
/// after them), and, for such a pipeline, whether neither it nor what feeds it may raise or act (effects/visible
/// hashjoin.rs `stamp_side`, `side_by`'s PG-quiet reading, is Safe: a sort, a merge join or a MergeAppend's merge compares
/// through PostgreSQL's own btree support, `sort_quiet` / `merge_quiet`, and an index over initdb opclasses reads as a
/// system one; and so are the groups
/// they fold into, `groups_side`: a hashed aggregate's arguments, a hashed DISTINCT's keys), so that a run of it ahead of
/// PostgreSQL's demand cannot be seen. A source only Results' one-time quals gate (`results`: decompose
/// `pg_demand::result_gated`), in a pipeline that makes each of those checks before it pulls a row, stamps no gate. A
/// program the content key shares carries the lowering statement's stamp (its outer-first choice reads the planner's
/// costs, which the key leaves out): the tracer reads it as a diagnosis.
fn gate_stamp(px: &mut Px<'_, '_>, (regions, results): (&gravity_decompose::pg_demand::Regions<'_>, &[(i32, mcx::PgVec<'_, i32>)]), q: usize) -> (i32, [i32; 4], bool, Option<(&'static str, &'static str)>) {
    let gx = px.gx;
    let none = (-1, [-1; 4], false, None);
    if regions.of.is_empty() || q >= gx.pipelines().end() {
        return none;
    }
    let p = &gx.pipelines()[q];
    let source = p.node_ops.first().map_or(p.node_id, |n| n.node_id);
    // a Material's pull-form fill (nest.spool S1, D3498's pulled Result: `spool_pull`) reads nothing: its function leaves at
    // its open and its one reader makes each row at its pull, PostgreSQL's demand, so its open is no run ahead. So does a
    // feed whose source graph.rs `source_kind` names nest.loop (rerun/inner_arms.rs `Empty`, left at its open by `fused`):
    // an arm of a union a nest inner streams (S2b, D3789's CteScan arm) or a Material's reader pulls (S1u), which that
    // inner or reader makes again and pulls as PostgreSQL's Append pulls the arm
    if regions.of.binary_search_by_key(&source, |x| x.0).is_err() || crate::pipes::spool_pull(gx, q).is_ok() || gx.source_kind(q) == crate::face::kinds::KindId::NestLoop {
        return none;
    }
    // a source only Results' one-time quals gate (`results`: decompose `pg_demand::result_gated`), in a pipeline that makes
    // each of those checks before it pulls a row: its open is PostgreSQL's ExecResult's first call, no run ahead
    if results.binary_search_by_key(&source, |x| x.0).is_ok_and(|i| checks_results(px, q, &results[i].1)) {
        return none;
    }
    // the diagnosis (`Pipeline::gate_tags`): the gated node's kind and its parent's, for the tracer's hit lines
    let tag = |n: types_nodes::Node<'_>| gravity_decompose::ledger::Kind::tag_name(n.node_tag());
    let tags = gx.graph.stmt.and_then(|s| gravity_decompose::lifetime::node_of(s, source)).map(|(n, up)| (tag(n), up.map_or("-", tag)));
    let mut chain = [-1; 4];
    regions.chain(source).zip(chain.iter_mut()).for_each(|(r, c)| *c = r);
    let safe = crate::ops::hashjoin::build_class::Side::Safe;
    (source, chain, crate::effects::visible::hashjoin::stamp_side(gx, q) == safe && crate::effects::visible::hashjoin::groups_side(gx, q) == safe, tags)
}

/// C2 (b), `gate_stamp`: graph pipeline `q` makes the check of every one-time-qual Result in `results` before it pulls a
/// row: the Result's gate is a step of `q` itself (ops/result/gate.rs: its verdict is taken in the prologue and a false or
/// NULL one never opens the source), or `q`'s feeder makes it (`feeder_checks_gate`: limit_feeder's arms). A Result with
/// no gate step in the graph fails it.
fn checks_results(px: &mut Px<'_, '_>, q: usize, results: &[i32]) -> bool {
    let gx = px.gx;
    let ps = gx.pipelines();
    let gate_of = |r: i32| (0..ps.len()).find_map(|k| ps[k].ops.iter().enumerate().find_map(|(j, o)| (matches!(o, gravity_decompose::Op::Gate(_)) && crate::rerun::inner::op_node(&ps[k], j) == r).then_some((k, j))));
    results.iter().all(|&r| gate_of(r).is_some_and(|(k, j)| k == q || crate::ops::result::gate::feeder_checks_gate(px, q, k, j)))
}

/// M8 nest.rerun: the first rerun set no owner declared, one no member proto names (its set index read through the lifetime
/// home's `rerun_set_of`, past a kept member's flag). A set whose every member a set nested in it runs (rerun/sets.rs
/// `all_nested_away`, D1337) names none of its own: those sets name them, and their owners sit in its owner's inner.
fn undeclared_set<'a>(gx: &'a Gx<'_, '_>, protos: &[gravity_program::protos::Proto]) -> Option<&'a gravity_decompose::graph::Rerun> {
    let named = |i: usize| protos.iter().any(|p| p.kind == ProtoKind::Rerun && gravity_program::lifetime::rerun_set_of(p.arg) == i as u64);
    gx.graph.reruns.iter().enumerate().find(|&(i, _)| !named(i) && !crate::rerun::sets::all_nested_away(gx, i)).map(|(_, r)| r)
}

/// One verified function per pipeline over the context's expressions; the hosted owner goes to `Descs::owner`.
pub fn emit<'g>(px: &mut Px<'g, '_>, mut mechs: Vec<Mechs<'g>>) -> Lowered {
    let (arena, node) = (px.arena, px.gx.root_node());
    // D2561: a top drain (form G) reads the record array its group's emit gathered and sorted in an earlier call, in that
    // call's scratch, free once it ends: the drain's fresh call rewinds the chunks to the pins, and a pipeline nested in it (a
    // lazy build the drain waits on, an InitPlan the drain's pipeline reads first, a rerun set, a LAZY hashed SubPlan's set,
    // a pulled CTE) pins from there and takes that chunk (the drain read a clobbered array: SIGSEGV). D4476: the drain's own
    // fresh call takes it too (its end's table, opened before the drain's first read: count(*) beside a count(DISTINCT)
    // over a sorted group's rows). So the emit always keeps the array past its call (pinned); where it cannot, a program
    // that runs a nested pipeline refuses (D2561).
    let nested = runs_nested(px);
    let tops: PgVec<'_, (usize, usize)> = mechs.iter().filter(|m| m.source.rereads_top()).filter_map(|m| top_drain(px.gx, m.k).map(|t| (m.k, t))).collect_in(px.mcx());
    for (q, t) in tops {
        if !mechs.iter_mut().find(|m| m.k == t).is_some_and(|m| m.source.keep_past_call(px, t)) && nested {
            crate::refuse!(px.gx.pipelines()[q].node_id, Reason::Shape { why: crate::admit::refusal::ShapeWhy(DRAIN_ARRAY_NOT_KEPT) });
        }
    }
    let mut pb = ProgramBuilder::new_owned(statement_owner());
    if gravity_lower::jit_tags_on() {
        pb.jit_tags(); // the JIT map's tags (frame/jit.rs), off the IR
    }
    let mut core = env::declare(&mut pb);
    core.strategies = px.strategies;
    let codes = px.codes.clone(); // alloc: Program
    let mcx = px.mcx();
    let hosted = {
        let mut refs: Vec<&mut Ex<'_>> = px.exs_mut().iter_mut().collect(); // alloc: Program
        Hosted::new_over(&mut refs, &codes, mcx)
    };
    let hir = hosted.declare(&mut pb);
    let exs = px.take_exs();
    px.place_late_sites();
    // D2147: a lazy build's probe pipeline stops inside a claim and resumes in a new call (D824): its end's state in frame words
    for m in mechs.iter_mut() {
        if m.end.state_bytes() > 0 && px.lazy.iter().any(|&(_, q)| q == m.k) {
            let k = m.k;
            m.state_word = (0..m.end.state_bytes().div_ceil(8) as u16).map(|i| stop::resume_word(px, stop::ResumeWord::EndState { k, i })).min();
        }
    }
    // D2772 POST (modifytable): an unread data-modifying CTE's pipelines run last (ops/modifytable/post.rs `order`)
    crate::ops::modifytable::post::order(px.gx, &mut mechs, |m| m.k);
    instr::pipe_order(&mechs.iter().map(|m| m.k).collect_in(px.mcx()));
    let words = px.add_words_site();
    mechs.iter_mut().for_each(|m| m.words = words);
    let gx = px.gx;
    let plan_mcx = px.mcx();
    let n = mechs.iter().map(|m| m.k + 1).max().unwrap_or(0);
    let mut sites = vec![None; n]; // alloc: Program
    mechs.iter().for_each(|m| sites[m.k] = m.site.map(|s| s as u16));
    let shape: PgVec<'_, usize> = (0..n).map(|k| px.after_of(k)).collect_in(plan_mcx);
    // D4197: a user key's host calls take the run's classes, behind the word the program declares once a site read it
    let keys = px.next_proto_word();
    let rels: std::rc::Rc<[u32]> = px.sites.iter().map(|s| s.relation).collect(); // alloc: Program
    let arms = arm_runs(px.gx, &px.protos, &mechs, plan_mcx);
    // D3168: a staged function lowers into a module of its own; the shape's memo (by the admission's key) may skip it
    let (strategies, memo) = (core.strategies, px.shape.filter(|_| crate::ops::hashjoin::strategies::probe_stage::memo()));
    let side = || {
        let mut spb = ProgramBuilder::new_owned(statement_owner());
        let mut score = env::declare(&mut spb);
        score.strategies = strategies;
        let shir = hosted.declare(&mut spb);
        (spb, score, shir)
    };
    let tag = |k: usize| memo.map(|key| gravity_program::code::stage_memo::tag(key, node, k));
    let ((fs, mut bounded, staged), read) = env::with_user_keys(keys, || env::with_pipe_sites(sites.into(), rels, || functions(&mut pb, core, &hir, &exs, &mut mechs, &|k| gx.pipelines()[shape[k]].node_id, &|k| !arms.get(k).copied().unwrap_or(true), (&side, &tag), plan_mcx)));
    if read {
        if px.proto_words(1) != keys {
            px.bug("the keys.user word moved under the program's functions");
        }
        px.declare_proto(gravity_program::protos::Proto::new(ProtoKind::UserKeys, 0).words(keys, 1));
    }
    let mut funcs = ArenaVec::with_capacity_in(fs.len(), arena);
    let mut pipelines = ArenaVec::with_capacity_in(mechs.len(), arena);
    let drained = crate::rerun::epq::marks::drained(gx); // D3278 (DRAINED)
    let copies = crate::rerun::epq::copy_row::copy_entries(&mechs);
    // construct C2 (b): the plan nodes PostgreSQL may never run (`gate_stamp`); none while the tracer is off
    // (`GATE_STAMPS`) and none in the plan-order program (every licence denied, `effects::deny_all`: the tracer
    // judges no run of it)
    let regions = gx.graph.stmt.filter(|_| GATE_STAMPS.load(std::sync::atomic::Ordering::Relaxed) && !gravity_decompose::effects::denying()).map(|s| gravity_decompose::pg_demand::regions(s, px.mcx())).unwrap_or_else(|| gravity_decompose::pg_demand::Regions::new_in(px.mcx()));
    let results = gx.graph.stmt.filter(|_| !regions.of.is_empty()).map(|s| gravity_decompose::pg_demand::result_gated(s, px.mcx())).unwrap_or_else(|| mcx::PgVec::new_in(px.mcx()));
    for (f, m) in fs.iter().zip(mechs.iter()) {
        funcs.push(*f);
        // D2877; EXPLAIN ANALYZE: a pipeline no claim reaches (an empty relation) still starts its nodes once, as
        // PostgreSQL's ExecProcNode starts a scan that returns nothing (its open counts their `Loops`)
        let first_call = m.steps.iter().any(|s| s.first_call()) || instr::on();
        let q = px.after_of(m.k);
        let recheck_hold = px.recheck_hold.contains(&q); // D2033 step 2
        let recheck_hold_epq = px.recheck_hold_epq.contains(&q); // D2033 step 3
        let held_sort = px.held_sort.contains(&q); // D1236's held inner
        let held_at = px.held_at.contains(&q); // D2316's held incremental sort
        let (pg_gate, pg_regions, quiet, gate_tags) = gate_stamp(px, (&regions, &results), q);
        let holds = m.end.holds().or(m.desc.holds); // C10: an end settled as its code was made
        pipelines.push(Pipeline { recheck_skip: crate::rerun::epq::copy_row::skipped(gx, &copies, q) || crate::rerun::epq::marks::wco_skipped(gx, q) || drained.contains(&q), first_call, recheck_hold, recheck_hold_epq, held_sort, held_at, pg_gate, pg_regions, quiet, licence: licence_stamp(px, q), gate_tags, holds, ..m.desc }); // D732-rest; W6-D11
    }
    // C10: every breaker's spill answer agrees with its descriptor (`spill::agrees`): an end settled without `spill::plan`
    // is named here, in debug builds
    if cfg!(debug_assertions) {
        for (i, p) in pipelines.iter().enumerate() {
            if let (Some(b), Some(h)) = (p.breaker(), p.holds()) {
                assert!(gravity_program::spill::agrees(&b, h), "gravity: pipeline {i}'s breaker {b:?} and its spill answer {h:?} disagree (C10)");
            }
        }
    }
    if mechs.iter().enumerate().any(|(i, m)| m.k != i) {
        let mut at = mcx::pg_vec![in plan_mcx; u16::MAX; n];
        mechs.iter().enumerate().for_each(|(i, m)| at[m.k] = i as u16);
        to_program(arena, &mut px.protos, &mut pipelines, &at, node);
    }
    // D824 (V4R-298): a lazy build names its probe's pipeline, both in program order
    for &(b, q) in px.lazy.iter() {
        let at = |k: usize| mechs.iter().position(|m| m.k == k);
        let (Some(bi), Some(qi)) = (at(b), at(q)) else { crate::refuse!(node, Reason::Bug { payload: "a lazy build or its probe with no program pipeline" }) };
        if bi >= qi || qi > i16::MAX as usize {
            crate::refuse!(node, Reason::Bug { payload: "a lazy build after its probe's pipeline" });
        }
        if pipelines[qi].is_eligible() { // a guard: Mechs::shaped pins a lazy build's probe pipeline to the leader (D1005)
            crate::refuse!(node, Reason::Shape { why: crate::admit::refusal::ShapeWhy("a volatile build side on PG's outer-first path that is not deferred") });
        }
        // D2143: unless its other end is deferred for `q` too (both run in its wait, the probe first: exec drain/proto.rs `check`)
        let mate = |o: u16| px.lazy.iter().any(|&(x, y)| y == q && at(x) == Some(o as usize));
        if px.protos.iter().any(|p| p.kind == ProtoKind::Follows && [p.pipeline, p.peer].contains(&(bi as u16)) && !(mate(p.pipeline) && mate(p.peer))) {
            crate::refuse!(node, Reason::Shape { why: crate::admit::refusal::ShapeWhy("a lazy build as join.follows' drain or its probe (D2143: it runs out of order)") });
        }
        pipelines[bi].lazy_for = qi as i16;
    }
    // v5 errorhold: a pipeline departing under a recheck licence (`Px::licensed`), and the root pipeline when mapping took
    // one (`PipelineGraph::licensed`)
    let root = px.gx.graph.recheck.then_some(px.gx.graph.root as usize);
    for &k in px.recheck.iter().chain(root.iter()) {
        if let Some(i) = mechs.iter().position(|m| m.k == k) {
            pipelines[i].recheck = true;
        }
    }
    // D3788: a pipeline a declared lazy group waits past only because it raises and acts nowhere (build_class
    // `raise_only`, `Px::lazy_raises`) runs ahead of PostgreSQL's order unchecked: tagged as `Px::ahead_unchecked` tags,
    // where EXPLAIN names its node
    let raised: PgVec<'_, usize> = px.lazy_raises.borrow().iter().filter(|&&(b, q, _)| px.lazy.contains(&(b, q))).map(|&(_, _, x)| x).collect_in(plan_mcx);
    for x in raised {
        if let (false, Some(t)) = (px.ahead.iter().any(|&(y, _)| y == x), crate::frame::px::ahead_tag(px.gx, x)) {
            px.ahead.push((x, t)); // alloc: Program
        }
    }
    // `DECISIONS.md` 4 (D) b: a pipeline running code ahead under a tagged licence (`Px::ahead_unchecked`)
    // an embedded nest inner (or a pipeline made inside one) runs in its owner's program pipeline: the tag goes there,
    // unless the owner carries its own
    for &(k, t) in px.ahead.iter() {
        let ps = gx.pipelines();
        let mut q = k;
        let host = loop {
            if let Some(i) = mechs.iter().position(|m| m.k == q) {
                break Some(i);
            }
            match q.checked_sub(ps.len()).and_then(|i| ps.inners().get(i).map(|x| x.0 .0)) {
                Some(o) if o != q => q = o,
                _ => break None,
            }
        };
        match host {
            Some(i) if q == k || pipelines[i].ahead.is_none() => pipelines[i].ahead = Some(t),
            Some(_) => {}
            None => crate::refuse!(node, Reason::Bug { payload: "a run-ahead pipeline with no program pipeline" }),
        }
    }
    // D1922: a pipeline served only to a reader that takes every row (`Px::whole_read`)
    for &k in px.whole_read.iter() {
        match mechs.iter().position(|m| m.k == k) {
            Some(i) => pipelines[i].whole_read = true,
            None => crate::refuse!(node, Reason::Bug { payload: "a whole-read pipeline with no program pipeline" }),
        }
    }
    // D3568 HASHSET: a hashed SubPlan's set build a lock's recheck steps past (rerun/epq/marks.rs `hashed_skipped`);
    // D3592: a producer only the part below a COPY row's entry into a nest inner reads (copy_row.rs `inner_skipped`)
    for (i, m) in mechs.iter().enumerate() {
        let q = px.after_of(m.k);
        pipelines[i].recheck_skip |= crate::rerun::epq::marks::hashed_skipped(gx, q) || crate::rerun::epq::copy_row::inner_skipped(gx, q);
    }
    // D3914: a pass (`Px::add_pass`) names the program pipeline it follows, whose ops it runs again over that pipeline's
    // consumed slots: the driver lets it open over a rerun set not yet run where it lets that pipeline (a set's owner)
    for (i, m) in mechs.iter().enumerate() {
        let q = px.after_of(m.k);
        if q != m.k {
            pipelines[i].pass_of = mechs.iter().position(|x| x.k == q).map_or(u16::MAX, |at| at as u16);
        }
    }
    let mut deferred: PgVec<'_, bool> = pipelines.iter().map(|p| p.lazy_for >= 0).collect_in(plan_mcx);
    // M8 B1: an Init's members run on demand (m8_init_run, program::v4::members_in, the runtime's closure): deferred too;
    // B2 (V4R-369): so do a LAZY hashed SubPlan's set's (m8_subhash_run, at its first probe), and no reader waits on its slot
    // V4R-527 / D2028: an Init's members never rerun by a recheck, a write's as a lock's (EvalPlanQualStart copies an evaluated
    // InitPlan's value; the recheck reads the execution's Init cell). D2112 phase B: one whose InitPlan the recheck plan's
    // extParam reads (decompose `recheck_inits`) is forced at the recheck when still pending (`INIT_RECHECK`, exec epq.rs).
    // D2673: one attached below the write or lock (decompose `epq_fresh`: inside a SubPlan's plan) runs again where a recheck
    // first reads it, as EvalPlanQualStart arms it afresh, and the statement's run keeps its own value (`INIT_EPQ_FRESH`)
    let rechecks = px.write_plan().is_some_and(|w| w.lock != gravity_program::LockKind::None || matches!(w.cmd, gravity_program::Cmd::Update | gravity_program::Cmd::Delete | gravity_program::Cmd::Merge));
    let bits = |first: i32| {
        use gravity_kernels::ops::shared::object::entries::{INIT_EPQ_FRESH, INIT_RECHECK};
        let g = px.gx.graph;
        g.init_plans.iter().filter(|i| i.param == first).fold(0, |b, i| b | (if g.recheck_inits.contains(&i.plan_id) { INIT_RECHECK } else { 0 }) | (if rechecks && g.epq_fresh.contains(&i.plan_id) { INIT_EPQ_FRESH } else { 0 }))
    };
    let flagged: PgVec<'_, (usize, u64)> = px.protos.iter().enumerate().filter(|(_, p)| p.kind == ProtoKind::Init).map(|(i, p)| (i, bits(gravity_kernels::ops::shared::object::entries::init_first(p.arg)))).collect_in(plan_mcx);
    flagged.into_iter().for_each(|(i, b)| px.protos[i].arg |= b);
    let on_demand = |p: &gravity_program::protos::Proto| p.kind == ProtoKind::Init || (p.kind == ProtoKind::SubHash && p.arg & gravity_kernels::ops::shared::object::entries::SUBHASH_LAZY != 0);
    for (p, set) in px.protos.iter().filter(|p| on_demand(p)).map(|p| (p.pipeline as usize, p.kind == ProtoKind::SubHash)).collect_in(plan_mcx) {
        if let (true, Some(s)) = (set, pipelines[p].produces()) {
            // only the set's own life: slot numbers repeat here (tsrf-015: a Sort's slot 0 read before the set's Distinct slot 0)
            let readers: PgVec<'_, usize> = (p + 1..pipelines.len()).filter(|&k| gravity_program::lives::reads(&pipelines, k, LifeId(p as u16))).collect_in(plan_mcx);
            // its build runs nested inside them: the set is held through its last reader (the grant's divisor, the frees)
            pipelines[p].live_through = readers.last().map_or(-1, |&k| k as i16);
            for k in readers {
                let d = pipelines[k];
                let keep = d.consumes.iter().enumerate().filter(|&(_, &x)| x != s);
                pipelines[k] = keep.fold(Pipeline { consumes: &[], consumes_from: &[], ..d }, |n, (j, &slot)| match d.consumes_from.get(j) { Some(&l) => n.consuming_from_in(arena, slot, l), None => n.consuming_in(arena, slot) });
            }
        }
        let m = gravity_program::protos::members_in(&pipelines, &px.protos, p);
        if !m.shared.is_empty() {
            crate::refuse!(node, Reason::Shape { why: crate::admit::refusal::ShapeWhy(if set { "a LAZY hashed SubPlan's set whose producer a pipeline outside it also reads" } else { "an InitPlan whose producer a pipeline outside it also reads" }) });
        }
        m.members.iter().for_each(|&x| {
            deferred[x as usize] = true;
            // V4R-527 / D2028 skip an InitPlan's members only; a LAZY set's keep B2's recheck (PG re-arms a hashed SubPlan in EPQ)
            pipelines[x as usize].recheck_skip |= rechecks && !set;
        });
    }
    // D3271 rest: an InitPlan a lock's recheck re-arms keeps the outside slots its members read live through that recheck
    if px.write_plan().is_some_and(|w| w.cmd == gravity_program::Cmd::Select && w.lock != gravity_program::LockKind::None) {
        crate::ops::lockrows::lock_init::hold_epq_reads(px, &mechs, &mut pipelines, node);
    }
    // M8 nest.rerun: a set's members run only through its owner's m8_rerun (nest/rerun.rs); a set no owner declared refuses
    if let Some(r) = undeclared_set(gx, &px.protos) {
        crate::refuse!(r.node_id, Reason::Shape { why: crate::admit::refusal::ShapeWhy("a rerun set no owner declares (D1262: served once its owner declares it, the NEST inner hook or B2's nested SubPlan, and nest.rerun's runtime R1b lands)") });
    }
    px.protos.iter().filter(|p| p.kind == ProtoKind::Rerun).for_each(|p| deferred[p.pipeline as usize] = true);
    // D3037 (`CTE_PRODUCERS_AHEAD`): a CTE whose producers run ahead reads their slots at its readers' pulls: each is held
    // through the CTE's last reader. A slot number another pipeline produces again before that reader (a reader's own
    // Sort or hashed group reuses the number: `SELECT * FROM w ORDER BY c DESC LIMIT 10`) is renamed to a free number first
    // (`frame::rename_slot`) when the CTE alone reads that production; else (arms, a table another pipeline also touches,
    // no free number) the CTE's whole plan is pulled, as without the bit.
    let ahead_bit = gravity_program::protos::CTE_PRODUCERS_AHEAD;
    for i in (0..px.protos.len()).filter(|&i| px.protos[i].kind == ProtoKind::Cte && px.protos[i].arg & ahead_bit != 0).collect_in(plan_mcx) {
        let p = px.protos[i].pipeline as usize;
        let Some(out) = pipelines[p].produces() else { crate::refuse!(node, Reason::Bug { payload: "a CTE whose producers run ahead produces no store slot" }) };
        let next = (p + 1..pipelines.len()).find(|&j| pipelines[j].produces() == Some(out)).unwrap_or(pipelines.len());
        let Some(last) = (p + 1..next).filter(|&j| pipelines[j].consumes.contains(&out)).max() else { crate::refuse!(node, Reason::Bug { payload: "a CTE whose producers run ahead has no reader" }) };
        // D3095: an InitPlan reader (a deferred member) pulls wherever its param is read, which may be a pipeline past the
        // CTE's last reader: the producers are held while the store's production lives
        let last = match (p + 1..next).any(|j| deferred[j] && pipelines[j].consumes.contains(&out)) {
            true => next - 1,
            false => last,
        };
        let mut held: PgVec<'_, (usize, u8)> = PgVec::new_in(plan_mcx);
        for &slot in pipelines[p].consumes {
            let Some(q) = (0..p).rev().find(|&q| pipelines[q].produces() == Some(slot)) else { continue };
            held.extend_in(gravity_program::protos::arms_of(&pipelines, &px.protos, q, slot).into_iter().map(|a| (a, slot)));
        }
        let reused = |pipelines: &[Pipeline], held: &[(usize, u8)], a: usize, slot: u8| (a + 1..=last).any(|j| pipelines[j].produces() == Some(slot) && !held.iter().any(|h| h.0 == j));
        let mut stuck = false;
        for k in 0..held.len() {
            let (a, slot) = held[k];
            if held.iter().any(|&(b, t)| t == slot && b > a) || !reused(&pipelines, &held, a, slot) {
                continue;
            }
            // only a lone production the CTE alone reads: arms, or a table another pipeline before the CTE also touches
            // (a hashed INTERSECT's probe arm reads and writes its build's table), or a production another object names
            // (a SetOp pairing two tables, any peer), keep the number and the pull
            let plain = px.protos.iter().filter(|x| x.pipeline as usize == a || x.pipeline as usize == p || x.peer as usize == a).all(|x| matches!(x.kind, ProtoKind::Verdicts | ProtoKind::Facts | ProtoKind::Observe | ProtoKind::Width | ProtoKind::Spool | ProtoKind::Cte));
            let alone = plain && held.iter().filter(|h| h.1 == slot).count() == 1 && (a + 1..p).all(|j| pipelines[j].produces() != Some(slot) && !pipelines[j].consumes.contains(&slot));
            let used = |t: u8| pipelines.iter().any(|q| q.produces() == Some(t) || q.consumes.contains(&t));
            let Some(to) = (0..=u8::MAX).find(|&t| !used(t)).filter(|_| alone) else {
                stuck = true;
                break;
            };
            frame::rename_slot(arena, &mut pipelines, &mut px.protos, a, slot, to);
            held[k].1 = to;
        }
        if stuck || held.iter().any(|&(a, slot)| reused(&pipelines, &held, a, slot)) {
            // D4203: a CTE whose producers must run before any row (ops/ctescan/lower.rs `licensed_first`) is never pulled
            // after an emitting pipeline in program order (`Late::Ordered`); on a road the lowering cannot time it is
            // pulled as before
            if mechs.get(p).is_some_and(|m| crate::ops::ctescan::lower::plan_late(px, m.k) == crate::frame::px::Late::Ordered) {
                crate::refuse!(node, Reason::Shape { why: crate::admit::refusal::ShapeWhy(crate::ops::ctescan::lower::LICENSED_LATE) });
            }
            px.protos[i].arg &= !ahead_bit;
            continue;
        }
        for (a, _) in held {
            pipelines[a].live_through = pipelines[a].live_through.max(last as i16);
        }
    }
    // nest.cte (pull form, CT-2): a CTE's pulled set runs only through its readers' m8_cte_pull (exec v4/m8_cte.rs); the
    // exact request's window fill (PLAN-v5 §2.5; W4) only through its object's spool (m10_win_ask, exec ops/window/proto.rs)
    for (p, cte) in px.protos.iter().filter(|p| gravity_program::protos::pulled(p)).map(|p| (p.pipeline as usize, p.kind == ProtoKind::Cte)).collect_in(plan_mcx) {
        let m = gravity_program::protos::members_in(&pipelines, &px.protos, p);
        if !(cte && Px::streamed(&pipelines, &px.protos, p, &m, node)) && !m.shared.is_empty() { // C7STREAM F1: nest/spool_stream.rs
            crate::refuse!(node, Reason::Shape { why: crate::admit::refusal::ShapeWhy(if cte { "a CTE whose producer a pipeline outside it also reads" } else { crate::ops::window::lower::PULLED_SHARED }) });
        }
        // a pulled set runs leader-serial (RT-2f: it stops and resumes inside a reader's call): every member is planned so.
        // D3095: but a CTE's producers, which the first pull runs whole before the pulled pipeline's first window (driver
        // entry.rs `pull_nested`), keep their eligibility: exec runs each as a section at the pull when its claims are big
        // enough (gravity_parallel `nested_section`, D3137's rule for every nested producer), else on the leader as before
        for &x in m.members.iter() {
            deferred[x as usize] = true;
            let d = &mut pipelines[x as usize];
            let producer = cte && x as usize != p && d.lazy_for < 0;
            if d.is_eligible() && !producer {
                d.parallel = gravity_program::Parallel::Ineligible { site: d.driving_site, reason: gravity_program::Reason::Host };
            }
        }
    }
    // a pull-form merge's Sort arm hands its run's slot to the merge reader, which reads the run (ops/append/merge_pull_runs.rs)
    crate::ops::append::merge_pull_runs::rehome(gx, arena, &|k| mechs.iter().position(|m| m.k == k), &mut pipelines, node);
    frame::renumber_deferred(arena, &deferred, &mut pipelines, &mut px.protos, node, plan_mcx);
    crate::frame::tail::renumber_seeded(arena, &mut pipelines, &mut px.protos, node); // D1188: a seeded drain's arms slot its probe reads
    // D1036: a run nested inside an outer row (an Init's members, a lazy build, a pulled set) takes a hosted lane of its own; the program's
    // on-demand groups bound how deep they nest, refused by name past the lanes (never a runtime error where PG answers)
    if gravity_program::protos::on_demand_groups(&pipelines, &px.protos) > gravity_driver::NESTED_LANES as usize {
        crate::refuse!(node, Reason::Shape { why: crate::admit::refusal::ShapeWhy("more on-demand runs (InitPlans, lazy builds, rerun sets, pulled CTE plans) than nested hosted lanes (D1036)") });
    }
    // D2772 POST (modifytable): an unread data-modifying CTE's pipelines run after the statement's own
    let post = crate::ops::modifytable::post::mark(gx, &|k| mechs.iter().position(|m| m.k == k), &mut pipelines, node);
    let nodes: PgVec<'_, i32> = mechs.iter().map(|m| gx.pipelines()[shape[m.k]].node_id).collect_in(plan_mcx);
    // D1687 / D2605 / D3449: the write plans beside `write`, and where each ModifyTable binds or ends (ops/modifytable/rowop.rs
    // `writes`)
    let (writes, write_end) = crate::ops::modifytable::rowop::writes(px, &|k| mechs.iter().position(|m| m.k == k), post, node);
    drop(mechs);
    // D280: a serial program (one pipeline, not eligible) runs with no producer: a protocol that acts after one could not fill its words
    if pipelines.len() == 1 && !pipelines[0].is_eligible() && px.protos.iter().any(|p| matches!(p.kind, ProtoKind::TwoPass | ProtoKind::RtFilter | ProtoKind::BuildSide | ProtoKind::Observe | ProtoKind::TopGroups)) {
        crate::refuse!(node, Reason::Bug { payload: "a serial program declares a protocol that acts after a producer" });
    }
    let tagged = pb.take_tagged();
    let jit = pb.take_jit_tags();
    let mut ir = pb.finish();
    // D3168 S0: a staged function is a program of its own, pruned and verified alone and compiled the first time its plain
    // function hands a call to it (gravity_lower `assemble`); a skipped one (`Want`) is the memo's stand-in. The list is the
    // running statement's, as the lowering is
    let mut sides = gravity_fn::arena::ChargedVec::new(Some(memledger::charge::statement_owner()), memledger::Kind::Region);
    for (plain, s) in staged.into_iter() {
        let e = match s {
            frame::StagedFn::Lowered(mut side, bs, t) => {
                if side.funcs.len() != 1 || side.funcs[0].blocks.iter().any(|b| b.insts.iter().any(|i| matches!(i.op, gravity_ir::Op::Call { .. }))) {
                    crate::refuse!(node, Reason::Bug { payload: "a staged function that calls another function" });
                }
                drop(gravity_ir::prune::unreachable(&mut side.funcs[0]));
                drop(verify(&side, None, &[Func(0)], core::slice::from_ref(&bs), node, plan_mcx));
                (plain, Some(side), t)
            }
            frame::StagedFn::Want(t) => (plain, None, t),
        };
        if sides.push(e).is_err() {
            std::alloc::handle_alloc_error(core::alloc::Layout::new::<(Func, Option<gravity_ir::Program>, u64)>());
        }
    }
    fs.iter().for_each(|&f| drop(gravity_ir::prune::unreachable(&mut ir.funcs[f.index()]))); // D528: dead blocks emptied
    let carried = instr::carry(&mut ir, plan_mcx); // EXPLAIN ANALYZE's counters in registers (an instrumented program's only)
    // D3585: a union's twin arms share one function (frame/twin.rs), verified with the stubs that call it; an instrumented
    // program keeps a function per arm
    let mut twins = match carried.is_none() && !instr::on() {
        true => {
            let sites: PgVec<'_, i16> = pipelines.iter().map(|p| p.driving_site).collect_in(plan_mcx);
            let runs: PgVec<'_, (usize, usize)> = px.protos.iter().filter(|p| p.kind == ProtoKind::Arms).map(|p| (p.pipeline as usize, (p.pipeline as usize).saturating_add(p.arg as usize))).collect_in(plan_mcx);
            crate::frame::twin::fold(&mut ir, &fs, &mut bounded, &sites, &runs, node, plan_mcx)
        }
        false => PgVec::new_in(plan_mcx),
    };
    // D3886: the code key the verify takes rides with the IR to the backend (`Lowered::key`); nothing below changes `ir`
    let key = match twins.is_empty() {
        true => verify(&ir, carried, &fs, &bounded, node, plan_mcx),
        false => {
            let checked: PgVec<'_, Func> = fs.iter().copied().chain(twins.iter().map(|t| t.0)).collect_in(plan_mcx);
            bounded.extend_in(twins.drain(..).map(|t| t.1));
            verify(&ir, carried, &checked, &bounded, node, plan_mcx)
        }
    };
    drop((twins, bounded)); // they borrow `exs`, which the owner takes below
    // D1035: a pipeline whose code can run an InitPlan or a rerun set runs on the leader, whose frame holds the cells; but
    // the reader of an eager Init (step 2, V4R-444: on_demand.rs)
    crate::frame::on_demand::pin(&ir, &fs, &mut pipelines, &mut px.protos);
    let owner = gravity_lower::no_owner();
    let no_cache = !px.data.is_empty();
    let (data, owned) = (core::mem::take(&mut px.data), core::mem::take(&mut px.owned));
    let keep = (!data.is_empty() || !owned.is_empty()).then(|| Box::new((data, owned)) as Box<dyn core::any::Any>);
    // memsig E-prog: the lists root goes with the programs built in it (`hir::finish_keeping`)
    owner.set(hosted.finish_keeping(exs, keep, px.hosted_root.take(), px.lists_root.take()));
    let out = px.out.take().unwrap_or(Out { cols: ArenaVec::new_in(arena), rows: OUT_ROWS, region: 0 });
    let sites = core::mem::replace(&mut px.sites, ArenaVec::new_in(arena));
    let layout = Layout { nsites: sites.len() as u16, out_natts: out.cols.len() as u16, out_rows: out.rows, scratch_bytes: px.scratch, exec_bytes: 0 };
    let write = px.take_write().unwrap_or(WritePlan::SELECT);
    // D4154 INITMULTI: the M4 pair's B of several marks that another form made (its marks numbered from entry 1, over A's)
    crate::ops::lockrows::plan::unclaimed(px);
    // V4R-277: prepare runs in declaration order, and agg.arena clamps the widths `width` declared after the ends
    px.protos.sort_by_key(|p| p.kind == ProtoKind::AggArena);
    // the pipelines the runtime runs again, each with its one runner: driver Entry::rerun_nested resets exactly these
    let lifetimes = match crate::lifetime::program(&pipelines, &px.protos, &px.gx.graph.refills) {
        Ok(l) => l,
        Err(why) => crate::refuse!(node, Reason::Bug { payload: why }),
    };
    let mut v4 = V4Descs::new(core::mem::take(&mut px.protos));
    v4.lifetimes = lifetimes;
    v4.rewind_reruns = crate::effects::visible::rewind_visible(px.gx).is_some(); // D2651: the entry routes a SCROLL portal
    v4.soft_rewind = v4.rewind_reruns && crate::effects::visible::soft_rewind(px.gx); // D2701: or keeps it served
    v4.pull_exact = crate::effects::visible::pull_exact(&v4.protos); // D439: a pull runs no row ahead of its count
    // D3769: the pinned sites, each named by its source pipeline's plan node (exec admit `registry` pairs them first)
    v4.site_pins = px.site_pins.iter().map(|&(s, q)| (s as u16, px.gx.pipelines()[q].node_id)).collect(); // alloc: Program
    gravity_lower::jit_note(key, &funcs, &nodes, jit);
    let l = Lowered { ir, funcs, key, tagged, staged: sides, descs: Descs { layout, sites, out: out.cols, write, pipelines, out_region: out.region, owner, no_cache, no_cache_why: if no_cache { "data" } else { "" }, instr: Vec::new(), v4, used: 0, locks: px.locks.clone(), writes, write_end } }; // alloc: Program (W5 Row A's locks)
    assembles(&l, &nodes, node);
    l
}

/// D296: the descriptors through `gravity_lower::assemble` itself (stub codes) inside the catch. Its rules (with_pipelines'
/// sink, admits, kernel words and validate_dag) otherwise first run in exec after the backend, where a panic is XX000, not a
/// refusal the fallback takes. A broken rule is `Reason::Bug` at its pipeline's plan node (`nodes`, program order), logged.
fn assembles(l: &Lowered, nodes: &[i32], root: i32) {
    gravity_abi::host! {
        #[unwind]
        fn never(_: *mut gravity_abi::Ctx) -> i64 {
            0
        }
    }
    let codes = l.descs.pipelines.iter().map(|_| gravity_program::Code::hand(never)).collect(); // alloc: Program
    let Err(p) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(gravity_lower::assemble(l, codes)))) else { return }; // unwind-ok: c-callback
    if p.is::<types_error::PgError>() || p.is::<Box<types_error::PgError>>() || p.is::<types_error::PanicExitThread>() {
        std::panic::resume_unwind(p);
    }
    let msg = p.downcast_ref::<String>().map(String::as_str).or_else(|| p.downcast_ref::<&str>().copied()).unwrap_or("");
    let at = msg.strip_prefix("pipeline ").and_then(|t| t.split(|c: char| !c.is_ascii_digit()).next()?.parse::<usize>().ok());
    let node = at.and_then(|i| nodes.get(i).copied()).unwrap_or(root);
    eprintln!("gravity lowerv4: the program's descriptors break assemble's rules (Reason::Bug) node={node}: {msg}");
    crate::admit::refusal::BUGS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    crate::admit::refusal::raise(node, Reason::Bug { payload: "the program's pipelines break Program::with_pipelines's rules (logged)" });
}

/// Program order is `resolve`'s (each pass right after its `after`): every make-time pipeline reference, a graph index or a
/// pass's virtual id (`Px::add_pass`), becomes its program index: protocols' pipeline / peer, the two-pass admit's pass 1,
/// the lives slots are read from, join.buildside's skip mask (D276: `arg` bit 8 + p = pipeline p, p < 40).
fn to_program(arena: ArenaRef, protos: &mut [gravity_program::protos::Proto], pipelines: &mut [Pipeline], at: &[u16], node: i32) {
    let map = |x: u16| at.get(x as usize).copied().filter(|&i| i != u16::MAX).unwrap_or(x);
    super::tail::seeded_arms(protos, map); // B4c-2: a seeded drain's run ends at its drain
    for p in protos.iter_mut() {
        p.pipeline = map(p.pipeline);
        if peer_is_pipeline(p.kind) {
            p.peer = map(p.peer);
        }
        if p.kind == gravity_program::protos::ProtoKind::BuildSide {
            let field = ((1u64 << 40) - 1) << 8;
            let (mut m, mut out) = ((p.arg & field) >> 8, 0u64);
            while m != 0 {
                let q = map(m.trailing_zeros() as u16) as u32;
                m &= m - 1;
                if q >= 40 {
                    crate::refuse!(node, Reason::Width { needed: q + 1, have: 40 });
                }
                out |= 1 << q;
            }
            p.arg = (p.arg & !field) | (out << 8);
        }
    }
    // M7-SPILL B4b-1: an Arms run's slot is one life, the last arm's (the driver finishes it there): a reader naming an
    // earlier arm (a spill pair's probe `q`, whose graph reader names it) reads the run's last arm.
    // D3218: an arm ends in the last arm's breaker kind; a pipeline inside the span producing the same slot number under
    // another kind is no arm (B4d: a build between a seeded probe and its drain, under a number decompose reused, which
    // tail.rs `renumber_seeded` renames with its readers), so its readers keep its life
    let mut runs: Vec<(u16, u16, u8)> = protos.iter().filter(|p| p.kind == gravity_program::protos::ProtoKind::Arms && p.arg >= 2).map(|p| (p.pipeline, p.pipeline + p.arg as u16 - 1, p.slot)).collect(); // alloc: Program
    super::tail::union_runs(&mut runs); // B4c-2: same-first runs on one slot are one production
    let kinds: Vec<Option<gravity_abi::breaker::BreakerKind>> = pipelines.iter().map(|p| p.breaker().map(|b| b.kind)).collect(); // alloc: Program
    let arm = |l: u16, z: u16| kinds.get(l as usize) == kinds.get(z as usize);
    let life = |slot: u8, l: u16| runs.iter().find(|&&(a, z, s)| s == slot && (a..z).contains(&l) && arm(l, z)).map_or(l, |r| r.1);
    for d in pipelines.iter_mut() {
        if let Some(a) = d.admit.as_mut() {
            a.pass1 = map(a.pass1);
        }
        if d.consumes.iter().zip(d.consumes_from).any(|(&s, l)| life(s, map(l.0)) != l.0) {
            let mut n = Pipeline { consumes: &[], consumes_from: &[], ..*d };
            for (&slot, l) in d.consumes.iter().zip(d.consumes_from) {
                n = n.consuming_from_in(arena, slot, LifeId(life(slot, map(l.0))));
            }
            *d = n;
        }
    }
}

/// D2841: whether kind `k`'s `peer` names a pipeline, which `to_program` maps to its program index. src.verdicts carries
/// the condition cache's varlena word mask there and runs.sort a fixed row's key count: values the map keeps. No `_` arm:
/// a new kind says which it is.
const fn peer_is_pipeline(k: ProtoKind) -> bool {
    use ProtoKind as K;
    match k {
        K::Verdicts | K::SortMerge => false,
        K::TwoPass | K::OrderedSink | K::Width | K::Facts | K::RtFilter | K::BuildSide | K::Render | K::TopGroups | K::Observe | K::Arms | K::Prune | K::Walk | K::Fetch => true,
        K::OwnerTop | K::Route | K::Literals | K::Transition | K::SetFn | K::CodeCut | K::Heads | K::Drain | K::RightMarks | K::SemiMarks | K::HandOver | K::Spill | K::Spool | K::SpoolChunks => true,
        K::Rerun | K::Cache | K::Init | K::ParallelOuter | K::Cte | K::SubHash | K::SubHashEpq | K::SubResult | K::Rescan | K::NestPrune | K::Generation | K::SetOp | K::PartTable | K::OwnedView => true,
        K::PartRuns | K::Window | K::Markable | K::KWay | K::CoSplit | K::Volatile | K::AggArena | K::Follows | K::TwoPassFloor | K::UserKeys | K::EmitRuns => true,
    }
}


/// The calling thread's running statement, else its session: a lowering's IR is its statement's from birth (the plan's
/// owner ends before the compile). Planmem's `memledger::charge::statement_owner`, carried here until it lands.
fn statement_owner() -> memledger::Owner {
    let acct = match memledger::charge::context().0 {
        memledger::Owner::Portal { acct, .. } => acct,
        o => return o,
    };
    match memledger::running_statement(acct) {
        0 => memledger::Owner::Session(acct),
        stmt => memledger::Owner::Statement { acct, stmt },
    }
}

#[cfg(test)]
mod tests {
    use gravity_program::protos::{Proto, ProtoKind, NONE};
    use gravity_program::{Arena, End, LifeId, Pipeline};

    thread_local! {
        /// How many times this thread's `verify` calls ran the verifiers (verify once's test).
        pub(super) static VERIFY_RUNS: core::cell::Cell<u32> = const { core::cell::Cell::new(0) };
    }

    /// Verify once (main 09-24): an IR verifies once per process; one of the same shape with one operand changed (a use
    /// its definition does not dominate) is another key, so it is verified, refused, and never filed as verified.
    #[test]
    fn an_ir_verifies_once_and_a_mutated_one_of_its_shape_is_verified_again() {
        use gravity_ir::{builder::ProgramBuilder, IntBinOp, IntCond, Type};
        let diamond = |undominated: bool| {
            let mut pb = ProgramBuilder::new();
            let f = pb.declare_func("verify_once", &[Type::Ptr], &[Type::I64]);
            let mut b = pb.define(f);
            let p = b.params()[0];
            let c0 = b.load(Type::I64, p, 4242);
            let z = b.iconst(Type::I64, 0);
            let c = b.icmp(IntCond::Ne, c0, z);
            let (arm, join) = (b.create_block(&[]), b.create_block(&[]));
            b.cond_br(c, (arm, &[]), (join, &[]));
            b.switch_to(arm);
            let x = b.load(Type::I64, p, 8);
            b.br(join, &[]);
            b.switch_to(join);
            let y = b.int_bin(IntBinOp::Add, if undominated { x } else { c0 }, z);
            b.ret(&[y]);
            (pb.finish(), f)
        };
        let runs = || VERIFY_RUNS.with(core::cell::Cell::get);
        let ((good, f), (bad, g)) = (diamond(false), diamond(true));
        let scratch = mcx::MemoryContext::new("verify test");
        let m = scratch.mcx();
        let r0 = runs();
        assert!(crate::catch(1, || super::verify(&good, None, &[f], &[mcx::PgVec::new_in(m)], 1, m)).is_ok());
        assert!(crate::catch(1, || super::verify(&good, None, &[f], &[mcx::PgVec::new_in(m)], 1, m)).is_ok());
        assert_eq!(runs() - r0, 1, "the same IR verifies once");
        for _ in 0..2 {
            let r = crate::catch(1, || super::verify(&bad, None, &[g], &[mcx::PgVec::new_in(m)], 1, m)).expect_err("the mutated IR is verified and refused");
            assert!(matches!(r.reason, crate::Reason::Bug { .. }), "{}", r.reason);
        }
        assert_eq!(runs() - r0, 3, "the mutated IR verifies every time");
        assert!(crate::catch(1, || super::verify(&good, None, &[f], &[mcx::pg_vec![in m; gravity_ir::Block(0)]], 1, m)).is_ok());
        assert_eq!(runs() - r0, 4, "another bounded set is another key");
        for k in [0x5401_u128, 0x5401, 0x5402 << 64] {
            assert!(crate::catch(1, || super::verify(&good, Some(k), &[f], &[mcx::PgVec::new_in(m)], 1, m)).is_ok());
        }
        assert_eq!(runs() - r0, 6, "a carried key stands for the IR: once each");
    }

    /// Verify once's key (main 09-24): one IR keyed with another function, another function list, a bounded block more or
    /// another, or one constant changed is another key; the same inputs are the same key.
    #[test]
    fn the_verified_key_separates_a_func_a_bounded_block_and_a_constant() {
        use gravity_ir::{builder::ProgramBuilder, Block, Func, Type};
        let two = |k: i64| {
            let mut pb = ProgramBuilder::new();
            let fs: Vec<Func> = (0..2)
                .map(|i| {
                    let f = pb.declare_func(&format!("f{i}"), &[], &[Type::I64]);
                    let mut b = pb.define(f);
                    let c = b.iconst(Type::I64, k);
                    b.ret(&[c]);
                    f
                })
                .collect();
            (pb.finish(), fs)
        };
        let ((p, fs), (q, _)) = (two(7), two(8));
        let code = |ir: &gravity_ir::Program| gravity_lower::code_key(ir);
        let rig = mcx::MemoryContext::new("test");
        let v = |b: &[Block]| mcx::to_vec_in(b, rig.mcx());
        let key = |ir: &gravity_ir::Program, funcs: &[Func], bounded: &[mcx::PgVec<'_, Block>]| super::verified_key(code(ir), None, funcs, bounded);
        let base = key(&p, &fs[..1], &[v(&[])]);
        assert_eq!(base, key(&p, &fs[..1], &[v(&[])]), "the same inputs are the same key");
        let keys = [
            base,
            key(&p, &fs[1..], &[v(&[])]),
            key(&p, &fs, &[v(&[]), v(&[])]),
            key(&p, &fs[..1], &[v(&[Block(0)])]),
            key(&p, &fs[..1], &[v(&[Block(1)])]),
            key(&p, &fs[..1], &[v(&[Block(0), Block(1)])]),
            key(&q, &fs[..1], &[v(&[])]),
            super::verified_key(code(&p), Some(1), &fs[..1], &[v(&[])]),
            super::verified_key(code(&p), Some(1 << 64), &fs[..1], &[v(&[])]),
            super::verified_key(code(&p), Some(1), &fs[1..], &[v(&[])]),
        ];
        assert_eq!(super::verified_key(code(&p), Some(1), &fs[..1], &[v(&[])]), super::verified_key(code(&q), Some(1), &fs[..1], &[v(&[])]), "a carried key stands for the IR");
        let distinct: std::collections::BTreeSet<u128> = keys.iter().copied().collect();
        assert_eq!(distinct.len(), keys.len(), "every changed input is another key: {keys:x?}");
    }

    /// A pass spliced after graph pipeline 0 of three (virtual id 3) runs second: graph 1 and 2 become 2 and 3, and every
    /// protocol's pipeline / peer and every named life follows them; NONE stays NONE.
    #[test]
    fn a_spliced_pass_remaps_every_pipeline_reference() {
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let at = [0u16, 2, 3, 1];
        let mut protos = [Proto::new(ProtoKind::TwoPass, 3).peer(0), Proto::new(ProtoKind::Facts, 2)];
        let p = Pipeline::eligible(-1, End::Sink, 0);
        let mut pipes = [p, p.consuming_from_in(a, 5, LifeId(3)).consuming_from_in(a, 7, LifeId(0)), p, p.consuming_from_in(a, 5, LifeId(1))];
        super::to_program(a, &mut protos, &mut pipes, &at, -1);
        assert_eq!((protos[0].pipeline, protos[0].peer, protos[1].pipeline, protos[1].peer), (1, 0, 3, NONE));
        assert_eq!((pipes[1].consumes, pipes[1].consumes_from), (&[5u8, 7][..], &[LifeId(1), LifeId(0)][..]));
        assert_eq!((pipes[3].consumes, pipes[3].consumes_from), (&[5u8][..], &[LifeId(2)][..]));
        assert!(pipes[0].consumes.is_empty() && pipes[2].consumes_from.is_empty());
    }

    /// B4c-2: a spill pair (virtual 4) spliced after the probe (graph 1) moves its seeded drain (graph 2) to program 3.
    /// The seeded Arms (1, 2) becomes (1, 3); spill_pair's own (1, 2) on the same probe becomes the same union (merge_arms
    /// would make it so); a reader naming the probe's life reads the union's last arm, the drain. An Append's run (no
    /// Follows names it) moves only its start.
    #[test]
    fn a_seeded_run_spans_a_spliced_pair_and_unions_with_its_run() {
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let at = [0u16, 1, 3, 4, 2];
        let mut protos = [
            Proto::new(ProtoKind::Arms, 1).slot(6).arg(2),
            Proto::new(ProtoKind::Follows, 2).peer(1),
            Proto::new(ProtoKind::Arms, 1).slot(6).arg(2),
            Proto::new(ProtoKind::Arms, 2).slot(3).arg(2),
        ];
        protos[2].pipeline = 1;
        let p = Pipeline::eligible(-1, End::Sink, 0);
        let mut pipes = [p, p, p, p, p.consuming_from_in(a, 6, LifeId(1))];
        super::to_program(a, &mut protos, &mut pipes, &at, -1);
        assert_eq!((protos[0].pipeline, protos[0].arg), (1, 3), "the seeded run takes in the spliced pair");
        assert_eq!((protos[2].pipeline, protos[2].arg), (1, 3), "spill_pair's run on the seeded probe is the same union");
        assert_eq!((protos[3].pipeline, protos[3].arg), (3, 2), "an Append's run moves its start only");
        assert_eq!(pipes[4].consumes_from, &[LifeId(3)][..], "a reader of the probe's life reads the union's last arm");
    }

    /// D3218: a seeded run (probe 0 .. drain 3, a plain fold's slot 5) whose span holds a hash build that decompose gave the
    /// same number (pipeline 1): its reader (2) keeps the build's life, which tail.rs `renumber_seeded` then renames; a
    /// reader of the probe's life still reads the run's last arm.
    #[test]
    fn a_build_inside_a_seeded_run_under_its_slot_number_is_no_arm() {
        use gravity_abi::breaker::{Breaker, BreakerKind, Budget, Spill};
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let end = |kind| End::Breaker(Breaker { kind, slot: 5, bytes: 64, budget: Budget::Fixed(64), spill: Spill::None });
        let fold = Pipeline::eligible(-1, end(BreakerKind::PlainAgg), 0);
        let build = Pipeline::eligible(-1, end(BreakerKind::HashBuild), 0);
        let sink = Pipeline::eligible(-1, End::Sink, 0);
        let at = [0u16, 1, 2, 3, 4, 5];
        let mut protos = [Proto::new(ProtoKind::Arms, 0).slot(5).arg(4), Proto::new(ProtoKind::Follows, 3).peer(0)];
        let mut pipes = [fold, build, sink.consuming_from_in(a, 5, LifeId(1)), fold, sink.consuming_from_in(a, 5, LifeId(3)), sink.consuming_from_in(a, 5, LifeId(0))];
        super::to_program(a, &mut protos, &mut pipes, &at, -1);
        assert_eq!(pipes[2].consumes_from, &[LifeId(1)][..], "the build's reader keeps the build's life");
        assert_eq!(pipes[4].consumes_from, &[LifeId(3)][..], "the run's reader reads its last arm");
        assert_eq!(pipes[5].consumes_from, &[LifeId(3)][..], "a reader naming the probe's life reads the run's last arm");
    }

    /// join.buildside's skip mask (D276) follows the same map: bits 0..7 and 48..63 stay, each pipeline bit moves.
    #[test]
    fn a_spliced_pass_remaps_the_buildside_skip_mask() {
        let arena = Arena::new(1 << 12);
        let at = [0u16, 2, 3, 1];
        let arg = 0xAB00_0000_0000_00CDu64 | (1 << (8 + 1)) | (1 << (8 + 3));
        let mut protos = [Proto { arg, ..Proto::new(ProtoKind::BuildSide, 2).peer(0) }];
        super::to_program(arena.handle(), &mut protos, &mut [], &at, -1);
        assert_eq!(protos[0].arg, 0xAB00_0000_0000_00CD | (1 << (8 + 2)) | (1 << (8 + 1)));
    }

    /// D2841: src.verdicts' varlena word mask and runs.sort's key count ride `peer` but are no pipeline: a map that moves
    /// pipeline 2 leaves them (word 0 of mask 0b10, a by-value literal, is never read as a pointer), and every other kind's
    /// peer moves with its pipeline. Each kind of `ProtoKind::ALL` on its own, so its side of the match is the one asserted.
    #[test]
    fn a_spliced_pass_maps_every_pipeline_peer_and_keeps_a_value_peer() {
        let arena = Arena::new(1 << 12);
        let at = [0u16, 1, 3, 2];
        let kept = [ProtoKind::Verdicts, ProtoKind::SortMerge];
        for &k in ProtoKind::ALL {
            let mut protos = [Proto::new(k, 3).peer(0b10)];
            super::to_program(arena.handle(), &mut protos, &mut [], &at, -1);
            let peer = if kept.contains(&k) { 0b10 } else { 3 };
            assert_eq!((protos[0].pipeline, protos[0].peer), (2, peer), "{k:?}: the pipeline moves; the peer {}", if peer == 3 { "moves" } else { "stays" });
        }
        assert_eq!(ProtoKind::ALL.iter().filter(|&&k| !super::peer_is_pipeline(k)).copied().collect::<Vec<_>>(), kept, "the value peers are exactly these");
    }

    /// D296: a pipeline that consumes the slot it produces (D295's top-k drain) is `Reason::Bug` at that pipeline's plan
    /// node inside the catch; well-formed descriptors pass through untouched.
    #[test]
    fn descriptors_assemble_rejects_are_bug_refusals_at_their_node() {
        use gravity_abi::breaker::{Breaker, BreakerKind, Budget, Spill};
        use gravity_program::{ArenaVec, Layout, WritePlan};
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let hash = End::Breaker(Breaker { kind: BreakerKind::HashAgg, slot: 0, bytes: 64, budget: Budget::Fixed(64), spill: Spill::None });
        let lowered = |bad: bool| {
            let mut pipelines = ArenaVec::new_in(a);
            pipelines.push(if bad { Pipeline::eligible(-1, hash, 0).consuming_in(a, 0) } else { Pipeline::eligible(-1, hash, 0) });
            pipelines.push(Pipeline::eligible(-1, End::Sink, 0).consuming_in(a, 0));
            let layout = Layout { nsites: 0, out_natts: 0, out_rows: 1, scratch_bytes: 0, exec_bytes: 0 };
            let descs = gravity_lower::Descs { layout, sites: ArenaVec::new_in(a), out: ArenaVec::new_in(a), write: WritePlan::SELECT, pipelines, out_region: 0, owner: gravity_lower::no_owner(), no_cache: false, no_cache_why: "", instr: Vec::new(), v4: gravity_program::protos::V4Descs::new(Vec::new()), used: 0, locks: Vec::new(), writes: Vec::new(), write_end: gravity_program::program::END_STATEMENT };
            gravity_lower::Lowered { ir: gravity_ir::builder::ProgramBuilder::new().finish(), funcs: ArenaVec::new_in(a), descs, key: None, tagged: Vec::new(), staged: gravity_fn::arena::ChargedVec::none() }
        };
        let (good, bad) = (lowered(false), lowered(true));
        assert!(crate::catch(1, || super::assembles(&good, &[7, 9], 1)).is_ok());
        let r = crate::catch(1, || super::assembles(&bad, &[7, 9], 1)).expect_err("a malformed DAG refuses");
        assert!(matches!(r.reason, crate::Reason::Bug { .. }) && r.node == 7, "{} at node {}", r.reason, r.node);
    }
}

/// The extra passes (twopass.having's pass 2) a pipeline's makers add after themselves: their virtual ids, their makes
/// under those ids, and their breaker slots; `emit` above remaps them to program order (moved from px.rs, its budget).
impl<'g, 'p> Px<'g, 'p> {
    /// The virtual id the next `add_pass` returns (past the graph's inners, + passes so far): its makes declare under it.
    pub fn pass_id(&self) -> usize {
        self.gx.pipelines().end() + self.pass_after.len()
    }

    /// An extra pipeline run right after graph pipeline `after` (twopass.having's pass 2), added only by `after`'s own
    /// makers. Returns its virtual id, which every make-time pipeline reference uses (Proto pipeline / peer, LifeId, its
    /// site and words); emit remaps them to program order. At lower time `env.k` is the virtual id: a pass's mechanisms
    /// capture what they read at make time and never index `gx.pipelines()` by it.
    pub fn add_pass(&mut self, after: usize, p: ExtraPass<'g>) -> usize {
        if self.making.0 != after {
            self.bug("a pass added by a pipeline other than the one it follows");
        }
        let v = self.pass_id();
        self.pass_after.push(after);
        self.passes.push((after, v, p)); // alloc: Program
        v
    }

    /// The passes that follow graph pipeline `k`, in the order added, with their virtual ids.
    pub(crate) fn take_passes(&mut self, k: usize) -> Vec<(usize, ExtraPass<'g>)> {
        let (mine, rest) = core::mem::take(&mut self.passes).into_iter().partition(|p| p.0 == k);
        self.passes = rest;
        mine.into_iter().map(|(_, v, p)| (v, p)).collect() // alloc: Program
    }

    /// The graph pipeline pass `v` follows; `v` itself for a graph pipeline or an embedded inner. With no pass added
    /// (the common case) that is every `v`, with no walk of the graph's inners (`Pipes::end` walks every pipeline's ops:
    /// called per pipeline, it made lowering O(pipelines^2) over thousands of union arms, m4_l20).
    pub fn after_of(&self, v: usize) -> usize {
        if self.pass_after.is_empty() {
            return v;
        }
        v.checked_sub(self.gx.pipelines().end()).and_then(|i| self.pass_after.get(i).copied()).unwrap_or(v)
    }

    /// Graph pipeline `after`'s source and steps made again for pass `v`: the makers read `after`'s shape and declare
    /// under `v`, so the pass shares no site, per-open or resume word with `after`.
    pub fn remake(&mut self, after: usize, v: usize) -> (PgBox<'g, dyn SourceMech + 'g>, PgVec<'g, PgBox<'g, dyn StepMech + 'g>>) {
        if self.unions.iter().any(|u| u.0 == after) {
            self.bug("a pass remade from a union arm");
        }
        let gx = self.gx;
        let node = gx.pipelines()[after].node_id;
        let mcx = self.mcx();
        self.making(after, v, |px| {
            let source = (crate::frame::iface::kind_impl::<SourceMaker>(&node, gx.source_kind(after)).0)(px, after);
            let mut steps = mcx::reserved_in(gx.pipelines()[after].ops.len(), mcx);
            for j in 0..gx.pipelines()[after].ops.len() {
                steps.push_in((crate::frame::iface::kind_impl::<StepMaker>(&node, gx.step_need(after, j)).0)(px, after, j));
            }
            (source, steps)
        })
    }

    /// A breaker slot no graph pipeline produces or consumes (a pass's own breaker).
    pub fn new_slot(&mut self) -> u8 {
        let graph = self.gx.pipelines().iter().flat_map(|p| p.desc.consumes.iter().copied().chain(p.desc.produces()));
        let s = graph.max().map_or(0, |s| s as usize + 1) + self.new_slots;
        if s >= NO_SLOT as usize {
            self.bug("no breaker slot left for a pass");
        }
        self.new_slots += 1;
        s as u8
    }
}
