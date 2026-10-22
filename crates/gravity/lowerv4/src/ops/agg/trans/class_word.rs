//! Count / sum / avg / min / max / bool / bit over Word. Hooks / levers: -. Budget 220 lines (files.tsv, +25 % gate).
//! Owner M1-FOLD.
//!
//! Registers: kind `class.word` (`KindId::ClassWord`) (D252 gives the bit aggregates their record slots). PG's
//! transitions: `count` (`int8inc*`), `sum(int2 / int4)` in an unchecked int8 (`int4_sum`), `sum(int8)` / `avg(int8)`
//! exact in 128 bits rendered as numeric (`int8_sum` / `int8_avg`), `avg(int2 / int4)`'s int8 sum (`int4_avg_accum`),
//! min / max (an unsigned type through its sign-flipped image), `bool_and / bool_or` as min / max of 0 / 1, `bit_*`
//! strict with no initcond. Register words are `[value…, n]`, `n` the rows the state holds (a count's only word); an
//! empty state holds its identity, so a step and a merge need no guard, and a slot reads back the same way.
use gravity_decompose::ops::shared::agg::{AggDecl, Register as R};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, Type, Value};
use mcx::{GrowIn, Mcx, PgVec};

use crate::face::fold::{Acc, AccShape, AnswerForm, BlockFold, ClassImpl, FinalOrder, FoldClass};
use crate::face::kinds::KindId;
use crate::frame::env::Env;
use crate::admit::refusal::Reason;
use crate::rep::CellRef;

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { kinds: &[(KindId::ClassWord, &ClassImpl(&WORD))], ..crate::ops::NONE };

pub(crate) struct Word;
pub(crate) static WORD: Word = Word;

#[derive(Clone, Copy, PartialEq, Eq)]
enum F {
    Count,
    /// `sum(int2 / int4)`: `[s, n]`.
    Sum,
    /// `sum(int8)`: `[lo, hi, n]`; `avg(int8)` the same with the count in its slot.
    Wide { avg: bool },
    /// `avg(int2 / int4)`: `[s, n]` in registers, the slot's `[lo, hi, n]`.
    Narrow,
    Min { img: bool },
    Max { img: bool },
    Bits(IntBinOp),
}

fn form(a: &AggDecl<'_>) -> F {
    match a.reg {
        R::CountStar | R::CountAny | R::CountPair => F::Count,
        R::SumInt => F::Sum,
        R::SumInt128 => F::Wide { avg: false },
        R::AvgInt128 => F::Wide { avg: true },
        R::AvgInt => F::Narrow,
        R::MinInt | R::BoolAnd | R::MinImage => F::Min { img: a.reg == R::MinImage },
        R::MaxInt | R::BoolOr | R::MaxImage => F::Max { img: a.reg == R::MaxImage },
        R::BitAnd => F::Bits(IntBinOp::And),
        R::BitOr => F::Bits(IntBinOp::Or),
        R::BitXor => F::Bits(IntBinOp::Xor),
        _ => crate::refuse!(0, Reason::Bug { payload: "class.word over another register" }),
    }
}

/// The identity of the value word (an empty state's).
fn ident(f: F) -> i64 {
    match f {
        F::Min { .. } => i64::MAX,
        F::Max { .. } => i64::MIN,
        F::Bits(IntBinOp::And) => -1,
        _ => 0,
    }
}

fn regs(f: F) -> usize {
    match f {
        F::Count => 1,
        F::Wide { .. } => 3,
        _ => 2,
    }
}

/// The state's words: registers as they are; a slot's words, `n` from its count or its validity, an invalid value word
/// read as the identity.
fn load<'e>(b: &mut FunctionBuilder<'_>, env: &Env<'e, '_>, f: F, acc: &Acc<'_>) -> PgVec<'e, Value> {
    let (base, off, valid) = match acc {
        Acc::Regs(r) => return mcx::to_vec_in(&r[..], env.mcx),
        Acc::Slot { base, off, valid } => (*base, *off, *valid),
    };
    let w = b.load(Type::I64, base, off);
    if f == F::Count {
        return mcx::pg_vec![in env.mcx; w];
    }
    let ok = crate::ops::shared::slots::slot_ok(b, env, base, valid);
    let n = if matches!(f, F::Wide { avg: true } | F::Narrow) { b.load(Type::I64, base, off + 16) } else { crate::ops::agg::trans::plain::n_of(b, env, ok) };
    let id = b.iconst(Type::I64, ident(f));
    let v = b.select(ok, w, id);
    match f {
        F::Wide { .. } | F::Narrow => {
            let hi = b.load(Type::I64, base, off + 8);
            let h = b.select(ok, hi, env.zero);
            mcx::pg_vec![in env.mcx; v, h, n]
        }
        _ => mcx::pg_vec![in env.mcx; v, n],
    }
}

/// `[s, n]` of a narrow average widened to `[lo, hi, n]`.
fn widen<'e>(b: &mut FunctionBuilder<'_>, mcx: Mcx<'e>, w: &[Value]) -> PgVec<'e, Value> {
    if w.len() == 3 {
        return mcx::to_vec_in(w, mcx);
    }
    let s63 = b.iconst(Type::I64, 63);
    let hi = b.int_bin(IntBinOp::Sshr, w[0], s63);
    mcx::pg_vec![in mcx; w[0], hi, w[1]]
}

fn store(b: &mut FunctionBuilder<'_>, env: &Env, f: F, acc: Acc<'_>, w: &[Value]) {
    match acc {
        // a narrow average's registers keep the sum's low word (PG's int8 sum) and the count
        Acc::Regs(r) if r.len() == 2 && w.len() == 3 => r.copy_from_slice(&[w[0], w[2]]),
        Acc::Regs(r) => r.copy_from_slice(&w[..r.len()]),
        Acc::Slot { base, off, valid } => {
            let n = *w.last().unwrap();
            let w = if f == F::Narrow { widen(b, env.mcx, w) } else { mcx::to_vec_in(w, env.mcx) };
            b.store(base, off, w[0]);
            if w.len() == 3 {
                b.store(base, off + 8, w[1]);
            }
            if matches!(f, F::Wide { avg: true } | F::Narrow) {
                b.store(base, off + 16, n);
            }
            // a count's bit too: abi's `Rec::fold` merges only a valid slot, as its `add_count` marks it (D271)
            let any = b.icmp(IntCond::Ne, n, env.zero);
            crate::ops::shared::slots::mark(b, env, base, valid, any);
        }
    }
}

/// `x` and `y` combined (both identity-seeded): PG's combinefn.
fn combine<'e>(b: &mut FunctionBuilder<'_>, mcx: Mcx<'e>, f: F, x: &[Value], y: &[Value]) -> PgVec<'e, Value> {
    let (x, y) = if x.len() != y.len() { (widen(b, mcx, x), widen(b, mcx, y)) } else { (mcx::to_vec_in(x, mcx), mcx::to_vec_in(y, mcx)) };
    let k = x.len() - 1;
    let n = b.int_bin(IntBinOp::Add, x[k], y[k]);
    let mut out = match (f, k) {
        (F::Count, _) => mcx::PgVec::new_in(mcx),
        (_, 2) => {
            let (lo, hi) = add128(b, x[0], x[1], y[0], y[1]);
            mcx::pg_vec![in mcx; lo, hi]
        }
        (F::Min { .. }, _) => mcx::pg_vec![in mcx; b.int_bin(IntBinOp::Smin, x[0], y[0])],
        (F::Max { .. }, _) => mcx::pg_vec![in mcx; b.int_bin(IntBinOp::Smax, x[0], y[0])],
        (F::Bits(op), _) => mcx::pg_vec![in mcx; b.int_bin(op, x[0], y[0])],
        _ => mcx::pg_vec![in mcx; b.int_bin(IntBinOp::Add, x[0], y[0])],
    };
    out.push_in(n);
    out
}

/// `(alo, ahi) + (blo, bhi)` in 128 bits: `iadd` + `addhi` (`adds` + `adc`).
pub(crate) fn add128(b: &mut FunctionBuilder<'_>, alo: Value, ahi: Value, blo: Value, bhi: Value) -> (Value, Value) {
    (b.int_bin(IntBinOp::Add, alo, blo), b.add_hi(alo, blo, ahi, bhi))
}

/// One row's state `[v…, n]` (the identity where the row adds nothing): `m` rows of it under join.mult (H20), exact as
/// `m` transitions (wrapping int8 sums, a 128-bit product, an odd count for `bit_xor`, min / max / and / or unchanged).
fn row_words<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, f: F, a: &AggDecl<'_>, args: &[CellRef<'_>]) -> PgVec<'e, Value> {
    let (v, ok) = crate::ops::agg::trans::plain::args_ok(b, env, a, args);
    let m = env.mult.unwrap_or(env.one);
    let one = crate::ops::agg::trans::plain::n_of(b, env, ok);
    let n = if env.mult.is_some() { b.select(ok, m, env.zero) } else { one };
    let id = b.iconst(Type::I64, ident(f));
    let flip = b.iconst(Type::I64, i64::MIN);
    let v = match f {
        F::Min { img: true } | F::Max { img: true } => b.int_bin(IntBinOp::Xor, v, flip),
        _ => v,
    };
    let x = match (f, env.mult) {
        (F::Sum | F::Narrow | F::Wide { .. }, Some(m)) => b.int_bin(IntBinOp::Mul, v, m),
        (F::Bits(IntBinOp::Xor), Some(m)) => {
            let odd = b.int_bin(IntBinOp::And, m, env.one);
            let odd = b.icmp(IntCond::Ne, odd, env.zero);
            b.select(odd, v, env.zero)
        }
        _ => v,
    };
    let x = b.select(ok, x, id);
    match (f, env.mult) {
        (F::Count, _) => mcx::pg_vec![in env.mcx; n],
        (F::Wide { .. }, Some(m)) => {
            let h = b.int_bin(IntBinOp::Smulh, v, m);
            let h = b.select(ok, h, env.zero);
            mcx::pg_vec![in env.mcx; x, h, n]
        }
        (F::Wide { .. }, None) => widen(b, env.mcx, &[x, n]),
        _ => mcx::pg_vec![in env.mcx; x, n],
    }
}

impl FoldClass for Word {
    fn class_kind(&self) -> Option<KindId> {
        Some(KindId::ClassWord)
    }

    fn shape(&self, a: &AggDecl<'_>) -> AccShape {
        let f = form(a);
        let bytes = match f {
            F::Wide { avg: true } | F::Narrow => 24,
            F::Wide { avg: false } => 16,
            _ => 8,
        };
        AccShape { words: regs(f) as u8, bytes, align: 8 }
    }

    fn init(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>) {
        let f = form(a);
        let mut w = mcx::pg_vec![in env.mcx; env.zero; regs(f)];
        w[0] = if f == F::Count { env.zero } else { b.iconst(Type::I64, ident(f)) };
        store(b, env, f, acc, &w[..]);
    }

    fn step(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>, args: &[CellRef<'_>]) {
        let f = form(a);
        let row = row_words(b, env, f, a, args);
        let cur = load(b, env, f, &acc);
        let new = combine(b, env.mcx, f, &cur[..], &row[..]);
        store(b, env, f, acc, &new[..]);
    }

    fn merge(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, into: Acc<'_>, from: Acc<'_>) {
        let f = form(a);
        let (x, y) = (load(b, env, f, &into), load(b, env, f, &from));
        let new = combine(b, env.mcx, f, &x[..], &y[..]);
        store(b, env, f, into, &new[..]);
    }

    fn puts(&self, _a: &AggDecl<'_>) -> bool {
        true
    }

    /// V4R-215: `combine(identity, y) == y`: the registers stored as they are (no init store, no reload).
    fn put(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, into: Acc<'_>, from: &[Value]) {
        store(b, env, form(a), into, from);
    }

    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>) -> (Value, Value) {
        let f = form(a);
        let w = load(b, env, f, &acc);
        let n = *w.last().unwrap();
        let null = b.icmp(IntCond::Eq, n, env.zero);
        match f {
            F::Count => (n, b.bconst(false)),
            F::Wide { .. } | F::Narrow => {
                let w = widen(b, env.mcx, &w[..]);
                let (name, host) = if matches!(f, F::Wide { avg: false }) { ("gravity_expr::int8_sum", gravity_abi::host_words!(gravity_expr::emit::agg::int8_sum(region, lo, hi, n))) } else { ("gravity_expr::int8_avg", gravity_abi::host_words!(gravity_expr::emit::agg::int8_avg(region, lo, hi, n))) };
                (crate::ops::agg::trans::plain::host_final(b, env, name, host, &w[..], null), null)
            }
            F::Min { img: true } | F::Max { img: true } => {
                let flip = b.iconst(Type::I64, i64::MIN);
                (b.int_bin(IntBinOp::Xor, w[0], flip), null)
            }
            _ => (w[0], null),
        }
    }

    fn order(&self, a: &AggDecl<'_>) -> Option<FinalOrder> {
        match form(a) {
            F::Wide { avg: true } | F::Narrow => Some(FinalOrder::Rational),
            F::Wide { avg: false } | F::Min { img: true } | F::Max { img: true } => None,
            _ => Some(FinalOrder::Word),
        }
    }

    /// FOLD-13: the class decides, not a register list: a count, a one-word sum and a min / max over any fixed-width
    /// word order (signed, or unsigned through its flipped image: oid, pg_lsn, xid8; bool_and / bool_or over 0 / 1) fold
    /// by block, reading the lane as the row path does. A 128-bit sum (two words carried) and the bitwise folds walk rows:
    /// their block loop, one per aggregate, costs more than the walk once half a block passes (pgrcolumnar A/B, 1M rows:
    /// +6 % to +11 % at 50 % and 90 % selectivity, -2 % to -6 % at 2 %); the min / max forms are never slower.
    fn block(&self, a: &AggDecl<'_>) -> Option<BlockFold> {
        match form(a) {
            F::Count => Some(BlockFold::Count),
            F::Sum | F::Narrow => Some(BlockFold::Sum),
            F::Min { img: false } => Some(BlockFold::Min),
            F::Max { img: false } => Some(BlockFold::Max),
            F::Min { img: true } => Some(BlockFold::MinImage),
            F::Max { img: true } => Some(BlockFold::MaxImage),
            F::Wide { .. } | F::Bits(_) => None,
        }
    }

    /// NEW-47: min / max of an unsigned 4-byte word (oid) answer too: pgrc keys it zero-extended (`UnsignedWordSmall`), an
    /// exact key the host answers as the lane's signed 4-byte word; an 8-byte one's key is already flipped (pg_lsn, xid8: no).
    fn answer(&self, a: &AggDecl<'_>) -> Option<AnswerForm> {
        let small = || a.node.as_aggref().is_some_and(|r| gravity_decompose::ops::shared::scan::zero_extended(r.aggtype));
        let image = matches!(a.reg, R::MinImage | R::MaxImage) && small();
        (image || matches!(a.reg, R::CountStar | R::CountAny | R::SumInt | R::SumInt128 | R::AvgInt | R::AvgInt128 | R::MinInt | R::MaxInt)).then_some(AnswerForm::Stats)
    }
}
