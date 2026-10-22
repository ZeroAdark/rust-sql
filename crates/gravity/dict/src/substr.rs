//! Substring search for `LIKE '%needle%'` over many entries against one needle (the rare-byte-pair
//! SIMD scan — memchr's `PackedPair`, Muła's "generic SIMD"): a byte-frequency rank picks the needle's two rarest bytes at offsets `o1`, `o2` (the
//! static English / URL `RANK`, or counts of the data's own bytes, `RarePair::sampled`); each
//! 16-byte block of the haystack is compared against the two bytes broadcast (loads
//! at `p + o1` and `p + o2`), the masks ANDed, and the full needle verified by `memcmp` only
//! where a bit survives. NEON on aarch64, SSE2 on x86_64, a scalar loop elsewhere.

/// Byte commonness in English text and URLs, 0 = most common (letters by English frequency, URL
/// punctuation and digits common, controls and non-ASCII bytes rare).
pub static RANK: [u8; 256] = build_rank();

const fn build_rank() -> [u8; 256] {
    let mut r = [200u8; 256];
    let common: &[u8] = b" etaoinsrhldcumfpgwybvkxjqz";
    let mut i = 0;
    while i < common.len() {
        r[common[i] as usize] = (i * 4) as u8;
        r[common[i].to_ascii_uppercase() as usize] = (i * 4 + 60) as u8;
        i += 1;
    }
    let punct: &[u8] = b"/.-_=&?:%0123456789+#@,;~()[]!'\"*$<>\\^`{|}";
    i = 0;
    while i < punct.len() {
        r[punct[i] as usize] = (8 + i * 3) as u8;
        i += 1;
    }
    r[b' ' as usize] = 0;
    r[b'e' as usize] = 1;
    i = 0;
    while i < 32 {
        r[i] = 250;
        i += 1;
    }
    r[127] = 250;
    i = 128;
    while i < 256 {
        r[i] = 230;
        i += 1;
    }
    r
}

/// The scan for one needle: the two rare offsets and their bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RarePair {
    o1: usize,
    o2: usize,
    b1: u8,
    b2: u8,
}

/// Adds each byte of `hay` to its count (dict worklist item 18, B2-01: the bytes a scan already read, counted when its
/// re-check rule says counting pays, `cost_profile::scan`).
pub fn count_bytes(hay: &[u8], counts: &mut [u64; 256]) {
    hay.iter().for_each(|&b| counts[b as usize] += 1);
}

/// What `RarePair::find_rechecking` stopped at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// The needle's first occurrence at or after `from`.
    Hit(usize),
    /// The re-check rule fired with the block at this position next: no occurrence before it.
    Recheck(usize),
    /// No occurrence at or after `from`.
    End,
}

impl RarePair {
    /// `None` for a needle shorter than two bytes (`memchr` is the single-byte scan). The pair by the static `RANK`.
    pub fn new(needle: &[u8]) -> Option<RarePair> {
        Self::by(needle, |b| RANK[b as usize] as u64)
    }

    /// The pair by the bytes' counts in data like the haystack it will scan (`count_bytes`): the needle's two least
    /// frequent bytes there, the static `RANK` breaking a tie (a byte the counts never saw is rarest).
    pub fn sampled(needle: &[u8], counts: &[u64; 256]) -> Option<RarePair> {
        Self::by(needle, |b| (u64::from(u32::MAX) - counts[b as usize].min(u64::from(u32::MAX))) << 8 | RANK[b as usize] as u64)
    }

    /// The needle's two rarest offsets by `rarity` (greater is rarer), the first of equals kept.
    fn by(needle: &[u8], rarity: impl Fn(u8) -> u64) -> Option<RarePair> {
        if needle.len() < 2 {
            return None;
        }
        let mut o1 = 0usize;
        for (i, &b) in needle.iter().enumerate() {
            if rarity(b) > rarity(needle[o1]) {
                o1 = i;
            }
        }
        let mut o2 = if o1 == 0 { 1 } else { 0 };
        for (i, &b) in needle.iter().enumerate() {
            if i != o1 && rarity(b) > rarity(needle[o2]) {
                o2 = i;
            }
        }
        Some(RarePair { o1, o2, b1: needle[o1], b2: needle[o2] })
    }

    /// `find`, counting the candidates the needle compare rejects into `rejected` and asking `pays(position, rejected)`
    /// after each: `Recheck` at the first block where it answers true (the caller may re-rank the pair and resume there).
    pub fn find_rechecking(&self, needle: &[u8], hay: &[u8], from: usize, rejected: &mut u64, pays: impl Fn(usize, u64) -> bool) -> Step {
        if hay.len() < needle.len() || from + needle.len() > hay.len() {
            return Step::End;
        }
        let last = hay.len() - needle.len();
        let omax = self.o1.max(self.o2);
        let mut i = from;
        #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
        while i + omax + 16 <= hay.len() {
            // SAFETY: `i + o + 16 <= hay.len()` for both offsets: the two loads stay inside `hay`.
            let mut m = unsafe { block_mask(hay.as_ptr().add(i + self.o1), hay.as_ptr().add(i + self.o2), self.b1, self.b2) };
            let mut miss = false;
            while m != 0 {
                let j = i + lane(m);
                if j <= last && hay[j..j + needle.len()] == *needle {
                    return Step::Hit(j);
                }
                *rejected += 1;
                miss = true;
                m &= m - 1;
            }
            i += 16;
            if miss && pays(i, *rejected) {
                return Step::Recheck(i);
            }
        }
        match self.find(needle, hay, i) {
            Some(j) => Step::Hit(j),
            None => Step::End,
        }
    }

    pub fn offsets(&self) -> (usize, usize) {
        (self.o1, self.o2)
    }

    /// The first occurrence of `needle` (the one this pair was built for) in `hay` at or after `from`.
    #[inline]
    pub fn find(&self, needle: &[u8], hay: &[u8], from: usize) -> Option<usize> {
        if hay.len() < needle.len() || from + needle.len() > hay.len() {
            return None;
        }
        let last = hay.len() - needle.len();
        let omax = self.o1.max(self.o2);
        let mut i = from;
        #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
        while i + omax + 16 <= hay.len() {
            // SAFETY: `i + o + 16 <= hay.len()` for both offsets: the two loads stay inside `hay`.
            let mut m = unsafe { block_mask(hay.as_ptr().add(i + self.o1), hay.as_ptr().add(i + self.o2), self.b1, self.b2) };
            while m != 0 {
                let j = i + lane(m);
                if j <= last && hay[j..j + needle.len()] == *needle {
                    return Some(j);
                }
                m &= m - 1;
            }
            i += 16;
        }
        while i <= last {
            if hay[i + self.o1] == self.b1 && hay[i + self.o2] == self.b2 && hay[i..i + needle.len()] == *needle {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    /// `find` from the start of a short haystack (one dictionary entry), counting into `rejected` the candidates the
    /// needle compare rejects (the per-entry build's re-check, `like.rs`).
    #[inline]
    pub fn find_counting(&self, needle: &[u8], hay: &[u8], rejected: &mut u64) -> Option<usize> {
        let from = 0usize;
        if hay.len() < needle.len() || from + needle.len() > hay.len() {
            return None;
        }
        let last = hay.len() - needle.len();
        let omax = self.o1.max(self.o2);
        let mut i = from;
        #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
        while i + omax + 16 <= hay.len() {
            // SAFETY: `i + o + 16 <= hay.len()` for both offsets: the two loads stay inside `hay`.
            let mut m = unsafe { block_mask(hay.as_ptr().add(i + self.o1), hay.as_ptr().add(i + self.o2), self.b1, self.b2) };
            while m != 0 {
                let j = i + lane(m);
                if j <= last && hay[j..j + needle.len()] == *needle {
                    return Some(j);
                }
                *rejected += 1;
                m &= m - 1;
            }
            i += 16;
        }
        while i <= last {
            if hay[i + self.o1] == self.b1 && hay[i + self.o2] == self.b2 {
                if hay[i..i + needle.len()] == *needle {
                    return Some(i);
                }
                *rejected += 1;
            }
            i += 1;
        }
        None
    }

    #[inline]
    pub fn contains(&self, needle: &[u8], hay: &[u8]) -> bool {
        self.find(needle, hay, 0).is_some()
    }
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn lane(m: u64) -> usize {
    (m.trailing_zeros() >> 2) as usize
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn lane(m: u64) -> usize {
    m.trailing_zeros() as usize
}

/// The candidate lanes of one block, `p1[k] == b1 && p2[k] == b2` (aarch64: bit `4k` per lane, the `vshrn` movemask).
/// # Safety — 16 bytes readable at `p1` and at `p2`.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn block_mask(p1: *const u8, p2: *const u8, b1: u8, b2: u8) -> u64 {
    use core::arch::aarch64::*;
    let m = vandq_u8(vceqq_u8(vld1q_u8(p1), vdupq_n_u8(b1)), vceqq_u8(vld1q_u8(p2), vdupq_n_u8(b2)));
    let nib = vshrn_n_u16(vreinterpretq_u16_u8(m), 4);
    vget_lane_u64(vreinterpret_u64_u8(nib), 0) & 0x1111_1111_1111_1111
}

/// # Safety — 16 bytes readable at `p1` and at `p2`.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn block_mask(p1: *const u8, p2: *const u8, b1: u8, b2: u8) -> u64 {
    use core::arch::x86_64::*;
    let m = _mm_and_si128(_mm_cmpeq_epi8(_mm_loadu_si128(p1 as *const __m128i), _mm_set1_epi8(b1 as i8)), _mm_cmpeq_epi8(_mm_loadu_si128(p2 as *const __m128i), _mm_set1_epi8(b2 as i8)));
    _mm_movemask_epi8(m) as u32 as u64
}
