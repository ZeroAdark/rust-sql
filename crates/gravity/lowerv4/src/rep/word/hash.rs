//! Word key images for hash / eq (`rep.word.hash`, plan §3.2; hash-contract.md §3.1). A by-value key's image is one exact
//! `I64` word, canonical under the type's `=`: equal values give equal words and equal words mean equal values, so a hit
//! needs no compare past the image (`Words::exact`).
//! - Integers (`WordImage::Raw` / `Enum`): the value at its type length, sign-extended when signed (int2 / int4 / int8,
//!   date, time, timestamp, money), zero-extended when not (oid, xid, bool, "char", enum oids). Every integer width of
//!   PostgreSQL's integer hash opfamily therefore keys alike (`int4 = int8` joins; `hashint8` agrees with `hashint4`).
//! - Floats (`WordImage::Float`): the value widened to float8 (`hashfloat4` hashes a float4 as its float8), `-0` folded to
//!   `+0` and every NaN to one pattern, as `float8eq` (all NaNs equal, `-0 = 0`). The image is not the datum: a keyed form
//!   that returns the key keeps the datum (a group of `-0` and `0` shows the first row's value, as PostgreSQL's).
//! - D2723: a by-value type the type table does not list (an enum, a user by-value type) whose `=` is user code (a user
//!   DEFAULT btree / hash opclass: typesem's D1604 refusal) keys by its CLASS under the type's own support functions
//!   (typesem `user_key`, the host `m6_user_class`, leader only): the image is its class representative's word (equal
//!   words are not equal values there, nor the reverse). D2640: a hashed group's by-value key under a user `=` likewise,
//!   by its class under the operator's own hash class procs (`TypeFacts::user`, rep/hosted/user_op.rs).
//! The image hashes only through `KeyHash` (face/keyimg.rs, `emit_key_hash`); a NULL key's word is zeroed there.
use gravity_decompose::catalog::typefacts::TypeFacts;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, FloatCond, IntCond, Type, Value};
use gravity_program::Rep;

use crate::cell::At;
use crate::face::kinds::KindId;
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::ops::Table;
use crate::rep::{self, CellRef, HashArm, Lx, Scope, StoredRef, Words};

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::RepWordHash, &ARM)], ..crate::ops::NONE };

pub(crate) const ARM: HashArm = HashArm { image, eq };

/// Every NaN's image: float8's canonical quiet NaN.
pub(crate) const F64_NAN: u64 = 0x7FF8_0000_0000_0000;

fn image(cx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, tf: &TypeFacts, _scope: Scope) -> Words {
    Words { w: [Some(word(cx, c, tf)), None], exact: true }
}

/// Equality against a stored image: the stored word is this arm's image, so equal words are equal keys.
fn eq(cx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, stored: StoredRef, tf: &TypeFacts) -> Value {
    let w = word(cx, c, tf);
    let s = cx.b.load(Type::I64, stored.ptr, stored.off);
    cx.b.icmp(IntCond::Eq, w, s)
}

/// The image word of a word cell (its datum through the Rep's datum arm, memoised per row).
pub(crate) fn word(cx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, tf: &TypeFacts) -> Value {
    // D2640: a hashed group's key under a user `=` (`TypeFacts::user`, rep/hosted/user_op.rs `tf`) keys by its class
    // under the operator's own hash class procs, its representative's word
    if tf.user != 0 {
        return user_class(cx, c, tf, tf.user);
    }
    if let Some(spec) = user_spec(cx, tf) {
        return user_class(cx, c, tf, spec);
    }
    let (v, _) = rep::datum(cx, c, tf);
    // a 2 / 4-byte lane element's datum is already sign-extended (rep::datum_word): a signed key's image is that word
    let extended = matches!((c.cell.at, c.cell.rep), (At::Lane(_), Rep::Word { bytes: b @ (2 | 4) }) if b as i16 == tf.typlen);
    match (tf.image.int_signed(), tf.image.float32()) {
        (Some(true), _) if extended => v,
        (Some(signed), _) => int_image(cx.b, v, tf.typlen, signed),
        _ if tf.image.is_enum() => int_image(cx.b, v, 4, false),
        (_, Some(f32)) => float_image(cx.b, v, f32),
        _ => crate::refuse!(cx, Reason::Bug { payload: "a word cell whose type has no word image" }),
    }
}

/// D2723: the user key of a by-value key type the type table does not list whose `=` is user code (typesem `form` refuses
/// it by a user-code reason), or `None` (its word is its image). Such a type with no usable user key refuses by typesem's
/// reason: PostgreSQL compares it by user code, so its word is no image.
fn user_spec(cx: &mut Lx<'_, '_, '_>, tf: &TypeFacts) -> Option<u64> {
    // KeySem decides it (keysem::rep::word_user_key)
    match gravity_fn::typesem::keysem::rep::word_user_key(tf.typid, tf.byval, gravity_decompose::catalog::typefacts::row_of(tf.typid).is_some()) {
        Ok(spec) => spec,
        Err(why) => crate::refuse!(cx, Reason::Shape { why: ShapeWhy(why) }),
    }
}

/// D2723: the word of the class representative of the key's word under user key `spec` in the run's classes (the host
/// `m6_user_class` over the program's `keys.user` handle, D4197: an exact image that renders back as a member of this
/// run's rows), 0 for a NULL row; the pipeline runs on the leader (user code reaches SPI).
fn user_class(cx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, tf: &TypeFacts, spec: u64) -> Value {
    gravity_fn::typesem::keysem::rep::user_key_site();
    let (v, null) = rep::datum(cx, c, tf);
    let zero = cx.b.iconst(Type::I64, 0);
    let (l_val, l_out) = (cx.b.create_block(&[]), cx.b.create_block(&[Type::I64]));
    cx.b.cond_br(null, (l_out, &[zero]), (l_val, &[]));
    cx.b.switch_to(l_val);
    let ctx = cx.frame().ctx;
    let Some(keys) = crate::frame::env::user_keys(cx.b, ctx) else { crate::refuse!(cx, Reason::Bug { payload: crate::frame::env::NO_USER_KEYS }) };
    let out = crate::rep::varlena::hash::out2(cx.b);
    let flags = gravity_ir::HostFlags { may_error: true, touches_memory: true, ..gravity_ir::HostFlags::default() };
    let host = gravity_kernels::contract::entry4::<gravity_kernels::typesem::userfp::UserClass> as *const () as usize;
    let f = cx.b.declare_host_once("m6_user_class", &[Type::Ptr, Type::I64, Type::I64, Type::I64, Type::Ptr], Some(Type::I64), host, flags);
    let spec = cx.b.iconst(Type::I64, spec as i64);
    let st = cx.b.call_host(f, &[ctx, keys, v, spec, out], Some(Type::I64)).expect("m6_user_class returns its status");
    crate::rep::varlena::hash::raise_on(cx, st);
    let w = cx.b.load(Type::I64, out, 0);
    cx.b.br(l_out, &[w]);
    cx.b.switch_to(l_out);
    cx.b.block_params(l_out)[0]
}

/// An integer datum word at `typlen` bytes, extended to the canonical `I64`: the datum's low bytes decide, whatever its upper
/// bits (a 4-byte pgrc lane is loaded sign-extended; an oid datum is zero-extended).
pub fn int_image(b: &mut FunctionBuilder<'_>, v: Value, typlen: i16, signed: bool) -> Value {
    let ty = match typlen {
        1 => Type::I8,
        2 => Type::I16,
        4 => Type::I32,
        _ => return v,
    };
    let t = b.conv(ConvOp::Trunc, ty, v);
    b.conv(if signed { ConvOp::Sext } else { ConvOp::Zext }, Type::I64, t)
}

/// A float datum word's canonical image: float8 bits of the (widened) value, `-0 → +0`, every NaN → [`F64_NAN`].
pub fn float_image(b: &mut FunctionBuilder<'_>, v: Value, f32: bool) -> Value {
    let f = if f32 {
        let t = b.conv(ConvOp::Trunc, Type::I32, v);
        let s = b.conv(ConvOp::Bitcast, Type::F32, t);
        b.conv(ConvOp::Fpromote, Type::F64, s)
    } else {
        b.conv(ConvOp::Bitcast, Type::F64, v)
    };
    let bits = b.conv(ConvOp::Bitcast, Type::I64, f);
    let fz = b.f64const(0.0);
    let zero = b.iconst(Type::I64, 0);
    let is_zero = b.fcmp(FloatCond::Eq, f, fz);
    let z = b.select(is_zero, zero, bits);
    let nan = b.fcmp(FloatCond::Unordered, f, f);
    let nan_w = b.const_raw(Type::I64, F64_NAN);
    b.select(nan, nan_w, z)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::{At, Cell, CellHome, CellLife, Nullable};
    use gravity_decompose::catalog::typefacts::row_of;
    use gravity_interp::Interp;
    use gravity_ir::builder::ProgramBuilder;
    use gravity_program::{NumTypmod, Rep, Until};

    /// The image of the datum word `d` of a computed cell of type `typid` (8-byte word, as a fmgr result).
    fn img(typid: u32, d: u64) -> u64 {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let tf = *row_of(typid).expect("type facts");
        let c = Cell { rep: Rep::Word { bytes: 8 }, typid, num: NumTypmod::NONE, life: CellLife { until: Until::Row, home: CellHome::RowCx }, at: At::Value(0), null: Nullable::Never };
        let mut pb = ProgramBuilder::new();
        let f = pb.declare_func("i", &[Type::I64], &[Type::I64]);
        let mut b = pb.define(f);
        let p = b.params()[0];
        let v = {
            let mut cx = Lx::new(&mut b, 1, rig.mcx());
            let w = image(&mut cx, CellRef { cell: &c, val: p, null: None }, &tf, Scope::Program);
            assert!(w.exact && w.w[1].is_none());
            w.w[0].unwrap()
        };
        b.ret(&[v]);
        let prog = pb.finish();
        Interp::new(&prog).run(f, &[d]).expect("runs")[0]
    }

    #[test]
    fn integer_widths_key_alike() {
        assert_eq!(img(23, 0xDEAD_BEEF_FFFF_FFFB), -5i64 as u64, "int4 by its low 32 bits, sign-extended");
        assert_eq!(img(21, 0xFFFB), -5i64 as u64, "int2");
        assert_eq!(img(20, -5i64 as u64), -5i64 as u64, "int8");
        assert_eq!(img(26, 0xFFFF_FFFF_FFFF_FFFB), 0xFFFF_FFFB, "oid zero-extended");
        assert_eq!(img(16, 0x101), 1, "bool by its low byte");
    }

    #[test]
    fn floats_fold_negative_zero_and_nan() {
        let f8 = |x: f64| img(701, x.to_bits());
        let f4 = |x: f32| img(700, x.to_bits() as i32 as i64 as u64);
        assert_eq!(f8(-0.0), f8(0.0));
        assert_eq!(f8(f64::NAN), F64_NAN);
        assert_eq!(f8(-f64::NAN), F64_NAN);
        assert_eq!(f4(-0.0), 0);
        assert_eq!(f4(f32::NAN), F64_NAN);
        assert_eq!(f4(1.5), f8(1.5), "a float4 keys as its float8");
        assert_ne!(f8(1.0), f8(-1.0));
        assert_eq!(f4(f32::INFINITY), f8(f64::INFINITY));
    }
}
