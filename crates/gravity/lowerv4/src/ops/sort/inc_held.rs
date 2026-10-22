//! D2316 (op 14 "held at len" for runs.incremental): an unbounded Incremental Sort whose input may raise, under a merge
//! join that may stop reading it (decompose ops/sort `lazy_ahead` marks it `held_incs` in the plan-order run).
//! PostgreSQL's Incremental Sort loads its input one presorted batch at a time (nodeIncrementalSort.c) and hands a batch's
//! rows on before the next load reads on, so a raising row past the rows its merge reads is never read. Here the fill
//! reads every input row first, in PostgreSQL's order, and counts PostgreSQL's batches as it goes (inc_batches.rs
//! `Batches` under `rows`: the rows they handed on). When a row raises, the fill's merge holds the error with the rows
//! the fill stored (the program's `held_at`, exec ops/sort/held.rs `stamp_at`), the emit's open seals them (kernels
//! `begin_in`), and the reader reads the handed rows, the run's first rows in the full order (whole closed prefix
//! groups, which order before every later row), raising the held error when it reads past them (kernels
//! `sort_held_end`), where PostgreSQL's next load reads the raising row. A fill that ends hands every row on.
//! Exact where the fill's rows come in PostgreSQL's order and nothing it does shows but its error (D1108's
//! `lazy_exact` child: a heap scan, filters and projections of builtin calls reading no executor parameter, run on the
//! leader), its prefix keys are exact order words of fixed rows (a group closes where PostgreSQL's prefix compare closes
//! it), and the run's one reader is a graph pipeline reading it as its source into a merge join PostgreSQL surely runs
//! (mergejoin's `lazy_outer::reached`, asked through frame::iface `rows_reached`), only filters and projections
//! between, so the merge's machine reads the run's rows where nodeMergejoin fetches its outer tuples. Neither pipeline
//! is in a rerun set or an InitPlan's, no stop is handed to the sort, and the statement neither writes nor locks. A
//! marked sort that is not this form refuses by D2316's name (decompose `LAZY_AHEAD`); a fill that put runs before it
//! raised (its rows past the run's room) is UNSUPPORTED at its reader's open. Owner M3-MERGE.
use gravity_abi::status::raise;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::sort::SortForm;
use gravity_decompose::{Op, Source};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{HostFlags, IntCond, Type};
use gravity_program::LifeId;
use mcx::{CollectIn, PgVec};

use crate::admit::refusal::{Reason, ShapeWhy};
use crate::face::kinds::Face;
use crate::face::runs::Shape;
use crate::frame::env::Env;
use crate::frame::px::{runs_word, Px, WordKey};
use crate::graph::Gx;

/// Whether graph pipeline `k`'s Incremental Sort is marked for the held form (decompose `held_incs`).
fn marked(gx: &Gx<'_, '_>, k: usize) -> bool {
    k < gx.pipelines().len() && gx.pipelines()[k].node_ops.iter().any(|o| gx.graph.held_incs.contains(&o.node_id))
}

/// The one pipeline reading producer `k`'s run when `k` takes the held form (the module's rule, its shape aside:
/// `producer` checks that); None otherwise. The producer's end and its reader's emit both ask it.
pub(crate) fn reader(gx: &Gx<'_, '_>, k: usize) -> Option<usize> {
    if !marked(gx, k) || gx.graph.acts {
        return None;
    }
    let ps = gx.pipelines();
    let p = &ps[k];
    let (Op::Sort(sd), before) = p.ops.split_last()? else { return None };
    if !matches!(sd.form, SortForm::Runs) || sd.presorted == 0 || p.desc.finish || !before.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_))) {
        return None;
    }
    let stopped = gx.graph.stops.iter().any(|s| p.node_ops.iter().any(|o| o.node_id == s.sort));
    if stopped || ps.iter().any(crate::lifetime::material::locks) || !crate::frame::iface::run_holds_rows(gx, k) {
        return None;
    }
    if !crate::rerun::spool::form::lazy_exact(gx, &p.source, &p.desc.consumes, before) {
        return None;
    }
    // the run's one reader: a graph pipeline reading it as its source (no other pipeline, no embedded inner, reads it)
    let life = Some(LifeId(k as u16));
    let reads = |q: usize| {
        let src = if let Source::Breaker { slot } = ps[q].source { Some(slot) } else { None };
        ps[q].desc.consumes.iter().copied().chain(src).any(|s| gx.life_of(q, s) == life)
    };
    let readers: PgVec<'_, usize> = (0..ps.len() + ps.inners().len()).filter(|&q| q != k && reads(q)).collect_in(gx.mcx);
    let &[r] = &readers[..] else { return None };
    if r >= ps.len() || !matches!(ps[r].source, Source::Breaker { slot } if gx.life_of(r, slot) == life) {
        return None;
    }
    let ops = &ps[r].ops;
    let m = ops.iter().position(|op| matches!(op, Op::Join(JoinDesc::Merge { .. })))?;
    let rerun = |x: usize| gx.graph.reruns.iter().any(|s| s.pipelines.contains(&(x as u16)));
    let init = |x: usize| gx.graph.init_plans.iter().any(|i| i.pipeline as usize == x);
    let plain = ops[..m].iter().all(|op| matches!(op, Op::Filter(_) | Op::Project(_)));
    (plain && !rerun(k) && !rerun(r) && !init(k) && !init(r) && crate::frame::iface::rows_reached(gx, r, m + 1)).then_some(r)
}

/// Producer `k`'s held form over its run of `shape` (`pre` prefix keys; `stopped`: a stop is handed to the sort): the held
/// error's licence when taken (the pipeline marked `held_sort` and `held_at`, run on the leader: the fill reads its whole
/// input before the batches, its error held to the batch PostgreSQL would raise it in); a marked sort that cannot take it
/// refuses by D2316's name; None for an unmarked one.
pub(crate) fn producer(px: &mut Px<'_, '_>, k: usize, shape: &Shape, pre: usize, stopped: bool) -> Option<gravity_decompose::effects::Licence> {
    if !marked(px.gx, k) {
        return None;
    }
    // a prefix group closes where PostgreSQL's prefix compare closes it: one exact order word per key (no float word,
    // whose -0 and 0 PostgreSQL's equality joins), fixed rows
    let exact = pre > 0 && pre <= shape.keys.len() && !shape.var() && shape.keys[..pre].iter().all(|key| {
        let tf = &shape.cols[key.col as usize].tf;
        tf.base != crate::rep::keylane::NAME && tf.image.is_word()
    });
    // `reader`: the statement acts nowhere and the fill's code calls no user or VOLATILE function, so it may raise and does
    // nothing else
    let held = gravity_decompose::effects::held_error(gravity_decompose::effects::Effects::RAISES, false, None).ok();
    let Some(held) = held.filter(|_| !stopped && exact && reader(px.gx, k).is_some()) else {
        crate::refuse!(px.gx.pipelines()[k].node_id, Reason::Shape { why: ShapeWhy(gravity_decompose::ops::sort::LAZY_AHEAD) })
    };
    for v in [&mut px.held_sort, &mut px.held_at, &mut px.serial] {
        if !v.contains(&k) {
            v.push(k); // alloc: Program
        }
    }
    // construct C2 (b): the fill runs ahead of PostgreSQL's batches with its error held under this licence: the run-ahead
    // tracer reads it (an error held for PostgreSQL's point is no licence by itself)
    px.hold(k, &held);
    Some(held)
}

/// The held reader's words: the producer's rows-handed word (`Batches::w_out`, a byte offset in the frame words) and the
/// region its end probes (a consumed index; `u64::MAX` the reader's own region).
#[derive(Clone, Copy)]
pub(crate) struct Read {
    w_out: i32,
    at: u64,
}

/// Pipeline `k`'s emit over producer `producer`'s run (read at consumed index `at`, or its own region) as the held
/// reader, run on the leader; None when `producer` does not take the held form for `k`.
pub(crate) fn read(px: &mut Px<'_, '_>, k: usize, producer: usize, at: Option<u64>) -> Option<Read> {
    if reader(px.gx, producer) != Some(k) {
        return None;
    }
    if !px.serial.contains(&k) {
        px.serial.push(k); // alloc: Program
    }
    let w_out = px.frame_word(WordKey::Open { face: Face::Runs, k: producer, j: u64::MAX, i: runs_word::INC_OUT }) as i32 * 8;
    Some(Read { w_out, at: at.unwrap_or(u64::MAX) })
}

impl Read {
    /// After the emit's open: its rows end at the rows the producer's batches handed on (every row once its fill ended).
    pub(crate) fn clamp(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, w_total: i32) {
        let (total, handed) = (env.load_word(b, w_total), env.load_word(b, self.w_out));
        let fewer = b.icmp(IntCond::Slt, handed, total);
        let rows = b.select(fewer, handed, total);
        env.store_word(b, w_total, rows);
    }

    /// At the emit's last row read: a held run's error raised (kernels `sort_held_end`: 1 leaves by the raise exit, as
    /// merge.join's held inner does), where PostgreSQL's next batch load reads the raising row; a run whose fill ended
    /// answers 0 and the reader ends.
    pub(crate) fn end(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        let flags = HostFlags { may_error: true, ..HostFlags::default() };
        let h = b.declare_host_once("sort_held_end", &[Type::Ptr, Type::I64], Some(Type::I64), gravity_abi::host_words!(gravity_kernels::host_abi::sort_held_end(ctx, k)), flags);
        let at = b.iconst(Type::I64, self.at as i64);
        let st = b.call_host(h, &[env.ctx, at], Some(Type::I64)).expect("sort_held_end returns its answer");
        let (held, l_raise, l_on) = (b.icmp(IntCond::Ne, st, env.zero), b.create_block(&[]), b.create_block(&[]));
        b.cond_br(held, (l_raise, &[]), (l_on, &[]));
        b.switch_to(l_raise);
        b.mark_cold(l_raise);
        let e = b.iconst(Type::I64, raise::bit(raise::HOST) as i64);
        b.br(env.l_raise, &[e]);
        b.switch_to(l_on);
    }
}
