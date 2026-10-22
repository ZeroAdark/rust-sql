//! Road `mask.rows` (lane brfix; default off, road_off.rs `DEFAULT_OFF`): the first filter's block mask over a source with
//! no word window, made row by row. The H01 block arm opens only through `SourceMech::word_win` (frame.rs `row_loop`) and
//! the heap sources have none (heap.rs, heap_pages.rs: a window is one pinned page, its cells deformed per row), so a heap
//! scan's filter tests each row with a branch on its outcome, and a qual that passes a middling share of rows in no order
//! (a one-character code equal to a constant in a quarter of the rows) mispredicts about once per passing row.
//! Here the window's rows go in blocks of up to 64: each row's early cells are read and every qual of the first filter op,
//! and of each next leading filter op whose quals are cheap (`StepMech::row_maskable`: a range's two bounds are two ops),
//! evaluated and ANDed into the row's bit (`StepMech::row_pass`: no branch on a qual's outcome; its raise word kept, not
//! raised), then the block's passing rows walk the chain in order by ctz with those ops' per-row tests skipped
//! (`env.masked` for the first, `Walk::skip` for the rest).
//! - A row whose quals would raise ends the mask before it: the rows before it walk, then the general body takes the window
//!   from that row, its quals evaluated again in plan order (the raise is the row path's own; a later qual of a row an
//!   earlier one drops never raises, D237).
//! - A row the fast body cannot read (`Env::escape`) sends the block, none of whose rows walked yet, to the general body.
//! - A passing row's walk reads back the cells the pass kept (`SourceMech::keep_row`: a fast body's row, its words and
//!   pointers into the pinned page) instead of reading the row again; `row_late` resumes from the kept offset.
//! - A qual that branches on its data inside (an IN list, AND / OR, CASE, COALESCE, GREATEST / LEAST) keeps out of the
//!   mask (filter.rs `row_quals`): the mask cannot take those branches away and would pay the qual for every row.
//! Picked per window by measurement, never by the planner's selectivity (consts.rs): decayed counts of the rows the mask
//! read, its passing rows and its outcome flips (adjacent rows that differ: what the branch could not predict), from the
//! blocks that ran it, against measured op costs: a window runs the mask body while flips x FLIP >= rows x ROW + passes x
//! PASS (`COSTS`: a flip's mispredict saved, the bit a row costs, the walk's second read of a passing row), and every
//! PROBE-th window runs it whatever the counts say, to measure again. Measurement knob (process env, read at lowering):
//! `PGRUST_GRAVITY_MASK_ROWS_COSTS=flip,row,pass` (three integers 0..=1000). Budget 360 lines.
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, IntBinOp, IntCond, IntUnOp, StackSlot, Type, Value};
use mcx::{CollectIn, GrowIn, PgVec};

use crate::frame::env::Env;
use crate::frame::frame::{Mechs, Win};
use crate::cell::Row;
use crate::frame::row::{Carry, RowVals};

crate::strategies! {
    /// The first filter's mask over up to 64 rows of a source without a word window, made row by row; rows by ctz.
    MASK_ROWS = 164, "mask.rows", ScanFilter; // a3-strategy-bits.tsv row 164 (after win.partfill 163)
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(MASK_ROWS, &())], ..crate::ops::NONE };

/// The counters (`Stats::off`): decayed passing rows, decayed flips, windows picked, decayed rows read.
const PASS: usize = 0;
const FLIP: usize = 1;
const WINS: usize = 2;
const NROWS: usize = 3;
/// The counters' frame words (`open::Base::Words`: zeroed at the run's open and kept across its calls, so a run a parallel
/// worker resumes per chunk measures on): the words site and each counter's byte offset (filter.rs `make`).
#[derive(Clone, Copy)]
pub(crate) struct Stats {
    pub(crate) at: Value,
    pub(crate) off: [i32; 4],
}

impl Stats {
    fn get(&self, b: &mut FunctionBuilder<'_>, k: usize) -> Value {
        b.load(Type::I64, self.at, self.off[k])
    }

    fn set(&self, b: &mut FunctionBuilder<'_>, k: usize, v: Value) {
        b.store(self.at, self.off[k], v);
    }
}
/// The rule's costs (flip, row, pass) in cycles: a least-squares fit of the mask body's cycles saved per flip and paid per
/// row read and per passing row walked, eight shapes forced on, Graviton4 (lane brfix, see the commit): 42.5, 8.8, -5.2,
/// the kept row's walk no dearer than the row path's late cells (pass 0: never a credit).
const COSTS: (i64, i64, i64) = (42, 9, 0);
/// Every PROBE-th window runs the mask body whatever the counts say (a power of two).
const PROBE: i64 = 64;
/// Each block's counts are added to 7/8 of those before it.
const DECAY: i64 = 3;

/// The rule's costs: `COSTS`, or the measurement knob's three integers (each 0..=1000; anything else leaves `COSTS`), read
/// off the text in place (nothing allocated).
fn costs() -> (i64, i64, i64) {
    let knob = gravity_driver::env::text("PGRUST_GRAVITY_MASK_ROWS_COSTS").and_then(|t| {
        let mut v = t.split(',').map(|x| x.trim().parse::<i64>().ok().filter(|x| (0..=1000).contains(x)));
        match (v.next(), v.next(), v.next(), v.next()) {
            (Some(Some(f)), Some(Some(r)), Some(Some(p)), None) => Some((f, r, p)),
            _ => None,
        }
    });
    knob.unwrap_or(COSTS)
}

/// Whether this window runs the mask body (`Bool`): every PROBE-th window, else while flips x FLIP >= rows x ROW + passes x
/// PASS.
pub(crate) fn pick(b: &mut FunctionBuilder<'_>, env: &Env, stats: Stats) -> Value {
    let wins = stats.get(b, WINS);
    let w1 = b.int_bin(IntBinOp::Add, wins, env.one);
    stats.set(b, WINS, w1);
    let low = b.iconst(Type::I64, PROBE - 1);
    let phase = b.int_bin(IntBinOp::And, wins, low);
    let probe = b.icmp(IntCond::Eq, phase, env.zero);
    let (pass, flip, rows) = (stats.get(b, PASS), stats.get(b, FLIP), stats.get(b, NROWS));
    let (cf, cr, cp) = costs();
    let (cf, cr, cp) = (b.iconst(Type::I64, cf), b.iconst(Type::I64, cr), b.iconst(Type::I64, cp));
    let gain = b.int_bin(IntBinOp::Mul, flip, cf);
    let row_cost = b.int_bin(IntBinOp::Mul, rows, cr);
    let pass_cost = b.int_bin(IntBinOp::Mul, pass, cp);
    let cost = b.int_bin(IntBinOp::Add, row_cost, pass_cost);
    let good = b.icmp(IntCond::Sge, gain, cost);
    b.int_bin(IntBinOp::Or, probe, good)
}

/// The mask's row walk: row `i` through the chain with the span's tests skipped (`skip`: steps 1.. of the span), over its
/// kept row (`Keep`) where the source kept it, else made again.
pub(crate) type SpanWalk<'a, 'e> = &'a mut dyn FnMut(&mut FunctionBuilder<'_>, &mut Env<'e, '_>, Value, Carry<'e>, u64, Option<&RowVals<'e>>) -> Carry<'e>;

/// A word of a kept row: its index among the row's kept words (a `Bool` kept widened), or a constant made again.
#[derive(Clone, Copy)]
enum Word {
    At(i32, Type),
    K(Type, u64),
}

/// The block's kept rows (`SourceMech::keep_row`): row `t`'s cells (value, NULL flag where it has one) and `row_late`'s
/// resume value, each word not a constant at `slot + (t * kept + k) * 8`, read back by a passing row's walk.
struct Keep<'e> {
    slot: StackSlot,
    kept: i64,
    words: PgVec<'e, Word>,
    nulls: PgVec<'e, bool>,
    cells: Row<'e>,
    cols: PgVec<'e, Option<u16>>,
}

impl<'e> Keep<'e> {
    /// Row `t`'s `row` and resume value `at` stored; `None` (the walk makes its rows again) for a vector or over 32 words.
    fn of(b: &mut FunctionBuilder<'_>, env: &Env<'e, '_>, row: &RowVals<'_>, at: Option<Value>, t: Value) -> Option<Keep<'e>> {
        let flat: PgVec<'_, Value> = row.vals.iter().flat_map(|&(v, n)| core::iter::once(v).chain(n)).chain(at).collect_in(env.mcx);
        let (mut kept, mut words) = (0, PgVec::new_in(env.mcx));
        for &v in flat.iter() {
            let ty = b.value_type(v);
            let w = match b.const_of(v) {
                Some((ty, bits)) => Word::K(ty, bits),
                None if ty.is_vector() => return None,
                None => Word::At(kept, ty),
            };
            kept += matches!(w, Word::At(..)) as i32;
            words.push_in(w);
        }
        if kept > 32 {
            return None;
        }
        let slot = b.declare_stack_slot(64 * 8 * kept.max(1) as u32, 8);
        let (base, row_at) = Self::at(b, slot, t, kept);
        for (&v, w) in flat.iter().zip(words.iter()) {
            if let Word::At(k, ty) = *w {
                let v = if ty == Type::Bool { b.conv(ConvOp::Zext, Type::I64, v) } else { v };
                let p = b.ptr_add(base, row_at, 8, k * 8);
                b.store(p, 0, v);
            }
        }
        let nulls = row.vals.iter().map(|v| v.1.is_some()).collect_in(env.mcx);
        let cells = Row { cells: mcx::to_vec_in(&row.row.cells[..], env.mcx) };
        Some(Keep { slot, kept: kept as i64, words, nulls, cells, cols: mcx::to_vec_in(&row.cols[..], env.mcx) })
    }

    /// The slot's address and row `t`'s first word's index.
    fn at(b: &mut FunctionBuilder<'_>, slot: StackSlot, t: Value, kept: i32) -> (Value, Value) {
        let base = b.stack_addr(slot);
        let width = b.iconst(Type::I64, kept as i64);
        (base, b.int_bin(IntBinOp::Mul, t, width))
    }

    /// Row `t`'s kept row as row `i` of the window (its carry `carry`), and its resume value.
    fn read(&self, b: &mut FunctionBuilder<'_>, env: &Env<'e, '_>, t: Value, i: Value, carry: Carry<'e>) -> (RowVals<'e>, Option<Value>) {
        let (base, row_at) = Self::at(b, self.slot, t, self.kept as i32);
        let mut read = |w: Word| match w {
            Word::K(ty, bits) => b.const_raw(ty, bits),
            Word::At(k, Type::Bool) => {
                let p = b.ptr_add(base, row_at, 8, k * 8);
                let w = b.load(Type::I64, p, 0);
                b.icmp(IntCond::Ne, w, env.zero)
            }
            Word::At(k, ty) => {
                let p = b.ptr_add(base, row_at, 8, k * 8);
                b.load(ty, p, 0)
            }
        };
        let words: PgVec<'_, Value> = self.words.iter().map(|&w| read(w)).collect_in(env.mcx);
        let mut words = words.into_iter();
        let mut vals = PgVec::new_in(env.mcx);
        for &null in self.nulls.iter() {
            let v = words.next().unwrap_or(env.zero);
            vals.push_in((v, if null { words.next() } else { None }));
        }
        let resume = words.next();
        let next = b.int_bin(IntBinOp::Add, i, env.one);
        let cells = Row { cells: mcx::to_vec_in(&self.cells.cells[..], env.mcx) };
        (RowVals { row: cells, vals, cols: mcx::to_vec_in(&self.cols[..], env.mcx), carry, next, at: i }, resume)
    }
}

/// The mask body `l_in(j, carry…)` from row `j` to the window's end, then `l_wend(carry…)`; the fast body's facts in
/// `env.body` and the general body's entry `l_gen(j, carry…)` in `env.escape` (frame.rs sets both). Per block of
/// `n = min(64, end - j)` rows: each row's bit from the span's `row_pass` over its early cells (`SourceMech::row_early`
/// where the source decodes late), the counters, then the passing rows through `walk` (the chain, the span's tests
/// skipped); a raising row's block hands the window to `l_gen` at that row after the rows before it walked.
pub(crate) fn body<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, m: &Mechs<'_>, win: &Win, stats: Stats, (l_in, l_wend, l_gen): (Block, Block, Block), walk: SpanWalk<'_, 'e>) {
    fires::fire("mask.rows"); // shapecheck fire signal (brfix): a window may run the first filter's row-made mask
    // the span: step 0 and each next leading filter op whose test joins the mask (at most 63 steps: skip bits 1..)
    let span = 1 + m.steps[1..].iter().take(62).take_while(|s| s.row_maskable()).count();
    let skip = ((1u64 << span) - 1) & !1;
    b.switch_to(l_in);
    let types: PgVec<'_, Type> = b.block_params(l_in).iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
    let p = mcx::to_vec_in(b.block_params(l_in), env.mcx);
    let l_blk = b.create_block(&types);
    b.br(l_blk, &p);
    b.mark_loop_header(l_blk);
    env.bounded.push_in(l_blk);
    b.switch_to(l_blk);
    let p = mcx::to_vec_in(b.block_params(l_blk), env.mcx);
    let j = p[0];
    let more = b.icmp(IntCond::Slt, j, win.end);
    let l_pre = b.create_block(&[]);
    b.cond_br(more, (l_pre, &[]), (l_wend, &p[1..]));
    b.switch_to(l_pre);
    let (sixty_four, sixty_three) = (b.iconst(Type::I64, 64), b.iconst(Type::I64, 63));
    let left = b.int_bin(IntBinOp::Sub, win.end, j);
    let n = b.int_bin(IntBinOp::Umin, left, sixty_four);
    // the bits `l_bit(t, mask, errs)`: row j + t's pass bit and raise bit at t
    let (l_bit, l_done) = (b.create_block(&[Type::I64; 3]), b.create_block(&[Type::I64; 2]));
    b.br(l_bit, &[env.zero, env.zero, env.zero]);
    b.mark_loop_header(l_bit);
    env.bounded.push_in(l_bit);
    b.switch_to(l_bit);
    let q = mcx::to_vec_in(b.block_params(l_bit), env.mcx);
    let (t, mask, errs) = (q[0], q[1], q[2]);
    let go = b.icmp(IntCond::Slt, t, n);
    let (l_row, l_esc) = (b.create_block(&[]), b.create_block(&types));
    b.cond_br(go, (l_row, &[]), (l_done, &[mask, errs]));
    b.switch_to(l_row);
    let saved = env.escape.map(|_| l_esc);
    let saved = core::mem::replace(&mut env.escape, saved);
    env.memo.clear();
    if gravity_driver::mem::reset_unit() == gravity_program::Until::Row {
        env.hir.reset(b);
    }
    let i = b.int_bin(IntBinOp::Add, j, t);
    let next = b.int_bin(IntBinOp::Add, i, env.one);
    let carry = Carry::from_params(&p[1..], env.mcx);
    let late = m.source.late_at();
    let early = late > 0 && late <= m.steps.len();
    let row = match early {
        true => m.source.row_early(b, env, win, i, carry, next),
        false => m.source.row(b, env, win, i, carry, next),
    };
    // the row kept for its walk where the source keeps it (else the walk makes it again)
    let keep = m.source.keep_row(early).and_then(|at| Keep::of(b, env, &row, at, t));
    let (mut pass, mut err) = (None, None);
    for (k, s) in m.steps[..span].iter().enumerate() {
        env.step = Some(k);
        let Some((p, e)) = s.row_pass(b, env, &row) else {
            crate::refuse!(env.node, crate::admit::refusal::Reason::Bug { payload: "a mask.rows step without its row test" })
        };
        pass = Some(pass.map_or(p, |a| b.int_bin(IntBinOp::And, a, p)));
        if !b.const_of(e).is_some_and(|(_, c)| c == 0) {
            err = Some(err.map_or(e, |a| b.int_bin(IntBinOp::Or, a, e)));
        }
    }
    env.step = None;
    let Some(pass) = pass else { crate::refuse!(env.node, crate::admit::refusal::Reason::Bug { payload: "a mask.rows span without a step" }) };
    env.memo.clear();
    env.escape = saved;
    let at = |b: &mut FunctionBuilder<'_>, v: Value| {
        let w = b.conv(ConvOp::Zext, Type::I64, v);
        b.int_bin(IntBinOp::Shl, w, t)
    };
    let pb = at(b, pass);
    let mask1 = b.int_bin(IntBinOp::Or, mask, pb);
    // a span none of whose quals can raise keeps no raise bits
    let errs1 = match err {
        Some(e) => {
            let raised = b.icmp(IntCond::Ne, e, env.zero);
            let eb = at(b, raised);
            b.int_bin(IntBinOp::Or, errs, eb)
        }
        None => errs,
    };
    let t1 = b.int_bin(IntBinOp::Add, t, env.one);
    b.br(l_bit, &[t1, mask1, errs1]);
    // a row the fast body cannot read: the whole block (none of its rows walked) to the general body (a source whose rows
    // never escape leaves the block empty, for the program's prune)
    if crate::frame::frame::reached(b, l_esc) {
        b.switch_to(l_esc);
        b.br(l_gen, &p);
    }
    b.switch_to(l_done);
    let q = mcx::to_vec_in(b.block_params(l_done), env.mcx);
    let (mask, errs) = (q[0], q[1]);
    // the first raising row ends the mask: rows [j, j + stop) walk
    let any = b.icmp(IntCond::Ne, errs, env.zero);
    let te = b.int_un(IntUnOp::Ctz, errs);
    let te = b.int_bin(IntBinOp::And, te, sixty_three);
    let bit = b.int_bin(IntBinOp::Shl, env.one, te);
    let below = b.int_bin(IntBinOp::Sub, bit, env.one);
    let cut = b.int_bin(IntBinOp::And, mask, below);
    let mask = b.select(any, cut, mask);
    let stop = b.select(any, te, n);
    count(b, env, stats, mask, n);
    // the walk `l_w(bits, carry…)`, then the next block or (a raising row) the general body from it
    let wt: PgVec<'_, Type> = core::iter::once(Type::I64).chain(types[1..].iter().copied()).collect_in(env.mcx);
    let (l_w, l_one, l_after) = (b.create_block(&wt), b.create_block(&[]), b.create_block(&types[1..]));
    let mut first = mcx::pg_vec![in env.mcx; mask];
    first.extend_from_slice_in(&p[1..]);
    b.br(l_w, &first);
    b.mark_loop_header(l_w);
    env.bounded.push_in(l_w);
    b.switch_to(l_w);
    let q = mcx::to_vec_in(b.block_params(l_w), env.mcx);
    let empty = b.icmp(IntCond::Eq, q[0], env.zero);
    b.cond_br(empty, (l_after, &q[1..]), (l_one, &[]));
    b.switch_to(l_one);
    let tz = b.int_un(IntUnOp::Ctz, q[0]);
    let i = b.int_bin(IntBinOp::Add, j, tz);
    let low = b.int_bin(IntBinOp::Sub, q[0], env.one);
    let rest = b.int_bin(IntBinOp::And, q[0], low);
    let carry = Carry::from_params(&q[1..], env.mcx);
    let out = match keep.as_ref() {
        Some(k) => {
            let (row, resume) = k.read(b, env, tz, i, carry.copy_in(env.mcx));
            if let Some(at) = resume {
                m.source.resume_late(at);
            }
            walk(b, env, i, carry, skip, Some(&row))
        }
        None => walk(b, env, i, carry, skip, None),
    };
    let mut back = mcx::pg_vec![in env.mcx; rest];
    back.extend_in(out.params());
    b.br(l_w, &back);
    b.switch_to(l_after);
    let c = mcx::to_vec_in(b.block_params(l_after), env.mcx);
    let (jn, js) = (b.int_bin(IntBinOp::Add, j, n), b.int_bin(IntBinOp::Add, j, stop));
    let on: PgVec<'_, Value> = core::iter::once(jn).chain(c.iter().copied()).collect_in(env.mcx);
    let gen: PgVec<'_, Value> = core::iter::once(js).chain(c.iter().copied()).collect_in(env.mcx);
    b.cond_br(any, (l_gen, &gen), (l_blk, &on));
}

/// The block's counts into the counters: its `n` rows, its passing rows and its flips (rows `t`, `t + 1` of its `n` that
/// differ), added to 7/8 of the counts before.
fn count(b: &mut FunctionBuilder<'_>, env: &Env, stats: Stats, mask: Value, n: Value) {
    let (sixty_three, decay) = (b.iconst(Type::I64, 63), b.iconst(Type::I64, DECAY));
    let pop = b.int_un(IntUnOp::Popcnt, mask);
    let shifted = b.int_bin(IntBinOp::Ushr, mask, env.one);
    let diff = b.int_bin(IntBinOp::Xor, mask, shifted);
    let n1 = b.int_bin(IntBinOp::Sub, n, env.one);
    let n1 = b.int_bin(IntBinOp::And, n1, sixty_three);
    let top = b.int_bin(IntBinOp::Shl, env.one, n1);
    let pairs = b.int_bin(IntBinOp::Sub, top, env.one);
    let diff = b.int_bin(IntBinOp::And, diff, pairs);
    let flips = b.int_un(IntUnOp::Popcnt, diff);
    for (off, add) in [(PASS, pop), (FLIP, flips), (NROWS, n)] {
        let old = stats.get(b, off);
        let part = b.int_bin(IntBinOp::Ushr, old, decay);
        let kept = b.int_bin(IntBinOp::Sub, old, part);
        let new = b.int_bin(IntBinOp::Add, kept, add);
        stats.set(b, off, new);
    }
}
