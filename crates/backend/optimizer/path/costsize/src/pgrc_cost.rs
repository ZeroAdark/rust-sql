//! The pgrcolumnar scan cost model: pure functions over the facts plancat put on the `RelOptInfo`, built
//! into a [`Scan`] by `cost_seqscan`'s pgrcolumnar arm. Never optimistic:
//! a qual without usable ranges or bloom keeps every granule.

/// PostgreSQL's block size, the unit of `seq_page_cost`.
pub const BLCKSZ: f64 = 8192.0;


/// Codec kinds, the values of `types_pathnodes::PGRC_CODEC_*`.
pub const CODEC_UNKNOWN: u8 = 0;
pub const CODEC_WORD: u8 = 1;
pub const CODEC_DICT: u8 = 2;
pub const CODEC_TEXT: u8 = 3;
pub const CODEC_FIXED: u8 = 4;

/// A zone comparison (pgrc_zonequal, shared with the executor's run-time pruning).
pub use pgrc_zonequal::ZoneOp;

/// How a restriction clause is evaluated, as the facts decide it: per
/// granule from the ranges (per row only where they cannot decide), by a
/// bloom probe, once per dictionary code, or per row.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum QualKind {
    Zone { op: ZoneOp, key: i64 },
    /// An `=` the column's blooms answer, built with `k` hash functions ([`bloom_k`]).
    Bloom { k: u32 },
    Dict,
    Row,
}

/// One restriction clause in evaluation order.
#[derive(Clone, Debug)]
pub struct Qual {
    pub kind: QualKind,
    pub attno: i16,
    pub selectivity: f64,
    pub per_tuple: f64,
    pub lanes: Vec<i16>,
}

/// One touched column.
#[derive(Clone, Copy, Debug)]
pub struct Lane {
    pub attno: i16,
    pub codec: u8,
    pub bytes: u64,
    pub ncodes: u64,
    pub in_target: bool,
}

/// The scan the model prices.
#[derive(Clone, Copy, Debug)]
pub struct Scan<'a> {
    pub tuples: f64,
    /// The planner's output row estimate for this path.
    pub rows: f64,
    pub granules: u64,
    pub lanes: &'a [Lane],
    pub quals: &'a [Qual],
    /// `(attno, summary)` for the zone columns the quals reference.
    pub zones: &'a [(i16, ZoneSummary<'a>)],
}

/// One zone column's summary as plancat holds it (statement-floor item 2;
/// `pgrc_format::zonemap::ZoneSummary` builds it at catalog load): `keyed`
/// granules with a bound of `granules`, and the shared key grid — the union
/// of the 257 quantile points of the granule minima and of the maxima — with
/// the exact number of minima and of maxima `<` / `≤` every grid key and
/// of granules whose both bounds equal it. A rank query is exact at a grid
/// key and linear between two keys, where fewer than `keyed / 256` values
/// of either multiset lie — the error bound per count, in granules (`=`
/// subtracts two: twice that; its decided granules are exact on the grid
/// and 0 off it, where at most that many can hide); a column with ≤ 514
/// distinct bounds is exact everywhere.
#[derive(Clone, Copy, Debug)]
pub struct ZoneSummary<'a> {
    pub granules: u64,
    pub keyed: u64,
    pub keys: &'a [i64],
    pub min_lt: &'a [u32],
    pub min_le: &'a [u32],
    pub max_lt: &'a [u32],
    pub max_le: &'a [u32],
    pub eq_whole: &'a [u32],
}

/// The census a summary gives: real-valued granule counts.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CensusEst {
    pub survived: f64,
    pub mixed: f64,
}

impl<'a> ZoneSummary<'a> {
    /// `#values ≤ key` (`le`) or `#values < key` over one multiset, from its
    /// two count rows: exact on the grid, interpolated between keys.
    fn rank(&self, lt: &[u32], le: &[u32], key: i64, want_le: bool) -> f64 {
        let keys = self.keys;
        let Some(&last) = keys.last() else { return 0.0 };
        if key < keys[0] {
            return 0.0;
        }
        if key > last {
            return self.keyed as f64;
        }
        let i = keys.partition_point(|&k| k <= key) - 1;
        if keys[i] == key {
            return f64::from(if want_le { le[i] } else { lt[i] });
        }
        // strictly between keys[i] and keys[i + 1]: the values there are neither
        let (lo, hi) = (f64::from(le[i]), f64::from(lt[i + 1]));
        let span = (keys[i + 1] as i128 - keys[i] as i128) as f64;
        lo + (hi - lo) * ((key as i128 - keys[i] as i128) as f64 / span)
    }

    fn mins(&self, key: i64, le: bool) -> f64 {
        self.rank(self.min_lt, self.min_le, key, le)
    }

    fn maxs(&self, key: i64, le: bool) -> f64 {
        self.rank(self.max_lt, self.max_le, key, le)
    }

    /// The granules `op key` keeps and, of those, the ones the bounds do
    /// not decide — [`census`]'s verdicts from the summary. Empty granules
    /// never survive (they are not in the multisets).
    pub fn census(&self, op: ZoneOp, key: i64) -> CensusEst {
        let n = self.keyed as f64;
        let (survived, whole) = match op {
            ZoneOp::Lt => (self.mins(key, false), self.maxs(key, false)),
            ZoneOp::Le => (self.mins(key, true), self.maxs(key, true)),
            ZoneOp::Gt => (n - self.maxs(key, true), n - self.mins(key, true)),
            ZoneOp::Ge => (n - self.maxs(key, false), n - self.mins(key, false)),
            ZoneOp::Eq => {
                let s = self.mins(key, true) - self.maxs(key, false);
                let whole = self.keys.binary_search(&key).map_or(0.0, |i| f64::from(self.eq_whole[i]));
                (s, whole)
            }
        };
        let survived = survived.clamp(0.0, n);
        CensusEst { survived, mixed: (survived - whole.clamp(0.0, survived)).max(0.0) }
    }
}

/// The per-unit constants. The two fractions (item 11b) scale the model's
/// fixed per-row terms inherited from PostgreSQL — `cpu_tuple_cost` per
/// output row and the per-row operator charge of a qual evaluated per row —
/// to the columnar engine's measured per-row prices; a dictionary-grain
/// qual (charged per code) and a zone-decided qual (charged nothing) are
/// not per-row and keep their charge. Both fractions are > 0: a per-row
/// qual on a pgrc scan is never free.
#[derive(Clone, Copy, Debug)]
pub struct Knobs {
    pub cpu_tuple_cost: f64,
    pub cpu_operator_cost: f64,
    pub seq_page_cost: f64,
    pub effective_cache_bytes: f64,
    pub granule: f64,
    pub word: f64,
    pub byref: f64,
    pub tuple_fraction: f64,
    pub qual_fraction: f64,
}

/// The priced scan: `run` is the run cost (startup is the caller's, from
/// the quals' and the target's startup terms).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScanCost {
    pub run: f64,
    pub granules_survived: f64,
    pub rows_scanned: f64,
    pub word_rows: f64,
    pub byref_rows: f64,
}

/// One zone qual's granule verdicts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Census {
    pub survived: u64,
    pub mixed: u64,
}

/// Count the granules `op key` keeps; an empty range (all-null granule,
/// `lo > hi`) never survives. The exact walk: the reference the summary's
/// [`ZoneSummary::census`] is measured against (the trace, the tests); the
/// zone-walk paths' own census is `gravity_paths::zone_walk_census`.
pub fn census(ranges: &[(i64, i64)], op: ZoneOp, key: i64) -> Census {
    let mut c = Census::default();
    for &(lo, hi) in ranges {
        if lo > hi {
            continue;
        }
        let (meets, whole) = match op {
            ZoneOp::Eq => (lo <= key && key <= hi, lo == key && hi == key),
            ZoneOp::Lt => (lo < key, hi < key),
            ZoneOp::Le => (lo <= key, hi <= key),
            ZoneOp::Gt => (hi > key, lo > key),
            ZoneOp::Ge => (hi >= key, lo >= key),
        };
        if meets {
            c.survived += 1;
            if !whole {
                c.mixed += 1;
            }
        }
    }
    c
}

/// The hash functions of the writer's blooms over a column of `width` bytes a value (P5-12: pgrc_meta `bloom_k_for`,
/// the rate from cost, `cost_profile::bloom`, over 8 192-row granules in the warm regime; a varlena column is priced
/// at its 4-byte header, as the writer prices it).
pub fn bloom_k(width: u64) -> u32 {
    cost_profile::bloom::bloom_k(8192, width.max(1), &cost_profile::device::Device::WARM)
}

/// The false-positive rate of one granule bloom holding `distinct` keys under `k` hash functions: the writer sizes the
/// block to the keys (`order_key::bloom::bloom_bytes_for`), so `(1 - e^(-k·d/m))^k` over its `m` bits stays at most
/// `2^-k`.
pub fn bloom_false_positive(distinct: f64, k: u32) -> f64 {
    order_key::bloom::bloom_false_positive(distinct, k)
}

/// The fraction of `granules` a bloom `=` of selectivity `selectivity`
/// keeps over `tuples` rows spread uniformly: the granules holding at least
/// one match, plus false positives among the rest. A granule holds
/// `min(rows per granule, 1 / selectivity)` distinct keys (the number of
/// distinct values an `=` of that selectivity implies), which sets its
/// bloom's false-positive rate at the lane's `k` ([`bloom_k`]).
pub fn bloom_fraction(granules: u64, tuples: f64, selectivity: f64, k: u32) -> f64 {
    let g = granules.max(1) as f64;
    let tuples = tuples.max(0.0);
    let sel = selectivity.clamp(0.0, 1.0);
    let matching = sel * tuples;
    let holds = 1.0 - (matching * (1.0 - 1.0 / g).ln()).exp();
    let distinct = if sel > 0.0 { (tuples / g).min(1.0 / sel) } else { tuples / g };
    (holds + (1.0 - holds) * bloom_false_positive(distinct, k)).clamp(0.0, 1.0)
}

fn decode_unit(codec: u8, k: &Knobs) -> (f64, bool) {
    match codec {
        CODEC_WORD | CODEC_DICT => (k.word, true),
        _ => (k.byref, false),
    }
}

/// Price the scan.
pub fn scan_cost(s: &Scan<'_>, k: &Knobs) -> ScanCost {
    let g = s.granules.max(1) as f64;
    let rows_per_granule = s.tuples / g;
    let zones = |attno: i16| s.zones.iter().find(|(a, _)| *a == attno).map(|(_, z)| *z);
    let ncodes = |attno: i16| s.lanes.iter().find(|l| l.attno == attno).map_or(0, |l| l.ncodes);

    // Survival fraction and, per qual, its own fraction and mixed share.
    let mut fraction = 1.0f64;
    let mut per_qual: Vec<(f64, f64)> = Vec::new();
    for q in s.quals {
        let (own, mixed) = match q.kind {
            QualKind::Zone { op, key } => match zones(q.attno) {
                Some(z) if z.granules > 0 => {
                    let c = z.census(op, key);
                    (c.survived / z.granules as f64, c.mixed / c.survived.max(1.0))
                }
                _ => (1.0, 1.0),
            },
            QualKind::Bloom { k } => (bloom_fraction(s.granules, s.tuples, q.selectivity, k), 1.0),
            QualKind::Dict | QualKind::Row => (1.0, 1.0),
        };
        fraction *= own;
        per_qual.push((own, mixed));
    }
    let granules_survived = (g * fraction).ceil().min(g);
    let rows_scanned = (granules_survived * rows_per_granule).min(s.tuples);
    let rows_out = s.rows.min(rows_scanned).max(0.0);

    let op = k.cpu_operator_cost;
    let mut out = ScanCost { granules_survived, rows_scanned, ..ScanCost::default() };
    out.run += k.granule * op * granules_survived;

    // Quals in order: each sees the previous one's survivors within the
    // survived granules; its lanes are decoded the first time a qual reads
    // them (driving), at the rows that qual sees.
    let mut decoded: Vec<i16> = Vec::new();
    let mut rows = rows_scanned;
    for (q, &(own, mixed)) in s.quals.iter().zip(&per_qual) {
        let evals = match q.kind {
            QualKind::Zone { .. } => rows * mixed,
            QualKind::Dict => rows.min(ncodes(q.attno) as f64 * fraction),
            QualKind::Bloom { .. } | QualKind::Row => rows,
        };
        // A dictionary qual's per-code charge is not per row: no fraction.
        let charge = if q.kind == QualKind::Dict { q.per_tuple } else { q.per_tuple * k.qual_fraction };
        out.run += charge * evals;
        for &a in &q.lanes {
            if decoded.contains(&a) {
                continue;
            }
            if let Some(l) = s.lanes.iter().find(|l| l.attno == a) {
                let (unit, word) = decode_unit(l.codec, k);
                let n = if matches!(q.kind, QualKind::Zone { .. }) { rows * mixed } else { rows };
                out.run += unit * op * n;
                if word {
                    out.word_rows += n;
                } else {
                    out.byref_rows += n;
                }
                decoded.push(a);
            }
        }
        let conditional = if own < 1.0 { (q.selectivity / own).min(1.0) } else { q.selectivity };
        rows *= conditional.clamp(0.0, 1.0);
    }

    // Late lanes for the survivors; dictionary payloads for output columns.
    for l in s.lanes {
        if !decoded.contains(&l.attno) {
            let (unit, word) = decode_unit(l.codec, k);
            out.run += unit * op * rows_out;
            if word {
                out.word_rows += rows_out;
            } else {
                out.byref_rows += rows_out;
            }
        }
        if l.codec == CODEC_DICT && l.in_target {
            out.run += k.byref * op * rows_out;
            out.byref_rows += rows_out;
        }
    }
    out.run += k.cpu_tuple_cost * k.tuple_fraction * rows_out;

    let touched: f64 = s.lanes.iter().map(|l| l.bytes as f64).sum::<f64>() * fraction;
    if touched > 0.0 {
        let cached = (k.effective_cache_bytes / touched).clamp(0.0, 1.0);
        out.run += k.seq_page_cost * touched / BLCKSZ * (1.0 - cached);
    }
    out
}
