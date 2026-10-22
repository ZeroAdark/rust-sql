//! A numeric constant operand of an expression extension (num.cmp, num.arith; D407, V4R-73): its bind word decoded once
//! per call at the extension's `entry` into a frame word, never baked (V4R-18: the front key is literal-free), and read
//! per row; and the compare table and operand strip num.cmp (cmp_num.rs) shares (gravfix3's num.aggcmp, D3024, takes them
//! too). Budget 110 lines. Owner M2-REP. Registers nothing.
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, IntCond, Type, Value};
use types_nodes::Node;

use crate::face::fold::NumShortImpl;
use crate::face::kinds::Face;
use crate::face::strategy::NUM_SHORT;
use crate::frame::env::Env;
use crate::frame::px::{ExId, Px, WordKey};

/// A declined constant (`konst`: NULL, a special, finer than the scale, past a word): no mantissa num.short gives. The
/// frame word holds `k ^ KONST_NONE` (`load_konst`), so a word no entry wrote reads as declined (cold), never as 0.
pub(crate) const KONST_NONE: i64 = i64::MIN;

/// Frame word `off`'s constant (`konst`'s, or `KONST_NONE`).
pub(crate) fn load_konst(b: &mut FunctionBuilder<'_>, env: &Env, off: i32) -> Value {
    let (w, none) = (env.load_word(b, off), b.iconst(Type::I64, KONST_NONE));
    b.int_bin(IntBinOp::Xor, w, none)
}

/// Stores `konst`'s `k` into frame word `off`.
pub(crate) fn store_konst(b: &mut FunctionBuilder<'_>, env: &Env, off: i32, k: Value) {
    let none = b.iconst(Type::I64, KONST_NONE);
    let w = b.int_bin(IntBinOp::Xor, k, none);
    env.store_word(b, off, w);
}

/// A constant operand's datum and valid flag, emitted where the row reads it (a cold block): `or_nan`'s datum.
pub(crate) type KDatum<'a> = &'a mut dyn FnMut(&mut FunctionBuilder<'_>, &mut Env) -> (Value, Value);

/// A NaN image (4-byte header): what a NULL constant's datum reads as, never a null pointer.
const NAN_IMAGE: i64 = (8 << 2) | (0xC000 << 32);

/// `p`, or a NaN image in a stack slot when `ok` is false (never an address baked into the program: pins and program
/// caches stay byte-identical across processes). Its callers are cold or entry blocks.
pub(crate) fn or_nan(b: &mut FunctionBuilder<'_>, p: Value, ok: Value) -> Value {
    let (sl, img) = (b.declare_stack_slot(8, 8), b.iconst(Type::I64, NAN_IMAGE));
    let a = b.stack_addr(sl);
    b.store(a, 0, img);
    let nan = b.conv(ConvOp::PtrToInt, Type::I64, a);
    b.select(ok, p, nan)
}

/// Frame word `i` of extension expression `ex` at step `at` (one per claimed constant, V4R-73), as a byte offset.
pub(crate) fn konst_word(px: &mut Px<'_, '_>, (k, j): (usize, usize), ex: ExId, i: u8) -> i32 {
    px.frame_word(WordKey::Open { face: Face::Rep, k, j: crate::frame::px::key_j(j) ^ ((ex.0 as u64 + 1) << 40), i }) as i32 * 8
}

/// Constant operand `id` (its bind word) decoded at `scale`, at a call's entry: `(mantissa or KONST_NONE, its dscale)`.
pub(crate) fn konst(b: &mut FunctionBuilder<'_>, env: &mut Env, id: ExId, scale: i16) -> (Value, Value) {
    // the operand's one bind word (frame/hoist.rs `load`'s Place::Word), never an immediate: that would be the literal
    let Some(&gravity_lower::Place::Word(w)) = env.placed(id).places.first() else { crate::refuse!(env.node, crate::admit::refusal::Reason::Bug { payload: "a numeric constant operand without its bind word" }) };
    let (p, nb) = (b.load(Type::I64, env.params, w as i32 * 8), b.load_zext(Type::I8, env.params_null, w as i32));
    let ok = b.icmp(IntCond::Eq, nb, env.zero);
    konst_at(b, env, p, ok, scale)
}

/// `konst` over the datum `p` (`ok`: not NULL).
pub(crate) fn konst_at(b: &mut FunctionBuilder<'_>, env: &mut Env, p: Value, ok: Value, scale: i16) -> (Value, Value) {
    let p = or_nan(b, p, ok);
    let (m, ok, ds) = crate::frame::iface::need_strategy::<NumShortImpl>(&env.node, NUM_SHORT).0.decode(b, env, p, scale, true);
    let none = b.iconst(Type::I64, KONST_NONE);
    (b.select(ok, m, none), ds.unwrap_or(env.zero))
}

/// `k` when `|k| < 10^digits` (`k + L - 1 <u 2L - 1`; `KONST_NONE` is not), else `KONST_NONE`.
pub(crate) fn bounded(b: &mut FunctionBuilder<'_>, k: Value, digits: u32) -> Value {
    let l = 10i64.pow(digits);
    let (l1, l2, none) = (b.iconst(Type::I64, l - 1), b.iconst(Type::I64, 2 * l - 1), b.iconst(Type::I64, KONST_NONE));
    let x = b.int_bin(IntBinOp::Add, k, l1);
    let inb = b.icmp(IntCond::Ult, x, l2);
    b.select(inb, k, none)
}

/// numeric_eq / ne / gt / ge / lt / le (1718-1723) as `x OP k`, and as `k OP x` (the swapped condition): num.cmp's
/// column and num.aggcmp's aggregate against a constant operand.
pub(crate) const CMP_OPS: [(u32, IntCond, IntCond); 6] = [
    (1718, IntCond::Eq, IntCond::Eq),
    (1719, IntCond::Ne, IntCond::Ne),
    (1720, IntCond::Sgt, IntCond::Slt),
    (1721, IntCond::Sge, IntCond::Sle),
    (1722, IntCond::Slt, IntCond::Sgt),
    (1723, IntCond::Sle, IntCond::Sge),
];

/// An operand without its RelabelType.
pub(crate) fn strip(e: Node<'_>) -> Node<'_> {
    e.as_relabel_type().map_or(e, |r| r.arg)
}
