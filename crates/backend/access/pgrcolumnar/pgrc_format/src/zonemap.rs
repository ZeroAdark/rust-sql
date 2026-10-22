//! One zone column's per-granule `[min, max]` in compact form — the facts
//! image's zone map (`facts::ColFacts::zones`, version 2 of the sections
//! file; gravity statement-floor item 1, 2026-09-13). The raw form, two
//! `i64` per granule, was 15.0 of the 15.2 MB of a 100M-row table's facts
//! section (12,207 granules × 77 word columns × 16 B); what a granule bound
//! needs is the column's native width at most, and on a `sorted_by` column
//! the minima are monotone, so the step from one granule to the next is
//! small. Both are one choice per column, made by the encoder from the
//! values alone (no schema in hand): the minima as offsets from a base or
//! as deltas from the previous minimum, whichever fits the narrower word,
//! and the spans `max − min` as offsets, each stream at the narrowest of
//! 1 / 2 / 4 / 8 bytes that holds every value. The map is expanded per
//! column on first use (`decode`); the consumers (the planner's census,
//! the JSON dump) see exactly the ranges that went in.
//!
//! Two granule kinds carry no bound: an all-null granule is the empty
//! range `(i64::MAX, i64::MIN)` (any `lo > hi` reads as it), a granule without an `Exact` key the
//! full range `(i64::MIN, i64::MAX)`. Both are exceptions listed apart
//! (granule index + kind) and hold their stream slots at "the previous
//! regular minimum, span 0", so a run of them costs nothing in width.
//!
//! ```text
//! header (24 B): min_mode u8 (0 = offset from base, 1 = delta from the previous minimum),
//!                min_width u8, span_width u8 (each 1|2|4|8), pad u8, nexcept u32,
//!                base i64, granules u32, pad u32
//! exceptions:    nexcept × (granule u32, kind u8 (1 = empty, 2 = full), pad ×3), granule-ascending
//! minima:        granules × min_width (offsets unsigned, deltas two's complement, wrapping at 8 B)
//! spans:         granules × span_width (unsigned)
//! ```

use std::sync::Arc;

use crate::wire::{put_i64, put_u32, Cur};
use crate::{FormatError, FormatResult};

pub const ZONEMAP_HEADER_LEN: usize = 24;
pub const ZONEMAP_EXCEPT_LEN: usize = 8;
const MODE_OFFSET: u8 = 0;
const MODE_DELTA: u8 = 1;
const EXCEPT_EMPTY: u8 = 1;
const EXCEPT_FULL: u8 = 2;

/// The empty range of an all-null granule.
pub const EMPTY_RANGE: (i64, i64) = (i64::MAX, i64::MIN);
/// The full range of a granule without an exact key.
pub const FULL_RANGE: (i64, i64) = (i64::MIN, i64::MAX);

/// A compact zone map: `granules` ranges, encoded as the module describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoneMap {
    granules: u32,
    bytes: Arc<[u8]>,
}

impl Default for ZoneMap {
    fn default() -> ZoneMap {
        ZoneMap::encode(&[]).expect("an empty zone map is header-sized")
    }
}

/// The narrowest of 1 / 2 / 4 / 8 bytes holding every unsigned value.
fn unsigned_width(max: u64) -> u8 {
    match max {
        0..=0xff => 1,
        0x100..=0xffff => 2,
        0x1_0000..=0xffff_ffff => 4,
        _ => 8,
    }
}

/// The narrowest of 1 / 2 / 4 / 8 bytes holding every signed value.
fn signed_width(v: i128) -> u8 {
    if (i8::MIN as i128..=i8::MAX as i128).contains(&v) {
        1
    } else if (i16::MIN as i128..=i16::MAX as i128).contains(&v) {
        2
    } else if (i32::MIN as i128..=i32::MAX as i128).contains(&v) {
        4
    } else {
        8
    }
}

fn put_word(out: &mut Vec<u8>, v: u64, width: u8) {
    out.extend_from_slice(&v.to_le_bytes()[..width as usize]);
}

fn unsigned_at(b: &[u8], i: usize, width: u8) -> u64 {
    let w = width as usize;
    let mut buf = [0u8; 8];
    buf[..w].copy_from_slice(&b[i * w..i * w + w]);
    u64::from_le_bytes(buf)
}

fn signed_at(b: &[u8], i: usize, width: u8) -> i64 {
    let v = unsigned_at(b, i, width);
    match width {
        1 => v as u8 as i8 as i64,
        2 => v as u16 as i16 as i64,
        4 => v as u32 as i32 as i64,
        _ => v as i64,
    }
}

/// `lo > hi` is the empty range whatever its words (the census law).
fn exception_kind(r: (i64, i64)) -> u8 {
    if r.0 > r.1 {
        EXCEPT_EMPTY
    } else if r == FULL_RANGE {
        EXCEPT_FULL
    } else {
        0
    }
}

impl ZoneMap {
    /// Encode `ranges` (granule order; any `lo > hi` is the empty range).
    pub fn encode(ranges: &[(i64, i64)]) -> FormatResult<ZoneMap> {
        let n = ranges.len();
        let base = ranges.iter().filter(|&&r| exception_kind(r) == 0).map(|r| r.0).min().unwrap_or(0);
        // the regular minimum sequence (exceptions repeat the previous one) and the two widths per mode
        let mut prev = base;
        let (mut off_max, mut delta_w, mut span_max, mut nexcept) = (0u64, 1u8, 0u64, 0u32);
        for &r in ranges {
            let (lo, hi) = r;
            if exception_kind(r) != 0 {
                nexcept += 1;
                continue;
            }
            off_max = off_max.max((lo as i128 - base as i128) as u64);
            delta_w = delta_w.max(signed_width(lo as i128 - prev as i128));
            span_max = span_max.max((hi as i128 - lo as i128) as u64);
            prev = lo;
        }
        let off_w = unsigned_width(off_max);
        let (mode, min_w) = if delta_w < off_w { (MODE_DELTA, delta_w) } else { (MODE_OFFSET, off_w) };
        let span_w = unsigned_width(span_max);
        let mut out = crate::try_vec(ZONEMAP_HEADER_LEN + nexcept as usize * ZONEMAP_EXCEPT_LEN + n * (min_w as usize + span_w as usize), "zonemap")?; // alloc: Cold
        out.extend_from_slice(&[mode, min_w, span_w, 0]);
        put_u32(&mut out, nexcept);
        put_i64(&mut out, base);
        put_u32(&mut out, n as u32);
        put_u32(&mut out, 0);
        for (g, &r) in ranges.iter().enumerate() {
            let k = exception_kind(r);
            if k != 0 {
                put_u32(&mut out, g as u32);
                out.extend_from_slice(&[k, 0, 0, 0]);
            }
        }
        let mut prev = base;
        for &r in ranges {
            let lo = if exception_kind(r) == 0 { r.0 } else { prev };
            let word = match mode {
                MODE_DELTA => lo.wrapping_sub(prev) as u64,
                _ => (lo as i128 - base as i128) as u64,
            };
            put_word(&mut out, word, min_w);
            prev = lo;
        }
        for &r in ranges {
            let span = if exception_kind(r) == 0 { (r.1 as i128 - r.0 as i128) as u64 } else { 0 };
            put_word(&mut out, span, span_w);
        }
        Ok(ZoneMap { granules: n as u32, bytes: out.into() })
    }

    /// A map from its persisted bytes, structure checked (`granules` is
    /// the count the holder expects).
    pub fn from_bytes(bytes: &[u8], granules: u32) -> FormatResult<ZoneMap> {
        let at = "zone map";
        if bytes.len() < ZONEMAP_HEADER_LEN {
            return Err(FormatError::Truncated { at });
        }
        let mut c = Cur::new(bytes);
        let mode = c.u8(at)?;
        let min_w = c.u8(at)?;
        let span_w = c.u8(at)?;
        c.u8(at)?;
        let nexcept = c.u32(at)? as usize;
        c.i64(at)?;
        let n = c.u32(at)?;
        c.u32(at)?;
        if mode > MODE_DELTA || !matches!(min_w, 1 | 2 | 4 | 8) || !matches!(span_w, 1 | 2 | 4 | 8) || n != granules {
            return Err(FormatError::Corrupt { at: "zone map header" });
        }
        let expect = ZONEMAP_HEADER_LEN + nexcept * ZONEMAP_EXCEPT_LEN + n as usize * (min_w as usize + span_w as usize);
        if bytes.len() != expect {
            return Err(FormatError::Corrupt { at: "zone map length" });
        }
        let mut prev_g: Option<u32> = None;
        for _ in 0..nexcept {
            let g = c.u32(at)?;
            let k = c.u8(at)?;
            c.take(3, at)?;
            if g >= n || prev_g.is_some_and(|p| g <= p) || !matches!(k, EXCEPT_EMPTY | EXCEPT_FULL) {
                return Err(FormatError::Corrupt { at: "zone map exceptions" });
            }
            prev_g = Some(g);
        }
        Ok(ZoneMap { granules, bytes: bytes.into() })
    }

    pub fn granules(&self) -> u32 {
        self.granules
    }

    pub fn is_empty(&self) -> bool {
        self.granules == 0
    }

    /// The persisted bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The ranges, granule order — the expansion, once per column per use.
    pub fn decode(&self) -> FormatResult<Vec<(i64, i64)>> {
        let b = &self.bytes;
        let (mode, min_w, span_w) = (b[0], b[1], b[2]);
        let nexcept = u32::from_le_bytes(b[4..8].try_into().expect("len 4")) as usize;
        let base = i64::from_le_bytes(b[8..16].try_into().expect("len 8"));
        let n = self.granules as usize;
        let exc = &b[ZONEMAP_HEADER_LEN..ZONEMAP_HEADER_LEN + nexcept * ZONEMAP_EXCEPT_LEN];
        let mins = &b[ZONEMAP_HEADER_LEN + nexcept * ZONEMAP_EXCEPT_LEN..][..n * min_w as usize];
        let spans = &b[b.len() - n * span_w as usize..];
        let mut out = crate::try_vec(n, "zonemap")?; // alloc: Cold
        let mut prev = base;
        let mut e = 0usize;
        for g in 0..n {
            let lo = match mode {
                MODE_DELTA => prev.wrapping_add(signed_at(mins, g, min_w)),
                _ => (base as i128 + unsigned_at(mins, g, min_w) as i128) as i64,
            };
            prev = lo;
            let next_exc = (e < nexcept).then(|| u32::from_le_bytes(exc[e * ZONEMAP_EXCEPT_LEN..e * ZONEMAP_EXCEPT_LEN + 4].try_into().expect("len 4")) as usize);
            if next_exc == Some(g) {
                out.push(if exc[e * ZONEMAP_EXCEPT_LEN + 4] == EXCEPT_EMPTY { EMPTY_RANGE } else { FULL_RANGE });
                e += 1;
                continue;
            }
            let hi = (lo as i128 + unsigned_at(spans, g, span_w) as i128) as i64;
            out.push((lo, hi));
        }
        Ok(out)
    }
}

/// The rank grid's resolution: quantile points per multiset.
pub const SUMMARY_BUCKETS: usize = 256;

/// The planner's per-column summary of a zone map (statement-floor item 2,
/// 2026-09-13), derived once at catalog load so no plan walks granules: the
/// keyed granule count and the column's extrema, and a shared key grid —
/// the union of the `SUMMARY_BUCKETS + 1` quantile points of the granule
/// minima and of the granule maxima (≤ 514 keys, every key a real bound)
/// — with, at every grid key, the exact number of minima `<` / `≤` it, of
/// maxima `<` / `≤` it, and of granules whose both bounds equal it (what
/// `= key` decides outright). A census for `col op const` is rank queries on
/// the two multisets: exact when the constant is a grid key, interpolated
/// linearly between the enclosing keys otherwise, and between two
/// consecutive grid keys lie fewer than `keyed / SUMMARY_BUCKETS` values of
/// either multiset (`max_gap` is the column's actual figure) — the error
/// bound per count. A column with at most 514 distinct bounds is on the
/// grid entirely and every census exact. The planner's reader is
/// `costsize::pgrc_cost::ZoneSummary` (the same contract, over slices).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ZoneSummary {
    pub granules: u32,
    /// Granules with a bound (the empty range excluded; a full-range granule counts).
    pub keyed: u32,
    /// The column's extrema over the keyed granules (`lo > hi` when none).
    pub lo: i64,
    pub hi: i64,
    /// The shared key grid, ascending, distinct.
    pub keys: Vec<i64>,
    pub min_lt: Vec<u32>,
    pub min_le: Vec<u32>,
    pub max_lt: Vec<u32>,
    pub max_le: Vec<u32>,
    /// Granules with `min = max = key` — the ones `= key` decides outright.
    pub eq_whole: Vec<u32>,
    /// The most values of either multiset strictly between two consecutive grid keys.
    pub max_gap: u32,
}

impl ZoneSummary {
    pub fn build(ranges: &[(i64, i64)]) -> ZoneSummary {
        let mut mins: Vec<i64> = ranges.iter().filter(|(lo, hi)| lo <= hi).map(|r| r.0).collect(); // alloc: Cold
        let mut maxs: Vec<i64> = ranges.iter().filter(|(lo, hi)| lo <= hi).map(|r| r.1).collect(); // alloc: Cold
        mins.sort_unstable();
        maxs.sort_unstable();
        let n = mins.len();
        let mut keys: Vec<i64> = Vec::with_capacity(2 * (SUMMARY_BUCKETS + 1)); // alloc: Cold
        if n > 0 {
            for i in 0..=SUMMARY_BUCKETS {
                let r = (i * n / SUMMARY_BUCKETS).min(n - 1);
                keys.push(mins[r]);
                keys.push(maxs[r]);
            }
        }
        keys.sort_unstable();
        keys.dedup();
        let rank = |sorted: &[i64], k: i64, le: bool| sorted.partition_point(|&v| if le { v <= k } else { v < k }) as u32;
        let min_lt: Vec<u32> = keys.iter().map(|&k| rank(&mins, k, false)).collect(); // alloc: Cold
        let min_le: Vec<u32> = keys.iter().map(|&k| rank(&mins, k, true)).collect(); // alloc: Cold
        let max_lt: Vec<u32> = keys.iter().map(|&k| rank(&maxs, k, false)).collect(); // alloc: Cold
        let max_le: Vec<u32> = keys.iter().map(|&k| rank(&maxs, k, true)).collect(); // alloc: Cold
        let mut wholes: Vec<i64> = ranges.iter().filter(|(lo, hi)| lo == hi).map(|r| r.0).collect(); // alloc: Cold
        wholes.sort_unstable();
        let eq_whole: Vec<u32> = keys.iter().map(|&k| rank(&wholes, k, true) - rank(&wholes, k, false)).collect(); // alloc: Cold
        let mut max_gap = 0u32;
        for i in 1..keys.len() {
            max_gap = max_gap.max(min_lt[i] - min_le[i - 1]).max(max_lt[i] - max_le[i - 1]);
        }
        ZoneSummary {
            granules: ranges.len() as u32,
            keyed: n as u32,
            lo: mins.first().copied().unwrap_or(i64::MAX),
            hi: maxs.last().copied().unwrap_or(i64::MIN),
            keys,
            min_lt,
            min_le,
            max_lt,
            max_le,
            eq_whole,
            max_gap,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(ranges: &[(i64, i64)]) -> ZoneMap {
        let m = ZoneMap::encode(ranges).unwrap();
        assert_eq!(m.decode().unwrap(), ranges);
        assert_eq!(ZoneMap::from_bytes(m.bytes(), ranges.len() as u32).expect("parse"), m);
        m
    }

    #[test]
    fn monotone_minima_delta_code_to_one_byte_and_random_words_stay_native() {
        // a sorted_by int4 column: minima climb slowly, spans are small
        let sorted: Vec<(i64, i64)> = (0..1000).map(|g| (1_000_000 + g * 3, 1_000_000 + g * 3 + 2)).collect();
        let m = round_trip(&sorted);
        assert_eq!((m.bytes()[0], m.bytes()[1], m.bytes()[2]), (MODE_DELTA, 1, 1));
        assert_eq!(m.bytes().len(), ZONEMAP_HEADER_LEN + 1000 * 2);
        // an int2 column with a full-domain span per granule: 2 + 2 bytes, never more than the native width
        let small: Vec<(i64, i64)> = (0..100).map(|g| (-32768 + g % 7, 32767 - g % 5)).collect();
        let m = round_trip(&small);
        assert_eq!((m.bytes()[1], m.bytes()[2]), (1, 2));
        // a random int8 column: 8 + 8 bytes
        let wide = [(i64::MIN + 1, i64::MAX - 1), (-3, i64::MAX / 2), (i64::MIN / 2, 7)];
        let m = round_trip(&wide);
        assert_eq!((m.bytes()[0], m.bytes()[1], m.bytes()[2]), (MODE_OFFSET, 8, 8));
    }

    #[test]
    fn exceptions_hold_their_slots_and_extremes_survive() {
        let ranges = [EMPTY_RANGE, (5, 9), FULL_RANGE, EMPTY_RANGE, (6, 6), (i64::MIN, i64::MIN), (i64::MAX, i64::MAX), FULL_RANGE];
        let m = round_trip(&ranges);
        assert_eq!(u32::from_le_bytes(m.bytes()[4..8].try_into().unwrap()), 4);
        round_trip(&[]);
        round_trip(&[EMPTY_RANGE]);
        round_trip(&[FULL_RANGE, FULL_RANGE]);
        round_trip(&[(i64::MIN, i64::MAX - 1), (i64::MAX - 1, i64::MAX - 1), (i64::MIN, i64::MIN)]);
        assert_eq!(ZoneMap::default().decode().unwrap(), Vec::<(i64, i64)>::new());
    }

    #[test]
    fn delta_wraps_at_eight_bytes_and_refusals_are_typed() {
        // consecutive minima 2^63 apart: the delta overflows i64 and wraps; offsets fit u64
        let ranges = [(i64::MIN, i64::MIN), (i64::MAX, i64::MAX), (i64::MIN, i64::MAX), (0, 0)];
        round_trip(&ranges);
        let m = ZoneMap::encode(&[(1, 2), (3, 4)]).unwrap();
        assert!(matches!(ZoneMap::from_bytes(m.bytes(), 3), Err(FormatError::Corrupt { at: "zone map header" })));
        assert!(matches!(ZoneMap::from_bytes(&m.bytes()[..m.bytes().len() - 1], 2), Err(FormatError::Corrupt { at: "zone map length" })));
        assert!(matches!(ZoneMap::from_bytes(&m.bytes()[..8], 2), Err(FormatError::Truncated { .. })));
        let mut bad = m.bytes().to_vec();
        bad[1] = 3;
        assert!(matches!(ZoneMap::from_bytes(&bad, 2), Err(FormatError::Corrupt { at: "zone map header" })));
        let m = ZoneMap::encode(&[(1, 2), EMPTY_RANGE, (3, 4)]).unwrap();
        let mut bad = m.bytes().to_vec();
        bad[ZONEMAP_HEADER_LEN + 4] = 9;
        assert!(matches!(ZoneMap::from_bytes(&bad, 3), Err(FormatError::Corrupt { at: "zone map exceptions" })));
    }

    #[test]
    fn a_pseudo_random_mix_round_trips_at_every_width() {
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for shift in [0u32, 8, 16, 24, 32, 40, 56, 63] {
            let ranges: Vec<(i64, i64)> = (0..500)
                .map(|g| {
                    let r = next();
                    match r % 13 {
                        0 => EMPTY_RANGE,
                        1 => FULL_RANGE,
                        _ => {
                            let lo = ((next() >> shift) as i64).wrapping_sub(g);
                            let span = (next() >> (shift + 1).min(63)) as i64;
                            (lo, lo.saturating_add(span.max(0)))
                        }
                    }
                })
                .collect();
            round_trip(&ranges);
        }
    }
}

#[cfg(test)]
mod summary_tests {
    use super::*;

    fn check(ranges: &[(i64, i64)]) -> ZoneSummary {
        let s = ZoneSummary::build(ranges);
        let keyed: Vec<&(i64, i64)> = ranges.iter().filter(|(lo, hi)| lo <= hi).collect();
        assert_eq!(s.keyed as usize, keyed.len());
        assert_eq!(s.granules as usize, ranges.len());
        assert!(s.keys.len() <= 2 * (SUMMARY_BUCKETS + 1));
        assert!(s.keys.windows(2).all(|w| w[0] < w[1]), "ascending, distinct");
        for (i, &k) in s.keys.iter().enumerate() {
            assert!(keyed.iter().any(|r| r.0 == k || r.1 == k), "every grid key is a real bound");
            assert_eq!(s.min_lt[i] as usize, keyed.iter().filter(|r| r.0 < k).count());
            assert_eq!(s.min_le[i] as usize, keyed.iter().filter(|r| r.0 <= k).count());
            assert_eq!(s.max_lt[i] as usize, keyed.iter().filter(|r| r.1 < k).count());
            assert_eq!(s.max_le[i] as usize, keyed.iter().filter(|r| r.1 <= k).count());
            assert_eq!(s.eq_whole[i] as usize, keyed.iter().filter(|r| r.0 == k && r.1 == k).count());
        }
        if !keyed.is_empty() {
            assert_eq!((s.lo, s.hi), (keyed.iter().map(|r| r.0).min().unwrap(), keyed.iter().map(|r| r.1).max().unwrap()));
            assert!((s.max_gap as usize) < keyed.len().div_ceil(SUMMARY_BUCKETS), "gap {} of {}", s.max_gap, keyed.len());
        }
        s
    }

    #[test]
    fn the_grid_counts_are_exact_and_the_gaps_bounded() {
        // the tuned hits shape: contiguous slices, two granules per key, one empty granule
        let mut sorted: Vec<(i64, i64)> = (0..12_207i64).map(|g| (17 + g / 2, 17 + (g + 1) / 2)).collect();
        sorted.push(EMPTY_RANGE);
        let s = check(&sorted);
        assert_eq!(s.keyed, 12_207);
        assert!(s.max_gap <= 47, "12,207 / 256: {}", s.max_gap);
        // every granule spanning the column: one key each side, every census exact
        let flat = vec![(17i64, 262_115i64); 500];
        let s = check(&flat);
        assert_eq!(s.keys, vec![17, 262_115]);
        assert_eq!(s.max_gap, 0);
        // few distinct bounds: the whole domain is on the grid
        let dates: Vec<(i64, i64)> = (0..3000i64).map(|g| (15_888 + g % 7, 15_888 + (g % 7).max(g % 11 % 7))).collect();
        let s = check(&dates);
        assert!(s.keys.len() <= 7);
        // pseudo-random int8 bounds: every value distinct, the gap bound holds
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let wide: Vec<(i64, i64)> = (0..4096)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let lo = (x >> 1) as i64 - (1 << 62);
                (lo, lo.saturating_add((x % 1_000_000) as i64))
            })
            .collect();
        let s = check(&wide);
        assert!(s.max_gap <= 16, "4096 / 256: {}", s.max_gap);
        assert_eq!(check(&[]).keys.len(), 0);
        assert_eq!(check(&[EMPTY_RANGE, EMPTY_RANGE]).keyed, 0);
        assert_eq!(check(&[FULL_RANGE]).keys, vec![i64::MIN, i64::MAX]);
    }
}
