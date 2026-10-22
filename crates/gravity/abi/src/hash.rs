//! The one key hash (lowerv4 M0; docs/gravity/hash-contract.md §3.4, §6 on malisper/gravity-v3-hash 338463a8087).
//!
//! Every table, route, split and tag takes a key's hash as a [`KeyHash`] and reads it only through its methods: `home`,
//! `part`, `sub`, `split`, `stored`. No other file names a mixing constant or a bit position, so swapping the mix touches
//! this file alone. The interface is closed at M0. The mix and the load constants are TUNABLE (kept flexible and
//! optimized if needed; Fibonacci hashing is the first one tried). A change needs the hash-quality pass
//! (`dict/tests/hash_quality.rs`, `abi/tests/hashbuild_mix.rs`, `expr/tests/hash_differential.rs` for the IR half in
//! `expr/src/emit/hash.rs`) plus a before / after on the affected bar cells, and counts as a Tuning reopen.
//!
//! **The mix: Fibonacci hashing** (S0b §6.4: the fastest mix at 1/2 on every measured set).
//! - A one-word image `k` hashes as `k · φ64`; a wider image is folded word by word (`(h ^ w) · φ64`, then
//!   `h ^= h >> IMAGE_SHIFT`), then multiplied once more.
//! - The route (partition, sub-split) reads the TOP bits. A table's home reads the bits just below every route bit its rows
//!   already consumed (the depth rule, carried by [`Depth`]), so a table refilled from one partition stays uniform.
//! - A one-word hash is a bijection (φ64 is odd), and bit j of the product depends only on key bits 0..=j. So keys that
//!   vary only in their top bits keep every varying bit in the top of the product and never share a home (walk 1.0 in
//!   `dict/tests/hash_quality.rs`); S0's "top-bits-only hole" does not occur. Fib's measured structured cases are a
//!   key-range build probed by the keys past it and a few strides (32, 48, 86 400 at some sizes): hit walks up to ~4.7×
//!   uniform at 1/2, never a collapse. The test pins them; the lo/hi finalizer (§3.4) is the reference beside it.
//! - `stored()` is a tag: a hint that filters, never an equality test.
//!
//! **The image** a key hashes is its words in key order (one per key, two for an Fp128 or 16-byte image). When any key is
//! nullable, a null key's words are zero and the flags words follow, one per 64 keys (`breaker::flag_words`): a null key k
//! sets bit k % 64 of flags word k / 64 (`breaker::flag_at`), so no two keys share a bit however many there are (§3.1;
//! CAP-28).
//!
//! **Seeds.** None: fixed constants, as PostgreSQL's `hash_any`. Workers, merge, spill and every backend agree bit for bit.

/// φ · 2^64 rounded to odd: the Fibonacci multiplier. TUNABLE.
pub const PHI64: u64 = 0x9E37_79B9_7F4A_7C15;
/// The per-word xorshift of a multi-word image's fold. TUNABLE.
pub const IMAGE_SHIFT: u32 = 29;
/// The two right shifts that fold a one-word key's top bits into its low half before the multiply ([`KeyHash::word`]).
/// Both at least 32, so the fold is its own inverse. TUNABLE: every `i << s` shape (s = 32..63) takes 0.96-1.0 of the
/// homes random hashes would; one shift of 32 takes 26 % of the homes for `i << 32`.
pub const WORD_FOLD: (u32, u32) = (43, 44);
const _: () = assert!(WORD_FOLD.0 >= 32 && WORD_FOLD.1 >= 32);

const fn word_fold(k: u64) -> u64 {
    (k >> WORD_FOLD.0) ^ (k >> WORD_FOLD.1)
}

/// φ64's inverse mod 2^64 (Newton's iteration from φ64, exact after six steps).
const PHI64_INV: u64 = {
    let mut x = PHI64;
    let mut i = 0;
    while i < 6 {
        x = x.wrapping_mul(2u64.wrapping_sub(PHI64.wrapping_mul(x)));
        i += 1;
    }
    x
};
const _: () = assert!(PHI64.wrapping_mul(PHI64_INV) == 1);

/// The one-word key whose [`KeyHash::word`] is `h` (the multiply undone, then the fold, its own inverse): tests craft keys
/// of a chosen hash through it.
pub const fn word_preimage(h: u64) -> u64 {
    let x = h.wrapping_mul(PHI64_INV);
    x ^ word_fold(x)
}

/// The hash bits a split never consumes: one, so `h << used` stays a defined shift (used < 64).
pub const HOME_MIN_BITS: u32 = 1;
/// The most route bits a row may consume: partition bits plus [`SPLIT_BITS`] per sub-split depth. D446 (V4R-123): a split
/// goes on into the bits below the first 32, down to the last [`HOME_MIN_BITS`]. A one-word key's hash is a bijection
/// (`KeyHash::word`, an odd multiplier): the keys of a sub-partition at `used` route bits differ in the 64 - `used` bits
/// left, so there are at most 2^(64 - used) of them (at most 2 at the deepest split). A table of more slots than those bits
/// homes them spread out (`home` reads zeros past the hash's last bit) and probes past each other.
pub const ROUTE_BITS: u32 = 64 - HOME_MIN_BITS;
/// The most home bits a table may take (its capacity is at most 2^32 slots).
pub const HOME_BITS: u32 = 32;
/// Route bits one sub-split consumes (a split fans out to 2^3 streams).
pub const SPLIT_BITS: u32 = 3;

/// A load factor `num / den` (the cost home's).
pub use cost_profile::hash::Load;
/// The build loads and least capacity live in the cost home (D02 / JOIN-04, `cost_profile::hash`): the hash join's
/// directory takes its load there from the detected machine; abi's own HashBuild keeps its bound below.
pub use cost_profile::hash::{BUILD_LOAD, BUILD_MIN_CAP_BITS, BUILD_SPARSE_LOAD};
/// D03: HashBuild tables of at most this many rows sit at [`BUILD_SPARSE_LOAD`]: the rows whose sparse table fits
/// `cost_profile::hash::HASHBUILD_L2` of this machine's L2 share (`breaker::detected_machine`; 2,048 on the reference).
pub fn build_sparse_rows() -> u64 {
    cost_profile::hash::hashbuild_sparse_rows(&crate::breaker::detected_machine())
}
/// A HashBuild table built in place or merged from partials (`HashBuild::table_cap_for`) fills to 7/8 of its slots: it lives
/// in its share's region, and past this load the caller spills instead of growing (D155). TUNABLE.
pub const BUILD_IN_PLACE_LOAD: Load = Load { num: 7, den: 8 };
/// The HashAgg table's grow load and least capacity live in the cost home too (`cost_profile::hash`): the planner's two-pass
/// top reads pass 1's table by them.
pub use cost_profile::hash::{agg_cap_bits, agg_grow_at, AGG_GROW_LOAD, AGG_MIN_CAP_BITS};

/// The route bits a table's rows already consumed: the partition bits plus [`SPLIT_BITS`] per sub-split. Built once at table
/// setup, where the limit is checked; every row's home then takes the bits below it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Depth {
    used: u32,
}

impl Depth {
    /// A table whose rows consumed no route bit (unpartitioned, W0).
    pub const TOP: Depth = Depth { used: 0 };
    /// The route bits one sub-split consumes ([`SPLIT_BITS`]), for a spill that splits by the same windows.
    pub const SPLIT_BITS: u32 = SPLIT_BITS;
    /// The split depths a table can reach: `Depth::new(0, d)` holds for `d` up to `ROUTE_BITS / SPLIT_BITS`, and more
    /// partition bits only lower it (a record kept per depth, such as the dense index's fills, holds this many slots).
    pub const LEVELS: usize = (ROUTE_BITS / SPLIT_BITS) as usize + 1;

    /// Rows routed by `pbits` partition bits and `depth` sub-splits; `None` past [`ROUTE_BITS`].
    pub const fn new(pbits: u32, depth: u32) -> Option<Depth> {
        let used = pbits as u64 + SPLIT_BITS as u64 * depth as u64;
        if used <= ROUTE_BITS as u64 { Some(Depth { used: used as u32 }) } else { None }
    }

    /// The rows of one partition of `pbits` bits.
    pub const fn part(pbits: u32) -> Option<Depth> {
        Depth::new(pbits, 0)
    }

    /// One sub-split below this depth; `None` past [`ROUTE_BITS`].
    pub const fn deeper(self) -> Option<Depth> {
        if self.used + SPLIT_BITS <= ROUTE_BITS { Some(Depth { used: self.used + SPLIT_BITS }) } else { None }
    }

    /// D446: the depth of `used` route bits read back from a table header, where only a `Depth`'s own bits are ever stored
    /// (`set_route_bits`), so `used <= ROUTE_BITS` always holds; a debug build asserts it, a release build never widens past it.
    pub const fn stored(used: u32) -> Depth {
        debug_assert!(used <= ROUTE_BITS, "a table's route bits past ROUTE_BITS");
        Depth { used: if used <= ROUTE_BITS { used } else { ROUTE_BITS } }
    }

    /// The route bits consumed.
    pub const fn used(self) -> u32 {
        self.used
    }
}

/// True when a table of 2^`cap_bits` slots at `at` stays inside the hash: asserted once at table setup.
pub const fn table_fits(cap_bits: u32, at: Depth) -> bool {
    cap_bits <= HOME_BITS && at.used <= ROUTE_BITS
}

/// A key's hash. It is read only through its methods; the word itself crosses the ABI (an IR register, a record word) by
/// [`KeyHash::to_abi`] / [`KeyHash::from_abi`] and is never taken apart elsewhere.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(transparent)]
pub struct KeyHash(u64);

impl KeyHash {
    /// A one-word image's hash (`of(&[k])`): the key's top bits folded into its low half by [`WORD_FOLD`]'s two shifts, then
    /// the Fibonacci multiply, the only multiply on the path (D2076's `hi * PHI64` fold put a second, dependent one on every
    /// probe of a key past 2^32). A key below 2^43 hashes to `k * PHI64` exactly, so ascending keys probe a table as they did
    /// under that plain multiply (a post-multiply fold scattered them: 1.2-1.9x slower hash join probes). Keys that differ
    /// only above bit 32 (`k << 32`, D2076) still spread over the bits a table homes on, not only its route bits. A
    /// bijection: the high half is kept, the low half is xored with a function of it, then an odd multiply.
    #[inline(always)]
    pub const fn word(k: u64) -> KeyHash {
        KeyHash((k ^ word_fold(k)).wrapping_mul(PHI64))
    }

    /// The hash of a key image (every image word: exact words and both Fp128 words, the flags words included).
    #[inline]
    pub const fn of(image: &[u64]) -> KeyHash {
        if image.len() == 1 {
            return KeyHash::word(image[0]);
        }
        let mut h = PHI64 ^ image.len() as u64;
        let mut i = 0;
        while i < image.len() {
            h = (h ^ image[i]).wrapping_mul(PHI64);
            h ^= h >> IMAGE_SHIFT;
            i += 1;
        }
        KeyHash(h.wrapping_mul(PHI64))
    }

    /// The home slot in a table of 2^`cap_bits` slots whose rows consumed `at`'s route bits: the `cap_bits` bits just below
    /// them. In a sliced table (one slice per partition), the slot is `part(pbits) << cap_bits | home(cap_bits, part)`.
    #[inline(always)]
    pub const fn home(self, cap_bits: u32, at: Depth) -> usize {
        if cap_bits == 0 {
            return 0;
        }
        ((self.0 << at.used) >> (64 - cap_bits)) as usize
    }

    /// The partition among 2^`pbits`: the top `pbits` bits.
    #[inline(always)]
    pub const fn part(self, pbits: u32) -> u32 {
        if pbits == 0 { 0 } else { (self.0 >> (64 - pbits)) as u32 }
    }

    /// The sub-split path after `pbits` partition bits and `depth` splits: the next `SPLIT_BITS · depth` bits.
    #[inline(always)]
    pub const fn sub(self, pbits: u32, depth: u32) -> u64 {
        let n = SPLIT_BITS * depth;
        if n == 0 { 0 } else { (self.0 << pbits) >> (64 - n) }
    }

    /// The child stream (one of 2^`SPLIT_BITS`) of a table at `at` when it splits once more: the next `SPLIT_BITS` bits.
    #[inline(always)]
    pub const fn split(self, at: Depth) -> u32 {
        ((self.0 << at.used) >> (64 - SPLIT_BITS)) as u32
    }

    /// The record's 32-bit tag where one is kept: a hint only, never an equality test.
    #[inline(always)]
    pub const fn stored(self) -> u32 {
        (self.0 ^ (self.0 >> 32)) as u32
    }

    /// The word, to carry across the ABI (an IR register, a host argument, a stored record word).
    #[inline(always)]
    pub const fn to_abi(self) -> u64 {
        self.0
    }

    /// A word carried across the ABI back into its hash; the word must come from [`KeyHash::to_abi`] or the IR half.
    #[inline(always)]
    pub const fn from_abi(w: u64) -> KeyHash {
        KeyHash(w)
    }
}

/// A row's route (the hash of its partition keys' image words, a prefix of `image`) and its full-key hash, from the one
/// helper, so routing and home can never take different keys (D154, D171).
#[inline]
pub const fn route_and_full(image: &[u64], route_words: usize) -> (KeyHash, KeyHash) {
    let full = KeyHash::of(image);
    if route_words >= image.len() {
        return (full, full);
    }
    let (prefix, _) = image.split_at(route_words);
    (KeyHash::of(prefix), full)
}

/// The capacity bits of a HashBuild table for `rows` entries: [`BUILD_LOAD`], or [`BUILD_SPARSE_LOAD`] up to
/// [`build_sparse_rows`], a power of two, at least [`BUILD_MIN_CAP_BITS`] (`cost_profile::hash::hashbuild_cap_bits`). The
/// caller checks [`table_fits`] and its region.
pub fn build_cap_bits(rows: u64) -> u32 {
    cost_profile::hash::hashbuild_cap_bits(rows, &crate::breaker::detected_machine())
}

/// A byte string's 64-bit bucket hash, its content identity across parts (dict's derived and bits tables, a per-code table's
/// fingerprint; dict/tests/hash_quality.rs pins its bits): a bucket or a leader-fold key beside the bytes, NEVER equality. Equality of byte strings is the bytes themselves or their [`fp128`].
pub fn hash_bytes64(b: &[u8]) -> u64 {
    const K1: u64 = 0x9E37_79B9_7F4A_7C15;
    const K2: u64 = 0xC2B2_AE3D_27D4_EB4F;
    let mut h = (b.len() as u64).wrapping_mul(K1) ^ K2;
    let mut rest = b;
    while rest.len() >= 8 {
        let w = u64::from_le_bytes([rest[0], rest[1], rest[2], rest[3], rest[4], rest[5], rest[6], rest[7]]);
        h = (h ^ w).wrapping_mul(K1).rotate_left(29);
        rest = &rest[8..];
    }
    let mut tail = 0u64;
    for (i, &x) in rest.iter().enumerate() {
        tail |= (x as u64) << (8 * i);
    }
    let mut h = (h ^ tail).wrapping_mul(K2);
    h ^= h >> 33;
    h = h.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    h ^= h >> 33;
    h = h.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    h ^ (h >> 33)
}

/// A program's content identity hash (`Program::key` and the re-keys built on it; never a key's hash): two 64-bit lanes
/// fed a byte at a time, FNV-1a's prime on the low lane and the [`PHI64`] multiply-rotate on the high lane; `finish` folds
/// them. The fields are its state: a fresh hash starts from FNV's offset basis (`hi` its halves swapped), a re-key from
/// the key's own two halves.
pub struct Fnv128 {
    pub lo: u64,
    pub hi: u64,
}

impl core::hash::Hasher for Fnv128 {
    fn finish(&self) -> u64 {
        self.lo ^ self.hi
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.lo = (self.lo ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
            self.hi = (self.hi ^ b as u64).wrapping_mul(PHI64).rotate_left(29);
        }
    }
}

/// The code cache's content key hasher (backend cache.rs `key_of`; never a key's hash): two 64-bit lanes mixed a word at
/// a time (the derived `Hash` feeds whole words; bytes only for names), FNV-1a's prime on the low lane and the [`PHI64`]
/// multiply-rotate on the high lane, finalised with murmur3's fmix64 into the 128-bit key (`key128`); `finish` is the
/// map's hash of the key itself.
pub struct Mix128 {
    lo: u64,
    hi: u64,
}

impl Mix128 {
    pub const fn new() -> Mix128 {
        Mix128 { lo: 0xcbf2_9ce4_8422_2325, hi: 0x8422_2325_cbf2_9ce4 }
    }

    #[inline]
    pub fn word(&mut self, w: u64) {
        self.lo = (self.lo ^ w).wrapping_mul(0x0000_0100_0000_01b3);
        self.hi = (self.hi ^ w).wrapping_mul(0x9e37_79b9_7f4a_7c15).rotate_left(29);
    }

    fn fmix(mut k: u64) -> u64 {
        k ^= k >> 33;
        k = k.wrapping_mul(0xff51_afd7_ed55_8ccd);
        k ^= k >> 33;
        k = k.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
        k ^ (k >> 33)
    }

    /// The 128-bit key: fmix64 of each lane (the low one crossed with the high).
    pub fn key128(&self) -> u128 {
        ((Mix128::fmix(self.hi) as u128) << 64) | Mix128::fmix(self.lo ^ self.hi.rotate_left(17)) as u128
    }
}

impl core::hash::Hasher for Mix128 {
    fn finish(&self) -> u64 {
        self.lo ^ self.hi
    }

    fn write(&mut self, bytes: &[u8]) {
        let mut it = bytes.chunks_exact(8);
        for c in &mut it {
            self.word(u64::from_le_bytes(c.try_into().unwrap()));
        }
        let rest = it.remainder();
        if !rest.is_empty() {
            let mut w = [0u8; 8];
            w[..rest.len()].copy_from_slice(rest);
            self.word(u64::from_le_bytes(w) ^ ((rest.len() as u64) << 56));
        }
    }

    fn write_u8(&mut self, v: u8) {
        self.word(v as u64)
    }
    fn write_u16(&mut self, v: u16) {
        self.word(v as u64)
    }
    fn write_u32(&mut self, v: u32) {
        self.word(v as u64)
    }
    fn write_u64(&mut self, v: u64) {
        self.word(v)
    }
    fn write_usize(&mut self, v: usize) {
        self.word(v as u64)
    }
    fn write_i8(&mut self, v: i8) {
        self.word(v as u64)
    }
    fn write_i16(&mut self, v: i16) {
        self.word(v as u64)
    }
    fn write_i32(&mut self, v: i32) {
        self.word(v as u64)
    }
    fn write_i64(&mut self, v: i64) {
        self.word(v as u64)
    }
    fn write_isize(&mut self, v: isize) {
        self.word(v as u64)
    }
}

/// Fibonacci hashing: the top `bits` (1..=63) of `h` times [`PHI64`], a spread pick among `1 << bits` (the program
/// cache's shard of a content key; never a key's hash).
#[inline]
pub const fn phi_top(h: u64, bits: u32) -> u64 {
    h.wrapping_mul(PHI64) >> (64 - bits)
}

/// A word list's multiplicative fold, `h = (h ^ w) * PHI64` from 0, every word taken and no finaliser: the floor prune's
/// group set (kernels ops/sort/topn/parthaving.rs `GroupSet`) indexes by its high bits and filters on bits 32..37. Not a
/// [`KeyHash`] (no length seed, no final mix), so it never picks a table home or a partition.
#[inline(always)]
pub fn phi_fold(w: &[u64]) -> u64 {
    w.iter().fold(0u64, |h, &x| (h ^ x).wrapping_mul(PHI64))
}

/// The process's Fp128 key (drawn once per process, never written anywhere): the caller holds it; this file keeps no state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FpKey {
    pub k0: u64,
    pub k1: u64,
}

/// The full-strength 128-bit fingerprint of ALL of a key's canonical bytes under the process key, `[lo, hi]`. Two keys with
/// equal fingerprints are equal; a table keyed by it keeps no bytes.
#[inline]
pub fn fp128(key: FpKey, b: &[u8]) -> [u64; 2] {
    siphash128(key.k0, key.k1, b)
}

#[inline(always)]
fn sip_round(v: &mut [u64; 4]) {
    v[0] = v[0].wrapping_add(v[1]);
    v[1] = v[1].rotate_left(13) ^ v[0];
    v[0] = v[0].rotate_left(32);
    v[2] = v[2].wrapping_add(v[3]);
    v[3] = v[3].rotate_left(16) ^ v[2];
    v[0] = v[0].wrapping_add(v[3]);
    v[3] = v[3].rotate_left(21) ^ v[0];
    v[2] = v[2].wrapping_add(v[1]);
    v[1] = v[1].rotate_left(17) ^ v[2];
    v[2] = v[2].rotate_left(32);
}

/// SipHash-2-4-128 keyed by `(k0, k1)`: `[lo, hi]` = the reference's 16 output bytes, little-endian (dict's `siphash128`,
/// the same bits).
pub fn siphash128(k0: u64, k1: u64, b: &[u8]) -> [u64; 2] {
    let mut v = [k0 ^ 0x736f_6d65_7073_6575, k1 ^ 0x646f_7261_6e64_6f6d ^ 0xee, k0 ^ 0x6c79_6765_6e65_7261, k1 ^ 0x7465_6462_7974_6573];
    let mut rest = b;
    while rest.len() >= 8 {
        let m = u64::from_le_bytes([rest[0], rest[1], rest[2], rest[3], rest[4], rest[5], rest[6], rest[7]]);
        v[3] ^= m;
        sip_round(&mut v);
        sip_round(&mut v);
        v[0] ^= m;
        rest = &rest[8..];
    }
    let mut m = (b.len() as u64) << 56;
    for (i, &x) in rest.iter().enumerate() {
        m |= (x as u64) << (8 * i);
    }
    v[3] ^= m;
    sip_round(&mut v);
    sip_round(&mut v);
    v[0] ^= m;
    v[2] ^= 0xee;
    for _ in 0..4 {
        sip_round(&mut v);
    }
    let lo = v[0] ^ v[1] ^ v[2] ^ v[3];
    v[1] ^= 0xdd;
    for _ in 0..4 {
        sip_round(&mut v);
    }
    [lo, v[0] ^ v[1] ^ v[2] ^ v[3]]
}
