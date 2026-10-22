//! Road `text.memmem`'s case maps (M2-INVENTORY I24, T3; D2858): `upper(text)` and `lower(text)` written straight into
//! the row's result region. Under a `ctype_is_c` collation PG maps ASCII letters byte-wise and leaves every other byte
//! (`asc_toupper` / `asc_tolower`); under any other collation whose map of every ASCII byte is that same map (probed once
//! through PG's own function: a Turkish locale's `i` / `I` is not), an ASCII value is mapped the same way and a non-ASCII
//! one takes PG's call. A full region takes PG's call too. Also `memeq`, the literal compare text.memmem and jsonb.inplace share. Budget 140
//! lines. Owner M2-FRAME.
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, IntCond, Type, Value};
use mcx::Mcx;

use crate::step::text_like::{Claim, Fold, Test};
use crate::frame::env::Env;

/// `upper(text)`.
pub(crate) const F_UPPER: u32 = 871;

/// The claim of `upper` / `lower` (`f`) under collation `coll`: every value (`Upper` / `Lower`), ASCII values only
/// (`UpperAscii` / `LowerAscii`), or none.
pub(crate) fn mode(coll: u32, f: u32, mcx: Mcx<'_>) -> Option<Test> {
    if f == F_LOWER {
        return Some(match fold_mode(coll, mcx)? { Fold::All => Test::Lower, Fold::Ascii => Test::LowerAscii, Fold::No => return None });
    }
    let facts = gravity_fn::typesem::collation::Facts::of(coll).ok()?;
    if facts.ctype_is_c {
        return Some(Test::Upper);
    }
    crate::step::raw::upper_keeps_ascii(coll, mcx)?.then_some(Test::UpperAscii)
}

/// `lower` (870).
pub(crate) const F_LOWER: u32 = 870;

/// How PG's `lower` maps a value under collation `coll` when it is byte-wise `asc_tolower`: every value (`Fold::All`, a
/// `ctype_is_c` collation), ASCII values only (`Fold::Ascii`: its lower of every ASCII byte is that map, probed once
/// through PG's own function; a Turkish locale's `I` is not), or not at all (`None`).
pub(crate) fn fold_mode(coll: u32, mcx: Mcx<'_>) -> Option<Fold> {
    if gravity_fn::typesem::collation::Facts::of(coll).ok()?.ctype_is_c {
        return Some(Fold::All);
    }
    crate::step::raw::lower_keeps_ascii(coll, mcx)?.then_some(Fold::Ascii)
}

/// `fold_mode` for PG's `upper` (`asc_toupper`): every value under a `ctype_is_c` collation, ASCII values only under one
/// whose upper of every ASCII byte is that map (probed as `fold_mode` probes `lower`), or not at all.
pub(crate) fn upper_mode(coll: u32, mcx: Mcx<'_>) -> Option<Fold> {
    if gravity_fn::typesem::collation::Facts::of(coll).ok()?.ctype_is_c {
        return Some(Fold::All);
    }
    crate::step::raw::upper_keeps_ascii(coll, mcx)?.then_some(Fold::Ascii)
}

/// `asc_tolower` of `[src, src + n)` to `[dst, dst + n)`: words, then the tail bytes.
pub(crate) fn lower_into(b: &mut FunctionBuilder<'_>, src: Value, dst: Value, n: Value) {
    let zero = b.iconst(Type::I64, 0);
    let i = map(b, src, dst, zero, n, 8, true);
    map(b, src, dst, i, n, 1, true);
}

/// `[p, p + len)` is ASCII: the words ORed, then the tail bytes, tested for a high bit.
pub(crate) fn ascii(b: &mut FunctionBuilder<'_>, p: Value, len: Value) -> Value {
    let [zero, seven, high] = [0, !7, 0x8080_8080_8080_8080u64 as i64].map(|k| b.iconst(Type::I64, k));
    let tail = b.int_bin(IntBinOp::And, len, seven);
    let (words, bytes) = (or_loads(b, p, zero, len, 8), or_loads(b, p, tail, len, 1));
    let all = b.int_bin(IntBinOp::Or, words, bytes);
    let hi = b.int_bin(IntBinOp::And, all, high);
    b.icmp(IntCond::Eq, hi, zero)
}

/// Loads of `w` bytes at `p + i`, `i` from `from` by `w` while `i + w <= to`, ORed together (an I64).
fn or_loads(b: &mut FunctionBuilder<'_>, p: Value, from: Value, to: Value, w: i64) -> Value {
    let (zero, step) = (b.iconst(Type::I64, 0), b.iconst(Type::I64, w));
    let (head, body, out) = (b.create_block(&[Type::I64, Type::I64]), b.create_block(&[]), b.create_block(&[Type::I64]));
    b.br(head, &[from, zero]);
    (b.switch_to(head), b.mark_loop_header(head));
    let [i, acc] = [b.block_params(head)[0], b.block_params(head)[1]];
    let end = b.int_bin(IntBinOp::Add, i, step);
    let more = b.icmp(IntCond::Sle, end, to);
    b.cond_br(more, (body, &[]), (out, &[acc]));
    b.switch_to(body);
    let q = b.ptr_add(p, i, 1, 0);
    let v = if w == 8 { b.load_unaligned(Type::I64, q, 0) } else { b.load_zext(Type::I8, q, 0) };
    let acc = b.int_bin(IntBinOp::Or, acc, v);
    b.br(head, &[end, acc]);
    b.switch_to(out);
    b.block_params(out)[0]
}

/// The mapped bytes of `[src + i, src + to)` to `dst + i`, `w` bytes at a time while `i + w <= to`: a letter `a`..`z`
/// loses bit 5 (`asc_toupper`; `A`..`Z` gains it when `lower`: `asc_tolower`), every other byte (a non-ASCII one too) is
/// copied. Returns where it stopped.
fn map(b: &mut FunctionBuilder<'_>, src: Value, dst: Value, from: Value, to: Value, w: i64, lower: bool) -> Value {
    let k = |b: &mut FunctionBuilder<'_>, byte: u8| b.iconst(Type::I64, (u64::from_le_bytes([byte; 8]) >> (64 - 8 * w as u32)) as i64);
    let (a, z) = if lower { (b'A', b'Z') } else { (b'a', b'z') };
    let (step, low7, ge_a, gt_z, high) = (b.iconst(Type::I64, w), k(b, 0x7f), k(b, 0x80 - a), k(b, 0x80 - z - 1), k(b, 0x80));
    let (ty, head, body, out) = (if w == 8 { Type::I64 } else { Type::I8 }, b.create_block(&[Type::I64]), b.create_block(&[]), b.create_block(&[Type::I64]));
    b.br(head, &[from]);
    (b.switch_to(head), b.mark_loop_header(head));
    let i = b.block_params(head)[0];
    let end = b.int_bin(IntBinOp::Add, i, step);
    let more = b.icmp(IntCond::Sle, end, to);
    b.cond_br(more, (body, &[]), (out, &[i]));
    b.switch_to(body);
    let (s, d) = (b.ptr_add(src, i, 1, 0), b.ptr_add(dst, i, 1, 0));
    let v = if w == 8 { b.load_unaligned(Type::I64, s, 0) } else { b.load_zext(Type::I8, s, 0) };
    // per byte, no carry across bytes (each sum stays under 0x100): bit 7 of `h + ge_a` is `h >= 'a'`, of `h + gt_z` `h > 'z'`
    let h = b.int_bin(IntBinOp::And, v, low7);
    let (a, z) = (b.int_bin(IntBinOp::Add, h, ge_a), b.int_bin(IntBinOp::Add, h, gt_z));
    let nz = b.int_un(gravity_ir::IntUnOp::Not, z);
    let nv = b.int_un(gravity_ir::IntUnOp::Not, v);
    let m = b.int_bin(IntBinOp::And, a, nz);
    let m = b.int_bin(IntBinOp::And, m, nv);
    let m = b.int_bin(IntBinOp::And, m, high);
    let two = b.iconst(Type::I64, 2);
    let bit5 = b.int_bin(IntBinOp::Ushr, m, two);
    let r = b.int_bin(IntBinOp::Xor, v, bit5);
    let r = if w == 8 { r } else { b.conv(ConvOp::Trunc, ty, r) };
    if w == 8 { b.store_unaligned(d, 0, r) } else { b.store(d, 0, r) }
    b.br(head, &[end]);
    b.switch_to(out);
    b.block_params(out)[0]
}

/// `upper` / `lower` (`c.test`) of the inline image `[p, p + len)` into the row's result region: `(hot, datum word)`;
/// not hot for a non-ASCII value under `UpperAscii` / `LowerAscii`, a program with no region, or a full region.
pub(crate) fn case_map(b: &mut FunctionBuilder<'_>, env: &mut Env, p: Value, len: Value, null: Value, c: &Claim) -> (Value, Value) {
    let (zero, f) = (b.iconst(Type::I64, 0), b.bconst(false));
    let Some(slot) = env.hir.refs(b, 0).1 else { return (f, zero) };
    let lower = matches!(c.test, Test::Lower | Test::LowerAscii);
    let ascii = if matches!(c.test, Test::UpperAscii | Test::LowerAscii) { ascii(b, p, len) } else { b.bconst(true) };
    let (cur, end) = (b.load(Type::I64, slot, 0), b.load(Type::I64, slot, 8));
    let (seven, four) = (b.iconst(Type::I64, 7), b.iconst(Type::I64, 4));
    let up = b.int_bin(IntBinOp::Add, cur, seven);
    let at = { let m = b.iconst(Type::I64, !7); b.int_bin(IntBinOp::And, up, m) };
    let size = b.int_bin(IntBinOp::Add, len, four);
    let top = b.int_bin(IntBinOp::Add, at, size);
    let fits = b.icmp(IntCond::Ule, top, end);
    let ok = b.int_bin(IntBinOp::And, ascii, fits);
    let t = b.bconst(true);
    let live = b.int_bin(IntBinOp::Xor, null, t);
    let go = b.int_bin(IntBinOp::And, ok, live);
    let (copy, out) = (b.create_block(&[]), b.create_block(&[Type::I64]));
    b.cond_br(go, (copy, &[]), (out, &[zero]));
    b.switch_to(copy);
    let dp = b.conv(ConvOp::IntToPtr, Type::Ptr, at);
    let two = b.iconst(Type::I64, 2);
    let hdr = b.int_bin(IntBinOp::Shl, size, two);
    let hdr = b.conv(ConvOp::Trunc, Type::I32, hdr);
    b.store(dp, 0, hdr);
    let dst = b.ptr_add(dp, four, 1, 0);
    let i = map(b, p, dst, zero, len, 8, lower);
    map(b, p, dst, i, len, 1, lower);
    b.store(slot, 0, top);
    b.br(out, &[at]);
    b.switch_to(out);
    let hot = b.int_bin(IntBinOp::Or, ok, null);
    (hot, b.block_params(out)[0])
}

/// The `lit.len()` bytes at `p` equal `lit`: whole words, then the tail as one overlapping word, or 4 / 2 / 1-byte reads
/// when the literal is shorter than a word (never a byte past it).
pub(crate) fn memeq(b: &mut FunctionBuilder<'_>, p: Value, lit: &[u8]) -> Value {
    let mut acc = b.bconst(true);
    let mut cmp = |b: &mut FunctionBuilder<'_>, ty: Type, at: usize, w: usize| {
        let mut k = [0u8; 8];
        k[..w].copy_from_slice(&lit[at..at + w]);
        let v = if ty == Type::I64 { b.load_unaligned(ty, p, at as i32) } else { b.load_zext_unaligned(ty, p, at as i32) };
        let want = b.iconst(Type::I64, i64::from_le_bytes(k));
        let e = b.icmp(IntCond::Eq, v, want);
        acc = b.int_bin(IntBinOp::And, acc, e);
    };
    let n = lit.len();
    (0..n / 8).for_each(|i| cmp(b, Type::I64, 8 * i, 8));
    if n > 8 && n % 8 != 0 {
        cmp(b, Type::I64, n - 8, 8);
    } else if n < 8 {
        let mut at = 0;
        for (w, ty) in [(4, Type::I32), (2, Type::I16), (1, Type::I8)].into_iter().filter(|&(w, _)| n & w != 0) {
            cmp(b, ty, at, w);
            at += w;
        }
    }
    acc
}

#[cfg(test)]
mod tests {
    use gravity_interp::Interp;
    use gravity_ir::builder::ProgramBuilder;
    use gravity_ir::Type;

    use crate::step::text_like::Test;

    /// `map` by words then bytes over `src`: PG's `asc_toupper` (a..z mapped, every other byte, non-ASCII too, copied), or
    /// `asc_tolower` when `lower`.
    fn mapped(src: &[u8], lower: bool) -> Vec<u8> {
        let mut pb = ProgramBuilder::new();
        let f = pb.declare_func("m", &[Type::I64, Type::I64, Type::I64], &[Type::I64]);
        let mut b = pb.define(f);
        let [s, d, n] = [0, 1, 2].map(|i| b.params()[i]);
        let (s, d) = (b.conv(gravity_ir::ConvOp::IntToPtr, Type::Ptr, s), b.conv(gravity_ir::ConvOp::IntToPtr, Type::Ptr, d));
        let zero = b.iconst(Type::I64, 0);
        let i = super::map(&mut b, s, d, zero, n, 8, lower);
        let j = super::map(&mut b, s, d, i, n, 1, lower);
        b.ret(&[j]);
        let prog = pb.finish();
        let mut dst = vec![0xAAu8; src.len() + 8];
        let r = Interp::new(&prog).run(f, &[src.as_ptr() as u64, dst.as_mut_ptr() as u64, src.len() as u64]).expect("runs");
        assert_eq!(r[0], src.len() as u64);
        assert!(dst[src.len()..].iter().all(|&x| x == 0xAA), "nothing past the value is written");
        dst.truncate(src.len());
        dst
    }

    #[test]
    fn upper_maps_ascii_letters_only() {
        let all: Vec<u8> = (0u8..=255).collect();
        for s in [&b""[..], b"a", b"abc", b"hello, World!", b"`az{@AZ[", "straße naïve ıi".as_bytes(), &all, b"exactly8", b"sixteen-bytes-xy"] {
            assert_eq!(mapped(s, false), s.to_ascii_uppercase(), "{s:?}");
        }
    }

    /// PS4-03 / PS4-06: the lower map is `asc_tolower`: A..Z only, every other byte (`[`, `@`, non-ASCII) copied.
    #[test]
    fn lower_maps_ascii_letters_only() {
        let all: Vec<u8> = (0u8..=255).collect();
        for s in [&b""[..], b"A", b"ABC", b"Hello, World!", b"`az{@AZ[", "STRASSE NAÏVE İI ẞ".as_bytes(), &all, b"EXACTLY8", b"SIXTEEN-BYTES-XY"] {
            assert_eq!(mapped(s, true), s.to_ascii_lowercase(), "{s:?}");
        }
    }

    /// A C / POSIX collation maps every value; the claim of any other collation is probed through PG's `upper`.
    #[test]
    fn c_collations_map_every_value() {
        let cx = mcx::MemoryContext::new("test");
        let m = cx.mcx();
        assert_eq!(super::mode(950, super::F_UPPER, m), Some(Test::Upper));
        assert_eq!(super::mode(951, super::F_UPPER, m), Some(Test::Upper));
        assert_eq!(super::mode(950, super::F_LOWER, m), Some(Test::Lower), "D2858");
        assert_eq!(super::mode(951, super::F_LOWER, m), Some(Test::Lower));
        assert_eq!(super::fold_mode(950, m), Some(crate::step::text_like::Fold::All));
        assert_eq!(super::fold_mode(951, m), Some(crate::step::text_like::Fold::All));
    }
}
