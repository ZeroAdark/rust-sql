
use crate::hash::meta_hash128;

pub const BLOOM_BYTES_MIN: u32 = 64;

pub fn bloom_bytes_for(ndv: u32, k: u32) -> u32 {
    let bits = (ndv.max(1) as f64 * k.max(1) as f64 / core::f64::consts::LN_2).ceil();
    let bytes = (bits / 8.0).ceil();
    if bytes >= (1u64 << 31) as f64 {
        return 1 << 31;
    }
    (bytes as u32).next_power_of_two().max(BLOOM_BYTES_MIN)
}

pub fn bloom_false_positive(ndv: f64, k: u32) -> f64 {
    if ndv.is_nan() || ndv <= 0.0 {
        return 0.0;
    }
    let n = ndv.min(u32::MAX as f64);
    let m = bloom_bytes_for(n.ceil() as u32, k) as f64 * 8.0;
    let k = k.max(1) as f64;
    (1.0 - (-k * n / m).exp()).powf(k)
}

pub fn bloom_false_positive_at(ndv: f64, k: u32, bytes: u32) -> f64 {
    if ndv.is_nan() || ndv <= 0.0 {
        return 0.0;
    }
    if bytes == 0 {
        return 1.0;
    }
    let m = bytes as f64 * 8.0;
    let k = k.max(1) as f64;
    (1.0 - (-k * ndv / m).exp()).powf(k)
}

/// Tile a filter block up to `bytes` (a power-of-two multiple of the block's power-of-two length) by repeating it. Sound
/// because a probe of the tiled block reads bit `h % (bytes * 8)`, which sits in some copy at offset `h % (len * 8)`
/// (the length divides `bytes`), the bit the insert set; a value the block lacks hits a set bit as often as it did.
pub fn bloom_tile(block: &[u8], bytes: usize) -> Vec<u8> {
    assert!(!block.is_empty() && block.len().is_power_of_two() && bytes.is_power_of_two() && bytes % block.len() == 0);
    block.repeat(bytes / block.len())
}

/// P5-11: a constant's probe word: the low 32 bits of the writer's two hashes of its canonical bytes
/// ([`crate::hash::meta_hash128`]), `h2`'s above `h1`'s. A probe of a block of at most `2^32` bits needs no more: its
/// positions `(h1 + i h2) % nbits` with `nbits` a power of two read only the low `log2(nbits)` bits of each hash.
pub fn bloom_pack(canonical: &[u8]) -> i64 {
    let (h1, h2) = meta_hash128(canonical);
    ((h1 & 0xffff_ffff) | (h2 << 32)) as i64
}

/// [`bloom_may_contain`] of the value whose probe word is `packed` ([`bloom_pack`]): the same bits, so the same answer,
/// on a block of a power-of-two length of at most `2^29` bytes; `true` (proves nothing) on any other block.
pub fn bloom_may_contain_packed(block: &[u8], k: u32, packed: i64) -> bool {
    let nbits = (block.len() as u64) * 8;
    if nbits == 0 || !nbits.is_power_of_two() || nbits > 1 << 32 {
        return true;
    }
    let (h1, h2) = (packed as u64 & 0xffff_ffff, packed as u64 >> 32);
    for bit in bit_positions_hashed(h1, h2, k, nbits) {
        if block[(bit / 8) as usize] & (1 << (bit % 8)) == 0 {
            return false;
        }
    }
    true
}

/// Fold a filter block down to `bytes` (a power of two dividing the
/// block's length) by OR-ing its `bytes`-sized pieces. Sound because a
/// bit's position is `h % nbits` with `nbits` a power of two: the folded
/// position `h % (bytes * 8)` is the original position modulo the new
/// size, which is exactly where the OR puts it.
pub fn bloom_fold(block: &[u8], bytes: usize) -> Vec<u8> {
    assert!(bytes > 0 && bytes.is_power_of_two() && block.len() % bytes == 0);
    let mut out = block[..bytes].to_vec();
    for piece in block[bytes..].chunks(bytes) {
        for (o, b) in out.iter_mut().zip(piece) {
            *o |= *b;
        }
    }
    out
}

#[inline]
fn bit_positions_hashed(h1: u64, h2: u64, k: u32, nbits: u64) -> impl Iterator<Item = u64> {
    (0..k as u64).map(move |i| h1.wrapping_add(i.wrapping_mul(h2)) % nbits)
}

#[inline]
fn bit_positions(canonical: &[u8], k: u32, nbits: u64) -> impl Iterator<Item = u64> {
    let (h1, h2) = meta_hash128(canonical);
    bit_positions_hashed(h1, h2, k, nbits)
}

pub fn bloom_insert_hashed(block: &mut [u8], k: u32, h1: u64, h2: u64) {
    let nbits = (block.len() as u64) * 8;
    debug_assert!(nbits > 0);
    for bit in bit_positions_hashed(h1, h2, k, nbits) {
        block[(bit / 8) as usize] |= 1 << (bit % 8);
    }
}

pub fn bloom_insert(block: &mut [u8], k: u32, canonical: &[u8]) {
    let (h1, h2) = meta_hash128(canonical);
    bloom_insert_hashed(block, k, h1, h2);
}

/// `false` is definitive absence; `true` proves nothing. A malformed (empty)
/// block answers `true` — consult-nothing is always sound.
pub fn bloom_may_contain(block: &[u8], k: u32, canonical: &[u8]) -> bool {
    let nbits = (block.len() as u64) * 8;
    if nbits == 0 {
        return true;
    }
    for bit in bit_positions(canonical, k, nbits) {
        if block[(bit / 8) as usize] & (1 << (bit % 8)) == 0 {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The block is the bits `2^-k` takes, rounded up to a power of two, at least the minimum.
    #[test]
    fn bloom_bytes_follow_ndv_and_k() {
        assert_eq!(bloom_bytes_for(0, 5), BLOOM_BYTES_MIN);
        assert_eq!(bloom_bytes_for(2, 5), BLOOM_BYTES_MIN);
        assert_eq!(bloom_bytes_for(1000, 5), 1024);
        assert_eq!(bloom_bytes_for(8192, 5), 8192);
        assert_eq!(bloom_bytes_for(8192, 7), 16384);
        assert_eq!(bloom_bytes_for(u32::MAX, 64), 1 << 31);
        for (ndv, k) in [(100u32, 4u32), (1000, 5), (5000, 6), (8192, 7)] {
            let fp = bloom_false_positive(ndv as f64, k);
            assert!(fp <= 0.5f64.powi(k as i32) * 1.05, "ndv {ndv} k {k}: fp {fp}");
        }
        assert_eq!(bloom_false_positive(0.0, 5), 0.0);
        for (ndv, k) in [(100u32, 4u32), (8192, 6)] {
            let own = bloom_bytes_for(ndv, k);
            assert_eq!(bloom_false_positive_at(ndv as f64, k, own), bloom_false_positive(ndv as f64, k));
            assert!(bloom_false_positive_at(ndv as f64, k, own / 2) > bloom_false_positive(ndv as f64, k));
        }
        assert_eq!(bloom_false_positive_at(10.0, 4, 0), 1.0);
        assert_eq!(bloom_false_positive_at(0.0, 4, 64), 0.0);
    }

    #[test]
    fn bloom_fold_and_tile_keep_members() {
        let k = 5;
        let mut block = vec![0u8; 16384];
        for i in 0..300u64 {
            bloom_insert(&mut block, k, &i.to_le_bytes());
        }
        for bytes in [64usize, 256, 512, 2048, 16384] {
            let f = bloom_fold(&block, bytes);
            assert_eq!(f.len(), bytes);
            for i in 0..300u64 {
                assert!(bloom_may_contain(&f, k, &i.to_le_bytes()));
            }
            for up in [bytes, bytes * 2, 32768] {
                let t = bloom_tile(&f, up);
                for i in 0..300u64 {
                    assert!(bloom_may_contain(&t, k, &i.to_le_bytes()));
                }
                for i in 1_000_000u64..1_000_500 {
                    assert_eq!(bloom_may_contain(&t, k, &i.to_le_bytes()), bloom_may_contain(&f, k, &i.to_le_bytes()));
                }
            }
        }
        let f = bloom_fold(&block, 512);
        let misses = (1_000_000u64..1_002_000)
            .filter(|i| !bloom_may_contain(&f, k, &i.to_le_bytes()))
            .count();
        assert!(misses > 1900, "512 B over 300 keys: only {misses}/2000 misses");
    }

    #[test]
    fn bloom_packed_probe_matches_the_bytes() {
        for bytes in [64usize, 1024, 8192, 16384] {
            for k in [1u32, 4, 7, 16] {
                let mut block = vec![0u8; bytes];
                let vals: Vec<Vec<u8>> = (0..bytes as u64 / 2).map(|i| format!("v{i}").into_bytes()).collect();
                for v in &vals {
                    bloom_insert(&mut block, k, v);
                }
                for i in 0..4000u64 {
                    let v = format!("v{i}").into_bytes();
                    assert_eq!(bloom_may_contain_packed(&block, k, bloom_pack(&v)), bloom_may_contain(&block, k, &v), "{bytes} B k {k} v{i}");
                }
            }
        }
        assert!(bloom_may_contain_packed(&[], 4, 7));
        assert!(bloom_may_contain_packed(&[0u8; 3], 4, 7));
    }

    #[test]
    fn bloom_never_false_negative_smoke() {
        let mut block = vec![0u8; 2048];
        for i in 0..64u64 {
            bloom_insert(&mut block, 4, &i.to_le_bytes());
        }
        for i in 0..64u64 {
            assert!(bloom_may_contain(&block, 4, &i.to_le_bytes()));
        }
        assert!(bloom_may_contain(&[], 4, b"anything"));
    }
}
