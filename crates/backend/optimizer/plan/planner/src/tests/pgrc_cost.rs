use crate::costsize::pgrc_cost::*;
use mcx::{Mcx, MemoryContext};

const TUPLES: f64 = 100_000_000.0;
const GRANULES: u64 = 12_208;
const BUCKETS: usize = 256;

struct Summary {
    granules: u64,
    keyed: u64,
    keys: Vec<i64>,
    min_lt: Vec<u32>,
    min_le: Vec<u32>,
    max_lt: Vec<u32>,
    max_le: Vec<u32>,
    eq_whole: Vec<u32>,
}

impl Summary {
    fn of(ranges: &[(i64, i64)]) -> Summary {
        let mut mins: Vec<i64> = ranges.iter().filter(|(lo, hi)| lo <= hi).map(|r| r.0).collect();
        let mut maxs: Vec<i64> = ranges.iter().filter(|(lo, hi)| lo <= hi).map(|r| r.1).collect();
        mins.sort_unstable();
        maxs.sort_unstable();
        let n = mins.len();
        let mut keys = Vec::new();
        for i in 0..=BUCKETS {
            if n > 0 {
                let r = (i * n / BUCKETS).min(n - 1);
                keys.push(mins[r]);
                keys.push(maxs[r]);
            }
        }
        keys.sort_unstable();
        keys.dedup();
        let rank = |v: &[i64], k: i64, le: bool| v.partition_point(|&x| if le { x <= k } else { x < k }) as u32;
        Summary {
            granules: ranges.len() as u64,
            keyed: n as u64,
            min_lt: keys.iter().map(|&k| rank(&mins, k, false)).collect(),
            min_le: keys.iter().map(|&k| rank(&mins, k, true)).collect(),
            max_lt: keys.iter().map(|&k| rank(&maxs, k, false)).collect(),
            max_le: keys.iter().map(|&k| rank(&maxs, k, true)).collect(),
            eq_whole: keys.iter().map(|&k| ranges.iter().filter(|r| r.0 == k && r.1 == k).count() as u32).collect(),
            keys,
        }
    }

    fn view(&self) -> ZoneSummary<'_> {
        ZoneSummary { granules: self.granules, keyed: self.keyed, keys: &self.keys, min_lt: &self.min_lt, min_le: &self.min_le, max_lt: &self.max_lt, max_le: &self.max_le, eq_whole: &self.eq_whole }
    }
}

/// Item 2: the summary's census against the exact walk, on a sorted key's
/// layout, an unsorted layout, a few-valued column and pseudo-random int8
/// bounds, for every operator and keys on and off the grid: exact on the
/// grid, within ⌈keyed / 256⌉ granules per count off it (`Eq` subtracts
/// two counts: twice that, and its decided granules — 0 off the grid —
/// once more for `mixed`), and never optimistic about empty granules.
#[test]
fn summary_census_is_exact_on_the_grid_and_bounded_off_it() {
    let mut sorted: Vec<(i64, i64)> = (0..GRANULES as i64).map(|g| (11 + g / 2, 11 + (g + 1) / 2)).collect();
    sorted.push((i64::MAX, i64::MIN));
    let unsorted = vec![(11i64, 262_143i64); GRANULES as usize];
    let dates: Vec<(i64, i64)> = (0..GRANULES as i64).map(|g| (19_783 + g % 7, 19_783 + (g % 7).max(g % 11 % 7))).collect();
    let mut x = 0x2545_f491_4f6c_dd1du64;
    let wide: Vec<(i64, i64)> = (0..GRANULES)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let lo = (x >> 1) as i64 - (1 << 62);
            (lo, lo.saturating_add((x % 1_000_000_000) as i64))
        })
        .collect();
    let ops = [ZoneOp::Eq, ZoneOp::Lt, ZoneOp::Le, ZoneOp::Gt, ZoneOp::Ge];
    for (name, ranges) in [("sorted", &sorted), ("unsorted", &unsorted), ("dates", &dates), ("wide", &wide)] {
        let s = Summary::of(ranges);
        let v = s.view();
        let bound = (s.keyed as usize).div_ceil(BUCKETS) as f64;
        let mut probes: Vec<i64> = s.keys.clone();
        for w in s.keys.windows(2) {
            probes.push(w[0] + (w[1] - w[0]) / 2);
            probes.push(w[0] + 1);
        }
        probes.extend([i64::MIN, i64::MAX, 0, 10, 11, 40, 262_143, 262_144, 19_782, 19_783, 19_789, 19_790]);
        for &key in &probes {
            for op in ops {
                let exact = census(ranges, op, key);
                let est = v.census(op, key);
                let (tol_s, tol_m) = if op == ZoneOp::Eq { (2.0 * bound, 3.0 * bound) } else { (bound, bound) };
                let on_grid = s.keys.binary_search(&key).is_ok() || key < s.keys[0] || key > *s.keys.last().unwrap();
                let (ds, dm) = ((est.survived - exact.survived as f64).abs(), (est.mixed - exact.mixed as f64).abs());
                if on_grid {
                    assert!(ds < 1e-9 && dm < 1e-9, "{name} {op:?} {key}: on the grid, summary {est:?} vs exact {exact:?}");
                } else {
                    assert!(ds <= tol_s && dm <= tol_m, "{name} {op:?} {key}: summary {est:?} vs exact {exact:?}, bounds {tol_s}/{tol_m}");
                }
                assert!(est.survived <= s.keyed as f64 && est.mixed <= est.survived + 1e-9);
            }
        }
        if name == "dates" {
            assert!(s.keys.len() <= 7, "seven dates: the whole domain is on the grid");
        }
    }
}

fn knobs() -> Knobs {
    Knobs {
        cpu_tuple_cost: 0.01,
        cpu_operator_cost: 0.0025,
        seq_page_cost: 1.0,
        effective_cache_bytes: 24.0 * 1024.0 * 1024.0 * 1024.0,
        granule: crate::costsize::gucs::pgrc_granule_cost(),
        word: crate::costsize::gucs::pgrc_decode_word_cost(),
        byref: crate::costsize::gucs::pgrc_decode_byref_cost(),
        tuple_fraction: crate::costsize::gucs::pgrc_tuple_cost_fraction(),
        qual_fraction: crate::costsize::gucs::pgrc_qual_cost_fraction(),
    }
}

/// `site_id` on its `sorted_by` layout (a 100 M-row `sightings`): 6,105 values over 12,208 granules
/// in contiguous slices — `= 40` keeps the one or two granules that
/// hold it, a range keeps its slice, an empty granule never survives.
#[test]
fn census_is_exact_on_a_sorted_layout() {
    let mut ranges: Vec<(i64, i64)> = (0..GRANULES as i64).map(|g| (11 + g / 2, 11 + (g + 1) / 2)).collect();
    ranges.push((i64::MAX, i64::MIN));
    let eq = census(&ranges, ZoneOp::Eq, 40);
    assert_eq!(eq, Census { survived: 3, mixed: 2 });
    let lt = census(&ranges, ZoneOp::Lt, 14);
    assert_eq!(lt.survived, 6, "granules with min < 14");
    assert_eq!(lt.mixed, 1, "only the granule spanning 13..14 needs the row check");
    assert_eq!(census(&ranges, ZoneOp::Ge, 11).survived, GRANULES);
    assert_eq!(census(&ranges, ZoneOp::Gt, 11 + 6104).survived, 0);
    assert_eq!(census(&ranges, ZoneOp::Le, 10).survived, 0);
}

#[test]
fn census_gives_no_credit_on_an_unsorted_layout() {
    let ranges = vec![(11i64, 262_143i64); GRANULES as usize];
    let c = census(&ranges, ZoneOp::Eq, 40);
    assert_eq!(c, Census { survived: GRANULES, mixed: GRANULES });
    assert_eq!(bloom_fraction(GRANULES, TUPLES, 1.0, 5), 1.0, "a value in every granule");
    let six = bloom_fraction(GRANULES, TUPLES, 6.0 / TUPLES, 5);
    let fpr = (1.0 - (-5.0 * (TUPLES / GRANULES as f64) / 65536.0).exp()).powi(5);
    assert!(fpr < 1.0 / 32.0);
    assert!(six > fpr && six < fpr + 0.001, "six rows: ~6 granules + false positives, got {six}");
}

#[test]
fn bloom_false_positive_follows_the_keys_per_granule() {
    assert_eq!(bloom_false_positive(0.0, 5), 0.0);
    assert_eq!((bloom_k(1), bloom_k(4), bloom_k(8), bloom_k(16)), (4, 5, 6, 7));
    for d in [2.0, 50.0, 100.0, 1000.0, 5000.0, 8192.0] {
        for k in [4, 5, 6, 7] {
            let f = bloom_false_positive(d, k);
            assert!(f > 0.0 && f <= 0.5f64.powi(k as i32) * 1.05, "{d} keys, k {k}: {f}");
        }
    }
    let thousand = bloom_false_positive(1000.0, 5);
    assert!((thousand - (1.0 - (-5000.0f64 / 8192.0).exp()).powi(5)).abs() < 1e-12, "got {thousand}");
    let g = 1_000u64;
    let tuples = 1_000_000.0;
    let f = bloom_fraction(g, tuples, 1.0 / 50.0, 5);
    let holds = 1.0 - (20_000.0f64 * (1.0 - 1.0 / g as f64).ln()).exp();
    let want = holds + (1.0 - holds) * bloom_false_positive(50.0, 5);
    assert!((f - want).abs() < 1e-12, "got {f}, want {want}");
}

fn word_lane(attno: i16, bytes: u64) -> Lane {
    Lane { attno, codec: CODEC_WORD, bytes, ncodes: 0, in_target: true }
}

/// §3.0.2 (item 11b): the heap arm of `cost_seqscan` is PostgreSQL's
/// formula, byte-identical — `cpu_tuple_cost × tuples + seq_page_cost ×
/// pages` for the fixture's 10 000-row, 100-page heap `t` — whatever the
/// pgrc fractions and knobs are (they are read by the pgrc arm only).
#[test]
fn heap_arm_is_postgresqls_formula() {
    use super::plan_shapes::{count_star, plan_of, query, setup};
    let _guc = super::GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let parse = query(mcx, None, &[(count_star(mcx), "count", 0)], &[], None);
    let plan = plan_of(mcx, parse, "SELECT count(*) FROM t");
    let scan = plan.as_plan().unwrap().lefttree.expect("Agg over Scan");
    assert_eq!(scan.node_tag(), types_nodes::NodeTag::T_SeqScan);
    let scan = scan.as_plan().unwrap();
    assert_eq!(scan.startup_cost, 0.0);
    assert_eq!(scan.total_cost, 0.01 * 10_000.0 + 1.0 * 100.0);
}

/// §3.0.2 (item 11b): the pgrc arm never prices a per-row qual at zero —
/// a `LIKE` on a non-dictionary text column keeps `qual_fraction ×` its
/// operator charge per scanned row even with every decode knob at 0 and no
/// output rows; and the fractions themselves are positive.
#[test]
fn per_row_qual_keeps_a_positive_charge() {
    let k0 = knobs();
    assert!(k0.tuple_fraction > 0.0 && k0.qual_fraction > 0.0, "{k0:?}");
    let lanes = [Lane { attno: 30, codec: CODEC_TEXT, bytes: 9_000_000_000, ncodes: 0, in_target: false }];
    let like = [Qual { kind: QualKind::Row, attno: 30, selectivity: 0.0, per_tuple: 0.0025, lanes: vec![30] }];
    let s = Scan { tuples: TUPLES, rows: 0.0, granules: GRANULES, lanes: &lanes, quals: &like, zones: &[] };
    let k = Knobs { granule: 0.0, word: 0.0, byref: 0.0, ..k0 };
    let c = scan_cost(&s, &k);
    let floor = k.qual_fraction * 0.0025 * TUPLES;
    assert!(floor > 0.0 && (c.run - floor).abs() < 1e-6 * floor, "{} vs floor {floor}", c.run);
}

#[test]
fn fractions_scale_only_the_per_row_fixed_terms() {
    let sorted: Vec<(i64, i64)> = (0..GRANULES as i64).map(|g| (11 + g / 2, 11 + (g + 1) / 2)).collect();
    let lanes = [
        word_lane(6, 380_000),
        Lane { attno: 28, codec: CODEC_DICT, bytes: 300_000_000, ncodes: 5_800_000, in_target: false },
        Lane { attno: 30, codec: CODEC_TEXT, bytes: 9_000_000_000, ncodes: 0, in_target: false },
        word_lane(8, 800_000_000),
    ];
    let quals = [
        Qual { kind: QualKind::Zone { op: ZoneOp::Eq, key: 40 }, attno: 6, selectivity: 0.0075, per_tuple: 0.0025 * 0.375, lanes: vec![6] },
        Qual { kind: QualKind::Dict, attno: 28, selectivity: 0.18, per_tuple: 0.0025 * 0.625, lanes: vec![28] },
        Qual { kind: QualKind::Row, attno: 30, selectivity: 0.5, per_tuple: 0.0025, lanes: vec![30] },
    ];
    let sum6 = Summary::of(&sorted);
    let s = Scan { tuples: TUPLES, rows: 10_000.0, granules: GRANULES, lanes: &lanes, quals: &quals, zones: &[(6, sum6.view())] };
    let one = Knobs { tuple_fraction: 1.0, qual_fraction: 1.0, ..knobs() };
    let two = Knobs { tuple_fraction: 2.0, qual_fraction: 2.0, ..one };
    let (c1, c2) = (scan_cost(&s, &one), scan_cost(&s, &two));
    assert_eq!((c1.granules_survived, c1.word_rows, c1.byref_rows), (c2.granules_survived, c2.word_rows, c2.byref_rows));
    // The per-row terms at fraction 1: the zone qual on its mixed rows, the LIKE on the dictionary
    // qual's survivors, cpu_tuple_cost on the output rows. Everything else is the same in both.
    let rows_scanned = c1.rows_scanned;
    let est = sum6.view().census(ZoneOp::Eq, 40);
    let zone_rows = rows_scanned * est.mixed / est.survived.max(1.0);
    let after_zone = rows_scanned * (0.0075 / (est.survived / GRANULES as f64)).min(1.0);
    let after_dict = after_zone * 0.18;
    let per_row = quals[0].per_tuple * zone_rows + quals[2].per_tuple * after_dict + 0.01 * rows_scanned.min(10_000.0);
    assert!(per_row > 0.0);
    assert!((c2.run - c1.run - per_row).abs() < 1e-6 * per_row, "delta {} vs per-row terms {per_row}", c2.run - c1.run);
    // Zero output rows and the fractions at their floor still pay the per-row quals (never free).
    let none = Scan { rows: 0.0, ..s };
    let tiny = Knobs { tuple_fraction: 0.01, qual_fraction: 0.01, granule: 0.0, word: 0.0, byref: 0.0, ..one };
    assert!(scan_cost(&none, &tiny).run > 0.0);
}

/// The census prices the pruned scan: `site_id = 40` on the sorted
/// layout scans the granules holding it (three exactly; the summary's
/// estimate between the grid keys around 40 is within its bound and
/// keeps the scan at a handful), on an unsorted layout all of them, and
/// uninformative ranges cost exactly what no ranges cost.
#[test]
fn zone_qual_prunes_only_with_informative_ranges() {
    let sorted: Vec<(i64, i64)> = (0..GRANULES as i64).map(|g| (11 + g / 2, 11 + (g + 1) / 2)).collect();
    let unsorted = vec![(11i64, 262_143i64); GRANULES as usize];
    let lanes = [word_lane(6, 380_000), word_lane(8, 800_000_000)];
    let q = [Qual { kind: QualKind::Zone { op: ZoneOp::Eq, key: 40 }, attno: 6, selectivity: 0.0075, per_tuple: 0.0025 * 0.375, lanes: vec![6] }];
    fn price(lanes: &[Lane], quals: &[Qual], zones: &[(i16, ZoneSummary<'_>)]) -> ScanCost {
        scan_cost(&Scan { tuples: TUPLES, rows: 750_000.0, granules: GRANULES, lanes, quals, zones }, &knobs())
    }
    let (ss, us) = (Summary::of(&sorted), Summary::of(&unsorted));
    let pruned = price(&lanes, &q, &[(6, ss.view())]);
    let full = price(&lanes, &q, &[(6, us.view())]);
    let none = price(&lanes, &q, &[]);
    assert_eq!(census(&sorted, ZoneOp::Eq, 40).survived, 3);
    assert!(pruned.granules_survived >= 1.0 && pruned.granules_survived <= 3.0 + 2.0 * (GRANULES as f64 / BUCKETS as f64).ceil(), "survived {}", pruned.granules_survived);
    assert_eq!(full.granules_survived, GRANULES as f64);
    assert_eq!(full, none);
    assert!(pruned.run < full.run / 100.0, "pruned {} vs full {}", pruned.run, full.run);
}

#[test]
fn dictionary_qual_is_charged_per_code() {
    let lanes = [Lane { attno: 28, codec: CODEC_DICT, bytes: 300_000_000, ncodes: 5_800_000, in_target: true }];
    let q = [Qual { kind: QualKind::Dict, attno: 28, selectivity: 0.18, per_tuple: 0.0025 * 0.625, lanes: vec![28] }];
    let s = Scan { tuples: TUPLES, rows: 18_000_000.0, granules: GRANULES, lanes: &lanes, quals: &q, zones: &[] };
    let k = knobs();
    let c = scan_cost(&s, &k);
    let expected = k.granule * k.cpu_operator_cost * GRANULES as f64
        + q[0].per_tuple * 5_800_000.0
        + k.word * k.cpu_operator_cost * TUPLES
        + k.byref * k.cpu_operator_cost * 18_000_000.0
        + 0.01 * k.tuple_fraction * 18_000_000.0;
    assert!((c.run - expected).abs() < 1e-6 * expected, "{} vs {expected}", c.run);
    assert_eq!((c.word_rows, c.byref_rows), (TUPLES, 18_000_000.0));
}

/// The io term follows PostgreSQL's cache-fraction shape: nothing while
/// the touched bytes fit `effective_cache_size`, the uncached share of
/// the pages at `seq_page_cost` beyond it.
#[test]
fn io_term_charges_the_uncached_pages() {
    let lanes = [word_lane(1, 8_192_000_000)];
    let s = Scan { tuples: TUPLES, rows: TUPLES, granules: GRANULES, lanes: &lanes, quals: &[], zones: &[] };
    let mut k = knobs();
    let resident = scan_cost(&s, &k);
    k.effective_cache_bytes = 4_096_000_000.0;
    let half = scan_cost(&s, &k);
    assert!((half.run - resident.run - 500_000.0).abs() < 1e-3, "{} - {}", half.run, resident.run);
}


use types_pathnodes::{AggPath, Path, PathKey, PathNode, RelId, AGGSPLIT_SIMPLE, AGG_HASHED, AGG_SORTED, COMPARE_LT};

fn agg_path<'m>(mcx: Mcx<'m>, rel: RelId, strategy: u32, total: f64, sorted: bool) -> PathNode<'m> {
    let mut pathkeys = mcx::PgVec::new_in(mcx);
    if sorted {
        pathkeys.push(PathKey { pk_eclass: None, pk_opfamily: 1976, pk_cmptype: COMPARE_LT, pk_nulls_first: false });
    }
    PathNode::AggPath(AggPath {
        path: Path {
            type_: 0,
            pathtype: 0,
            parent: rel,
            pathtarget_id: None,
            param_info: None,
            parallel_aware: false,
            parallel_safe: true,
            parallel_workers: 0,
            rows: 100.0,
            disabled_nodes: 0,
            startup_cost: total * 0.99,
            total_cost: total,
            pathkeys,
        },
        subpath: None,
        aggstrategy: strategy,
        aggsplit: AGGSPLIT_SIMPLE,
        numGroups: 100.0,
        transitionSpace: 0,
        groupClause: mcx::PgVec::new_in(mcx),
        qual: mcx::PgVec::new_in(mcx),
        distinctHashed: false,
        gravity_rewrite: None,
    })
}

fn survivors(columnar: bool, first: (u32, f64, bool), second: (u32, f64, bool)) -> Vec<u32> {
    let cx = MemoryContext::new_bump("pgrc-tie");
    let mcx = cx.mcx();
    let mut run = types_pathnodes::run::PlannerRun::new(mcx);
    let mut base = types_pathnodes::RelOptInfo::new(mcx);
    if columnar {
        base.amflags |= types_pathnodes::AMFLAG_PGRCOLUMNAR;
    }
    let base = run.root.alloc_rel(base);
    run.root.simple_rel_array.push(Some(base));
    let grouped = run.root.alloc_rel(types_pathnodes::RelOptInfo::new(mcx));
    for (strategy, total, sorted) in [first, second] {
        let id = run.root.alloc_path(agg_path(mcx, grouped, strategy, total, sorted));
        crate::pathnode::add_path(&mut run, grouped, id);
    }
    run.root
        .rel(grouped)
        .pathlist
        .iter()
        .map(|&p| match run.root.path(p) {
            PathNode::AggPath(a) => a.aggstrategy,
            _ => unreachable!(),
        })
        .collect()
}

/// Inside `STD_FUZZ_FACTOR` the hashed aggregate wins on a pgrcolumnar-fed
/// plan in either insertion order (PostgreSQL's rule would keep the
/// presorted path for its pathkeys); on a heap plan and outside the fuzz
/// PostgreSQL's rules stand.
#[test]
fn hashed_aggregate_wins_the_fuzzy_tie_on_pgrc() {
    let sorted = (AGG_SORTED, 1_000.0, true);
    let hashed_dearer = (AGG_HASHED, 1_005.0, false);
    let hashed_cheaper = (AGG_HASHED, 995.0, false);
    assert_eq!(survivors(true, sorted, hashed_dearer), vec![AGG_HASHED]);
    assert_eq!(survivors(true, hashed_dearer, sorted), vec![AGG_HASHED]);
    assert_eq!(survivors(true, sorted, hashed_cheaper), vec![AGG_HASHED]);
    assert_eq!(survivors(false, sorted, hashed_dearer), vec![AGG_SORTED], "heap plan: pathkeys break the tie");
    assert_eq!(survivors(false, sorted, hashed_cheaper), vec![AGG_SORTED], "heap plan: pathkeys break the tie");
    assert_eq!(survivors(true, sorted, (AGG_HASHED, 1_030.0, false)), vec![AGG_SORTED], "outside the fuzz: cost decides");
}
