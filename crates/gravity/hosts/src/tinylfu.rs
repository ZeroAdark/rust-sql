//! cachepolicy (C5, `cache_policy` = 2): W-TinyLFU for the condition cache (ops/scan/pgrc/cond_cache.rs). The cache keeps
//! its segments (a window of 1 % of its cap, then main: probation and protected) and asks this module's frequency sketch
//! whether the window's least recent entry earns a place in main (`mempolicy::cache::tinylfu_admit`: counted more often
//! than the main entries it would push out, summed). The policy's numbers are mempolicy's (`tinylfu_window`,
//! `tinylfu_protected`, `TINYLFU_SKETCH_WIDTH`, `TINYLFU_RESET_ADDS`). Under lru (the knob's default) the cache makes no
//! sketch and reads no segment. The fp tables stay lru at every value: their W-TinyLFU (a count x bytes admission) cut
//! misses but slowed single queries, so it is parked (FOLLOWUPS CACHEPOLICY-FP-TLFU).

/// The switch: `cache_policy` = 2 (W-TinyLFU), read at each insert and lookup of the condition cache.
#[inline]
pub fn on() -> bool {
    mempolicy::cache::tinylfu_on()
}

/// The segments, an entry's place in its cache: the window, then the main space's probation and protected.
pub const WINDOW: u8 = 0;
pub const PROBATION: u8 = 1;
pub const PROTECTED: u8 = 2;

/// The words of a sketch: 4 rows of `TINYLFU_SKETCH_WIDTH` 4-bit counters, 16 to a word. The caller allocates them on its
/// cache's charge ([`Sketch::new`]).
pub const SKETCH_WORDS: usize = 4 * mempolicy::cache::TINYLFU_SKETCH_WIDTH / 16;

/// A count-min sketch of the keys looked up (TinyLFU's frequency estimate): 4 rows of 4-bit counters, each key's count
/// the least of its 4 counters; every `TINYLFU_RESET_ADDS` adds halve every counter, so old popularity fades.
pub struct Sketch {
    /// The words' bytes on the owning cache's charge, back on the dropping thread's account at its drop (first: fields
    /// drop in declaration order, so the charge goes before the bytes it counts).
    _charge: memledger::Charge,
    words: Box<[u64]>,
    adds: u64,
}

/// A key's 64-bit mix (splitmix64's finalizer: every key bit moves every mixed bit), so keys that differ only in high bits
/// (a fp key's attno sits at bit 48) still spread over every row's counters.
#[inline]
const fn mix(key: u64) -> u64 {
    let mut x = key ^ 0x9E37_79B9_7F4A_7C15;
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

impl Sketch {
    /// A sketch over `words` (`SKETCH_WORDS` zeroed words the caller allocated on its cache's `charge`).
    pub fn new(words: Box<[u64]>, charge: memledger::Charge) -> Sketch {
        debug_assert_eq!(words.len(), SKETCH_WORDS);
        Sketch { _charge: charge, words, adds: 0 }
    }

    /// The bytes of a sketch's words (the caller's charge).
    pub const fn bytes() -> u64 {
        (SKETCH_WORDS * core::mem::size_of::<u64>()) as u64
    }

    /// Row `row`'s counter of a key mixed to `h` (`mix`): the word and the counter's shift in it. Each row takes its own 16
    /// bits of the mix (the width is at most 2^16).
    #[inline]
    fn slot(h: u64, row: usize) -> (usize, u32) {
        let i = ((h >> (16 * row)) as usize) & (mempolicy::cache::TINYLFU_SKETCH_WIDTH - 1);
        (row * mempolicy::cache::TINYLFU_SKETCH_WIDTH / 16 + i / 16, (i % 16) as u32 * 4)
    }

    /// One lookup of `key`: its 4 counters up by one (each at most 15); past the reset, every counter halves.
    pub fn add(&mut self, key: u64) {
        let h = mix(key);
        // bounded("const-domain: the sketch's 4 rows")
        for row in 0..4 {
            let (w, sh) = Self::slot(h, row);
            if (self.words[w] >> sh) & 0xF < 0xF {
                self.words[w] += 1 << sh;
            }
        }
        self.adds += 1;
        if self.adds >= mempolicy::cache::TINYLFU_RESET_ADDS {
            self.halve();
        }
    }

    /// TinyLFU's reset: every counter halved.
    fn halve(&mut self) {
        // bounded("const-domain: the sketch's SKETCH_WORDS words, once every TINYLFU_RESET_ADDS adds")
        for w in self.words.iter_mut() {
            *w = (*w >> 1) & 0x7777_7777_7777_7777;
        }
        self.adds /= 2;
    }

    /// `key`'s estimated lookups (the least of its 4 counters, at most 15).
    pub fn estimate(&self, key: u64) -> u32 {
        let h = mix(key);
        (0..4).map(|row| {
            let (w, sh) = Self::slot(h, row);
            ((self.words[w] >> sh) & 0xF) as u32
        })
        .min()
        .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sketch() -> Sketch {
        let words = vec![0u64; SKETCH_WORDS].into_boxed_slice(); // alloc: Cold
        Sketch::new(words, memledger::Charge::open(memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::CondCache)), memledger::Release::Evict, memledger::Kind::Region, memledger::charge::Door::Heap, Sketch::bytes()))
    }

    /// cachepolicy (C5): a key's estimate counts its adds (at most 15), a key never added reads (almost always) 0, and the
    /// reset halves every count.
    #[test]
    fn cachepolicy_tinylfu_sketch_counts_saturates_and_halves() {
        let mut s = sketch();
        for _ in 0..5 {
            s.add(42);
        }
        assert_eq!(s.estimate(42), 5);
        for _ in 0..20 {
            s.add(7);
        }
        assert_eq!(s.estimate(7), 15, "4-bit counters saturate");
        let unseen = (1000..1100u64).filter(|k| s.estimate(*k) != 0).count();
        assert!(unseen <= 2, "keys never added read 0 (count-min: a collision in all 4 rows is rare): {unseen}");
        // keys that differ only in their high bits (a fp key's attno at bit 48, a cond key's words rotated) spread too
        let mut t = sketch();
        // bounded("const-domain: the test's 1 000 keys")
        for k in 0..1000u64 {
            t.add(0x0303_0303_0303_0303 ^ k.rotate_left(48));
        }
        let over = (0..1000u64).filter(|k| t.estimate(0x0303_0303_0303_0303 ^ k.rotate_left(48)) != 1).count();
        assert!(over <= 3, "1 000 keys added once each read 1: {over} read more");
        s.halve();
        assert_eq!((s.estimate(42), s.estimate(7)), (2, 7), "the reset halves every count");
        // the reset comes by itself after TINYLFU_RESET_ADDS adds
        let before = s.adds;
        for k in 0..(mempolicy::cache::TINYLFU_RESET_ADDS - before) {
            s.add(1 << 40 | k);
        }
        assert!(s.adds < mempolicy::cache::TINYLFU_RESET_ADDS / 2 + 1, "halved at the reset: {}", s.adds);
    }
}
