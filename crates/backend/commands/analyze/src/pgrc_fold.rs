//! P6-5: the ANALYZE-fold CONSUMER (the other half of `pgrc_am::analyze`,
//! M5a ST-2). The AM folds seal-built facts into `pg_statistic`-shaped
//! per-column output of CANONICAL BYTES; this module owns the render gates
//! the AM deliberately does not: which value lists may be served for which
//! type semantics, and how canonical bytes become datums of the column type.
//!
//! ## The witness membrane (lane constitution)
//!
//! Everything written here lands in `pg_statistic` / `pg_class` and feeds
//! PLANNING ONLY — selectivities, join order, path costs. Nothing here is an
//! engine correctness input; the engine's own sound-witness vocabulary
//! (bankstats et al.) is a separate plane this module never touches.
//!
//! ## Render gates
//!
//! - **Scalar family** (stanullfrac / stawidth / stadistinct / correlation):
//!   always served — exact-at-seal counts, footer-HLL NDV, the FT-6 cluster
//!   witness. No byte rendering involved.
//! - **MCV**: admitted only where BYTE equality is VALUE equality in both
//!   directions modulo frequency splits the planner survives — the integer
//!   word families, bool, memcmp-ordered fixed types, oid-word enums, and
//!   text under a deterministic collation (PG's texteq is bitwise for
//!   deterministic collations). Floats are admitted with the one documented
//!   split (-0.0/+0.0 and NaN payloads byte-differ yet compare equal): a
//!   split only LOWERS an individual MCV frequency — planning-only,
//!   conservative. numeric (1.0 vs 1.00), interval (1 day vs 24 h) and
//!   timetz byte-differ while comparing equal in ways that can HIDE a heavy
//!   hitter under distinct spellings — refused.
//! - **Histogram**: the sidecar's equi-depth bounds live in CANONICAL BYTE
//!   order. Where byte order IS value order (memcmp-ordered types, text
//!   under the C collation) the bounds serve directly. For the fixed-width
//!   word families (ints, dates/timestamps, floats, bool) byte order is a
//!   value-independent permutation of value order (LE words under memcmp),
//!   so the bounds are a SYSTEMATIC SAMPLE of the column's rank space:
//!   decoded and re-sorted by value they are honest approximate quantiles —
//!   the same estimate class as C's sampled histogram, and the planner's
//!   only consumer. Everything else (numeric, non-C text, opaque) serves no
//!   histogram rather than a wrong one.
//!
//! ## Fail-open law (ES-1.3 applied to ANALYZE)
//!
//! Any "cannot serve" here — a decline from the AM, a column the fold does
//! not cover, an inadmissible configuration — returns `None` and the caller
//! falls open to the sampling path (`pgrc_acquire_sample_rows`). Never an
//! error, never silently-wrong stats.
//! A column with rows and an equality operator that the fold gives no NDV (the
//! Opaque profile: bpchar, jsonb), or neither an MCV list nor a histogram
//! (numeric, interval), is sampled by the caller instead (D2621); the fold's
//! NDV, when it has one, still wins over the sample's, except over a sample
//! that holds every row, whose count is exact (D2672).

use datum::Datum;
use mcx::{self, Mcx, PgVec};
use tableam::{
    PgrcCollationClass as Coll, PgrcColFold, PgrcRelFold, PgrcStorageClass as Class,
    PgrcTypeSemantics as Sem,
};
use types_error::PgResult;

use crate::{
    ComputeStats, VacAttrStats, STATISTIC_KIND_CORRELATION, STATISTIC_KIND_HISTOGRAM,
    STATISTIC_KIND_MCV,
};

/// PGRUST_PGRC_ANALYZE_FOLD=0|off: kill switch from mode `fold` back to the
/// sampling path (default ON; it applies only when `fold` is chosen; banked
/// data declines only on the typed causes the census records).
fn fold_enabled() -> bool {
    !matches!(
        std::env::var("PGRUST_PGRC_ANALYZE_FOLD").ok().as_deref().map(str::trim),
        Some("0") | Some("off")
    )
}

const MODE_GUC: &str = "pgrust.pgrc_analyze_mode";

/// `pgrust.pgrc_analyze_mode`: `sample_hll` (the default, D3181) is `sample` with n_distinct from the fold's
/// HyperLogLog; `sample` is PostgreSQL's sample and compute at the statistics target; `fold` serves the seal-time
/// fold. The kill switch above turns `fold` into `sample`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AnalyzeMode {
    Fold,
    Sample,
    SampleHll,
}

pub(crate) fn analyze_mode() -> PgResult<AnalyzeMode> {
    let set = guc::GetConfigOption(MODE_GUC, true, false)?;
    let mode = match parse_mode(set.as_deref().unwrap_or("")) {
        Some(m) => m,
        None => {
            return Err(types_error::PgError::error(format!(
                "invalid value for parameter \"{MODE_GUC}\": \"{}\"",
                set.as_deref().unwrap_or("")
            ))
            .with_sqlstate(types_error::ERRCODE_INVALID_PARAMETER_VALUE)
            .with_hint("Available values: fold, sample, sample_hll.")
            .into())
        }
    };
    Ok(if mode == AnalyzeMode::Fold && !fold_enabled() { AnalyzeMode::Sample } else { mode })
}

const THREADS_GUC: &str = "pgrust.pgrc_analyze_threads";

/// D3742 `pgrust.pgrc_analyze_threads`: the threads that decode a pgrc ANALYZE sample's rows (1 = one, the
/// row-at-a-time walk). Unset or empty: `None`, the caller's default.
pub(crate) fn sample_threads() -> PgResult<Option<usize>> {
    let set = guc::GetConfigOption(THREADS_GUC, true, false)?;
    let v = set.as_deref().unwrap_or("").trim();
    if v.is_empty() {
        return Ok(None);
    }
    match v.parse::<usize>() {
        Ok(n) if (1..=1024).contains(&n) => Ok(Some(n)),
        _ => Err(types_error::PgError::error(format!("invalid value for parameter \"{THREADS_GUC}\": \"{v}\""))
            .with_sqlstate(types_error::ERRCODE_INVALID_PARAMETER_VALUE)
            .with_hint("A thread count from 1 to 1024, or empty for the default.")
            .into()),
    }
}

fn parse_mode(v: &str) -> Option<AnalyzeMode> {
    let v = v.trim();
    if v.eq_ignore_ascii_case("fold") {
        Some(AnalyzeMode::Fold)
    } else if v.eq_ignore_ascii_case("sample") {
        Some(AnalyzeMode::Sample)
    } else if v.is_empty() || v.eq_ignore_ascii_case("sample_hll") {
        Some(AnalyzeMode::SampleHll)
    } else {
        None
    }
}

/// `sample_hll`'s n_distinct per attribute number, in pg_statistic's convention: the fold's HyperLogLog count for
/// the columns it has one for. `None` when the fold declines (e.g. a part with deletions): the sample's stands.
pub(crate) fn hll_ndistinct(mcx: mcx::Mcx<'_>, rel: &types_rel::Relation<'_>) -> PgResult<Option<Vec<(i32, f32)>>> {
    let fold = match tableam::pgrc_analyze_fold(mcx, rel)? {
        tableam::PgrcAnalyzeFold::Folded(f) => f,
        tableam::PgrcAnalyzeFold::Declined(d) => {
            if crate::analyze_trace() {
                eprintln!("ANALYZE|TRACE|pgrc-hll declined rel={} cause={}", rel.rd_id, d.cause());
            }
            return Ok(None);
        }
    };
    let totalrows = fold.total_rows as f64;
    Ok(Some(
        fold.cols
            .iter()
            .filter(|c| c.nonnull > 0 && c.stat.stadistinct > 0.0)
            .map(|c| (c.attno as i32, stadistinct_convention(c.stat.stadistinct, totalrows)))
            .collect(),
    ))
}

/// MCV admission: byte identity must adjudicate value identity (module doc).
fn mcv_admissible(sem: Sem, coll: Coll) -> bool {
    match sem {
        Sem::SignedInt | Sem::UnsignedInt | Sem::Bool | Sem::MemcmpOrdered | Sem::EnumEqOnly => {
            true
        }
        // Documented split: -0.0/+0.0 (and NaN payloads) byte-differ while
        // comparing equal; a split can only lower one entry's frequency.
        Sem::Float => true,
        // texteq is bitwise under deterministic collations.
        Sem::TextCollated => coll != Coll::Nondeterministic,
        Sem::Opaque
        | Sem::PackedNumeric { .. }
        | Sem::NumericUnpacked
        | Sem::IntervalCmp
        | Sem::TimetzUtc => false,
    }
}

/// How (whether) the byte-ordered histogram bounds recover value order.
enum HistMode {
    /// memcmp order IS the type's order: serve the bounds as-is.
    ByteOrder,
    /// Fixed-width word family: decode and re-sort by value (systematic
    /// sample of rank space — module doc).
    Resort,
    /// Not recoverable: serve no histogram.
    Refuse,
}

fn hist_mode(sem: Sem, coll: Coll) -> HistMode {
    match sem {
        Sem::MemcmpOrdered => HistMode::ByteOrder,
        Sem::TextCollated if coll == Coll::C => HistMode::ByteOrder,
        Sem::SignedInt | Sem::UnsignedInt | Sem::Float | Sem::Bool => HistMode::Resort,
        // Enum order is catalog sort order, not oid-word order; numeric /
        // interval / timetz orders are not byte orders; opaque has none.
        Sem::EnumEqOnly
        | Sem::TextCollated
        | Sem::Opaque
        | Sem::PackedNumeric { .. }
        | Sem::NumericUnpacked
        | Sem::IntervalCmp
        | Sem::TimetzUtc => HistMode::Refuse,
    }
}

/// The value-order sort key for `HistMode::Resort` (PG btree order for the
/// word families: NaN sorts greatest, NaN == NaN, -0 == +0 — float_cmp).
#[derive(PartialEq, PartialOrd)]
enum ResortKey {
    I(i64),
    U(u64),
    F(f64),
}

fn resort_cmp(a: &ResortKey, b: &ResortKey) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    match (a, b) {
        (ResortKey::I(x), ResortKey::I(y)) => x.cmp(y),
        (ResortKey::U(x), ResortKey::U(y)) => x.cmp(y),
        (ResortKey::F(x), ResortKey::F(y)) => match (x.is_nan(), y.is_nan()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
        },
        // Keys within one column are homogeneous by construction.
        _ => Ordering::Equal,
    }
}

/// Canonical word bytes → the datum word (the inverse of
/// `datum_canonical_bytes`' byval arms: LE bytes, width-truncated, datum
/// extension per class).
fn word_from_bytes(bytes: &[u8], width: usize, signed: bool) -> Option<u64> {
    if bytes.len() != width || width > 8 {
        return None;
    }
    let mut le = [0u8; 8];
    le[..width].copy_from_slice(bytes);
    let mut w = u64::from_le_bytes(le);
    if signed && width < 8 {
        let shift = 64 - width as u32 * 8;
        w = (((w << shift) as i64) >> shift) as u64;
    }
    Some(w)
}

/// Render one canonical value into a datum of the column's type in `mcx`.
/// `None` = the bytes do not fit the declared class (refuse-and-sample).
fn render_datum<'mcx>(
    mcx: Mcx<'mcx>,
    class: Class,
    bytes: &[u8],
) -> PgResult<Option<Datum>> {
    Ok(match class {
        Class::ByvalWord { width, signed } => {
            word_from_bytes(bytes, width as usize, signed).map(Datum::from_u64)
        }
        Class::F32 => word_from_bytes(bytes, 4, false).map(Datum::from_u64),
        Class::F64 => word_from_bytes(bytes, 8, false).map(Datum::from_u64),
        Class::Bool => word_from_bytes(bytes, 1, false).map(Datum::from_u64),
        Class::Fixed { len } => {
            if bytes.len() != len as usize {
                None
            } else {
                let copy = mcx::slice_borrow_in(mcx, bytes)?;
                Some(Datum::from_usize(copy.as_ptr() as usize))
            }
        }
        Class::VarlenaVerbatim => {
            // Canonical bytes are the PAYLOAD (header stripped); rebuild the
            // 4B-U image the operators read.
            let total = bytes.len() + 4;
            let mut img = ::mcx::std_vec_with_capacity("commands", total)?;
            img.extend_from_slice(
                &types_tuple::varatt::set_varsize_4b_word(total as u32).to_ne_bytes(),
            );
            img.extend_from_slice(bytes);
            let copy = mcx::slice_borrow_in(mcx, &img)?;
            Some(Datum::from_usize(copy.as_ptr() as usize))
        }
    })
}

/// The value-order key for a rendered word datum under `Resort`.
fn resort_key(class: Class, sem: Sem, word: u64) -> ResortKey {
    match class {
        Class::F32 => ResortKey::F(f32::from_bits(word as u32) as f64),
        Class::F64 => ResortKey::F(f64::from_bits(word)),
        _ => match sem {
            Sem::SignedInt => ResortKey::I(word as i64),
            _ => ResortKey::U(word),
        },
    }
}

/// pg_statistic's stawidth from the fold's average payload (D2659):
/// compute_scalar_stats averages VARSIZE_ANY of the stored datums (a 1-byte
/// header up to 126 payload bytes, else 4) and truncates. The fold keeps only
/// the payload sum, so a column mixing both header sizes, or holding values
/// heap would compress or move out of line, reads the header of its average.
fn varlena_width(payload_avg: f64) -> i32 {
    let header = if payload_avg <= 126.0 { 1.0 } else { 4.0 };
    (payload_avg + header) as i32
}

/// stawidth of a column with only nulls: compute_scalar_stats /
/// compute_distinct_stats keep typlen for a fixed-width type and write 0
/// ("unknown") for a varwidth one (D2660).
fn null_only_width(typlen: i16) -> i32 {
    if typlen > 0 { i32::from(typlen) } else { 0 }
}

/// The indices of the histogram bounds a statistics target allows:
/// compute_scalar_stats keeps at most target + 1 bounds (D2667). The fold's
/// sorted bounds thin evenly and keep both ends, as its cross-part merge
/// thins to the format's bound budget.
fn thinned(n: usize, target: i32) -> impl Iterator<Item = usize> {
    let k = (target.max(1) as usize + 1).min(n);
    (0..k).map(move |i| if k < 2 { 0 } else { i * (n - 1) / (k - 1) })
}

/// A value the fold counted at least twice (frequencies are count / total
/// rows) (D2664).
fn repeated(freq: f64, total_rows: f64) -> bool {
    freq * total_rows >= 1.5
}

/// An MCV list whose counts sum to every non-null row holds every value, so
/// C builds no histogram (compute_scalar_stats: the histogram is over the
/// values left outside the MCV list) (D2661). Frequencies are count /
/// total rows.
fn mcv_covers_all(freqs: impl Iterator<Item = f64>, nonnull: u64, total_rows: f64) -> bool {
    let counted: f64 = freqs.map(|f| f * total_rows).sum();
    nonnull > 0 && counted + 0.5 >= nonnull as f64
}

/// pg_statistic negative-fraction convention (compute_scalar_stats /
/// the footer-NDV override): d > 10% of rows → a fraction that scales.
fn stadistinct_convention(d: f64, totalrows: f64) -> f32 {
    if d <= 0.0 {
        return 0.0;
    }
    let d = d.min(totalrows.max(1.0));
    if d > 0.1 * totalrows {
        -(d / totalrows.max(1.0)) as f32
    } else {
        d as f32
    }
}

/// What the caller's per-column sample computes for a column the fold left.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Sampled {
    /// the column's whole compute (the fold could not serve it)
    Whole,
    /// only the jsonb per-path slot (the fold served the std part)
    PathSlot,
}

#[derive(Debug, PartialEq)]
enum Fill {
    Served,
    /// Left untouched for the caller's per-column sample; the fold's
    /// stadistinct (0 = none).
    Sample(f32),
    Refused,
}

/// An equality-less type keeps the fold: compute_trivial_stats has no more.
fn needs_sample(nonnull: u64, has_ndv: bool, mcv: bool, hist: bool, compute: &ComputeStats) -> bool {
    nonnull > 0 && !matches!(compute, ComputeStats::Trivial) && (!has_ndv || (!mcv && !hist))
}

/// Fill `stats` (the examine_attribute output for this relation) from the
/// AM's fold. `None` = some targeted column is not honestly servable from
/// the fold (fail OPEN: the caller samples the relation instead; nothing is
/// left written). `Some(sampled)` = served, except the `vacattrstats`
/// indices listed, which the fold left untouched (or, for a jsonb column's
/// path slot, unfinished) for the caller to compute from a sample of those
/// columns, each with the fold's stadistinct (0 = none).
pub(crate) fn apply_fold<'mcx>(
    anl_mcx: Mcx<'mcx>,
    fold: &PgrcRelFold,
    vacattrstats: &mut [VacAttrStats<'mcx>],
) -> PgResult<Option<PgVec<'mcx, (usize, f32, Sampled)>>> {
    let totalrows = fold.total_rows as f64;
    // All-or-nothing admission first: every targeted column must be std
    // (Scalar/Distinct/Trivial) or a jsonb column with per-path statistics
    // (std + the path slot), and covered by the fold.
    for s in vacattrstats.iter() {
        if !matches!(
            s.compute,
            ComputeStats::Scalar
                | ComputeStats::Distinct
                | ComputeStats::Trivial
                | ComputeStats::Jsonb { .. }
        ) {
            return Ok(None);
        }
        if s.tupattnum < 1
            || !fold.cols.iter().any(|c| c.attno == s.tupattnum as u32)
        {
            return Ok(None);
        }
    }

    let mut sampled: PgVec<'mcx, (usize, f32, Sampled)> = PgVec::new_in(anl_mcx);
    for i in 0..vacattrstats.len() {
        let s = &mut vacattrstats[i];
        let col: &PgrcColFold = fold
            .cols
            .iter()
            .find(|c| c.attno == s.tupattnum as u32)
            .expect("admission checked coverage");
        // pgrust-only: a jsonb column's std part is the fold's exactly as
        // without path statistics; its path slot always comes from the sample
        // (the same collector heap runs, so the same rows give the same slot).
        let saved = crate::jsonb_typanalyze::std_compute_of(&s.compute)
            .map(|std| core::mem::replace(&mut s.compute, std));
        let jsonb = saved.is_some();
        let fill = fill_column(anl_mcx, s, col, totalrows);
        if let Some(c) = saved {
            s.compute = c;
        }
        match fill? {
            Fill::Served if jsonb => sampled.push((i, 0.0, Sampled::PathSlot)),
            Fill::Served => {}
            Fill::Sample(ndv) => sampled.push((i, ndv, Sampled::Whole)),
            Fill::Refused => {
                // Mid-way render refusal (corrupt-class rarity): scrub every
                // partially-filled struct so the sampling path starts clean —
                // its compute may write fewer slots than the fold did.
                for s in vacattrstats.iter_mut() {
                    reset_stats(anl_mcx, s);
                }
                return Ok(None);
            }
        }
    }
    Ok(Some(sampled))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// analyze.c's histogram leaves the MCV members out (num_hist = ndistinct - num_mcv): a bound that is a served MCV
    /// value goes; with no MCV slot served the bounds pass through.
    #[test]
    fn histogram_excludes_the_served_mcv_members() {
        let hist: Vec<Vec<u8>> = [b"a", b"b", b"c", b"d"].iter().map(|b| b.to_vec()).collect();
        let (b, d) = ((b"b".to_vec(), 0.5), (b"d".to_vec(), 0.25));
        let tail = tail_histogram(&hist, &[&b, &d]).unwrap();
        assert_eq!(tail, [b"a".as_slice(), b"c".as_slice()]);
        assert_eq!(tail_histogram(&hist, &[]).unwrap().len(), 4);
    }

    /// The inverse of `datum_canonical_bytes`' byval arm: LE truncation
    /// round-trips through sign/zero extension.
    #[test]
    fn word_round_trip() {
        // int2 -3: datum word (sign-extended) → canonical 2 LE bytes → word.
        let w = (-3i64) as u64;
        let bytes = w.to_le_bytes();
        assert_eq!(word_from_bytes(&bytes[..2], 2, true), Some(w));
        // oid word zero-extends.
        assert_eq!(word_from_bytes(&0xFFFFFFFFu32.to_le_bytes(), 4, false), Some(0xFFFF_FFFF));
        // Width mismatch refuses.
        assert_eq!(word_from_bytes(&bytes[..3], 2, true), None);
    }

    /// Resort order is PG btree order for the word families: signed ints by
    /// value, floats with NaN greatest.
    #[test]
    fn resort_order_is_value_order() {
        let mut v = vec![ResortKey::I(10), ResortKey::I(-5), ResortKey::I(0)];
        v.sort_by(resort_cmp);
        assert!(matches!(v[0], ResortKey::I(-5)));
        assert!(matches!(v[2], ResortKey::I(10)));
        let mut f = vec![
            ResortKey::F(f64::NAN),
            ResortKey::F(1.5),
            ResortKey::F(-2.0),
        ];
        f.sort_by(resort_cmp);
        assert!(matches!(f[0], ResortKey::F(x) if x == -2.0));
        assert!(matches!(f[2], ResortKey::F(x) if x.is_nan()));
    }

    /// pg_statistic's negative-fraction convention (the footer-NDV override's
    /// exact arithmetic).
    #[test]
    fn stadistinct_negative_fraction_convention() {
        assert_eq!(stadistinct_convention(50.0, 1000.0), 50.0);
        assert_eq!(stadistinct_convention(500.0, 1000.0), -0.5);
        assert_eq!(stadistinct_convention(0.0, 1000.0), 0.0);
        // Clamped to totalrows first: 2000 → 1000 → the -1.0 all-distinct form.
        assert_eq!(stadistinct_convention(2000.0, 1000.0), -1.0);
    }

    /// The admission vocabulary is pinned: byte-eq==value-eq classes serve
    /// MCVs; equality-normalizing types refuse.
    #[test]
    fn mcv_admission_gates() {
        assert!(mcv_admissible(Sem::SignedInt, Coll::C));
        assert!(mcv_admissible(Sem::MemcmpOrdered, Coll::OtherDeterministic));
        assert!(mcv_admissible(Sem::TextCollated, Coll::OtherDeterministic));
        assert!(!mcv_admissible(Sem::TextCollated, Coll::Nondeterministic));
        assert!(!mcv_admissible(Sem::PackedNumeric { scale: 2 }, Coll::C));
        assert!(!mcv_admissible(Sem::IntervalCmp, Coll::C));
        assert!(!mcv_admissible(Sem::Opaque, Coll::C));
    }

    #[test]
    fn analyze_mode_values() {
        assert_eq!(parse_mode(""), Some(AnalyzeMode::SampleHll));
        assert_eq!(parse_mode("  "), Some(AnalyzeMode::SampleHll));
        assert_eq!(parse_mode("fold"), Some(AnalyzeMode::Fold));
        assert_eq!(parse_mode(" Sample "), Some(AnalyzeMode::Sample));
        assert_eq!(parse_mode("SAMPLE_HLL"), Some(AnalyzeMode::SampleHll));
        assert_eq!(parse_mode("hll"), None);
        assert_eq!(parse_mode("sample-hll"), None);
    }

    #[test]
    fn thinned_bounds_respect_the_target() {
        assert_eq!(thinned(33, 100).collect::<Vec<_>>(), (0..33).collect::<Vec<_>>());
        assert_eq!(thinned(33, 10).collect::<Vec<_>>(), vec![0, 3, 6, 9, 12, 16, 19, 22, 25, 28, 32]);
        assert_eq!(thinned(33, 1).collect::<Vec<_>>(), vec![0, 32]);
        assert_eq!(thinned(2, 10).collect::<Vec<_>>(), vec![0, 1]);
    }

    #[test]
    fn a_value_counted_once_is_not_repeated() {
        assert!(!repeated(1.0 / 9000.0, 9000.0));
        assert!(repeated(2.0 / 9000.0, 9000.0));
        assert!(repeated(0.5, 10.0));
    }

    #[test]
    fn null_only_width_keeps_a_fixed_typlen() {
        assert_eq!(null_only_width(4), 4);
        assert_eq!(null_only_width(16), 16);
        assert_eq!(null_only_width(-1), 0);
        assert_eq!(null_only_width(-2), 0);
    }

    #[test]
    fn mcv_covering_every_nonnull_row_is_complete() {
        // 3 values x 300 rows of 1000, 100 nulls.
        assert!(mcv_covers_all([0.3, 0.3, 0.3].into_iter(), 900, 1000.0));
        // One value outside the list.
        assert!(!mcv_covers_all([0.3, 0.3, 0.299].into_iter(), 900, 1000.0));
        assert!(!mcv_covers_all(std::iter::empty(), 0, 1000.0));
    }

    #[test]
    fn varlena_width_counts_the_stored_header() {
        assert_eq!(varlena_width(10.0), 11);
        assert_eq!(varlena_width(27.6), 28);
        assert_eq!(varlena_width(126.0), 127);
        assert_eq!(varlena_width(200.0), 204);
    }

    #[test]
    fn unestimated_columns_are_sampled() {
        // No NDV (Opaque: bpchar, jsonb).
        assert!(needs_sample(1000, false, false, false, &ComputeStats::Scalar));
        assert!(needs_sample(1000, false, false, false, &ComputeStats::Distinct));
        // An NDV but neither value list (numeric, interval).
        assert!(needs_sample(1000, true, false, false, &ComputeStats::Scalar));
        // The fold characterizes it.
        assert!(!needs_sample(1000, true, true, false, &ComputeStats::Scalar));
        assert!(!needs_sample(1000, true, false, true, &ComputeStats::Scalar));
        // Null-only, or no equality operator: the fold's own values are C's.
        assert!(!needs_sample(0, false, false, false, &ComputeStats::Scalar));
        assert!(!needs_sample(1000, false, false, false, &ComputeStats::Trivial));
    }

    #[test]
    fn hist_admission_gates() {
        assert!(matches!(hist_mode(Sem::MemcmpOrdered, Coll::C), HistMode::ByteOrder));
        assert!(matches!(hist_mode(Sem::TextCollated, Coll::C), HistMode::ByteOrder));
        assert!(matches!(
            hist_mode(Sem::TextCollated, Coll::OtherDeterministic),
            HistMode::Refuse
        ));
        assert!(matches!(hist_mode(Sem::SignedInt, Coll::C), HistMode::Resort));
        assert!(matches!(hist_mode(Sem::Float, Coll::C), HistMode::Resort));
        assert!(matches!(
            hist_mode(Sem::PackedNumeric { scale: 2 }, Coll::C),
            HistMode::Refuse
        ));
        assert!(matches!(hist_mode(Sem::EnumEqOnly, Coll::C), HistMode::Refuse));
    }
}

fn reset_stats<'mcx>(anl_mcx: Mcx<'mcx>, s: &mut VacAttrStats<'mcx>) {
    s.stats_valid = false;
    s.stanullfrac = 0.0;
    s.stawidth = 0;
    s.stadistinct = 0.0;
    for k in 0..s.stakind.len() {
        s.stakind[k] = 0;
        s.staop[k] = types_core::InvalidOid;
        s.stacoll[k] = types_core::InvalidOid;
        s.stanumbers[k] = PgVec::new_in(anl_mcx);
        s.stavalues[k] = PgVec::new_in(anl_mcx);
        s.stavalues_set[k] = false;
    }
}

/// The histogram bounds left once the served MCV members are taken out. analyze.c's compute_scalar_stats builds the
/// histogram over the values not in the MCV list (num_hist = ndistinct - num_mcv), and the planner scales histogram
/// selectivity by the non-MCV fraction, so a bound that is also a served MCV value would be counted twice.
fn tail_histogram<'a>(hist: &'a [Vec<u8>], mcv: &[&(Vec<u8>, f64)]) -> PgResult<Vec<&'a [u8]>> {
    let mut out = ::mcx::std_vec_with_capacity("commands", hist.len())?;
    out.extend(hist.iter().map(Vec::as_slice).filter(|b| !mcv.iter().any(|(m, _)| m.as_slice() == *b)));
    Ok(out)
}

fn fill_column<'mcx>(
    anl_mcx: Mcx<'mcx>,
    s: &mut VacAttrStats<'mcx>,
    col: &PgrcColFold,
    totalrows: f64,
) -> PgResult<Fill> {
    let stat = &col.stat;

    if totalrows <= 0.0 {
        // Committed-empty relations decline AM-side; defensive only.
        return Ok(Fill::Refused);
    }
    let has_eq = s.extra.eqopr != types_core::InvalidOid;
    let has_lt = s.extra.ltopr != types_core::InvalidOid;
    // D2664: compute_scalar_stats tracks only values seen at least twice, so
    // a value the fold counted once is no MCV (compute_distinct_stats keeps
    // its singletons).
    let scalar = matches!(s.compute, ComputeStats::Scalar);
    let mut mcv_list: Vec<&(Vec<u8>, f64)> =
        stat.sta_mcv.iter().filter(|e| !scalar || repeated(e.1, totalrows)).collect();
    // D2667: at most the column's statistics target of MCVs (the list is in
    // count order).
    mcv_list.truncate(s.attstattarget().max(1) as usize);
    let mcv = has_eq && !mcv_list.is_empty() && mcv_admissible(col.semantics, col.collation_class);
    let hist = has_lt
        && stat.sta_histogram.len() >= 2
        && !matches!(hist_mode(col.semantics, col.collation_class), HistMode::Refuse);
    if needs_sample(col.nonnull, stat.stadistinct > 0.0, mcv, hist, &s.compute) {
        return Ok(Fill::Sample(stadistinct_convention(stat.stadistinct, totalrows)));
    }

    // ---- scalar family (always served) ----
    s.stats_valid = true;
    s.stanullfrac = stat.stanullfrac.clamp(0.0, 1.0) as f32;
    s.stawidth = if s.typlen > 0 {
        s.typlen as i32
    } else if stat.stawidth > 0.0 {
        varlena_width(stat.stawidth)
    } else {
        0
    };
    if col.nonnull == 0 {
        // Null-only column: C's convention (nullfrac 1, distinct 0, width
        // typlen, or 0 = unknown for a varwidth type) (D2660).
        s.stanullfrac = 1.0;
        s.stawidth = null_only_width(s.typlen);
        s.stadistinct = 0.0;
        return Ok(Fill::Served);
    }
    s.stadistinct = stadistinct_convention(stat.stadistinct, totalrows);

    let mut slot_idx = 0usize;

    // ---- MCV ----
    if mcv {
        let n = mcv_list.len();
        let mut values: PgVec<'mcx, Datum> = mcx::vec_with_capacity_in(anl_mcx, n)?;
        let mut freqs: PgVec<'mcx, f32> = mcx::vec_with_capacity_in(anl_mcx, n)?;
        for &(bytes, freq) in &mcv_list {
            let Some(d) = render_datum(anl_mcx, col.class, bytes)? else {
                return Ok(Fill::Refused);
            };
            values.push(d);
            freqs.push(*freq as f32);
        }
        s.stakind[slot_idx] = STATISTIC_KIND_MCV;
        s.staop[slot_idx] = s.extra.eqopr;
        s.stacoll[slot_idx] = s.attrcollid;
        s.stanumbers[slot_idx] = freqs;
        s.stavalues[slot_idx] = values;
        s.stavalues_set[slot_idx] = true;
        slot_idx += 1;
    }

    // ---- histogram ----
    let all_in_mcv = mcv && mcv_covers_all(mcv_list.iter().map(|e| e.1), col.nonnull, totalrows);
    let tail = tail_histogram(&stat.sta_histogram, if mcv { &mcv_list[..] } else { &[] })?;
    if has_lt && tail.len() >= 2 && !all_in_mcv {
        match hist_mode(col.semantics, col.collation_class) {
            HistMode::ByteOrder => {
                let n = tail.len();
                let mut values: PgVec<'mcx, Datum> = mcx::vec_with_capacity_in(anl_mcx, n)?;
                for bytes in thinned(n, s.attstattarget()).map(|i| tail[i]) {
                    let Some(d) = render_datum(anl_mcx, col.class, bytes)? else {
                        return Ok(Fill::Refused);
                    };
                    values.push(d);
                }
                s.stakind[slot_idx] = STATISTIC_KIND_HISTOGRAM;
                s.staop[slot_idx] = s.extra.ltopr;
                s.stacoll[slot_idx] = s.attrcollid;
                s.stavalues[slot_idx] = values;
                s.stavalues_set[slot_idx] = true;
                slot_idx += 1;
            }
            HistMode::Resort => {
                let mut keyed: Vec<(ResortKey, u64)> = ::mcx::std_vec_with_capacity("commands", tail.len())?;
                for &bytes in &tail {
                    let (width, signed) = match col.class {
                        Class::ByvalWord { width, signed } => (width as usize, signed),
                        Class::F32 => (4, false),
                        Class::F64 => (8, false),
                        Class::Bool => (1, false),
                        _ => return Ok(Fill::Refused),
                    };
                    let Some(w) = word_from_bytes(bytes, width, signed) else {
                        return Ok(Fill::Refused);
                    };
                    keyed.push((resort_key(col.class, col.semantics, w), w));
                }
                keyed.sort_by(|a, b| resort_cmp(&a.0, &b.0));
                let mut values: PgVec<'mcx, Datum> =
                    mcx::vec_with_capacity_in(anl_mcx, keyed.len())?;
                for (_, w) in thinned(keyed.len(), s.attstattarget()).map(|i| &keyed[i]) {
                    values.push(Datum::from_u64(*w));
                }
                s.stakind[slot_idx] = STATISTIC_KIND_HISTOGRAM;
                s.staop[slot_idx] = s.extra.ltopr;
                s.stacoll[slot_idx] = s.attrcollid;
                s.stavalues[slot_idx] = values;
                s.stavalues_set[slot_idx] = true;
                slot_idx += 1;
            }
            HistMode::Refuse => {}
        }
    }

    // ---- correlation (P-8: seal sortedness + the FT-6 cluster witness) ----
    if has_lt {
        let mut corrs: PgVec<'mcx, f32> = mcx::vec_with_capacity_in(anl_mcx, 1)?;
        corrs.push(stat.stacorrelation as f32);
        s.stakind[slot_idx] = STATISTIC_KIND_CORRELATION;
        s.staop[slot_idx] = s.extra.ltopr;
        s.stacoll[slot_idx] = s.attrcollid;
        s.stanumbers[slot_idx] = corrs;
    }

    Ok(Fill::Served)
}
