//! Tier A open-coded (expr-compiler.md §2.2): the integer, float, bool, "char", oid, date and
//! timestamp builtins as IR ops over the `I64` datum word, the error as a raise mask — the `checked::*`
//! rules (a masked divisor so the division never traps, the range of a narrowing).

use gravity_abi::status::raise;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, FloatBinOp, FloatCond, FloatUnOp, IntBinOp, IntCond, IntOvfOp, IntUnOp, Type, Value, VecBinOp};
use types_core::Oid;

use crate::nodes::func::Call;
use crate::nodes::Program;

pub mod datetime;

#[derive(Clone, Copy)]
pub struct Out {
    pub value: Value,
    pub err: Value,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Cmp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Arith {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

#[derive(Clone, Copy)]
enum Form {
    /// Integer arithmetic at the result width (2 / 4 / 8) over sign-extended words.
    IntArith(Arith, u8),
    IntCmp(Cmp),
    IntBtCmp,
    /// `btint2cmp`: PG returns the difference `(int32) a - (int32) b`, not -1 / 0 / 1 (D2742).
    Int2BtCmp,
    /// `btoidcmp`: oid orders unsigned (D2348).
    UBtCmp,
    IntNeg(u8),
    IntAbs(u8),
    Bit(IntBinOp),
    BitNot,
    IntMinMax(bool),
    /// A narrowing to width 2 / 4 with the range flag.
    IntNarrow(u8),
    /// An integer widening from the width: the word read at it, sign-extended (as `DatumGetInt32` / `DatumGetInt16`
    /// read a word whose high bits another convention set, D858's relabelled oid).
    Widen(u8),
    /// Float arithmetic at width 4 / 8 (the operands promoted to the result width).
    FloatArith(Arith, u8, u8, u8),
    FloatCmp(Cmp, u8, u8),
    FloatBtCmp(u8, u8),
    FloatNeg(u8),
    FloatAbs(u8),
    FloatMinMax(bool, u8),
    IntToFloat(u8),
    Promote,
    Demote,
    UCmp(Cmp),
    CharCmp(Cmp),
    BoolCmp(Cmp),
    BoolBtCmp,
    /// `btcharcmp`: the bytes' difference as uint8, `(int32) (uint8) a - (int32) (uint8) b` (D2742).
    CharBtCmp,
    DateMi,
    /// `extract` / `date_part` / `date_trunc` over a timestamp (`datetime.rs`); the site's constant unit decides.
    DateTime,
}

fn cmp_of(s: &str) -> Option<Cmp> {
    Some(match s {
        "eq" => Cmp::Eq,
        "ne" => Cmp::Ne,
        "lt" => Cmp::Lt,
        "le" => Cmp::Le,
        "gt" => Cmp::Gt,
        "ge" => Cmp::Ge,
        _ => return None,
    })
}

fn arith_of(s: &str) -> Option<Arith> {
    Some(match s {
        "pl" => Arith::Add,
        "mi" => Arith::Sub,
        "mul" => Arith::Mul,
        "div" => Arith::Div,
        "mod" => Arith::Mod,
        _ => return None,
    })
}

fn widths(d: &str) -> Option<(u8, u8)> {
    Some(match d {
        "2" => (2, 2),
        "4" => (4, 4),
        "8" => (8, 8),
        "24" => (2, 4),
        "42" => (4, 2),
        "28" => (2, 8),
        "82" => (8, 2),
        "48" => (4, 8),
        "84" => (8, 4),
        _ => return None,
    })
}

fn form(name: &str) -> Option<Form> {
    if let Some(rest) = name.strip_prefix("btint") {
        return rest.strip_suffix("cmp").and_then(widths).map(|w| if w == (2, 2) { Form::Int2BtCmp } else { Form::IntBtCmp });
    }
    if let Some(rest) = name.strip_prefix("btfloat") {
        let (a, b) = rest.strip_suffix("cmp").and_then(widths)?;
        return Some(Form::FloatBtCmp(a, b));
    }
    if let Some(rest) = name.strip_prefix("int") {
        // a cast's name is all digits (int48, int84, int28, int82)
        let n = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        let (d, op) = rest.split_at(n);
        let (a, b) = widths(d)?;
        if op.is_empty() {
            return (a != b).then_some(if b < a { Form::IntNarrow(b) } else { Form::Widen(a) });
        }
        if let Some(c) = cmp_of(op) {
            return Some(Form::IntCmp(c));
        }
        if let Some(ar) = arith_of(op) {
            return Some(Form::IntArith(ar, a.max(b)));
        }
        return Some(match op {
            "um" => Form::IntNeg(a),
            "abs" => Form::IntAbs(a),
            "and" => Form::Bit(IntBinOp::And),
            "or" => Form::Bit(IntBinOp::Or),
            "xor" => Form::Bit(IntBinOp::Xor),
            "not" => Form::BitNot,
            "larger" => Form::IntMinMax(true),
            "smaller" => Form::IntMinMax(false),
            _ => return None,
        });
    }
    if let Some(rest) = name.strip_prefix("float") {
        let n = rest.find(|c: char| !c.is_ascii_digit())?;
        let (d, op) = rest.split_at(n);
        let (a, b) = widths(d)?;
        if let Some(c) = cmp_of(op) {
            return Some(Form::FloatCmp(c, a, b));
        }
        if let Some(ar) = arith_of(op).filter(|a| *a != Arith::Mod) {
            return Some(Form::FloatArith(ar, a, b, a.max(b)));
        }
        return Some(match op {
            "um" => Form::FloatNeg(a),
            "abs" => Form::FloatAbs(a),
            "larger" => Form::FloatMinMax(true, a),
            "smaller" => Form::FloatMinMax(false, a),
            _ => return None,
        });
    }
    if let Some(rest) = name.strip_prefix("oid") {
        return cmp_of(rest).map(Form::UCmp);
    }
    if let Some(rest) = name.strip_prefix("char") {
        return cmp_of(rest).map(Form::CharCmp);
    }
    if let Some(rest) = name.strip_prefix("bool") {
        return cmp_of(rest).map(Form::BoolCmp);
    }
    if let Some(rest) = name.strip_prefix("date_") {
        return match rest {
            "cmp" => Some(Form::IntBtCmp),
            "mi" => Some(Form::DateMi),
            r => cmp_of(r).map(Form::IntCmp),
        };
    }
    if datetime_name(name) {
        return Some(Form::DateTime);
    }
    if let Some(rest) = name.strip_prefix("timestamp_") {
        return match rest {
            "cmp" => Some(Form::IntBtCmp),
            "larger" => Some(Form::IntMinMax(true)),
            "smaller" => Some(Form::IntMinMax(false)),
            r => cmp_of(r).map(Form::IntCmp),
        };
    }
    Some(match name {
        "i2toi4" => Form::Widen(2),
        "i4toi8" => Form::Widen(4),
        "i4toi2" => Form::IntNarrow(2),
        "i2tod" | "i4tod" | "i8tod" => Form::IntToFloat(8),
        "i2tof" | "i4tof" | "i8tof" => Form::IntToFloat(4),
        "ftod" => Form::Promote,
        "dtof" => Form::Demote,
        "btboolcmp" => Form::BoolBtCmp,
        "btcharcmp" => Form::CharBtCmp,
        "btoidcmp" => Form::UBtCmp,
        _ => return None,
    })
}

fn form_of(funcid: Oid) -> Option<Form> {
    let row = gravity_fn::row_of(funcid)?;
    if row.tier != gravity_fn::Tier::A {
        return None;
    }
    form(row.name)
}

fn datetime_name(name: &str) -> bool {
    matches!(name, "extract_timestamp" | "extract_date" | "timestamp_part" | "timestamp_trunc")
}

/// The site is open-coded: a form by name, or a datetime form by its constant unit (`word_root`: the
/// site is the root a caller reads as the integer word, `RowRefs::word_root`).
pub fn is_prim(p: &Program<'_>, call: &Call, word_root: bool) -> bool {
    match form_of(call.funcid) {
        Some(Form::DateTime) => datetime::form_of(p, call, gravity_fn::row_of(call.funcid).map_or("", |r| r.name), word_root).is_some(),
        Some(_) => true,
        None => false,
    }
}

/// `emit` with the datetime forms: the value and, when the form decides it, the validity.
pub fn emit_ext(b: &mut FunctionBuilder<'_>, p: &Program<'_>, call: &Call, a: &[Value], word_root: bool) -> Option<(Out, Option<Value>)> {
    if matches!(form_of(call.funcid), Some(Form::DateTime)) {
        let f = datetime::form_of(p, call, gravity_fn::row_of(call.funcid).map_or("", |r| r.name), word_root)?;
        return Some(datetime::emit(b, f, a[1]));
    }
    emit(b, call.funcid, a).map(|o| (o, None))
}

fn int_ty(w: u8) -> Type {
    match w {
        2 => Type::I16,
        4 => Type::I32,
        _ => Type::I64,
    }
}

fn flt_ty(w: u8) -> Type {
    if w == 4 {
        Type::F32
    } else {
        Type::F64
    }
}

/// Whether the `I64` `v` is representable at width `w` (2 / 4): its narrowing sign-extends back to it.
fn fits(b: &mut FunctionBuilder<'_>, v: Value, w: u8) -> Value {
    let t = b.conv(ConvOp::Trunc, int_ty(w), v);
    let back = b.conv(ConvOp::Sext, Type::I64, t);
    b.icmp(IntCond::Eq, back, v)
}

fn to_float(b: &mut FunctionBuilder<'_>, v: Value, w: u8) -> Value {
    if w == 4 {
        let t = b.conv(ConvOp::Trunc, Type::I32, v);
        b.conv(ConvOp::Bitcast, Type::F32, t)
    } else {
        b.conv(ConvOp::Bitcast, Type::F64, v)
    }
}

fn from_float(b: &mut FunctionBuilder<'_>, v: Value, w: u8) -> Value {
    if w == 4 {
        let t = b.conv(ConvOp::Bitcast, Type::I32, v);
        b.conv(ConvOp::Zext, Type::I64, t)
    } else {
        b.conv(ConvOp::Bitcast, Type::I64, v)
    }
}

fn promote(b: &mut FunctionBuilder<'_>, v: Value, from: u8, to: u8) -> Value {
    let f = to_float(b, v, from);
    if from == 4 && to == 8 {
        b.conv(ConvOp::Fpromote, Type::F64, f)
    } else {
        f
    }
}

fn is_nan(b: &mut FunctionBuilder<'_>, x: Value) -> Value {
    b.fcmp(FloatCond::Unordered, x, x)
}

fn is_inf(b: &mut FunctionBuilder<'_>, x: Value, w: u8) -> Value {
    let a = b.float_un(FloatUnOp::Abs, x);
    let inf = if w == 4 { b.f32const(f32::INFINITY) } else { b.f64const(f64::INFINITY) };
    b.fcmp(FloatCond::Eq, a, inf)
}

fn is_zero(b: &mut FunctionBuilder<'_>, x: Value, w: u8) -> Value {
    let z = if w == 4 { b.f32const(0.0) } else { b.f64const(0.0) };
    b.fcmp(FloatCond::Eq, x, z)
}

fn not(b: &mut FunctionBuilder<'_>, x: Value) -> Value {
    let t = b.bconst(true);
    b.int_bin(IntBinOp::Xor, x, t)
}

fn and(b: &mut FunctionBuilder<'_>, x: Value, y: Value) -> Value {
    b.int_bin(IntBinOp::And, x, y)
}

fn or(b: &mut FunctionBuilder<'_>, x: Value, y: Value) -> Value {
    b.int_bin(IntBinOp::Or, x, y)
}

/// The raise mask of a `Bool` flag (abi `status::raise`): `code`'s bit where the flag is set.
pub(crate) fn raise(b: &mut FunctionBuilder<'_>, flag: Value, code: u8) -> Value {
    let z = b.iconst(Type::I64, 0);
    let c = b.iconst(Type::I64, raise::bit(code) as i64);
    b.select(flag, c, z)
}

pub(crate) fn no_raise(b: &mut FunctionBuilder<'_>) -> Value {
    b.iconst(Type::I64, 0)
}

/// The status word of a raise mask (abi `status::raised`): `-(3 | mask << 8)`, the row's error.
pub fn raise_status(b: &mut FunctionBuilder<'_>, mask: Value) -> Value {
    let three = b.iconst(Type::I64, -gravity_abi::status::ERROR);
    let eight = b.iconst(Type::I64, 8);
    let sh = b.int_bin(IntBinOp::Shl, mask, eight);
    let word = b.int_bin(IntBinOp::Or, three, sh);
    b.int_un(IntUnOp::Neg, word)
}

/// The out-of-range code of an integer width.
pub(crate) fn range_code(w: u8) -> u8 {
    match w {
        2 => raise::INT2_RANGE,
        8 => raise::INT8_RANGE,
        _ => raise::INT4_RANGE,
    }
}

/// PostgreSQL's float order: NaN equals NaN and sorts above everything.
fn float_cmp(b: &mut FunctionBuilder<'_>, c: Cmp, x: Value, y: Value) -> Value {
    crate::stack_check();
    let (nx, ny) = (is_nan(b, x), is_nan(b, y));
    match c {
        Cmp::Eq => {
            let e = b.fcmp(FloatCond::Eq, x, y);
            let both = and(b, nx, ny);
            or(b, e, both)
        }
        Cmp::Ne => {
            let e = float_cmp(b, Cmp::Eq, x, y);
            not(b, e)
        }
        Cmp::Lt => {
            let l = b.fcmp(FloatCond::Lt, x, y);
            let r = or(b, ny, l);
            let nnx = not(b, nx);
            and(b, nnx, r)
        }
        Cmp::Le => {
            let l = b.fcmp(FloatCond::Le, x, y);
            let nnx = not(b, nx);
            let r = and(b, nnx, l);
            or(b, ny, r)
        }
        Cmp::Gt => float_cmp(b, Cmp::Lt, y, x),
        Cmp::Ge => float_cmp(b, Cmp::Le, y, x),
    }
}

fn icond(c: Cmp, signed: bool) -> IntCond {
    match (c, signed) {
        (Cmp::Eq, _) => IntCond::Eq,
        (Cmp::Ne, _) => IntCond::Ne,
        (Cmp::Lt, true) => IntCond::Slt,
        (Cmp::Le, true) => IntCond::Sle,
        (Cmp::Gt, true) => IntCond::Sgt,
        (Cmp::Ge, true) => IntCond::Sge,
        (Cmp::Lt, false) => IntCond::Ult,
        (Cmp::Le, false) => IntCond::Ule,
        (Cmp::Gt, false) => IntCond::Ugt,
        (Cmp::Ge, false) => IntCond::Uge,
    }
}

fn zext(b: &mut FunctionBuilder<'_>, v: Value) -> Value {
    b.conv(ConvOp::Zext, Type::I64, v)
}

/// `(x > y) - (x < y)` as an `I64` from two `Bool`s.
fn three_way(b: &mut FunctionBuilder<'_>, gt: Value, lt: Value) -> Value {
    let g = zext(b, gt);
    let l = zext(b, lt);
    b.int_bin(IntBinOp::Sub, g, l)
}

fn int_arith(b: &mut FunctionBuilder<'_>, op: Arith, w: u8, x: Value, y: Value) -> Out {
    let zero = b.iconst(Type::I64, 0);
    let f = b.bconst(false);
    if w == 8 {
        let mone = b.iconst(Type::I64, -1);
        let one = b.iconst(Type::I64, 1);
        return match op {
            Arith::Add => {
                let o = b.int_ovf(IntOvfOp::SaddOvf, x, y);
                Out { value: b.int_bin(IntBinOp::Add, x, y), err: raise(b, o, raise::INT8_RANGE) }
            }
            Arith::Sub => {
                let o = b.int_ovf(IntOvfOp::SsubOvf, x, y);
                Out { value: b.int_bin(IntBinOp::Sub, x, y), err: raise(b, o, raise::INT8_RANGE) }
            }
            Arith::Mul => {
                let o = b.int_ovf(IntOvfOp::SmulOvf, x, y);
                Out { value: b.int_bin(IntBinOp::Mul, x, y), err: raise(b, o, raise::INT8_RANGE) }
            }
            Arith::Div => {
                let min = b.iconst(Type::I64, i64::MIN);
                let z = b.icmp(IntCond::Eq, y, zero);
                let m1 = b.icmp(IntCond::Eq, y, mone);
                let mn = b.icmp(IntCond::Eq, x, min);
                let ovf = and(b, m1, mn);
                let bad = or(b, z, ovf);
                let d = b.select(bad, one, y);
                let ez = raise(b, z, raise::DIV_ZERO);
                let eo = raise(b, ovf, raise::INT8_RANGE);
                Out { value: b.int_bin(IntBinOp::Sdiv, x, d), err: or(b, ez, eo) }
            }
            Arith::Mod => {
                let z = b.icmp(IntCond::Eq, y, zero);
                let m1 = b.icmp(IntCond::Eq, y, mone);
                let mask = or(b, z, m1);
                let d = b.select(mask, one, y);
                Out { value: b.int_bin(IntBinOp::Srem, x, d), err: raise(b, z, raise::DIV_ZERO) }
            }
        };
    }
    match op {
        Arith::Add | Arith::Sub | Arith::Mul => {
            let iop = match op {
                Arith::Add => IntBinOp::Add,
                Arith::Sub => IntBinOp::Sub,
                _ => IntBinOp::Mul,
            };
            let r = b.int_bin(iop, x, y);
            let ok = fits(b, r, w);
            let nok = not(b, ok);
            Out { value: r, err: raise(b, nok, range_code(w)) }
        }
        Arith::Div => {
            // D857: the dividend word need not be a sign-extended narrow value (a NULL argument's or an erroring
            // product's word flows on), so `sdiv i64::MIN, -1` must never run (x86-64 #DE): divide by 1 there;
            // i64::MIN then fails `fits` and raises the width's range error, which a NULL argument masks.
            let one = b.iconst(Type::I64, 1);
            let mone = b.iconst(Type::I64, -1);
            let min = b.iconst(Type::I64, i64::MIN);
            let z = b.icmp(IntCond::Eq, y, zero);
            let m1 = b.icmp(IntCond::Eq, y, mone);
            let mn = b.icmp(IntCond::Eq, x, min);
            let ovf = and(b, m1, mn);
            let bad = or(b, z, ovf);
            let d = b.select(bad, one, y);
            let q = b.int_bin(IntBinOp::Sdiv, x, d);
            let ok = fits(b, q, w);
            let nok = not(b, ok);
            let ez = raise(b, z, raise::DIV_ZERO);
            let eo = raise(b, nok, range_code(w));
            Out { value: q, err: or(b, ez, eo) }
        }
        Arith::Mod => {
            let one = b.iconst(Type::I64, 1);
            let mone = b.iconst(Type::I64, -1);
            let z = b.icmp(IntCond::Eq, y, zero);
            let m1 = b.icmp(IntCond::Eq, y, mone);
            let mask = or(b, z, m1);
            let d = b.select(mask, one, y);
            let _ = f;
            Out { value: b.int_bin(IntBinOp::Srem, x, d), err: raise(b, z, raise::DIV_ZERO) }
        }
    }
}

fn float_arith(b: &mut FunctionBuilder<'_>, op: Arith, w: u8, x: Value, y: Value) -> Out {
    let (xi, yi) = (is_inf(b, x, w), is_inf(b, y, w));
    let finite_in = {
        let o = or(b, xi, yi);
        not(b, o)
    };
    let f = b.bconst(false);
    let (r, zero, ovf, under) = match op {
        Arith::Add | Arith::Sub => {
            let r = b.float_bin(if op == Arith::Add { FloatBinOp::Add } else { FloatBinOp::Sub }, x, y);
            let ri = is_inf(b, r, w);
            (r, f, and(b, ri, finite_in), f)
        }
        Arith::Mul => {
            let r = b.float_bin(FloatBinOp::Mul, x, y);
            let ri = is_inf(b, r, w);
            let ovf = and(b, ri, finite_in);
            let rz = is_zero(b, r, w);
            let xz = is_zero(b, x, w);
            let yz = is_zero(b, y, w);
            let nz = {
                let o = or(b, xz, yz);
                not(b, o)
            };
            let under = and(b, rz, nz);
            (r, f, ovf, under)
        }
        _ => {
            let yz = is_zero(b, y, w);
            let xn = is_nan(b, x);
            let nxn = not(b, xn);
            let zero = and(b, yz, nxn);
            let one = if w == 4 { b.f32const(1.0) } else { b.f64const(1.0) };
            let d = b.select(zero, one, y);
            let r = b.float_bin(FloatBinOp::Div, x, d);
            let ri = is_inf(b, r, w);
            let nxi = not(b, xi);
            let ovf = and(b, ri, nxi);
            let rz = is_zero(b, r, w);
            let xz = is_zero(b, x, w);
            let nxz = not(b, xz);
            let nyi = not(b, yi);
            let u0 = and(b, rz, nxz);
            let under = and(b, u0, nyi);
            (r, zero, ovf, under)
        }
    };
    let (ez, eo, eu) = (raise(b, zero, raise::DIV_ZERO), raise(b, ovf, raise::FLOAT_OVF), raise(b, under, raise::FLOAT_UNDER));
    let e = or(b, ez, eo);
    Out { value: r, err: or(b, e, eu) }
}

/// The tier-A form of `funcid` over the `I64` words `a`; `None` when it is not one.
pub fn emit(b: &mut FunctionBuilder<'_>, funcid: Oid, a: &[Value]) -> Option<Out> {
    let form = form_of(funcid)?;
    if matches!(form, Form::DateTime) {
        return None;
    }
    let f = no_raise(b);
    let x = a[0];
    let y = a.get(1).copied();
    Some(match form {
        Form::IntArith(op, w) => int_arith(b, op, w, x, y?),
        Form::IntCmp(c) => {
            let r = b.icmp(icond(c, true), x, y?);
            Out { value: zext(b, r), err: f }
        }
        Form::UCmp(c) => {
            let (x, y) = (b.conv(ConvOp::Trunc, Type::I32, x), b.conv(ConvOp::Trunc, Type::I32, y?));
            let r = b.icmp(icond(c, false), x, y);
            Out { value: zext(b, r), err: f }
        }
        Form::CharCmp(c) => {
            // D2689: char.c orders the bytes as uint8
            let (x, y) = (b.conv(ConvOp::Trunc, Type::I8, x), b.conv(ConvOp::Trunc, Type::I8, y?));
            let r = b.icmp(icond(c, false), x, y);
            Out { value: zext(b, r), err: f }
        }
        Form::BoolCmp(c) => {
            let zero = b.iconst(Type::I64, 0);
            let (x, y) = (b.icmp(IntCond::Ne, x, zero), b.icmp(IntCond::Ne, y?, zero));
            let (x, y) = (zext(b, x), zext(b, y));
            let r = b.icmp(icond(c, true), x, y);
            Out { value: zext(b, r), err: f }
        }
        Form::IntBtCmp => {
            let y = y?;
            let gt = b.icmp(IntCond::Sgt, x, y);
            let lt = b.icmp(IntCond::Slt, x, y);
            Out { value: three_way(b, gt, lt), err: f }
        }
        Form::UBtCmp => {
            let (x, y) = (b.conv(ConvOp::Trunc, Type::I32, x), b.conv(ConvOp::Trunc, Type::I32, y?));
            let gt = b.icmp(IntCond::Ugt, x, y);
            let lt = b.icmp(IntCond::Ult, x, y);
            Out { value: three_way(b, gt, lt), err: f }
        }
        Form::BoolBtCmp => {
            let zero = b.iconst(Type::I64, 0);
            let (x, y) = (b.icmp(IntCond::Ne, x, zero), b.icmp(IntCond::Ne, y?, zero));
            let (x, y) = (zext(b, x), zext(b, y));
            let gt = b.icmp(IntCond::Sgt, x, y);
            let lt = b.icmp(IntCond::Slt, x, y);
            Out { value: three_way(b, gt, lt), err: f }
        }
        Form::CharBtCmp => {
            // D2742: nbtcompare.c's btcharcmp, the uint8 difference (D2689's sibling: GREATEST / LEAST / a row compare over "char"
            // read its sign; a direct call shows the value)
            let (x, y) = (b.conv(ConvOp::Trunc, Type::I8, x), b.conv(ConvOp::Trunc, Type::I8, y?));
            let (x, y) = (b.conv(ConvOp::Zext, Type::I64, x), b.conv(ConvOp::Zext, Type::I64, y));
            Out { value: b.int_bin(IntBinOp::Sub, x, y), err: f }
        }
        Form::Int2BtCmp => {
            // D2742: nbtcompare.c's btint2cmp, the difference of the two int2 values (in int32 range)
            let (x, y) = (b.conv(ConvOp::Trunc, Type::I16, x), b.conv(ConvOp::Trunc, Type::I16, y?));
            let (x, y) = (b.conv(ConvOp::Sext, Type::I64, x), b.conv(ConvOp::Sext, Type::I64, y));
            Out { value: b.int_bin(IntBinOp::Sub, x, y), err: f }
        }
        Form::IntNeg(w) => {
            let zero = b.iconst(Type::I64, 0);
            if w == 8 {
                let o = b.int_ovf(IntOvfOp::SsubOvf, zero, x);
                Out { value: b.int_bin(IntBinOp::Sub, zero, x), err: raise(b, o, raise::INT8_RANGE) }
            } else {
                let r = b.int_bin(IntBinOp::Sub, zero, x);
                let ok = fits(b, r, w);
                let nok = not(b, ok);
                Out { value: r, err: raise(b, nok, range_code(w)) }
            }
        }
        Form::IntAbs(w) => {
            let zero = b.iconst(Type::I64, 0);
            let neg = b.icmp(IntCond::Slt, x, zero);
            let n = b.int_bin(IntBinOp::Sub, zero, x);
            let r = b.select(neg, n, x);
            let bad = if w == 8 {
                let min = b.iconst(Type::I64, i64::MIN);
                b.icmp(IntCond::Eq, x, min)
            } else {
                let ok = fits(b, r, w);
                not(b, ok)
            };
            Out { value: r, err: raise(b, bad, range_code(w)) }
        }
        Form::Bit(op) => Out { value: b.int_bin(op, x, y?), err: f },
        Form::BitNot => {
            let m = b.iconst(Type::I64, -1);
            Out { value: b.int_bin(IntBinOp::Xor, x, m), err: f }
        }
        Form::IntMinMax(larger) => Out { value: b.int_bin(if larger { IntBinOp::Smax } else { IntBinOp::Smin }, x, y?), err: f },
        Form::IntNarrow(w) => {
            let ok = fits(b, x, w);
            let nok = not(b, ok);
            Out { value: x, err: raise(b, nok, range_code(w)) }
        }
        Form::Widen(w) => {
            let t = b.conv(ConvOp::Trunc, int_ty(w), x);
            Out { value: b.conv(ConvOp::Sext, Type::I64, t), err: f }
        }
        Form::DateTime => return None,
        Form::DateMi => {
            // PG's date_mi: -infinity / infinity are i32::MIN / i32::MAX (DATEVAL_NOBEGIN / NOEND) and raise 22008.
            let (x, y) = (b.conv(ConvOp::Trunc, Type::I32, x), b.conv(ConvOp::Trunc, Type::I32, y?));
            let (wx, wy) = (b.conv(ConvOp::Sext, Type::I64, x), b.conv(ConvOp::Sext, Type::I64, y));
            let (lo, hi) = (b.iconst(Type::I64, i32::MIN as i64), b.iconst(Type::I64, i32::MAX as i64));
            let (xl, xh, yl, yh) = (b.icmp(IntCond::Eq, wx, lo), b.icmp(IntCond::Eq, wx, hi), b.icmp(IntCond::Eq, wy, lo), b.icmp(IntCond::Eq, wy, hi));
            let (bx, by) = (or(b, xl, xh), or(b, yl, yh));
            let bad = or(b, bx, by);
            let d = b.int_bin(IntBinOp::Sub, x, y);
            Out { value: b.conv(ConvOp::Sext, Type::I64, d), err: raise(b, bad, raise::DATE_NOT_FINITE) }
        }
        Form::FloatArith(op, wa, wb, w) => {
            let (x, y) = (promote(b, x, wa, w), promote(b, y?, wb, w));
            let o = float_arith(b, op, w, x, y);
            Out { value: from_float(b, o.value, w), err: o.err }
        }
        Form::FloatCmp(c, wa, wb) => {
            let w = wa.max(wb);
            let (x, y) = (promote(b, x, wa, w), promote(b, y?, wb, w));
            let r = float_cmp(b, c, x, y);
            Out { value: zext(b, r), err: f }
        }
        Form::FloatBtCmp(wa, wb) => {
            let w = wa.max(wb);
            let (x, y) = (promote(b, x, wa, w), promote(b, y?, wb, w));
            let gt = float_cmp(b, Cmp::Gt, x, y);
            let lt = float_cmp(b, Cmp::Lt, x, y);
            Out { value: three_way(b, gt, lt), err: f }
        }
        Form::FloatNeg(w) => {
            let x = to_float(b, x, w);
            let r = b.float_un(FloatUnOp::Neg, x);
            Out { value: from_float(b, r, w), err: f }
        }
        Form::FloatAbs(w) => {
            let x = to_float(b, x, w);
            let r = b.float_un(FloatUnOp::Abs, x);
            Out { value: from_float(b, r, w), err: f }
        }
        Form::FloatMinMax(larger, w) => {
            let (x, y) = (to_float(b, x, w), to_float(b, y?, w));
            let c = float_cmp(b, if larger { Cmp::Gt } else { Cmp::Lt }, x, y);
            let r = b.select(c, x, y);
            Out { value: from_float(b, r, w), err: f }
        }
        Form::IntToFloat(w) => {
            let r = b.conv(ConvOp::SintToFloat, flt_ty(w), x);
            Out { value: from_float(b, r, w), err: f }
        }
        Form::Promote => {
            let r = promote(b, x, 4, 8);
            Out { value: from_float(b, r, 8), err: f }
        }
        Form::Demote => {
            let x = to_float(b, x, 8);
            let r = b.conv(ConvOp::Fdemote, Type::F32, x);
            let ri = is_inf(b, r, 4);
            let xi = is_inf(b, x, 8);
            let nxi = not(b, xi);
            let ovf = and(b, ri, nxi);
            let rz = is_zero(b, r, 4);
            let xz = is_zero(b, x, 8);
            let nxz = not(b, xz);
            let under = and(b, rz, nxz);
            let eo = raise(b, ovf, raise::FLOAT_OVF);
            let eu = raise(b, under, raise::FLOAT_UNDER);
            Out { value: from_float(b, r, 4), err: or(b, eo, eu) }
        }
    })
}

/// The vector compare of a tier-A compare over a lane of `width` (`word.rs`): signed for the
/// integer, date, timestamp and bool orders, unsigned for oid and "char" (D2689); an 8-bit lane takes only
/// `=` / `<>` (its scalar path zero-extends); `swapped` mirrors a `const op var`.
pub fn vector_cond(funcid: Oid, width: u8, swapped: bool) -> Option<IntCond> {
    let (c, signed, w_ok) = match form_of(funcid)? {
        Form::IntCmp(c) => (c, true, width >= 2),
        Form::UCmp(c) => (c, false, width == 4),
        Form::CharCmp(c) => (c, false, width == 1),
        Form::BoolCmp(c) => (c, true, width == 1),
        _ => return None,
    };
    if !w_ok || (width == 1 && !matches!(c, Cmp::Eq | Cmp::Ne)) {
        return None;
    }
    let c = if swapped {
        match c {
            Cmp::Lt => Cmp::Gt,
            Cmp::Gt => Cmp::Lt,
            Cmp::Le => Cmp::Ge,
            Cmp::Ge => Cmp::Le,
            x => x,
        }
    } else {
        c
    };
    Some(icond(c, signed))
}

/// The vector `+` / `-` of a tier-A integer op whose operands are both of `width` (2 / 4): wrapping
/// at the width, the overflow a mask (`vec.rs`); the mixed-width forms (`int24pl`) and int8 refused.
pub fn vector_arith(funcid: Oid, width: u8) -> Option<VecBinOp> {
    let name = gravity_fn::row_of(funcid)?.name;
    let Form::IntArith(ar, w) = form_of(funcid)? else { return None };
    let digits = name.strip_prefix("int")?.trim_end_matches(|c: char| !c.is_ascii_digit()).len();
    if w != width || digits != 1 || w == 8 {
        return None;
    }
    match ar {
        Arith::Add => Some(VecBinOp::Add),
        Arith::Sub => Some(VecBinOp::Sub),
        _ => None,
    }
}
