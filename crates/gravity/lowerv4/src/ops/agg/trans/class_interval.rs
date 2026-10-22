//! Interval folds (the interval kinds). Hooks / levers: -. Budget 200 lines.
//! Owner M1-FOLD.
//!
//! Dispatched from kind `class.other` by register (class_other.rs: no kind of its own). `min / max(interval)`
//! (decompose `MinInterval` / `MaxInterval`, abi `IntervalMin` / `IntervalMax`): the state is PG's Interval as two words,
//! `time` and `day | month << 32` (its 16 bytes as stored), then the valid word. Values order as `interval_cmp_value`
//! (time + (month * 30 + day) * USECS_PER_DAY), here as its exact 128-bit value (the day count times USECS_PER_DAY by `Mul`
//! and `Smulh`, plus time sign-extended, class_word.rs `add128`), compared as a signed pair: no division, the infinities at
//! the ends, paid only where the day counts differ. A NULL row folds nothing (a branch). Equal values can print apart ('1 day' /
//! '24:00:00'), and PG keeps the second argument of `interval_smaller / larger` on a tie: the row over the state, the later
//! part over the earlier, so a tie takes the incoming value (abi `Rec::fold_interval`, the record merge, is the same rule).
//! decompose keeps a call under ORDER BY or DISTINCT on PG's own fold, whose order is not the fold's.
use gravity_decompose::ops::shared::agg::{AggDecl, Register as R};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, IntCond, Type, Value};

use crate::admit::refusal::Reason;
use crate::face::fold::{Acc, AccShape, FoldClass};
use crate::frame::env::Env;
use crate::rep::CellRef;

pub(crate) struct Interval;
pub(crate) static INTERVAL: Interval = Interval;

/// PG's USECS_PER_DAY.
const USECS_PER_DAY: i64 = 86_400_000_000;

fn is_min(env: &Env, a: &AggDecl<'_>) -> bool {
    match a.reg {
        R::MinInterval => true,
        R::MaxInterval => false,
        _ => crate::refuse!(env.node, Reason::Bug { payload: "class.other's interval fold over another register" }),
    }
}

/// `[time, day | month << 32, valid]`: a slot's words under its valid bit (zero when empty), registers as they are.
fn load(b: &mut FunctionBuilder<'_>, env: &Env, acc: &Acc<'_>) -> [Value; 3] {
    match acc {
        Acc::Regs(w) => [w[0], w[1], w[2]],
        Acc::Slot { base, off, valid } => {
            let ok = crate::ops::shared::slots::slot_ok(b, env, *base, *valid);
            let (t, dm) = (b.load(Type::I64, *base, *off), b.load(Type::I64, *base, off + 8));
            let (t, dm) = (b.select(ok, t, env.zero), b.select(ok, dm, env.zero));
            [t, dm, b.conv(ConvOp::Zext, Type::I64, ok)]
        }
    }
}

fn store(b: &mut FunctionBuilder<'_>, env: &Env, acc: Acc<'_>, w: [Value; 3]) {
    match acc {
        Acc::Regs(r) => r.copy_from_slice(&w),
        Acc::Slot { base, off, valid } => {
            b.store(base, off, w[0]);
            b.store(base, off + 8, w[1]);
            let any = b.icmp(IntCond::Ne, w[2], env.zero);
            crate::ops::shared::slots::mark(b, env, base, valid, any);
        }
    }
}

/// `interval_cmp_value` as its exact i128, `(high, low)` words: the day count `month * 30 + day` (at most 2^37 in
/// magnitude) times USECS_PER_DAY in 128 bits, plus time sign-extended.
fn span(b: &mut FunctionBuilder<'_>, t: Value, dm: Value) -> (Value, Value) {
    let day = b.conv(ConvOp::Trunc, Type::I32, dm);
    let day = b.conv(ConvOp::Sext, Type::I64, day);
    let s32 = b.iconst(Type::I64, 32);
    let month = b.int_bin(IntBinOp::Sshr, dm, s32);
    let thirty = b.iconst(Type::I64, 30);
    let days = b.int_bin(IntBinOp::Mul, month, thirty);
    let days = b.int_bin(IntBinOp::Add, days, day);
    let (du, s63) = (b.iconst(Type::I64, USECS_PER_DAY), b.iconst(Type::I64, 63));
    let (lo, hi) = (b.int_bin(IntBinOp::Mul, days, du), b.int_bin(IntBinOp::Smulh, days, du));
    let th = b.int_bin(IntBinOp::Sshr, t, s63);
    let (lo, hi) = crate::ops::agg::trans::class_word::add128(b, lo, hi, t, th);
    (hi, lo)
}

/// Whether `x` orders strictly before `y` (`interval_cmp_internal(x, y) < 0`): the high words signed, then the low unsigned.
fn lt(b: &mut FunctionBuilder<'_>, x: (Value, Value), y: (Value, Value)) -> Value {
    let (dl, de) = (b.icmp(IntCond::Slt, x.0, y.0), b.icmp(IntCond::Eq, x.0, y.0));
    let ul = b.icmp(IntCond::Ult, x.1, y.1);
    let tie = b.int_bin(IntBinOp::And, de, ul);
    b.int_bin(IntBinOp::Or, dl, tie)
}

/// `interval_smaller / larger(s, v)` under the valid words: `s` stays only when it is valid and strictly smaller
/// (min) / larger (max) than a valid `v`; an invalid `v` leaves `s`. Branches, not selects (perf: a run carry merges into
/// its slot at every key change, so unclustered keys paid four spans a row): an empty side, or one known empty or valid
/// at compile time, folds no span, and equal `day | month` words order by `time` alone (the spans differ by exactly that).
fn fold(b: &mut FunctionBuilder<'_>, env: &Env, min: bool, s: [Value; 3], v: [Value; 3]) -> [Value; 3] {
    let known = |b: &FunctionBuilder<'_>, x: Value| b.const_of(x).map(|(_, c)| c != 0);
    match (known(b, s[2]), known(b, v[2])) {
        (_, Some(false)) => return s,
        (Some(false), _) => return v,
        _ => {}
    }
    let one = b.iconst(Type::I64, 1);
    let out = b.create_block(&[Type::I64; 3]);
    if known(b, v[2]).is_none() {
        let (on, v_ok) = (b.create_block(&[]), b.icmp(IntCond::Ne, v[2], env.zero));
        b.cond_br(v_ok, (on, &[]), (out, &s));
        b.switch_to(on);
    }
    if known(b, s[2]).is_none() {
        let (on, s_ok) = (b.create_block(&[]), b.icmp(IntCond::Ne, s[2], env.zero));
        b.cond_br(s_ok, (on, &[]), (out, &[v[0], v[1], one]));
        b.switch_to(on);
    }
    let (l_eq, l_ne) = (b.create_block(&[]), b.create_block(&[]));
    let same = b.icmp(IntCond::Eq, s[1], v[1]);
    b.cond_br(same, (l_eq, &[]), (l_ne, &[]));
    b.switch_to(l_eq);
    let keep = if min { b.icmp(IntCond::Slt, s[0], v[0]) } else { b.icmp(IntCond::Slt, v[0], s[0]) };
    let t = b.select(keep, s[0], v[0]);
    b.br(out, &[t, s[1], one]);
    b.switch_to(l_ne);
    let (ss, vs) = (span(b, s[0], s[1]), span(b, v[0], v[1]));
    let keep = if min { lt(b, ss, vs) } else { lt(b, vs, ss) };
    let w = [b.select(keep, s[0], v[0]), b.select(keep, s[1], v[1]), one];
    b.br(out, &w);
    b.switch_to(out);
    let p = b.block_params(out);
    [p[0], p[1], p[2]]
}


impl FoldClass for Interval {
    fn shape(&self, _a: &AggDecl<'_>) -> AccShape {
        AccShape { words: 3, bytes: 16, align: 8 }
    }

    fn init(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>) {
        is_min(env, a);
        store(b, env, acc, [env.zero; 3]);
    }

    fn step(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>, args: &[CellRef<'_>]) {
        let min = is_min(env, a);
        if args.len() != 1 {
            crate::refuse!(env.node, Reason::Bug { payload: "class.other's interval fold over other than one argument" });
        }
        let (d, ok) = crate::ops::agg::trans::plain::args_ok(b, env, a, args);
        // a NULL or FILTER-dropped row folds nothing: the state passes the branch unchanged (a NULL's datum is no address)
        let s = load(b, env, &acc);
        let (l_do, l_j) = (b.create_block(&[]), b.create_block(&[Type::I64, Type::I64, Type::I64]));
        b.cond_br(ok, (l_do, &[]), (l_j, &s));
        b.switch_to(l_do);
        let p = b.conv(ConvOp::IntToPtr, Type::Ptr, d);
        let (t, dm) = (b.load_unaligned(Type::I64, p, 0), b.load_unaligned(Type::I64, p, 8));
        let one = b.iconst(Type::I64, 1);
        let w = fold(b, env, min, s, [t, dm, one]);
        b.br(l_j, &w);
        b.switch_to(l_j);
        let w = [b.block_params(l_j)[0], b.block_params(l_j)[1], b.block_params(l_j)[2]];
        store(b, env, acc, w);
    }

    fn merge(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, into: Acc<'_>, from: Acc<'_>) {
        let min = is_min(env, a);
        let (x, y) = (load(b, env, &into), load(b, env, &from));
        let w = fold(b, env, min, x, y);
        store(b, env, into, w);
    }

    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>) -> (Value, Value) {
        is_min(env, a);
        let w = load(b, env, &acc);
        let none = b.icmp(IntCond::Eq, w[2], env.zero);
        let host = crate::ops::agg::raw::interval_box as *const () as usize;
        let v = crate::ops::agg::trans::slots::host_final(b, env, "class_interval::interval_box", host, &[w[0], w[1]], none);
        (v, none)
    }
}
