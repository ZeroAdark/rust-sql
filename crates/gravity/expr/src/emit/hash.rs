//! The IR half of the one key hash (lowerv4 M0; hash-contract.md §3.5): bit for bit `gravity_abi::hash`, pinned by
//! `expr/tests/hash_differential.rs` on the interpreter, TPDE and wasm. Emitted code takes a key's hash only through this
//! file: an image, its `KeyHash` word, the route and full hashes, `home` / `part` / `sub` / `split` / `stored`, and image
//! equality. No caller names a mixing constant or a bit position, so a tuned mix (abi/src/hash.rs, reopen class Tuning)
//! changes these two files and nothing else. The constants come from abi's; the formula is mirrored here.
use gravity_abi::breaker::{flag_at, flag_words};
use gravity_abi::hash::{IMAGE_SHIFT, PHI64, SPLIT_BITS, WORD_FOLD};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, IntCond, Type, Value};

/// One key's part of an image: its canonical `I64` words (one; two for an Fp128 or 16-byte image), and for a nullable key
/// its null flag (`Bool`). A null key's words become zero and its bit goes in the image's flags words (§3.1).
#[derive(Clone, Copy, Debug)]
pub struct KeyPart<'a> {
    pub words: &'a [Value],
    pub null: Option<Value>,
}

/// A key image as `I64` words, in key order, the flags words last when any key is nullable ([`emit_image`]).
#[derive(Clone, Debug, Default)]
pub struct ImageValues {
    pub words: Vec<Value>,
}

/// A bit count: known at lowering, or read at run time (a table's capacity bits, a spilled life's depth). Every count is
/// what abi's `KeyHash` methods take: capacity bits ≤ `HOME_BITS`, consumed route bits ≤ `ROUTE_BITS`.
#[derive(Clone, Copy, Debug)]
pub enum Bits {
    Const(u32),
    Dyn(Value),
}

impl Bits {
    /// The route bits a table's rows consumed (abi's `Depth`).
    pub fn of(at: gravity_abi::hash::Depth) -> Bits {
        Bits::Const(at.used())
    }

    fn value(self, b: &mut FunctionBuilder<'_>) -> Value {
        match self {
            Bits::Const(n) => b.iconst(Type::I64, n as i64),
            Bits::Dyn(v) => v,
        }
    }
}

fn k(b: &mut FunctionBuilder<'_>, v: u64) -> Value {
    b.iconst(Type::I64, v as i64)
}

/// The image of `parts`: every key's words in order; when any key is nullable, a null key's words are zero and the flags
/// words follow, one per 64 keys (`breaker::flag_words`), a null key k setting bit k % 64 of word k / 64 (`flag_at`;
/// CAP-28: one word whose shift wrapped past 64 made key k and key k + 64 share a bit). A flags word no nullable key
/// reaches is a zero word.
pub fn emit_image(b: &mut FunctionBuilder<'_>, parts: &[KeyPart<'_>]) -> ImageValues {
    emit_image_then(b, parts, |_| None)
}

/// [`emit_image`], then `tail`'s word, if any, after the flags words (built after them: a parted record's part ordinal).
pub fn emit_image_then(b: &mut FunctionBuilder<'_>, parts: &[KeyPart<'_>], tail: impl FnOnce(&mut FunctionBuilder<'_>) -> Option<Value>) -> ImageValues {
    let nflags = if parts.iter().any(|p| p.null.is_some()) { flag_words(parts.len()) } else { 0 };
    let mut words = Vec::with_capacity(parts.iter().map(|p| p.words.len()).sum::<usize>() + nflags + 1);
    let mut flags: Vec<Option<Value>> = vec![None; nflags]; // alloc: Program
    let zero = k(b, 0);
    for (i, p) in parts.iter().enumerate() {
        match p.null {
            None => words.extend_from_slice(p.words),
            Some(n) => {
                words.extend(p.words.iter().map(|&w| b.select(n, zero, w)));
                let (word, at) = flag_at(i);
                let bit = b.conv(ConvOp::Zext, Type::I64, n);
                let at = k(b, at as u64);
                let bit = b.int_bin(IntBinOp::Shl, bit, at);
                let f = &mut flags[word];
                *f = Some(match *f {
                    None => bit,
                    Some(f) => b.int_bin(IntBinOp::Or, f, bit),
                });
            }
        }
    }
    let tail = tail(b);
    words.extend(flags.into_iter().map(|f| f.unwrap_or(zero)).chain(tail));
    ImageValues { words }
}

/// The image's `KeyHash` word (`KeyHash::of(image).to_abi()`), an `I64`.
pub fn emit_key_hash(b: &mut FunctionBuilder<'_>, image: &ImageValues) -> Value {
    hash_words(b, &image.words)
}

/// `emit_key_hash` of the image whose words the caller holds in place (no `ImageValues` built for it).
pub fn emit_key_hash_of(b: &mut FunctionBuilder<'_>, words: &[Value]) -> Value {
    hash_words(b, words)
}

/// Perf: a loop carries it as a block parameter; the backend rematerializes a 64-bit constant at each use (four
/// instructions on AArch64).
pub fn emit_hash_mix(b: &mut FunctionBuilder<'_>) -> Value {
    k(b, PHI64)
}

pub fn emit_key_hash_with(b: &mut FunctionBuilder<'_>, words: &[Value], mix: Value) -> Value {
    if let [w] = words {
        return word_hash(b, *w, mix);
    }
    let shift = k(b, IMAGE_SHIFT as u64);
    let n = k(b, words.len() as u64);
    let h = b.int_bin(IntBinOp::Xor, mix, n);
    image_hash(b, words, h, mix, shift)
}

fn hash_words(b: &mut FunctionBuilder<'_>, words: &[Value]) -> Value {
    let phi = k(b, PHI64);
    if let [w] = words {
        return word_hash(b, *w, phi);
    }
    let shift = k(b, IMAGE_SHIFT as u64);
    let h = k(b, PHI64 ^ words.len() as u64);
    image_hash(b, words, h, phi, shift)
}

fn word_hash(b: &mut FunctionBuilder<'_>, w: Value, phi: Value) -> Value {
    let (sa, sb) = (k(b, WORD_FOLD.0 as u64), k(b, WORD_FOLD.1 as u64));
    let ta = b.int_bin(IntBinOp::Ushr, w, sa);
    let tb = b.int_bin(IntBinOp::Ushr, w, sb);
    let t = b.int_bin(IntBinOp::Xor, ta, tb);
    let x = b.int_bin(IntBinOp::Xor, w, t);
    b.int_bin(IntBinOp::Mul, x, phi)
}

fn image_hash(b: &mut FunctionBuilder<'_>, words: &[Value], mut h: Value, phi: Value, shift: Value) -> Value {
    for &w in words {
        let x = b.int_bin(IntBinOp::Xor, h, w);
        h = b.int_bin(IntBinOp::Mul, x, phi);
        let s = b.int_bin(IntBinOp::Ushr, h, shift);
        h = b.int_bin(IntBinOp::Xor, h, s);
    }
    b.int_bin(IntBinOp::Mul, h, phi)
}

/// `(route, full)`: the hash of the image's first `route_words` words (the partition keys) and of the whole image, from one
/// helper (`route_and_full`), so a row's route and home never take different keys (D154, D171).
pub fn emit_route_and_full(b: &mut FunctionBuilder<'_>, image: &ImageValues, route_words: usize) -> (Value, Value) {
    let full = hash_words(b, &image.words);
    if route_words >= image.words.len() {
        return (full, full);
    }
    (hash_words(b, &image.words[..route_words]), full)
}

/// `x >> (64 - n)`, and 0 when `n` is 0 (the IR's shift count is taken modulo 64).
fn top(b: &mut FunctionBuilder<'_>, x: Value, n: Bits) -> Value {
    match n {
        Bits::Const(0) => k(b, 0),
        Bits::Const(n) => {
            let s = k(b, 64 - n as u64);
            b.int_bin(IntBinOp::Ushr, x, s)
        }
        Bits::Dyn(n) => {
            let sixty_four = k(b, 64);
            let s = b.int_bin(IntBinOp::Sub, sixty_four, n);
            let r = b.int_bin(IntBinOp::Ushr, x, s);
            let zero = k(b, 0);
            let none = b.icmp(IntCond::Eq, n, zero);
            b.select(none, zero, r)
        }
    }
}

fn shl(b: &mut FunctionBuilder<'_>, x: Value, n: Bits) -> Value {
    match n {
        Bits::Const(0) => x,
        n => {
            let s = n.value(b);
            b.int_bin(IntBinOp::Shl, x, s)
        }
    }
}

/// `KeyHash::home(cap_bits, at)`: the `cap_bits` bits just below the `used` route bits the table's rows consumed.
pub fn emit_home(b: &mut FunctionBuilder<'_>, h: Value, cap_bits: Bits, used: Bits) -> Value {
    let below = shl(b, h, used);
    top(b, below, cap_bits)
}

/// The shift [`emit_home_at`] takes for `cap_bits` capacity bits read at run time, at least 1 (D3326: taken once a
/// window, so a row's home is two shifts with no zero test).
pub fn emit_home_shift(b: &mut FunctionBuilder<'_>, cap_bits: Value) -> Value {
    let sixty_four = k(b, 64);
    b.int_bin(IntBinOp::Sub, sixty_four, cap_bits)
}

/// `KeyHash::home(cap_bits, used)` over `shift` = [`emit_home_shift`] of `cap_bits` (at least 1 bit).
pub fn emit_home_at(b: &mut FunctionBuilder<'_>, h: Value, shift: Value, used: Bits) -> Value {
    let below = shl(b, h, used);
    b.int_bin(IntBinOp::Ushr, below, shift)
}

/// `KeyHash::part(pbits)`: the top `pbits` bits.
pub fn emit_part(b: &mut FunctionBuilder<'_>, h: Value, pbits: Bits) -> Value {
    top(b, h, pbits)
}

/// `KeyHash::sub(pbits, depth)`: the `SPLIT_BITS · depth` bits after the partition bits.
pub fn emit_sub(b: &mut FunctionBuilder<'_>, h: Value, pbits: Bits, depth: Bits) -> Value {
    let n = match depth {
        Bits::Const(d) => Bits::Const(SPLIT_BITS * d),
        Bits::Dyn(d) => {
            let three = k(b, SPLIT_BITS as u64);
            Bits::Dyn(b.int_bin(IntBinOp::Mul, d, three))
        }
    };
    let below = shl(b, h, pbits);
    top(b, below, n)
}

/// `KeyHash::split(at)`: the child stream of a table at `used` consumed bits when it splits once more.
pub fn emit_split(b: &mut FunctionBuilder<'_>, h: Value, used: Bits) -> Value {
    let below = shl(b, h, used);
    top(b, below, Bits::Const(SPLIT_BITS))
}

/// `KeyHash::stored()`: the 32-bit tag (zero-extended in an `I64`), a hint only, never an equality test.
pub fn emit_stored(b: &mut FunctionBuilder<'_>, h: Value) -> Value {
    let s = k(b, 32);
    let hi = b.int_bin(IntBinOp::Ushr, h, s);
    let x = b.int_bin(IntBinOp::Xor, h, hi);
    let mask = k(b, u32::MAX as u64);
    b.int_bin(IntBinOp::And, x, mask)
}

/// Equality of two images (`Bool`): every word, exact words and both Fp128 words alike. Never a tag, a fold or a partial
/// image; images of different lengths are never equal.
pub fn emit_image_eq(b: &mut FunctionBuilder<'_>, x: &ImageValues, y: &ImageValues) -> Value {
    if x.words.len() != y.words.len() {
        return b.bconst(false);
    }
    let mut eq = b.bconst(true);
    for (&p, &q) in x.words.iter().zip(&y.words) {
        let e = b.icmp(IntCond::Eq, p, q);
        eq = b.int_bin(IntBinOp::And, eq, e);
    }
    eq
}

/// Equality of `x` with the image stored at `base + off` (its words at 8-byte steps), as [`emit_image_eq`].
pub fn emit_image_eq_at(b: &mut FunctionBuilder<'_>, x: &ImageValues, base: Value, off: i32) -> Value {
    let stored = ImageValues { words: (0..x.words.len()).map(|i| b.load(Type::I64, base, off + 8 * i as i32)).collect() };
    emit_image_eq(b, x, &stored)
}
