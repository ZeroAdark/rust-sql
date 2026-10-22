//! Interval sums (the interval kinds). Hooks / levers: -. Budget 380 lines.
//! Owner M1-FOLD.
//!
//! Dispatched from kind `class.other` by register (class_other.rs: no kind of its own). `sum / avg(interval)` (decompose
//! `SumInterval` / `AvgInterval` over an abi `IntervalSum` slot; a call under ORDER BY, DISTINCT or a split keeps the hosted
//! fold): the state is abi `IntervalSum`'s eight words (the sum's `time` and `day | month << 32`, PG's Interval image; the
//! absolute sums of time, day and month; N; `pinf | ninf << 32`; the flags), then the valid word. A NULL row folds nothing;
//! an infinity counts (a cold block: its time is i64::MAX / MIN); a finite value adds to N and the sum. Under the
//! statement's recheck licence (facts/order.rs `rechecked`) the sum adds wrapping, the absolute sums add, and a state whose
//! absolute sum reaches its component's maximum raises: below it no input order overflows (abi `IntervalSum::may_overflow`),
//! at it the plan-order re-run answers. Without the licence the add is PG's `finite_interval_pl` exactly: a component past
//! its range, or a sum on an infinity image, raises PG's "interval out of range" at its row. A merge adds as the step does
//! (abi `IntervalSum::compose` for records) and skips an empty side. The final is the row executor's own
//! (`interval_sum_final` / `interval_avg_final`) by one host call (raw.rs `interval_sum_final`).
use gravity_abi::breaker::{IVSUM_LICENSED, IVSUM_OVERFLOWED};
use gravity_decompose::ops::shared::agg::{AggDecl, Register as R};
use gravity_ir::builder::FunctionBuilder;
use gravity_expr::emit::CellValid;
use gravity_ir::{Block, ConvOp, HostFlags, IntBinOp, IntCond, Type, Value};
use mcx::{CollectIn, GrowIn, PgVec};

use crate::admit::refusal::Reason;
use crate::face::fold::{Acc, AccShape, FoldClass};
use crate::frame::env::Env;
use crate::ops::agg::trans::float_pg::not;
use crate::rep::CellRef;

pub(crate) struct IntervalSum;
pub(crate) static INTERVAL_SUM: IntervalSum = IntervalSum;

/// The state's words in slot order, then the valid word (registers only).
const T: usize = 0;
const DM: usize = 1;
const AT: usize = 2;
const AD: usize = 3;
const AM: usize = 4;
const N: usize = 5;
const INF: usize = 6;
const FL: usize = 7;
const VALID: usize = 8;
const WORDS: usize = 9;

/// The `day | month << 32` words of `+infinity` and `-infinity` (their `time` is i64::MAX / i64::MIN).
const PINF_DM: i64 = 0x7FFF_FFFF_7FFF_FFFF;
const NINF_DM: i64 = 0x8000_0000_8000_0000u64 as i64;

fn is_avg(env: &Env, a: &AggDecl<'_>) -> bool {
    match a.reg {
        R::SumInterval => false,
        R::AvgInterval => true,
        _ => crate::refuse!(env.node, Reason::Bug { payload: "class.other's interval sum over another register" }),
    }
}

/// The licence: the statement re-runs in plan order on a raise, so a state that may overflow raises (D2597's form).
fn licensed() -> bool {
    gravity_decompose::facts::order::rechecked()
}

fn load(b: &mut FunctionBuilder<'_>, env: &Env, acc: &Acc<'_>) -> [Value; WORDS] {
    let mut w = [env.zero; WORDS];
    match acc {
        Acc::Regs(r) => w.copy_from_slice(&r[..WORDS]),
        Acc::Slot { base, off, valid } => {
            let ok = crate::ops::shared::slots::slot_ok(b, env, *base, *valid);
            for (k, v) in w.iter_mut().take(VALID).enumerate() {
                let x = b.load(Type::I64, *base, off + 8 * k as i32);
                *v = b.select(ok, x, env.zero);
            }
            w[VALID] = b.conv(ConvOp::Zext, Type::I64, ok);
        }
    }
    w
}

fn store(b: &mut FunctionBuilder<'_>, env: &Env, acc: Acc<'_>, w: [Value; WORDS]) {
    match acc {
        Acc::Regs(r) => r[..WORDS].copy_from_slice(&w),
        Acc::Slot { base, off, valid } => {
            for (k, &v) in w.iter().take(VALID).enumerate() {
                b.store(base, off + 8 * k as i32, v);
            }
            let any = b.icmp(IntCond::Ne, w[VALID], env.zero);
            crate::ops::shared::slots::mark(b, env, base, valid, any);
        }
    }
}

/// The words `ks` of a record's slot (zero when the slot holds no state yet), or of the registers `s`.
fn read<'e>(b: &mut FunctionBuilder<'_>, env: &Env<'e, '_>, slot: Option<(Value, i32, CellValid)>, s: &[Value; WORDS], ks: &[usize]) -> PgVec<'e, Value> {
    let Some((base, off, valid)) = slot else { return ks.iter().map(|&k| s[k]).collect_in(env.mcx) };
    let ok = crate::ops::shared::slots::slot_ok(b, env, base, valid);
    let mut out = mcx::reserved_in(ks.len(), env.mcx);
    for &k in ks {
        let x = b.load(Type::I64, base, off + 8 * k as i32);
        out.push_in(b.select(ok, x, env.zero));
    }
    out
}

/// The words `ks` of `w` stored to a record's slot (marked valid), then to `l_j`; or all of `w` to `l_j` (registers).
fn write(b: &mut FunctionBuilder<'_>, env: &Env, slot: Option<(Value, i32, CellValid)>, w: &[Value; WORDS], ks: &[usize], l_j: Block) {
    let Some((base, off, valid)) = slot else { return b.br(l_j, w) };
    for &k in ks {
        b.store(base, off + 8 * k as i32, w[k]);
    }
    let t = b.bconst(true);
    crate::ops::shared::slots::mark(b, env, base, valid, t);
    b.br(l_j, &[]);
}

/// Where `bad`, PG's "interval out of range" (raw.rs `interval_range` stashes it), then `HOST`.
fn raise_range(b: &mut FunctionBuilder<'_>, env: &Env, bad: Value) {
    let (l_bad, l_ok) = (b.create_block(&[]), b.create_block(&[]));
    b.cond_br(bad, (l_bad, &[]), (l_ok, &[]));
    b.switch_to(l_bad);
    let host = crate::ops::agg::raw::interval_range as *const () as usize;
    let h = b.declare_host_once("class_interval_sum::interval_range", &[], Some(Type::I64), host, HostFlags { may_error: true, ..HostFlags::default() });
    b.call_host(h, &[], Some(Type::I64));
    let e = b.iconst(Type::I64, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST) as i64);
    b.br(env.l_raise, &[e]);
    b.switch_to(l_ok);
}

/// `x`'s low 32 bits sign-extended (the day of a `day | month << 32` word).
fn day_of(b: &mut FunctionBuilder<'_>, x: Value) -> Value {
    let d = b.conv(ConvOp::Trunc, Type::I32, x);
    b.conv(ConvOp::Sext, Type::I64, d)
}

/// `x`'s high 32 bits sign-extended (the month).
fn month_of(b: &mut FunctionBuilder<'_>, x: Value) -> Value {
    let s32 = b.iconst(Type::I64, 32);
    b.int_bin(IntBinOp::Sshr, x, s32)
}

/// `|x|` as an unsigned word (i64::MIN's is 2^63).
fn abs(b: &mut FunctionBuilder<'_>, x: Value) -> Value {
    let s63 = b.iconst(Type::I64, 63);
    let m = b.int_bin(IntBinOp::Sshr, x, s63);
    let f = b.int_bin(IntBinOp::Xor, x, m);
    b.int_bin(IntBinOp::Sub, f, m)
}

/// Whether `x` (an i64 holding an i32 sum) is past the i32 range.
fn past_i32(b: &mut FunctionBuilder<'_>, x: Value) -> Value {
    let y = day_of(b, x);
    b.icmp(IntCond::Ne, x, y)
}

/// The image `(t, dm)` is `+infinity` or `-infinity`.
fn infinite(b: &mut FunctionBuilder<'_>, t: Value, dm: Value) -> (Value, Value) {
    let (tmax, tmin) = (b.iconst(Type::I64, i64::MAX), b.iconst(Type::I64, i64::MIN));
    let (dmax, dmin) = (b.iconst(Type::I64, PINF_DM), b.iconst(Type::I64, NINF_DM));
    let (a, c) = (b.icmp(IntCond::Eq, t, tmax), b.icmp(IntCond::Eq, dm, dmax));
    let p = b.int_bin(IntBinOp::And, a, c);
    let (a, c) = (b.icmp(IntCond::Eq, t, tmin), b.icmp(IntCond::Eq, dm, dmin));
    let n = b.int_bin(IntBinOp::And, a, c);
    (p, n)
}

/// `(t, dm) + (xt, xdm)` wrapping: the sum's `time` and `day | month << 32`, and the day and month sums in i64.
fn add_sum(b: &mut FunctionBuilder<'_>, t: Value, dm: Value, xt: Value, xdm: Value) -> [Value; 4] {
    let t2 = b.int_bin(IntBinOp::Add, t, xt);
    let (d0, d1) = (day_of(b, dm), day_of(b, xdm));
    let day = b.int_bin(IntBinOp::Add, d0, d1);
    let (m0, m1) = (month_of(b, dm), month_of(b, xdm));
    let month = b.int_bin(IntBinOp::Add, m0, m1);
    let lo = b.iconst(Type::I64, 0xFFFF_FFFF);
    let (s32, dl) = (b.iconst(Type::I64, 32), b.int_bin(IntBinOp::And, day, lo));
    let mh = b.int_bin(IntBinOp::Shl, month, s32);
    let dm2 = b.int_bin(IntBinOp::Or, dl, mh);
    [t2, dm2, day, month]
}

/// Under the licence: `(t, dm) + (xt, xdm)` wrapping, the halves of `dm` added apart (each sum's low 32 bits; the high
/// word's carry from the day half is masked off), exact wherever the absolute sums leave every component in range.
fn add_fast(b: &mut FunctionBuilder<'_>, t: Value, dm: Value, xt: Value, xdm: Value) -> (Value, Value) {
    let t2 = b.int_bin(IntBinOp::Add, t, xt);
    let (hm, lm) = (b.iconst(Type::I64, 0xFFFF_FFFF_0000_0000u64 as i64), b.iconst(Type::I64, 0xFFFF_FFFF));
    let (h0, h1) = (b.int_bin(IntBinOp::And, dm, hm), b.int_bin(IntBinOp::And, xdm, hm));
    let h = b.int_bin(IntBinOp::Add, h0, h1);
    let l = b.int_bin(IntBinOp::Add, dm, xdm);
    let l = b.int_bin(IntBinOp::And, l, lm);
    (t2, b.int_bin(IntBinOp::Or, h, l))
}

/// Whether `finite_interval_pl((t, _), (xt, _))` raises for the sum `add_sum` made: a component past its range, or the sum an
/// infinity image.
fn pl_raises(b: &mut FunctionBuilder<'_>, env: &Env, t: Value, xt: Value, sum: [Value; 4]) -> Value {
    let [t2, dm2, day, month] = sum;
    let (a, c) = (b.int_bin(IntBinOp::Xor, t, t2), b.int_bin(IntBinOp::Xor, xt, t2));
    let both = b.int_bin(IntBinOp::And, a, c);
    let ovf_t = b.icmp(IntCond::Slt, both, env.zero);
    let (ovf_d, ovf_m) = (past_i32(b, day), past_i32(b, month));
    let (p, n) = infinite(b, t2, dm2);
    [ovf_d, ovf_m, p, n].into_iter().fold(ovf_t, |acc, v| b.int_bin(IntBinOp::Or, acc, v))
}

/// Whether some absolute sum reached its component's maximum (abi `IntervalSum::may_overflow`).
fn may_overflow(b: &mut FunctionBuilder<'_>, at: Value, ad: Value, am: Value) -> Value {
    let (tm, im) = (b.iconst(Type::I64, i64::MAX), b.iconst(Type::I64, i32::MAX as i64));
    let (x, y, z) = (b.icmp(IntCond::Uge, at, tm), b.icmp(IntCond::Uge, ad, im), b.icmp(IntCond::Uge, am, im));
    let xy = b.int_bin(IntBinOp::Or, x, y);
    b.int_bin(IntBinOp::Or, xy, z)
}

impl FoldClass for IntervalSum {
    fn shape(&self, _a: &AggDecl<'_>) -> AccShape {
        AccShape { words: WORDS as u8, bytes: 64, align: 8 }
    }

    /// A hashed end steps each row into the record (the straight road): a run carry would merge eight words at every key
    /// change, which costs more than the step (the FILTER cell of the first quiet pass: 1.43x of the hosted fold).
    fn record_only(&self, _a: &AggDecl<'_>) -> bool {
        true
    }

    fn init(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>) {
        is_avg(env, a);
        let mut w = [env.zero; WORDS];
        w[FL] = b.iconst(Type::I64, if licensed() { IVSUM_LICENSED as i64 } else { 0 });
        store(b, env, acc, w);
    }

    fn step(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>, args: &[CellRef<'_>]) {
        is_avg(env, a);
        if args.len() != 1 {
            crate::refuse!(env.node, Reason::Bug { payload: "class.other's interval sum over other than one argument" });
        }
        let (d, ok) = crate::ops::agg::trans::plain::args_ok(b, env, a, args);
        // a record's slot is read and written only in the words a row changes (a NULL row touches none); registers join
        let slot = match acc {
            Acc::Slot { base, off, valid } => Some((base, off, valid)),
            Acc::Regs(_) => None,
        };
        let s = if slot.is_some() { [env.zero; WORDS] } else { load(b, env, &acc) };
        let (l_do, l_j) = (b.create_block(&[]), b.create_block(if slot.is_some() { &[] } else { &[Type::I64; WORDS] }));
        b.cond_br(ok, (l_do, &[]), (l_j, if slot.is_some() { &[] } else { &s }));
        b.switch_to(l_do);
        let p = b.conv(ConvOp::IntToPtr, Type::Ptr, d);
        let (xt, xdm) = (b.load_unaligned(Type::I64, p, 0), b.load_unaligned(Type::I64, p, 8));
        let one = b.iconst(Type::I64, 1);
        // an infinity's time is i64::MAX / MIN: those rows take a cold block, which counts the image and sends any other
        // value of that time back to the finite add
        let (tmax, tmin) = (b.iconst(Type::I64, i64::MAX), b.iconst(Type::I64, i64::MIN));
        let (e0, e1) = (b.icmp(IntCond::Eq, xt, tmax), b.icmp(IntCond::Eq, xt, tmin));
        let edge = b.int_bin(IntBinOp::Or, e0, e1);
        let (l_edge, l_fin, l_inf) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        b.mark_cold(l_edge);
        b.mark_cold(l_inf);
        b.cond_br(edge, (l_edge, &[]), (l_fin, &[]));
        b.switch_to(l_edge);
        let (pinf, ninf) = infinite(b, xt, xdm);
        let inf = b.int_bin(IntBinOp::Or, pinf, ninf);
        b.cond_br(inf, (l_inf, &[]), (l_fin, &[]));
        b.switch_to(l_inf);
        let hi = b.iconst(Type::I64, 1 << 32);
        let inc = b.select(ninf, hi, env.zero);
        let inc = b.select(pinf, one, inc);
        let mut w = s;
        let r = read(b, env, slot, &s, &[INF]);
        (w[INF], w[VALID]) = (b.int_bin(IntBinOp::Add, r[0], inc), one);
        write(b, env, slot, &w, &[INF], l_j);
        b.switch_to(l_fin);
        let r = read(b, env, slot, &s, &[T, DM, AT, AD, AM, N]);
        let mut w = s;
        (w[T], w[DM], w[AT], w[AD], w[AM]) = (r[0], r[1], r[2], r[3], r[4]);
        (w[N], w[VALID]) = (b.int_bin(IntBinOp::Add, r[5], one), one);
        if licensed() {
            (w[T], w[DM]) = add_fast(b, r[0], r[1], xt, xdm);
            let (xd, xm) = (day_of(b, xdm), month_of(b, xdm));
            let (at, ad, am) = (abs(b, xt), abs(b, xd), abs(b, xm));
            w[AT] = b.int_bin(IntBinOp::Add, r[2], at);
            w[AD] = b.int_bin(IntBinOp::Add, r[3], ad);
            w[AM] = b.int_bin(IntBinOp::Add, r[4], am);
            let bad = may_overflow(b, w[AT], w[AD], w[AM]);
            raise_range(b, env, bad);
        } else {
            let sum = add_sum(b, r[0], r[1], xt, xdm);
            (w[T], w[DM]) = (sum[0], sum[1]);
            let bad = pl_raises(b, env, r[0], xt, sum);
            raise_range(b, env, bad);
        }
        write(b, env, slot, &w, &[T, DM, AT, AD, AM, N], l_j);
        b.switch_to(l_j);
        if slot.is_none() {
            let p = b.block_params(l_j);
            let w: [Value; WORDS] = core::array::from_fn(|k| p[k]);
            store(b, env, acc, w);
        }
    }

    fn merge(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, into: Acc<'_>, from: Acc<'_>) {
        is_avg(env, a);
        // an empty `from` (a run whose rows FILTER skipped, or registers still at init) leaves `into` as it is
        let y = load(b, env, &from);
        let known = b.const_of(y[VALID]).map(|(_, c)| c != 0);
        if known == Some(false) {
            return;
        }
        // registers join at `l_out` (the skip passes them on as they are); a record's slot is stored on the merging path
        let regs = if let Acc::Regs(_) = into { Some(load(b, env, &into)) } else { None };
        let l_out = b.create_block(if regs.is_some() { &[Type::I64; WORDS] } else { &[] });
        if known.is_none() {
            let (l_on, some) = (b.create_block(&[]), b.icmp(IntCond::Ne, y[VALID], env.zero));
            b.cond_br(some, (l_on, &[]), (l_out, regs.as_ref().map_or(&[][..], |r| &r[..])));
            b.switch_to(l_on);
        }
        let x = match regs {
            Some(r) => r,
            None => load(b, env, &into),
        };
        let mut w = x;
        for k in [AT, AD, AM, N, INF] {
            w[k] = b.int_bin(IntBinOp::Add, x[k], y[k]);
        }
        w[FL] = b.int_bin(IntBinOp::Or, x[FL], y[FL]);
        w[VALID] = b.iconst(Type::I64, 1);
        if licensed() {
            // the absolute sums bound every order's running sums: below the maximum the wrapping add is exact
            (w[T], w[DM]) = add_fast(b, x[T], x[DM], y[T], y[DM]);
            let bad = may_overflow(b, w[AT], w[AD], w[AM]);
            raise_range(b, env, bad);
        } else {
            let sum = add_sum(b, x[T], x[DM], y[T], y[DM]);
            (w[T], w[DM]) = (sum[0], sum[1]);
            let bad = pl_raises(b, env, x[T], y[T], sum);
            raise_range(b, env, bad);
        }
        if regs.is_some() {
            b.br(l_out, &w);
            b.switch_to(l_out);
            let p = b.block_params(l_out);
            let w: [Value; WORDS] = core::array::from_fn(|k| p[k]);
            store(b, env, into, w);
        } else {
            store(b, env, into, w);
            b.br(l_out, &[]);
            b.switch_to(l_out);
        }
    }

    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>) -> (Value, Value) {
        let avg = is_avg(env, a);
        let w = load(b, env, &acc);
        let none = b.icmp(IntCond::Eq, w[VALID], env.zero);
        let some = not(b, none);
        // a record merge's deferred overflow (abi `Rec::fold_interval_sum` without the licence), or a licensed state that
        // may overflow, raises: never for an empty state
        let ov = b.iconst(Type::I64, IVSUM_OVERFLOWED as i64);
        let f = b.int_bin(IntBinOp::And, w[FL], ov);
        let mut bad = b.icmp(IntCond::Ne, f, env.zero);
        if licensed() {
            let m = may_overflow(b, w[AT], w[AD], w[AM]);
            bad = b.int_bin(IntBinOp::Or, bad, m);
        }
        let bad = b.int_bin(IntBinOp::And, bad, some);
        raise_range(b, env, bad);
        let host = crate::ops::agg::raw::interval_sum_final as *const () as usize;
        let k = b.iconst(Type::I64, avg as i64);
        let v = crate::ops::agg::trans::slots::host_final(b, env, "class_interval_sum::interval_sum_final", host, &[w[T], w[DM], w[N], w[INF], k], none);
        (v, none)
    }
}
