//! Kind `runs.incremental` (cards/M3.md L6, I43): IncrementalSort. PostgreSQL's two-mode sort over prefix-sorted input
//! emits the full sort order, ties in any order. The unbounded form (I-C, D479) is runs/sort.rs's run with its prefix
//! groups sorted as they close: every row is appended as runs.sort appends it (`RunEnd::append`); when a row's prefix key
//! pairs differ from the row before it, the group before it (`[start, row)`, frame word `w_start`) is sorted in place
//! (`incsort_span`) while it is still in cache, so the run is in key order when it is put or sealed and the hosts take it
//! as it lies (`RUN_ORDERED`, set only for the put). A row whose prefix orders before the row before it (a parallel
//! claim's input, an input not in prefix order) marks the run disordered and its put sorts it whole. A finishing
//! (parallel) producer, whose last run the section end puts, is runs.sort's end as it is. The emit is
//! runs.sort's (the sealed run's rows in place). The breaker stays: the driver runs a producer to its end before its
//! reader opens (driver entry.rs `run`), so the groups are not streamed out as they close. A bounded incremental sort (a
//! top over a presorted prefix) is `runs.topk_rows`' run and needs this kind beside it: PostgreSQL stops reading once the
//! prefix group holding the bound's last row closes; the top stops there too where it can (V4R-179, runs/topk_rows.rs
//! `Stop`), else reads every row and is admitted only when no row before the sort can raise (props `incremental_admit`,
//! D449's rule). An unbounded incremental sort under a Unique a counted LIMIT stops (group_core-0259: decompose
//! facts/stream.rs hands the stop down) models PostgreSQL's unbounded batches and ends its producer where PostgreSQL's
//! last load ends (`Batches`), so code before it runs for exactly PostgreSQL's rows; the same stop under a sorted
//! GroupAggregate (the stop road: `Batches::groups`) ends it where the group above has its last group. Budget 400 lines. Owner M3-MERGE
//! (M3-INCSORT: I-C).

use gravity_abi::breaker::SORTRUN_HDR;
use gravity_decompose::ops::shared::sort::SortForm;
use gravity_decompose::Op;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, IntCond, Type, Value};
use gravity_kernels::ops::sort::entries::{SpanCut, SpanSeen, RUN_ORDERED, SPAN_DISTINCT};
use mcx::PgBox;

use crate::face::end::EndMech;
use crate::face::kinds::{Face, KindId};
use crate::face::strategy::{SORT_KEYLANE, SORT_NATURAL, SORT_RESIDENT};
use crate::face::step::StepMech;
use crate::frame::env::Env;
use crate::frame::frame::{EndMaker, Next, StepMaker};
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::{runs_word, Px, WordKey};
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::ops::Table;

use crate::ops::sort::strategies::resident as res;
use crate::ops::sort::inc_batches::{unsupported_exit, Batches};
use crate::ops::sort::inc_stream;
use crate::ops::sort::lower::{sort_of, Arm, RunEnd};
use crate::ops::sort::topk_rows::SOFT_ROWS;

pub(crate) const ROWS: Table = Table {
    kinds: &[(KindId::RunsIncremental, &StepMaker(make_step)), (KindId::RunsIncremental, &EndMaker(make_end))],
    ..crate::ops::NONE
};

/// The byte offset of `SortRun::pad[2]` (natural.rs `PAD2`).
const PAD2: i32 = 14;
fn bug(node: i32, payload: &'static str) -> ! { crate::refuse!(node, Reason::Bug { payload }) }

/// The Sort op of an unbounded incremental sort: the end takes the row as it is.
struct Pass;

impl StepMech for Pass {
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        next(b, env, row)
    }
}

fn unbounded(op: &Op<'_>) -> bool { matches!(op, Op::Sort(d) if matches!(d.form, SortForm::Runs) && d.presorted > 0) }

fn make_step<'g, 'p>(px: &mut Px<'g, 'p>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let p = &px.gx.pipelines()[k];
    match &p.ops[j] {
        op if unbounded(op) => mcx::pg_box_dyn!(px.mcx(), Pass => dyn StepMech + 'g),
        _ => bug(p.node_id, "runs.incremental's step over an op that is not an unbounded presorted sort"),
    }
}

/// The producer: runs.sort's run end whose prefix groups are sorted as they close.
struct IncEnd {
    end: RunEnd,
    /// The prefix keys (0 when the run dropped a Const key: one span, sorted at the put).
    pre: usize,
    /// The pipeline's last run is its section end's (`Pipeline::finish`, a parallel producer's): its rows are runs.sort's
    /// (the section end's put sorts the run whole), no group sorted.
    finishing: bool,
    /// Frame words: the open group's first row in the run; nonzero once a row ordered before the row before it.
    w_start: i32,
    w_disorder: i32,
    /// The stop a LIMIT over a Unique (group_core-0259) or over a sorted group (the stop road) handed this sort, when armed.
    stop: Option<Batches>,
    /// INCSTREAM (inc_stream.rs): the producer of a stream its lock pipeline pulls, when armed.
    stream: Option<inc_stream::Words>,
    /// D2690: the top-K DISTINCT form of a counted stop, when armed.
    top: Option<Top>,
    /// D2316 (inc_held.rs): the held form, its rows-handed count the stop's (`Batches::rows`); a fill that ends hands on
    /// every row.
    held: bool,
    node: i32,
}

/// D2690, the top-K DISTINCT incremental sort: under a counted stop (a Unique a LIMIT stops, not the group form) the
/// Unique hands on at most `k` rows of a prefix group (the stop's `need`; one under `per_group`), so the open group keeps
/// only the rows that can be among them. Past `cut` rows, or at a full run past `2 k`, the group is cut (kernels
/// `sort_cut_span`: ordered, one row per run of equal rows, its first `k` kept, the rest's var copies freed); past a cut
/// a stored row is dropped when the cut kept `k` and it orders at or after the `k`-th, or (distinct) when it equals a
/// kept row (`sort_span_seen` over the cut's word `w_bound`, `2 n + full`: its `n` ordered rows from the group's first).
/// PostgreSQL's rows are read as before (the stop is unchanged, and a closed group's distinct rows count the same up to
/// `k`); only the rows the sort holds and orders shrink. rnd-2670: a 2.27 M row first group PostgreSQL sorts on disk for
/// its first 10 distinct rows.
#[derive(Clone, Copy)]
struct Top {
    k: i64,
    distinct: bool,
    cut: i64,
    w_bound: i32,
}

fn make_end<'g, 'p>(px: &mut Px<'g, 'p>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    let p = &px.gx.pipelines()[k];
    let (node, finishing, breaker) = (p.node_id, p.desc.finish, crate::frame::iface::breaker_of(&p.desc));
    let (sd, cols) = sort_of(px, k);
    if !matches!(sd.form, SortForm::Runs) || sd.presorted == 0 {
        bug(node, "runs.incremental's end over a sort that is not an unbounded presorted sort");
    }
    // V4R-140: the key lane over an incremental sort (a disordered run's put radix-sorts it)
    let keylane = crate::ops::sort::shapes::lane_key(sd, None) && res::on(px, SORT_KEYLANE);
    let natural = finishing && res::on(px, SORT_NATURAL);
    let arm = Arm { keylane, natural, resident: finishing && res::on(px, SORT_RESIDENT), finish: false };
    let end = RunEnd::over(px, k, &sd.keys, Some(sd.tlist), cols, arm);
    if let (true, Some(br)) = (end.resident, breaker) {
        res::declare(px, k, br.slot, (!end.shape.var()).then_some((end.shape.spec, 2 * end.shape.keys.len() as u16)));
    }
    let pre = if end.shape.keys.len() == sd.keys.len() { (sd.presorted as usize).min(sd.keys.len()) } else { 0 };
    let p = &px.gx.pipelines()[k];
    let handed = px.gx.graph.stops.iter().find(|s| p.node_ops.iter().any(|o| o.node_id == s.sort)).map(|s| (s.need, s.groups, s.per_group));
    let late = px.gx.graph.stops.iter().any(|s| s.late && p.node_ops.iter().any(|o| o.node_id == s.sort));
    let folds = px.gx.graph.stops.iter().any(|s| s.folds && p.node_ops.iter().any(|o| o.node_id == s.sort));
    let rows = px.gx.graph.stops.iter().any(|s| s.rows && p.node_ops.iter().any(|o| o.node_id == s.sort));
    let serial = !(p.desc.is_eligible() && p.desc.driving_site >= 0);
    // the stop is exact over exact prefix words (topk_rows.rs `Stop::of`'s rule) from a serial producer; else the sort reads
    // every row and is admitted only when no row before it can raise (D449's rule). The group form, and a Unique over the
    // prefix keys (`per_group`, topn_core-0240), count a prefix group as one group or one distinct row only over raw words
    // (a float's word may part two values its group's equality joins: -0 and 0)
    let by_prefix = handed.is_some_and(|(_, g, one)| g || one);
    let exact = end.shape.keys[..pre].iter().all(|key| {
        let tf = &end.shape.cols[key.col as usize].tf;
        tf.base != crate::rep::keylane::NAME && (tf.image.is_word() || !by_prefix && tf.image.is_float())
    });
    let mut armed = handed.filter(|_| !finishing && serial && pre > 0 && exact);
    // the window form's `f = 1` qual (decompose `Stop::one`): its prefix-group count holds for the constant 1 only, and the
    // content key holds the constant as a word, so the armed stop is filed as a literal guard on it: a statement of the key
    // with another constant lowers again (where the stop is not armed the sort reads every row, right for any constant)
    let one = px.gx.graph.stops.iter().find(|s| p.node_ops.iter().any(|o| o.node_id == s.sort)).and_then(|s| s.one);
    if let (Some(_), Some(c)) = (armed, one) {
        let width = c.as_const().map_or(u64::MAX, |c| word_mask(c.constlen));
        if gravity_lower::inputs::key::lit_decide(c, is_one, [width, 0, 0, 0]) != Some(1) {
            armed = None;
        }
    }
    // the group form unarmed reads every row, as it did before the stop (nothing before the sort raises); so does a counted
    // stop handed as a cost (`late`: nothing below the sort raises or acts), where reading past PostgreSQL's batches shows
    // nothing
    if handed.is_some_and(|(_, g, _)| !g) && armed.is_none() && !late && !crate::props::incremental_admit(px.gx, k) {
        crate::refuse!(node, Reason::Shape { why: ShapeWhy("an incremental sort under a Unique a LIMIT stops whose rows can raise past PostgreSQL's batches, its stop not exact (D449's rule)") });
    }
    let mut word = |i: u8| px.frame_word(WordKey::Open { face: Face::Runs, k, j: u64::MAX, i }) as i32 * 8;
    let (w_start, w_disorder) = (word(runs_word::INC_START), word(runs_word::INC_DISORDER)); // HOSTS f01537c4d4b: the one index table
    let stop = armed.map(|(need, groups, per_group)| Batches {
        need: need.min(i64::MAX as u64) as i64,
        groups,
        per_group,
        late,
        rows,
        w_rows: word(runs_word::INC_ROWS),
        w_mode: word(runs_word::INC_MODE),
        w_held: word(runs_word::INC_HELD),
        w_out: word(runs_word::INC_OUT),
        w_split_lo: word(runs_word::INC_SPLIT_LO),
        w_split_hi: word(runs_word::INC_SPLIT_HI),
        w_out_hi: word(runs_word::INC_OUT_HI),
        w_held_hi: word(runs_word::INC_HELD_HI),
        w_pend: word(runs_word::INC_PEND),
    });
    // D2690: a counted stop's groups keep their first `need` distinct rows (one under `per_group`); a group above folds
    // every row (`Stop::folds`, agg_shop-0033)
    // (`k` rides 29 bits of the hosts' word with a cut's `2 n + full`: a larger one never cuts a run's rows anyway)
    let top = armed.filter(|&(need, groups, _)| !groups && !folds && need >> 29 == 0).map(|(need, _, one)| {
        let k = if one { 1 } else { need as i64 };
        Top { k, distinct: !one, cut: SOFT_ROWS.max(4 * k), w_bound: word(runs_word::INC_BOUND) }
    });
    // D2316: the plan-order run's held form (inc_held.rs): PostgreSQL's batches counted whole, the rows they handed on its
    // reader's count when the fill raises (a marked sort that cannot take it refuses by D2316's name)
    let held = crate::ops::sort::inc_held::producer(px, k, &end.shape, pre, handed.is_some()).is_some();
    let stop = stop.or_else(|| held.then(|| {
        let mut word = |i: u8| px.frame_word(WordKey::Open { face: Face::Runs, k, j: u64::MAX, i }) as i32 * 8;
        Batches {
            need: i64::MAX,
            groups: false,
            per_group: false,
            late: true,
            rows: true,
            w_rows: word(runs_word::INC_ROWS),
            w_mode: word(runs_word::INC_MODE),
            w_held: word(runs_word::INC_HELD),
            w_out: word(runs_word::INC_OUT),
            w_split_lo: word(runs_word::INC_SPLIT_LO),
            w_split_hi: word(runs_word::INC_SPLIT_HI),
            w_out_hi: word(runs_word::INC_OUT_HI),
            w_held_hi: word(runs_word::INC_HELD_HI),
            w_pend: word(runs_word::INC_PEND),
        }
    }));
    // INCSTREAM: under a LockRows a LIMIT stops, the producer of a stream (its reader decides the same: inc_stream.rs)
    let stream = if handed.is_none() && !held && inc_stream::exact(&end.shape, pre) { inc_stream::form(px, k).map(|(r, pull)| inc_stream::arm(px, k, r, pull)) } else { None };
    mcx::pg_box_dyn!(px.mcx(), IncEnd { end, pre, finishing, w_start, w_disorder, stop, stream, top, held, node } => dyn EndMech + 'g)
}

impl IncEnd {
    /// Rows `[from, to)` of the run sorted in place when two or more; `l_exit(status)` below zero. Under a stop (`Batches`)
    /// the group's distinct rows too (`incsort_span` counts them as it sorts: `COUNT` in its key word; fewer than two
    /// rows have as many as their rows; one under `Batches::per_group`, whose Unique compares the prefix only), else None.
    fn span(&self, b: &mut FunctionBuilder<'_>, env: &Env, run: Value, from: Value, to: Value) -> Option<Value> {
        let counted = self.stop.as_ref().is_some_and(|st| !st.groups);
        let one = self.stop.as_ref().is_some_and(|st| st.per_group);
        let rows = self.stop.as_ref().is_some_and(|st| st.rows); // D3376: the group's rows, not its distinct rows
        let (l_sort, l_done) = (b.create_block(&[]), b.create_block(if counted { &[Type::I64] } else { &[] }));
        let n = b.int_bin(IntBinOp::Sub, to, from);
        let two = b.iconst(Type::I64, 2);
        let many = b.icmp(IntCond::Sge, n, two);
        let few = [n];
        b.cond_br(many, (l_sort, &[]), (l_done, if counted { &few[..] } else { &[] }));
        b.switch_to(l_sort);
        let s = &self.end.shape;
        let h = res::host(b, "incsort_span", &[Type::Ptr, Type::Ptr, Type::I64, Type::I64, Type::I64, Type::I64, Type::I64], Type::I64, gravity_abi::host_words!(super::raw::incsort_span(ctx, run, from, to, nkey, spec, tbl)));
        let nkey = b.iconst(Type::I64, s.keys.len() as i64 | if counted && !one && !rows { super::raw::COUNT as i64 } else { 0 });
        let spec = b.const_raw(Type::I64, s.spec);
        // a var row compares through its table (`RowCmp`: var tails, collations, keys past four; M6 L0-a), and its var
        // slots are the table's
        let tbl = if s.var() { let t = s.table(b); b.conv(ConvOp::PtrToInt, Type::I64, t) } else { env.zero };
        let r = b.call_host(h, &[env.ctx, run, from, to, nkey, spec, tbl], Some(Type::I64)).expect("incsort_span returns its status");
        let bad = b.icmp(IntCond::Slt, r, env.zero);
        let got = [r];
        b.cond_br(bad, (env.x.l_exit, &[r]), (l_done, if counted { &got[..] } else { &[] }));
        b.switch_to(l_done);
        counted.then(|| if one { b.iconst(Type::I64, 1) } else if rows { n } else { b.block_params(l_done)[0] })
    }

    /// D2432: a row of key pairs `pairs` against the run's last row (the run's `len` rows, at least one): whether its prefix
    /// pair words equal the last row's (it continues the open group) and whether they order before them (`boundary`'s
    /// lexicographic unsigned compare).
    fn after_last(&self, b: &mut FunctionBuilder<'_>, run: Value, len: Value, pairs: &[[Value; 2]], one: Value) -> (Value, Value) {
        let rb = b.iconst(Type::I64, self.end.shape.row_bytes as i64);
        let last = b.int_bin(IntBinOp::Sub, len, one);
        let off = b.int_bin(IntBinOp::Mul, last, rb);
        let at = b.ptr_add(run, off, 1, SORTRUN_HDR as i32);
        let (mut less, mut eq) = (b.bconst(false), b.bconst(true));
        for (i, pair) in pairs.iter().take(self.pre).enumerate() {
            for (j, &v) in pair.iter().enumerate() {
                let p = b.load(Type::I64, at, 16 * i as i32 + 8 * j as i32);
                let (lt, e) = (b.icmp(IntCond::Ult, v, p), b.icmp(IntCond::Eq, v, p));
                let both = b.int_bin(IntBinOp::And, eq, lt);
                less = b.int_bin(IntBinOp::Or, less, both);
                eq = b.int_bin(IntBinOp::And, eq, e);
            }
        }
        (eq, less)
    }

    /// D2690: the top form's host word (kernels sort raw.rs `SpanCut` / `SpanSeen`): the open group's first row `start`,
    /// the count `n` (bits 32 up), `SPAN_DISTINCT` unless one row per group; and the run's emit table.
    fn top_args(&self, b: &mut FunctionBuilder<'_>, t: &Top, start: Value, n: Value) -> [Value; 2] {
        let sh = b.iconst(Type::I64, 32);
        let hi = b.int_bin(IntBinOp::Shl, n, sh);
        let w = b.int_bin(IntBinOp::Or, start, hi);
        let w = if t.distinct { let d = b.const_raw(Type::I64, SPAN_DISTINCT); b.int_bin(IntBinOp::Or, w, d) } else { w };
        let tb = self.end.shape.table(b);
        [w, b.conv(ConvOp::PtrToInt, Type::I64, tb)]
    }

    /// D2690: the open group `[start, len)` cut to its first `k` rows (distinct unless one per group), the cut's word
    /// (`2 n + full`) kept for `trim`'s seen test; `l_exit(status)` below zero.
    fn cut(&self, b: &mut FunctionBuilder<'_>, env: &Env, t: &Top, run: Value, start: Value) {
        let h = res::host(b, "sort_cut_span", &[Type::Ptr, Type::Ptr, Type::I64, Type::I64], Type::I64, gravity_kernels::contract::entry3::<SpanCut> as *const () as usize);
        let k = b.iconst(Type::I64, t.k);
        let [w, tbl] = self.top_args(b, t, start, k);
        let r = b.call_host(h, &[env.ctx, run, w, tbl], Some(Type::I64)).expect("sort_cut_span returns its rows");
        let (bad, l_ok) = (b.icmp(IntCond::Slt, r, env.zero), b.create_block(&[]));
        b.cond_br(bad, (env.x.l_exit, &[r]), (l_ok, &[]));
        b.switch_to(l_ok);
        let full = b.icmp(IntCond::Sge, r, k);
        let full = b.conv(ConvOp::Zext, Type::I64, full);
        let two = b.int_bin(IntBinOp::Add, r, r);
        let w = b.int_bin(IntBinOp::Add, two, full);
        env.store_word(b, t.w_bound, w);
    }

    /// D2690: after row `len - 1` was stored and counted. A row continuing a cut open group that it adds nothing to is
    /// dropped (`sort_span_seen`); else the group past `cut` rows is cut. A row opening a group, or a run, clears the cut.
    fn trim(&self, b: &mut FunctionBuilder<'_>, env: &Env, t: &Top) {
        let run = self.end.run(b, env);
        let len = b.load_zext(Type::I32, run, 4);
        let start = env.load_word(b, self.w_start);
        let idx = b.int_bin(IntBinOp::Sub, len, env.one);
        let [l_in, l_open, l_test, l_size, l_cut, l_done] = [(); 6].map(|_| b.create_block(&[]));
        let later = b.icmp(IntCond::Sgt, idx, start);
        b.cond_br(later, (l_in, &[]), (l_open, &[]));
        b.switch_to(l_open);
        env.store_word(b, t.w_bound, env.zero);
        b.br(l_done, &[]);
        b.switch_to(l_in);
        let w = env.load_word(b, t.w_bound);
        let some = b.icmp(IntCond::Ne, w, env.zero);
        b.cond_br(some, (l_test, &[]), (l_size, &[]));
        b.switch_to(l_test);
        let h = res::host(b, "sort_span_seen", &[Type::Ptr, Type::Ptr, Type::I64, Type::I64], Type::I64, gravity_kernels::contract::entry3::<SpanSeen> as *const () as usize);
        let [hw, tbl] = self.top_args(b, t, start, w);
        let r = b.call_host(h, &[env.ctx, run, hw, tbl], Some(Type::I64)).expect("sort_span_seen returns its status");
        let (bad, l_ok) = (b.icmp(IntCond::Slt, r, env.zero), b.create_block(&[]));
        b.cond_br(bad, (env.x.l_exit, &[r]), (l_ok, &[]));
        b.switch_to(l_ok);
        let dropped = b.icmp(IntCond::Ne, r, env.zero);
        b.cond_br(dropped, (l_done, &[]), (l_size, &[]));
        b.switch_to(l_size);
        let n = b.int_bin(IntBinOp::Sub, len, start);
        let cut = b.iconst(Type::I64, t.cut);
        let over = b.icmp(IntCond::Sge, n, cut);
        b.cond_br(over, (l_cut, &[]), (l_done, &[]));
        b.switch_to(l_cut);
        self.cut(b, env, t, run, start);
        b.br(l_done, &[]);
        b.switch_to(l_done);
    }

    /// The open group sorted (`[start, len)`).
    fn close(&self, b: &mut FunctionBuilder<'_>, env: &Env) {
        let run = self.end.run(b, env);
        let len = b.load_zext(Type::I32, run, 4);
        let start = env.load_word(b, self.w_start);
        self.span(b, env, run, start, len);
    }

    /// The run's pad[2] with `RUN_ORDERED` set to `on`.
    fn ordered(&self, b: &mut FunctionBuilder<'_>, run: Value, on: Value) {
        let p2 = b.load_zext(Type::I8, run, PAD2);
        let keep = b.iconst(Type::I64, !(RUN_ORDERED as i64) & 0xff);
        let rest = b.int_bin(IntBinOp::And, p2, keep);
        let (bit, zero) = (b.iconst(Type::I64, RUN_ORDERED as i64), b.iconst(Type::I64, 0));
        let set = b.select(on, bit, zero);
        let v = b.int_bin(IntBinOp::Or, rest, set);
        let v8 = b.conv(ConvOp::Trunc, Type::I8, v);
        b.store(run, PAD2, v8);
    }

    /// The run put (`last`: a serial producer's last run) through the marked hosts: `RUN_ORDERED` only while the put runs,
    /// when no row was out of prefix order since the last put; then the next run starts its first group at row 0. D3529: a
    /// var put's status above zero names the next run (D1330: a big run for a row whose copies the empty run had no room
    /// for, then the own run again once the big run's put freed it), so the bit is cleared on the run the word names now.
    fn put(&self, b: &mut FunctionBuilder<'_>, env: &Env, last: bool) {
        let s = &self.end.shape;
        let run = self.end.run(b, env);
        let dis = env.load_word(b, self.w_disorder);
        let in_order = b.icmp(IntCond::Eq, dis, env.zero);
        self.ordered(b, run, in_order);
        let h = res::put(b, s.var(), last, true);
        let r = if s.var() {
            let tbl = s.table(b);
            b.call_host(h, &[env.ctx, run, tbl], Some(Type::I64))
        } else {
            let [_, nkey, spec] = s.args(b);
            b.call_host(h, &[env.ctx, run, nkey, spec], Some(Type::I64))
        }
        .expect("a sort put returns its status");
        let (bad, l_ok) = (b.icmp(IntCond::Slt, r, env.zero), b.create_block(&[]));
        b.cond_br(bad, (env.x.l_exit, &[r]), (l_ok, &[]));
        b.switch_to(l_ok);
        if !last {
            self.end.next_run(b, env, last, r);
            let (now, no) = (self.end.run(b, env), b.bconst(false));
            self.ordered(b, now, no);
        }
        env.store_word(b, self.w_start, env.zero);
        env.store_word(b, self.w_disorder, env.zero);
    }

    /// After row `idx` was appended: when its prefix pairs differ from row `idx - 1`'s (both in the open group's run), the
    /// group before it is sorted and `idx` opens the next; a prefix that orders before marks the run disordered. Under a
    /// stop (`Batches`) the row is counted into PostgreSQL's batches (`carry`, `next`: the walk's, for its end). A stream
    /// that fell back (NEW-43) puts its runs here instead, the row taken off the run first, and stops at the row (`row`'s
    /// carry and cursor: the resume delivers it again), so its reader reads the closed groups' merge before more is read.
    fn boundary(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, carry: &Carry, row: &RowVals) {
        let next = row.next;
        let run = self.end.run(b, env);
        let len = b.load_zext(Type::I32, run, 4);
        let idx = b.int_bin(IntBinOp::Sub, len, env.one);
        let start = env.load_word(b, self.w_start);
        let [l_cmp, l_new, l_done] = [(); 3].map(|_| b.create_block(&[]));
        // a row of the open group's prefix: counted into the batches under a stop (not the group form's), else nothing to do
        let batches = self.stop.as_ref().filter(|st| !st.groups);
        let l_same = if batches.is_some() { b.create_block(&[]) } else { l_done };
        let later = b.icmp(IntCond::Sgt, idx, start);
        b.cond_br(later, (l_cmp, &[]), (l_same, &[]));
        b.switch_to(l_cmp);
        let rb = b.iconst(Type::I64, self.end.shape.row_bytes as i64);
        let off = b.int_bin(IntBinOp::Mul, idx, rb);
        let at = b.ptr_add(run, off, 1, SORTRUN_HDR as i32);
        let back = b.int_un(gravity_ir::IntUnOp::Neg, rb);
        let prev = b.ptr_add(at, back, 1, 0);
        // lexicographic over the prefix pair words, unsigned (natural.rs `mark`)
        let (mut less, mut eq) = (b.bconst(false), b.bconst(true));
        for w in 0..2 * self.pre as i32 {
            let (x, p) = (b.load(Type::I64, at, 8 * w), b.load(Type::I64, prev, 8 * w));
            let (lt, e) = (b.icmp(IntCond::Ult, x, p), b.icmp(IntCond::Eq, x, p));
            let both = b.int_bin(IntBinOp::And, eq, lt);
            less = b.int_bin(IntBinOp::Or, less, both);
            eq = b.int_bin(IntBinOp::And, eq, e);
        }
        b.cond_br(eq, (l_same, &[]), (l_new, &[]));
        b.switch_to(l_new);
        if self.stop.is_some() || self.stream.is_some() {
            unsupported_exit(b, env, less);
        }
        let dis = env.load_word(b, self.w_disorder);
        let lz = b.conv(ConvOp::Zext, Type::I64, less);
        let dis = b.int_bin(IntBinOp::Or, dis, lz);
        env.store_word(b, self.w_disorder, dis);
        let d = self.span(b, env, run, start, idx);
        env.store_word(b, self.w_start, idx);
        match (&self.stop, d) {
            (Some(st), Some(d)) => st.opened(b, env, d, carry, next),
            (Some(st), None) => st.closed(b, env, carry, next),
            (None, _) => {}
        }
        if let Some(w) = self.stream {
            // the row that opened the next group taken off the run and the stop at it (its resume delivers it again)
            let at_row = |b: &mut FunctionBuilder<'_>, env: &mut Env, put: bool| {
                let run = self.end.run(b, env);
                let len = b.conv(ConvOp::Trunc, Type::I32, idx);
                b.store(run, 4, len);
                if put {
                    self.put(b, env, false);
                    inc_stream::stopped(b, env, w);
                }
                let st = b.iconst(Type::I64, gravity_abi::status::suspend(false));
                env.stop(b, &row.carry, row.at, st);
            };
            let mut fell = |b: &mut FunctionBuilder<'_>, env: &mut Env| at_row(b, env, true);
            match w.cursor {
                true => inc_stream::closed(b, env, w, idx, carry, next, &mut fell, Some(&mut |b, env| at_row(b, env, false))),
                false => inc_stream::closed(b, env, w, idx, carry, next, &mut fell, None),
            }
        }
        b.br(l_done, &[]);
        if let Some(st) = batches {
            b.switch_to(l_same);
            st.row(b, env, carry, next);
            b.br(l_done, &[]);
        }
        b.switch_to(l_done);
    }
}

impl EndMech for IncEnd {
    fn open_words(&self, w: &mut OpenWords) {
        self.end.open_words(w);
        w.declare(OpenWord::open("incremental sort group start", Base::Words, self.w_start, Type::I64, 0));
        w.declare(OpenWord::open("incremental sort disorder", Base::Words, self.w_disorder, Type::I64, 0));
        if let Some(st) = &self.stop {
            w.declare(OpenWord::open("incremental sort batch rows", Base::Words, st.w_rows, Type::I64, 0));
            w.declare(OpenWord::open("incremental sort batch mode", Base::Words, st.w_mode, Type::I64, 0));
            w.declare(OpenWord::open("incremental sort batch held", Base::Words, st.w_held, Type::I64, 0));
            w.declare(OpenWord::open("incremental sort rows handed on", Base::Words, st.w_out, Type::I64, 0));
            w.declare(OpenWord::open("incremental sort split group's most part rows", Base::Words, st.w_split_lo, Type::I64, 0));
            w.declare(OpenWord::open("incremental sort split group's part rows", Base::Words, st.w_split_hi, Type::I64, 0));
            w.declare(OpenWord::open("incremental sort rows handed on at most", Base::Words, st.w_out_hi, Type::I64, 0));
            w.declare(OpenWord::open("incremental sort batch held at most", Base::Words, st.w_held_hi, Type::I64, 0));
            w.declare(OpenWord::open("incremental sort group a put closed", Base::Words, st.w_pend, Type::I64, 0));
        }
        if let Some(t) = &self.top {
            w.declare(OpenWord::open("incremental sort top group's cut", Base::Words, t.w_bound, Type::I64, 0));
        }
        if let Some(sw) = &self.stream {
            w.declare(OpenWord::open("incremental stream closed rows", Base::Words, sw.closed, Type::I64, 0));
            w.declare(OpenWord::open("incremental stream ended", Base::Words, sw.done, Type::I64, 0));
            w.declare(OpenWord::open("incremental stream fell back", Base::Words, sw.spill, Type::I64, 0));
        }
    }

    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        self.end.open(b, env);
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, x: &Exits) -> Carry<'e> {
        if self.finishing {
            return self.end.lower(b, env, row, x);
        }
        // a full run: its open group sorted, the run put; the row then opens the next run's first group. Under a stop
        // (D2432) the open group's part is counted: a part of a group PostgreSQL's batch holds whole when the row continues
        // it (`Batches::split`: the group stays open in the next run), else the group closed (`Batches::pend`: the row
        // counts it as it opens the next); a row out of prefix order is UNSUPPORTED. D3529: an empty run (a row whose copies
        // its region has no room for) holds no group part to count: it is put as it is, the put handing the row a big run
        let carry = self.end.append(b, env, row, &mut |b, env, pairs| {
            // INCSTREAM (NEW-43): a stream's producer (never under a stop) falls back to this breaker form at a full run
            // (inc_stream.rs `spilled`): the open group sorted and the run put below, the reader then reads their merge
            if let Some(w) = self.stream {
                inc_stream::spilled(b, env, w);
            }
            // D2690: a full run whose open group holds `2 k` rows or more is cut first (the row is then written again)
            let joined = self.top.map(|t| {
                let run = self.end.run(b, env);
                let len = b.load_zext(Type::I32, run, 4);
                let start = env.load_word(b, self.w_start);
                let n = b.int_bin(IntBinOp::Sub, len, start);
                let twice = b.iconst(Type::I64, 2 * t.k);
                let big = b.icmp(IntCond::Sge, n, twice);
                let [l_cut, l_put, l_join] = [(); 3].map(|_| b.create_block(&[]));
                b.cond_br(big, (l_cut, &[]), (l_put, &[]));
                b.switch_to(l_cut);
                self.cut(b, env, &t, run, start);
                b.br(l_join, &[]);
                b.switch_to(l_put);
                (t.w_bound, l_join)
            });
            match self.stop.as_ref().filter(|st| !st.groups) {
                Some(st) => {
                    let run = self.end.run(b, env);
                    let len = b.load_zext(Type::I32, run, 4);
                    let empty = b.icmp(IntCond::Eq, len, env.zero);
                    let [l_rows, l_split, l_pend, l_put] = [(); 4].map(|_| b.create_block(&[]));
                    b.cond_br(empty, (l_put, &[]), (l_rows, &[]));
                    b.switch_to(l_rows);
                    let (cont, less) = self.after_last(b, run, len, pairs, env.one);
                    unsupported_exit(b, env, less);
                    let start = env.load_word(b, self.w_start);
                    let d = self.span(b, env, run, start, len).unwrap_or_else(|| bug(self.node, "a counted stop's span without its count"));
                    b.cond_br(cont, (l_split, &[]), (l_pend, &[]));
                    b.switch_to(l_split);
                    st.split(b, env, d);
                    b.br(l_put, &[]);
                    b.switch_to(l_pend);
                    st.pend(b, env, d);
                    b.br(l_put, &[]);
                    b.switch_to(l_put);
                }
                None => self.close(b, env),
            }
            self.put(b, env, false);
            if let Some((w_bound, l_join)) = joined {
                env.store_word(b, w_bound, env.zero);
                b.br(l_join, &[]);
                b.switch_to(l_join);
            }
        });
        if self.pre > 0 {
            self.boundary(b, env, &carry, row);
        }
        if let Some(t) = self.top {
            self.trim(b, env, &t);
        }
        carry
    }

    /// A serial producer's last run: its open group sorted, then sealed as it lies (or put when runs were put before it).
    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) {
        if self.finishing {
            return self.end.finish(b, env, x);
        }
        self.close(b, env);
        self.put(b, env, true);
        if let (true, Some(st)) = (self.held, &self.stop) {
            let all = b.iconst(Type::I64, i64::MAX);
            env.store_word(b, st.w_out, all);
        }
        if let Some(w) = self.stream {
            // D3529: only a stream that never fell back reads its run's rows; a fallen-back one (`spilled`) may end in a big run
            // its last put freed (D1330), and its reader reads the merge, never `closed`
            let sp = env.load_word(b, w.spill);
            let streaming = b.icmp(IntCond::Eq, sp, env.zero);
            let (l_len, l_end) = (b.create_block(&[]), b.create_block(&[Type::I64]));
            b.cond_br(streaming, (l_len, &[]), (l_end, &[env.zero]));
            b.switch_to(l_len);
            let run = self.end.run(b, env);
            let len = b.load_zext(Type::I32, run, 4);
            b.br(l_end, &[len]);
            b.switch_to(l_end);
            let len = b.block_params(l_end)[0];
            inc_stream::ended(b, env, w, len);
        }
    }

    /// The run's row, as runs.sort's end declares it (RUNS-26).
    fn pipeline_desc(&self, px: &Px<'_, '_>, k: usize, d: gravity_program::Pipeline) -> gravity_program::Pipeline {
        self.end.pipeline_desc(px, k, d)
    }
}

/// The literal guard of the window form's `f = 1` qual (`make_end`): 1 when the word is 1 in its width (`args[0]`: the
/// width's mask).
fn is_one(word: u64, args: &[u64; 4]) -> u64 {
    (word & args[0] == 1) as u64
}

/// The mask of a by-value constant's width in bytes (`constlen`).
fn word_mask(len: i32) -> u64 {
    match len {
        1 => 0xff,
        2 => 0xffff,
        4 => 0xffff_ffff,
        _ => u64::MAX,
    }
}
