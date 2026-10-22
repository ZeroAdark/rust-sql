//! INCSTREAM (rows lk-0038 lk-0044 lk-0050 lk-0057 lk-0065): an
//! unbounded Incremental Sort under a LockRows a counted LIMIT stops. PostgreSQL's LockRows passes no bound down
//! (`ExecSetTupleBound` stops at it), so its Incremental Sort loads one batch of presorted groups at a time and the Limit
//! stops pulling once LockRows handed on count + offset rows (more when SKIP LOCKED, a concurrent delete or a failed recheck
//! drops some). runs.incremental is a breaker: the driver ran its producer to the input's end before the lock pipeline
//! opened, so the index scan read every row (lk-0038: 2.0 ms against rowexec's 0.3).
//!
//! Here the sort's producer `k` is a pulled set (nest.cte's object: `ProtoKind::Cte` with the `SORT_STREAM` bit, exec
//! v4/m8_cte.rs, RT-2f) whose one reader is the lock pipeline `r`: the runner steps past `k` (its slot opened zeroed for
//! `r`), `r` reads the run's closed prefix groups in place (`SORT_MODE_MEM`, row by row as runs.sort's emit reads a sealed
//! run) and, past them, pulls `k` on for a budget of closed rows (the LIMIT's count + offset); `k`'s end stops resumably at
//! the first prefix group close past the budget (incremental.rs `IncEnd::boundary`), and its last call sorts the open group,
//! seals the run as it lies (`RUN_ORDERED`: nothing moves) and marks the stream ended. The rows `r` sees are the full sort
//! order, ties in any order, as before; only fewer input rows are read.
//!
//! Where (`form`, fail closed): `k` is a serial heap or index scan whose ops before the sort are filters and projections
//! none of which raises (effects `ops_raise_free`) and no VOLATILE call (effects `calls_visible`): the batches PostgreSQL
//! reads are then not visible, so the budget need not model them. It ends in the unbounded presorted sort over exact
//! prefix words (topk_rows.rs `Stop::of`'s rule), its slot read by `r` alone; `r` reads it as its source, then
//! projections, the lock, projections and a LIMIT whose bound the lock's node is asked (frame::iface: a constant count and
//! offset, no WITH TIES; PG's LockRows hands none on, so the sort's own request stays unbounded); neither is a rerun set's member nor an InitPlan's but `r` a CTE's plan, and `r`
//! ends in the sink unless it is that CTE's plan (so no runner recheck reruns `r` over a skipped `k`: the lock under a LIMIT
//! rechecks in the IR, lockrows 15b). A row out of prefix order leaves the statement UNSUPPORTED at run time, as `Batches`
//! does. NEW-43: a run that fills mid-stream (a prefix group past the run's room, a skewed key the estimate did not see)
//! falls back to the breaker form (`spilled`): the producer sorts its open group and puts the run as runs.incremental's
//! breaker does, puts its runs again at the next prefix group's close and stops at that group's first row (`stopped`), or
//! runs to its input's end; the reader opens the host merge of the runs and reads past the rows it already handed on,
//! which are all in closed prefix groups, so no tie crosses into the later rows and they are exactly the merge's first
//! rows. Past the merge's last row, the stream not ended, the reader drops the runs, arms the region again as an empty run
//! and pulls on: the producer streams from the row it stopped at, so a big group costs its own rows (PostgreSQL's
//! presorted-prefix mode reads it whole too) and the input past it is read as before. Rows handed on stay handed on,
//! nothing reruns. Every frame word is the node's own, so a recheck node (whose producers rerun whole, its budget word
//! zero) never pulls and never stops. C1 part 2 (`Pull::Cursor`): the same stream under a named cursor's root (decompose
//! `PipelineGraph::partial`: a FETCH, a PL/pgSQL FOR loop, a portal's Execute stops pulling where its count ends), its
//! reader's ops projections and a Unique's first-of-run. It pulls in batches as nodeIncrementalSort.c loads them (at least
//! DEFAULT_MIN_GROUP_SIZE rows, then to the prefix group's close), each batch twice the last up to half the run's room;
//! its producer stops at the row that opens the next group, taken off the run, so once the reader handed on every closed
//! row it empties the run (`RunEmit::recycle`) before it pulls again: a cursor read whole never fills the run unless one
//! batch does, and then falls back as above. A helper file (no rows). Owner lockrows.
use gravity_decompose::ops::shared::sort::SortForm;
use gravity_decompose::{Op, Source};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, IntBinOp, IntCond, Type, Value};
use gravity_program::cost::INC_CURSOR_BATCH_FIRST as CURSOR_BATCH;
use gravity_program::protos::{Proto, ProtoKind};
use gravity_program::{Parallel, Pipeline, Reason as Why};
use mcx::{Mcx, PgBox};

use crate::face::kinds::Face;
use crate::face::runs::Shape;
use crate::face::source::SourceMech;
use crate::frame::env::Env;
use crate::frame::frame::Win;
use crate::frame::iface::Bound;
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::{runs_word, Px, WordKey};
use crate::frame::row::{Carry, RowVals};
use crate::graph::EndKind;
use crate::ops::sort::lower::RunEmit;

/// The Cte proto's `arg` bit naming a streamed incremental sort's producer (exec v4/m8_cte.rs reads only nest/cte.rs's
/// DRAIN, bit 32; hashjoin/drain/spool_stream.rs's STREAM is bit 33).
const SORT_STREAM: u64 = 1 << 34;

/// NEW-50: the producer pipelines of `program`'s streamed Incremental Sorts (their run holds a prefix group at a time, not
/// every row): exec's grant ask sizes their run from the planner's largest prefix group (inputs/knobs/grant.rs).
pub fn stream_producers(program: &gravity_program::Program) -> impl Iterator<Item = usize> + '_ {
    program.v4.iter().flat_map(|d| d.protos.iter()).filter(|p| p.kind == ProtoKind::Cte && p.arg & SORT_STREAM != 0).map(|p| p.pipeline as usize)
}

/// The stream's frame words (byte offsets), keyed under producer `k`: the run's rows in closed prefix groups, the end flag
/// and the fallback flag (NEW-43, `spilled`; `k`'s per-open words), then the reader's pull: the closed rows it wants (its
/// budget), the closed count it pulled at and the rows it handed on before its last re-arm (NEW-43). A cursor's stream
/// (`cursor`) also: the producer's open group start (incremental.rs `w_start`) and the reader's last batch.
#[derive(Clone, Copy)]
pub(crate) struct Words {
    pub(crate) closed: i32,
    pub(crate) done: i32,
    pub(crate) spill: i32,
    budget: i32,
    base: i32,
    handed: i32,
    pub(crate) cursor: bool,
    start: i32,
    batch: i32,
}

pub(crate) fn words(px: &mut Px<'_, '_>, k: usize, pull: Pull) -> Words {
    let mut word = |i: u8| px.frame_word(WordKey::Open { face: Face::Runs, k, j: u64::MAX, i }) as i32 * 8;
    let (closed, done, spill) = (word(runs_word::INC_CLOSED), word(runs_word::INC_DONE), word(runs_word::INC_SPILL));
    let (budget, base, handed) = (word(runs_word::INC_WANT), word(runs_word::INC_BASE), word(runs_word::INC_HANDED));
    let cursor = pull == Pull::Cursor;
    let (start, batch) = if cursor { (word(runs_word::INC_START), word(runs_word::INC_ASKED)) } else { (0, 0) };
    Words { closed, done, spill, budget, base, handed, cursor, start, batch }
}

/// How the reader pulls (`form`): a lock's LIMIT count + offset, every pull's budget; or a cursor's batches.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pull {
    Lock(i64),
    Cursor,
}

/// Producer `k` streams into its one reader: `(r, pull)`, the reader and how it pulls (see the file's head; the shape's
/// exact prefix is `exact`'s, checked by each maker over its own `Shape`).
pub(crate) fn form(px: &Px<'_, '_>, k: usize) -> Option<(usize, Pull)> {
    let gx = px.gx;
    let ps = gx.pipelines();
    let p = &ps[k];
    let (Op::Sort(sd), before) = p.ops.split_last()? else { return None };
    if !matches!(sd.form, SortForm::Runs) || sd.presorted == 0 || !before.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_))) {
        return None;
    }
    if !matches!(p.source, Source::Index { .. } | Source::Range { .. }) || p.desc.finish || p.desc.is_eligible() {
        return None;
    }
    if !crate::effects::ops_raise_free(gx, before) || crate::effects::visible::calls_visible(gx, k).is_some() {
        return None;
    }
    // a stop a Unique or a sorted group above hands the sort (decompose facts/stream.rs) is incremental.rs's `Batches`, a
    // held sort (D2316) inc_held.rs's
    if gx.graph.stops.iter().any(|s| p.node_ops.iter().any(|o| o.node_id == s.sort)) || crate::ops::sort::inc_held::reader(gx, k).is_some() {
        return None;
    }
    // every pipeline (inner ones too) reading a slot `k` fills: one, the lock pipeline, reading it as its source
    let from_k = |q: usize, slot: u8| gx.life_of(q, slot).is_some_and(|l| l.0 as usize == k);
    let mut readers = (0..ps.len() + ps.inners().len()).filter(|&q| ps[q].desc.consumes.iter().any(|&s| from_k(q, s)));
    let Some(r) = readers.next() else { return None };
    if readers.next().is_some() {
        return None;
    }
    if r >= ps.len() || !matches!(ps[r].source, Source::Breaker { slot } if from_k(r, slot)) {
        return None;
    }
    let pull = match lock_bound(px, r) {
        Some(n) => Pull::Lock(n),
        None if cursor_reader(px, r) => Pull::Cursor,
        None => return None,
    };
    let rerun = |x: usize| gx.graph.reruns.iter().any(|s| s.pipelines.contains(&(x as u16)));
    let init = |x: usize| gx.graph.init_plans.iter().any(|i| i.pipeline as usize == x);
    let cte = gx.cte_write(r); // `r` is a CTE's plan (its InitPlan's link is the CTE's)
    if rerun(k) || rerun(r) || init(k) || (init(r) && !cte) || (!cte && gx.end_kind(r) != EndKind::Sink) {
        return None;
    }
    Some((r, pull))
}

/// C1 part 2: reader `r` is the statement's root pipeline under a partial reader (decompose `PipelineGraph::partial`, a
/// named cursor's FETCH) and hands each row it reads on (projections, a Unique's first-of-run) to its sink, where each
/// FETCH stops pulling.
fn cursor_reader(px: &Px<'_, '_>, r: usize) -> bool {
    let gx = px.gx;
    gx.graph.partial && r == gx.graph.root as usize && gx.pipelines()[r].ops.iter().all(|o| matches!(o, Op::Project(_) | Op::FirstOfRun(_)))
}

/// Reader `r`'s ops after its source are projections, the lock and projections up to the plan node above the lock's, in
/// `r`; that node's request of the lock's (frame::iface `pass_down`: a LIMIT's constant count + offset, no WITH TIES) is
/// the budget, at least one row.
fn lock_bound(px: &Px<'_, '_>, r: usize) -> Option<i64> {
    let p = &px.gx.pipelines()[r];
    let at = p.ops.iter().position(|o| !matches!(o, Op::Project(_)))?;
    if !matches!(p.ops[at], Op::Lock(_)) {
        return None;
    }
    let n = p.node_ops.iter().rposition(|x| x.first_op as usize <= at)?;
    let above = p.node_ops.get(n + 1)?.first_op as usize;
    if !p.ops.get(at + 1..above)?.iter().all(|o| matches!(o, Op::Project(_))) {
        return None;
    }
    match px.request(p.node_ops[n].node_id).bound {
        Bound::Rows(c) if c >= 1 => i64::try_from(c).ok(),
        _ => None,
    }
}

/// The prefix keys of `shape` (`pre` of them) are exact order words from one serial producer (topk_rows.rs `Stop::of`'s
/// rule, incremental.rs's `exact`): a closed group is a prefix group.
pub(crate) fn exact(shape: &Shape, pre: usize) -> bool {
    pre > 0
        && shape.keys[..pre].iter().all(|key| {
            let tf = &shape.cols[key.col as usize].tf;
            tf.base != crate::rep::keylane::NAME && (tf.image.is_word() || tf.image.is_float())
        })
}

/// Producer `k`'s pulled-set object (its handle word), declared once whichever maker asks first.
fn handle(px: &mut Px<'_, '_>, k: usize, r: usize) -> u16 {
    if let Some(p) = px.protos.iter().find(|p| p.kind == ProtoKind::Cte && p.pipeline as usize == k && p.arg & SORT_STREAM != 0) {
        return p.words.base;
    }
    let h = px.proto_words(1);
    px.declare_proto(Proto::new(ProtoKind::Cte, k as u16).peer(r as u16).words(h, 1).arg(SORT_STREAM));
    h
}

/// The producer's end armed: its words, its object declared.
pub(crate) fn arm(px: &mut Px<'_, '_>, k: usize, r: usize, pull: Pull) -> Words {
    handle(px, k, r);
    words(px, k, pull)
}

/// `INC_SPILL` once the producer fell back (`spilled`), and once it then stopped at a prefix group's close (`stopped`).
const FELL: i64 = 1;
const STOPPED: i64 = 2;

/// After a prefix group closed at run row `idx` (its rows sorted): the closed rows filed, and once the reader's pull has its
/// budget past the count it pulled at, a resumable stop after the row (`carry`, `next`). A zero budget (no reader's pull: a
/// recheck node's rerun) never stops. A stream that fell back (`spilled`) files nothing: under a pull it leaves through
/// `fell` (the caller's: its runs put and a stop at the row that opened the next group, `stopped`); a rerun reads on. A
/// cursor's stream stops through `at_row` (the caller's: the row taken off the run, the stop at it), so a stopped stream's
/// run holds closed rows only.
pub(crate) fn closed(b: &mut FunctionBuilder<'_>, env: &mut Env, w: Words, idx: Value, carry: &Carry, next: Value, fell: &mut dyn FnMut(&mut FunctionBuilder<'_>, &mut Env), at_row: Option<&mut dyn FnMut(&mut FunctionBuilder<'_>, &mut Env)>) {
    let [l_file, l_spill, l_fell, l_past] = [(); 4].map(|_| b.create_block(&[]));
    let sp = env.load_word(b, w.spill);
    let streaming = b.icmp(IntCond::Eq, sp, env.zero);
    b.cond_br(streaming, (l_file, &[]), (l_spill, &[]));
    b.switch_to(l_spill);
    let budget = env.load_word(b, w.budget);
    let pulled = b.icmp(IntCond::Ne, budget, env.zero);
    b.cond_br(pulled, (l_fell, &[]), (l_past, &[]));
    b.switch_to(l_fell);
    fell(b, env);
    b.switch_to(l_file);
    env.store_word(b, w.closed, idx);
    let (budget, base) = (env.load_word(b, w.budget), env.load_word(b, w.base));
    let got = b.int_bin(IntBinOp::Sub, idx, base);
    let (taken, set) = (b.icmp(IntCond::Sge, got, budget), b.icmp(IntCond::Ne, budget, env.zero));
    let (full, l_stop, l_on) = (b.int_bin(IntBinOp::And, taken, set), b.create_block(&[]), b.create_block(&[]));
    b.cond_br(full, (l_stop, &[]), (l_on, &[]));
    b.switch_to(l_stop);
    match at_row {
        Some(f) => f(b, env),
        None => {
            let st = b.iconst(Type::I64, gravity_abi::status::suspend(false));
            env.stop(b, carry, next, st);
        }
    }
    b.switch_to(l_on);
    b.br(l_past, &[]);
    b.switch_to(l_past);
}

/// NEW-43: the producer's run is full (an open prefix group past its room): the stream falls back to the breaker form. The
/// caller then sorts the open group and puts the run (runs.incremental's full run); from here no group close is filed
/// (`closed`): the producer puts its runs at the next prefix group's close and stops there (`stopped`), or runs to its
/// end, whose finish puts the last run; the reader then reads the merge of the runs (`StreamEmit::next_window`).
pub(crate) fn spilled(b: &mut FunctionBuilder<'_>, env: &Env, w: Words) {
    let v = b.iconst(Type::I64, FELL);
    env.store_word(b, w.spill, v);
}

/// NEW-43: the fallen-back producer put its runs at a prefix group's close and stops at the row that opened the next group
/// (the caller's stop, the row re-delivered when its reader pulls again): every row of the closed groups is in the runs.
pub(crate) fn stopped(b: &mut FunctionBuilder<'_>, env: &Env, w: Words) {
    let v = b.iconst(Type::I64, STOPPED);
    env.store_word(b, w.spill, v);
}

/// The producer's last call, after its run was sealed: every row closed, the stream ended.
pub(crate) fn ended(b: &mut FunctionBuilder<'_>, env: &mut Env, w: Words, len: Value) {
    env.store_word(b, w.closed, len);
    env.store_word(b, w.done, env.one);
}

/// The reader: runs.sort's emit over the run as it fills (`RunEmit`'s rows and words), its windows the closed rows.
struct StreamEmit<'g> {
    emit: RunEmit,
    w: Words,
    h: u16,
    pull: Pull,
    mcx: Mcx<'g>,
}

/// Pipeline `k`'s reader of producer `q`'s stream, when `q` streams (`form`, and `exact` over the reader's `shape`).
pub(crate) fn reader<'g>(px: &mut Px<'g, '_>, k: usize, q: usize, shape: Shape, pre: usize) -> Result<PgBox<'g, dyn SourceMech + 'g>, Shape> {
    match form(px, q) {
        Some((r, pull)) if r == k && exact(&shape, pre) => {
            let h = handle(px, q, r);
            let w = words(px, q, pull);
            let emit = RunEmit::over(px, k, shape);
            Ok(mcx::pg_box_dyn!(px.mcx(), StreamEmit { emit, w, h, pull, mcx: px.mcx() } => dyn SourceMech + 'g))
        }
        _ => Err(shape),
    }
}

impl<'g> StreamEmit<'g> {
    /// A pull's closed rows past the count it pulls at: the lock's count + offset, or the cursor's last batch (its first
    /// before any).
    fn full(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        match self.pull {
            Pull::Lock(n) => b.iconst(Type::I64, n),
            Pull::Cursor => {
                let (last, first) = (env.load_word(b, self.w.batch), b.iconst(Type::I64, CURSOR_BATCH));
                let short = b.icmp(IntCond::Slt, last, first);
                b.select(short, first, last)
            }
        }
    }

    /// C1 part 2: a cursor's pull past every closed row (`l_cursor`): the run emptied when it holds them only (its producer
    /// stopped at a group's first row), the words with it, then the batch twice the last, at most half the room (at least
    /// `CURSOR_BATCH`; that before any).
    fn cursor_pull(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_cursor: Block, l_pull: Block, l_eof: Block) {
        let (_, w_cursor, _) = self.emit.words();
        let [l_recycle, l_clear] = [(); 2].map(|_| b.create_block(&[]));
        let l_grow = b.create_block(&[Type::I64]);
        b.switch_to(l_cursor);
        let closed = env.load_word(b, self.w.closed);
        let some = b.icmp(IntCond::Sgt, closed, env.zero);
        b.cond_br(some, (l_recycle, &[]), (l_grow, &[env.zero]));
        b.switch_to(l_recycle);
        let r = self.emit.recycle(b, env, closed, l_eof);
        let bit = b.int_bin(IntBinOp::And, r, env.one);
        let room = b.int_bin(IntBinOp::Ushr, r, env.one);
        let emptied = b.icmp(IntCond::Ne, bit, env.zero);
        b.cond_br(emptied, (l_clear, &[]), (l_grow, &[room]));
        b.switch_to(l_clear);
        for word in [self.w.closed, w_cursor, self.w.start] {
            env.store_word(b, word, env.zero);
        }
        b.br(l_grow, &[room]);
        b.switch_to(l_grow);
        let room = b.block_params(l_grow)[0];
        let (last, first) = (env.load_word(b, self.w.batch), b.iconst(Type::I64, CURSOR_BATCH));
        let twice = b.int_bin(IntBinOp::Add, last, last);
        let half = b.int_bin(IntBinOp::Ushr, room, env.one);
        let cap = { let big = b.icmp(IntCond::Sgt, half, first); b.select(big, half, first) };
        let grown = { let under = b.icmp(IntCond::Slt, twice, cap); b.select(under, twice, cap) };
        let fresh = b.icmp(IntCond::Slt, last, first);
        let next = b.select(fresh, first, grown);
        env.store_word(b, self.w.batch, next);
        b.br(l_pull, &[next]);
    }
}

impl<'g> SourceMech for StreamEmit<'g> {
    fn open_words(&self, w: &mut OpenWords) {
        self.emit.open_words(w);
        w.declare(OpenWord::open("incremental stream budget", Base::Words, self.w.budget, Type::I64, 0));
        w.declare(OpenWord::open("incremental stream pulled at", Base::Words, self.w.base, Type::I64, 0));
        w.declare(OpenWord::open("incremental stream rows handed before a re-arm", Base::Words, self.w.handed, Type::I64, 0));
        if self.w.cursor {
            w.declare(OpenWord::open("incremental stream cursor batch", Base::Words, self.w.batch, Type::I64, 0));
        }
    }

    /// The run is read in place (`SORT_MODE_MEM`); nothing is begun: the first window pulls.
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        let (w_mode, ..) = self.emit.words();
        let mem = b.iconst(Type::I64, gravity_kernels::host_abi::SORT_MODE_MEM);
        env.store_word(b, w_mode, mem);
        env.zero
    }

    /// The closed rows past the cursor; else, the stream not ended, one pull of the producer for the budget (a stopped pull
    /// closes at least one row; a pull that closed none pulls again, cancel-checked), then its rows; else the end. NEW-43: a
    /// stream that fell back (`spilled`) is not read in place: once its producer stopped at a prefix group's close
    /// (`stopped`) or ended, the host merge of its runs is opened and read past the `cur` rows handed on
    /// (`RunEmit::begin_past`), then read as runs.sort's emit reads a merge. Past the merge's last row, the stream not
    /// ended, the region is armed again as an empty run and the runs dropped (`RunEmit::rearm`), the words reset to an
    /// empty stream read in place, and the producer pulled on from the row it stopped at for the budget less the rows
    /// handed on so far: each group past the run's room costs its own rows only, as PostgreSQL's presorted-prefix mode
    /// reads it. A cursor's reader empties the run before each pull past its closed rows and pulls its next batch
    /// (`cursor_pull`); past a re-arm it pulls its last batch.
    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        let (w_mode, w_cursor, w_total) = self.emit.words();
        let [l_top, l_mem, l_merge, l_drained, l_rearm, l_fell, l_switch, l_check, l_past, l_bad, l_ok] = [(); 11].map(|_| b.create_block(&[]));
        let (l_win, l_pull) = (b.create_block(&[Type::I64, Type::I64, Type::I64]), b.create_block(&[Type::I64]));
        b.br(l_top, &[]);
        b.switch_to(l_top);
        let (mode, cur) = (env.load_word(b, w_mode), env.load_word(b, w_cursor));
        let in_place = { let m = b.iconst(Type::I64, gravity_kernels::host_abi::SORT_MODE_MEM); b.icmp(IntCond::Eq, mode, m) };
        b.cond_br(in_place, (l_mem, &[]), (l_merge, &[]));
        // the merge after a fallback: its rows past the cursor (a mode of 0 sends nothing)
        b.switch_to(l_merge);
        let total = env.load_word(b, w_total);
        let (on, left) = (b.icmp(IntCond::Sgt, mode, env.zero), b.icmp(IntCond::Slt, cur, total));
        let go = b.int_bin(IntBinOp::And, on, left);
        b.cond_br(go, (l_win, &[total, mode, cur]), (l_drained, &[]));
        // past the merge: the end once the producer ended, else the stream streams on
        b.switch_to(l_drained);
        let done = env.load_word(b, self.w.done);
        let ended = b.icmp(IntCond::Ne, done, env.zero);
        b.cond_br(ended, (l_eof, &[env.zero]), (l_rearm, &[]));
        b.switch_to(l_rearm);
        self.emit.rearm(b, env, l_eof);
        // the pull after a re-arm asks for the budget less every row handed on so far, at least one (a cursor: its batch)
        let want = match self.pull {
            Pull::Lock(n) => {
                let before = env.load_word(b, self.w.handed);
                let handed = b.int_bin(IntBinOp::Add, before, cur);
                env.store_word(b, self.w.handed, handed);
                let full = b.iconst(Type::I64, n);
                let left = b.int_bin(IntBinOp::Sub, full, handed);
                let short = b.icmp(IntCond::Slt, left, env.one);
                b.select(short, env.one, left)
            }
            Pull::Cursor => self.full(b, env),
        };
        let mem = b.iconst(Type::I64, gravity_kernels::host_abi::SORT_MODE_MEM);
        for word in [self.w.spill, self.w.closed, w_cursor, w_total] {
            env.store_word(b, word, env.zero);
        }
        env.store_word(b, w_mode, mem);
        b.br(l_pull, &[want]);
        b.switch_to(l_mem);
        let sp = env.load_word(b, self.w.spill);
        let fell = b.icmp(IntCond::Ne, sp, env.zero);
        b.cond_br(fell, (l_fell, &[]), (l_check, &[]));
        // fell back: pulled on to the producer's stop at a group close or its end, then the merge opened past the rows
        // handed on
        b.switch_to(l_fell);
        let done = env.load_word(b, self.w.done);
        let ended = b.icmp(IntCond::Ne, done, env.zero);
        let stop = { let s2 = b.iconst(Type::I64, STOPPED); b.icmp(IntCond::Eq, sp, s2) };
        let ready = b.int_bin(IntBinOp::Or, ended, stop);
        let full = self.full(b, env);
        b.cond_br(ready, (l_switch, &[]), (l_pull, &[full]));
        b.switch_to(l_switch);
        self.emit.begin_past(b, env, cur, l_eof);
        crate::frame::cancel::check(b, env.ctx, env.l_cancel);
        b.br(l_top, &[]);
        b.switch_to(l_check);
        let closed = env.load_word(b, self.w.closed);
        let more = b.icmp(IntCond::Slt, cur, closed);
        b.cond_br(more, (l_win, &[closed, mode, cur]), (l_past, &[]));
        b.switch_to(l_past);
        let done = env.load_word(b, self.w.done);
        let ended = b.icmp(IntCond::Ne, done, env.zero);
        match self.pull {
            Pull::Lock(n) => {
                let full = b.iconst(Type::I64, n);
                b.cond_br(ended, (l_eof, &[env.zero]), (l_pull, &[full]));
            }
            Pull::Cursor => {
                let l_cursor = b.create_block(&[]);
                b.cond_br(ended, (l_eof, &[env.zero]), (l_cursor, &[]));
                self.cursor_pull(b, env, l_cursor, l_pull, l_eof);
            }
        }
        b.switch_to(l_pull);
        let budget = b.block_params(l_pull)[0];
        crate::frame::cancel::check(b, env.ctx, env.l_cancel);
        let closed = env.load_word(b, self.w.closed);
        env.store_word(b, self.w.base, closed);
        env.store_word(b, self.w.budget, budget);
        let r = crate::rerun::pull::pull(b, env, self.h, budget);
        env.store_word(b, self.w.budget, env.zero);
        // below zero: the producer's error (the host stashed it), raised inside the reader's read, where PG raises it
        let bad = b.icmp(IntCond::Slt, r, env.zero);
        b.mark_cold(l_bad);
        b.cond_br(bad, (l_bad, &[]), (l_ok, &[]));
        b.switch_to(l_bad);
        let e = b.iconst(Type::I64, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST) as i64);
        b.br(env.l_raise, &[e]);
        b.switch_to(l_ok);
        b.br(l_top, &[]);
        let p = mcx::to_vec_in(&b.block_params(l_win)[..], self.mcx);
        self.emit.window(b, p[1], p[2], p[0], l_win)
    }

    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        self.emit.row(b, env, win, j, carry, next)
    }

    fn write_back(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, cur: Value, out: Value) {
        self.emit.write_back(b, env, cur, out)
    }

    /// On the leader (the pull runs the producer there).
    fn pipeline_desc(&self, px: &Px<'_, '_>, k: usize) -> Pipeline {
        let d = px.gx.pipelines()[k].desc;
        match d.is_eligible() {
            true => Pipeline { parallel: Parallel::Ineligible { site: d.driving_site, reason: Why::Host }, ..d },
            false => d,
        }
    }
}
