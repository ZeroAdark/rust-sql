//! Road `key.tagged` (H21, R5-L10: tagged bytes head / tail images; L3 straight, L6 engaged) (cards/M6.md,
//! M6-changes.md §1).
//! Engaged (L6) where the road applies: a statement with no pgrc source and no code key (applies.rs
//! `VarlenaKeyHeap`: every side of every table / join is heap, so no key meets a dictionary's Fp128), a `Bytes` image
//! (text, varchar, bpchar trimmed by `bcTruelen`, bytea; the caller asks only for a key whose collation compares bytes,
//! keyimg.rs `bytes_eq`: a nondeterministic collation keeps key.coll / the Fp128). The image of a payload of at most 15
//! bytes is the bytes themselves, `[b0..7, b8..14 | len << 56]` (exact: equal bytes, equal words); a longer payload is
//! its Fp128 with the top byte `0xFF` (a length past 15 never reads as one, so the two forms never meet). A NULL row's
//! words are 0 (the image's flags word marks it). The bytes are read with loads that stay on the 4 KiB pages the payload
//! touches: each word is the 8 bytes ending at the payload's last byte when those share its page, else the 8 bytes from
//! its first byte (a varlena's header and the bytes before it are on its page then). A numeric key (a varlena lane
//! with no decided scale) keeps rep.varlena.hash's `m6_num_fp`. Declines (`None`) everywhere else.
use gravity_decompose::catalog::typefacts::{KeyColl, TypeFacts};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, Type, Value};

use crate::ops::Table;
use crate::rep::{self, CellRef, Lx, Scope, Words};

use crate::rep::varkey::{TailForm, VarKeyImpl, VarKeyMech};

crate::strategies! {
    /// Tagged bytes head / tail images.
    KEY_TAGGED = 82, "key.tagged", VarlenaKeyHeap;
}

pub(crate) const ROWS: Table = Table { strategies: &[(KEY_TAGGED, &IMPL)], ..crate::ops::NONE };

const IMPL: VarKeyImpl = VarKeyImpl(&Tagged);

/// The inline form's length bound: a payload of at most this many bytes is its own image (text.set's list images too).
pub(crate) const INLINE: i64 = 15;

struct Tagged;

impl VarKeyMech for Tagged {
    fn hash(&self, lx: &mut Lx<'_, '_, '_>, c: CellRef<'_>, tf: &TypeFacts, _: KeyColl, _: Scope) -> Option<Words> {
        if !lx.strategies.has(KEY_TAGGED) || !tf.image.is_bytes() {
            return None;
        }
        let (p, null) = rep::datum(lx, c, tf);
        let zero = lx.b.iconst(Type::I64, 0);
        let (l_val, l_out) = (lx.b.create_block(&[]), lx.b.create_block(&[Type::I64, Type::I64]));
        lx.b.cond_br(null, (l_out, &[zero, zero]), (l_val, &[]));
        lx.b.switch_to(l_val);
        let p = crate::rep::varlena::hash::ptr(lx.b, p);
        let (bytes, len, mark) = crate::rep::varlena::hash::payload(lx, p, tf);
        let (w0, w1) = image(lx, bytes, len);
        crate::rep::varlena::hash::release(lx, mark);
        lx.b.br(l_out, &[w0, w1]);
        lx.b.switch_to(l_out);
        let o = lx.b.block_params(l_out);
        Some(Words { w: [Some(o[0]), Some(o[1])], exact: true })
    }

    fn order(&self, _: &mut Lx<'_, '_, '_>, _: CellRef<'_>, _: &TypeFacts, _: KeyColl) -> Option<(Value, TailForm)> {
        None
    }
}

/// The image of the `len` payload bytes at `bytes`: inline up to `INLINE` bytes, else the Fp128 with its top byte 0xFF.
fn image(lx: &mut Lx<'_, '_, '_>, bytes: Value, len: Value) -> (Value, Value) {
    image_as(lx, bytes, len, Hashed::Tagged)
}

/// How a payload past `INLINE` bytes images: key.tagged's Fp128 under a top byte 0xFF, or key.canon's (key_canon.rs) under
/// its one flag bit, bit 63 (`gravity_dict::fp::HASHED`: 127 bits of the Fp128).
#[derive(Clone, Copy)]
pub(super) enum Hashed {
    Tagged,
    Canon,
}

/// `image` under `form`: the inline form is the same two words either way (bit 63 clear: the length is at most 15).
pub(super) fn image_as(lx: &mut Lx<'_, '_, '_>, bytes: Value, len: Value, form: Hashed) -> (Value, Value) {
    let b = &mut *lx.b;
    let (zero, s56) = (b.iconst(Type::I64, 0), b.iconst(Type::I64, 56));
    let lim = b.iconst(Type::I64, INLINE);
    let long = b.icmp(IntCond::Ugt, len, lim);
    let (l_short, l_long, l_out) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::I64, Type::I64]));
    b.cond_br(long, (l_long, &[]), (l_short, &[]));
    b.switch_to(l_short);
    let tag = b.int_bin(IntBinOp::Shl, len, s56);
    let (eight, empty) = (b.iconst(Type::I64, 8), b.icmp(IntCond::Eq, len, zero));
    let (l_some, l_two) = (b.create_block(&[]), b.create_block(&[]));
    b.cond_br(empty, (l_out, &[zero, tag]), (l_some, &[]));
    b.switch_to(l_some);
    let k0 = b.int_bin(IntBinOp::Umin, len, eight);
    let w0 = tail(b, bytes, k0);
    let two = b.icmp(IntCond::Ugt, len, eight);
    b.cond_br(two, (l_two, &[]), (l_out, &[w0, tag]));
    b.switch_to(l_two);
    let hi = b.ptr_add(bytes, zero, 1, 8);
    let k1 = b.int_bin(IntBinOp::Sub, len, eight);
    let w1 = tail(b, hi, k1);
    let w1 = b.int_bin(IntBinOp::Or, w1, tag);
    b.br(l_out, &[w0, w1]);
    b.switch_to(l_long);
    let [lo, hi] = crate::rep::varlena::hash::fp(lx, bytes, len);
    let b = &mut *lx.b;
    let (keep, top) = match form {
        Hashed::Tagged => (u64::MAX >> 8, 0xFFu64 << 56),
        Hashed::Canon => (!gravity_dict::fp::HASHED, gravity_dict::fp::HASHED),
    };
    let (keep, top) = (b.iconst(Type::I64, keep as i64), b.iconst(Type::I64, top as i64));
    let hi = b.int_bin(IntBinOp::And, hi, keep);
    let hi = b.int_bin(IntBinOp::Or, hi, top);
    b.br(l_out, &[lo, hi]);
    b.switch_to(l_out);
    let o = b.block_params(l_out);
    (o[0], o[1])
}

/// The `k` bytes at `q` (1 <= k <= 8, all of them the payload's) as one little-endian word, zero above them: the 8 bytes
/// ending at `q + k` when the first of them shares the last byte's 4 KiB page (the word's top `k` bytes, shifted down),
/// else the 8 bytes from `q` masked to `k` (the last byte is then among a page's first 7, so the read ends on its page).
pub(super) fn tail(b: &mut FunctionBuilder<'_>, q: Value, k: Value) -> Value {
    let qi = b.conv(gravity_ir::ConvOp::PtrToInt, Type::I64, q);
    let (one, eight, page) = (b.iconst(Type::I64, 1), b.iconst(Type::I64, 8), b.iconst(Type::I64, 4095));
    let last = b.int_bin(IntBinOp::Add, qi, k);
    let last = b.int_bin(IntBinOp::Sub, last, one);
    let off = b.int_bin(IntBinOp::And, last, page);
    let seven = b.iconst(Type::I64, 7);
    let back = b.icmp(IntCond::Uge, off, seven);
    let start = b.int_bin(IntBinOp::Sub, last, seven);
    let at = b.select(back, start, qi);
    let at = b.conv(gravity_ir::ConvOp::IntToPtr, Type::Ptr, at);
    let raw = b.load_unaligned(Type::I64, at, 0);
    let pad = b.int_bin(IntBinOp::Sub, eight, k);
    let s3 = b.iconst(Type::I64, 3);
    let pad_bits = b.int_bin(IntBinOp::Shl, pad, s3);
    let zero = b.iconst(Type::I64, 0);
    let sh = b.select(back, pad_bits, zero);
    let v = b.int_bin(IntBinOp::Ushr, raw, sh);
    let ones = b.iconst(Type::I64, -1);
    let mask = b.int_bin(IntBinOp::Ushr, ones, pad_bits);
    b.int_bin(IntBinOp::And, v, mask)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gravity_interp::Interp;
    use gravity_ir::builder::ProgramBuilder;

    /// `tail(q, k)` run by the interpreter.
    fn run_tail(q: *const u8, k: u64) -> u64 {
        let mut pb = ProgramBuilder::new();
        let f = pb.declare_func("t", &[Type::I64, Type::I64], &[Type::I64]);
        let mut b = pb.define(f);
        let (p, k_) = (b.params()[0], b.params()[1]);
        let p = b.conv(gravity_ir::ConvOp::IntToPtr, Type::Ptr, p);
        let v = tail(&mut b, p, k_);
        b.ret(&[v]);
        let prog = pb.finish();
        Interp::new(&prog).run(f, &[q as u64, k]).expect("runs")[0]
    }

    /// PERF-TH01 L1: every `k` (1..=8) at every placement around a 4 KiB boundary reads exactly the `k` bytes at `q`,
    /// little-endian, zero above them: the backward read (the 8 bytes ending at the last byte) and the forward one (the
    /// last byte among a page's first 7) agree with the bytes.
    #[test]
    fn a_tail_word_is_its_k_bytes_on_either_side_of_a_page_edge() {
        let mut buf = vec![0u8; 3 * 4096];
        for (i, x) in buf.iter_mut().enumerate() {
            *x = (i * 131 + 7) as u8;
        }
        let base = buf.as_ptr() as usize;
        let page = (base + 4095) & !4095;
        for at in page + 4096 - 24..page + 4096 + 24 {
            for k in 1..=8usize {
                let q = at as *const u8;
                let want = (0..k).fold(0u64, |w, i| w | (u64::from(buf[at - base + i]) << (8 * i)));
                assert_eq!(run_tail(q, k as u64), want, "k {k} at page offset {}", at & 4095);
            }
        }
    }
}
