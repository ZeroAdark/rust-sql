//! Bank identity + MANIFEST vocabulary (spec §18; ruling O-10): bank
//! identity is LOGICAL — per-column content hashes + row counts,
//! decode-verified. Physical bytes are free to vary under parallel load
//! (the COPY-order ruling), so the hash is an order-insensitive MULTISET
//! accumulator. M3-J's recipe writer + verifier consume this vocabulary.

use crate::ident::fold64;

/// Required bank-MANIFEST field names (O-10; schema-validated by M3-J).
pub const BANK_FIELD_FORMAT_VERSION: &str = "format_version";
pub const BANK_FIELD_RECIPE_VERSION: &str = "bank_recipe_version";
pub const BANK_FIELD_BUILT_BY_TIP: &str = "built_by_tip";
/// Per-chunk encoding elections (the bank-names-lie law made contractual).
pub const BANK_FIELD_ELECTIONS: &str = "elections";
/// Geometry facts: part/granule counts, dict sizes.
pub const BANK_FIELD_GEOMETRY: &str = "geometry";
/// Per-column logical identity rows: (name, row_count, hash_hi, hash_lo).
pub const BANK_FIELD_COLUMNS: &str = "columns";

pub const BANK_REQUIRED_FIELDS: [&str; 6] = [
    BANK_FIELD_FORMAT_VERSION,
    BANK_FIELD_RECIPE_VERSION,
    BANK_FIELD_BUILT_BY_TIP,
    BANK_FIELD_ELECTIONS,
    BANK_FIELD_GEOMETRY,
    BANK_FIELD_COLUMNS,
];

/// Per-row lane seeds (ASCII "banklane" / "colident").
const LANE_LO_SEED: u64 = 0x6261_6e6b_6c61_6e65;
const LANE_HI_SEED: u64 = 0x636f_6c69_6465_6e74;
/// The row-digest lane seeds `(lo, hi)`: a hash lane walking the same bytes
/// folds these chains alongside its own (PGRC-WRITER-FAST `bank`,
/// `order_key::hash::meta_hash128_and_digests`).
pub const BANK_LANE_SEEDS: (u64, u64) = (LANE_LO_SEED, LANE_HI_SEED);
/// Null-row marker word.
const NULL_MARK: u64 = 0x6e75_6c6c_6d61_726b;

/// Order-insensitive per-column logical identity (spec §18): two independent
/// mix64 folds per row over the canonical value bytes (spec §18.1),
/// accumulated by wrapping add. Merge is commutative + associative, so any
/// row partition across parts/workers yields the same digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LogicalColHash {
    lo: u64,
    hi: u64,
    rows: u64,
}

/// One lane's row digest walked by itself (the reference form of
/// [`row_digests`]).
#[cfg(test)]
fn row_digest(seed: u64, bytes: &[u8]) -> u64 {
    let mut acc = fold64(seed, bytes.len() as u64);
    let mut chunks = bytes.chunks_exact(8);
    for c in &mut chunks {
        acc = fold64(acc, u64::from_le_bytes(c.try_into().expect("len 8")));
    }
    let rem = chunks.remainder();
    if !rem.is_empty() {
        let mut w = [0u8; 8];
        w[..rem.len()].copy_from_slice(rem);
        acc = fold64(acc, u64::from_le_bytes(w));
    }
    acc
}

/// Both row digests of a canonical value image in ONE pass
/// (PGRC-WRITER-FAST `fuse`): one load per word feeding two chains —
/// bit-identical to walking each lane by itself.
#[inline(always)]
pub fn row_digests(bytes: &[u8]) -> (u64, u64) {
    let len = bytes.len() as u64;
    let mut a = fold64(LANE_LO_SEED, len);
    let mut b = fold64(LANE_HI_SEED, len);
    let mut chunks = bytes.chunks_exact(8);
    for c in &mut chunks {
        let w = u64::from_le_bytes(c.try_into().expect("len 8"));
        a = fold64(a, w);
        b = fold64(b, w);
    }
    let rem = chunks.remainder();
    if !rem.is_empty() {
        let mut t = [0u8; 8];
        t[..rem.len()].copy_from_slice(rem);
        let w = u64::from_le_bytes(t);
        a = fold64(a, w);
        b = fold64(b, w);
    }
    (a, b)
}

/// Per-width `fold64(seed, width)`: the length fold of a word image.
const fn word_seed_table(seed: u64) -> [u64; 9] {
    let mut t = [0u64; 9];
    let mut i = 0;
    while i < 9 {
        t[i] = fold64(seed, i as u64);
        i += 1;
    }
    t
}
const WORD_LO: [u64; 9] = word_seed_table(LANE_LO_SEED);
const WORD_HI: [u64; 9] = word_seed_table(LANE_HI_SEED);

impl LogicalColHash {
    pub fn new() -> LogicalColHash {
        LogicalColHash::default()
    }

    /// Fold one non-null row's canonical value bytes (spec §18.1).
    pub fn observe(&mut self, canonical_bytes: &[u8]) {
        let (a, b) = row_digests(canonical_bytes);
        self.add(a, b);
    }

    /// The row digests of a word datum's canonical image
    /// `w.to_le_bytes()[..width]` (width 1..=8; PGRC-WRITER-FAST `word`:
    /// one zero-padded word, the length fold a per-width constant), as
    /// `(masked word, lo, hi)` for a caller that re-adds them on a repeated
    /// value through [`Self::add`]. Bit-identical to the byte form.
    #[inline]
    pub fn word_digests(w: u64, width: usize) -> (u64, u64, u64) {
        debug_assert!((1..=8).contains(&width));
        let wm = if width >= 8 { w } else { w & ((1u64 << (8 * width)) - 1) };
        (wm, fold64(WORD_LO[width], wm), fold64(WORD_HI[width], wm))
    }

    /// Fold one non-null row whose [`row_digests`] are already in hand.
    #[inline]
    pub fn add(&mut self, lo: u64, hi: u64) {
        self.lo = self.lo.wrapping_add(lo);
        self.hi = self.hi.wrapping_add(hi);
        self.rows += 1;
    }

    /// Fold `count` non-null rows that all carry the value whose
    /// [`row_digests`] are `(lo, hi)` (PGRC-WRITER-FAST `bank`): the
    /// multiset sum is a wrapping add, so `count` identical rows add
    /// `count · digest` exactly.
    #[inline]
    pub fn observe_repeated(&mut self, lo: u64, hi: u64, count: u64) {
        self.lo = self.lo.wrapping_add(lo.wrapping_mul(count));
        self.hi = self.hi.wrapping_add(hi.wrapping_mul(count));
        self.rows += count;
    }

    /// Fold one null row.
    pub fn observe_null(&mut self) {
        self.lo = self.lo.wrapping_add(fold64(LANE_LO_SEED, NULL_MARK));
        self.hi = self.hi.wrapping_add(fold64(LANE_HI_SEED, NULL_MARK));
        self.rows += 1;
    }

    /// Merge another accumulator (any partition, any order).
    pub fn merge(&mut self, other: &LogicalColHash) {
        self.lo = self.lo.wrapping_add(other.lo);
        self.hi = self.hi.wrapping_add(other.hi);
        self.rows += other.rows;
    }

    /// (hash_lo, hash_hi, row_count) — the bank MANIFEST column row.
    pub fn digest(&self) -> (u64, u64, u64) {
        (self.lo, self.hi, self.rows)
    }
}

#[cfg(test)]
mod writer_fast_tests {
    use super::*;

    /// The one-pass, word and repeated forms fold exactly what per-row
    /// `observe` of the same images folded before PGRC-WRITER-FAST (the
    /// per-lane walks).
    #[test]
    fn writer_fast_forms_equal_the_per_lane_walks() {
        let mut x: u64 = 0x0bad_c0de_1234_5678;
        let mut a = LogicalColHash::new();
        let mut b = LogicalColHash::new();
        for len in 0..70usize {
            x = crate::ident::mix64(x);
            let v: Vec<u8> = (0..len).map(|i| (x >> (i % 56)) as u8).collect();
            let (lo, hi) = row_digests(&v);
            assert_eq!((lo, hi), (row_digest(LANE_LO_SEED, &v), row_digest(LANE_HI_SEED, &v)), "len {len}");
            for _ in 0..3 {
                a.lo = a.lo.wrapping_add(row_digest(LANE_LO_SEED, &v));
                a.hi = a.hi.wrapping_add(row_digest(LANE_HI_SEED, &v));
                a.rows += 1;
            }
            b.observe_repeated(lo, hi, 3);
        }
        for width in 1..=8usize {
            x = crate::ident::mix64(x);
            let img = &x.to_le_bytes()[..width];
            a.lo = a.lo.wrapping_add(row_digest(LANE_LO_SEED, img));
            a.hi = a.hi.wrapping_add(row_digest(LANE_HI_SEED, img));
            a.rows += 1;
            let (wm, lo, hi) = LogicalColHash::word_digests(x, width);
            assert_eq!(wm.to_le_bytes()[..width], *img);
            b.add(lo, hi);
        }
        assert_eq!(a.digest(), b.digest());
    }

    /// Golden digests of the PGRC-WRITER-FAST forms; the expected values
    /// are trunk's per-row `observe` of the same images (computed at
    /// bb5e1ad91b2): a drift here breaks bank identities.
    #[test]
    fn writer_fast_forms_golden() {
        for &(w, width, want) in WORD_GOLDEN.iter() {
            let (_, lo, hi) = LogicalColHash::word_digests(w, width);
            let mut h = LogicalColHash::new();
            h.add(lo, hi);
            assert_eq!(h.digest(), want, "word {w:#x} width {width}");
        }
        for &(img, count, want) in BYTES_GOLDEN.iter() {
            let (lo, hi) = row_digests(img);
            let mut h = LogicalColHash::new();
            h.observe_repeated(lo, hi, count);
            assert_eq!(h.digest(), want, "{img:?} x{count}");
        }
    }

    const WORD_GOLDEN: [(u64, usize, (u64, u64, u64)); 8] = [
        (0x0000_0000_0000_0000, 1, (0x03d3_1556_e7fe_fc85, 0x8a32_32b5_7294_34a4, 1)),
        (0x0000_0000_0000_00ff, 1, (0xe03a_27fe_daca_c2a0, 0x2446_595b_bbb0_f846, 1)),
        (0x0000_0000_0000_1234, 2, (0xe23b_7fe0_9bc2_5680, 0xf693_de45_54d3_cb28, 1)),
        (0x0000_0000_dead_beef, 4, (0xc636_87bc_0b7a_d12b, 0x51fe_4e76_5691_5472, 1)),
        (0x8000_0000_0000_0001, 3, (0x89ff_bdfa_4fec_ede2, 0xea6b_ebd0_2cb3_3ecf, 1)),
        (0x0123_4567_89ab_cdef, 8, (0xd558_4944_aada_2077, 0x6157_8c8f_ec97_c30d, 1)),
        (0xffff_ffff_ffff_ffff, 8, (0x96ce_b7ba_115f_0fe0, 0x11d1_6d31_6bf1_8199, 1)),
        (0x3ff8_0000_0000_0000, 8, (0xa17e_d6ee_0bac_0884, 0x14a0_a66e_15f3_f0ce, 1)),
    ];
    const BYTES_GOLDEN: [(&[u8], u64, (u64, u64, u64)); 5] = [
        (b"", 1, (0x39fa_ba74_0d38_e594, 0x647d_5284_2ebe_9f6c, 1)),
        (b"alpha", 3, (0xbfad_a745_db75_8acf, 0x87c7_c154_58c7_97fb, 3)),
        (b"abcdefgh", 2, (0x232b_0a72_a019_1c94, 0x6e84_e92f_6b59_d53e, 2)),
        (b"abcdefghi", 5, (0x7e51_8eb8_6019_84a0, 0x9794_4c87_73b8_16d1, 5)),
        (b"sample.bin row 42 of 999", 1000, (0x5187_3a2d_d437_77a8, 0xd63a_cb7c_9880_3510, 1000)),
    ];
}
