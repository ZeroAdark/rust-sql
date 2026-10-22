//! PostgreSQL's float arithmetic as IR. Hooks / levers: -. Budget 140 lines (files-extra.tsv, +25 % gate). Owner M1-FOLD.
//!
//! Split from class_float.rs by responsibility (D267): float4pl / float8pl with their overflow test (an infinite result of
//! finite operands), float8_accum's Youngs-Cramer step with its overflow and Inf / NaN rules, the argument read as f64
//! bits (float4 promoted), and the order image of an f64 (NaN largest). class_float.rs folds with them; registers nothing.
use gravity_decompose::ops::shared::agg::AggDecl;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, FloatBinOp as Fo, FloatCond, IntBinOp, IntCond, Type, Value};
use mcx::{Mcx, PgVec};

use crate::frame::env::Env;

pub(crate) const MAG: i64 = i64::MAX;
pub(crate) const INF: i64 = 0x7FF0_0000_0000_0000;
pub(crate) const NAN: i64 = 0x7FF8_0000_0000_0000;

pub(crate) fn fl(b: &mut FunctionBuilder<'_>, bits: Value) -> Value {
    b.conv(ConvOp::Bitcast, Type::F64, bits)
}

pub(crate) fn bits(b: &mut FunctionBuilder<'_>, x: Value) -> Value {
    b.conv(ConvOp::Bitcast, Type::I64, x)
}

/// `bits` is ±Inf (`nan`: or NaN).
pub(crate) fn inf(b: &mut FunctionBuilder<'_>, x: Value, nan: bool) -> Value {
    let (m, i) = (b.iconst(Type::I64, MAG), b.iconst(Type::I64, INF));
    let a = b.int_bin(IntBinOp::And, x, m);
    b.icmp(if nan { IntCond::Uge } else { IntCond::Eq }, a, i)
}

pub(crate) fn not(b: &mut FunctionBuilder<'_>, v: Value) -> Value {
    let t = b.bconst(true);
    b.int_bin(IntBinOp::Xor, v, t)
}

pub(crate) fn and3(b: &mut FunctionBuilder<'_>, x: Value, y: Value, z: Value) -> Value {
    let xy = b.int_bin(IntBinOp::And, x, y);
    b.int_bin(IntBinOp::And, xy, z)
}

/// Raises 22003 "value out of range: overflow" where `bad` holds.
pub(crate) fn raise_ovf(b: &mut FunctionBuilder<'_>, env: &Env, bad: Value) {
    let e = b.iconst(Type::I64, gravity_abi::status::raise::bit(gravity_abi::status::raise::FLOAT_OVF) as i64);
    let l_ok = b.create_block(&[]);
    b.cond_br(bad, (env.l_raise, &[e]), (l_ok, &[]));
    b.switch_to(l_ok);
}

/// `float8pl` / `float4pl` over bits (a float4 sum's operands are exact f32 values widened): the sum and its overflow.
pub(crate) fn pl(b: &mut FunctionBuilder<'_>, f32: bool, s: Value, x: Value) -> (Value, Value) {
    let (fs, fx) = (fl(b, s), fl(b, x));
    let r = if f32 {
        let (s4, x4) = (b.conv(ConvOp::Fdemote, Type::F32, fs), b.conv(ConvOp::Fdemote, Type::F32, fx));
        let r4 = b.float_bin(Fo::Add, s4, x4);
        b.conv(ConvOp::Fpromote, Type::F64, r4)
    } else {
        b.float_bin(Fo::Add, fs, fx)
    };
    let r = bits(b, r);
    let (ri, si, xi) = (inf(b, r, false), inf(b, s, false), inf(b, x, false));
    let (ns, nx) = (not(b, si), not(b, xi));
    (r, and3(b, ri, ns, nx))
}

/// `float8_accum` (utils/adt/float.c) of `x` into `[N, Sx, Sxx, n]`, and PG's overflow at this row. Out of PG's order the
/// overflow is deferred (D432): PG raises it at the row, which makes it depend on the scan order (`{NaN, 0.5, 1e300}` is
/// NaN, `{0.5, 1e300, NaN}` raises), so the state keeps it as Sxx = +Inf (`TAINT`: PG's Sxx is never infinite) until
/// the final, where a NaN Sx wins (`tainted`); in PG's order the caller raises the row's (D2597).
pub(crate) fn accum<'m>(b: &mut FunctionBuilder<'_>, s: &[Value], x: Value, mcx: Mcx<'m>) -> (PgVec<'m, Value>, Value) {
    let [n0, sx0, sxx0, fx] = [s[0], s[1], s[2], x].map(|v| fl(b, v));
    let one = b.f64const(1.0);
    let n1 = b.float_bin(Fo::Add, n0, one);
    let sx1 = b.float_bin(Fo::Add, sx0, fx);
    let t = b.float_bin(Fo::Mul, fx, n1);
    let tmp = b.float_bin(Fo::Sub, t, sx1);
    let q = b.float_bin(Fo::Mul, tmp, tmp);
    let d = b.float_bin(Fo::Mul, n1, n0);
    let q = b.float_bin(Fo::Div, q, d);
    let sxx1 = b.float_bin(Fo::Add, sxx0, q);
    let (n1, sx1, sxx1) = (bits(b, n1), bits(b, sx1), bits(b, sxx1));
    let (si, xi, sxi, xin) = (inf(b, sx1, false), inf(b, sxx1, false), inf(b, s[1], false), inf(b, x, false));
    let bad = b.int_bin(IntBinOp::Or, si, xi);
    let (nsx, nx) = (not(b, sxi), not(b, xin));
    let zero = b.f64const(0.0);
    let later = b.fcmp(FloatCond::Gt, n0, zero);
    let ovf = and3(b, bad, nsx, nx);
    let (taint, t0) = (b.iconst(Type::I64, INF), tainted(b, s[2]));
    let ovf = b.int_bin(IntBinOp::Or, ovf, t0);
    let nan = b.iconst(Type::I64, NAN);
    let sxx_l = b.select(bad, nan, sxx1);
    let sxx_l = b.select(ovf, taint, sxx_l);
    let nonfinite = inf(b, x, true);
    let sxx_f = b.select(nonfinite, nan, s[2]);
    let sxx = b.select(later, sxx_l, sxx_f);
    let one_i = b.iconst(Type::I64, 1);
    let row = and3(b, later, bad, nsx);
    let row = b.int_bin(IntBinOp::And, row, nx);
    (mcx::pg_vec![in mcx; n1, sx1, sxx, b.int_bin(IntBinOp::Add, s[3], one_i)], row)
}

/// D2597: `x` is finite and at least `2^e` in magnitude, the least at which some input order of a fold's rows could
/// overflow (class_float's `risk`): its rows then raise where a raise re-runs the statement in plan order.
pub(crate) fn big(b: &mut FunctionBuilder<'_>, x: Value, e: i64) -> Value {
    let (m, lo) = (b.iconst(Type::I64, MAG), b.iconst(Type::I64, (1023 + e) << 52));
    let a = b.int_bin(IntBinOp::And, x, m);
    let (d, span) = (b.int_bin(IntBinOp::Sub, a, lo), b.iconst(Type::I64, INF - ((1023 + e) << 52)));
    b.icmp(IntCond::Ult, d, span)
}

/// Sxx bits `sxx` hold the deferred overflow (+Inf, D432).
pub(crate) fn tainted(b: &mut FunctionBuilder<'_>, sxx: Value) -> Value {
    let i = b.iconst(Type::I64, INF);
    b.icmp(IntCond::Eq, sxx, i)
}

/// The final's overflow (D432): the state is tainted and its Sx is not NaN (a NaN input, or Inf + -Inf, wins as it would
/// had it come first in PG's scan).
pub(crate) fn final_ovf(b: &mut FunctionBuilder<'_>, sx: Value, sxx: Value) -> Value {
    let (nan_or_inf, is_inf) = (inf(b, sx, true), inf(b, sx, false));
    let nan = b.int_bin(IntBinOp::Xor, nan_or_inf, is_inf);
    let not_nan = not(b, nan);
    let t = tainted(b, sxx);
    b.int_bin(IntBinOp::And, t, not_nan)
}

/// D2685: the type a float aggregate's argument `i` arrives as, for `arg64`: the aggregate's declared input type (the
/// catalog table's row), the cell's own type only when the table has no row. A column typed by a domain over float4 carries
/// the domain's oid, and PG's float4pl / float4_accum read a float4 datum whatever the call passed.
pub(crate) fn arg_type(a: &AggDecl<'_>, i: usize, cell: u32) -> u32 {
    gravity_decompose::ops::agg::trans::table::row(a.fnoid).and_then(|r| r.args.get(i).copied()).unwrap_or(cell)
}

/// A float datum's f64 bits: float4's low 32 bits promoted (exact).
pub(crate) fn arg64(b: &mut FunctionBuilder<'_>, typid: u32, d: Value) -> Value {
    if typid != types_core::FLOAT4OID {
        return d;
    }
    let t = b.conv(ConvOp::Trunc, Type::I32, d);
    let f = b.conv(ConvOp::Bitcast, Type::F32, t);
    let x = b.conv(ConvOp::Fpromote, Type::F64, f);
    bits(b, x)
}

/// The order image of f64 `x`: NaN `i64::MAX`, else the sign-magnitude bits as a signed word (an involution).
pub(crate) fn image(b: &mut FunctionBuilder<'_>, x: Value) -> Value {
    let s63 = b.iconst(Type::I64, 63);
    let m = b.iconst(Type::I64, MAG);
    let sign = b.int_bin(IntBinOp::Sshr, x, s63);
    let mask = b.int_bin(IntBinOp::And, sign, m);
    let img = b.int_bin(IntBinOp::Xor, x, mask);
    let (a, i) = (b.int_bin(IntBinOp::And, x, m), b.iconst(Type::I64, INF));
    let nan = b.icmp(IntCond::Ugt, a, i);
    b.select(nan, m, img)
}
