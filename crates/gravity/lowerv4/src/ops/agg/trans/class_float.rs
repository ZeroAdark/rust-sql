//! Float folds with PG's NaN / Inf rules. Hooks / levers: -. Budget 220 lines (files.tsv, +25 % gate). Owner M1-FOLD.
//!
//! Registers: kind `class.float` (`KindId::ClassFloat`; registered). Words are f64
//! bits, `n` last:
//! - `sum(float8)` / `sum(float4)`: `float8pl` / `float4pl` per row (the float4 sum rounds to f32 at every step, kept
//!   widened in its `SumF32` slot); the first row's value is the state (PG's NULL initcond: `sum` over `-0` is `-0`); an
//!   infinite sum of finite operands raises 22003 at the row.
//! - `avg(float4 / float8)`: `float8_accum`'s Youngs–Cramer step into {N, Sx, Sxx} (D244's `F64Accum`), an Inf / NaN
//!   input making Sxx NaN; windows and records merge by `float8_combine`. Its overflow (Sx or Sxx infinite from finite
//!   operands) is deferred to the final as Sxx = +Inf (D432: rows fold in another order than PG's scan, and PG's overflow
//!   depends on it), where it raises unless Sx is NaN; in PG's order (`facts::order::plan_order`) it raises at the row, as
//!   float8_accum does (D2597). The final is Sx / N, NULL at N = 0.
//! - D2597: where a raise re-runs the statement in plan order (`facts::order::rechecked`), a sum's or average's row whose
//!   finite input could overflow in some order (`risk`) raises, a NaN or Inf met first or not, so the plan-order run gives
//!   PG's answer (NaN where PG meets the NaN first, the overflow otherwise; the order / multiset ruling: never overflow vs
//!   NaN, never rows where PG raises).
//! - `min / max(float4 / float8)`: the value's order-preserving i64 image in a `MinI64` / `MaxI64` slot (decompose
//!   `F64Ordered`), every NaN mapped to `i64::MAX` (PG's NaN above all); `-0` orders below `+0` (PG ties them).
//! No join.mult form (a float sum's product is not `w` rounded adds: plain.rs `weighted`); no block or answer form.
use gravity_decompose::ops::shared::agg::{AggDecl, Register as R};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, FloatBinOp as Fo, IntBinOp, IntCond, Type, Value};
use mcx::{CollectIn, GrowIn, Mcx, PgVec};

use crate::face::fold::{Acc, AccShape, ClassImpl, FinalOrder, FoldClass};
use crate::face::kinds::KindId;
use crate::frame::env::Env;
use crate::admit::refusal::Reason;
use crate::rep::CellRef;

use crate::ops::agg::trans::float_pg::{MAG, INF, accum, and3, arg64, arg_type, big, bits, final_ovf, fl, image, inf, not, pl, raise_ovf, tainted};

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { kinds: &[(KindId::ClassFloat, &ClassImpl(&FLOAT))], ..crate::ops::NONE };

struct Float;
static FLOAT: Float = Float;

#[derive(Clone, Copy, PartialEq, Eq)]
enum F {
    Sum { f32: bool },
    Avg,
    Min,
    Max,
}

fn form(env: &Env, a: &AggDecl<'_>) -> F {
    match a.reg {
        R::SumF64 => F::Sum { f32: false },
        R::SumF32 => F::Sum { f32: true },
        R::AvgF64 => F::Avg,
        R::MinF64 => F::Min,
        R::MaxF64 => F::Max,
        _ => crate::refuse!(env.node, Reason::Bug { payload: "class.float over another register" }),
    }
}

/// D2597: the least input magnitude `2^e` at which fold `f`'s rows could overflow in some order, for fewer than 2^48 rows
/// (a rounded add at most doubles its addend, so a partial sum stays under 2·n·max|x|): float8pl's 2^1024 at 2^974,
/// float4pl's 2^128 at 2^78; float8_accum's `x·N - Sx` stays under 3·n·max|x|, its square under 2^1022, at 2^460
/// (class.stats' float variances too: `STAT_RISK`). An extremum never overflows.
pub(crate) const STAT_RISK: i64 = 460;

fn risk(f: F) -> Option<i64> {
    match f {
        F::Sum { f32: false } => Some(974),
        F::Sum { f32: true } => Some(78),
        F::Avg => Some(STAT_RISK),
        F::Min | F::Max => None,
    }
}

fn ident<'e>(b: &mut FunctionBuilder<'_>, mcx: Mcx<'e>, f: F) -> PgVec<'e, Value> {
    let z = b.iconst(Type::I64, 0);
    match f {
        F::Sum { .. } => mcx::pg_vec![in mcx; z, z],
        F::Avg => mcx::pg_vec![in mcx; z, z, z, z],
        F::Min => mcx::pg_vec![in mcx; b.iconst(Type::I64, i64::MAX), z],
        F::Max => mcx::pg_vec![in mcx; b.iconst(Type::I64, i64::MIN), z],
    }
}

/// The state as words `[…, n]`: registers as they are; a slot's words (an invalid slot the identity, `n` its validity).
fn load<'e>(b: &mut FunctionBuilder<'_>, env: &Env<'e, '_>, f: F, acc: &Acc<'_>) -> PgVec<'e, Value> {
    match acc {
        Acc::Regs(w) => mcx::to_vec_in(&w[..], env.mcx),
        Acc::Slot { base, off, valid } => {
            let ok = crate::ops::shared::slots::slot_ok(b, env, *base, *valid);
            let id = ident(b, env.mcx, f);
            let mut w: PgVec<'_, Value> = (0..id.len() - 1).map(|i| b.load(Type::I64, *base, off + 8 * i as i32)).collect_in(env.mcx);
            for (v, &i) in w.iter_mut().zip(id.iter()) {
                *v = b.select(ok, *v, i);
            }
            w.push_in(b.conv(ConvOp::Zext, Type::I64, ok));
            w
        }
    }
}

fn store(b: &mut FunctionBuilder<'_>, env: &Env, acc: Acc<'_>, w: &[Value]) {
    match acc {
        Acc::Regs(r) => r.copy_from_slice(w),
        Acc::Slot { base, off, valid } => {
            for (i, &v) in w[..w.len() - 1].iter().enumerate() {
                b.store(base, off + 8 * i as i32, v);
            }
            let any = b.icmp(IntCond::Ne, w[w.len() - 1], env.zero);
            crate::ops::shared::slots::mark(b, env, base, valid, any);
        }
    }
}

/// Two states combined (`float8pl`, `float8_combine`, the extreme image): the words and the overflow.
fn combine<'m>(b: &mut FunctionBuilder<'_>, mcx: Mcx<'m>, zero: Value, f: F, a: &[Value], c: &[Value]) -> (PgVec<'m, Value>, Value) {
    let k = a.len() - 1;
    let n = b.int_bin(IntBinOp::Add, a[k], c[k]);
    let (a0, c0) = (b.icmp(IntCond::Eq, a[k], zero), b.icmp(IntCond::Eq, c[k], zero));
    let pick = |b: &mut FunctionBuilder<'_>, v: Value, i: usize| {
        let v = b.select(a0, c[i], v);
        b.select(c0, a[i], v)
    };
    let (mut w, ovf) = match f {
        F::Sum { f32 } => {
            let (s, o) = pl(b, f32, a[0], c[0]);
            (mcx::pg_vec![in mcx; pick(b, s, 0)], o)
        }
        F::Avg => {
            let [n1, sx1, sxx1, n2, sx2, sxx2] = [a[0], a[1], a[2], c[0], c[1], c[2]].map(|v| fl(b, v));
            let (sx, o1) = pl(b, false, a[1], c[1]);
            let nn = b.float_bin(Fo::Add, n1, n2);
            let (q1, q2) = (b.float_bin(Fo::Div, sx1, n1), b.float_bin(Fo::Div, sx2, n2));
            let tmp = b.float_bin(Fo::Sub, q1, q2);
            let p = b.float_bin(Fo::Mul, n1, n2);
            let p = b.float_bin(Fo::Mul, p, tmp);
            let p = b.float_bin(Fo::Mul, p, tmp);
            let p = b.float_bin(Fo::Div, p, nn);
            let s12 = b.float_bin(Fo::Add, sxx1, sxx2);
            let sxx = b.float_bin(Fo::Add, s12, p);
            let (nn, sxx) = (bits(b, nn), bits(b, sxx));
            let (xi, i1, i2) = (inf(b, sxx, false), inf(b, a[2], false), inf(b, c[2], false));
            let (n1i, n2i) = (not(b, i1), not(b, i2));
            let o2 = and3(b, xi, n1i, n2i);
            // D432: an overflow here, or a tainted side, taints the merge (deferred to the final)
            let (t1, t2) = (tainted(b, a[2]), tainted(b, c[2]));
            let t = [o1, o2, t1].into_iter().fold(t2, |x, y| b.int_bin(IntBinOp::Or, x, y));
            let ti = b.iconst(Type::I64, INF);
            let sxx = b.select(t, ti, sxx);
            (mcx::pg_vec![in mcx; pick(b, nn, 0), pick(b, sx, 1), pick(b, sxx, 2)], b.bconst(false))
        }
        F::Min | F::Max => (mcx::pg_vec![in mcx; b.int_bin(if f == F::Min { IntBinOp::Smin } else { IntBinOp::Smax }, a[0], c[0])], b.bconst(false)),
    };
    let either = b.int_bin(IntBinOp::Or, a0, c0);
    let no = b.bconst(false);
    let ovf = b.select(either, no, ovf);
    w.push_in(n);
    (w, ovf)
}

fn words(f: F) -> u8 {
    if f == F::Avg { 4 } else { 2 }
}

impl FoldClass for Float {
    fn shape(&self, a: &AggDecl<'_>) -> AccShape {
        let f = match a.reg {
            R::AvgF64 => F::Avg,
            _ => F::Min,
        };
        AccShape { words: words(f), bytes: if f == F::Avg { 24 } else { 8 }, align: 8 }
    }

    fn init(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>) {
        let id = ident(b, env.mcx, form(env, a));
        store(b, env, acc, &id[..]);
    }

    fn step(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>, args: &[CellRef<'_>]) {
        let f = form(env, a);
        let [c] = args else { crate::refuse!(env.node, Reason::Bug { payload: "class.float over other than one argument" }) };
        let (d, ok) = crate::ops::agg::trans::plain::args_ok(b, env, a, args);
        let x = arg64(b, arg_type(a, 0, c.cell.typid), d);
        if let Some(e) = risk(f).filter(|_| gravity_decompose::facts::order::rechecked()) {
            let at = big(b, x, e); // D2597: some order could overflow here, a NaN met first or not: re-run in plan order
            let at = b.int_bin(IntBinOp::And, at, ok);
            raise_ovf(b, env, at);
        }
        let s = load(b, env, f, &acc);
        let (w, ovf) = match f {
            F::Avg => {
                let (w, row) = accum(b, &s[..], x, env.mcx); // D2597: in PG's order the row's overflow is PG's; else the final's (D432)
                (w, if gravity_decompose::facts::order::plan_order() { row } else { b.bconst(false) })
            }
            F::Min | F::Max => {
                let (img, one) = (image(b, x), b.iconst(Type::I64, 1));
                combine(b, env.mcx, env.zero, f, &s[..], &[img, one])
            }
            F::Sum { .. } => {
                let one = b.iconst(Type::I64, 1);
                combine(b, env.mcx, env.zero, f, &s[..], &[x, one])
            }
        };
        let bad = b.int_bin(IntBinOp::And, ovf, ok);
        raise_ovf(b, env, bad);
        let w: PgVec<'_, Value> = w.iter().zip(s.iter()).map(|(&n, &o)| b.select(ok, n, o)).collect_in(env.mcx);
        store(b, env, acc, &w[..]);
    }

    fn merge(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, into: Acc<'_>, from: Acc<'_>) {
        let f = form(env, a);
        let (x, y) = (load(b, env, f, &into), load(b, env, f, &from));
        let (w, ovf) = combine(b, env.mcx, env.zero, f, &x[..], &y[..]);
        raise_ovf(b, env, ovf);
        store(b, env, into, &w);
    }

    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>) -> (Value, Value) {
        let f = form(env, a);
        let w = load(b, env, f, &acc);
        let none = b.icmp(IntCond::Eq, w[w.len() - 1], env.zero);
        let f4 = a.node.as_aggref().is_some_and(|r| r.aggtype == types_core::FLOAT4OID);
        let v = match f {
            F::Avg => {
                let ovf = final_ovf(b, w[1], w[2]);
                let some = not(b, none);
                let bad = b.int_bin(IntBinOp::And, ovf, some);
                raise_ovf(b, env, bad);
                let (n, sx) = (fl(b, w[0]), fl(b, w[1]));
                let q = b.float_bin(Fo::Div, sx, n);
                bits(b, q)
            }
            F::Min | F::Max => {
                let s63 = b.iconst(Type::I64, 63);
                let m = b.iconst(Type::I64, MAG);
                let sign = b.int_bin(IntBinOp::Sshr, w[0], s63);
                let mask = b.int_bin(IntBinOp::And, sign, m);
                b.int_bin(IntBinOp::Xor, w[0], mask)
            }
            F::Sum { .. } => w[0],
        };
        let v = if f4 && f != F::Avg {
            let x = fl(b, v);
            let x4 = b.conv(ConvOp::Fdemote, Type::F32, x);
            let b32 = b.conv(ConvOp::Bitcast, Type::I32, x4);
            b.conv(ConvOp::Zext, Type::I64, b32)
        } else {
            v
        };
        (v, none)
    }

    fn order(&self, a: &AggDecl<'_>) -> Option<FinalOrder> {
        match a.reg {
            R::SumF64 => Some(FinalOrder::F64),
            R::MinF64 | R::MaxF64 if a.node.as_aggref().is_some_and(|r| r.aggtype == types_core::FLOAT8OID) => Some(FinalOrder::Word),
            _ => None,
        }
    }
}

/// 5c-F6: the scaled absolute sum a float sum top's pass 1 folds beside its lead (table/rec.rs `GroupRec::led`), in a
/// `SumF64` slot: the sum of `|x| 2^-128` over the lead's argument and FILTER, by which the two-pass rule bounds how far
/// another fold order may round the lead (topn raw.rs `float_slack`). The scale keeps every partial sum finite (a term
/// under 2^896, 2^64 of them under 2^960), so it raises nothing and merges as a float sum does (float8pl, which cannot
/// overflow it); a NaN or infinite input's term is NaN or infinite, as the lead is then. Never finished.
pub(crate) struct AbsSum;
pub(crate) static ABS_SUM: AbsSum = AbsSum;

impl FoldClass for AbsSum {
    fn shape(&self, _a: &AggDecl<'_>) -> AccShape {
        AccShape { words: 2, bytes: 8, align: 8 }
    }

    fn init(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _a: &AggDecl<'_>, acc: Acc<'_>) {
        let id = ident(b, env.mcx, F::Sum { f32: false });
        store(b, env, acc, &id[..]);
    }

    fn step(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, a: &AggDecl<'_>, acc: Acc<'_>, args: &[CellRef<'_>]) {
        let [c] = args else { crate::refuse!(env.node, Reason::Bug { payload: "an absolute float sum over other than one argument" }) };
        let (d, ok) = crate::ops::agg::trans::plain::args_ok(b, env, a, args);
        let x = arg64(b, arg_type(a, 0, c.cell.typid), d);
        let m = b.iconst(Type::I64, MAG);
        let ax = b.int_bin(IntBinOp::And, x, m);
        let (fx, scale) = (fl(b, ax), b.f64const(2f64.powi(-128)));
        let t = b.float_bin(Fo::Mul, fx, scale);
        let t = bits(b, t);
        let s = load(b, env, F::Sum { f32: false }, &acc);
        let one = b.iconst(Type::I64, 1);
        let (w, _) = combine(b, env.mcx, env.zero, F::Sum { f32: false }, &s[..], &[t, one]);
        let w: PgVec<'_, Value> = w.iter().zip(s.iter()).map(|(&n, &o)| b.select(ok, n, o)).collect_in(env.mcx);
        store(b, env, acc, &w[..]);
    }

    fn merge(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, _a: &AggDecl<'_>, into: Acc<'_>, from: Acc<'_>) {
        let (x, y) = (load(b, env, F::Sum { f32: false }, &into), load(b, env, F::Sum { f32: false }, &from));
        let (w, _) = combine(b, env.mcx, env.zero, F::Sum { f32: false }, &x[..], &y[..]);
        store(b, env, into, &w);
    }

    fn finish(&self, _b: &mut FunctionBuilder<'_>, env: &mut Env, _a: &AggDecl<'_>, _acc: Acc<'_>) -> (Value, Value) {
        crate::refuse!(env.node, Reason::Bug { payload: "an absolute float sum finished (a pass 1 record is read whole, never emitted)" })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `f` over `n` word params, its words returned, in the interpreter.
    fn run(args: &[u64], out: usize, f: impl FnOnce(&mut FunctionBuilder<'_>, &[Value]) -> Vec<Value>) -> Vec<u64> {
        let mut pb = gravity_ir::builder::ProgramBuilder::new();
        let fid = pb.declare_func("t", &vec![Type::I64; args.len()], &vec![Type::I64; out]);
        let mut b = pb.define(fid);
        let p = b.params().to_vec();
        let r = f(&mut b, &p);
        b.ret(&r);
        let prog = pb.finish();
        gravity_interp::Interp::new(&prog).run(fid, args).expect("runs")
    }

    fn same(a: f64, b: f64) -> bool {
        a.to_bits() == b.to_bits() || a.is_nan() && b.is_nan()
    }

    /// PG's float8_accum (utils/adt/float.c), `None` for its float_overflow_error.
    fn pg_accum([n0, sx0, sxx0]: [f64; 3], x: f64) -> Option<[f64; 3]> {
        let (n, sx) = (n0 + 1.0, sx0 + x);
        let mut sxx = sxx0;
        if n0 > 0.0 {
            let tmp = x * n - sx;
            sxx += tmp * tmp / (n * n0);
            if sx.is_infinite() || sxx.is_infinite() {
                if !sx0.is_infinite() && !x.is_infinite() {
                    return None;
                }
                sxx = f64::NAN;
            }
        } else if x.is_nan() || x.is_infinite() {
            sxx = f64::NAN;
        }
        Some([n, sx, sxx])
    }

    /// The final's overflow over state words `[N, Sx, Sxx]` (D432: raised there, not at the row).
    fn final_raises(s: [f64; 3]) -> bool {
        run(&[s[1].to_bits(), s[2].to_bits()], 1, |b, p| {
            let o = final_ovf(b, p[0], p[1]);
            vec![b.conv(ConvOp::Zext, Type::I64, o)]
        })[0] == 1
    }

    /// The accum step matches PG's row by row and its Inf / NaN rules; PG's overflow (from Sx or from Sxx alone) leaves the
    /// state tainted (D432), which the final raises unless a later NaN wins: `{0.5, 1e300, NaN}` is NaN, as PG's
    /// `{NaN, 0.5, 1e300}` (e2e-av2-v2agg-049: the heap walk feeds a plain aggregate's rows last to first).
    #[test]
    fn accum_is_float8_accum() {
        let rows: [&[f64]; 8] = [&[1.5, 2.25, -3.0, 1e10, 0.1], &[1e200, -1e200], &[1e308, 1e308], &[f64::INFINITY, 1.0], &[f64::NAN, 2.0], &[f64::NEG_INFINITY, f64::INFINITY], &[0.5, 1e300, 2.0, f64::NAN, 1.0], &[0.5, 1e300, 2.0, f64::INFINITY]];
        let cx = mcx::MemoryContext::new("d2597");
        for xs in rows {
            let (mut s, mut tainted) = ([0.0f64; 3], false);
            for (i, &x) in xs.iter().enumerate() {
                let w = run(&[s[0].to_bits(), s[1].to_bits(), s[2].to_bits(), i as u64, x.to_bits()], 5, |b, p| {
                    let (w, row) = accum(b, &p[..4], p[4], cx.mcx());
                    let mut w = w.to_vec();
                    w.push(b.conv(ConvOp::Zext, Type::I64, row));
                    w
                });
                let got = [f64::from_bits(w[0]), f64::from_bits(w[1]), f64::from_bits(w[2])];
                if !tainted {
                    assert_eq!(w[4] == 1, pg_accum(s, x).is_none(), "{xs:?} row {i}: the row's overflow is float8_accum's (D2597)");
                }
                let want = match (tainted, pg_accum(s, x)) {
                    (false, Some(want)) => want,
                    _ => {
                        tainted = true;
                        assert_eq!(got[2], f64::INFINITY, "{xs:?} row {i}: the overflow taints Sxx");
                        s = got;
                        continue;
                    }
                };
                assert!(got.iter().zip(want).all(|(&g, w)| same(g, w)), "{xs:?} row {i}: {got:?} vs {want:?}");
                s = got;
            }
            assert_eq!(final_raises(s), tainted && !s[1].is_nan(), "{xs:?}: the final's overflow");
        }
        assert!(!final_raises([3.0, f64::NAN, f64::INFINITY]) && final_raises([2.0, 1e300, f64::INFINITY]) && final_raises([2.0, f64::INFINITY, f64::INFINITY]));
    }

    /// D2597: `risk`'s bound: finite magnitudes from `2^e` up (either sign) raise, smaller ones, Inf and NaN do not; and no
    /// order of fewer than 2^48 rows each under the bound overflows (2^48 · 2 · 2^974 < 2^1024, 3 · 2^48 · 2^460 squared
    /// under 2^1022, 2^48 · 2 · 2^78 < 2^128).
    #[test]
    fn big_marks_the_risk() {
        let at = |x: f64, e: i64| run(&[x.to_bits()], 1, |b, p| {
            let v = big(b, p[0], e);
            vec![b.conv(ConvOp::Zext, Type::I64, v)]
        })[0] == 1;
        for (e, f) in [(974, f64::MAX), (STAT_RISK, f64::MAX), (78, f32::MAX as f64)] {
            let t = 2f64.powi(e as i32);
            assert!(at(t, e) && at(-t, e) && at(f64::MAX, e) && at(-f, e), "2^{e} and up");
            assert!(!at(t * (1.0 - f64::EPSILON), e) && !at(1.0, e) && !at(0.0, e) && !at(f64::INFINITY, e) && !at(f64::NEG_INFINITY, e) && !at(f64::NAN, e), "2^{e}");
        }
        assert!(2f64.powi(48) * 2.0 * 2f64.powi(974) < f64::MAX && (3.0 * 2f64.powi(48) * 2f64.powi(STAT_RISK as i32)).powi(2) < 2f64.powi(1022));
        assert!(2f64.powi(48) * 2.0 * 2f64.powi(78) <= 2f64.powi(127));
    }

    /// Records merge as abi's float8_combine does (an empty side yields the other; an Sx / Sxx overflow or a tainted side
    /// taints, D432).
    #[test]
    fn avg_merge_is_float8_combine() {
        let cx = mcx::MemoryContext::new("d432");
        let st = [[0.0, 0.0, 0.0], [2.0, 3.0, 0.5], [3.0, -1e10, 7.25], [1.0, 1e200, 0.0], [1.0, -1e200, 0.0], [1.0, f64::NAN, f64::NAN], [2.0, 1e300, f64::INFINITY], [1.0, 1.7e308, 0.0]];
        for a in st {
            for c in st {
                let arg = [a[0].to_bits(), a[1].to_bits(), a[2].to_bits(), a[0] as u64, c[0].to_bits(), c[1].to_bits(), c[2].to_bits(), c[0] as u64];
                let w = run(&arg, 4, |b, p| {
                    let z = b.iconst(Type::I64, 0);
                    let (w, o) = combine(b, cx.mcx(), z, F::Avg, &p[..4], &p[4..]);
                    let mut w = w.to_vec();
                    w[3] = b.conv(ConvOp::Zext, Type::I64, o);
                    w
                });
                match gravity_abi::breaker::float8_combine(a, c) {
                    Ok(r) => assert!(w[3] == 0 && (0..3).all(|i| same(f64::from_bits(w[i]), r[i])), "{a:?} + {c:?}: {w:x?} vs {r:?}"),
                    Err(_) => unreachable!("float8_combine defers its overflow (D432)"),
                }
            }
        }
    }

    /// float4pl rounds to f32 at the step and overflows in f32; the first row is the state (a sum over -0 is -0).
    #[test]
    fn sums_are_float4pl_and_float8pl() {
        let cx = mcx::MemoryContext::new("float4pl");
        let sum = |f32: bool, s: f64, n: u64, x: f64| {
            let w = run(&[s.to_bits(), n, x.to_bits(), 1], 3, |b, p| {
                let z = b.iconst(Type::I64, 0);
                let (w, o) = combine(b, cx.mcx(), z, F::Sum { f32 }, &p[..2], &p[2..]);
                let mut w = w.to_vec();
                w.push(b.conv(ConvOp::Zext, Type::I64, o));
                w
            });
            (f64::from_bits(w[0]), w[2] == 1)
        };
        assert_eq!(sum(true, 16777216.0, 1, 1.0), (16777216.0, false));
        assert_eq!(sum(false, 16777216.0, 1, 1.0), (16777217.0, false));
        assert_eq!(sum(true, 3e38f32 as f64, 1, 3e38f32 as f64).1, true);
        assert_eq!(sum(false, 1e308, 1, 1e308).1, true);
        assert_eq!(sum(false, f64::INFINITY, 1, 1.0), (f64::INFINITY, false));
        assert_eq!(sum(false, 0.0, 0, -0.0).0.to_bits(), (-0.0f64).to_bits());
    }

    /// The min / max image orders as PG's float8 compare (NaN, of either sign, above all) and decodes back.
    #[test]
    fn images_order_as_pg() {
        let xs = [f64::NEG_INFINITY, -1.0, -0.0, 0.0, 1e-300, 1.0, f64::INFINITY, f64::NAN];
        let img: Vec<i64> = xs.iter().map(|x| run(&[x.to_bits()], 1, |b, p| vec![image(b, p[0])])[0] as i64).collect();
        assert!(img.windows(2).all(|w| w[0] < w[1]), "{img:x?}");
        assert_eq!(run(&[(-f64::NAN).to_bits()], 1, |b, p| vec![image(b, p[0])])[0] as i64, i64::MAX);
    }
}
