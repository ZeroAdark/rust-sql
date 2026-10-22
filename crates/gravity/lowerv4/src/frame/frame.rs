//! The frame (lowerv4 plan §2.1, §3.1; lowerv3 design §7.2, §3.6): a graph's mechanisms resolved through the registry, and
//! the one function form every pipeline lowers to: open, per window the source's rows through the steps into the end, the
//! window's end, the exits. Per window at most two bodies of the chain are emitted: the 64-row block path (a first step
//! with a block form over a source with block values: whole blocks by mask, H01) and the straight row loop (the window's
//! tail, a failed guard, every other window). A resumed call re-enters the window loop at the cursor its stop wrote back;
//! an expanding step (H32) stops with the cursor at its row (`RowVals::at`) and continues at its offset word (H17 c). The
//! frame alone writes the per-open words and emits the cancel checks.
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::{Block, Func, IntBinOp, IntCond, IntUnOp, Type, Value};
use gravity_program::Pipeline;
use mcx::{CollectIn, GrowIn};

use crate::face::end::{EndMech, SinkKind};
use crate::face::strategy::{BODY_FORM, BODY_VERSIONED, LOOP_HOIST, MASK_RUN, MASK_VEC};
use crate::face::source::{Body, BodyForm, BodyImpl, DecodeOrder, Packed, SourceMech, WinConds};
use crate::face::step::{BlockForm, CarryImpl, Hoist, HoistAt, HoistImpl, LoopRoad, MaskRunImpl, StepMech};
use crate::frame::cancel;
use crate::frame::env::Core;
use crate::frame::env::{BatchRow, BatchRows, Batched};
use crate::frame::env::Env;
use crate::frame::instr;
use crate::frame::open::OpenWords;
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::{self, Exits, WriteBack};
use crate::hir::{Ex, Ir};

pub use crate::frame::row::{BlockVals, Mask, WordWin};
pub use crate::frame::mechs::resolve;

/// One window of the source's rows, `[cursor, end)`, with the facts the source knows of it (H06: nulls, selection, runs,
/// form); a fact left `None` is unknown and takes the straight body.
#[derive(Clone, Copy, Debug)]
pub struct Win {
    pub cursor: Value,
    pub end: Value,
    pub facts: WinConds,
}

/// A step's continuation: the row it hands on, through the rest of the chain into the end; the carry after is returned.
pub type Next<'a, 'e> = &'a mut dyn FnMut(&mut FunctionBuilder<'_>, &mut Env<'e, '_>, &RowVals) -> Carry<'e>;

pub type MakeSource = for<'g, 'p> fn(&mut Px<'g, 'p>, usize) -> mcx::PgBox<'g, dyn SourceMech + 'g>;
pub type MakeStep = for<'g, 'p> fn(&mut Px<'g, 'p>, usize, usize) -> mcx::PgBox<'g, dyn StepMech + 'g>;
pub type MakeEnd = for<'g, 'p> fn(&mut Px<'g, 'p>, usize) -> mcx::PgBox<'g, dyn EndMech + 'g>;

/// A pipeline-level kind's maker as a milestone table registers it (`(KindId::SrcPgrc, &SourceMaker(make))`): sources,
/// steps (every op kind) and ends (the sink, fold and keyed ends, runs, builds, row ops).
pub struct SourceMaker(pub MakeSource);
pub struct StepMaker(pub MakeStep);
pub struct EndMaker(pub MakeEnd);

/// A pipeline's mechanisms, each made by its kind's maker.
pub struct Mechs<'g> {
    pub k: usize,
    pub source: mcx::PgBox<'g, dyn SourceMech + 'g>,
    pub steps: mcx::PgVec<'g, mcx::PgBox<'g, dyn StepMech + 'g>>,
    pub end: mcx::PgBox<'g, dyn EndMech + 'g>,
    pub site: Option<usize>,
    pub desc: Pipeline,
    /// The program's frame-words site (`Px::add_words_site`), set once every pipeline's mechanisms are made.
    pub words: Option<usize>,
    /// D2147: the byte offset of the end's state in the frame words (`ResumeWord::EndState`), set by `emit` for a lazy
    /// build's probe pipeline; `None`: a stack slot.
    pub state_word: Option<i32>,
}

/// How one body walks the chain: the steps it elides (bit j: op j, a covered window's leading filters) and, in a body
/// that runs uncovered windows, the step whose entry the source's condition-cache fill sees the row at (the row passed
/// every covered step: `SourceMech::passed`), and the step whose entry the source's late cells are made at
/// (`SourceMech::row_late`, decode.late).
#[derive(Clone, Copy)]
struct Walk<'a> {
    skip: u64,
    fill: Option<(usize, &'a dyn SourceMech, &'a Win)>,
    late: Option<(usize, &'a dyn SourceMech, &'a Win)>,
}

/// The steps from `j` on over `row`, each through the next's continuation; the last hands the row to the end. `env.step`
/// names the step lowering; `env.masked` holds only for the first step (a block's mask already passed the row).
fn chain<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, steps: &[mcx::PgBox<'_, dyn StepMech + '_>], j: usize, row: &RowVals, w: Walk<'_>, last: Next<'_, 'e>) -> Carry<'e> {
    crate::stack_check(); // D3832
    if let Some((_, src, win)) = w.fill.filter(|f| f.0 == j) {
        src.passed(b, env, win, row);
    }
    if let Some((_, src, win)) = w.late.filter(|l| l.0 == j) {
        let copy = RowVals { row: crate::cell::Row { cells: mcx::to_vec_in(&row.row.cells[..], env.mcx) }, vals: mcx::to_vec_in(&row.vals[..], env.mcx), cols: mcx::to_vec_in(&row.cols[..], env.mcx), carry: row.carry.copy_in(env.mcx), next: row.next, at: row.at };
        let full = src.row_late(b, env, win, copy);
        return chain(b, env, steps, j, &full, Walk { late: None, ..w }, last);
    }
    bound(b, env, j);
    match steps.split_first() {
        None => {
            env.step = None;
            let t = super::jit::tag(b, env.jit_body, format_args!("end"), env.jit_end);
            let c = last(b, env, row);
            b.restore_tag(t);
            c
        }
        // a step past the 64th is never elided: `skip >> j` would wrap to bit j % 64 and drop it (D2521)
        Some((_, rest)) if j < 64 && w.skip >> j & 1 == 1 => chain(b, env, rest, j + 1, row, w, last),
        Some((s, rest)) => {
            env.step = Some(j);
            let t = super::jit::tag(b, env.jit_body, format_args!("s{j}"), s.mech_name());
            let c = s.lower(b, env, row, &mut |b, env, r| {
                let masked = core::mem::replace(&mut env.masked, false);
                let c = chain(b, env, rest, j + 1, r, w, &mut *last);
                (env.masked, env.step) = (masked, Some(j));
                c
            });
            b.restore_tag(t);
            env.step = None;
            c
        }
    }
}

/// EXPLAIN ANALYZE: a row entering step `j` (the end, past the last) counts a row of each node it left (`instr::Bounds`),
/// unless it is a row a stop inside an expanding step at or after `j` re-enters (counted when it first came: stop.rs `held`).
pub(crate) fn bound(b: &mut FunctionBuilder<'_>, env: &Env, j: usize) {
    if !instr::bounded() {
        return;
    }
    let nodes = instr::row_at(j, env.mcx);
    if nodes.is_empty() {
        return;
    }
    // an end counted from its write-backs' `out` (`instr::by_out`): a row past a join.mult probe adds its count's rest
    let by_out = instr::end_by_out(j);
    if by_out && env.mult.is_none() {
        return;
    }
    // the body that lowers a step as non-expanding (`Env::single`) never sets its offset but by a wait: no row re-enters
    // through it otherwise
    let mut offs: mcx::PgVec<'_, i32> = env.expands.iter().filter(|e| e.0 >= j && env.single != Some(e.0)).map(|e| e.1).collect_in(env.mcx);
    offs.extend_in(env.waits.iter().filter(|w| w.0 >= j && !offs.contains(&w.1)).map(|w| w.1).collect_in(env.mcx));
    let n = match env.words.filter(|_| !offs.is_empty()) {
        None => env.one,
        Some(words) => {
            let set = offs.iter().fold(env.zero, |a, &o| {
                let w = b.load(Type::I64, words, o);
                b.int_bin(IntBinOp::Or, a, w)
            });
            let fresh = b.icmp(IntCond::Eq, set, env.zero);
            b.select(fresh, env.one, env.zero)
        }
    };
    // join.mult: past a probe that hands a row on once with its key's count (`Env::mult`), the row stands for that many
    let n = match env.mult {
        Some(m) if by_out => {
            let m1 = b.int_bin(IntBinOp::Sub, m, env.one);
            b.int_bin(IntBinOp::Mul, n, m1)
        }
        Some(m) => b.int_bin(IntBinOp::Mul, n, m),
        None => n,
    };
    nodes.into_iter().for_each(|x| instr::count_row(b, env.ctx, x, n));
}

/// One pipeline's function: `name(ctx) -> status`. A `once` source has no window loop: a resumed call is done, unless an
/// expanding step stopped inside its row.
pub fn pipeline_fn<'m>(pb: &mut ProgramBuilder, core: Core, hir: Ir, exs: &'m [Ex<'_>], name: &str, node: i32, m: &mut Mechs<'_>, mcx: mcx::Mcx<'m>) -> (Func, mcx::PgVec<'m, Block>) {
    pipeline_fn_in(pb, core, hir, exs, &crate::face::ext::claimed(exs), name, node, m, mcx)
}

/// `pipeline_fn`, with `claimed` the expressions of `exs` an extension claimed (`face::ext::claimed`, found once for
/// every function of the program).
#[allow(clippy::too_many_arguments)]
pub fn pipeline_fn_in<'m>(pb: &mut ProgramBuilder, core: Core, hir: Ir, exs: &'m [Ex<'_>], claimed: &[u32], name: &str, node: i32, m: &mut Mechs<'_>, mcx: mcx::Mcx<'m>) -> (Func, mcx::PgVec<'m, Block>) {
    let (f, b, _) = lower_fn(pb, core, hir, exs, claimed, name, node, m, None, mcx);
    (f, b)
}

/// A pipeline's function and its bounded blocks (`pipeline_fn`).
pub type PipeFn<'m> = (Func, mcx::PgVec<'m, Block>);

/// D3168 S0: a pipeline's staged function (`pipeline_fns_in`): lowered in a module of its own (the function alone), its
/// bounded blocks and the shape's memo tag (0: none); or `Want`: the shape's memo skipped its lowering (gravity_program
/// `stage_memo`), so the plain function's guard reruns it with its stages off (`Code::call`) and marks the shape.
pub enum StagedFn<'m> {
    Lowered(gravity_ir::Program, mcx::PgVec<'m, Block>, u64),
    Want(u64),
}

/// The module a staged function lowers into, with its own core hosts and hosted calls (program.rs `functions`).
pub type SideModule<'s> = &'s dyn Fn() -> (ProgramBuilder, Core, Ir);

/// D3168 S0 (join.probe.staged): `pipeline_fn_in`, unless a probe of the pipeline stages its rows (`StepMech::stages`) and
/// `split` allows it (a long union arm run keeps one function: twin.rs folds its arms by their code). Then the pipeline
/// lowers twice: `<name>s`, the staged function (its windows past L2 run the stages, the others the per-row bodies), in a
/// module of its own (`side`), and `name`, the plain one: no probe stages there, so its code is the road-off code (a staged
/// body that never runs costs the per-row body its registers: TPDE's loop invariants go to the deeper stage loops), and
/// its entry, before anything it runs, returns `status::STAGED` when the sealed build outgrows L2 (`StepMech::stage_entry`),
/// so the call runs the staged one. Every call of a run reads one sealed header, so a run takes one function. The program
/// is the plain functions alone whether or not a staged one was lowered, so a shape's later lowering finds its code
/// cached. `tag`: the shape's memo entry (`stage_memo::tag`; none: every lowering lowers the staged function): a later
/// lowering of a shape whose staged function no call chose lowers the plain function alone (`Want`).
#[allow(clippy::too_many_arguments)]
pub fn pipeline_fns_in<'m>(pb: &mut ProgramBuilder, core: Core, hir: &Ir, exs: &'m [Ex<'_>], claimed: &[u32], name: &str, node: i32, m: &mut Mechs<'_>, split: bool, (side, tag): (SideModule<'_>, Option<u64>), mcx: mcx::Mcx<'m>) -> (PipeFn<'m>, Option<StagedFn<'m>>) {
    use gravity_program::code::stage_memo::{self, Split};
    let j = {
        let _bounds = instr::enter(m.k);
        // an EvalPlanQual COPY row lowers the steps' row before their windows (`copy_entry`): one function only
        m.steps.iter().position(|s| s.stages()).filter(|_| split && m.source.copy_entry().is_none() && crate::ops::hashjoin::strategies::probe_stage::split())
    };
    let Some(j) = j else {
        let (f, b, _) = lower_fn(pb, core, hir.per_func(), exs, claimed, name, node, m, None, mcx);
        return ((f, b), None);
    };
    match tag.map_or(Split::Lower, stage_memo::split) {
        Split::Plain => {
            gravity_program::code::STAGED_SKIPPED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            // the plain function a lowering that found no staged body made (`Lower`): the same program, so its code is cached
            let (f, b, _) = lower_fn(pb, core, hir.per_func(), exs, claimed, name, node, m, Some(None), mcx);
            ((f, b), None)
        }
        Split::Want => {
            gravity_program::code::STAGED_SKIPPED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            fires::fire("join.probe.staged"); // shapecheck fire signal (D3168): the probe stages; its shape's memo skipped the lowering
            let (f, b, _) = lower_fn(pb, core, hir.per_func(), exs, claimed, name, node, m, Some(Some(j)), mcx);
            ((f, b), tag.map(StagedFn::Want))
        }
        Split::Lower => {
            let (mut spb, score, shir) = side();
            let (_, bs, staged) = lower_fn(&mut spb, score, shir, exs, claimed, &mcx::pg_format!(mcx, "{}s", name), node, m, None, mcx);
            // D4875: the shape's memo learns the outcome here, never at `split`: a lowering stopped before this line (an
            // interrupt; install lowers again) records nothing, so the next one lowers the staged function again
            let t = tag.unwrap_or(0);
            if staged {
                fires::fire("join.probe.staged.split"); // shapecheck fire signal (D3168 S0): a staged function beside the plain one
                if t != 0 {
                    stage_memo::lowered(t);
                }
            } else if t != 0 {
                stage_memo::dead(t);
            }
            let (f, b, _) = lower_fn(pb, core, hir.per_func(), exs, claimed, name, node, m, Some(Some(j).filter(|_| staged)), mcx);
            ((f, b), staged.then(|| StagedFn::Lowered(spb.finish(), bs, t)))
        }
    }
}

/// The pipeline's function, its bounded blocks and whether a probe staged its rows in it (`Env::staged`); `plain`
/// (`pipeline_fns_in`): the plain one (no probe stages), whose entry returns `status::STAGED` when step `j`'s
/// `stage_entry` holds, so its call runs the staged function instead (gravity_program `Code::call`; none: no staged one).
#[allow(clippy::too_many_arguments)]
fn lower_fn<'m>(pb: &mut ProgramBuilder, core: Core, hir: Ir, exs: &'m [Ex<'_>], claimed: &[u32], name: &str, node: i32, m: &mut Mechs<'_>, plain: Option<Option<usize>>, mcx: mcx::Mcx<'m>) -> (Func, mcx::PgVec<'m, Block>, bool) {
    let _bounds = instr::enter(m.k);
    m.source.declare(pb);
    m.steps.iter_mut().for_each(|s| s.declare(pb)); // M8: an embedded inner's source
    let mut words = OpenWords::new_in(mcx);
    m.source.open_words(&mut words);
    m.steps.iter().for_each(|s| s.open_words(&mut words));
    m.end.open_words(&mut words);
    let once = m.source.once();
    // EXPLAIN ANALYZE: an end whose `out` is the rows it took is counted at the write-backs, where every row it takes
    // passes one: not under a step that holds rows back (its flush hands them on past the last) nor a once source
    instr::by_out(m.end.out_counts_rows() && !once && !m.steps.iter().any(|s| s.flushes()));
    // a stop inside an expanding step's row resumes into that row (its offset word), so a once source under one keeps its
    // window on resume instead of finishing at once
    // D3289: so does a step's stop AT its row (`StepMech::reentry`)
    let resumable = !once || m.steps.iter().any(|s| s.expand().is_some() || s.reentry().is_some());
    let f = pb.declare_func(name, &[Type::Ptr], &[Type::I64]);
    let mut b = pb.define(f);
    let x = stop::exits(&mut b);
    let mut env = Env::new_in(&mut b, core, hir, exs, x, mcx);
    env.unstaged = plain.is_some();
    if let Some(j) = plain.flatten() {
        let (l_staged, l_plain, l_past) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        m.steps[j].stage_entry(&mut b, &mut env, (l_past, l_plain));
        // a call `Code::call` reruns with its stages off (no staged function lowered) runs the per-row body
        b.switch_to(l_past);
        let flags = b.load_zext(Type::I32, env.ctx, gravity_abi::ctx::hdr::FLAGS as i32);
        let bit = b.iconst(Type::I64, gravity_abi::ctx::FLAG_STAGE_OFF as i64);
        let off = b.int_bin(IntBinOp::And, flags, bit);
        let on = b.icmp(IntCond::Eq, off, env.zero);
        b.cond_br(on, (l_staged, &[]), (l_plain, &[]));
        b.switch_to(l_staged);
        let r = b.iconst(Type::I64, gravity_abi::status::STAGED);
        b.ret(&[r]);
        b.switch_to(l_plain);
    }
    env.jit_end = m.end.mech_name();
    instr::open_base(&mut b, env.ctx);
    instr::by_window(&mut b, !once && !m.steps.iter().any(|s| s.flushes()));
    (env.site, env.k, env.node) = (m.site, m.k, node);
    // D2580: a step's count reached before a step that holds rows back records the step met (`Env::full`), so the
    // count-reached exit hands the held rows on (`flush_path`) before `done`; set on every call, read only on that exit
    if let Some(last) = m.steps.iter().rposition(|s| s.flushes()) {
        let slot = b.declare_stack_slot(8, 8);
        let at = b.stack_addr(slot);
        let none = b.iconst(Type::I64, crate::frame::env::FULL_NONE);
        b.store(at, 0, none);
        env.full_from = Some(crate::frame::env::FullFrom { at, k: m.k, node, last });
    }
    // D3289: and each step that may stop AT its row (`StepMech::reentry`), read by the steps before it as a later offset
    let again = m.steps.iter().enumerate().filter_map(|(j, s)| s.reentry().map(|o| (j, o)));
    env.expands = m.steps.iter().enumerate().filter_map(|(j, s)| s.expand().map(|e| (j, e.offset))).chain(again).collect_in(env.mcx);
    env.waits = m.steps.iter().enumerate().filter_map(|(j, s)| s.wait_offset().map(|o| (j, o))).collect_in(env.mcx);
    if let Some(ws) = m.words {
        env.words = Some(b.load(Type::Ptr, env.ctx, (gravity_abi::ctx::site_off(ws) + gravity_abi::site::site::KEYS) as i32));
    }
    env.hir.enter(&mut b);
    let bytes = m.end.state_bytes();
    if let Some(off) = m.state_word {
        let w = env.words.unwrap_or_else(|| crate::refuse!(node, crate::admit::refusal::Reason::Bug { payload: "an end state word without the words site" }));
        env.state = Some(b.ptr_add(w, env.zero, 1, off));
    } else if bytes > 0 {
        let slot = b.declare_stack_slot(bytes, 8);
        env.state = Some(b.stack_addr(slot));
    }
    super::jit::tag(&mut b, "fn", format_args!("prologue"), "");
    // D2056: the outer checks first (the feeder's), then each step's own, in step order
    m.steps.iter().filter(|s| s.outer()).chain(m.steps.iter().filter(|s| !s.outer())).for_each(|s| s.prologue(&mut b, &mut env, &x));
    crate::face::ext::entries(&mut b, &mut env, exs, claimed);
    let mut hoist = Hoist::new_in(mcx);
    m.steps.iter().for_each(|s| s.hoist(&mut hoist));
    let open_hoists = env.hoisted.len();
    // D2152: a step that passes no row this call closes it (`StepMech::closes`): a scan source then never opens (its keys,
    // e.g. an index key over an InitPlan, are never evaluated) and no window runs; a breaker's source reads no parameter
    // D2141: a step before the last one that holds rows back whose window ends the call (a gate false, a count reached
    // on a resumed call, an empty build) or that closes it at the open: PG runs the nodes above it over an empty input,
    // so the holding steps past it hand their rows on (a RIGHT / FULL merge's ENDOUTER). Step j's done is `wdone[j]`,
    // lowered last: the windows past j, then the flush from j (`flush_path`, as the count-reached exit's: D2580).
    let wdone: mcx::PgVec<'_, Block> = (0..m.steps.iter().rposition(|s| s.flushes()).unwrap_or(0)).map(|_| b.create_block(&[])).collect_in(env.mcx);
    let xj = |j: usize| wdone.get(j).map_or(x, |&l| Exits { l_done: l, ..x });
    let (mut closed, mut closed_lo) = (None, None);
    let mut closers: mcx::PgVec<'_, (usize, Value)> = mcx::PgVec::new_in(env.mcx);
    // D3422: a pipeline without a driving site (a breaker's emit, a Result's or a function scan's words) keeps opening
    // its source, but under EXPLAIN ANALYZE its closing steps still shut the loops of the nodes below them, as PG never
    // starts a node its zero LIMIT or false gate never pulls (made in the instrumented program only: the plain one is
    // unchanged)
    let drives = m.desc.driving_site >= 0;
    for (j, s) in m.steps.iter().enumerate().filter(|_| drives || instr::on()) {
        if let Some(c) = s.closes(&mut b, &env) {
            if drives && j < wdone.len() {
                closed_lo = Some(closed_lo.map_or((j, c), |(f, a)| (f, b.int_bin(IntBinOp::Or, a, c))));
            } else if drives {
                closed = Some(closed.map_or(c, |a| b.int_bin(IntBinOp::Or, a, c)));
            }
            closers.push_in((j, c));
        }
    }
    if instr::on() {
        for (j, s) in m.steps.iter().enumerate() {
            if let Some(c) = s.met_at_open(&mut b, &env) {
                closers.push_in((j, c));
            }
        }
    }
    let l_open = b.create_block(&[]);
    let (l_loop, l_win) = if once {
        let l_win = b.create_block(&[]);
        (if resumable { l_win } else { x.l_done }, l_win)
    } else {
        cancel::header(&mut b, env.ctx, x.l_cancel, &[], env.mcx)
    };
    stop::dispatch(&mut b, &env, l_open, l_loop);
    b.switch_to(l_open);
    super::jit::tag(&mut b, "fn", format_args!("open"), "");
    stop::mark_open(&mut b, &env);
    words.write_all(&mut b, node, env.ctx, env.state, env.words);
    instr::count_open(&mut b, env.ctx, node, &closers[..], env.mcx);
    super::jit::tag(&mut b, "fn", format_args!("end.open"), env.jit_end);
    m.end.open(&mut b, &mut env);
    super::jit::tag(&mut b, "fn", format_args!("open"), "");
    copy_entry(&mut b, &mut env, m, &x);
    if let Some(c) = closed {
        let l_src = b.create_block(&[]);
        b.cond_br(c, (x.l_done, &[]), (l_src, &[]));
        b.switch_to(l_src);
    }
    if let Some((f, c)) = closed_lo {
        let l_src = b.create_block(&[]);
        b.cond_br(c, (wdone[f], &[]), (l_src, &[]));
        b.switch_to(l_src);
    }
    super::jit::tag(&mut b, "fn", format_args!("src.open"), m.source.mech_name());
    let r = m.source.open(&mut b, &mut env);
    let neg = b.icmp(IntCond::Slt, r, env.zero);
    b.cond_br(neg, (x.l_exit, &[r]), (if once { l_win } else { l_loop }, &[]));
    b.switch_to(l_win);
    super::jit::tag(&mut b, "fn", format_args!("window"), "");
    if once {
        cancel::check(&mut b, env.ctx, x.l_cancel);
    }
    m.steps.iter().enumerate().for_each(|(j, s)| s.window(&mut b, &mut env, &xj(j)));
    if gravity_driver::mem::reset_unit() == gravity_program::Until::Window {
        env.hir.reset(&mut b);
    }
    let l_eof = b.create_block(&[Type::I64]);
    if m.steps.iter().any(|s| s.flushes()) {
        end_again(&mut b, &env, m.site, l_eof); // D2827: a call resumed inside its input end's flush
    }
    super::jit::tag(&mut b, "fn", format_args!("src.window"), m.source.mech_name());
    let win = m.source.next_window(&mut b, &mut env, l_eof);
    super::jit::tag(&mut b, "fn", format_args!("window"), "");
    env.win = Some(win);
    let l_window = b.current();
    b.switch_to(l_eof);
    let r = b.block_params(l_eof)[0];
    let at_end = b.icmp(IntCond::Eq, r, env.zero);
    // D1071: a step holding rows back hands them on at the input's end (none: the end is `done` as it always was); its
    // path is lowered after the window body, so a flush may use what the steps' `lower` learned (MERGE-2: the row's shape)
    let flushes = m.steps.iter().any(|s| s.flushes());
    let l_end = if flushes { b.create_block(&[Type::I64]) } else { x.l_done };
    // the input's end flushes every holding step (`flush_path`'s `from` before the first)
    let from_end = if flushes { mcx::pg_vec![in env.mcx; b.iconst(Type::I64, -1)] } else { mcx::PgVec::new_in(env.mcx) };
    b.cond_br(at_end, (l_end, &from_end[..]), (x.l_exit, &[r]));
    b.switch_to(l_window);
    hoisted(&mut b, &mut env, HoistAt::Window, &hoist);
    instr::count_window(&mut b, env.ctx, win.cursor, win.end, env.mcx);
    env.l_raise_row = Some(stop::raise_row(&mut b, &env, &x));
    // A1 (D3224): the window's packed numeric lane kinds tested once here, read by its row loops alone
    let kinds = m.source.num_kinds(&mut b, &mut env);
    env.num_kinds = (!kinds.is_empty()).then(|| kinds.into()); // alloc: Program
    // V4R-27: a window whose expanding step proves one row per row (a probe over a unique build) runs a second body of
    // the chain, that step lowered as a non-expanding one; the guard reads what `window` loaded (the build's header)
    let single = m.steps.iter().enumerate().find_map(|(j, s)| s.single_guard(&mut b, &mut env).map(|g| (j, g)));
    // D3194: a window whose probe's build filter is armed (join.rowfilter) runs a body that tests it, any other the plain one
    let rowf = m.steps.iter().enumerate().find_map(|(j, s)| s.rowfilter_guard(&mut b, &mut env).map(|g| (j, g)));
    // D3168: a window whose probe's build outgrows L2 runs a body where that probe's rows run its stages over a block
    // (road join.probe.staged); any other runs the bodies below
    let batch = m.steps.iter().enumerate().find_map(|(j, s)| s.batch_guard(&mut b, &mut env).map(|g| (j, g)));
    match single {
        // D3323 (join.rowfilter.single): a later probe's one-row body has no filtered version, so while the first probe's
        // filter is armed the window runs the filtered body; a window whose filter is off runs the one-row body or the plain
        Some((j, g)) if rowf.is_some_and(|(jr, _)| jr != j) && crate::ops::hashjoin::strategies::rowfilter::before_single() => {
            fires::fire("rowfilter.single"); // shapecheck fire signal (D3323): an armed filter runs ahead of a one-row body
            let (jr, gr) = rowf.expect("a filtered probe");
            let (l_rf, l_rest, l_one, l_gen) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
            b.cond_br(gr, (l_rf, &[]), (l_rest, &[]));
            b.switch_to(l_rf);
            batched_or(&mut b, &mut env, m, &win, &x, batch.filter(|bt| bt.0 == jr), Some((jr, None)));
            b.switch_to(l_rest);
            b.cond_br(g, (l_one, &[]), (l_gen, &[]));
            b.switch_to(l_one);
            env.single = Some(j);
            row_loop(&mut b, &mut env, m, &win, &x);
            env.single = None;
            b.switch_to(l_gen);
            batched_or(&mut b, &mut env, m, &win, &x, batch, None);
        }
        Some((j, g)) => {
            let (l_one, l_gen) = (b.create_block(&[]), b.create_block(&[]));
            b.cond_br(g, (l_one, &[]), (l_gen, &[]));
            b.switch_to(l_one);
            env.single = Some(j);
            row_loop(&mut b, &mut env, m, &win, &x);
            env.single = None;
            b.switch_to(l_gen);
            batched_or(&mut b, &mut env, m, &win, &x, batch, rowf.map(|(j, g)| (j, Some(g))));
        }
        None if rowf.is_some() => batched_or(&mut b, &mut env, m, &win, &x, batch, rowf.map(|(j, g)| (j, Some(g)))),
        // D3148: a window whose end admits every row (join.domain's every-key set) runs a second body without the test
        None => match m.end.every_guard(&mut b, &mut env) {
            Some(g) => {
                let (l_every, l_gen) = (b.create_block(&[]), b.create_block(&[]));
                b.cond_br(g, (l_every, &[]), (l_gen, &[]));
                b.switch_to(l_every);
                env.every = true;
                row_loop(&mut b, &mut env, m, &win, &x);
                env.every = false;
                b.switch_to(l_gen);
                row_loop(&mut b, &mut env, m, &win, &x);
            }
            None => batched_or(&mut b, &mut env, m, &win, &x, batch, None),
        },
    }
    env.num_kinds = None;
    if flushes {
        flush_path(&mut b, &mut env, m, &x, (l_end, open_hoists));
    }
    super::jit::tag(&mut b, "fn", format_args!("exit"), "");
    let src = &m.source;
    let end = &m.end;
    let steps = &m.steps;
    let mut finish = |b: &mut FunctionBuilder<'_>, env: &mut Env| {
        steps.iter().for_each(|s| s.epilogue(b, env, &x));
        end.finish(b, env, &x);
    };
    // D2580: the count reached, once written back, flushes the holding steps after the step met (`Env::full`), else is done
    let l_full_then = if env.full_from.is_some() { b.create_block(&[]) } else { x.l_done };
    let ends = (if once { x.l_done } else { l_loop }, l_full_then);
    match m.site {
        Some(k) => stop::define_exits(&mut b, &mut env, &x, ends, WriteBack::Site(k), &mut finish),
        None => {
            let mut own = |b: &mut FunctionBuilder<'_>, env: &mut Env, out: Value, cur: Value| src.write_back(b, env, cur, out);
            stop::define_exits(&mut b, &mut env, &x, ends, WriteBack::Own(&mut own), &mut finish)
        }
    }
    if let Some(f) = env.full_from.filter(|_| reached(&b, l_full_then)) {
        b.switch_to(l_full_then);
        let from = b.load(Type::I64, f.at, 0);
        let last = b.iconst(Type::I64, f.last as i64);
        let held = b.icmp(IntCond::Slt, from, last);
        b.cond_br(held, (l_end, &[from]), (x.l_done, &[]));
    }
    // D2141: a step's done before the last holding step: the windows past it (their values dominate this path's flush, the
    // window's own path left as it was lowered), then the flush from it; a later step's done is entered from here too
    for j in 0..wdone.len() {
        if !reached(&b, wdone[j]) {
            continue;
        }
        b.switch_to(wdone[j]);
        let (aside, wh) = (env.memo.set_aside(), env.hoisted.drain(open_hoists..).collect_in(env.mcx));
        let (win, raise_row) = (env.win.take(), env.l_raise_row.take());
        m.steps.iter().enumerate().skip(j + 1).for_each(|(i, s)| s.window(&mut b, &mut env, &xj(i)));
        let l = b.create_block(&[Type::I64]);
        let from = b.iconst(Type::I64, j as i64);
        b.br(l, &[from]);
        flush_path(&mut b, &mut env, m, &x, (l, open_hoists));
        (env.win, env.l_raise_row) = (win, raise_row);
        env.hoisted.extend_in(wh);
        env.memo.restore(aside);
    }
    env.bounded.extend_in(env.memo.take_bounded());
    instr::close_base(f);
    (f, env.bounded, env.staged)
}

/// D3168: the window's rows under probe `batch.0`'s guard (`batch.1`: its build past L2, no spilled partition): the
/// staged body (`Env::batch`), where that probe's rows run its stages over a block and test its build's filter while it
/// is armed; otherwise `filtered_or_plain`'s bodies (`rowf`: the filtered probe and its guard, `None` where the caller
/// already took it: the filtered body alone). With no batch guard (or one at another step than the filter's), those
/// bodies alone, as before the road.
fn batched_or(b: &mut FunctionBuilder<'_>, env: &mut Env, m: &Mechs<'_>, win: &Win, x: &Exits, batch: Option<(usize, Value)>, rowf: Option<(usize, Option<Value>)>) {
    let rest = |b: &mut FunctionBuilder<'_>, env: &mut Env| match rowf {
        Some((j, None)) => {
            env.rowfilter = Some(j);
            row_loop(b, env, m, win, x);
            env.rowfilter = None;
        }
        Some((j, Some(g))) => filtered_or_plain(b, env, m, win, x, Some((j, g))),
        None => row_loop(b, env, m, win, x),
    };
    let Some((j, g)) = batch.filter(|bt| rowf.is_none_or(|r| r.0 == bt.0)) else { return rest(b, env) };
    let (l_batch, l_rest) = (b.create_block(&[]), b.create_block(&[]));
    b.cond_br(g, (l_batch, &[]), (l_rest, &[]));
    b.switch_to(l_batch);
    env.batch = Some(j);
    row_loop(b, env, m, win, x);
    env.batch = None;
    b.switch_to(l_rest);
    rest(b, env);
}

/// D3194: the window's rows under probe `rowf.0`'s armed build filter (`rowf.1`): the body where it tests the filter
/// (`Env::rowfilter`), else the plain body; with no such probe, the plain body alone.
fn filtered_or_plain(b: &mut FunctionBuilder<'_>, env: &mut Env, m: &Mechs<'_>, win: &Win, x: &Exits, rowf: Option<(usize, Value)>) {
    let Some((j, g)) = rowf else { return row_loop(b, env, m, win, x) };
    let (l_rf, l_plain) = (b.create_block(&[]), b.create_block(&[]));
    b.cond_br(g, (l_rf, &[]), (l_plain, &[]));
    b.switch_to(l_rf);
    env.rowfilter = Some(j);
    row_loop(b, env, m, win, x);
    env.rowfilter = None;
    b.switch_to(l_plain);
    row_loop(b, env, m, win, x);
}

/// D2827: before the source's next window in a frame whose input end flushes a holding step. A scan site that reported its
/// end (state EOF) means the call resumed inside that end's flush (an open's seek leaves the site CLOSED, and the end
/// leaves the window loop at once), so it enters `l_eof` with status 0, the end again, never the source: a site at its
/// end seeks anew at its next window and the input would restart at its first row. A source without a site keeps its
/// own position at its end. The pipeline's frame and a nest inner's walk (rerun/inner.rs) both take it.
pub(crate) fn end_again(b: &mut FunctionBuilder<'_>, env: &Env, site: Option<usize>, l_eof: Block) {
    let Some(k) = site else { return };
    let st = b.load_zext(Type::I32, env.ctx, (gravity_abi::ctx::site_off(k) + gravity_abi::site::site::STATE) as i32);
    let eof = b.iconst(Type::I64, gravity_abi::site::site::STATE_EOF as i64);
    let (met, l_src) = (b.icmp(IntCond::Eq, st, eof), b.create_block(&[]));
    b.cond_br(met, (l_eof, &[env.zero]), (l_src, &[]));
    b.switch_to(l_src);
}

/// `flush_path_in` under the JIT map's body `flush` (frame/jit.rs).
fn flush_path(b: &mut FunctionBuilder<'_>, env: &mut Env, m: &Mechs<'_>, x: &Exits, lo: (Block, usize)) {
    let (body, t) = (core::mem::replace(&mut env.jit_body, "flush"), super::jit::tag(b, "flush", format_args!("loop"), ""));
    flush_path_in(b, env, m, x, lo);
    env.jit_body = body;
    b.restore_tag(t);
}

/// D1071 (V4R-n): the input's end under a step that holds rows back (`StepMech::flushes`), entered before `done`: the
/// cancel check, the end's fresh carry, each such step's flush through the steps after it into the end, the end's window
/// close, then `done`. Its stop and count-reached exits keep the source's position (no cursor is written back: a resumed
/// call reaches the end again and the step's own words say what it still holds); its rows count as a window's do.
/// D2580: `l(from)` is entered by the input's end (`from` -1) and by a count reached at step `from` once written back (a
/// resumed call meets it again there): only the holding steps after `from` flush (a step before it feeds the step met).
fn flush_path_in(b: &mut FunctionBuilder<'_>, env: &mut Env, m: &Mechs<'_>, x: &Exits, (l, open_hoists): (Block, usize)) {
    b.switch_to(l);
    let from = b.block_params(l)[0];
    let last = m.steps.iter().rposition(|s| s.flushes());
    // the window's hoists sit in its body, which does not dominate the input's end
    let window_hoists = env.hoisted.drain(open_hoists..).collect_in(env.mcx);
    cancel::check(b, env.ctx, x.l_cancel);
    env.memo.clear();
    if gravity_driver::mem::reset_unit() == gravity_program::Until::Row {
        env.hir.reset(b);
    }
    let fx = Exits { l_stop_in: b.create_block(&[Type::I64, Type::I64, Type::I64]), l_full: b.create_block(&[Type::I64, Type::I64]), ..*x };
    let (win, raise_row, outer) = (env.win.take(), env.l_raise_row.take(), core::mem::replace(&mut env.x, fx));
    let mut c = Carry::new(env.zero, env.mcx);
    c.accs = m.end.carried(b, env);
    let types: mcx::PgVec<'_, Type> = c.accs.iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
    env.carry_exits = (!types.is_empty()).then(|| LoopRoad::Carry.need::<CarryImpl>(&env.node).0.exits(b, &types[..], env.mcx));
    let end = &m.end;
    for (j, s) in m.steps.iter().enumerate().filter(|(_, s)| s.flushes()) {
        // the last holding step always flushes (a count reached enters only before it); an earlier one only after `from`
        let skip = (Some(j) != last).then(|| {
            let types = c.types(&|v| b.value_type(v));
            let jv = b.iconst(Type::I64, j as i64);
            let after = b.icmp(IntCond::Slt, from, jv);
            let (l_go, l_join) = (b.create_block(&[]), b.create_block(&types));
            b.cond_br(after, (l_go, &[]), (l_join, &c.params()));
            b.switch_to(l_go);
            l_join
        });
        let rest = &m.steps[j + 1..];
        env.step = Some(j);
        c = s.flush(b, env, c, &mut |b, env, r| {
            let c = chain(b, env, rest, j + 1, r, Walk { skip: 0, fill: None, late: None }, &mut |b, env, r| end.lower(b, env, r, &fx));
            env.step = Some(j);
            c
        });
        env.step = None;
        if let Some(l_join) = skip {
            b.br(l_join, &c.params());
            b.switch_to(l_join);
            c = Carry::from_params(b.block_params(l_join), env.mcx);
        }
    }
    if let Some(e) = env.carry_exits.take() {
        // a reached exit is sealed in its own block; the end's window close follows on the path the flush left by
        let here = b.current();
        LoopRoad::Carry.need::<CarryImpl>(&env.node).0.seal(b, env, &*m.end, e, &fx);
        b.switch_to(here);
    }
    m.end.window_end(b, env, &c);
    flushed_rows(b, env, m, c.out);
    b.br(x.l_done, &[]);
    if reached(b, fx.l_stop_in) {
        b.switch_to(fx.l_stop_in);
        let p = mcx::to_vec_in(b.block_params(fx.l_stop_in), env.mcx);
        flushed_rows(b, env, m, p[0]);
        b.br(x.l_status, &[p[2]]);
    }
    if reached(b, fx.l_full) {
        b.switch_to(fx.l_full);
        let out = b.block_params(fx.l_full)[0];
        flushed_rows(b, env, m, out);
        b.br(x.l_done, &[]);
    }
    (env.win, env.l_raise_row, env.x) = (win, raise_row, outer);
    env.hoisted.extend_in(window_hoists);
}

/// `out` rows a flush handed on, counted as a window's are (stop.rs `define_exits`: EXPLAIN's rows, a site's rows out),
/// its cursor left as it is.
fn flushed_rows(b: &mut FunctionBuilder<'_>, env: &Env, m: &Mechs<'_>, out: Value) {
    instr::count_out(b, env.ctx, env.node, out);
    if let Some(k) = m.site {
        let at = gravity_abi::ctx::site_off(k) as i32 + gravity_abi::site::site::ROWS_OUT as i32;
        let n = b.load(Type::I64, env.ctx, at);
        let n = b.int_bin(IntBinOp::Add, n, out);
        b.store(env.ctx, at, n);
    }
}

/// `copy_entry_in` under the JIT map's body `copy` (frame/jit.rs).
fn copy_entry(b: &mut FunctionBuilder<'_>, env: &mut Env, m: &Mechs<'_>, x: &Exits) {
    let (body, t) = (core::mem::replace(&mut env.jit_body, "copy"), super::jit::tag(b, "copy", format_args!("loop"), ""));
    copy_entry_in(b, env, m, x);
    env.jit_body = body;
    b.restore_tag(t);
}

/// D732-rest (V4R-290): under a recheck's stored COPY row (`copy_row.rs`) the call is that one row, entering the chain at
/// the SubqueryScan's first op (its quals and projection run again, as ExecScan's EvalPlanQual path runs them); the source
/// and the ops before it never run. Then the call is done. D1685: the empty copy (a null-extended row's) is a window of
/// no row.
fn copy_entry_in(b: &mut FunctionBuilder<'_>, env: &mut Env, m: &Mechs<'_>, x: &Exits) {
    let Some(e) = m.source.copy_entry() else { return };
    let (l_copy, l_src) = (b.create_block(&[]), b.create_block(&[]));
    let on = crate::rerun::epq::copy_row::copying(b, env, e.site.get());
    b.cond_br(on, (l_copy, &[]), (l_src, &[]));
    b.switch_to(l_copy);
    env.memo.clear();
    let accs = m.end.carried(b, env);
    let (l_row, l_none) = (b.create_block(&[]), b.create_block(&[]));
    let none = crate::rerun::epq::copy_row::none(b, env, e.site.get());
    b.cond_br(none, (l_none, &[]), (l_row, &[]));
    b.switch_to(l_none);
    m.end.window_end(b, env, &Carry { out: env.zero, accs: mcx::to_vec_in(&accs[..], env.mcx) });
    match m.site {
        Some(k) => stop::write_back(b, env.ctx, k, env.zero, env.zero),
        None => m.source.write_back(b, env, env.zero, env.zero),
    }
    b.br(x.l_done, &[]);
    b.switch_to(l_row);
    env.memo.clear();
    let row = crate::rerun::epq::copy_row::row(b, env, e, accs);
    let end = &m.end;
    // no node counts the stored row: the recheck's nodes are PostgreSQL's EvalPlanQual copies, not the plan's
    let c = instr::unbounded(|| chain(b, env, &m.steps, 0, &row, Walk { skip: (1u64 << e.boundary) - 1, fill: None, late: None }, &mut |b, env, r| end.lower(b, env, r, x)));
    // the window's end and write-back as a window's (`wend`, stop.rs `define_exits`): the row counted and handed on,
    // the never-opened source's cursor at 0
    end.window_end(b, env, &c);
    instr::count_out(b, env.ctx, env.node, c.out);
    match m.site {
        Some(k) => stop::write_back(b, env.ctx, k, c.out, env.zero),
        None => m.source.write_back(b, env, env.zero, c.out),
    }
    b.br(x.l_done, &[]);
    b.switch_to(l_src);
}

/// Road `hoist` (H35): the steps' entries of scope `at` computed where the frame is (the call's entry, a window's start),
/// when the road applies; a step reads them through `Env::hoisted`.
fn hoisted(b: &mut FunctionBuilder<'_>, env: &mut Env, at: HoistAt, h: &Hoist) {
    if h.words.iter().any(|w| w.0 == at) && env.applies(LOOP_HOIST) {
        LoopRoad::Hoist.need::<HoistImpl>(&env.node).0.emit(b, env, at, h);
    }
}

/// The window's fast-body guard (H06): the applying body roads' guards over the window's facts, ANDed, with the facts the
/// fast body runs under; `None` when no road gives one.
fn body_guard(b: &mut FunctionBuilder<'_>, env: &mut Env, facts: &WinConds) -> Option<(Value, Body)> {
    let mut out: Option<(Value, Body)> = None;
    for (form, r) in [(BodyForm::Versioned, BODY_VERSIONED), (BodyForm::Form, BODY_FORM)] {
        if !env.applies(r) {
            continue;
        }
        let Some((g, f)) = form.need::<BodyImpl>(&env.node).0.guard(b, env, facts) else { continue };
        out = Some(match out {
            None => (g, f),
            Some((g0, f0)) => {
                let both = b.int_bin(IntBinOp::And, g0, g);
                let u = Body { nonulls: f0.nonulls | f.nonulls, nosel: f0.nosel | f.nosel, dense: f0.dense | f.dense, codes: f0.codes | f.codes, page_full: f0.page_full | f.page_full, packed: false, clean: f0.clean | f.clean };
                (both, u)
            }
        });
    }
    out
}

/// `row_loop_in` under the JIT map's body `row` (frame/jit.rs).
fn row_loop(b: &mut FunctionBuilder<'_>, env: &mut Env, m: &Mechs<'_>, win: &Win, x: &Exits) {
    let (body, t) = (core::mem::replace(&mut env.jit_body, "row"), super::jit::tag(b, "row", format_args!("loop"), ""));
    row_loop_in(b, env, m, win, x);
    env.jit_body = body;
    b.restore_tag(t);
}

/// The row loop over `[cursor, end)` (H06). A window the source's verdicts cover (`WinConds::covered`, when `covers()` names
/// steps) runs only the covered bodies: the straight rows with the covered steps elided, since their lanes may be unread
/// (verdict.elide), under the window's fast facts while its guard holds (D3193), else the general one. Every other window
/// runs at most two bodies of the chain:
/// - the fast body while the window's guard holds, with `env.body` set to the facts it runs under: the 64-row block path
///   when the first step has a block form (`block_open`'s guard ANDed in), else the straight rows;
/// - the general body, the straight rows with every fact tested per row (the tail, a failed guard).
/// The end may take the window whole first (`EndMech::whole`). Row loops are bounded by the window, so their back edges
/// carry no check (§3.6). The rows counted leave at every write-back (stop.rs).
fn row_loop_in(b: &mut FunctionBuilder<'_>, env: &mut Env, m: &Mechs<'_>, win: &Win, x: &Exits) {
    let mut carry0 = Carry::new(env.zero, env.mcx);
    carry0.accs = m.end.carried(b, env);
    let types: mcx::PgVec<'_, Type> = core::iter::once(Type::I64).chain(carry0.params().iter().map(|&v| b.value_type(v))).collect_in(env.mcx);
    let l_row = b.create_block(&types);
    // a block path's tail enters the general body here: its late lanes filled for the tail rows (decode.late)
    let mut l_tail: Option<Block> = None;
    let l_wend = b.create_block(&types[1..]);
    env.carry_exits = match carry0.accs.is_empty() {
        true => None,
        false => Some(LoopRoad::Carry.need::<CarryImpl>(&env.node).0.exits(b, &types[2..], env.mcx)),
    };
    // EXPLAIN ANALYZE: a window the end takes whole (a count(*)'s, a covered or answered window) passed every node: each
    // of the frame's nodes counts the rows the end took
    let l_take = if instr::bounded() { b.create_block(&types[1..]) } else { l_wend };
    m.end.whole(b, env, win, &carry0, l_take);
    if l_take != l_wend && reached(b, l_take) {
        let here = b.current();
        b.switch_to(l_take);
        let p = mcx::to_vec_in(b.block_params(l_take), env.mcx);
        instr::row_nodes(env.mcx).into_iter().for_each(|x| instr::count_bound(b, env.ctx, x, p[0]));
        b.br(l_wend, &p);
        b.switch_to(here);
    }
    // D528: an end that takes every window whole (a count-only fold) leaves the builder in a block nothing enters: no
    // row loop is emitted under it (its bodies, their carry exits and the exits only they reach stay unemitted)
    if !reached(b, b.current()) {
        if let Some(e) = env.carry_exits.take() { LoopRoad::Carry.need::<CarryImpl>(&env.node).0.seal(b, env, &*m.end, e, x) }
        return wend(b, env, m, win, x, l_wend);
    }
    let mut start = mcx::pg_vec![in env.mcx; win.cursor];
    start.extend_in(carry0.params());
    let covers = m.source.covers();
    let fill = (covers != 0 && m.source.fills()).then(|| (64 - covers.leading_zeros() as usize, &*m.source as &dyn SourceMech, win));
    let general = Walk { skip: 0, fill, late: None };
    if let Some(cv) = win.facts.covered.filter(|_| covers != 0) {
        // a covered window runs only the bodies that elide its covered steps (verdict.elide leaves their lanes unread). D3193:
        // the fast one under the window's guard as any other window (with the general body only, an elided filter made a
        // window slower than a Mixed one), escaping into the general one
        let (l_cov, l_rest, l_cgen) = (b.create_block(&types), b.create_block(&[]), b.create_block(&types));
        let c = all(b, env, &[cv]).unwrap_or(cv);
        b.cond_br(c, (l_cov, &start), (l_rest, &[]));
        b.switch_to(l_cov);
        let (p, skip) = (mcx::to_vec_in(b.block_params(l_cov), env.mcx), Walk { skip: covers, fill: None, late: None });
        match body_guard(b, env, &win.facts) {
            Some((g, fast)) => {
                let l_cfast = b.create_block(&types);
                b.cond_br(g, (l_cfast, &p), (l_cgen, &p));
                (env.body, env.escape) = (fast, Some(l_cgen));
                straight(b, env, m, win, x, skip, l_cfast, l_wend);
                (env.body, env.escape) = (Body::default(), None);
            }
            None => b.br(l_cgen, &p),
        }
        straight(b, env, m, win, x, skip, l_cgen, l_wend);
        b.switch_to(l_rest);
    }
    let guard = body_guard(b, env, &win.facts);
    // an instrumented frame takes the block path when no node's rows leave inside its masked steps (`instr::clear_before`)
    let ww = m.steps.first().and_then(|_| m.source.word_win(b, env, win));
    let blk = ww.as_ref().and_then(|w| block_span(b, env, m, w, general.fill).filter(|s| instr::clear_before(s.1)).map(|(ok, span)| (w, ok, span)));
    // the packed body (body.form, H06 b): a window whose filter lanes are ByteFor at width 1 / 2 runs the block path with
    // the span's masks on the raw deltas where a qual allows (mask_vec.rs); its tail and every other window below
    let packed = win.facts.packed.filter(|_| env.applies(BODY_FORM) && env.applies(MASK_VEC));
    if let (Some(pk), Some((w, ok, span))) = (packed, blk) {
        let c = all(b, env, &[pk, ok.0]).unwrap_or(pk);
        let (l_pk, l_else) = (b.create_block(&types), b.create_block(&[]));
        b.cond_br(c, (l_pk, &start), (l_else, &[]));
        env.body = Body { packed: true, ..Body::default() };
        let lt = *l_tail.get_or_insert_with(|| b.create_block(&types));
        block_loop(b, env, m, win, x, (w, span), general, (l_pk, lt, &types[..]));
        b.switch_to(l_else);
    }
    let fast = guard.map_or(Body::default(), |g| g.1);
    let conds: mcx::PgVec<'_, Value> = [blk.map(|(_, ok, _)| ok.0), guard.map(|g| g.0)].into_iter().flatten().collect_in(env.mcx);
    match (blk, all(b, env, &conds[..])) {
        (Some((w, _, span)), Some(c)) => {
            let l_blk = b.create_block(&types);
            b.cond_br(c, (l_blk, &start), (l_row, &start));
            env.body = fast;
            let lt = *l_tail.get_or_insert_with(|| b.create_block(&types));
            block_loop(b, env, m, win, x, (w, span), general, (l_blk, lt, &types[..]));
        }
        (None, Some(c)) => {
            let l_fast = b.create_block(&types);
            // road mask.rows (mask_rows.rs): over a source with no word window, a window its counters pick runs the first
            // filter's row-made mask under the same facts, else the straight rows
            match m.steps.first().and_then(|s| s.row_stats()).filter(|_| ww.is_none() && general.fill.is_none() && general.skip == 0) {
                Some(stats) => {
                    let (l_pick, l_rm) = (b.create_block(&[]), b.create_block(&types));
                    b.cond_br(c, (l_pick, &[]), (l_row, &start));
                    b.switch_to(l_pick);
                    let take = crate::frame::mask_rows::pick(b, env, stats);
                    b.cond_br(take, (l_rm, &start), (l_fast, &start));
                    (env.body, env.escape) = (fast, Some(l_row));
                    let late = m.source.late_at();
                    let late = (late > 0 && late <= m.steps.len()).then_some((late, &*m.source as &dyn SourceMech, win));
                    crate::frame::mask_rows::body(b, env, m, win, stats, (l_rm, l_wend, l_row), &mut |b, env, i, c, skip, kept| {
                        env.masked = true;
                        let out = match kept {
                            Some(row) => one_row_made(b, env, m, x, Walk { skip, late, ..general }, row),
                            None => one_row(b, env, m, win, x, Walk { skip, ..general }, i, c),
                        };
                        env.masked = false;
                        out
                    });
                }
                None => b.cond_br(c, (l_fast, &start), (l_row, &start)),
            }
            (env.body, env.escape) = (fast, Some(l_row));
            straight(b, env, m, win, x, general, l_fast, l_wend);
            env.escape = None;
        }
        (_, None) => b.br(l_row, &start),
    }
    env.body = Body::default();
    let l_gen = straight(b, env, m, win, x, general, l_row, l_wend);
    if let Some(lt) = l_tail {
        tail_entry(b, env, m, win, lt, l_gen);
    }
    if let Some(e) = env.carry_exits.take() { LoopRoad::Carry.need::<CarryImpl>(&env.node).0.seal(b, env, &*m.end, e, x) }
    wend(b, env, m, win, x, l_wend)
}

/// The window's end `l_wend(out, accs…)`: the end's `window_end`, then `l_finish(out, end)`.
fn wend(b: &mut FunctionBuilder<'_>, env: &mut Env, m: &Mechs<'_>, win: &Win, x: &Exits, l_wend: Block) {
    b.switch_to(l_wend);
    let c = Carry::from_params(b.block_params(l_wend), env.mcx);
    m.end.window_end(b, env, &c);
    b.br(x.l_finish, &[c.out, win.end]);
}

/// D528: whether any emitted edge enters `l` (the function's entry always is). A body or exit nothing enters is not
/// emitted: its block stays an empty `unreachable` the program's `prune` drops.
pub(crate) fn reached(b: &FunctionBuilder<'_>, l: Block) -> bool {
    let f = b.func();
    f.entry == l || f.blocks.iter().any(|bb| bb.term.edges().any(|e| e.target == l))
}

/// The AND of condition words as one `Bool` (a word holds when nonzero); `None` for none.
pub(crate) fn all(b: &mut FunctionBuilder<'_>, env: &Env, conds: &[Value]) -> Option<Value> {
    let mut out: Option<Value> = None;
    for &v in conds {
        let c = if b.value_type(v) == Type::Bool { v } else { b.icmp(IntCond::Ne, v, env.zero) };
        out = Some(match out {
            None => c,
            Some(o) => b.int_bin(IntBinOp::And, o, c),
        });
    }
    out
}

/// The straight rows `l(j, out, accs…)` from `l_in` to the window's end, then `l_wend(out, accs…)`: one body of the chain.
/// Its entry fills the window's late lanes once (`SourceMech::fill_late`, decode.late); returns the loop header.
fn straight(b: &mut FunctionBuilder<'_>, env: &mut Env, m: &Mechs<'_>, win: &Win, x: &Exits, w: Walk<'_>, l_in: Block, l_wend: Block) -> Block {
    b.switch_to(l_in);
    let p = mcx::to_vec_in(b.block_params(l_in), env.mcx);
    m.source.fill_late(b, env, win, None);
    let types: mcx::PgVec<'_, Type> = p.iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
    let l = b.create_block(&types);
    // D3168: in the staged body, a chain whose rows all reach that probe (the steps before it elided, or plain
    // projections: `passes`) runs whole chunks of rows through its stages first; the rows past the last whole chunk walk
    // one at a time from `l`
    let pass = env.batch.filter(|_| w.fill.is_none() && (env.body.nosel || win.facts.nosel.is_some())).and_then(|jp| passes(m, w.skip, jp, env.mcx).map(|s| (jp, s)));
    let head = match pass {
        Some((jp, s)) => {
            let at = b.current();
            let lc = chunks(b, env, m, win, x, w, (jp, &s[..], &types[..]), l);
            b.switch_to(at);
            lc
        }
        None => l,
    };
    b.br(head, &p);
    b.mark_loop_header(l);
    env.bounded.push_in(l);
    b.switch_to(l);
    let p = mcx::to_vec_in(b.block_params(l), env.mcx);
    let (j, carry) = (p[0], Carry::from_params(&p[1..], env.mcx));
    let more = b.icmp(IntCond::Slt, j, win.end);
    let l_body = b.create_block(&[]);
    b.cond_br(more, (l_body, &[]), (l_wend, &carry.params()));
    b.switch_to(l_body);
    let out = one_row(b, env, m, win, x, w, j, carry);
    // `j + 1` at the latch, from the header's `j`: a sum made at the row's top is a second loop-carried value across
    // the body, a stack slot in the baseline backend (a store per row and a reload here)
    let next = b.int_bin(IntBinOp::Add, j, env.one);
    let mut back = mcx::pg_vec![in env.mcx; next];
    back.extend_in(out.params());
    b.br(l, &back);
    head
}

/// D3168: the steps before probe `jp` a row walks (not in `skip`), when each hands every row on with its columns bare (a
/// plain projection, `StepMech::forwards`); `None`: another step comes first.
fn passes<'e>(m: &Mechs<'_>, skip: u64, jp: usize, mcx: mcx::Mcx<'e>) -> Option<mcx::PgVec<'e, usize>> {
    let live: mcx::PgVec<'_, usize> = (0..jp).filter(|&i| i >= 64 || skip >> i & 1 == 0).collect_in(mcx);
    live.iter().all(|&i| m.steps[i].forwards(1).is_some()).then_some(live)
}

/// D3168: the source row's column that column `a` of the probe's row passes on from, through the steps `pass`.
fn back(m: &Mechs<'_>, pass: &[usize], a: i16) -> Option<i16> {
    pass.iter().rev().try_fold(a, |a, &i| m.steps[i].forwards(a).flatten())
}

/// D3168: a staged block's row sets, `nw` mask words in a stack array, each set to `bits` (`None`: every row, the last
/// word's `rows % 64` low bits when it is short).
fn mask_words(b: &mut FunctionBuilder<'_>, rows: usize, bits: Option<Value>) -> Value {
    let nw = rows.div_ceil(64);
    let s = b.declare_stack_slot(8 * nw as u32, 8);
    let at = b.stack_addr(s);
    for w in 0..nw {
        let v = match bits {
            Some(v) => v,
            None => b.iconst(Type::I64, if rows - 64 * w >= 64 { -1 } else { (1i64 << (rows - 64 * w)) - 1 }),
        };
        b.store(at, 8 * w as i32, v);
    }
    at
}

/// D3168: the straight rows' whole chunks `l_chunk(j, carry…)` of `probe_stage::rows()` rows: probe `jp` (every row of a
/// chunk reaches it: the steps `pass` before it hand each row on) runs its stages over the chunk's rows
/// (`StepMech::block_batch`, every late lane already filled), then the rows its stages keep walk in order, each reading
/// its first record from the stages (`Env::batch_row`). Fewer rows left than a chunk, a selection, or stages declined: the
/// rows go on one at a time from `l_rows`. Returns the chunk loop's header.
#[allow(clippy::too_many_arguments)]
fn chunks(b: &mut FunctionBuilder<'_>, env: &mut Env, m: &Mechs<'_>, win: &Win, x: &Exits, w: Walk<'_>, (jp, pass, types): (usize, &[usize], &[Type]), l_rows: Block) -> Block {
    let rows = crate::ops::hashjoin::strategies::probe_stage::rows();
    let nw = rows.div_ceil(64);
    let l_chunk = b.create_block(types);
    b.mark_loop_header(l_chunk);
    env.bounded.push_in(l_chunk);
    b.switch_to(l_chunk);
    let q = mcx::to_vec_in(b.block_params(l_chunk), env.mcx);
    let j = q[0];
    let n = b.iconst(Type::I64, rows as i64);
    let jn = b.int_bin(IntBinOp::Add, j, n);
    let whole = b.icmp(IntCond::Sle, jn, win.end);
    // a selection's rows are no run of their lanes' elements: such a window's rows go on one at a time
    let whole = match (env.body.nosel, win.facts.nosel) {
        (false, Some(plain)) => {
            let plain = all(b, env, &[plain]).unwrap_or(plain);
            b.int_bin(IntBinOp::And, whole, plain)
        }
        _ => whole,
    };
    let l_blk = b.create_block(&[]);
    b.cond_br(whole, (l_blk, &[]), (l_rows, &q));
    b.switch_to(l_blk);
    let next = b.int_bin(IntBinOp::Add, j, env.one);
    let row = m.source.row(b, env, win, j, Carry::from_params(&q[1..], env.mcx), next);
    let masks = mask_words(b, rows, None);
    let Some(bt) = m.steps[jp].block_batch(b, env, (&row, &|a| back(m, pass, a)), BatchRows { masks, nw, all: false }, &mut |_, _, _| true) else {
        b.br(l_rows, &q);
        return l_chunk;
    };
    fires::fire("join.probe.staged.chunk"); // shapecheck fire signal (D3168): a straight body's chunks run the stages
    // the chunk's rows by mask word (`l_word(w, carry…)`), each word's rows in order (`l_bits(bits, w, carry…)`)
    let l_word = b.create_block(types);
    let mut wt = mcx::pg_vec![in env.mcx; Type::I64, Type::I64];
    wt.extend_from_slice_in(&types[1..]);
    let l_bits = b.create_block(&wt[..]);
    let mut first = mcx::pg_vec![in env.mcx; env.zero];
    first.extend_from_slice_in(&q[1..]);
    b.br(l_word, &first);
    b.mark_loop_header(l_word);
    env.bounded.push_in(l_word);
    b.switch_to(l_word);
    let r = mcx::to_vec_in(b.block_params(l_word), env.mcx);
    let (wi, carry) = (r[0], Carry::from_params(&r[1..], env.mcx));
    let nwv = b.iconst(Type::I64, nw as i64);
    let done = b.icmp(IntCond::Eq, wi, nwv);
    let l_load = b.create_block(&[]);
    let mut on = mcx::pg_vec![in env.mcx; jn];
    on.extend_in(carry.params());
    b.cond_br(done, (l_chunk, &on), (l_load, &[]));
    b.switch_to(l_load);
    let at = b.ptr_add(bt.walk, wi, 8, 0);
    let bits = b.load(Type::I64, at, 0);
    let mut go = mcx::pg_vec![in env.mcx; bits, wi];
    go.extend_in(carry.params());
    b.br(l_bits, &go);
    b.mark_loop_header(l_bits);
    env.bounded.push_in(l_bits);
    b.switch_to(l_bits);
    let r = mcx::to_vec_in(b.block_params(l_bits), env.mcx);
    let (bits, wi, carry) = (r[0], r[1], Carry::from_params(&r[2..], env.mcx));
    let empty = b.icmp(IntCond::Eq, bits, env.zero);
    let l_bit = b.create_block(&[]);
    let w1 = b.int_bin(IntBinOp::Add, wi, env.one);
    let mut on = mcx::pg_vec![in env.mcx; w1];
    on.extend_in(carry.params());
    b.cond_br(empty, (l_word, &on), (l_bit, &[]));
    b.switch_to(l_bit);
    let t = b.int_un(IntUnOp::Ctz, bits);
    let six = b.iconst(Type::I64, 6);
    let base = b.int_bin(IntBinOp::Shl, wi, six);
    let t = b.int_bin(IntBinOp::Add, base, t);
    let i = b.int_bin(IntBinOp::Add, j, t);
    let low = b.int_bin(IntBinOp::Sub, bits, env.one);
    let rest = b.int_bin(IntBinOp::And, bits, low);
    env.batch_row = Some(BatchRow { offset: bt.offset, firsts: bt.firsts, t });
    let out = one_row(b, env, m, win, x, w, i, carry);
    env.batch_row = None;
    let mut back = mcx::pg_vec![in env.mcx; rest, wi];
    back.extend_in(out.params());
    b.br(l_bits, &back);
    l_chunk
}

/// `l_tail(j, carry)`: a block path's tail (fewer than 64 rows left) into the general body's header `l_gen`, the tail
/// rows' late lanes filled first (every tail row a candidate; none when the window ends on a block).
fn tail_entry(b: &mut FunctionBuilder<'_>, env: &mut Env, m: &Mechs<'_>, win: &Win, l_tail: Block, l_gen: Block) {
    b.switch_to(l_tail);
    let p = mcx::to_vec_in(b.block_params(l_tail), env.mcx);
    let n = b.int_bin(IntBinOp::Sub, win.end, p[0]);
    let bit = b.int_bin(IntBinOp::Shl, env.one, n);
    let tail = b.int_bin(IntBinOp::Sub, bit, env.one);
    m.source.fill_late(b, env, win, Some((p[0], Mask(tail))));
    b.br(l_gen, &p);
}

/// Row `j` through the steps into the end (one body of the chain): the carry after. The row's `next` (`j + 1`) is read
/// only by its stops' edges; the straight rows' latch makes its own.
fn one_row<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, m: &Mechs<'_>, win: &Win, x: &Exits, w: Walk<'_>, j: Value, carry: Carry<'e>) -> Carry<'e> {
    env.memo.clear();
    if gravity_driver::mem::reset_unit() == gravity_program::Until::Row {
        env.hir.reset(b);
    }
    // D3990: an end that copies its row, reached once per row (no step expands a row or holds rows back), gives a row of
    // big hosted results back after it: the lane's big-result mark cleared before anything of the row is computed
    let mark = match m.end.copies_row() && m.steps.iter().all(|s| s.expand().is_none() && !s.flushes()) {
        true => env.hir.end_mark(b),
        false => None,
    };
    let next = b.int_bin(IntBinOp::Add, j, env.one);
    // decode.late (straight rows): the cells past the leading filters' reads are made once a row passed them
    let late = m.source.late_at();
    let w = match late > 0 && late <= m.steps.len() && w.skip == 0 && w.fill.is_none() && !env.masked {
        true => Walk { late: Some((late, &*m.source as &dyn SourceMech, win)), ..w },
        false => Walk { late: None, ..w },
    };
    let row = match w.late {
        Some(_) => m.source.row_early(b, env, win, j, carry, next),
        None => m.source.row(b, env, win, j, carry, next),
    };
    row_chain(b, env, m, x, w, &row, mark)
}

/// `one_row` over a row its caller made (road mask.rows: a passing row's cells kept from the mask's pass; `w.late` makes
/// its late cells).
fn one_row_made<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, m: &Mechs<'_>, x: &Exits, w: Walk<'_>, row: &RowVals<'e>) -> Carry<'e> {
    env.memo.clear();
    if gravity_driver::mem::reset_unit() == gravity_program::Until::Row {
        env.hir.reset(b);
    }
    let mark = match m.end.copies_row() && m.steps.iter().all(|s| s.expand().is_none() && !s.flushes()) {
        true => env.hir.end_mark(b),
        false => None,
    };
    row_chain(b, env, m, x, w, row, mark)
}

/// Row `row` through the chain into the end, its big-result mark `mark` (`one_row`) given back after the end.
fn row_chain<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, m: &Mechs<'_>, x: &Exits, w: Walk<'_>, row: &RowVals<'e>, mut mark: Option<crate::hir::RowMark>) -> Carry<'e> {
    let loc = m.end.sink() == SinkKind::RowOp || m.end.wants_locator() || m.steps.iter().any(|s| s.wants_locator());
    env.loc = if loc { m.source.locator(b, env, row) } else { None };
    let end = &m.end;
    chain(b, env, &m.steps, 0, row, w, &mut |b, env, r| {
        let c = end.lower(b, env, r, x);
        if let Some(m) = mark.as_mut() {
            env.hir.give_back_big(b, m);
        }
        c
    })
}

/// The packed body's views of block `[j, j + 64)`: each by-value lane of the window that the source serves as raw deltas.
fn packed_views<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, m: &Mechs<'_>, win: &Win, w: &WordWin, j: Value) -> mcx::PgVec<'e, (i16, Packed)> {
    let byval = |a: usize| w.lanes[a].is_some_and(|l| l.kind == gravity_abi::ctx::lane::KIND_BYVAL && l.width >= 2);
    let mcx = env.mcx;
    let attnos: mcx::PgVec<'_, i16> = (0..w.lanes.len()).filter(|&a| byval(a)).map(|a| a as i16 + 1).collect_in(mcx);
    attnos.into_iter().filter_map(|a| m.source.packed(b, env, win, a, j).map(|p| (a, p))).collect_in(mcx)
}

/// The block path's guard and span (H01): step 0's block form, then each next step's while one opens (a filter whose quals
/// all have raise-free word or vector forms: step/filter.rs `block_open`), their tests in the block's mask, not per row (a
/// filter's `lower` is its test alone; the counters are per node). Under a condition-cache fill the span ends with the
/// covered steps (`passed` records the rows that reach the step after them).
fn block_span(b: &mut FunctionBuilder<'_>, env: &mut Env, m: &Mechs<'_>, w: &WordWin, fill: Option<(usize, &dyn SourceMech, &Win)>) -> Option<(Mask, usize)> {
    let (mut ok, mut span) = (m.steps[0].block_open(b, env, w)?.0, 1);
    let cap = fill.map_or(m.steps.len(), |f| f.0).min(m.steps.len()).min(63);
    while span < cap {
        let Some(g) = m.steps[span].block_open(b, env, w) else { break };
        ok = all(b, env, &[ok, g.0]).unwrap_or(ok);
        span += 1;
    }
    Some((Mask(ok), span))
}

/// The span's mask of block `[j, j + 64)`, each step's within the last's (`Env::span` names its place), step 0's own, and
/// whether the late lanes are filled: for the rows so far, before a spanned step reading one (`StepMech::block_reads`).
fn first_mask(b: &mut FunctionBuilder<'_>, env: &mut Env, m: &Mechs<'_>, win: &Win, (w, span): (&WordWin, usize), j: Value, all: Value) -> (Value, Value, bool) {
    let early = match m.source.decode_order() { DecodeOrder::Late { first } => first, DecodeOrder::Eager => u64::MAX };
    let (mut mask, mut first, mut filled) = (Mask(all), all, false);
    for (i, s) in m.steps[..span].iter().enumerate() {
        (env.step, env.span) = (Some(i), (i, span));
        if i > 0 && !filled && s.block_reads(env, w) & !early != 0 {
            m.source.fill_late(b, env, win, Some((j, mask)));
            filled = true;
        }
        mask = s.block(b, env, w, j, mask);
        first = if i == 0 { mask.0 } else { first };
    }
    (env.step, env.span) = (None, (0, 1));
    (first, mask.0, filled)
}

/// The count form (H01 c, mask.fold): a counting end (`EndMech::counts`) over a first step that counts the block `[j, j +
/// 64)` through the span without a mask (`StepMech::block_count`), later steps keeping their rows and no condition-cache
/// fill (`unfilled`), folds a mask of that many low bits, no late lane filled (it reads none). Every row of the block is
/// live as for `first_mask`: the all-ones `all`, the whole test `j + 64 <= win.end`, the guard's `w.plain` (no selection).
fn counted<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, m: &Mechs<'_>, (w, span): (&WordWin, usize), j: Value, unfilled: bool, c: Carry<'e>) -> Option<Carry<'e>> {
    if !unfilled || !m.end.counts(env) || !m.steps[span..].iter().all(|s| s.keeps_rows()) {
        return None;
    }
    (env.step, env.span) = (Some(0), (0, span));
    let n = m.steps[0].block_count(b, env, w, j);
    (env.step, env.span) = (None, (0, 1));
    let (n, sixty_four, ones) = (n?, b.iconst(Type::I64, 64), b.iconst(Type::I64, -1));
    instr::count_all(b, env.ctx, n, env.mcx);
    let (bit, full) = (b.int_bin(IntBinOp::Shl, env.one, n), b.icmp(IntCond::Eq, n, sixty_four));
    let low = b.int_bin(IntBinOp::Sub, bit, env.one);
    let mask = b.select(full, ones, low);
    let Some(c2) = m.end.block(b, env, w, j, Mask(mask), &c) else {
        crate::refuse!(env.node, crate::admit::refusal::Reason::Bug { payload: "a counting end that did not fold its block" })
    };
    Some(c2)
}

/// `l_blk(j, carry)`: while a whole block `[j, j + 64)` is in the window, its mask's rows (`l_bits(mask, carry)`) through the
/// chain with the span's per-row tests skipped (`env.masked`, `Walk::skip`), or the block folded by the end (mask.fold).
#[allow(clippy::too_many_arguments)]
/// `block_loop_in` under the JIT map's body `block` (frame/jit.rs).
fn block_loop(b: &mut FunctionBuilder<'_>, env: &mut Env, m: &Mechs<'_>, win: &Win, x: &Exits, ws: (&WordWin, usize), walk: Walk<'_>, ls: (Block, Block, &[Type])) {
    let (body, t) = (core::mem::replace(&mut env.jit_body, "block"), super::jit::tag(b, "block", format_args!("loop"), ""));
    block_loop_in(b, env, m, win, x, ws, walk, ls);
    env.jit_body = body;
    b.restore_tag(t);
}

fn block_loop_in(b: &mut FunctionBuilder<'_>, env: &mut Env, m: &Mechs<'_>, win: &Win, x: &Exits, (w, span): (&WordWin, usize), walk: Walk<'_>, (l_blk, l_row, types): (Block, Block, &[Type])) {
    b.mark_loop_header(l_blk);
    env.bounded.push_in(l_blk);
    b.switch_to(l_blk);
    let p = mcx::to_vec_in(b.block_params(l_blk), env.mcx);
    let j = p[0];
    let sixty_four = b.iconst(Type::I64, 64);
    let j64 = b.int_bin(IntBinOp::Add, j, sixty_four);
    let whole = b.icmp(IntCond::Sle, j64, win.end);
    let l_word = b.create_block(&[]);
    b.cond_br(whole, (l_word, &[]), (l_row, &p));
    b.switch_to(l_word);
    let all = b.iconst(Type::I64, -1);
    if env.body.packed {
        env.packed = packed_views(b, env, m, win, w, j);
    }
    if let Some(c2) = counted(b, env, m, (w, span), j, walk.fill.is_none(), Carry::from_params(&p[1..], env.mcx)) {
        env.packed.clear();
        let mut on = mcx::pg_vec![in env.mcx; j64];
        on.extend_in(c2.params());
        b.br(l_blk, &on);
        return;
    }
    let (mask0, mask, filled) = first_mask(b, env, m, win, (w, span), j, all);
    env.packed.clear();
    // D3194 (join.rowfilter): in the filtered body the probe right after the span keeps only the rows its build's filter
    // passes, before the late lanes decode: a row it drops decodes none and walks no step (none under a condition-cache fill,
    // which must see every row the span passes)
    let pre = match env.rowfilter == Some(span) && !filled && walk.fill.is_none() {
        true => rowfilter_block(b, env, m, win, (span, j, Mask(mask)), &p[1..]),
        false => None,
    };
    if let Some(kept) = pre {
        instr::count_dropped(b, env.ctx, span, mask, kept, env.mcx);
    }
    // D3168: in the staged body the probe the span's rows reach (right after it, or past plain projections: `passes`)
    // runs its stages over the block's rows first (its build filter's test among them, so the rows it drops walk no step
    // either), before the late lanes decode; under a condition-cache fill that must see each row the span passes (any but
    // step 0's whole mask, below), every row walks
    let row_fill = walk.fill.is_some_and(|f| f.0 != 1 || !f.1.fills_blocks());
    let pass = env.batch.filter(|&jp| jp >= span && pre.is_none()).and_then(|jp| passes(m, (1u64 << span) - 1, jp, env.mcx).map(|s| (jp, s)));
    let bt = pass.and_then(|(jp, s)| batch_block(b, env, m, win, (jp, &s[..], j, Mask(mask)), (row_fill, filled), &p[1..]));
    let mask = match bt {
        Some(x) => {
            fires::fire("join.probe.staged.block"); // shapecheck fire signal (D3168): the block path runs the stages
            b.load(Type::I64, x.walk, 0)
        }
        None => pre.unwrap_or(mask),
    };
    // decode.late: the block's survivors' late lanes before the fold or the walk reads them, unless a spanned step's read
    // filled them (the source fills nothing for an empty mask, as in tail_entry)
    if !filled {
        m.source.fill_late(b, env, win, Some((j, Mask(mask))));
    }
    // a fill whose covered step is the first alone takes step 0's mask whole (bit i: row j + i passed it), and the rows
    // then walk without a per-row fill; the span's later steps skip their per-row tests
    let mut walk = Walk { skip: walk.skip | ((1u64 << span) - 1) & !1, ..walk };
    if let Some((1, src, win)) = walk.fill.filter(|f| f.1.fills_blocks()) {
        src.passed_block(b, env, win, j, Mask(mask0));
        walk.fill = None;
    }
    // mask.fold: the end folds the whole block when no step past the span drops, adds or computes a row it reads; not
    // while the source's condition-cache fill must see each passing row
    if walk.fill.is_none() && m.steps[span..].iter().all(|s| s.keeps_rows()) {
        if let Some(c2) = m.end.block(b, env, w, j, Mask(mask), &Carry::from_params(&p[1..], env.mcx)) {
            instr::count_mask(b, env.ctx, mask, env.mcx);
            let mut on = mcx::pg_vec![in env.mcx; j64];
            on.extend_in(c2.params());
            b.br(l_blk, &on);
            return;
        }
        // KEYED-24: a block the end's host may decline at run time: the taken carry onto the next block, the declined
        // block's rows walked below from the edge the end leaves
        if let Some((c2, l_no)) = m.end.block_or_walk(b, env, w, j, Mask(mask), &Carry::from_params(&p[1..], env.mcx)) {
            instr::count_mask(b, env.ctx, mask, env.mcx);
            let mut on = mcx::pg_vec![in env.mcx; j64];
            on.extend_in(c2.params());
            b.br(l_blk, &on);
            b.switch_to(l_no);
        }
    }
    env.prefiltered = pre.is_some();
    if env.applies(MASK_RUN) {
        let run = BlockForm::Run.need::<MaskRunImpl>(&env.node).0;
        let carry = Carry::from_params(&p[1..], env.mcx);
        run.walk(b, env, j, Mask(mask), &carry, (l_blk, j64), &mut |b, env, i, c| {
            env.masked = true;
            env.batch_row = bt.map(|x| BatchRow { offset: x.offset, firsts: x.firsts, t: b.int_bin(IntBinOp::Sub, i, j) });
            let out = one_row(b, env, m, win, x, walk, i, c);
            (env.masked, env.batch_row) = (false, None);
            out
        });
        env.prefiltered = false;
        return;
    }
    let mut bits_types = mcx::to_vec_in(types, env.mcx);
    bits_types[0] = Type::I64;
    let l_bits = b.create_block(&bits_types);
    let mut first = mcx::pg_vec![in env.mcx; mask];
    first.extend_from_slice_in(&p[1..]);
    b.br(l_bits, &first);
    b.mark_loop_header(l_bits);
    env.bounded.push_in(l_bits);
    b.switch_to(l_bits);
    let q = mcx::to_vec_in(b.block_params(l_bits), env.mcx);
    let (bits, carry) = (q[0], Carry::from_params(&q[1..], env.mcx));
    let empty = b.icmp(IntCond::Eq, bits, env.zero);
    let l_bit = b.create_block(&[]);
    let mut on = mcx::pg_vec![in env.mcx; j64];
    on.extend_in(carry.params());
    b.cond_br(empty, (l_blk, &on), (l_bit, &[]));
    b.switch_to(l_bit);
    let t = b.int_un(IntUnOp::Ctz, bits);
    let i = b.int_bin(IntBinOp::Add, j, t);
    let low = b.int_bin(IntBinOp::Sub, bits, env.one);
    let rest = b.int_bin(IntBinOp::And, bits, low);
    env.masked = true;
    env.batch_row = bt.map(|x| BatchRow { offset: x.offset, firsts: x.firsts, t });
    let out = one_row(b, env, m, win, x, walk, i, carry);
    (env.masked, env.prefiltered, env.batch_row) = (false, false, None);
    let mut back = mcx::pg_vec![in env.mcx; rest];
    back.extend_in(out.params());
    b.br(l_bits, &back);
}

/// D3168: block `[j, j + 64)`'s `mask` through the staged probe `m.steps[r]`'s stages (`StepMech::block_batch`), read from
/// the source's row `j` through the steps `pass` between, its key columns' late lanes filled first for the mask's rows
/// (unless the span `filled` every late lane); `all`: every row of the mask walks after.
#[allow(clippy::type_complexity)]
fn batch_block(b: &mut FunctionBuilder<'_>, env: &mut Env, m: &Mechs<'_>, win: &Win, (r, pass, j, mask): (usize, &[usize], Value, Mask), (all, filled): (bool, bool), carry: &[Value]) -> Option<Batched> {
    let next = b.int_bin(IntBinOp::Add, j, env.one);
    let row = m.source.row(b, env, win, j, Carry::from_params(carry, env.mcx), next);
    let masks = mask_words(b, 64, Some(mask.0));
    let src: &dyn SourceMech = &*m.source;
    m.steps[r].block_batch(b, env, (&row, &|a| back(m, pass, a)), BatchRows { masks, nw: 1, all }, &mut |b, env, cols| filled || src.fill_late_cols(b, env, win, (j, mask), cols))
}

/// D3194: block `[j, j + 64)`'s `mask` narrowed by the filtered probe `m.steps[r]` (`StepMech::block_rowfilter`), tested
/// from the source's row `j`, its key columns' late lanes filled first for the mask's rows (`SourceMech::fill_late_cols`).
fn rowfilter_block(b: &mut FunctionBuilder<'_>, env: &mut Env, m: &Mechs<'_>, win: &Win, (r, j, mask): (usize, Value, Mask), carry: &[Value]) -> Option<Value> {
    let next = b.int_bin(IntBinOp::Add, j, env.one);
    let row = m.source.row(b, env, win, j, Carry::from_params(carry, env.mcx), next);
    let src: &dyn SourceMech = &*m.source;
    m.steps[r].block_rowfilter(b, env, &row, j, mask, &mut |b, env, cols| src.fill_late_cols(b, env, win, (j, mask), cols))
}

/// D1005: a deferred pipeline (`deferred[x]`: a lazy build or its producer, `lazy_for` set; M8 may add on-demand runs) fills
/// its slot out of program order, after the pipelines between its last reader and its runner ran; decompose frees a slot at
/// its life's end, so one of those (or the runner's own end) may hold its number then. Each deferred slot takes the lowest
/// number no pipeline uses (a pending one, a lazy build's, below 64), its readers and protocols with it, but a lazy build
/// its probe's pipeline reads when that pipeline is not deferred: its life spans that probe, so none reuses it.
/// D3219: the arms of one production are read off the protocols as `tail::merge_arms` leaves them (renumber_seeded runs it
/// after this pass): a seeded drain's run (the probe .. the drain) that is an arm of an Append's run is one production with
/// the Append's other arms (D1187). Read off the seeded run's own protocol, the run took one number at the drain and the
/// Append's later arms another: two productions, each folded by its own owner, the second over the region the first sealed
/// (kind 2), and the reader of the union read the later arms only.
pub(crate) fn renumber_deferred(arena: gravity_program::ArenaRef, deferred: &[bool], pipelines: &mut [Pipeline], protos: &mut [gravity_program::protos::Proto], node: i32, mcx: mcx::Mcx<'_>) {
    let seeded = protos.iter().any(|f| f.kind == gravity_program::protos::ProtoKind::Follows);
    for x in (0..pipelines.len()).filter(|&x| deferred.get(x) == Some(&true)) {
        let Some(s) = pipelines[x].produces() else { continue };
        // D393 (append.arms): arms that fold into one production of `s` take one number together, at the last arm
        let arms = arms_at(pipelines, protos, seeded, x, s, mcx);
        if arms.last() != Some(&x) {
            continue;
        }
        let q = usize::try_from(pipelines[x].lazy_for).ok();
        let build = q.is_some_and(|q| (0..q).rev().find(|&k| pipelines[k].produces() == Some(s)) == Some(x) && pipelines[q].consumes.contains(&s));
        if build && q.is_some_and(|q| deferred.get(q) != Some(&true)) {
            continue;
        }
        let used = |t: u8| pipelines.iter().any(|p| p.produces() == Some(t) || p.consumes.contains(&t));
        let Some(to) = (0..=u8::MAX).find(|&t| !used(t)).filter(|&t| !build || t < 64) else {
            crate::refuse!(node, crate::admit::refusal::Reason::Width { needed: 257, have: if build { 64 } else { 256 } })
        };
        arms.iter().for_each(|&a| rename_slot(arena, pipelines, protos, a, s, to));
    }
    // D3907 (c-random seed 7 rnd-1414: a CTE's kept run filled at the nest's first outer row, rerun/inner_once.rs
    // `kept_run_lazy`, under an ORDER BY .. LIMIT): a pipeline run in program order that feeds a deferred one (a producer
    // the lazy group leaves out) keeps its life to that reader's run inside runner `q`, but decompose freed its number at
    // the reader's place, so a pipeline after it up to `q` (the runner's own sort end) produced the live number again
    // (driver `open_slot`: L3). Such a production takes the lowest number no pipeline uses, its readers and protocols with it.
    for x in (0..pipelines.len()).filter(|&x| deferred.get(x) != Some(&true)) {
        let Some(s) = pipelines[x].produces() else { continue };
        let arms = arms_at(pipelines, protos, seeded, x, s, mcx);
        let Some(q) = deferred_runner(deferred, pipelines, x, s).filter(|_| arms.last() == Some(&x)) else { continue };
        if !(x + 1..=q.min(pipelines.len() - 1)).any(|z| deferred.get(z) != Some(&true) && pipelines[z].produces() == Some(s)) {
            continue;
        }
        let used = |t: u8| pipelines.iter().any(|p| p.produces() == Some(t) || p.consumes.contains(&t));
        let Some(to) = (0..=u8::MAX).find(|&t| !used(t)) else { crate::refuse!(node, crate::admit::refusal::Reason::Width { needed: 257, have: 256 }) };
        arms.iter().for_each(|&a| rename_slot(arena, pipelines, protos, a, s, to));
    }
}

/// `renumber_deferred`'s arms of pipeline `x`'s production of `s` (D393), read off the protocols as `tail::merge_arms`
/// leaves them when a seeded drain's run is among them (D3219).
fn arms_at(pipelines: &[Pipeline], protos: &[gravity_program::protos::Proto], seeded: bool, x: usize, s: u8, mcx: mcx::Mcx<'_>) -> Vec<usize> {
    if !seeded {
        return gravity_program::protos::arms_of(pipelines, protos, x, s);
    }
    let mut merged = mcx::to_vec_in(protos, mcx);
    crate::frame::tail::merge_arms(&mut merged[..]);
    gravity_program::protos::arms_of(pipelines, &merged[..], x, s)
}

/// D3907: the last runner (`lazy_for`) of a deferred pipeline reading pipeline `x`'s life of slot `s`, if any.
fn deferred_runner(deferred: &[bool], pipelines: &[Pipeline], x: usize, s: u8) -> Option<usize> {
    let mut runner = None;
    for (m, d) in pipelines.iter().enumerate().skip(x + 1) {
        let reads = d.consumes.iter().position(|&c| c == s).is_some_and(|i| d.consumes_from.get(i).is_none_or(|l| l.0 as usize == x));
        if reads && deferred.get(m) == Some(&true) {
            runner = runner.max(usize::try_from(d.lazy_for).ok());
        }
        if d.produces() == Some(s) {
            break;
        }
    }
    runner
}

/// Slot `s` of pipeline `x`'s life renamed `to`: `x`'s breaker, each reader of that life (its consumed slot, a spill
/// partition claim) and the protocols of those pipelines that name it (not `Facts`' lane nor `Verdicts`' kind).
/// D3511: a reader that also produces `s` (a seeded drain's probe reading the life its own run's number reuses, D1188's
/// case) keeps the `Arms` protocol of its own production: that run is a later life, renamed with its own arms.
pub(crate) fn rename_slot(arena: gravity_program::ArenaRef, pipelines: &mut [Pipeline], protos: &mut [gravity_program::protos::Proto], x: usize, s: u8, to: u8) {
    if let Some(b) = crate::frame::iface::breaker_of_mut(&mut pipelines[x]) {
        *b = b.in_slot(to);
    }
    let mut at = vec![x]; // alloc: Program
    for m in x + 1..pipelines.len() {
        let d = pipelines[m];
        if let Some(i) = d.consumes.iter().position(|&c| c == s) {
            let reads = d.consumes_from.get(i).map_or_else(|| (x + 1..m).all(|k| pipelines[k].produces() != Some(s)), |l| l.0 as usize == x);
            if reads {
                let mut n = Pipeline { consumes: &[], consumes_from: &[], ..d };
                for (j, &c) in d.consumes.iter().enumerate() {
                    let c = if j == i { to } else { c };
                    n = match d.consumes_from.get(j) {
                        Some(&l) => n.consuming_from_in(arena, c, l),
                        None => n.consuming_in(arena, c),
                    };
                }
                if n.claims == (gravity_program::ClaimKind::SpillPartitions { slot: s }) {
                    n.claims = gravity_program::ClaimKind::SpillPartitions { slot: to };
                }
                // D3024 (b): a wide build drain names the build it walks (ops/hashjoin/drain/source.rs)
                if n.claims == (gravity_program::ClaimKind::BuildRecords { slot: s }) {
                    n.claims = gravity_program::ClaimKind::BuildRecords { slot: to };
                }
                pipelines[m] = n;
                at.push(m); // alloc: Program
            }
        }
        if d.produces() == Some(s) {
            break;
        }
    }
    let own = |p: &gravity_program::protos::Proto| p.kind == gravity_program::protos::ProtoKind::Arms && p.pipeline as usize != x && pipelines[p.pipeline as usize].produces() == Some(s);
    let named = |p: &gravity_program::protos::Proto| p.slot == s && !matches!(p.kind, gravity_program::protos::ProtoKind::Facts | gravity_program::protos::ProtoKind::Verdicts) && at.contains(&(p.pipeline as usize)) && !own(p);
    protos.iter_mut().filter(|p| named(p)).for_each(|p| p.slot = to);
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use gravity_abi::ctx::{ctx_size, hdr, site_off};
    use gravity_abi::site::site;
    use gravity_interp::Interp;
    use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
    use gravity_ir::{Block, IntBinOp, IntCond, Type, Value};

    use super::{Mask, Mechs, Next, Win, WordWin};
    use crate::face::end::EndMech;
    use crate::face::source::{SourceMech, WinConds};
    use crate::face::step::{Expand, ExpandKind, StepMech};
    use crate::frame::env;
    use crate::frame::env::Env;
    use crate::frame::open::{Base, OpenWord, OpenWords};
    use crate::frame::row::{Carry, RowVals};
    use crate::frame::stop::Exits;

    const I64: Type = Type::I64;

    /// Windows of `.0` rows from site 0's cursor up to `3 * .0`, with block values when `.1`; the verdicts cover step 0 in the
    /// first and last windows.
    struct Src(i64, bool);

    impl SourceMech for Src {
        fn open_words(&self, w: &mut OpenWords) {
            w.declare(OpenWord::open("cursor", Base::Ctx, (site_off(0) + site::CURSOR) as i32, Type::I32, 0));
        }
        fn open(&self, b: &mut FunctionBuilder<'_>, _env: &mut Env) -> Value {
            b.iconst(I64, 0)
        }
        fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
            let cursor = b.load_zext(Type::I32, env.ctx, (site_off(0) + site::CURSOR) as i32);
            let n = b.iconst(I64, 3 * self.0);
            let past = b.icmp(IntCond::Sge, cursor, n);
            let (l_end, l_win) = (b.create_block(&[]), b.create_block(&[]));
            b.cond_br(past, (l_end, &[]), (l_win, &[]));
            b.switch_to(l_end);
            let st = b.iconst(I64, -5);
            b.br(l_eof, &[st]);
            b.switch_to(l_win);
            let w = b.iconst(I64, self.0);
            let end = b.int_bin(IntBinOp::Add, cursor, w);
            let covered = b.icmp(IntCond::Ne, cursor, w);
            Win { cursor, end, facts: WinConds { covered: Some(covered), ..Default::default() } }
        }
        fn row<'e>(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
            RowVals { row: crate::cell::Row::new_in(env.mcx), vals: mcx::PgVec::new_in(env.mcx), cols: mcx::PgVec::new_in(env.mcx), carry, next, at: j }
        }
        fn covers(&self) -> u64 {
            1
        }
        fn word_win<'e>(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win) -> Option<WordWin<'e>> {
            self.1.then(|| WordWin { lanes: mcx::PgVec::new_in(env.mcx), valid: mcx::PgVec::new_in(env.mcx), plain: env.one })
        }
    }

    /// Adds `k` to `ctx.budget_bytes`, the evaluations counter.
    fn tally(b: &mut FunctionBuilder<'_>, env: &Env, k: i64) {
        let n = b.load(I64, env.ctx, hdr::BUDGET_BYTES as i32);
        let k = b.iconst(I64, k);
        let n1 = b.int_bin(IntBinOp::Add, n, k);
        b.store(env.ctx, hdr::BUDGET_BYTES as i32, n1);
    }

    /// A filter passing even rows, tallying 1 per row it tests and, with a block form (`.0`), 100 per block mask.
    struct Even(bool);

    impl StepMech for Even {
        fn block_open(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env, _w: &WordWin) -> Option<Mask> {
            self.0.then_some(Mask(env.one))
        }
        fn block(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _w: &WordWin, _base: Value, _m: Mask) -> Mask {
            tally(b, env, 100);
            Mask(b.iconst(I64, 0x5555_5555_5555_5555))
        }
        fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
            if env.masked {
                return next(b, env, row);
            }
            tally(b, env, 1);
            let odd = b.int_bin(IntBinOp::And, row.at, env.one);
            let even = b.icmp(IntCond::Eq, odd, env.zero);
            let (l_yes, l_join) = (b.create_block(&[]), b.create_block(&[I64]));
            b.cond_br(even, (l_yes, &[]), (l_join, &[row.carry.out]));
            b.switch_to(l_yes);
            let c = next(b, env, row);
            b.br(l_join, &[c.out]);
            b.switch_to(l_join);
            Carry::new(b.block_params(l_join)[0], env.mcx)
        }
    }

    /// Counts the rows that reach it.
    struct Count;

    impl EndMech for Count {
        fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
            Carry::new(b.int_bin(IntBinOp::Add, row.carry.out, env.one), env.mcx)
        }
    }

    /// `(rows out, evaluations)` of the pipeline over `Src(w, blocks)` through `Even(blocks)`.
    fn run(w: i64, blocks: bool) -> (u64, u64) {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let desc = gravity_program::Pipeline::eligible(0, gravity_program::End::Sink, 0);
        let mut m = Mechs { k: 0, source: mcx::pg_box_dyn!(rig.mcx(), Src(w, blocks) => dyn SourceMech), steps: mcx::pg_vec![in rig.mcx(); mcx::pg_box_dyn!(rig.mcx(), Even(blocks) => dyn StepMech)], end: mcx::pg_box_dyn!(rig.mcx(), Count => dyn EndMech), site: Some(0), desc, words: None, state_word: None };
        let mut pb = ProgramBuilder::new();
        let core = env::declare(&mut pb);
        let (f, bounded) = super::pipeline_fn(&mut pb, core, crate::hir::Ir::none(), &[], "p0", 1, &mut m, rig.mcx());
        let p = pb.finish();
        gravity_ir::verify::verify(&p).expect("the frame's IR verifies");
        assert_eq!(crate::frame::cancel::verify(p.func(f), &bounded, rig.mcx()), Ok(()));
        let mut ctx = vec![0u64; ctx_size(1) / 8];
        assert_eq!(Interp::new(&p).run(f, &[ctx.as_mut_ptr() as u64]).expect("runs")[0] as i64, -5);
        (ctx[(site_off(0) + site::ROWS_OUT) / 8], ctx[hdr::BUDGET_BYTES / 8])
    }

    /// One window of rows `[cursor, .0)` from site 0's cursor (a resumed call's window starts at its stopped row).
    struct Rows(i64);

    impl SourceMech for Rows {
        fn open_words(&self, w: &mut OpenWords) {
            Src(self.0, false).open_words(w)
        }
        fn open(&self, b: &mut FunctionBuilder<'_>, _env: &mut Env) -> Value {
            b.iconst(I64, 0)
        }
        fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
            let cursor = b.load_zext(Type::I32, env.ctx, (site_off(0) + site::CURSOR) as i32);
            let end = b.iconst(I64, self.0);
            let past = b.icmp(IntCond::Sge, cursor, end);
            let (l_end, l_win) = (b.create_block(&[]), b.create_block(&[]));
            b.cond_br(past, (l_end, &[]), (l_win, &[]));
            b.switch_to(l_end);
            let st = b.iconst(I64, -5);
            b.br(l_eof, &[st]);
            b.switch_to(l_win);
            Win { cursor, end, facts: WinConds::default() }
        }
        fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
            Src(self.0, false).row(b, env, win, j, carry, next)
        }
    }

    /// L0-g: an expanding step whose row holds one volatile word (frame word 1; its offset is word 0). Each row's `10 j` is
    /// "evaluated" once (tallied), its first output stops the call with the cursor at the row, and the resumed call hands
    /// on the second output with the held value; word 2 sums the values the outputs saw.
    struct Twice;

    impl StepMech for Twice {
        fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
            let v = crate::frame::stop::held(b, env, &[8], Some(0), &mut |b, env| {
                tally(b, env, 1);
                let ten = b.iconst(I64, 10);
                mcx::pg_vec![in env.mcx; b.int_bin(IntBinOp::Mul, row.at, ten)]
            })[0];
            let sum = env.load_word(b, 16);
            let sum = b.int_bin(IntBinOp::Add, sum, v);
            env.store_word(b, 16, sum);
            let off = env.load_word(b, 0);
            let first = b.icmp(IntCond::Eq, off, env.zero);
            let (l_first, l_second) = (b.create_block(&[]), b.create_block(&[]));
            b.cond_br(first, (l_first, &[]), (l_second, &[]));
            b.switch_to(l_first);
            let c = next(b, env, row);
            env.store_word(b, 0, env.one);
            let st = b.iconst(I64, -7);
            env.stop(b, &c, row.at, st);
            b.switch_to(l_second);
            env.store_word(b, 0, env.zero);
            next(b, env, row)
        }
        fn expand(&self) -> Option<Expand> {
            Some(Expand { kind: ExpandKind::Chain, offset: 0, held: 1 })
        }
    }

    /// A row a stopped call re-enters reads its held word: each of the 12 rows is evaluated once though it is lowered on two
    /// calls (24 outputs), and both of its outputs see the first evaluation's value.
    #[test]
    fn a_resumed_row_reads_its_held_words() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let desc = gravity_program::Pipeline::eligible(0, gravity_program::End::Sink, 0);
        let mut m = Mechs { k: 0, source: mcx::pg_box_dyn!(rig.mcx(), Rows(12) => dyn SourceMech), steps: mcx::pg_vec![in rig.mcx(); mcx::pg_box_dyn!(rig.mcx(), Twice => dyn StepMech)], end: mcx::pg_box_dyn!(rig.mcx(), Count => dyn EndMech), site: Some(0), desc, words: Some(1), state_word: None }; // alloc: Program
        let mut pb = ProgramBuilder::new();
        let core = env::declare(&mut pb);
        let (f, _) = super::pipeline_fn(&mut pb, core, crate::hir::Ir::none(), &[], "p0", 1, &mut m, rig.mcx());
        let p = pb.finish();
        gravity_ir::verify::verify(&p).expect("the frame's IR verifies");
        let mut words = [0u64; 3];
        let mut ctx = vec![0u64; ctx_size(2) / 8]; // alloc: Program
        ctx[(site_off(1) + site::KEYS) / 8] = words.as_mut_ptr() as u64;
        let mut calls = vec![]; // alloc: Program
        while calls.last() != Some(&-5) {
            calls.push(Interp::new(&p).run(f, &[ctx.as_mut_ptr() as u64]).expect("runs")[0] as i64);
            assert!(calls.len() <= 13, "{calls:?}");
        }
        assert_eq!(calls, [[-7; 12].as_slice(), &[-5]].concat());
        assert_eq!((ctx[(site_off(0) + site::ROWS_OUT) / 8], ctx[hdr::BUDGET_BYTES / 8]), (24, 12), "(rows out, evaluations)");
        assert_eq!(words[2], 2 * 10 * (0..12).sum::<u64>(), "both outputs of a row saw its one value");
    }

    /// V4R-27: an expanding step (two outputs per row) whose `single_guard` is `.0`; in the single-row body the frame lowers
    /// (`Env::single` naming it) it hands the row on once. Tallies 1 per row in that body, 2 in the chain's.
    struct Pair(bool);

    impl StepMech for Pair {
        fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
            if env.single.is_some() && env.single == env.step {
                tally(b, env, 1);
                return next(b, env, row);
            }
            tally(b, env, 2);
            let c = next(b, env, row);
            next(b, env, &RowVals { row: row.row.clone(), vals: row.vals.clone(), cols: row.cols.clone(), carry: c, next: row.next, at: row.at })
        }
        fn expand(&self) -> Option<Expand> {
            Some(Expand { kind: ExpandKind::Chain, offset: 0, held: 0 })
        }
        fn single_guard(&self, b: &mut FunctionBuilder<'_>, _env: &mut Env) -> Option<Value> {
            Some(b.bconst(self.0))
        }
    }

    /// V4R-27: a window whose guard holds runs the single-row body (12 rows out, 12 evaluations), else the chain's (24, 24).
    #[test]
    fn a_single_guard_selects_the_single_row_body_per_window() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        for (one, want) in [(true, (12, 12)), (false, (24, 24))] {
            let desc = gravity_program::Pipeline::eligible(0, gravity_program::End::Sink, 0);
            let mut m = Mechs { k: 0, source: mcx::pg_box_dyn!(rig.mcx(), Rows(12) => dyn SourceMech), steps: mcx::pg_vec![in rig.mcx(); mcx::pg_box_dyn!(rig.mcx(), Pair(one) => dyn StepMech)], end: mcx::pg_box_dyn!(rig.mcx(), Count => dyn EndMech), site: Some(0), desc, words: Some(1), state_word: None }; // alloc: Program
            let mut pb = ProgramBuilder::new();
            let core = env::declare(&mut pb);
            let (f, bounded) = super::pipeline_fn(&mut pb, core, crate::hir::Ir::none(), &[], "p0", 1, &mut m, rig.mcx());
            let p = pb.finish();
            gravity_ir::verify::verify(&p).expect("the frame's IR verifies");
            assert_eq!(crate::frame::cancel::verify(p.func(f), &bounded, rig.mcx()), Ok(()));
            let mut words = [0u64; 1];
            let mut ctx = vec![0u64; ctx_size(2) / 8]; // alloc: Program
            ctx[(site_off(1) + site::KEYS) / 8] = words.as_mut_ptr() as u64;
            assert_eq!(Interp::new(&p).run(f, &[ctx.as_mut_ptr() as u64]).expect("runs")[0] as i64, -5);
            assert_eq!((ctx[(site_off(0) + site::ROWS_OUT) / 8], ctx[hdr::BUDGET_BYTES / 8]), want, "guard {one}: (rows out, evaluations)");
        }
    }

    #[test]
    fn a_covered_window_never_runs_its_covered_steps() {
        // straight: covered 0..3 and 8..11 pass unevaluated; 4..7 tests 4 rows and passes 4, 6
        assert_eq!(run(4, false), (10, 4));
        // blocks: covered 0..69 and 140..209 (each a whole block and a 6-row tail) pass unevaluated; 70..139 takes one
        // block mask (32 even rows) and tests its 6-row tail (134, 136, 138 pass)
        assert_eq!(run(70, true), (140 + 32 + 3, 100 + 6));
    }

    /// An end whose section close (`finish`, emitted in the `done` exit) stops and raises: the exits only it enters are
    /// emitted with their bodies (D528: an exit left an empty `unreachable` there returned garbage rows, e2e heapgroup-041).
    struct FinishExits;

    impl EndMech for FinishExits {
        fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
            Carry::new(b.int_bin(IntBinOp::Add, row.carry.out, env.one), env.mcx)
        }
        fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) {
            let w = b.load(I64, env.ctx, hdr::BUDGET_BYTES as i32);
            let (l_raise, l_on, l_cont) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
            let stop = b.icmp(IntCond::Eq, w, env.one);
            b.cond_br(stop, (x.l_stop_in, &[env.zero, env.zero, env.one]), (l_on, &[]));
            b.switch_to(l_on);
            let two = b.iconst(I64, 2);
            let raise = b.icmp(IntCond::Eq, w, two);
            b.cond_br(raise, (l_raise, &[]), (l_cont, &[]));
            b.switch_to(l_raise);
            b.br(x.l_raise, &[two]);
            b.switch_to(l_cont);
        }
    }

    /// D1071: `.0` rows from site 0's cursor whose end is `done` (status 0); with `.1` it sets the byte `ctx.cancel` points
    /// at as it reports the end (a cancel arriving past the window loop's check).
    struct Ends(i64, bool);

    impl SourceMech for Ends {
        fn open_words(&self, w: &mut OpenWords) {
            Src(self.0, false).open_words(w)
        }
        fn open(&self, b: &mut FunctionBuilder<'_>, _env: &mut Env) -> Value {
            b.iconst(I64, 0)
        }
        fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
            let cursor = b.load_zext(Type::I32, env.ctx, (site_off(0) + site::CURSOR) as i32);
            let end = b.iconst(I64, self.0);
            let past = b.icmp(IntCond::Sge, cursor, end);
            let (l_end, l_win) = (b.create_block(&[]), b.create_block(&[]));
            b.cond_br(past, (l_end, &[]), (l_win, &[]));
            b.switch_to(l_end);
            if self.1 {
                let byte = b.load(Type::Ptr, env.ctx, hdr::CANCEL as i32);
                let one = b.iconst(Type::I8, 1);
                b.store(byte, 0, one);
            }
            b.br(l_eof, &[env.zero]);
            b.switch_to(l_win);
            Win { cursor, end, facts: WinConds::default() }
        }
        fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
            Src(self.0, false).row(b, env, win, j, carry, next)
        }
    }

    /// D1071: holds every row back (frame word 0 holds a count of them, word 2 counts every row lowered) and at the input's
    /// end hands one row on, then clears word 0; a stop inside leaves word 0 set, so the resumed call hands it on again.
    struct Hold;

    impl StepMech for Hold {
        fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _next: Next<'_, 'e>) -> Carry<'e> {
            for off in [0, 16] {
                let n = env.load_word(b, off);
                let n = b.int_bin(IntBinOp::Add, n, env.one);
                env.store_word(b, off, n);
            }
            row.carry.copy_in(env.mcx)
        }
        fn flushes(&self) -> bool {
            true
        }
        fn flush<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, carry: Carry<'e>, next: Next<'_, 'e>) -> Carry<'e> {
            let n = env.load_word(b, 0);
            let held = b.icmp(IntCond::Ne, n, env.zero);
            let (l_on, l_join) = (b.create_block(&[]), b.create_block(&[I64]));
            b.cond_br(held, (l_on, &[]), (l_join, &[carry.out]));
            b.switch_to(l_on);
            let mcx = env.mcx;
            let c = next(b, env, &RowVals { row: crate::cell::Row::new_in(mcx), vals: mcx::PgVec::new_in(mcx), cols: mcx::PgVec::new_in(mcx), carry, next: env.one, at: env.zero });
            env.store_word(b, 0, env.zero);
            b.br(l_join, &[c.out]);
            b.switch_to(l_join);
            Carry::new(b.block_params(l_join)[0], env.mcx)
        }
    }

    /// Counts the rows that reach it; the first stops at its row (frame word 1 marks it), as a full out block does.
    struct StopOnce;

    impl EndMech for StopOnce {
        fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
            let s = env.load_word(b, 8);
            let first = b.icmp(IntCond::Eq, s, env.zero);
            let (l_stop, l_take) = (b.create_block(&[]), b.create_block(&[]));
            b.cond_br(first, (l_stop, &[]), (l_take, &[]));
            b.switch_to(l_stop);
            env.store_word(b, 8, env.one);
            let st = b.iconst(I64, -7);
            env.stop(b, &row.carry, row.at, st);
            b.switch_to(l_take);
            Carry::new(b.int_bin(IntBinOp::Add, row.carry.out, env.one), env.mcx)
        }
    }

    /// `Ends(4, cancel)` through `Hold` into `StopOnce`, verified; each call's status, then `(site 0's cursor, rows out)`
    /// and the frame words.
    fn flushed(cancel: bool) -> (Vec<i64>, (u64, u64), [u64; 3]) {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let desc = gravity_program::Pipeline::eligible(0, gravity_program::End::Sink, 0);
        let mut m = Mechs { k: 0, source: mcx::pg_box_dyn!(rig.mcx(), Ends(4, cancel) => dyn SourceMech), steps: mcx::pg_vec![in rig.mcx(); mcx::pg_box_dyn!(rig.mcx(), Hold => dyn StepMech)], end: mcx::pg_box_dyn!(rig.mcx(), StopOnce => dyn EndMech), site: Some(0), desc, words: Some(1), state_word: None }; // alloc: Program
        let mut pb = ProgramBuilder::new();
        let core = env::declare(&mut pb);
        let (f, bounded) = super::pipeline_fn(&mut pb, core, crate::hir::Ir::none(), &[], "p0", 1, &mut m, rig.mcx());
        let p = pb.finish();
        gravity_ir::verify::verify(&p).expect("the frame's IR verifies");
        assert_eq!(crate::frame::cancel::verify(p.func(f), &bounded, rig.mcx()), Ok(()));
        let (mut words, mut out, mut byte) = ([0u64; 3], [0u64; 32], [0u8; 8]);
        let mut ctx = vec![0u64; ctx_size(2) / 8]; // alloc: Program
        ctx[(site_off(1) + site::KEYS) / 8] = words.as_mut_ptr() as u64;
        ctx[hdr::OUT / 8] = out.as_mut_ptr() as u64;
        ctx[hdr::CANCEL / 8] = byte.as_mut_ptr() as u64;
        let mut calls = vec![]; // alloc: Program
        while calls.last().is_none_or(|&s| s == -7) {
            calls.push(Interp::new(&p).run(f, &[ctx.as_mut_ptr() as u64]).expect("runs")[0] as i64);
            assert!(calls.len() <= 3, "{calls:?}");
        }
        // SAFETY: the site's cursor word lies inside `ctx`
        let cursor = unsafe { (ctx.as_ptr() as *const u8).add(site_off(0) + site::CURSOR).cast::<u32>().read_unaligned() } as u64;
        (calls, (cursor, ctx[(site_off(0) + site::ROWS_OUT) / 8]), words)
    }

    /// D1071: a flush the end stops at keeps the source's position (no row is read again) and the step's held row, which
    /// the resumed call hands on once; then the call is done.
    #[test]
    fn a_flush_the_end_stops_at_is_handed_on_by_the_resumed_call() {
        let (calls, (cursor, rows), words) = flushed(false);
        assert_eq!(calls, [-7, gravity_abi::status::DONE]);
        assert_eq!((cursor, rows), (4, 1), "(site 0's cursor, rows out)");
        assert_eq!(words, [0, 1, 4], "(held, the end's stop mark, rows lowered)");
    }

    /// D1071: a cancel that arrives as the input ends is honoured before the flush hands anything on.
    #[test]
    fn a_cancel_at_the_input_end_leaves_before_the_flush() {
        let (calls, (cursor, rows), words) = flushed(true);
        assert_eq!(calls, [gravity_abi::status::CANCELLED]);
        assert_eq!((cursor, rows), (4, 0), "(site 0's cursor, rows out)");
        assert_eq!(words, [4, 0, 4], "(held, the end's stop mark, rows lowered)");
    }

    /// D2827: `Ends` as a scan site: its end sets site 0's state to EOF, and a window asked of a site at EOF seeks anew
    /// (state OPEN, cursor 0) and reads from row 0 again, as the driver's `next` does for any state but OPEN.
    struct Reseeks(i64);

    impl SourceMech for Reseeks {
        fn open_words(&self, w: &mut OpenWords) {
            Src(self.0, false).open_words(w)
        }
        fn open(&self, b: &mut FunctionBuilder<'_>, _env: &mut Env) -> Value {
            b.iconst(I64, 0)
        }
        fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
            let state = (site_off(0) + site::STATE) as i32;
            let st = b.load_zext(Type::I32, env.ctx, state);
            let eof = b.iconst(I64, site::STATE_EOF as i64);
            let seek = b.icmp(IntCond::Eq, st, eof);
            let (l_seek, l_at) = (b.create_block(&[]), b.create_block(&[]));
            b.cond_br(seek, (l_seek, &[]), (l_at, &[]));
            b.switch_to(l_seek);
            let (open, zero) = (b.iconst(Type::I32, site::STATE_OPEN as i64), b.iconst(Type::I32, 0));
            b.store(env.ctx, state, open);
            b.store(env.ctx, (site_off(0) + site::CURSOR) as i32, zero);
            b.br(l_at, &[]);
            b.switch_to(l_at);
            let cursor = b.load_zext(Type::I32, env.ctx, (site_off(0) + site::CURSOR) as i32);
            let end = b.iconst(I64, self.0);
            let past = b.icmp(IntCond::Sge, cursor, end);
            let (l_end, l_win) = (b.create_block(&[]), b.create_block(&[]));
            b.cond_br(past, (l_end, &[]), (l_win, &[]));
            b.switch_to(l_end);
            let eof32 = b.iconst(Type::I32, site::STATE_EOF as i64);
            b.store(env.ctx, state, eof32);
            b.br(l_eof, &[env.zero]);
            b.switch_to(l_win);
            Win { cursor, end, facts: WinConds::default() }
        }
        fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
            Src(self.0, false).row(b, env, win, j, carry, next)
        }
    }

    /// D2827: a stop inside the input end's flush over a scan site that seeks anew past its end: the resumed call reaches
    /// the end again without the source (no row is lowered twice) and hands the held row on once; then the call is done.
    #[test]
    fn a_flush_stopped_over_a_reseeking_site_never_reads_it_again() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let desc = gravity_program::Pipeline::eligible(0, gravity_program::End::Sink, 0);
        let mut m = Mechs { k: 0, source: mcx::pg_box_dyn!(rig.mcx(), Reseeks(4) => dyn SourceMech), steps: mcx::pg_vec![in rig.mcx(); mcx::pg_box_dyn!(rig.mcx(), Hold => dyn StepMech)], end: mcx::pg_box_dyn!(rig.mcx(), StopOnce => dyn EndMech), site: Some(0), desc, words: Some(1), state_word: None }; // alloc: Program
        let mut pb = ProgramBuilder::new();
        let core = env::declare(&mut pb);
        let (f, bounded) = super::pipeline_fn(&mut pb, core, crate::hir::Ir::none(), &[], "p0", 1, &mut m, rig.mcx());
        let p = pb.finish();
        gravity_ir::verify::verify(&p).expect("the frame's IR verifies");
        assert_eq!(crate::frame::cancel::verify(p.func(f), &bounded, rig.mcx()), Ok(()));
        let (mut words, mut out, mut byte) = ([0u64; 3], [0u64; 32], [0u8; 8]);
        let mut ctx = vec![0u64; ctx_size(2) / 8]; // alloc: Program
        ctx[(site_off(1) + site::KEYS) / 8] = words.as_mut_ptr() as u64;
        ctx[hdr::OUT / 8] = out.as_mut_ptr() as u64;
        ctx[hdr::CANCEL / 8] = byte.as_mut_ptr() as u64;
        let mut calls = vec![]; // alloc: Program
        while calls.last().is_none_or(|&s| s == -7) {
            calls.push(Interp::new(&p).run(f, &[ctx.as_mut_ptr() as u64]).expect("runs")[0] as i64);
            assert!(calls.len() <= 3, "{calls:?}");
        }
        assert_eq!(calls, [-7, gravity_abi::status::DONE]);
        assert_eq!(ctx[(site_off(0) + site::ROWS_OUT) / 8], 1, "rows out");
        assert_eq!(words, [0, 1, 4], "(held, the end's stop mark, rows lowered)");
    }

    /// D2580: lets rows through up to row `.0`, where its count is reached (`Env::full`, the cursor at the row), as a met LIMIT
    /// or a merge join whose inner ran out does (mergejoin/fill_inner.rs `DONE`).
    struct Cut(i64);

    impl StepMech for Cut {
        fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
            let n = b.iconst(I64, self.0);
            let met = b.icmp(IntCond::Eq, row.at, n);
            let (l_met, l_go) = (b.create_block(&[]), b.create_block(&[]));
            b.cond_br(met, (l_met, &[]), (l_go, &[]));
            b.switch_to(l_met);
            env.full(b, &row.carry, row.at);
            b.switch_to(l_go);
            next(b, env, row)
        }
    }

    /// D2580: counts the rows it gets in frame word `.0 / 8` and, with `.1`, passes each on too (else holds it); at the input's
    /// end it hands one row on when it counted any, then clears its word.
    struct Keep(i32, bool);

    impl StepMech for Keep {
        fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
            let n = env.load_word(b, self.0);
            let n = b.int_bin(IntBinOp::Add, n, env.one);
            env.store_word(b, self.0, n);
            if self.1 {
                return next(b, env, row);
            }
            row.carry.copy_in(env.mcx)
        }
        fn flushes(&self) -> bool {
            true
        }
        fn flush<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, carry: Carry<'e>, next: Next<'_, 'e>) -> Carry<'e> {
            let n = env.load_word(b, self.0);
            let held = b.icmp(IntCond::Ne, n, env.zero);
            let (l_on, l_join) = (b.create_block(&[]), b.create_block(&[I64]));
            b.cond_br(held, (l_on, &[]), (l_join, &[carry.out]));
            b.switch_to(l_on);
            let mcx = env.mcx;
            let c = next(b, env, &RowVals { row: crate::cell::Row::new_in(mcx), vals: mcx::PgVec::new_in(mcx), cols: mcx::PgVec::new_in(mcx), carry, next: env.one, at: env.zero });
            env.store_word(b, self.0, env.zero);
            b.br(l_join, &[c.out]);
            b.switch_to(l_join);
            Carry::new(b.block_params(l_join)[0], env.mcx)
        }
    }

    /// `Ends(4, false)` through `steps` into `end`, verified; each call's status, then `(site 0's cursor, rows out)` and the
    /// frame words.
    fn cut(steps: mcx::PgVec<'_, mcx::PgBox<'_, dyn StepMech>>, end: mcx::PgBox<'_, dyn EndMech>) -> (Vec<i64>, (u64, u64), [u64; 6]) {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let desc = gravity_program::Pipeline::eligible(0, gravity_program::End::Sink, 0);
        let mut m = Mechs { k: 0, source: mcx::pg_box_dyn!(rig.mcx(), Ends(4, false) => dyn SourceMech), steps, end, site: Some(0), desc, words: Some(1), state_word: None };
        let mut pb = ProgramBuilder::new();
        let core = env::declare(&mut pb);
        let (f, bounded) = super::pipeline_fn(&mut pb, core, crate::hir::Ir::none(), &[], "p0", 1, &mut m, rig.mcx());
        let p = pb.finish();
        gravity_ir::verify::verify(&p).expect("the frame's IR verifies");
        assert_eq!(crate::frame::cancel::verify(p.func(f), &bounded, rig.mcx()), Ok(()));
        let (mut words, mut out, mut byte) = ([0u64; 6], [0u64; 32], [0u8; 8]);
        let mut ctx = vec![0u64; ctx_size(2) / 8]; // alloc: Program
        ctx[(site_off(1) + site::KEYS) / 8] = words.as_mut_ptr() as u64;
        ctx[hdr::OUT / 8] = out.as_mut_ptr() as u64;
        ctx[hdr::CANCEL / 8] = byte.as_mut_ptr() as u64;
        let mut calls = vec![]; // alloc: Program
        while calls.last().is_none_or(|&s| s == -7) {
            calls.push(Interp::new(&p).run(f, &[ctx.as_mut_ptr() as u64]).expect("runs")[0] as i64);
            assert!(calls.len() <= 3, "{calls:?}");
        }
        // SAFETY: the site's cursor word lies inside `ctx`
        let cursor = unsafe { (ctx.as_ptr() as *const u8).add(site_off(0) + site::CURSOR).cast::<u32>().read_unaligned() } as u64;
        (calls, (cursor, ctx[(site_off(0) + site::ROWS_OUT) / 8]), words)
    }

    /// D2580 (shapecheck g5-text-null-col.join.target): a count reached before a step that holds rows back (a merge join
    /// whose inner ran out under a streamed GROUP BY's open group) hands the held row on before `done`, as the input's end
    /// does: the end stops at it, the cursor stays at the row met, and the resumed call meets the count there again and
    /// hands the row on once. Before, the count reached was `done` at once and the held row was lost.
    #[test]
    fn a_count_reached_before_a_holding_step_hands_its_row_on() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let c = rig.mcx();
        let (calls, (cursor, rows), words) = cut(mcx::pg_vec![in c; mcx::pg_box_dyn!(c, Cut(2) => dyn StepMech), mcx::pg_box_dyn!(c, Hold => dyn StepMech)], mcx::pg_box_dyn!(c, StopOnce => dyn EndMech)); // alloc: Program
        assert_eq!(calls, [-7, gravity_abi::status::DONE]);
        assert_eq!((cursor, rows), (2, 1), "(site 0's cursor, rows out)");
        assert_eq!(words[..3], [0, 1, 2], "(held, the end's stop mark, rows lowered)");
    }

    /// D2580: only the holding steps after the step met flush: the one before it (word 3) feeds the met count and keeps
    /// what it counted; the one after it (word 4) hands its row on. The input's end flushes both (no count met).
    #[test]
    fn a_count_reached_flushes_only_the_holding_steps_after_it() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let c = rig.mcx();
        let steps = |n: i64| mcx::pg_vec![in c; mcx::pg_box_dyn!(c, Keep(24, true) => dyn StepMech), mcx::pg_box_dyn!(c, Cut(n) => dyn StepMech), mcx::pg_box_dyn!(c, Keep(32, false) => dyn StepMech)]; // alloc: Program
        let (calls, (cursor, rows), words) = cut(steps(2), mcx::pg_box_dyn!(c, Count => dyn EndMech));
        assert_eq!(calls, [gravity_abi::status::DONE]);
        assert_eq!((cursor, rows), (2, 1), "(site 0's cursor, rows out)");
        assert_eq!(words[3..5], [3, 0], "(the step before the met count's rows, the step after it's)");
        // never met: the input's end flushes the first step's row through the count into the second step, then that one's
        let (calls, (cursor, rows), words) = cut(steps(9), mcx::pg_box_dyn!(c, Count => dyn EndMech));
        assert_eq!(calls, [gravity_abi::status::DONE]);
        assert_eq!((cursor, rows), (4, 1), "(site 0's cursor, rows out)");
        assert_eq!(words[3..5], [0, 0]);
    }

    /// D2141: ends the call before any row passes it, at its window (a false gate's, a met LIMIT's on a resumed call) or,
    /// with `.0`, already at the open (`StepMech::closes`, D2152: the source never opens).
    struct Shut(bool);

    impl StepMech for Shut {
        fn closes(&self, b: &mut FunctionBuilder<'_>, _env: &Env) -> Option<Value> {
            self.0.then(|| b.bconst(true))
        }
        fn window(&self, b: &mut FunctionBuilder<'_>, _env: &mut Env, x: &Exits) {
            let (t, l_go) = (b.bconst(true), b.create_block(&[]));
            b.cond_br(t, (x.l_done, &[]), (l_go, &[]));
            b.switch_to(l_go);
        }
        fn lower<'e>(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _next: Next<'_, 'e>) -> Carry<'e> {
            row.carry.copy_in(env.mcx)
        }
    }

    /// D2141: holds nothing back but hands one row on at the input's end (a RIGHT merge's ENDOUTER over an empty outer), then
    /// marks frame word `.0 / 8`; a stop inside leaves the word clear, so the resumed call hands the row on again.
    struct Fill(i32);

    impl StepMech for Fill {
        fn lower<'e>(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _next: Next<'_, 'e>) -> Carry<'e> {
            row.carry.copy_in(env.mcx)
        }
        fn flushes(&self) -> bool {
            true
        }
        fn flush<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, carry: Carry<'e>, next: Next<'_, 'e>) -> Carry<'e> {
            let n = env.load_word(b, self.0);
            let fresh = b.icmp(IntCond::Eq, n, env.zero);
            let (l_on, l_join) = (b.create_block(&[]), b.create_block(&[I64]));
            b.cond_br(fresh, (l_on, &[]), (l_join, &[carry.out]));
            b.switch_to(l_on);
            let mcx = env.mcx;
            let c = next(b, env, &RowVals { row: crate::cell::Row::new_in(mcx), vals: mcx::PgVec::new_in(mcx), cols: mcx::PgVec::new_in(mcx), carry, next: env.one, at: env.zero });
            env.store_word(b, self.0, env.one);
            b.br(l_join, &[c.out]);
            b.switch_to(l_join);
            Carry::new(b.block_params(l_join)[0], env.mcx)
        }
    }

    /// D2141: a step that ends the call before a step that hands rows on at the input's end (at its window, or closing the
    /// call at the open) lets that step flush: PG runs the nodes above it over an empty input. The source is never read
    /// (cursor 0). One after the flushing step ends the call with nothing handed on. Before, every such end was `done`.
    #[test]
    fn a_step_that_ends_the_call_before_a_flushing_step_lets_it_flush() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let c = rig.mcx();
        for open in [false, true] {
            let (calls, (cursor, rows), words) = cut(mcx::pg_vec![in c; mcx::pg_box_dyn!(c, Shut(open) => dyn StepMech), mcx::pg_box_dyn!(c, Fill(24) => dyn StepMech)], mcx::pg_box_dyn!(c, Count => dyn EndMech)); // alloc: Program
            assert_eq!(calls, [gravity_abi::status::DONE], "closes at the open: {open}");
            assert_eq!((cursor, rows, words[3]), (0, 1, 1), "(cursor, rows out, the fill's mark); closes at the open: {open}");
            let (calls, (cursor, rows), words) = cut(mcx::pg_vec![in c; mcx::pg_box_dyn!(c, Fill(24) => dyn StepMech), mcx::pg_box_dyn!(c, Shut(open) => dyn StepMech)], mcx::pg_box_dyn!(c, Count => dyn EndMech)); // alloc: Program
            assert_eq!(calls, [gravity_abi::status::DONE]);
            assert_eq!((cursor, rows, words[3]), (0, 0, 0), "after the flushing step; closes at the open: {open}");
        }
    }

    /// D2141: the end stops at the flushed row; the resumed call meets the step's end again at its window and flushes once more.
    #[test]
    fn a_flush_after_an_ended_step_is_resumed_by_the_next_call() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let c = rig.mcx();
        for open in [false, true] {
            let (calls, (cursor, rows), words) = cut(mcx::pg_vec![in c; mcx::pg_box_dyn!(c, Shut(open) => dyn StepMech), mcx::pg_box_dyn!(c, Fill(24) => dyn StepMech)], mcx::pg_box_dyn!(c, StopOnce => dyn EndMech)); // alloc: Program
            assert_eq!(calls, [-7, gravity_abi::status::DONE], "closes at the open: {open}");
            assert_eq!((cursor, rows), (0, 1), "(site 0's cursor, rows out); closes at the open: {open}");
            assert_eq!((words[1], words[3]), (1, 1), "(the end's stop mark, the fill's mark); closes at the open: {open}");
        }
    }

    /// D2147: an end whose open fills its state (state word 1 = 7) and whose every row adds it to the rows out; the first row
    /// stops (frame word 1 marks it), as a lazy build's wait does, and the resumed call skips the open.
    struct Stateful;

    impl EndMech for Stateful {
        fn state_bytes(&self) -> u32 {
            16
        }
        fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
            let seven = b.iconst(I64, 7);
            b.store(env.state.expect("the state is laid before the open"), 8, seven);
        }
        fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, x: &Exits) -> Carry<'e> {
            let c = StopOnce.lower(b, env, row, x);
            let v = b.load(I64, env.state.expect("the state is laid before the rows"), 8);
            let v = b.int_bin(IntBinOp::Sub, v, env.one);
            Carry::new(b.int_bin(IntBinOp::Add, c.out, v), env.mcx)
        }
    }

    /// `Rows(4)` into `Stateful`, its state at `state_word` (`None`: a stack slot); each call's status, the rows out, the frame
    /// words and the function's stack slots.
    fn resumed(state_word: Option<i32>) -> (Vec<i64>, u64, [u64; 4], usize) {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let desc = gravity_program::Pipeline::eligible(0, gravity_program::End::Sink, 0);
        let mut m = Mechs { k: 0, source: mcx::pg_box_dyn!(rig.mcx(), Rows(4) => dyn SourceMech), steps: mcx::PgVec::new_in(rig.mcx()), end: mcx::pg_box_dyn!(rig.mcx(), Stateful => dyn EndMech), site: Some(0), desc, words: Some(1), state_word }; // alloc: Program
        let mut pb = ProgramBuilder::new();
        let core = env::declare(&mut pb);
        let (f, bounded) = super::pipeline_fn(&mut pb, core, crate::hir::Ir::none(), &[], "p0", 1, &mut m, rig.mcx());
        let p = pb.finish();
        gravity_ir::verify::verify(&p).expect("the frame's IR verifies");
        assert_eq!(crate::frame::cancel::verify(p.func(f), &bounded, rig.mcx()), Ok(()));
        let (mut words, mut out, mut byte) = ([0u64; 4], [0u64; 32], [0u8; 8]);
        let mut ctx = vec![0u64; ctx_size(2) / 8]; // alloc: Program
        ctx[(site_off(1) + site::KEYS) / 8] = words.as_mut_ptr() as u64;
        ctx[hdr::OUT / 8] = out.as_mut_ptr() as u64;
        ctx[hdr::CANCEL / 8] = byte.as_mut_ptr() as u64;
        let calls = (0..2).map(|_| Interp::new(&p).run(f, &[ctx.as_mut_ptr() as u64]).expect("runs")[0] as i64).collect(); // alloc: Program
        (calls, ctx[(site_off(0) + site::ROWS_OUT) / 8], words, p.func(f).stack_slots.len())
    }

    /// D2147: in frame words the resumed call reads the state the open wrote (4 rows x 7); in a stack slot (trunk's layout,
    /// every pipeline but a lazy build's probe) the resumed call's fresh frame has lost it (the interpreter zeroes a slot).
    /// The resumed call ends at `Rows`' end of input (-5).
    #[test]
    fn an_end_state_in_frame_words_survives_a_resume() {
        assert_eq!(resumed(Some(16)), (vec![-7, -5], 28, [0, 1, 0, 7], 0));
        assert_eq!(resumed(None), (vec![-7, -5], 0, [0, 1, 0, 0], 1));
    }

    #[test]
    fn an_exit_only_the_section_close_enters_is_emitted() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let desc = gravity_program::Pipeline::eligible(0, gravity_program::End::Sink, 0);
        let mut m = Mechs { k: 0, source: mcx::pg_box_dyn!(rig.mcx(), Rows(4) => dyn SourceMech), steps: mcx::PgVec::new_in(rig.mcx()), end: mcx::pg_box_dyn!(rig.mcx(), FinishExits => dyn EndMech), site: Some(0), desc, words: None, state_word: None };
        let mut pb = ProgramBuilder::new();
        let core = env::declare(&mut pb);
        let (f, _) = super::pipeline_fn(&mut pb, core, crate::hir::Ir::none(), &[], "p0", 1, &mut m, rig.mcx());
        let mut p = pb.finish();
        gravity_ir::prune::unreachable(&mut p.funcs[f.index()]);
        gravity_ir::verify::verify(&p).expect("the frame's IR verifies");
        let func = p.func(f);
        let entered: std::collections::BTreeSet<Block> = func.blocks.iter().flat_map(|bb| bb.term.edges().map(|e| e.target)).collect();
        for b in entered {
            let bb = func.block(b);
            assert!(!(bb.insts.is_empty() && matches!(bb.term, gravity_ir::Terminator::Unreachable)), "b{} is entered but was never emitted", b.0);
        }
    }

    /// D1005: a fed lazy build's producer (a GROUP BY into slot 0, read by the build) and the probe's own end (slot 0 again,
    /// decompose's reuse once the producer's life ended): the producer takes the lowest unused number, its reader and its
    /// protocols with it (not a `Facts` lane); the build its not-deferred probe reads keeps slot 1. Nested (the inner
    /// build lazy for a deferred probe pipeline): the inner build is renumbered too, below 64.
    #[test]
    fn a_deferred_slot_is_renumbered_clear_of_the_probes_end() {
        use gravity_abi::breaker::{Breaker, BreakerKind, Budget, Spill};
        use gravity_program::protos::{Proto, ProtoKind};
        use gravity_program::{Arena, End, LifeId, Pipeline};
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let br = |slot: u8| End::Breaker(Breaker { kind: BreakerKind::HashAgg, slot, bytes: 0, budget: Budget::Share, spill: Spill::None });
        let lazy = |p: Pipeline, q: i16| Pipeline { lazy_for: q, ..p };
        let rig = mcx::MemoryContext::new("test");
        let run = |ps: &mut [Pipeline], protos: &mut [Proto]| {
            let deferred: Vec<bool> = ps.iter().map(|p| p.lazy_for >= 0).collect();
            super::renumber_deferred(a, &deferred, ps, protos, -1, rig.mcx());
        };
        // fed: p0 GROUP BY -> 0 (lazy for 2); p1 reads 0, builds 1 (lazy for 2); p2 probes 1, count -> 0; p3 emits 0
        let mut ps = [
            lazy(Pipeline::eligible(-1, br(0), 0), 2),
            lazy(Pipeline::eligible(-1, br(1), 0).consuming_from_in(a, 0, LifeId(0)), 2),
            Pipeline::eligible(-1, br(0), 0).consuming_from_in(a, 1, LifeId(1)),
            Pipeline::eligible(-1, End::Sink, 0).consuming_from_in(a, 0, LifeId(2)),
        ];
        let mut protos = [Proto::new(ProtoKind::Observe, 0).slot(0), Proto::new(ProtoKind::Facts, 0).slot(0), Proto::new(ProtoKind::Observe, 2).slot(0)];
        run(&mut ps, &mut protos);
        assert_eq!((ps[0].produces(), ps[1].consumes, ps[1].produces()), (Some(2), &[2u8][..], Some(1)));
        assert_eq!((ps[2].consumes, ps[2].produces(), ps[3].consumes), (&[1u8][..], Some(0), &[0u8][..]));
        assert_eq!((protos[0].slot, protos[1].slot, protos[2].slot), (2, 0, 0));
        // nested: p0 VALUES build -> 0 (lazy for 1); p1 probes 0, builds 1 (lazy for 2); p2 probes 1, count -> 0
        let mut ps = [
            lazy(Pipeline::eligible(-1, br(0), 0), 1),
            lazy(Pipeline::eligible(-1, br(1), 0).consuming_from_in(a, 0, LifeId(0)), 2),
            Pipeline::eligible(-1, br(0), 0).consuming_from_in(a, 1, LifeId(1)),
        ];
        run(&mut ps, &mut []);
        assert_eq!((ps[0].produces(), ps[1].consumes, ps[1].produces(), ps[2].produces()), (Some(2), &[2u8][..], Some(1), Some(0)));
    }

    /// D3907 (c-random seed 7 rnd-1414): p0 sorts into 0; p1 reads it and sorts into 1, in program order; p2 reads 1 and
    /// fills the kept run 0, deferred to the nest p3's first outer row; p3 waits on 0 and ends in its own top-N sort, 1
    /// again (decompose freed it at p2's place); p4 emits 1. p1's life lasts to p2's run inside p3, so p1's production
    /// takes a free number, p2's read with it. With a runner whose end takes another number, nothing moves.
    #[test]
    fn an_in_order_producer_of_a_deferred_reader_is_renumbered_clear_of_the_runners_end() {
        use gravity_abi::breaker::{Breaker, BreakerKind, Budget, Spill};
        use gravity_program::{Arena, End, LifeId, Pipeline};
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let br = |slot: u8| End::Breaker(Breaker { kind: BreakerKind::Sort, slot, bytes: 0, budget: Budget::Share, spill: Spill::None });
        let lazy = |p: Pipeline, q: i16| Pipeline { lazy_for: q, ..p };
        let make = |end: u8| {
            [
                Pipeline::eligible(-1, br(0), 0),
                Pipeline::eligible(-1, br(1), 0).consuming_from_in(a, 0, LifeId(0)),
                lazy(Pipeline::eligible(-1, br(0), 0).consuming_from_in(a, 1, LifeId(1)), 3),
                Pipeline::eligible(-1, br(end), 0).consuming_from_in(a, 0, LifeId(2)),
                Pipeline::eligible(-1, End::Sink, 0).consuming_from_in(a, end, LifeId(3)),
            ]
        };
        let rig = mcx::MemoryContext::new("test");
        let run = |ps: &mut [Pipeline]| {
            let deferred: Vec<bool> = ps.iter().map(|p| p.lazy_for >= 0).collect();
            super::renumber_deferred(a, &deferred, ps, &mut [], -1, rig.mcx());
        };
        let mut ps = make(1);
        run(&mut ps);
        assert_eq!((ps[0].produces(), ps[1].consumes, ps[1].produces()), (Some(0), &[0u8][..], Some(2)));
        assert_eq!((ps[2].consumes, ps[2].produces(), ps[3].consumes, ps[3].produces(), ps[4].consumes), (&[2u8][..], Some(0), &[0u8][..], Some(1), &[1u8][..]));
        let mut ps = make(3);
        run(&mut ps);
        assert_eq!((ps[1].produces(), ps[2].consumes, ps[2].produces(), ps[3].produces()), (Some(1), &[1u8][..], Some(0), Some(3)));
    }

    /// D3219: an Init's members (all deferred): p0 builds 0; p1 probes it, the seeded first arm of a GROUP BY into 2; p2
    /// drains the build into 2 (`Follows` p1); p3 scans into 2; p4 stores 2 into 0; p5 reads 0. The Append's two arms
    /// start at the drain (p2..p3), its first the seeded run p1..p2: one production, so the three take one number and p4
    /// reads it (the seeded run's own protocol alone gave p1..p2 one number and p3 another, p4 reading p3's only).
    #[test]
    fn a_seeded_run_inside_an_append_run_takes_one_number() {
        use gravity_abi::breaker::{Breaker, BreakerKind, Budget, Spill};
        use gravity_program::protos::{Proto, ProtoKind};
        use gravity_program::{Arena, End, LifeId, Pipeline};
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let br = |slot: u8| End::Breaker(Breaker { kind: BreakerKind::HashAgg, slot, bytes: 0, budget: Budget::Share, spill: Spill::None });
        let mut ps = [
            Pipeline::eligible(-1, br(0), 0),
            Pipeline::eligible(-1, br(2), 0).consuming_from_in(a, 0, LifeId(0)),
            Pipeline::eligible(-1, br(2), 0).consuming_from_in(a, 0, LifeId(0)),
            Pipeline::eligible(-1, br(2), 0),
            Pipeline::eligible(-1, br(0), 0).consuming_from_in(a, 2, LifeId(3)),
            Pipeline::eligible(-1, End::Sink, 0).consuming_from_in(a, 0, LifeId(4)),
        ];
        let mut protos = [Proto::new(ProtoKind::Follows, 2).peer(1), Proto::new(ProtoKind::Arms, 1).slot(2).arg(2), Proto::new(ProtoKind::Arms, 2).slot(2).arg(2)];
        let deferred = [true, true, true, true, true, false];
        let rig = mcx::MemoryContext::new("test");
        super::renumber_deferred(a, &deferred, &mut ps, &mut protos, -1, rig.mcx());
        let out = |p: &Pipeline| p.produces();
        assert_eq!((out(&ps[1]), out(&ps[2]), out(&ps[3]), ps[4].consumes), (Some(3), Some(3), Some(3), &[3u8][..]));
        assert_eq!((out(&ps[0]), ps[1].consumes, ps[2].consumes, out(&ps[4]), ps[5].consumes), (Some(1), &[1u8][..], &[1u8][..], Some(2), &[2u8][..]));
        assert_eq!((protos[1].slot, protos[2].slot), (3, 3));
    }

    /// D3511: an Init's members (all deferred): p0 builds 0; p1 probes it, the seeded first arm into 0 (decompose freed the
    /// build's number at its last reader, the probe: D1188's case); p2 drains into 0 (`Follows` p1); p3 reads the run. The
    /// build's rename renames the probe's read, not the probe's own `Arms` run, so the run's two arms take one number
    /// and p3 reads it (the renamed `Arms` left p1 and p2 two numbers, p3 reading the drain's alone).
    #[test]
    fn a_probe_reading_the_number_its_run_reuses_keeps_its_arms() {
        use gravity_abi::breaker::{Breaker, BreakerKind, Budget, Spill};
        use gravity_program::protos::{Proto, ProtoKind};
        use gravity_program::{Arena, End, LifeId, Pipeline};
        let arena = Arena::new(1 << 16);
        let a = arena.handle();
        let br = |slot: u8| End::Breaker(Breaker { kind: BreakerKind::HashAgg, slot, bytes: 0, budget: Budget::Share, spill: Spill::None });
        let mut ps = [
            Pipeline::eligible(-1, br(0), 0),
            Pipeline::eligible(-1, br(0), 0).consuming_from_in(a, 0, LifeId(0)),
            Pipeline::eligible(-1, br(0), 0),
            Pipeline::eligible(-1, End::Sink, 0).consuming_from_in(a, 0, LifeId(2)),
        ];
        let mut protos = [Proto::new(ProtoKind::Follows, 2).peer(1), Proto::new(ProtoKind::Arms, 1).slot(0).arg(2)];
        let deferred = [true, true, true, false];
        let rig = mcx::MemoryContext::new("test");
        super::renumber_deferred(a, &deferred, &mut ps, &mut protos, -1, rig.mcx());
        let out = |p: &Pipeline| p.produces();
        assert_eq!((out(&ps[0]), ps[1].consumes), (Some(1), &[1u8][..]));
        assert_eq!((out(&ps[1]), out(&ps[2]), ps[3].consumes, protos[1].slot), (Some(2), Some(2), &[2u8][..], 2));
    }
}
