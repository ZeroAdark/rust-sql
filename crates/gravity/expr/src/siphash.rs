//! The per-row text key over a by-reference datum (lower.md §15.11; the ladder's rung 7 face, §6.11.2
//! need 1): `gravity_dict::fp::siphash128` — the identity every `Fp128` table holds for a dictionary
//! entry — as IR over the datum's bytes, so a heap row's text key meets a code lane's. An IR pattern,
//! not a host (abi-v2 §3: no host per row); the keys are the process seed read at run time by the
//! caller, never an immediate (the code cache is keyed by content; the seed is per process).
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, IntCond, Type, Value};

/// A by-reference datum's payload: `ptr` / `len` of its bytes; `bad` nonzero for a compressed or external image.
#[derive(Clone, Copy, Debug)]
pub struct Varlena {
    pub ptr: Value,
    pub len: Value,
    pub bad: Value,
}

/// The varlena header at `datum`: a 1-byte or an uncompressed 4-byte header gives the bytes; a compressed
/// inline image or a TOAST pointer (`0x01`) sets `bad` and a zero length (the caller refuses the window).
pub fn varlena(b: &mut FunctionBuilder<'_>, datum: Value) -> Varlena {
    let b0 = b.load(Type::I8, datum, 0);
    let b0 = b.conv(ConvOp::Zext, Type::I64, b0);
    let one = b.iconst(Type::I64, 1);
    let zero = b.iconst(Type::I64, 0);
    let low = b.int_bin(IntBinOp::And, b0, one);
    let is1 = b.icmp(IntCond::Ne, low, zero);
    let sh = b.int_bin(IntBinOp::Ushr, b0, one);
    let m7 = b.iconst(Type::I64, 0x7f);
    let l1 = b.int_bin(IntBinOp::And, sh, m7);
    let len1 = b.int_bin(IntBinOp::Sub, l1, one);
    let bad1b = b.icmp(IntCond::Eq, b0, one);
    let bad1 = b.conv(ConvOp::Zext, Type::I64, bad1b);
    // the 4-byte header is read only behind the 1-byte test: a short datum may end a mapping
    let l_four = b.create_block(&[]);
    let l_out = b.create_block(&[Type::I64, Type::I64, Type::I64]);
    b.cond_br(is1, (l_out, &[one, len1, bad1]), (l_four, &[]));

    b.switch_to(l_four);
    let two = b.iconst(Type::I64, 2);
    let four = b.iconst(Type::I64, 4);
    let w = b.load_unaligned(Type::I32, datum, 0);
    let w = b.conv(ConvOp::Zext, Type::I64, w);
    let l4 = b.int_bin(IntBinOp::Ushr, w, two);
    let len4 = b.int_bin(IntBinOp::Sub, l4, four);
    let c4 = b.int_bin(IntBinOp::And, w, two);
    let bad4b = b.icmp(IntCond::Ne, c4, zero);
    let bad4 = b.conv(ConvOp::Zext, Type::I64, bad4b);
    b.br(l_out, &[four, len4, bad4]);

    b.switch_to(l_out);
    let p = b.block_params(l_out);
    let (off, lenv, bad) = (p[0], p[1], p[2]);
    let isbad = b.icmp(IntCond::Ne, bad, zero);
    let len = b.select(isbad, zero, lenv);
    let ptr = b.ptr_add(datum, off, 1, 0);
    Varlena { ptr, len, bad }
}

fn rotl(b: &mut FunctionBuilder<'_>, x: Value, r: i64) -> Value {
    let k = b.iconst(Type::I64, r);
    b.int_bin(IntBinOp::Rotl, x, k)
}

fn round(b: &mut FunctionBuilder<'_>, v: [Value; 4]) -> [Value; 4] {
    let [v0, v1, v2, v3] = v;
    let v0 = b.int_bin(IntBinOp::Add, v0, v1);
    let v1 = rotl(b, v1, 13);
    let v1 = b.int_bin(IntBinOp::Xor, v1, v0);
    let v0 = rotl(b, v0, 32);
    let v2 = b.int_bin(IntBinOp::Add, v2, v3);
    let v3 = rotl(b, v3, 16);
    let v3 = b.int_bin(IntBinOp::Xor, v3, v2);
    let v0 = b.int_bin(IntBinOp::Add, v0, v3);
    let v3 = rotl(b, v3, 21);
    let v3 = b.int_bin(IntBinOp::Xor, v3, v0);
    let v2 = b.int_bin(IntBinOp::Add, v2, v1);
    let v1 = rotl(b, v1, 17);
    let v1 = b.int_bin(IntBinOp::Xor, v1, v2);
    let v2 = rotl(b, v2, 32);
    [v0, v1, v2, v3]
}

fn rounds(b: &mut FunctionBuilder<'_>, mut v: [Value; 4], n: usize) -> [Value; 4] {
    for _ in 0..n {
        v = round(b, v);
    }
    v
}

/// `m | zext(the w bytes at q + off) << 8 * off` when `r & w`, `m` otherwise: a 4-, 2- or 1-byte piece of the last
/// word's `r` bytes, read only when present (the payload may end its mapping). The pieces go 4, 2, 1, so a piece's offset
/// is the bytes of the larger ones (`r & !(2w - 1)` within 0..8): only `m` is carried through the join.
fn piece(b: &mut FunctionBuilder<'_>, m: Value, q: Value, r: Value, w: i64) -> Value {
    let ty = match w {
        4 => Type::I32,
        2 => Type::I16,
        _ => Type::I8,
    };
    let (zero, wk, above, three) = (b.iconst(Type::I64, 0), b.iconst(Type::I64, w), b.iconst(Type::I64, 7 & !(2 * w - 1)), b.iconst(Type::I64, 3));
    let has = b.int_bin(IntBinOp::And, r, wk);
    let has = b.icmp(IntCond::Ne, has, zero);
    let l_load = b.create_block(&[]);
    let l_join = b.create_block(&[Type::I64]);
    b.cond_br(has, (l_load, &[]), (l_join, &[m]));
    b.switch_to(l_load);
    let off = b.int_bin(IntBinOp::And, r, above);
    let sh = b.int_bin(IntBinOp::Shl, off, three);
    let at = b.ptr_add(q, off, 1, 0);
    let x = b.load_zext_unaligned(ty, at, 0);
    let x = b.int_bin(IntBinOp::Shl, x, sh);
    let m1 = b.int_bin(IntBinOp::Or, m, x);
    b.br(l_join, &[m1]);
    b.switch_to(l_join);
    b.block_params(l_join)[0]
}

/// One message word `m` into the state: `v3 ^= m`, two rounds, `v0 ^= m`.
fn word(b: &mut FunctionBuilder<'_>, v: [Value; 4], m: Value) -> [Value; 4] {
    let v3m = b.int_bin(IntBinOp::Xor, v[3], m);
    let r = rounds(b, [v[0], v[1], v[2], v3m], 2);
    let v0m = b.int_bin(IntBinOp::Xor, r[0], m);
    [v0m, r[1], r[2], r[3]]
}

/// SipHash-2-4-128 of the `len` bytes at `ptr` under `(k0, k1)` as `(lo, hi)`, `gravity_dict::fp::siphash128`'s
/// words: an odd first word (`len & 8`) before a loop over word pairs that carries the four state words and the cursor
/// only (its header marked when the function marks its loops: a backend then knows no other loops; a pair halves the
/// loop edge's cost per byte), the last word's 0-7 bytes as a 4-, a 2- and a 1-byte piece, the rounds straight-line.
/// Emitted from the current block; on return the builder is in a fresh block the callers continue in.
pub fn siphash128(b: &mut FunctionBuilder<'_>, ptr: Value, len: Value, k0: Value, k1: Value) -> (Value, Value) {
    let c = |b: &mut FunctionBuilder<'_>, v: u64| b.iconst(Type::I64, v as i64);
    let x0 = c(b, 0x736f_6d65_7073_6575);
    let x1 = c(b, 0x646f_7261_6e64_6f6d ^ 0xee);
    let x2 = c(b, 0x6c79_6765_6e65_7261);
    let x3 = c(b, 0x7465_6462_7974_6573);
    let v0 = b.int_bin(IntBinOp::Xor, k0, x0);
    let v1 = b.int_bin(IntBinOp::Xor, k1, x1);
    let v2 = b.int_bin(IntBinOp::Xor, k0, x2);
    let v3 = b.int_bin(IntBinOp::Xor, k1, x3);
    let (zero, seven, eight, sixteen, fifty_six) = (c(b, 0), c(b, 7), c(b, 8), c(b, 16), c(b, 56));
    let not7 = c(b, !7);
    let words = b.int_bin(IntBinOp::And, len, not7);
    let end = b.ptr_add(ptr, words, 1, 0);
    let l_first = b.create_block(&[]);
    let l_w = b.create_block(&[Type::I64, Type::I64, Type::I64, Type::I64, Type::Ptr]);
    let l_body = b.create_block(&[]);
    let l_tail = b.create_block(&[]);
    let odd = b.int_bin(IntBinOp::And, len, eight);
    let odd = b.icmp(IntCond::Ne, odd, zero);
    b.cond_br(odd, (l_first, &[]), (l_w, &[v0, v1, v2, v3, ptr]));

    b.switch_to(l_first);
    let m = b.load_unaligned(Type::I64, ptr, 0);
    let w = word(b, [v0, v1, v2, v3], m);
    let q8 = b.ptr_add(ptr, eight, 1, 0);
    b.br(l_w, &[w[0], w[1], w[2], w[3], q8]);

    b.switch_to(l_w);
    if b.marks_loops() {
        b.mark_loop_header(l_w);
    }
    let p = b.block_params(l_w);
    let (v, q) = ([p[0], p[1], p[2], p[3]], p[4]);
    let more = b.icmp(IntCond::Ult, q, end);
    b.cond_br(more, (l_body, &[]), (l_tail, &[]));

    b.switch_to(l_body);
    let m = b.load_unaligned(Type::I64, q, 0);
    let w = word(b, v, m);
    let m = b.load_unaligned(Type::I64, q, 8);
    let w = word(b, w, m);
    let q16 = b.ptr_add(q, sixteen, 1, 0);
    b.br(l_w, &[w[0], w[1], w[2], w[3], q16]);

    // the words' end: `v` and `q` are the loop's last state (l_w dominates the rest)
    b.switch_to(l_tail);
    let rest = b.int_bin(IntBinOp::And, len, seven);
    let m0 = b.int_bin(IntBinOp::Shl, len, fifty_six);
    let mut m = m0;
    for w in [4, 2, 1] {
        m = piece(b, m, q, rest, w);
    }
    let r = word(b, v, m);
    let ee = b.iconst(Type::I64, 0xee);
    let v2e = b.int_bin(IntBinOp::Xor, r[2], ee);
    let r = rounds(b, [r[0], r[1], v2e, r[3]], 4);
    let a = b.int_bin(IntBinOp::Xor, r[0], r[1]);
    let a = b.int_bin(IntBinOp::Xor, a, r[2]);
    let lo = b.int_bin(IntBinOp::Xor, a, r[3]);
    let dd = b.iconst(Type::I64, 0xdd);
    let v1d = b.int_bin(IntBinOp::Xor, r[1], dd);
    let r = rounds(b, [r[0], v1d, r[2], r[3]], 4);
    let a = b.int_bin(IntBinOp::Xor, r[0], r[1]);
    let a = b.int_bin(IntBinOp::Xor, a, r[2]);
    let hi = b.int_bin(IntBinOp::Xor, a, r[3]);
    let l_out = b.create_block(&[Type::I64, Type::I64]);
    b.br(l_out, &[lo, hi]);
    b.switch_to(l_out);
    let p = b.block_params(l_out);
    (p[0], p[1])
}

/// The text key of the by-reference datum at `datum`: `(lo, hi, bad)` — its `Fp128` words and `varlena`'s verdict.
pub fn text_key(b: &mut FunctionBuilder<'_>, datum: Value, k0: Value, k1: Value) -> (Value, Value, Value) {
    let v = varlena(b, datum);
    let (lo, hi) = siphash128(b, v.ptr, v.len, k0, k1);
    (lo, hi, v.bad)
}
