//! ST-2: the columnar ANALYZE fold — pg_statistic-shaped output folded
//! from seal-built facts (footer StatsRecords + the Stats sidecar
//! sketches), NO sampling reads (OD-2: the fold covers the pg_statistic
//! families; the sampling path stays designed-but-idle with no consumer).
//!
//! Exactness classes, stated per family (the ST-1 law):
//! - `stanullfrac` / `stawidth`: EXACT (footer part records).
//! - MCV: per-part counts EXACT; the cross-part fold sums counts by value
//!   — a value absent from one part's top-k contributes its OTHER parts'
//!   counts only, so folded frequencies are LOWER BOUNDS (exact for
//!   single-part tables and for values in every part's top-k). FOLDSTATS:
//!   PG's admission (compute_scalar_stats): when every part's top-k is its
//!   whole distinct set and the merged set fits the list, every value is
//!   kept and no histogram is served; otherwise a value is kept only if its
//!   count is at least 2 and 1.25x the average (a unique column has none).
//! - histogram bounds: per-part EXACT equi-depth in the column's value
//!   order ([`HistOrder`], FOLDSTATS: the word families' LE bytes are not in
//!   value order); the fold merges the parts' bounds as a weighted CDF (each
//!   bound stands for an equal share of its part's rows) in that order and
//!   takes up to [`FOLD_HIST_BOUNDS`] equi-depth bounds from it —
//!   approximate across parts, exact for one part.
//! - `stadistinct`: the caller supplies the NDV estimate (the footer HLL
//!   max-union); FOLDSTATS: exact when every part's top-k is its whole
//!   distinct set (the merged set's size), or when every part's values are
//!   distinct and the parts' exact key ranges are disjoint (the rows);
//!   otherwise the estimate within the parts' exact bounds (the largest
//!   part's distinct count .. their sum).
//! - `stacorrelation` (P-8): cluster-key columns fold to ±1.0 from the
//!   FT-6 witness; otherwise derived from the P-1 sortedness bytes
//!   (all-Ascending ⇒ 1.0, all-Descending ⇒ -1.0, else 0.0). Never
//!   default-0 on data the format KNOWS is clustered. FOLDSTATS: across
//!   parts only when their key ranges follow the parts' order (each part's
//!   range at or past the previous one's), which is what makes the table
//!   sorted, not just each part.
//!
//! The never-analyzed refusal law (ST-2): banked data always has these
//! inputs (stats are a seal byproduct), so a consumer holding a sealed
//! part can ALWAYS fold — the refusal path is unreachable on banked data
//! by construction, which is exactly what `M3.estimate-probe.<rung>`
//! witnesses (with a born-RED stats-stripped seed).

use crate::format::meta::{KeyKind, Sortedness, StatsRecord};
use crate::format::sidecar::{ColDistribution, STATS_MCV_K};
use crate::kept::{collect_on_n, On};
pub use crate::sketch::HistOrder;
use crate::sketch::or_skipped;
use pgrc_format::FormatResult;

/// FOLDSTATS: the most histogram bounds a fold serves (PG's for the default statistics target 100).
pub const FOLD_HIST_BOUNDS: usize = 101;

/// One part's fold inputs for one column.
pub struct PartColInput<'a> {
    /// The part-grain StatsRecord (footer Stats section, part row).
    pub part_record: &'a StatsRecord,
    /// Rows in the part (footer fact).
    pub rows: u64,
    /// The column's Stats-sidecar sketch, if the profile computed one.
    pub sketch: Option<&'a ColDistribution>,
}

/// pg_statistic-shaped output for one column (the R1-class seam currency;
/// values are canonical bytes — the AM layer renders them into datums).
#[derive(Debug, Clone, PartialEq)]
pub struct PgStatColumn {
    pub stanullfrac: f64,
    /// Average value width in bytes (pg_statistic stawidth).
    pub stawidth: f64,
    /// Distinct estimate — POSITIVE absolute count (callers may convert to
    /// the pg_statistic negative-fraction form).
    pub stadistinct: f64,
    /// (canonical value bytes, frequency), MCV form.
    pub sta_mcv: Vec<(Vec<u8>, f64)>,
    /// Histogram bound values (canonical bytes, ascending).
    pub sta_histogram: Vec<Vec<u8>>,
    pub stacorrelation: f64,
}

/// Fold one column across its parts. `ndv_estimate` is the footer-HLL
/// union (never re-derived here); `cluster_declared` is the FT-6 witness;
/// `order` the column's value order (the sketches' and the histogram's).
pub fn fold_pg_statistic(
    parts: &[PartColInput<'_>],
    ndv_estimate: f64,
    cluster_declared: bool,
    order: HistOrder,
) -> Option<PgStatColumn> {
    fold_pg_statistic_for(parts, ndv_estimate, cluster_declared, order, None)
}

/// Wave 3 (cvformat): [`fold_pg_statistic`] with its per-column working lists (every part's MCV entries, every part's
/// histogram bounds: parts x list length) in `ChargedVec`s on `on` (the ANALYZE statement; `None`: uncharged). The
/// output is [`fold_pg_statistic`]'s: the MCV merge is a sort by value with equal values summed (what the ordered map
/// did), then the same total order by count and value.
pub fn fold_pg_statistic_for(
    parts: &[PartColInput<'_>],
    ndv_estimate: f64,
    cluster_declared: bool,
    order: HistOrder,
    on: On,
) -> Option<PgStatColumn> {
    if parts.is_empty() {
        return None;
    }
    let total_rows: u64 = parts.iter().map(|p| p.rows).sum();
    let total_nonnull: u64 = parts.iter().map(|p| p.part_record.nonnull as u64).sum();
    if total_rows == 0 {
        return None;
    }
    let stanullfrac = 1.0 - (total_nonnull as f64 / total_rows as f64);
    let byte_sum: u64 = parts.iter().map(|p| p.part_record.byte_len_sum).sum();
    let stawidth = if total_nonnull > 0 && byte_sum > 0 {
        byte_sum as f64 / total_nonnull as f64
    } else {
        // Word classes carry no byte_len stats; the record's key width
        // class is the caller's to render. 0.0 = "not supplied here".
        0.0
    };

    // MCV: sum exact counts by value across parts (lower-bound law above): every part's entries sorted by value,
    // equal values summed in place (wave 3: the working list on `on`; what the ordered map did).
    let n: usize = parts.iter().filter_map(|p| p.sketch).map(|s| s.mcv.len()).sum();
    let mut by_count = or_skipped(collect_on_n(
        on,
        n,
        parts.iter().filter_map(|p| p.sketch).flat_map(|s| s.mcv.iter().map(|(v, c)| (v.as_slice(), *c))),
    ))?;
    by_count.as_mut_slice().sort_unstable_by(|a, b| a.0.cmp(b.0));
    // dedup_by keeps the first of a run and hands it each later equal entry: the run's counts summed into it.
    by_count.with_vec(|v| {
        v.dedup_by(|later, kept| {
            let same = later.0 == kept.0;
            if same {
                kept.1 += later.1;
            }
            same
        })
    });
    // Every part's top-k is its whole eligible distinct set: the merged counts are the column's exact ones.
    let sketches: Option<Vec<&ColDistribution>> = parts.iter().filter(|p| p.part_record.nonnull > 0).map(|p| p.sketch).collect();
    let complete = sketches
        .as_ref()
        .is_some_and(|v| v.iter().all(|s| s.long_values == 0 && s.mcv.len() as u64 >= s.ndv_eligible));

    let stadistinct = fold_ndv(parts, sketches.as_deref(), complete, by_count.len(), ndv_estimate);

    // Values are distinct now, so (count desc, value asc) is a total order.
    by_count.as_mut_slice().sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    // compute_scalar_stats' admission: all values fit the list, or a value 1.25x as common as the average, twice
    // at least
    let all_fit = complete && by_count.len() <= STATS_MCV_K;
    if !all_fit {
        let avg = if stadistinct > 0.0 { total_nonnull as f64 / stadistinct } else { f64::INFINITY };
        let min_count = (1.25 * avg).max(2.0);
        by_count.with_vec(|v| v.retain(|&(_, c)| c as f64 >= min_count));
    }
    let mut sta_mcv: Vec<(Vec<u8>, f64)> = or_skipped(pgrc_format::try_vec(by_count.len().min(STATS_MCV_K), "fold"))?;
    for &(v, c) in by_count.iter().take(STATS_MCV_K) {
        sta_mcv.push((or_skipped(pgrc_format::try_to_vec(v, "fold"))?, c as f64 / total_rows as f64));
    }

    // Histogram: none when the list holds every value (compute_scalar_stats); else the parts' bounds merged in value
    // order, weighted by the part's rows.
    let sta_histogram = if all_fit && !sta_mcv.is_empty() { Vec::new() } else { or_skipped(merge_histograms(parts, order))? };

    // P-8 correlation.
    let desc = parts
        .iter()
        .all(|p| p.part_record.sortedness == Sortedness::Descending.as_u8());
    let stacorrelation = if cluster_declared {
        if desc && ranges_follow(parts, false) {
            -1.0
        } else if !desc && ranges_follow(parts, true) {
            1.0
        } else {
            0.0
        }
    } else {
        let all = |s: Sortedness| {
            parts
                .iter()
                .all(|p| p.part_record.sortedness == s.as_u8())
        };
        if (all(Sortedness::Ascending) || all(Sortedness::Constant)) && ranges_follow(parts, true) {
            1.0
        } else if all(Sortedness::Descending) && ranges_follow(parts, false) {
            -1.0
        } else {
            0.0
        }
    };

    Some(PgStatColumn {
        stanullfrac,
        stawidth,
        stadistinct,
        sta_mcv,
        sta_histogram,
        stacorrelation,
    })
}

/// The parts with values, in part order, when every one's record carries an order key (`None`: some does not).
fn keyed(parts: &[PartColInput<'_>]) -> Option<Vec<(i64, i64)>> {
    parts
        .iter()
        .filter(|p| p.part_record.nonnull > 0)
        .map(|p| (p.part_record.key_kind != KeyKind::Absent as u8).then_some((p.part_record.min_key, p.part_record.max_key)))
        .collect()
}

/// Each part's key range at or past (`ascending`; at or before otherwise) the previous part's, in part order: one
/// part always.
fn ranges_follow(parts: &[PartColInput<'_>], ascending: bool) -> bool {
    if parts.iter().filter(|p| p.part_record.nonnull > 0).count() <= 1 {
        return true;
    }
    keyed(parts).is_some_and(|r| r.windows(2).all(|w| if ascending { w[0].1 <= w[1].0 } else { w[0].0 >= w[1].1 }))
}

/// `stadistinct` (a positive count): see the module doc.
fn fold_ndv(parts: &[PartColInput<'_>], sketches: Option<&[&ColDistribution]>, complete: bool, merged: usize, estimate: f64) -> f64 {
    let Some(v) = sketches else { return estimate };
    if v.is_empty() {
        return estimate;
    }
    if complete {
        return merged as f64;
    }
    let lo = v.iter().map(|s| s.ndv_eligible).max().unwrap_or(0) as f64;
    let hi = v.iter().map(|s| s.ndv_eligible + s.long_values).sum::<u64>() as f64;
    // every part's values distinct, and no value in two parts: exact keys, disjoint ranges
    let unique = v.iter().all(|s| s.long_values == 0 && s.ndv_eligible == s.nonnull);
    let exact = parts.iter().filter(|p| p.part_record.nonnull > 0).all(|p| p.part_record.key_kind == KeyKind::Exact as u8);
    if unique && exact {
        if let Some(mut r) = keyed(parts) {
            r.sort_unstable();
            if r.windows(2).all(|w| w[0].1 < w[1].0) {
                return hi;
            }
        }
    }
    estimate.clamp(lo, hi.max(lo))
}

/// The parts' equi-depth bounds merged in `order` into one table-wide CDF, and the histogram read off it.
///
/// A part's m bounds cut its eligible rows into m - 1 equal shares, each spread evenly between two adjacent bounds (a
/// share between equal bounds is a point mass: a value that spans ranks). The parts' shares sum into one CDF,
/// piecewise linear between the bounds' positions, with jumps at the point masses. For the word orders a position is
/// the value and the histogram is [`FOLD_HIST_BOUNDS`] equi-depth bounds interpolated off the CDF, as values of the
/// column's width (a sampled ANALYZE's bounds are the same quantiles). For byte order, whose values cannot be
/// interpolated, a position is the first 8 bytes read as a big-endian number, and each bound is the first input bound
/// at which the CDF reaches its target. Either way a quantile inside a point mass lands on that value once: a value
/// that heavy is an MCV, which the histogram leaves out, as PG's does.
fn merge_histograms(parts: &[PartColInput<'_>], order: HistOrder) -> FormatResult<Vec<Vec<u8>>> {
    // (lo, hi, mass): one share of a part's rows
    let mut shares: Vec<(&[u8], &[u8], f64)> = Vec::new();
    for s in parts.iter().filter_map(|p| p.sketch) {
        let m = s.hist_bounds.len();
        let w = s.nonnull.saturating_sub(s.long_values) as f64;
        if m == 0 || w <= 0.0 {
            continue;
        }
        if m == 1 {
            shares.push((&s.hist_bounds[0], &s.hist_bounds[0], w));
            continue;
        }
        let step = w / (m - 1) as f64;
        for b in s.hist_bounds.windows(2) {
            shares.push((&b[0], &b[1], step));
        }
    }
    if shares.is_empty() {
        return Ok(Vec::new());
    }
    let words = Words::of(order, shares.iter().flat_map(|s| [s.0, s.1]));
    let pos = |v: &[u8]| match &words {
        Some(w) => w.pos(v),
        None => prefix_pos(v),
    };

    // knots: the distinct bound values in order, each with the jumps at it and the slope changes there
    struct Knot<'a> {
        v: &'a [u8],
        at: f64,
        jump: f64,
        dslope: f64,
    }
    let mut knots: Vec<Knot<'_>> = pgrc_format::try_vec(shares.len() * 2, "fold")?;
    for &(lo, hi, mass) in &shares {
        let (a, b) = (pos(lo), pos(hi));
        if b > a {
            knots.push(Knot { v: lo, at: a, jump: 0.0, dslope: mass / (b - a) });
            knots.push(Knot { v: hi, at: b, jump: 0.0, dslope: -mass / (b - a) });
        } else if order.same(lo, hi) {
            knots.push(Knot { v: lo, at: a, jump: mass, dslope: 0.0 });
        } else {
            // distinct values at one position (a long common prefix): half the share at each
            knots.push(Knot { v: lo, at: a, jump: mass / 2.0, dslope: 0.0 });
            knots.push(Knot { v: hi, at: b, jump: mass / 2.0, dslope: 0.0 });
        }
    }
    knots.sort_by(|x, y| x.at.total_cmp(&y.at).then_with(|| order.cmp(x.v, y.v)));
    let mut merged: Vec<Knot<'_>> = pgrc_format::try_vec(knots.len(), "fold")?;
    for k in knots {
        match merged.last_mut() {
            Some(l) if order.same(l.v, k.v) => {
                l.jump += k.jump;
                l.dslope += k.dslope;
            }
            _ => merged.push(k),
        }
    }
    // F just before (lin) and just after (jumps included) each knot
    let mut before = pgrc_format::try_vec(merged.len(), "fold")?;
    let mut after = pgrc_format::try_vec(merged.len(), "fold")?;
    let (mut f, mut slope, mut prev) = (0.0f64, 0.0f64, merged[0].at);
    for k in &merged {
        f += slope * (k.at - prev);
        before.push(f);
        f += k.jump;
        after.push(f);
        slope = (slope + k.dslope).max(0.0);
        prev = k.at;
    }
    let total = f;
    if total <= 0.0 {
        return Ok(Vec::new());
    }
    let last = merged.len() - 1;
    let n = match &words {
        Some(_) => FOLD_HIST_BOUNDS,
        None => merged.len().min(FOLD_HIST_BOUNDS),
    };
    if n < 2 || last == 0 {
        return Ok(vec![pgrc_format::try_to_vec(merged[0].v, "fold")?]);
    }
    let mut out: Vec<Vec<u8>> = pgrc_format::try_vec(n, "fold")?;
    let mut k = 0usize;
    for j in 0..n {
        let v: Vec<u8> = if j == 0 {
            pgrc_format::try_to_vec(merged[0].v, "fold")?
        } else if j == n - 1 {
            pgrc_format::try_to_vec(merged[last].v, "fold")?
        } else {
            let t = total * j as f64 / (n - 1) as f64;
            while k < last && after[k] < t {
                k += 1;
            }
            match &words {
                // on the rise into knot k: interpolate from knot k-1
                Some(w) if k > 0 && t < before[k] && before[k] > after[k - 1] => {
                    let frac = (t - after[k - 1]) / (before[k] - after[k - 1]);
                    w.between(merged[k - 1].v, merged[k].v, frac)?
                }
                _ => pgrc_format::try_to_vec(merged[k].v, "fold")?,
            }
        };
        if out.last().is_none_or(|l| order.cmp(l, &v) == std::cmp::Ordering::Less) {
            out.push(v);
        }
    }
    Ok(out)
}

/// A byte-ordered value's histogram position: its first 8 bytes, big-endian (monotone in memcmp order).
fn prefix_pos(v: &[u8]) -> f64 {
    let mut b = [0u8; 8];
    let n = v.len().min(8);
    b[..n].copy_from_slice(&v[..n]);
    u64::from_be_bytes(b) as f64
}

/// The word orders' arithmetic on canonical bytes (one width per column), for interpolating bounds.
struct Words {
    order: HistOrder,
    width: usize,
}

impl Words {
    /// `None`: byte order, mixed widths, or a float bound that is not finite (NaN and the infinities cannot be
    /// interpolated; the byte-order pick serves them).
    fn of<'a>(order: HistOrder, mut vals: impl Iterator<Item = &'a [u8]>) -> Option<Words> {
        if order == HistOrder::Bytes {
            return None;
        }
        let first = vals.next()?;
        let w = Words { order, width: first.len() };
        if !(1..=8).contains(&w.width) || (order == HistOrder::Float && w.width != 4 && w.width != 8) {
            return None;
        }
        let ok = |v: &[u8]| v.len() == w.width && (order != HistOrder::Float || w.float(v).is_finite());
        (ok(first) && vals.all(ok)).then_some(w)
    }

    fn word(&self, v: &[u8]) -> u64 {
        let mut le = [0u8; 8];
        le[..self.width].copy_from_slice(v);
        u64::from_le_bytes(le)
    }

    fn int(&self, v: &[u8]) -> i128 {
        let w = self.word(v);
        match self.order {
            HistOrder::Signed => {
                let shift = 64 - 8 * self.width as u32;
                i128::from(((w << shift) as i64) >> shift)
            }
            _ => i128::from(w),
        }
    }

    fn float(&self, v: &[u8]) -> f64 {
        let w = self.word(v);
        if self.width == 4 {
            f64::from(f32::from_bits(w as u32))
        } else {
            f64::from_bits(w)
        }
    }

    fn pos(&self, v: &[u8]) -> f64 {
        match self.order {
            HistOrder::Float => self.float(v),
            _ => self.int(v) as f64,
        }
    }

    /// The value `frac` of the way from `lo` to `hi`, as canonical bytes.
    fn between(&self, lo: &[u8], hi: &[u8], frac: f64) -> FormatResult<Vec<u8>> {
        let frac = frac.clamp(0.0, 1.0);
        match self.order {
            HistOrder::Float => {
                let (a, b) = (self.float(lo), self.float(hi));
                let x = (a + (b - a) * frac).clamp(a.min(b), a.max(b));
                if self.width == 4 {
                    pgrc_format::try_to_vec(&(x as f32).to_bits().to_le_bytes()[..], "fold")
                } else {
                    pgrc_format::try_to_vec(&x.to_bits().to_le_bytes()[..], "fold")
                }
            }
            _ => {
                let (a, b) = (self.int(lo), self.int(hi));
                let x = (a + ((b - a) as f64 * frac).round() as i128).clamp(a.min(b), a.max(b));
                pgrc_format::try_to_vec(&(x as u64).to_le_bytes()[..self.width], "fold")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::sidecar::ColDistribution;

    fn rec(nonnull: u32, sortedness: Sortedness, byte_len_sum: u64) -> StatsRecord {
        let mut r = StatsRecord::absent();
        r.nonnull = nonnull;
        r.sortedness = sortedness.as_u8();
        r.byte_len_sum = byte_len_sum;
        r
    }

    #[test]
    fn single_part_fold_is_exact() {
        let r = rec(90, Sortedness::Ascending, 450);
        let sketch = ColDistribution {
            nonnull: 90,
            ndv_eligible: 3,
            long_values: 0,
            mcv: vec![(b"a".to_vec(), 60), (b"b".to_vec(), 20), (b"c".to_vec(), 10)],
            hist_bounds: vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()],
        };
        let out = fold_pg_statistic(
            &[PartColInput {
                part_record: &r,
                rows: 100,
                sketch: Some(&sketch),
            }],
            3.0,
            false,
            HistOrder::Bytes,
        )
        .expect("fold");
        assert!((out.stanullfrac - 0.1).abs() < 1e-9);
        assert!((out.stawidth - 5.0).abs() < 1e-9);
        assert_eq!(out.stadistinct, 3.0);
        assert_eq!(out.sta_mcv[0], (b"a".to_vec(), 0.6));
        assert!(out.sta_histogram.is_empty(), "FOLDSTATS: every value in the list, no histogram (compute_scalar_stats)");
        assert_eq!(out.stacorrelation, 1.0, "P-8: ascending folds to 1.0");
    }

    #[test]
    fn cluster_witness_forces_unit_correlation() {
        let r = rec(10, Sortedness::Unknown, 0);
        let out = fold_pg_statistic(
            &[PartColInput {
                part_record: &r,
                rows: 10,
                sketch: None,
            }],
            10.0,
            true,
            HistOrder::Bytes,
        )
        .expect("fold");
        assert_eq!(out.stacorrelation, 1.0, "FT-6 witness dominates");
        assert!(out.sta_mcv.is_empty(), "no sketch = empty lists, honest");
    }

    /// Wave 3 (cvformat): the sort-and-sum MCV merge and the unstable bound sort give what the ordered map and the
    /// stable sorts gave (reference written out here), on many parts with overlapping values, count ties and repeated
    /// bounds; the charged form's output is the plain one's.
    /// R15 merge (pgrc line): the line's fold (FOLDSTATS) admits MCVs as compute_scalar_stats does and merges the
    /// histograms by weight (`merge_histograms`), so the reference applies that admission to the ordered map's merge,
    /// three values per part are heavy (so the admitted list is not empty), and the histogram checks are its ends and
    /// its bound budget; the charged-vs-plain checks are trunk's.
    #[test]
    fn the_merge_on_an_owner_matches_the_ordered_map() {
        let recs: Vec<StatsRecord> = (0..40).map(|_| rec(500, Sortedness::Unknown, 900)).collect();
        let sketches: Vec<ColDistribution> = (0..40u32)
            .map(|p| ColDistribution {
                nonnull: 500,
                ndv_eligible: 30,
                long_values: 0,
                mcv: (0..30u32)
                    .map(|i| (format!("v{:02}", (i * 7 + p) % 45).into_bytes(), u64::from(1 + (i + p) % 5) * if i < 3 { 1000 } else { 1 }))
                    .collect(),
                hist_bounds: (0..33u32).map(|i| format!("b{:03}", (i * 3 + p) % 70).into_bytes()).collect(),
            })
            .collect();
        let inputs: Vec<PartColInput<'_>> =
            recs.iter().zip(&sketches).map(|(r, s)| PartColInput { part_record: r, rows: 600, sketch: Some(s) }).collect();
        let total = 600.0 * 40.0;
        let mut merged: std::collections::BTreeMap<&[u8], u64> = std::collections::BTreeMap::new();
        for s in &sketches {
            for (v, c) in &s.mcv {
                *merged.entry(v.as_slice()).or_insert(0) += c;
            }
        }
        let mut by_count: Vec<(&[u8], u64)> = merged.into_iter().collect();
        by_count.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        let plain = fold_pg_statistic(&inputs, 45.0, false, HistOrder::Bytes).expect("fold");
        // the line's admission (45 distinct values > STATS_MCV_K: not all fit)
        assert!(by_count.len() > crate::format::sidecar::STATS_MCV_K);
        let min_count = (1.25 * (500.0 * 40.0 / plain.stadistinct)).max(2.0);
        by_count.retain(|&(_, c)| c as f64 >= min_count);
        assert!(!by_count.is_empty(), "the heavy values are admitted");
        let want_mcv: Vec<(Vec<u8>, f64)> =
            by_count.iter().take(crate::format::sidecar::STATS_MCV_K).map(|(v, c)| (v.to_vec(), *c as f64 / total)).collect();
        let mut bounds: Vec<&[u8]> = sketches.iter().flat_map(|s| s.hist_bounds.iter().map(|b| b.as_slice())).collect();
        bounds.sort();
        bounds.dedup();
        assert_eq!(plain.sta_mcv, want_mcv);
        assert!(!plain.sta_histogram.is_empty() && plain.sta_histogram.len() <= FOLD_HIST_BOUNDS.min(bounds.len()));
        assert_eq!(plain.sta_histogram.first().map(Vec::as_slice), bounds.first().copied());
        assert_eq!(plain.sta_histogram.last().map(Vec::as_slice), bounds.last().copied());
        std::thread::spawn(move || {
            memledger::bind_backend(3973, 8173);
            memledger::statement_begin();
            let (o, _) = memledger::charge::context();
            let inputs: Vec<PartColInput<'_>> =
                recs.iter().zip(&sketches).map(|(r, s)| PartColInput { part_record: r, rows: 600, sketch: Some(s) }).collect();
            let charged =
                fold_pg_statistic_for(&inputs, 45.0, false, HistOrder::Bytes, Some((o, memledger::Kind::Region))).expect("fold");
            assert_eq!(charged.sta_mcv, plain.sta_mcv);
            assert_eq!(charged.sta_histogram, plain.sta_histogram);
            assert_eq!(memledger::charge::owner_row(o).committed, 0, "the working lists are given back");
            memledger::statement_end();
            memledger::unbind_backend();
        })
        .join()
        .unwrap();
    }

    #[test]
    fn multi_part_mcv_counts_sum() {
        let r1 = rec(50, Sortedness::Unknown, 100);
        let r2 = rec(50, Sortedness::Unknown, 100);
        let s1 = ColDistribution {
            nonnull: 50,
            ndv_eligible: 2,
            long_values: 0,
            mcv: vec![(b"x".to_vec(), 30), (b"y".to_vec(), 20)],
            hist_bounds: vec![b"x".to_vec(), b"y".to_vec()],
        };
        let s2 = ColDistribution {
            nonnull: 50,
            ndv_eligible: 2,
            long_values: 0,
            mcv: vec![(b"x".to_vec(), 40), (b"z".to_vec(), 10)],
            hist_bounds: vec![b"x".to_vec(), b"z".to_vec()],
        };
        let out = fold_pg_statistic(
            &[
                PartColInput {
                    part_record: &r1,
                    rows: 50,
                    sketch: Some(&s1),
                },
                PartColInput {
                    part_record: &r2,
                    rows: 50,
                    sketch: Some(&s2),
                },
            ],
            3.0,
            false,
            HistOrder::Bytes,
        )
        .expect("fold");
        assert_eq!(out.sta_mcv[0], (b"x".to_vec(), 0.7), "counts sum by value");
        assert_eq!(out.stacorrelation, 0.0);
    }

    // ---- FOLDSTATS (lane AUTOVAC unit 3) ----

    fn int8(v: i64) -> Vec<u8> {
        v.to_le_bytes().to_vec()
    }

    fn keyed_rec(nonnull: u32, sortedness: Sortedness, min: i64, max: i64) -> StatsRecord {
        let mut r = rec(nonnull, sortedness, 0);
        r.key_kind = KeyKind::Exact as u8;
        r.min_key = min;
        r.max_key = max;
        r
    }

    /// A part of the unique int8 keys `lo..=hi` as the seal sketches it: every value once (top-k of counts 1),
    /// value-ordered equi-depth bounds.
    fn unique_part(lo: i64, hi: i64) -> ColDistribution {
        let entries: Vec<(Vec<u8>, u64)> = (lo..=hi).map(|v| (int8(v), 1)).collect();
        let mut byte_ordered = entries.clone();
        byte_ordered.sort();
        crate::sketch::distribution_from_sorted_counts_in(&byte_ordered, HistOrder::Signed)
    }

    fn decode(bounds: &[Vec<u8>]) -> Vec<i64> {
        bounds.iter().map(|b| i64::from_le_bytes(b.as_slice().try_into().expect("int8"))).collect()
    }

    /// The share of the histogram's bins above `x` (the planner's reading, bins equal mass).
    fn frac_above(h: &[i64], x: i64) -> f64 {
        let bins = (h.len() - 1) as f64;
        let mut above = 0.0;
        for w in h.windows(2) {
            if w[0] >= x {
                above += 1.0;
            } else if w[1] > x {
                above += (w[1] - x) as f64 / (w[1] - w[0]) as f64;
            }
        }
        above / bins
    }

    /// Born red: lane AUTOVAC's box 44 table (ids 1..200000, then 300001..500000, one part each) folded to 32
    /// MCVs, n_distinct -0.959 and 33 bounds clustered below each 64 Ki boundary (id > 300000 planned at 14 %).
    #[test]
    fn two_disjoint_parts_fold_like_a_sample() {
        let (a, b) = (unique_part(1, 200_000), unique_part(300_001, 500_000));
        let (ra, rb) = (keyed_rec(200_000, Sortedness::Ascending, 1, 200_000), keyed_rec(200_000, Sortedness::Ascending, 300_001, 500_000));
        let out = fold_pg_statistic(
            &[
                PartColInput { part_record: &ra, rows: 200_000, sketch: Some(&a) },
                PartColInput { part_record: &rb, rows: 200_000, sketch: Some(&b) },
            ],
            383_625.0, // the HLL's estimate on box 44
            false,
            HistOrder::Signed,
        )
        .expect("fold");
        assert!(out.sta_mcv.is_empty(), "a unique column has no MCVs: {:?}", out.sta_mcv.len());
        assert_eq!(out.stadistinct, 400_000.0, "distinct within parts, disjoint ranges: exact");
        assert_eq!(out.stacorrelation, 1.0, "ascending parts in ascending ranges");
        let h = decode(&out.sta_histogram);
        assert!(h.windows(2).all(|w| w[0] < w[1]), "value order: {h:?}");
        assert_eq!((h[0], *h.last().unwrap()), (1, 500_000));
        let above = frac_above(&h, 300_000);
        assert!((above - 0.5).abs() < 0.02, "id > 300000: {above} of the rows, 0.5 true");
        let between = frac_above(&h, 400_000) - frac_above(&h, 410_000);
        assert!((between - 0.025).abs() < 0.005, "BETWEEN 400000 AND 410000: {between}, 0.025 true");
    }

    /// A part weighs by its rows, not its bound count.
    #[test]
    fn parts_weigh_by_rows() {
        let (small, big) = (unique_part(1_000_001, 1_001_000), unique_part(1, 100_000));
        let (rs, rbig) = (keyed_rec(1_000, Sortedness::Ascending, 1_000_001, 1_001_000), keyed_rec(100_000, Sortedness::Ascending, 1, 100_000));
        let out = fold_pg_statistic(
            &[
                PartColInput { part_record: &rs, rows: 1_000, sketch: Some(&small) },
                PartColInput { part_record: &rbig, rows: 100_000, sketch: Some(&big) },
            ],
            101_000.0,
            false,
            HistOrder::Signed,
        )
        .expect("fold");
        let h = decode(&out.sta_histogram);
        let above = frac_above(&h, 100_000);
        assert!(above < 0.03, "the 1000-row part is ~1 % of the rows: {above}");
        assert_eq!(out.stacorrelation, 0.0, "ranges out of part order");
    }

    /// Negative and positive keys: little-endian bytes put -1 after every positive value; the bounds do not.
    #[test]
    fn signed_bounds_follow_values() {
        let d = unique_part(-50_000, 49_999);
        let h = decode(&d.hist_bounds);
        assert_eq!((h[0], *h.last().unwrap()), (-50_000, 49_999));
        assert!(h.windows(2).all(|w| w[0] < w[1]), "{h:?}");
        let mid = h[h.len() / 2];
        assert!(mid.abs() < 2_000, "the median bound is near 0: {mid}");
    }

    /// Overlapping parts: the HLL estimate stands within the parts' exact bounds.
    #[test]
    fn overlapping_unique_parts_keep_the_estimate_in_bounds() {
        let (a, b) = (unique_part(1, 1_000), unique_part(501, 1_500));
        let (ra, rb) = (keyed_rec(1_000, Sortedness::Ascending, 1, 1_000), keyed_rec(1_000, Sortedness::Ascending, 501, 1_500));
        let fold = |est: f64| {
            fold_pg_statistic(
                &[
                    PartColInput { part_record: &ra, rows: 1_000, sketch: Some(&a) },
                    PartColInput { part_record: &rb, rows: 1_000, sketch: Some(&b) },
                ],
                est,
                false,
                HistOrder::Signed,
            )
            .expect("fold")
        };
        assert_eq!(fold(1_490.0).stadistinct, 1_490.0);
        assert_eq!(fold(100.0).stadistinct, 1_000.0, "at least the larger part's exact count");
        assert_eq!(fold(9_999.0).stadistinct, 2_000.0, "at most the parts' sum");
        assert!(fold(1_490.0).sta_mcv.iter().all(|(_, f)| *f >= 2.0 / 2_000.0), "a listed value repeats");
        assert_eq!(fold(1_490.0).stacorrelation, 0.0, "overlapping ranges: not sorted across parts");
    }

    /// A skewed column keeps its heavy hitters (and only them) as MCVs.
    #[test]
    fn heavy_hitters_are_kept_and_singles_are_not() {
        let mut entries: Vec<(Vec<u8>, u64)> = (1..=5_000).map(|v| (int8(v), 1)).collect();
        entries.push((int8(0), 5_000));
        entries.sort();
        let d = crate::sketch::distribution_from_sorted_counts_in(&entries, HistOrder::Signed);
        let r = keyed_rec(10_000, Sortedness::Unknown, 0, 5_000);
        let out = fold_pg_statistic(&[PartColInput { part_record: &r, rows: 10_000, sketch: Some(&d) }], 5_001.0, false, HistOrder::Signed)
            .expect("fold");
        assert_eq!(out.sta_mcv.len(), 1, "{:?}", out.sta_mcv.len());
        assert_eq!(out.sta_mcv[0], (int8(0), 0.5));
    }

    /// Three shuffled parts over one range (their bounds interleave): the merged histogram is the range's quantiles,
    /// not the bounds' clusters.
    #[test]
    fn overlapping_parts_merge_to_quantiles() {
        // part i holds the ids congruent to i mod 3: the same range, and bounds at nearly the same values
        let parts: Vec<ColDistribution> = (0..3i64)
            .map(|i| {
                let mut e: Vec<(Vec<u8>, u64)> = (0..100_000i64).map(|k| (int8(3 * k + i + 1), 1)).collect();
                e.sort();
                crate::sketch::distribution_from_sorted_counts_in(&e, HistOrder::Signed)
            })
            .collect();
        let recs: Vec<StatsRecord> = (0..3i64).map(|i| keyed_rec(100_000, Sortedness::Ascending, i + 1, 299_998 + i)).collect();
        let inputs: Vec<PartColInput<'_>> = parts
            .iter()
            .zip(&recs)
            .map(|(d, r)| PartColInput { part_record: r, rows: 100_000, sketch: Some(d) })
            .collect();
        let out = fold_pg_statistic(&inputs, 300_000.0, false, HistOrder::Signed).expect("fold");
        let h = decode(&out.sta_histogram);
        assert_eq!(h.len(), FOLD_HIST_BOUNDS);
        let below = |x: i64| 1.0 - frac_above(&h, x);
        assert!((below(1_000) - 1_000.0 / 300_000.0).abs() < 0.001, "id < 1000: {}", below(1_000));
        assert!(((below(160_000) - below(150_000)) - 10_000.0 / 300_000.0).abs() < 0.002);
        assert_eq!(out.stacorrelation, 0.0, "overlapping ranges");
    }

    /// Floats interpolate as floats, in value order across the sign.
    #[test]
    fn float_bounds_interpolate_in_value_order() {
        let mk = |lo: i64, hi: i64| {
            let mut e: Vec<(Vec<u8>, u64)> = (lo..hi).map(|k| ((k as f64 / 4.0).to_bits().to_le_bytes().to_vec(), 1)).collect();
            e.sort();
            crate::sketch::distribution_from_sorted_counts_in(&e, HistOrder::Float)
        };
        let (a, b) = (mk(-40_000, 0), mk(0, 40_000));
        let (ra, rb) = (rec(40_000, Sortedness::Unknown, 0), rec(40_000, Sortedness::Unknown, 0));
        let out = fold_pg_statistic(
            &[
                PartColInput { part_record: &ra, rows: 40_000, sketch: Some(&a) },
                PartColInput { part_record: &rb, rows: 40_000, sketch: Some(&b) },
            ],
            80_000.0,
            false,
            HistOrder::Float,
        )
        .expect("fold");
        let h: Vec<f64> = out.sta_histogram.iter().map(|b| f64::from_bits(u64::from_le_bytes(b.as_slice().try_into().expect("f64")))).collect();
        assert!(h.windows(2).all(|w| w[0] < w[1]), "{h:?}");
        assert_eq!((h[0], *h.last().unwrap()), (-10_000.0, 9_999.75));
        assert!(h[50].abs() < 250.0, "the median bound is near 0: {}", h[50]);
    }

    /// Byte order (text under C): the bounds are input bounds, picked where the merged CDF reaches each quantile.
    #[test]
    fn byte_ordered_bounds_are_picked_not_made() {
        let mk = |p: &str| {
            let mut e: Vec<(Vec<u8>, u64)> = (0..10_000).map(|k| (format!("{p}{k:05}").into_bytes(), 1)).collect();
            e.sort();
            crate::sketch::distribution_from_sorted_counts_in(&e, HistOrder::Bytes)
        };
        let (a, b) = (mk("a-"), mk("m-"));
        let (ra, rb) = (rec(10_000, Sortedness::Unknown, 70_000), rec(10_000, Sortedness::Unknown, 70_000));
        let out = fold_pg_statistic(
            &[
                PartColInput { part_record: &ra, rows: 10_000, sketch: Some(&a) },
                PartColInput { part_record: &rb, rows: 10_000, sketch: Some(&b) },
            ],
            20_000.0,
            false,
            HistOrder::Bytes,
        )
        .expect("fold");
        let h = &out.sta_histogram;
        let inputs: std::collections::BTreeSet<&Vec<u8>> = a.hist_bounds.iter().chain(&b.hist_bounds).collect();
        assert!(h.iter().all(|v| inputs.contains(v)), "every bound an input bound");
        assert!(h.windows(2).all(|w| w[0] < w[1]));
        let from_m = h.iter().filter(|v| v.starts_with(b"m-")).count();
        assert!((from_m as i64 - h.len() as i64 / 2).abs() <= 2, "half the bounds from each part: {from_m} of {}", h.len());
    }
}
