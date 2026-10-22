#[inline]
pub const fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[inline]
pub const fn fold64(acc: u64, word: u64) -> u64 {
    mix64(acc ^ word.wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

const LANE1_SEED: u64 = 0x3168_6d32_6372_6770;
const LANE2_SEED: u64 = 0x3268_6d32_6372_6770;

fn lane(seed: u64, bytes: &[u8]) -> u64 {
    let mut acc = fold64(seed, bytes.len() as u64);
    let mut chunks = bytes.chunks_exact(8);
    for c in &mut chunks {
        acc = fold64(acc, u64::from_le_bytes(c.try_into().expect("len 8")));
    }
    let rem = chunks.remainder();
    if !rem.is_empty() {
        let mut tail = [0u8; 8];
        tail[..rem.len()].copy_from_slice(rem);
        acc = fold64(acc, u64::from_le_bytes(tail));
    }
    mix64(acc)
}

pub fn meta_hash128(bytes: &[u8]) -> (u64, u64) {
    let len = bytes.len() as u64;
    let mut a = fold64(LANE1_SEED, len);
    let mut b = fold64(LANE2_SEED, len);
    let mut chunks = bytes.chunks_exact(8);
    for c in &mut chunks {
        let w = u64::from_le_bytes(c.try_into().expect("len 8"));
        a = fold64(a, w);
        b = fold64(b, w);
    }
    let rem = chunks.remainder();
    if !rem.is_empty() {
        let mut tail = [0u8; 8];
        tail[..rem.len()].copy_from_slice(rem);
        let w = u64::from_le_bytes(tail);
        a = fold64(a, w);
        b = fold64(b, w);
    }
    (mix64(a), mix64(b))
}

#[inline]
pub fn meta_hash128_and_digests(bytes: &[u8], bank: (u64, u64)) -> (u64, u64, u64, u64) {
    let len = bytes.len() as u64;
    let mut a = fold64(LANE1_SEED, len);
    let mut b = fold64(LANE2_SEED, len);
    let mut c = fold64(bank.0, len);
    let mut d = fold64(bank.1, len);
    let mut chunks = bytes.chunks_exact(8);
    for ch in &mut chunks {
        let w = u64::from_le_bytes(ch.try_into().expect("len 8"));
        a = fold64(a, w);
        b = fold64(b, w);
        c = fold64(c, w);
        d = fold64(d, w);
    }
    let rem = chunks.remainder();
    if !rem.is_empty() {
        let mut t = [0u8; 8];
        t[..rem.len()].copy_from_slice(rem);
        let w = u64::from_le_bytes(t);
        a = fold64(a, w);
        b = fold64(b, w);
        c = fold64(c, w);
        d = fold64(d, w);
    }
    (mix64(a), mix64(b), c, d)
}

#[inline]
pub fn meta_hash128_word(w: u64, width: u32) -> (u64, u64) {
    debug_assert!((1..=8).contains(&width));
    let wm = if width >= 8 { w } else { w & ((1u64 << (8 * width)) - 1) };
    let (k1, k2) = word_seeds(width);
    (mix64(fold64(k1, wm)), mix64(fold64(k2, wm)))
}

#[inline(always)]
fn word_seeds(width: u32) -> (u64, u64) {
    const fn k(seed: u64) -> [u64; 9] {
        let mut t = [0u64; 9];
        let mut i = 0;
        while i < 9 {
            t[i] = fold64(seed, i as u64);
            i += 1;
        }
        t
    }
    const K1: [u64; 9] = k(LANE1_SEED);
    const K2: [u64; 9] = k(LANE2_SEED);
    (K1[width as usize], K2[width as usize])
}

#[inline]
pub fn ndv_hash(bytes: &[u8]) -> u64 {
    lane(LANE1_SEED, bytes)
}

#[derive(Debug, Clone, Copy)]
pub struct FoldChain(u64);

impl FoldChain {
    pub fn new(seed: u64) -> FoldChain {
        FoldChain(mix64(seed))
    }
    pub fn word(&mut self, w: u64) {
        self.0 = fold64(self.0, w);
    }
    pub fn bytes(&mut self, b: &[u8]) {
        self.word(lane(LANE2_SEED, b));
    }
    pub fn finish(self) -> u64 {
        mix64(self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::{meta_hash128, meta_hash128_and_digests, meta_hash128_word, FoldChain};

    #[test]
    fn hash_family_golden_vectors() {
        assert_eq!(
            meta_hash128(b""),
            (0x22af_6b82_c16f_c5d8, 0xd32b_d8b3_fdcd_e7be)
        );
        assert_eq!(
            meta_hash128(&[0x70, 0x67, 0x72, 0x63, 0x32]),
            (0x8491_3bbc_e706_7257, 0xc828_bbc2_9cb2_3b94)
        );
        assert_eq!(
            meta_hash128(b"hello meta plane"),
            (0x25fb_f673_0c93_65f5, 0x4903_0cd2_b4aa_3441)
        );
        assert_eq!(
            meta_hash128(&[0u8]),
            (0xb69c_9127_5573_441b, 0x86c7_899e_136f_0bae)
        );
    }

    #[test]
    fn fused_and_word_forms_equal_the_per_lane_walks() {
        let mut x: u64 = 0x1234_5678_9abc_def1;
        for len in 0..80usize {
            let b: Vec<u8> = (0..len).map(|i| (x.rotate_left(i as u32) >> 7) as u8).collect();
            let want = (super::lane(super::LANE1_SEED, &b), super::lane(super::LANE2_SEED, &b));
            assert_eq!(meta_hash128(&b), want, "len {len}");
            let (h1, h2, _, _) = meta_hash128_and_digests(&b, (x, !x));
            assert_eq!((h1, h2), want, "len {len}");
            x = super::mix64(x);
        }
        for width in 1..=8u32 {
            for _ in 0..200 {
                x = super::mix64(x);
                let img = x.to_le_bytes();
                assert_eq!(meta_hash128_word(x, width), meta_hash128(&img[..width as usize]));
            }
        }
    }

    #[test]
    fn writer_fast_forms_golden_vectors() {
        for &(w, width, want) in WORD_GOLDEN.iter() {
            assert_eq!(meta_hash128_word(w, width), want, "word {w:#x} width {width}");
        }
        for &(img, want) in BYTES_GOLDEN.iter() {
            assert_eq!(meta_hash128(img), want, "{img:?}");
            let (h1, h2, _, _) = meta_hash128_and_digests(img, (1, 2));
            assert_eq!((h1, h2), want, "{img:?}");
        }
    }

    const WORD_GOLDEN: [(u64, u32, (u64, u64)); 8] = [
        (0x0000_0000_0000_0000, 1, (0xb69c_9127_5573_441b, 0x86c7_899e_136f_0bae)),
        (0x0000_0000_0000_00ff, 1, (0xaab8_48c4_2d40_f3fb, 0x5c0c_d652_b17f_2e2f)),
        (0x0000_0000_0000_1234, 2, (0x21a3_768c_e5b1_9784, 0x0a22_b41b_d4d1_b7a3)),
        (0x0000_0000_dead_beef, 4, (0x2317_1862_17b7_f54c, 0x1bba_32ab_30e9_d8ba)),
        (0x8000_0000_0000_0001, 3, (0x9373_a9ab_e833_6a5b, 0x17d0_9a33_d3ff_5f52)),
        (0x0123_4567_89ab_cdef, 8, (0xe89c_c696_a153_f785, 0x84c3_25f1_026f_cbba)),
        (0xffff_ffff_ffff_ffff, 8, (0x65cb_604c_dd12_2b5c, 0xe48e_7180_c322_bf35)),
        (0x3ff8_0000_0000_0000, 8, (0x5c9d_a2a8_09f5_9484, 0x0de5_f8c4_4cae_8f1a)),
    ];
    const BYTES_GOLDEN: [(&[u8], (u64, u64)); 5] = [
        (b"abcdefg", (0x7e6d_cd56_50b4_67eb, 0x5dfd_cacc_214e_273c)),
        (b"abcdefgh", (0x1ed8_e3cd_4ce3_01d5, 0xd499_eeea_901f_a3af)),
        (b"abcdefghi", (0x16a9_0c7f_e15d_67f5, 0xf5b2_5ede_5eee_2590)),
        (b"0123456789abcdefg", (0x5bc2_d4e0_e432_c723, 0x7992_66a7_469d_5a2d)),
        (b"sample.bin row 42 of 999", (0xd6bc_fbbf_b704_59dd, 0x4bfc_2f4f_77bd_85db)),
    ];

    #[test]
    fn fold_chain_golden_vectors() {
        let mut c = FoldChain::new(0x7066_7032_6372_6770);
        c.word(1);
        c.word(8);
        assert_eq!(c.finish(), 0x15b5_df2e_7d16_b100);
    }
}

// The hash is unseeded and invertible (its value is part of the on-disk format),
// so raw `h1 & mask` lets a client craft colliding values and drive linear
// probing quadratic. The mix folds in a per-process random seed the client
// cannot observe.
static SLOT_SEED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cold]
fn init_slot_seed() -> u64 {
    use std::hash::BuildHasher;
    use std::sync::atomic::Ordering::Relaxed;
    let fresh = std::collections::hash_map::RandomState::new().hash_one(0x5eed_u64) | 1;
    match SLOT_SEED.compare_exchange(0, fresh, Relaxed, Relaxed) {
        Ok(_) => fresh,
        Err(existing) => existing,
    }
}

#[inline]
pub fn slot_index(h1: u64, mask: usize) -> usize {
    let seed = match SLOT_SEED.load(std::sync::atomic::Ordering::Relaxed) {
        0 => init_slot_seed(),
        s => s,
    };
    ((h1 ^ seed).wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_right(29) as usize) & mask
}
