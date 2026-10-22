//! Kind `rep.varlena.hash` (`HashArm`: text / varchar / bpchar / bytea / inet / cidr / heap numeric key images; L3, first
//! runnable) (M6-plan §3.1, cards/M6.md). Owner M6-TEXT.
//! - `image`: the roads first (face/varkey.rs: key.surrogate, key.tagged, key.coll, each while on), then the exact Fp128
//!   of the payload (M6 Decision 3a): SipHash-2-4-128 under the process seed, the function every dictionary's Fp128 table
//!   holds for an entry (`gravity_dict::fp`), so a heap key and a pgrc code of the same bytes have one image and equal
//!   images are equal keys (D247; the bytes-verify of long keys dropped on purpose). The payload: 1-byte and 4-byte
//!   uncompressed headers decoded inline (`gravity_expr::siphash::varlena`), a compressed or external value through
//!   `m6_detoast` into the Call's scratch (I15), handed back once the image is taken (D2239 `release`); a bpchar trimmed
//!   of its trailing blanks first (`bcTruelen`, I14: `hashbpchar`'s image). A NULL row's words are 0 (the image's flags
//!   word marks it).
//! - A numeric over a varlena lane (no decided scale): `m6_num_fp`, the Fp128 of its normalized digits (`hash_numeric`'s
//!   canonical form: 1.0 = 1.00, every zero one image).
//! - `eq`: the two words against a stored image (never reached on an exact image; kept for callers holding a copy).
//! - join.payload_fp (ops/hashjoin/strategies/payload_fp.rs): a record's plain text value a hash probe (or drain) handed on
//!   with the Fp128 words its build stored (`Lx::fps`) images as those words, the image above taken once a build row.
//! Keys under a nondeterministic collation never reach this arm by the bytes (coderep.rs / join keys / WITH TIES refuse
//! them by key.coll's name until that road's image lands). The seed is read per image (`m6_fp_seed`, no immediate: the
//! code cache is keyed by content); hoisting it to the function's open is a perf row (M6 fold-in 7 timings first).
use gravity_abi::ctx::hdr;
use gravity_decompose::catalog::typefacts::{KeyColl, TypeFacts};
use gravity_expr::siphash;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, HostFlags, IntBinOp, IntCond, Type, Value};
use gravity_kernels::ops::agg::trans::entries::{m6_detoast, m6_fp_seed, m6_num_fp};

use crate::face::kinds::KindId;
use crate::admit::refusal::Reason;
use crate::ops::Table;
use crate::rep::{self, CellRef, HashArm, Lx, Scope, StoredRef, Words};

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::RepVarlenaHash, &ARM)], ..crate::ops::NONE };

pub(crate) const ARM: HashArm = HashArm { image, eq };

fn err() -> HostFlags {
    HostFlags { may_error: true, touches_memory: true, ..HostFlags::default() }
}

fn image(lx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, tf: &TypeFacts, scope: Scope) -> Words {
    if let Some([lo, hi]) = lx.fps.get(c.val) {
        return Words { w: [Some(lo), Some(hi)], exact: true };
    }
    crate::rep::varkey::hash_image(lx, c, tf, KeyColl::NONE, scope, fp_image)
}

/// The exact Fp128 of the cell's value: `[0, 0]` on a NULL row.
pub(crate) fn fp_image(lx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, tf: &TypeFacts) -> Words {
    let (p, null) = rep::datum(lx, c, tf);
    let zero = lx.b.iconst(Type::I64, 0);
    let (l_val, l_out) = (lx.b.create_block(&[]), lx.b.create_block(&[Type::I64, Type::I64]));
    lx.b.cond_br(null, (l_out, &[zero, zero]), (l_val, &[]));
    lx.b.switch_to(l_val);
    let p = ptr(lx.b, p);
    let [lo, hi] = if tf.image.is_num_form() {
        let out = out2(lx.b);
        let ctx = lx.frame().ctx;
        let f = lx.b.declare_host_once("m6_num_fp", &[Type::Ptr, Type::Ptr, Type::Ptr], Some(Type::I64), m6_num_fp as *const () as usize, err());
        let st = lx.b.call_host(f, &[ctx, p, out], Some(Type::I64)).expect("m6_num_fp returns its status");
        raise_on(lx, st);
        [lx.b.load(Type::I64, out, 0), lx.b.load(Type::I64, out, 8)]
    } else {
        let (bytes, len, mark) = payload(lx, p, tf);
        let w = fp(lx, bytes, len);
        release(lx, mark);
        w
    };
    lx.b.br(l_out, &[lo, hi]);
    lx.b.switch_to(l_out);
    let o = lx.b.block_params(l_out);
    Words { w: [Some(o[0]), Some(o[1])], exact: true }
}

/// Where a detoasted copy sits in the Call's scratch (`payload`): the window's `(cur, end)` before the copy; `cur` 0 when
/// the payload was read in place.
pub(crate) struct Mark {
    cur: Value,
    end: Value,
}

/// The payload `(ptr, len, mark)` of the non-NULL varlena at `p`: its bytes after an inline header, else the detoasted
/// copy in the Call's scratch; a bpchar's `len` without its trailing blanks. The caller takes its image from the bytes
/// and then hands the copy back (`release`).
pub(crate) fn payload(lx: &mut Lx<'_, '_, '_>, p: Value, tf: &TypeFacts) -> (Value, Value, Mark) {
    let v = siphash::varlena(lx.b, p);
    let zero = lx.b.iconst(Type::I64, 0);
    let bad = lx.b.icmp(IntCond::Ne, v.bad, zero);
    let (l_host, l_out) = (lx.b.create_block(&[]), lx.b.create_block(&[Type::Ptr, Type::I64, Type::I64, Type::I64]));
    lx.b.cond_br(bad, (l_host, &[]), (l_out, &[v.ptr, v.len, zero, zero]));
    lx.b.switch_to(l_host);
    lx.b.mark_cold(l_host);
    let (ctx, out) = (lx.frame().ctx, out2(lx.b));
    let (cur, end) = (lx.b.load(Type::I64, ctx, hdr::ARENA_CUR as i32), lx.b.load(Type::I64, ctx, hdr::ARENA_END as i32));
    let f = lx.b.declare_host_once("m6_detoast", &[Type::Ptr, Type::Ptr, Type::Ptr], Some(Type::I64), m6_detoast as *const () as usize, err());
    let st = lx.b.call_host(f, &[ctx, p, out], Some(Type::I64)).expect("m6_detoast returns its status");
    raise_on(lx, st);
    let (dp, dl) = (lx.b.load(Type::Ptr, out, 0), lx.b.load(Type::I64, out, 8));
    lx.b.br(l_out, &[dp, dl, cur, end]);
    lx.b.switch_to(l_out);
    let o = lx.b.block_params(l_out);
    let (bytes, len, mark) = (o[0], o[1], Mark { cur: o[2], end: o[3] });
    if !tf.image.is_bpchar() {
        return (bytes, len, mark);
    }
    // bcTruelen: drop trailing blanks, a backward scan (a loop the key's length bounds)
    let n0 = lx.b.num_blocks();
    let (l_head, l_byte, l_done) = (lx.b.create_block(&[Type::I64]), lx.b.create_block(&[]), lx.b.create_block(&[Type::I64]));
    lx.b.br(l_head, &[len]);
    lx.b.switch_to(l_head);
    lx.b.mark_loop_header(l_head);
    let n = lx.b.block_params(l_head)[0];
    let empty = lx.b.icmp(IntCond::Eq, n, zero);
    lx.b.cond_br(empty, (l_done, &[n]), (l_byte, &[]));
    lx.b.switch_to(l_byte);
    let last = lx.b.ptr_add(bytes, n, 1, -1);
    let c = lx.b.load_zext(Type::I8, last, 0);
    let blank = lx.b.iconst(Type::I64, b' ' as i64);
    let is_blank = lx.b.icmp(IntCond::Eq, c, blank);
    let one = lx.b.iconst(Type::I64, 1);
    let n1 = lx.b.int_bin(IntBinOp::Sub, n, one);
    lx.b.cond_br(is_blank, (l_head, &[n1]), (l_done, &[n]));
    lx.b.switch_to(l_done);
    lx.memo.bounded_since(lx.b, n0);
    (bytes, lx.b.block_params(l_done)[0], mark)
}

/// D2239: a detoasted copy goes back to the Call's scratch once the caller's image is taken, as PostgreSQL detoasts a
/// key in its per-tuple memory: kept to the Call's end, a copy per row held what the group table no longer does (100K
/// ~4.8 KB compressed keys: +212 MB VmHWM against the rowexec's +28 MB). The cursor goes back only while the window is
/// the one the copy came from (`end` unchanged: no chunk was taken under it); nothing is bumped between the copy and here
/// (the image is IR over the bytes, `m6_fp_seed` takes no scratch).
pub(crate) fn release(lx: &mut Lx<'_, '_, '_>, m: Mark) {
    let zero = lx.b.iconst(Type::I64, 0);
    let copied = lx.b.icmp(IntCond::Ne, m.cur, zero);
    let (l_back, l_done) = (lx.b.create_block(&[]), lx.b.create_block(&[]));
    lx.b.cond_br(copied, (l_back, &[]), (l_done, &[]));
    lx.b.switch_to(l_back);
    lx.b.mark_cold(l_back);
    let ctx = lx.frame().ctx;
    let (cur, end) = (lx.b.load(Type::I64, ctx, hdr::ARENA_CUR as i32), lx.b.load(Type::I64, ctx, hdr::ARENA_END as i32));
    let same = lx.b.icmp(IntCond::Eq, end, m.end);
    let back = lx.b.select(same, m.cur, cur);
    lx.b.store(ctx, hdr::ARENA_CUR as i32, back);
    lx.b.br(l_done, &[]);
    lx.b.switch_to(l_done);
}

/// The Fp128 `[lo, hi]` of the `len` bytes at `bytes` under the process seed (`gravity_dict::fp::siphash128`).
pub(crate) fn fp(lx: &mut Lx<'_, '_, '_>, bytes: Value, len: Value) -> [Value; 2] {
    let out = out2(lx.b);
    let f = lx.b.declare_host_once("m6_fp_seed", &[Type::Ptr], Some(Type::I64), m6_fp_seed as *const () as usize, err());
    let st = lx.b.call_host(f, &[out], Some(Type::I64)).expect("m6_fp_seed returns its status");
    raise_on(lx, st);
    let (k0, k1) = (lx.b.load(Type::I64, out, 0), lx.b.load(Type::I64, out, 8));
    let n0 = lx.b.num_blocks();
    let (lo, hi) = siphash::siphash128(lx.b, bytes, len, k0, k1);
    lx.memo.bounded_since(lx.b, n0);
    [lo, hi]
}

/// Equality against a stored image: the two Fp128 words.
fn eq(lx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, stored: StoredRef, tf: &TypeFacts) -> Value {
    let w = image(lx, c, tf, Scope::Program);
    let (Some(lo), Some(hi), true) = (w.w[0], w.w[1], w.exact) else { crate::refuse!(lx, Reason::Bug { payload: "a varlena key image that is not two exact words" }) };
    let p = ptr(lx.b, stored.ptr);
    let (slo, shi) = (lx.b.load(Type::I64, p, stored.off), lx.b.load(Type::I64, p, stored.off + 8));
    let (a, b) = (lx.b.int_bin(IntBinOp::Xor, lo, slo), lx.b.int_bin(IntBinOp::Xor, hi, shi));
    let d = lx.b.int_bin(IntBinOp::Or, a, b);
    let zero = lx.b.iconst(Type::I64, 0);
    lx.b.icmp(IntCond::Eq, d, zero)
}

pub(crate) fn ptr(b: &mut FunctionBuilder<'_>, v: Value) -> Value {
    if b.value_type(v) == Type::Ptr { v } else { b.conv(ConvOp::IntToPtr, Type::Ptr, v) }
}

/// Two words of stack a host writes its answer into.
pub(crate) fn out2(b: &mut FunctionBuilder<'_>) -> Value {
    let s = b.declare_stack_slot(16, 8);
    b.stack_addr(s)
}

/// A host's nonzero status leaves through the function's raise block with the HOST bit (its error stashed, D344).
pub(crate) fn raise_on(lx: &mut Lx<'_, '_, '_>, st: Value) {
    let Some(l_raise) = lx.raise else { crate::refuse!(lx, Reason::Bug { payload: "a varlena key host outside a function with a raise block" }) };
    let zero = lx.b.iconst(Type::I64, 0);
    let bad = lx.b.icmp(IntCond::Ne, st, zero);
    let e = lx.b.iconst(Type::I64, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST) as i64);
    let ok = lx.b.create_block(&[]);
    lx.b.cond_br(bad, (l_raise, &[e]), (ok, &[]));
    lx.b.switch_to(ok);
}
