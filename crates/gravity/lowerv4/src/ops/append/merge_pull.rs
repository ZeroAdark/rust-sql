//! MergeAppend's pull form (V4R-143, D478; a helper of runs/mergeappend.rs): under the reader's LIMIT over index arms,
//! PostgreSQL's merge pulls each arm one row at a time, so it reads about `count + offset` rows in all. The breaker form
//! reads that many from every arm. Here the reader's source is the merge itself: each arm's index source (made a second
//! time over the site its own pipeline numbered) and its ops before the Feed run inside the reader, one row per arm is
//! held as its merge keys' `[flags, word]` pairs (runs/keylane.rs `pairs`: D443's NULL placement) and its columns' datum
//! words in frame words, and each window is the row at the top of PostgreSQL's binary heap over the arms
//! (merge_pull_heap.rs, D2884: its tie order too). Only that arm then reads its next row, at the next window. The arm pipelines run nothing: their end (`Fused`) leaves at the open. The
//! form is serial (an index arm is), holds no memory but a text column's copies, and needs props `merge_pull`: every arm
//! an index scan whose ops before its Feed are filters and projections (which may raise: PostgreSQL's rows, D449), every
//! key by value or a text key (its held copy compared past its prefix word: runs/merge_pull_cmp.rs, V4R-458), another
//! column by value or a varlena (its held copy: runs/merge_pull_copy.rs). Any arm count whose words fit. Budget 400 lines.
//! Owner M3-MERGE.
use gravity_decompose::ops::shared::append::AppendDesc;
use gravity_decompose::catalog::typefacts::TypeFacts;
use gravity_decompose::Op;
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_abi::status::raise;
use gravity_ir::{Block, ConvOp, HostFlags, IntBinOp, IntCond, Type, Value};
use gravity_kernels::host_abi::m11_run_held;
use gravity_program::protos::ProtoKind;
use gravity_program::{NumTypmod, Rep, Until};
use mcx::{CollectIn, GrowIn, Mcx, PgBox, PgVec};

use crate::cell::{At, Cell, CellHome, CellLife, Nullable, Row};
use crate::face::end::EndMech;
use crate::face::kinds::Face;
use crate::face::source::{SourceMech, WinConds};
use crate::face::step::StepMech;
use crate::frame::env::Env;
use crate::frame::frame::{SourceMaker, StepMaker, Win};
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::{Px, WordKey};
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::frame::iface::kind_impl;
use crate::props::Merge;
use crate::admit::refusal::Reason;

use crate::rep::keylane::Key;
use crate::ops::append::merge_pull_copy::Buf;
use crate::ops::append::merge_pull_heap::Heap;
use crate::face::runs::{Form, Shape};

fn bug(node: i32, payload: &'static str) -> ! {
    crate::refuse!(node, Reason::Bug { payload })
}

/// An arm pipeline of a pull-form merge: it reads nothing (the reader pulls its rows), so its function leaves at the open.
pub(crate) struct Fused;

impl EndMech for Fused {
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        b.br(env.x.l_done, &[]);
        let dead = b.create_block(&[]);
        b.switch_to(dead);
    }

    fn lower<'e>(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
        row.carry.copy_in(env.mcx)
    }
}

/// A pull-form arm's own source: it reads nothing (the reader pulls the arm's rows through the arm's site). Its row, in
/// dead code only, carries the relation's columns as NULL cells so the arm's ops still lower.
pub(crate) struct Empty<'g> {
    cols: PgVec<'g, (u32, Rep)>,
}

/// Pipeline `k`'s empty source (a pull-form arm's: graph.rs maps its source to runs.mergeappend).
pub(crate) fn empty<'g>(px: &Px<'g, '_>, k: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    let gx = px.gx;
    let mcx = px.mcx();
    let p = &gx.pipelines()[k];
    // a Sort arm's row is its full sort's target list
    if let gravity_decompose::Source::Breaker { slot } = p.source {
        let Some(l) = gx.life_of(k, slot) else { bug(p.node_id, "a pull-form run arm with no producer") };
        let cols = crate::frame::iface::sort_cols(px, l.0 as usize).iter().map(|n| {
            let typ = n.as_target_entry().map_or(types_core::INT8OID, |te| gravity_decompose::catalog::expr_type::type_of(te.expr).unwrap_or_else(|| gravity_decompose::catalog::expr_type::result_type(te.expr)));
            match gravity_decompose::catalog::typefacts::facts_of(gx.facts, typ) {
                Some(f) if f.byval => (typ, Rep::Word { bytes: f.typlen.clamp(1, 8) as u8 }),
                Some(f) if f.typlen > 0 => (typ, Rep::FixedPtr { len: f.typlen as u16 }),
                Some(_) => (typ, Rep::VarlenaPtr),
                None => (types_core::INT8OID, Rep::Word { bytes: 8 }),
            }
        }).collect_in(mcx);
        return mcx::pg_box_dyn!(mcx, Empty { cols } => dyn SourceMech + 'g);
    }
    let Some((_, relid, _)) = p.source.relation() else { bug(p.node_id, "a pull-form arm without its relation") };
    let natts = gx.facts.rel(relid).map_or(0, |r| r.natts.max(0) as usize);
    let cols = (1..=natts as i16).map(|a| match gx.facts.att(relid, a) {
        Some(f) if f.byval => (f.typid, Rep::Word { bytes: f.typlen.clamp(1, 8) as u8 }),
        Some(f) if f.typlen > 0 => (f.typid, Rep::FixedPtr { len: f.typlen as u16 }),
        Some(f) => (f.typid, Rep::VarlenaPtr),
        None => (types_core::INT8OID, Rep::Word { bytes: 8 }),
    }).collect_in(mcx);
    mcx::pg_box_dyn!(mcx, Empty { cols } => dyn SourceMech + 'g)
}

impl SourceMech for Empty<'_> {
    fn open(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        env.zero
    }

    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        b.br(l_eof, &[env.zero]);
        let dead = b.create_block(&[]);
        b.switch_to(dead);
        Win { cursor: env.zero, end: env.zero, facts: WinConds::default() }
    }

    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        let life = CellLife { until: Until::Row, home: CellHome::FrameSlot };
        let cells = self.cols.iter().enumerate().map(|(c, &(typid, rep))| Cell { rep, typid, num: NumTypmod::NONE, life, at: At::Slot(c as u16), null: Nullable::Maybe }).collect_in(env.mcx);
        let t = b.bconst(true);
        let vals = self.cols.iter().map(|_| (env.zero, Some(t))).collect_in(env.mcx);
        RowVals { row: Row { cells }, cols: (0..self.cols.len() as u16).map(Some).collect_in(env.mcx), vals, carry, next, at: j }
    }
}

/// One arm as the reader pulls it: its source over its own numbered site (a run arm: sort's cursor over the reader's consumed
/// run, no site), its ops before the Feed, and its frame words
/// (byte offsets into the words site): `need` (read a row at the window after an open), `alive` (a row is held), each
/// column's datum and null word, each key's pair.
struct Arm<'g> {
    src: PgBox<'g, dyn SourceMech + 'g>,
    steps: PgVec<'g, PgBox<'g, dyn StepMech + 'g>>,
    site: Option<usize>,
    /// Its src.prune skip bit when the merge prunes (D509): a pruned arm is neither opened nor read.
    skip: Option<(u16, u32)>,
    w_need: i32,
    w_alive: i32,
    w_vals: PgVec<'g, i32>,
    w_nulls: PgVec<'g, i32>,
    w_pairs: PgVec<'g, i32>,
    /// Each copied column's buffer (`vars`' order: a varlena's or a fixed image's): runs/merge_pull_copy.rs.
    bufs: PgVec<'g, Buf>,
    /// D449: a run arm whose Sort may hold its fill's error (op 14 for runs.sort): the reader's consumed index of the run,
    /// whose end raises it (`m11_run_held`).
    held: Option<u64>,
}

/// The reader's source: the arms, the merge row's shape, and the out row's words (`w_left`: the window's rows the frame
/// has not finished, 0 or 1: a stop before the row re-emits it; `w_last`: the arm that emitted the last window plus one,
/// the one arm to read, the heap's top; 0 after an open: every arm that `need`s a row, then the heap built).
pub(crate) struct Pull<'g> {
    arms: PgVec<'g, Arm<'g>>,
    keys: Vec<Key>,
    tfs: PgVec<'g, TypeFacts>,
    node: i32,
    w_left: i32,
    w_last: i32,
    w_out: PgVec<'g, i32>,
    w_outn: PgVec<'g, i32>,
    /// The varlena and fixed-image columns, whose held words are copies (V4R-305).
    vars: PgVec<'g, usize>,
    /// PostgreSQL's binary heap over the arms (merge_pull_heap.rs).
    heap: Heap<'g>,
    /// The nest.prune handle word of a pruning decided at every re-open (ops/append/prune.rs `reopen`).
    reopen: Option<u16>,
}

/// The reader `k` of merge `m` in the pull form, the merge row planned by runs/sort.rs over `cols` (the last arm's).
pub(crate) fn make<'g>(px: &mut Px<'g, '_>, k: usize, m: &Merge<'_>, cols: &types_nodes::list::NodeList<'_>) -> PgBox<'g, dyn SourceMech + 'g> {
    let gx = px.gx;
    let mcx = px.mcx();
    let node = gx.pipelines()[k].node_id;
    let shape = Shape::plan(px, node, m.keys, None, cols);
    // V4R-458: a text key's var tail is compared over its held copy (runs/merge_pull_cmp.rs); no other tail is pulled
    let var_tail = |k: &Key| k.tail == 0 || k.tail >> 8 & 0x7f == gravity_kernels::host_abi::VAR_WIDTH;
    if !shape.keys.iter().all(var_tail) || shape.cols.iter().any(|c| !matches!(c.form, Form::Word | Form::Var { .. } | Form::Image { .. })) {
        bug(node, "a pull-form merge whose row is not words, varlenas and fixed images (props merge_pull)");
    }
    // the columns whose held word is a copy: a varlena's, and a fixed-width by-reference image's (a name non-key column)
    let vars = shape.cols.iter().enumerate().filter(|(_, c)| matches!(c.form, Form::Var { .. } | Form::Image { .. })).map(|(c, _)| c).collect_in(mcx);
    let word = |px: &mut Px<'_, '_>, j: usize, i: usize| px.frame_word(WordKey::Open { face: Face::Runs, k, j: crate::frame::px::key_j(usize::MAX - 1 - j), i: i as u8 }) as i32 * 8;
    let n = shape.cols.len();
    let w_left = word(px, 0, 0);
    let w_out = (0..n).map(|c| word(px, 0, 1 + c)).collect_in(mcx);
    let w_outn = (0..n).map(|c| word(px, 0, 1 + n + c)).collect_in(mcx);
    let w_last = word(px, 0, 1 + 2 * n);
    let prune = crate::ops::append::merge::prune_of(gx, k);
    // a reader no rerun restarts decides its pruning at every re-open (D2013's road), its arms then carry no skip bit
    let reopen = prune.and_then(|pr| crate::ops::append::prune::reopen(px, k, pr, m.arms.len()));
    let mut arms: PgVec<'g, Arm<'g>> = mcx::reserved_in(m.arms.len(), mcx);
    // every arm's words before any arm's source is made (which takes words of its own): one stride apart, so the heap reads
    // an arm's held row at a computed address (merge_pull_heap.rs)
    let per = 2 + 2 * n + 2 * shape.keys.len() + 2 * vars.len();
    let aw = (0..m.arms.len()).map(|a| (0..per).map(|i| word(px, a + 1, i)).collect_in(mcx)).collect_in(mcx);
    if aw.iter().enumerate().any(|(a, w)| w.iter().enumerate().any(|(i, &x)| x != aw[0][0] + 8 * (a * per + i) as i32)) {
        bug(node, "a pull-form merge whose arms' words are not one stride apart");
    }
    // a Sort arm's run slot is consumed by this reader past its own consumes, in arm order (merge_pull_runs.rs `rehome`)
    let mut runs = gx.pipelines()[k].desc.consumes.len();
    for (a, &q) in m.arms.iter().enumerate() {
        let p = &gx.pipelines()[q];
        let feed = p.ops.iter().position(|op| matches!(op, Op::Append(AppendDesc::Feed { .. }))).unwrap_or_else(|| bug(p.node_id, "a MergeAppend arm without its feed"));
        let run = match p.source {
            gravity_decompose::Source::Breaker { slot } => {
                let Some(l) = gx.life_of(q, slot) else { bug(p.node_id, "a pull-form run arm with no producer") };
                runs += 1;
                Some((l.0 as usize, slot, runs - 1))
            }
            _ => None,
        };
        // the arm's source made again over the same relation and index: its draft is the arm's own, already numbered
        let (src, steps) = px.making(q, q, |px| {
            let src = match run {
                Some((producer, slot, i)) => crate::frame::iface::sort_cursor(px, q, producer, slot, i),
                None => (kind_impl::<SourceMaker>(&p.node_id, crate::graph::source_kind_of(&p.source, p.node_id)).0)(px, q),
            };
            let steps: PgVec<'g, PgBox<'g, dyn StepMech + 'g>> = (0..feed).map(|j| (kind_impl::<StepMaker>(&p.node_id, gx.step_need(q, j)).0)(px, q, j)).collect_in(mcx);
            (src, steps)
        });
        // D449: a Sort input that may raise holds its error until this reader first reads the run's end, PostgreSQL's
        // first pull of the arm (merge_admit.rs `merge_pull`: `held_fill`)
        let held = run.filter(|&(producer, ..)| crate::effects::visible::merge::ops_can_raise(gx, &gx.pipelines()[producer].ops)).map(|(producer, _, i)| {
            if !px.held_sort.contains(&producer) {
                px.held_sort.push(producer); // alloc: Program
            }
            i as u64
        });
        // the arm's own pipeline has no source (graph.rs maps a pull arm's source to this kind): the reader numbers its site
        let site = match run {
            Some(_) => None,
            None => {
                let Some(draft) = px.take_site(q) else { bug(p.node_id, "a pull-form arm's source drafted no site") };
                Some(px.add_site(draft))
            }
        };
        let skip = prune.filter(|_| reopen.is_none()).map(|pr| (kind_impl::<crate::face::source::PruneImpl>(&node, crate::face::kinds::KindId::SrcPrune).0)(px, q, a as u16, m.arms.len() as u16, pr, node));
        let w = &aw[a];
        let (w_need, w_alive) = (w[0], w[1]);
        let w_vals = mcx::to_vec_in(&w[2..2 + n], mcx);
        let w_nulls = mcx::to_vec_in(&w[2 + n..2 + 2 * n], mcx);
        let at = 2 + 2 * n + 2 * shape.keys.len();
        let w_pairs = mcx::to_vec_in(&w[2 + 2 * n..at], mcx);
        let bufs = (0..vars.len()).map(|v| Buf { w_ptr: w[at + 2 * v], w_cap: w[at + 2 * v + 1] }).collect_in(mcx);
        arms.push_in(Arm { src, steps, site, skip, w_need, w_alive, w_vals, w_nulls, w_pairs, bufs, held });
    }
    // a late or rerun pruning lays the skip words as an image at a pipeline's start (exec prune_exec.rs, rerun/mod.rs):
    // the arms' own pipelines run nothing here, so their Prune protos name this reader, whose start takes the image too
    if prune.is_some() && reopen.is_none() {
        let reader = px.ident(k) as u16;
        px.protos.iter_mut().filter(|q| q.kind == ProtoKind::Prune && m.arms.iter().any(|&a| a as u16 == q.pipeline)).for_each(|q| q.peer = reader);
    }
    let tfs = shape.cols.iter().map(|c| c.tf).collect_in(mcx);
    // the heap's words follow the arms' (keyed past the last arm): its size, then one word per position, contiguous
    let first = m.arms.len() + 1;
    let size = word(px, first, 0);
    let hw = (0..m.arms.len()).map(|p| word(px, first + 1 + p / 256, (p % 256) as usize)).collect_in(mcx);
    if hw.iter().enumerate().any(|(p, &x)| x != hw[0] + 8 * p as i32) {
        bug(node, "a pull-form merge whose heap words are not contiguous");
    }
    let a0 = &arms[0];
    let heap = Heap { h: hw[0], size, n: m.arms.len(), stride: per as i32, alive: a0.w_alive, pairs: mcx::to_vec_in(&a0.w_pairs[..], mcx), vals: mcx::to_vec_in(&a0.w_vals[..], mcx), nulls: mcx::to_vec_in(&a0.w_nulls[..], mcx) };
    mcx::pg_box_dyn!(mcx, Pull { arms, keys: shape.keys, tfs, node, w_left, w_last, w_out, w_outn, vars, heap, reopen } => dyn SourceMech + 'g)
}

/// The merge row over datum words `vals` (`(word, null)` per column), as runs/sort.rs's emit gives it (a varlena's until
/// the emitting arm's next row, which it reads at the next window).
fn word_row<'e>(tfs: &[TypeFacts], vals: PgVec<'e, (Value, Option<Value>)>, carry: Carry<'e>, next: Value, at: Value, mcx: Mcx<'e>) -> RowVals<'e> {
    let cells = tfs.iter().enumerate().map(|(c, tf)| {
        let life = CellLife { until: Until::Row, home: CellHome::FrameSlot };
        Cell { rep: Rep::of_storage(tf.typlen, tf.byval), typid: tf.typid, num: NumTypmod::NONE, life, at: At::Slot(c as u16), null: Nullable::Maybe }
    }).collect_in(mcx);
    RowVals { row: Row { cells }, cols: (0..tfs.len() as u16).map(Some).collect_in(mcx), vals, carry, next, at }
}

/// D449's held Sort arm at its run's end: 1 from `m11_run_held` (the error its fill held, stashed again for this thread)
/// leaves by the raise exit, where PostgreSQL's pull of the arm runs the Sort; 0 (nothing held) reads on.
fn raise_held(b: &mut FunctionBuilder<'_>, env: &mut Env, i: u64) {
    let flags = HostFlags { may_error: true, ..HostFlags::default() };
    let h = b.declare_host_once("m11_run_held", &[Type::Ptr, Type::I64], Some(Type::I64), gravity_abi::host_words!(m11_run_held(ctx, k)), flags);
    let i = b.iconst(Type::I64, i as i64);
    let st = b.call_host(h, &[env.ctx, i], Some(Type::I64)).expect("m11_run_held returns its answer");
    let (held, l_raise, l_on) = (b.icmp(IntCond::Ne, st, env.zero), b.create_block(&[]), b.create_block(&[]));
    b.cond_br(held, (l_raise, &[]), (l_on, &[]));
    b.switch_to(l_raise);
    b.mark_cold(l_raise);
    let e = b.iconst(Type::I64, raise::bit(raise::HOST) as i64);
    b.br(env.l_raise, &[e]);
    b.switch_to(l_on);
}

/// The chain of an arm's ops over `row`, `fin` at its end (the Feed's row).
fn chain<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, steps: &[PgBox<'_, dyn StepMech + '_>], row: &RowVals, fin: &mut dyn FnMut(&mut FunctionBuilder<'_>, &mut Env<'e, '_>, &RowVals) -> Carry<'e>) -> Carry<'e> {
    crate::stack_check(); // D3832
    match steps.split_first() {
        None => fin(b, env, row),
        Some((s, rest)) => s.lower(b, env, row, &mut |b, env, r| chain(b, env, rest, r, fin)),
    }
}

impl Pull<'_> {
    /// Arm `a`'s next row that passes its ops, held in its words (`alive`), or `alive` cleared at its end; a status below
    /// zero leaves through `l_eof`. The row read is consumed at once (the site's cursor past it): its words are the copy.
    fn fill(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: usize, l_eof: Block, l_next: Block) {
        let arm = &self.arms[a];
        let saved = env.site;
        if let Some(s) = arm.site {
            env.site = Some(s);
        }
        let l_end = b.create_block(&[Type::I64]);
        // a filtered row reads the next: the loop's back edge checks for cancel
        let (l_loop, _) = crate::frame::cancel::header(b, env.ctx, env.l_cancel, &[], env.mcx);
        b.br(l_loop, &[]);
        b.switch_to(l_loop);
        let win = arm.src.next_window(b, env, l_end);
        let row = arm.src.row(b, env, &win, win.cursor, Carry::new(env.zero, env.mcx), env.zero);
        let cur1 = b.int_bin(IntBinOp::Add, win.cursor, env.one);
        // a run arm's position is its own cursor word (sort's RunEmit), a scan arm's its site's
        match arm.site {
            Some(s) => crate::frame::stop::write_back(b, env.ctx, s, env.one, cur1),
            None => arm.src.write_back(b, env, cur1, env.zero),
        }
        let (node, tfs, keys) = (self.node, &self.tfs[..], &self.keys[..]);
        let c = chain(b, env, &arm.steps, &row, &mut |b, env, r| {
            let mut cx = env.lx(b, node);
            let vals: PgVec<'_, (Value, Value)> = tfs.iter().enumerate().map(|(c, tf)| {
                let Some(cr) = r.col(c as i16 + 1) else { bug(node, "a merge column past the arm's row") };
                crate::rep::datum(&mut cx, cr, tf)
            }).collect_in(env.mcx);
            let wr = word_row(tfs, vals.iter().map(|&(v, n)| (v, Some(n))).collect_in(env.mcx), Carry::new(env.zero, env.mcx), env.zero, env.zero, env.mcx);
            let pairs = crate::rep::keylane::pairs(&mut cx, node, &wr, keys, tfs, env.mcx);
            env.memo = cx.memo;
            for (c, &(v, null)) in vals.iter().enumerate() {
                let mut word = b.select(null, env.zero, v);
                if let Some(x) = self.vars.iter().position(|&y| y == c) {
                    word = match tfs[c].typlen {
                        len if len > 0 => crate::ops::append::merge_pull_copy::copy_fixed(b, env, arm.bufs[x], word, len as u32),
                        _ => crate::ops::append::merge_pull_copy::copy(b, env, arm.bufs[x], word),
                    };
                }
                env.store_word(b, arm.w_vals[c], word);
                let nw = b.conv(ConvOp::Zext, Type::I64, null);
                env.store_word(b, arm.w_nulls[c], nw);
            }
            for (i, w) in pairs.iter().flatten().enumerate() {
                env.store_word(b, arm.w_pairs[i], *w);
            }
            Carry::new(env.one, env.mcx)
        });
        let found = b.icmp(IntCond::Ne, c.out, env.zero);
        let l_held = b.create_block(&[]);
        b.cond_br(found, (l_held, &[]), (l_loop, &[]));
        b.switch_to(l_held);
        env.store_word(b, arm.w_alive, env.one);
        env.store_word(b, arm.w_need, env.zero);
        b.br(l_next, &[]);
        b.switch_to(l_end);
        let r = b.block_params(l_end)[0];
        let (bad, l_ended) = (b.icmp(IntCond::Slt, r, env.zero), b.create_block(&[]));
        b.cond_br(bad, (l_eof, &[r]), (l_ended, &[]));
        b.switch_to(l_ended);
        if let Some(i) = arm.held {
            raise_held(b, env, i);
        }
        env.store_word(b, arm.w_alive, env.zero);
        env.store_word(b, arm.w_need, env.zero);
        b.br(l_next, &[]);
        env.site = saved;
    }
}

impl SourceMech for Pull<'_> {
    fn declare(&mut self, pb: &mut ProgramBuilder) {
        self.arms.iter_mut().for_each(|a| a.src.declare(pb));
    }

    fn open_words(&self, w: &mut OpenWords) {
        w.declare(OpenWord::open("merge pull left", Base::Words, self.w_left, Type::I64, 0));
        w.declare(OpenWord::open("merge pull last", Base::Words, self.w_last, Type::I64, 0));
        for a in self.arms.iter() {
            w.declare(OpenWord::open("merge pull need", Base::Words, a.w_need, Type::I64, 1));
            w.declare(OpenWord::open("merge pull alive", Base::Words, a.w_alive, Type::I64, 0));
            crate::ops::append::merge_pull_copy::open_words(w, &a.bufs[..]);
            if a.site.is_none() {
                a.src.open_words(w); // a run arm's cursor (mode, cursor, total) starts afresh at each open
            }
        }
    }

    /// Every arm's open (its keys stored, its seek asked for); the first status below zero leaves. A pruned arm (D509) is
    /// not opened and needs no row, so it is never read: its skip bit, or its bit of the mask this re-open decides.
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        let saved = env.site;
        let mut st = env.zero;
        let decided = self.reopen.map(|h| {
            let m = crate::ops::append::prune::reopen_mask(b, env, h);
            (m, b.icmp(IntCond::Sge, m, env.zero))
        });
        if let Some((m, ok)) = decided {
            st = b.select(ok, env.zero, m);
        }
        for (i, a) in self.arms.iter().enumerate() {
            env.site = a.site.or(saved);
            let pass = match (decided, a.skip) {
                (Some((m, ok)), _) => {
                    let sh = b.iconst(Type::I64, i as i64);
                    let s = b.int_bin(IntBinOp::Ushr, m, sh);
                    let s1 = b.int_bin(IntBinOp::And, s, env.one);
                    let kept = b.icmp(IntCond::Eq, s1, env.zero);
                    Some(b.int_bin(IntBinOp::And, ok, kept))
                }
                (None, Some(skip)) => Some(crate::ops::append::merge::unpruned(b, env, skip)),
                (None, None) => None,
            };
            let Some(pass) = pass else {
                let r = a.src.open(b, env);
                let (bad, first) = (b.icmp(IntCond::Slt, r, env.zero), b.icmp(IntCond::Eq, st, env.zero));
                let take = b.int_bin(IntBinOp::And, bad, first);
                st = b.select(take, r, st);
                continue;
            };
            let (l_open, l_join) = (b.create_block(&[]), b.create_block(&[Type::I64]));
            b.cond_br(pass, (l_open, &[]), (l_join, &[st]));
            b.switch_to(l_open);
            let r = a.src.open(b, env);
            let (bad, first) = (b.icmp(IntCond::Slt, r, env.zero), b.icmp(IntCond::Eq, st, env.zero));
            let take = b.int_bin(IntBinOp::And, bad, first);
            let st2 = b.select(take, r, st);
            b.br(l_join, &[st2]);
            b.switch_to(l_join);
            st = b.block_params(l_join)[0];
            let need = b.conv(ConvOp::Zext, Type::I64, pass);
            env.store_word(b, a.w_need, need);
        }
        env.site = saved;
        st
    }

    /// A row the frame has not finished is its window again; else the arm that emitted the last window reads its next row
    /// and the heap settles over it (after an open: each arm that needs a row reads one, in arm order, and the heap is
    /// built), and the window is the heap's top, its arm then the one to read next. The arm's read is reached by a switch
    /// over `last`, so a window does no work per arm it does not read.
    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        let l_win = b.create_block(&[Type::I64]);
        let left = env.load_word(b, self.w_left);
        let (again, l_pop) = (b.icmp(IntCond::Sgt, left, env.zero), b.create_block(&[]));
        let back = b.int_bin(IntBinOp::Sub, env.one, left);
        b.cond_br(again, (l_win, &[back]), (l_pop, &[]));
        b.switch_to(l_pop);
        let last1 = env.load_word(b, self.w_last);
        let all = b.icmp(IntCond::Eq, last1, env.zero);
        let last = b.int_bin(IntBinOp::Sub, last1, env.one);
        let arms = self.arms.len();
        let mut chk: PgVec<'_, Block> = mcx::reserved_in(arms, env.mcx);
        let mut fills: PgVec<'_, Block> = mcx::reserved_in(arms, env.mcx);
        for _ in 0..arms {
            chk.push_in(b.create_block(&[]));
            fills.push_in(b.create_block(&[]));
        }
        let (l_one, l_pick) = (b.create_block(&[]), b.create_block(&[]));
        b.cond_br(all, (chk[0], &[]), (l_one, &[]));
        b.switch_to(l_one);
        b.switch(last, fills.iter().enumerate().map(|(a, &t)| (a as u64, t, [])), (l_pick, &[]));
        for a in 0..arms {
            b.switch_to(chk[a]);
            let l_cont = b.create_block(&[]);
            let need = env.load_word(b, self.arms[a].w_need);
            let go = b.icmp(IntCond::Ne, need, env.zero);
            b.cond_br(go, (fills[a], &[]), (l_cont, &[]));
            b.switch_to(fills[a]);
            self.fill(b, env, a, l_eof, l_cont);
            b.switch_to(l_cont);
            let next = chk.get(a + 1).copied().unwrap_or(l_pick);
            b.cond_br(all, (next, &[]), (l_pick, &[]));
        }
        b.switch_to(l_pick);
        // PostgreSQL's heap over the held rows: built after an open, else settled over the arm just read
        let (l_build, l_settle, l_top) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        b.cond_br(all, (l_build, &[]), (l_settle, &[]));
        b.switch_to(l_build);
        self.heap.build(b, env, &self.keys);
        b.br(l_top, &[]);
        b.switch_to(l_settle);
        self.heap.settle(b, env, &self.keys);
        b.br(l_top, &[]);
        b.switch_to(l_top);
        let n = self.tfs.len();
        let best = self.heap.top(b, env);
        let (none, l_emit) = (b.icmp(IntCond::Slt, best, env.zero), b.create_block(&[]));
        b.cond_br(none, (l_eof, &[env.zero]), (l_emit, &[]));
        b.switch_to(l_emit);
        let bv = self.heap.cols(b, env, best);
        for c in 0..n {
            env.store_word(b, self.w_out[c], bv[c]);
            env.store_word(b, self.w_outn[c], bv[n + c]);
        }
        let best1 = b.int_bin(IntBinOp::Add, best, env.one);
        env.store_word(b, self.w_last, best1);
        b.br(l_win, &[env.zero]);
        b.switch_to(l_win);
        let cursor = b.block_params(l_win)[0];
        Win { cursor, end: env.one, facts: WinConds::default() }
    }

    /// The window's row: the out words.
    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        let mcx = env.mcx;
        let vals = (0..self.tfs.len()).map(|c| {
            let v = env.load_word(b, self.w_out[c]);
            let nw = env.load_word(b, self.w_outn[c]);
            (v, Some(b.icmp(IntCond::Ne, nw, env.zero)))
        }).collect_in(mcx);
        word_row(&self.tfs[..], vals, carry, next, j, mcx)
    }

    fn write_back(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, cur: Value, _out: Value) {
        let left = b.int_bin(IntBinOp::Sub, env.one, cur);
        env.store_word(b, self.w_left, left);
    }
}
