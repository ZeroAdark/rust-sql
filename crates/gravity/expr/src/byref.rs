//! The by-reference output column face (lower.md §15.13): a by-ref datum's image copied into the out block's
//! region at the emit (IR, no host call), the stored datum the copy's address; the push ships the region.

use gravity_abi::ctx::out;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, IntCond, Type, Value};

/// A by-reference output column at the emit: the datum word, its validity, the type's `typlen` (-1 varlena, > 0 fixed).
#[derive(Clone, Copy)]
pub struct ByRef {
    pub value: Value,
    pub valid: Value,
    pub typlen: i16,
}

/// `VARSIZE_ANY` of the image at `p` in IR (1-byte header, the 18-byte on-disk toast pointer, 4-byte header), or `typlen`.
pub fn image_len(b: &mut FunctionBuilder<'_>, p: Value, typlen: i16) -> Value {
    assert!(typlen == -1 || typlen > 0, "a by-reference output column's typlen is -1 or fixed, not {typlen}");
    if typlen > 0 {
        return b.iconst(Type::I64, typlen as i64);
    }
    let b0 = b.load_zext_unaligned(Type::I8, p, 0);
    let one = b.iconst(Type::I64, 1);
    let zero = b.iconst(Type::I64, 0);
    let lo = b.int_bin(IntBinOp::And, b0, one);
    let is1 = b.icmp(IntCond::Ne, lo, zero);
    let is_ext = b.icmp(IntCond::Eq, b0, one);
    let ext = b.iconst(Type::I64, 18);
    let len1 = b.int_bin(IntBinOp::Ushr, b0, one);
    let short = b.select(is_ext, ext, len1);
    // the 4-byte header is read only behind the 1-byte test: a short datum may end its page or mapping (siphash.rs's trap)
    let l_four = b.create_block(&[]);
    let l_out = b.create_block(&[Type::I64]);
    b.cond_br(is1, (l_out, &[short]), (l_four, &[]));
    b.switch_to(l_four);
    let w = b.load_zext_unaligned(Type::I32, p, 0);
    let two = b.iconst(Type::I64, 2);
    let mask = b.iconst(Type::I64, 0x3fff_ffff);
    let len4 = b.int_bin(IntBinOp::Ushr, w, two);
    let len4 = b.int_bin(IntBinOp::And, len4, mask);
    b.br(l_out, &[len4]);
    b.switch_to(l_out);
    b.block_params(l_out)[0]
}

/// A by-reference column's bytes at the emit: `len` its image's (`VARSIZE_ANY`, or `typlen`), `size` the region bytes the
/// image takes (`len` 8-aligned; 0 for a NULL).
#[derive(Clone, Copy)]
pub struct Size {
    pub len: Value,
    pub size: Value,
}

/// Per column its `Size`, and the region bytes of all of them.
pub fn sizes(b: &mut FunctionBuilder<'_>, cols: &[ByRef]) -> (Vec<Size>, Value) {
    let zero = b.iconst(Type::I64, 0);
    let seven = b.iconst(Type::I64, 7);
    let nseven = b.iconst(Type::I64, !7);
    let mut total = zero;
    let mut per = Vec::with_capacity(cols.len()); // alloc: Program
    for c in cols {
        let p = b.conv(ConvOp::IntToPtr, Type::Ptr, c.value);
        let len = image_len(b, p, c.typlen);
        let up = b.int_bin(IntBinOp::Add, len, seven);
        let aligned = b.int_bin(IntBinOp::And, up, nseven);
        let size = b.select(c.valid, aligned, zero);
        total = b.int_bin(IntBinOp::Add, total, size);
        per.push(Size { len, size });
    }
    (per, total)
}

/// `total` bytes fit behind the rows already in the block.
pub fn fits(b: &mut FunctionBuilder<'_>, o: Value, total: Value) -> Value {
    let cap = b.load(Type::I64, o, out::REGION_CAP as i32);
    let len = b.load(Type::I64, o, out::REGION_LEN as i32);
    let free = b.int_bin(IntBinOp::Sub, cap, len);
    b.icmp(IntCond::Sle, total, free)
}

/// `total` bytes exceed the whole region: the sink regrows it to hold the row (D645, `gravity_abi::program::out_grow`).
pub fn oversize(b: &mut FunctionBuilder<'_>, o: Value, total: Value) -> Value {
    let cap = b.load(Type::I64, o, out::REGION_CAP as i32);
    b.icmp(IntCond::Sgt, total, cap)
}

/// Column `k` at `vp` / `np` (as `store_col`): the image copied to the region's end (`copy_image`, `copy_fixed`), the datum
/// the copy's address, the null byte; a NULL stores 0. `fits` was checked.
pub fn store(b: &mut FunctionBuilder<'_>, o: Value, vp: Value, np: Value, k: i32, c: ByRef, s: Size) {
    let t = b.bconst(true);
    let nv = b.int_bin(IntBinOp::Xor, c.valid, t);
    let byte = b.conv(ConvOp::Zext, Type::I8, nv);
    b.store(np, k, byte);
    let l_copy = b.create_block(&[]);
    let l_done = b.create_block(&[Type::I64]);
    let zero = b.iconst(Type::I64, 0);
    b.cond_br(c.valid, (l_copy, &[]), (l_done, &[zero]));
    b.switch_to(l_copy);
    let region = b.load(Type::Ptr, o, out::REGION as i32);
    let len = b.load(Type::I64, o, out::REGION_LEN as i32);
    let dst = b.ptr_add(region, len, 1, 0);
    let src = b.conv(ConvOp::IntToPtr, Type::Ptr, c.value);
    let new_len = b.int_bin(IntBinOp::Add, len, s.size);
    b.store(o, out::REGION_LEN as i32, new_len);
    match c.typlen {
        n if n > 0 => copy_fixed(b, dst, 0, src, n),
        _ => copy_image(b, dst, src, s.len),
    }
    let dst_i = b.conv(ConvOp::PtrToInt, Type::I64, dst);
    b.br(l_done, &[dst_i]);
    b.switch_to(l_done);
    let datum = b.block_params(l_done)[0];
    b.store(vp, k * 8, datum);
}

/// The `len` bytes (> 0) of the image at `src` copied to `dst`, which holds `len` 8-aligned: its whole 8-byte words, then
/// its last `len & 7` bytes as one word, zero above them (`tail`). No byte past the image is read: an image need not be
/// 8-aligned (a 1-byte-header varlena), and one that ends a heap page in the pool's last buffer ends its mapping (D3661).
/// On return the builder is in a fresh block.
pub fn copy_image(b: &mut FunctionBuilder<'_>, dst: Value, src: Value, len: Value) {
    let (zero, seven, eight) = (b.iconst(Type::I64, 0), b.iconst(Type::I64, 7), b.iconst(Type::I64, 8));
    let not7 = b.iconst(Type::I64, !7);
    let whole = b.int_bin(IntBinOp::And, len, not7);
    let k = b.int_bin(IntBinOp::And, len, seven);
    let l_loop = b.create_block(&[Type::I64]);
    let (l_rest, l_tail, l_done) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
    let any = b.icmp(IntCond::Ne, whole, zero);
    b.cond_br(any, (l_loop, &[zero]), (l_rest, &[]));
    b.switch_to(l_loop);
    b.mark_loop_header(l_loop);
    let i = b.block_params(l_loop)[0];
    let s = b.ptr_add(src, i, 1, 0);
    let d = b.ptr_add(dst, i, 1, 0);
    let word = b.load_unaligned(Type::I64, s, 0);
    b.store(d, 0, word);
    let next = b.int_bin(IntBinOp::Add, i, eight);
    let more = b.icmp(IntCond::Slt, next, whole);
    b.cond_br(more, (l_loop, &[next]), (l_rest, &[]));
    b.switch_to(l_rest);
    let some = b.icmp(IntCond::Ne, k, zero);
    b.cond_br(some, (l_tail, &[]), (l_done, &[]));
    b.switch_to(l_tail);
    let q = b.ptr_add(src, whole, 1, 0);
    let w = tail(b, q, 0, k);
    let d = b.ptr_add(dst, whole, 1, 0);
    b.store(d, 0, w);
    b.br(l_done, &[]);
    b.switch_to(l_done);
}

/// `copy_image` of a fixed-width image (`typlen` > 0) to `dst + off`, straight-line.
pub fn copy_fixed(b: &mut FunctionBuilder<'_>, dst: Value, off: i32, src: Value, typlen: i16) {
    let (words, k) = (typlen as i32 / 8, typlen as i64 % 8);
    for w in 0..words {
        let v = b.load_unaligned(Type::I64, src, 8 * w);
        b.store(dst, off + 8 * w, v);
    }
    if k != 0 {
        let kv = b.iconst(Type::I64, k);
        let v = tail(b, src, 8 * words, kv);
        b.store(dst, off + 8 * words, v);
    }
}

/// The `k` bytes at `p + off` (1 <= k <= 7, all of them the image's) as one little-endian word, zero above them, read
/// inside the 4 KiB pages of the first and the last of them only: the 8 bytes ending at the last byte when they share its
/// page (shifted down), else the 8 bytes from the first (the last byte is then among its page's first 7, so they end on
/// that page), masked.
fn tail(b: &mut FunctionBuilder<'_>, p: Value, off: i32, k: Value) -> Value {
    let c = |b: &mut FunctionBuilder<'_>, v: i64| b.iconst(Type::I64, v);
    let (one, three, seven, eight, page, zero, ones) = (c(b, 1), c(b, 3), c(b, 7), c(b, 8), c(b, 4095), c(b, 0), c(b, -1));
    let qi = b.conv(ConvOp::PtrToInt, Type::I64, p);
    let qi = match off {
        0 => qi,
        _ => {
            let o = c(b, off as i64);
            b.int_bin(IntBinOp::Add, qi, o)
        }
    };
    let last = b.int_bin(IntBinOp::Add, qi, k);
    let last = b.int_bin(IntBinOp::Sub, last, one);
    let in_page = b.int_bin(IntBinOp::And, last, page);
    let back = b.icmp(IntCond::Uge, in_page, seven);
    let start = b.int_bin(IntBinOp::Sub, last, seven);
    let at = b.select(back, start, qi);
    let at = b.conv(ConvOp::IntToPtr, Type::Ptr, at);
    let raw = b.load_unaligned(Type::I64, at, 0);
    let pad = b.int_bin(IntBinOp::Sub, eight, k);
    let pad_bits = b.int_bin(IntBinOp::Shl, pad, three);
    let sh = b.select(back, pad_bits, zero);
    let v = b.int_bin(IntBinOp::Ushr, raw, sh);
    let mask = b.int_bin(IntBinOp::Ushr, ones, pad_bits);
    b.int_bin(IntBinOp::And, v, mask)
}
