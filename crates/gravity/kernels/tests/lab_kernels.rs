use gravity_kernels_rig as common;

use common::{allowed, row, subset, Ret, Rig, Row};
use gravity_abi::site::site;
use gravity_abi::Datum;
use gravity_kernels::{par_hashjoin, par_scan_count};
use gravity_program::hand::{self as programs, Oids};

const PAR_BLOCKS: u64 = 6;
const PAR_PER_BLOCK: u64 = 10;

fn par_rows(keys: &[Datum], f: impl Fn(u64) -> Vec<Datum>) -> Vec<Row> {
    let (lo, hi) = (keys[0], keys[1].min(PAR_BLOCKS));
    (lo..hi).flat_map(|b| (0..PAR_PER_BLOCK).map(move |i| b * PAR_PER_BLOCK + i)).map(|r| row(&f(r))).collect()
}

fn t_a(keys: &[Datum], _: &[u8]) -> Vec<Row> {
    par_rows(keys, |r| vec![r])
}

fn t_kv(keys: &[Datum], _: &[u8]) -> Vec<Row> {
    par_rows(keys, |r| vec![r % 7, r])
}

fn t_inner(keys: &[Datum], _: &[u8]) -> Vec<Row> {
    par_rows(keys, |r| if r % 3 == 0 { vec![r % 11, r * 10] } else { vec![] })
        .into_iter()
        .filter(|r| !r.0.is_empty())
        .collect()
}

fn t_outer(keys: &[Datum], _: &[u8]) -> Vec<Row> {
    par_rows(keys, |r| vec![r % 11, r])
}

fn t_kv_nulls(keys: &[Datum], _: &[u8]) -> Vec<Row> {
    par_rows(keys, |r| vec![r % 7, r]).into_iter().map(|(v, mut n)| {
        n[0] = (v[1] % 5 == 0) as u8;
        n[1] = (v[1] % 4 == 0) as u8;
        (v, n)
    }).collect()
}

#[test]
fn par_scan_count_takes_an_all_match_window_whole_without_reading_the_lane() {
    // 60 rows in windows of 4: windows 1-8 (32 rows) filtered by `a < 25`, windows 9-15 (28 rows) AllMatch with lane 0 undecoded
    let mut rig = Rig::new(programs::par_scan_count(Oids::T_A), vec![25]);
    rig.site(0).rows_for = t_a;
    rig.site(0).all_match_from = Some(9);
    let (rows, rets) = rig.run_pipelines();
    assert_eq!(rets, vec![Ret::Done, Ret::Done]);
    let filtered: i64 = t_a(&[0, u64::MAX], &[0, 0]).iter().take(32).filter(|(v, n)| n[0] == 0 && (v[0] as u32 as i32) < 25).count() as i64;
    assert_eq!(rows, vec![vec![(filtered + 28) as u64]]);
    assert_eq!(rig.cell(gravity_abi::ctx::site_off(0) + site::ROWS_OUT), (filtered + 28) as u64);
    // the answered form: windows 9-15 arrive as `rows` with no lanes
    let mut rig = Rig::new(programs::par_scan_count(Oids::T_A), vec![25]);
    rig.site(0).rows_for = t_a;
    rig.site(0).answered_from = Some(9);
    let (rows, rets) = rig.run_pipelines();
    assert_eq!(rets, vec![Ret::Done, Ret::Done]);
    assert_eq!(rows, vec![vec![(filtered + 28) as u64]]);
}

#[test]
fn par_hashagg_codes_counts_by_part_and_code_and_refuses_a_datum_lane() {
    // 60 rows, code = r % 7 (t_kv's first column): seven groups keyed (part 0 << 20 | code)
    let mut rig = Rig::new(programs::par_hashagg_codes(Oids::T_KV), vec![]);
    rig.site(0).rows_for = t_kv;
    rig.site(0).codes_lanes = 1;
    let (rows, rets) = rig.run_pipelines();
    assert_eq!(rets, vec![Ret::Done, Ret::Done]);
    // the emit walks the unfolded table: `sum` (the value datum after the leader's fold, 0 here) and the count per code
    let mut counts: Vec<u64> = rows.iter().map(|r| r[1]).collect();
    counts.sort_unstable();
    let mut want: Vec<u64> = (0..7u64).map(|c| (0..60u64).filter(|r| r % 7 == c).count() as u64).collect();
    want.sort_unstable();
    assert_eq!(counts, want);
    assert_eq!(rig.cell(gravity_abi::ctx::site_off(0) + site::ROWS_OUT), 60);
    // the same rows through a datum lane are refused: the kernel needs codes
    let mut rig = Rig::new(programs::par_hashagg_codes(Oids::T_KV), vec![]);
    rig.site(0).rows_for = t_kv;
    let (_, rets) = rig.run_pipelines();
    assert!(matches!(rets[0], Ret::Unsupported { .. }), "{rets:?}");
}

#[test]
fn par_topk_keeps_the_n_smallest_rows_in_key_order() {
    // t_kv rows (r % 7, r) for r in 0..60 with the key r % 7 in column 0: the 5 smallest keys are five zeros (r = 0, 7, .., 28)
    let mut rig = Rig::new(programs::par_topk(Oids::T_KV, 5), vec![5, 2]);
    rig.site(0).rows_for = t_kv;
    let (rows, rets) = rig.run_pipelines();
    assert_eq!(rets, vec![Ret::Done, Ret::Done]);
    assert_eq!(rows.len(), 5);
    assert!(rows.iter().all(|r| r[0] == 0), "{rows:?}");
    let mut seconds: Vec<u64> = rows.iter().map(|r| r[1]).collect();
    seconds.sort_unstable();
    assert_eq!(seconds, vec![0, 7, 14, 21, 28]);
    // n larger than the rows: every row, sorted by key
    let mut rig = Rig::new(programs::par_topk(Oids::T_KV, 100), vec![100, 2]);
    rig.site(0).rows_for = t_kv;
    let (rows, _) = rig.run_pipelines();
    assert_eq!(rows.len(), 60);
    assert!(rows.windows(2).all(|w| w[0][0] <= w[1][0]), "sorted by key");
}

#[test]
fn par_scan_count_counts_the_claim_and_emits_once() {
    let mut rig = Rig::new(programs::par_scan_count(Oids::T_A), vec![25]);
    rig.site(0).rows_for = t_a;
    let before = rig.snapshot();
    let (rows, rets) = rig.run_pipelines();
    assert_eq!(rets, vec![Ret::Done, Ret::Done]);
    assert_eq!(rows, vec![vec![25]]);
    assert!(subset(&rig.changed_cells(&before), &allowed(&par_scan_count::TOUCH)), "{:?}", rig.changed_cells(&before));
    assert_eq!((rig.windows(0), rig.nexts(0)), (16, 0), "60 rows in windows of 4 plus the empty one; no host call in the row loop");
    assert_eq!(rig.cell(gravity_abi::ctx::site_off(0) + site::ROWS_IN), 60);
    assert_eq!(rig.cell(gravity_abi::ctx::site_off(0) + site::ROWS_OUT), 25);
    let mut rig = Rig::new(programs::par_scan_count(Oids::T_A), vec![25]);
    rig.site(0).rows_for = t_a;
    rig.set_claim(1, 3);
    assert_eq!(rig.call_pipeline(0), Ret::Done);
    assert_eq!(rig.site(0).positioned, vec![vec![1, 3]], "a range site is sought with the claim");
    let count = |rig: &Rig| unsafe { gravity_abi::breaker::HashAgg::view(rig.partial.as_ptr() as *const u8) }.record(0).count(0);
    assert_eq!(count(&rig), 15, "rows 10..25 of blocks 1 and 2");
    rig.set_claim(3, 6);
    assert_eq!(rig.call_pipeline(0), Ret::Done);
    assert_eq!(count(&rig), 15, "the partial accumulates across claims");
}

#[test]
fn par_scan_count_honours_validity_and_a_null_threshold_and_the_abort_byte_once_per_window() {
    let mut rig = Rig::new(programs::par_scan_count(Oids::T_A), vec![30]);
    rig.site(0).rows_for = |keys, n| t_kv_nulls(keys, n).into_iter().map(|(v, n)| (vec![v[1]], vec![n[1]])).collect();
    rig.site(0).granule = 64;
    let (rows, _) = rig.run_pipelines();
    assert_eq!(rows, vec![vec![(0..30u64).filter(|r| r % 4 != 0).count() as u64]], "NULL rows do not count");
    let mut rig = Rig::with_nulls(programs::par_scan_count(Oids::T_A), vec![30], vec![1]);
    rig.site(0).rows_for = t_a;
    let (rows, _) = rig.run_pipelines();
    assert_eq!(rows, vec![vec![0]], "a NULL threshold counts nothing");
    let mut rig = Rig::new(programs::par_scan_count(Oids::T_A), vec![30]);
    rig.site(0).rows_for = t_a;
    rig.site(0).granule = 10;
    *rig.interrupt = 1;
    assert_eq!(rig.call_pipeline(0), Ret::Aborted, "the abort byte is read once per window");
    assert_eq!(rig.windows(0), 1, "one window fetched, then the byte");
    let mut rig = Rig::new(programs::par_scan_count(Oids::T_A), vec![30]);
    rig.site(0).rows_for = t_a;
    rig.site(0).fail_next_after = Some(2);
    assert_eq!(rig.call_pipeline(0), Ret::Error);
}

#[test]
fn par_hashagg_groups_and_the_emit_resumes_under_stop() {
    let mut rig = Rig::new(programs::par_hashagg_with(Oids::T_KV, 2, 0), gravity_kernels::table::params::DEFAULT_PARAMS.to_vec());
    rig.site(0).rows_for = t_kv;
    rig.host.stop_after_rows = Some(3);
    let (mut rows, rets) = rig.run_pipelines();
    assert_eq!(rets[0], Ret::Done);
    assert!(matches!(rets[1], Ret::Suspended { .. }), "{rets:?}");
    assert_eq!(*rets.last().unwrap(), Ret::Done);
    rows.sort();
    let expect: Vec<Vec<Datum>> = (0..7u64).map(|k| vec![k, (0..60).filter(|r| r % 7 == k).sum(), (0..60).filter(|r| r % 7 == k).count() as u64]).collect();
    assert_eq!(rows, expect);
    assert_eq!(rig.nexts(0), 0);
    let mut rig = Rig::new(programs::par_hashagg_with(Oids::T_KV, 64, 0), gravity_kernels::table::params::DEFAULT_PARAMS.to_vec());
    rig.site(0).rows_for = t_kv_nulls;
    rig.site(0).granule = 7;
    let (mut rows, rets) = rig.run_pipelines();
    assert_eq!(rets, vec![Ret::Done, Ret::Done]);
    rows.sort();
    let live = |k: u64| (0..60u64).filter(move |r| r % 7 == k && r % 5 != 0);
    let expect: Vec<Vec<Datum>> = (0..7u64).map(|k| vec![k, live(k).filter(|r| r % 4 != 0).sum(), live(k).count() as u64]).collect();
    assert_eq!(rows, expect, "a NULL key drops the row, a NULL value adds nothing to the sum");
}

#[test]
fn par_hashjoin_builds_then_probes_in_pipeline_order() {
    let mut rig = Rig::new(programs::par_hashjoin_with(Oids::T_INNER, Oids::T_OUTER, 8, 0, 0), par_hashjoin::DEFAULT_PARAMS.to_vec());
    rig.site(par_hashjoin::INNER).rows_for = t_inner;
    rig.site(par_hashjoin::OUTER).rows_for = t_outer;
    let (mut rows, rets) = rig.run_pipelines();
    assert_eq!(rets, vec![Ret::Done, Ret::Done, Ret::Done], "build, probe, and the (empty) third phase");
    rows.sort();
    let inner: Vec<(u64, u64)> = (0..60u64).filter(|r| r % 3 == 0).map(|r| (r % 11, r * 10)).collect();
    let mut expect: Vec<Vec<Datum>> = (0..60u64).flat_map(|r| inner.iter().filter(move |(k, _)| *k == r % 11).map(move |(k, a)| vec![*k, *a, r])).collect();
    expect.sort();
    assert_eq!(rows, expect);
    assert_eq!(rig.site(par_hashjoin::INNER).positioned, vec![vec![0, u64::MAX]]);
    assert_eq!(rig.site(par_hashjoin::OUTER).positioned, vec![vec![0, u64::MAX]]);
    assert_eq!((rig.nexts(0), rig.nexts(1)), (0, 0));
}

#[test]
fn par_hashjoin_probe_suspends_mid_chain_and_resumes_from_the_cont_cell() {
    let mut rig = Rig::new(programs::par_hashjoin_with(Oids::T_INNER, Oids::T_OUTER, 4, 0, 0), par_hashjoin::DEFAULT_PARAMS.to_vec());
    rig.site(par_hashjoin::INNER).rows_for = t_inner;
    rig.site(par_hashjoin::OUTER).rows_for = t_outer;
    rig.site(par_hashjoin::OUTER).granule = 16;
    rig.host.stop_after_rows = Some(3);
    let (mut rows, rets) = rig.run_pipelines();
    assert_eq!(rets[0], Ret::Done);
    assert!(rets.len() > 4 && rets[1..rets.len() - 2].iter().all(|r| matches!(r, Ret::Suspended { .. })), "{rets:?}");
    assert_eq!(&rets[rets.len() - 2..], &[Ret::Done, Ret::Done], "the probe finishes, then the empty third phase");
    assert_eq!(rig.windows(par_hashjoin::OUTER), 5, "60 outer rows in windows of 16: the suspends stay inside a window");
    rows.sort();
    let inner: Vec<(u64, u64)> = (0..60u64).filter(|r| r % 3 == 0).map(|r| (r % 11, r * 10)).collect();
    let mut expect: Vec<Vec<Datum>> = (0..60u64).flat_map(|r| inner.iter().filter(move |(k, _)| *k == r % 11).map(move |(k, a)| vec![*k, *a, r])).collect();
    expect.sort();
    assert_eq!(rows, expect, "every match once across the suspends");
}

#[test]
fn par_hashagg_uses_the_dense_accumulator_inside_the_domain_and_the_hashed_path_outside() {
    use gravity_kernels::table::params::{DEFAULT_PARAMS, PARAM_DENSE_HI, PARAM_DENSE_LO};
    let expect: Vec<Vec<Datum>> = (0..7u64).map(|k| vec![k, (0..60).filter(|r| r % 7 == k).sum(), (0..60).filter(|r| r % 7 == k).count() as u64]).collect();
    // the exact domain, a superset and a subset (keys outside take the hashed path) all agree
    for (lo, hi) in [(0i64, 6i64), (-100, 500), (2, 4)] {
        let mut params = DEFAULT_PARAMS;
        params[PARAM_DENSE_LO] = lo as u64;
        params[PARAM_DENSE_HI] = hi as u64;
        let mut rig = Rig::new(programs::par_hashagg_with(Oids::T_KV, 64, 0), params.to_vec());
        rig.site(0).rows_for = t_kv;
        let (mut rows, rets) = rig.run_pipelines();
        assert_eq!(rets, vec![Ret::Done, Ret::Done], "domain [{lo}, {hi}]");
        rows.sort();
        assert_eq!(rows, expect, "domain [{lo}, {hi}]");
    }
    // no domain: the hashed path alone
    let mut rig = Rig::new(programs::par_hashagg_with(Oids::T_KV, 64, 0), DEFAULT_PARAMS.to_vec());
    rig.site(0).rows_for = t_kv;
    let (mut rows, _) = rig.run_pipelines();
    rows.sort();
    assert_eq!(rows, expect);
}

#[test]
fn par_hashagg_dense_accumulator_folds_nulls_and_a_late_nullable_window_like_the_records() {
    use gravity_kernels::table::params::{DEFAULT_PARAMS, PARAM_DENSE_HI, PARAM_DENSE_LO};
    // 60 rows in windows of 8: rows 0..40 all valid (the two-add loop), a null v every 4th row from
    // 40 on (the checked loop, `nn` made explicit mid-claim), rows 56.. key 8 with v always null
    let gen = |keys: &[Datum], _: &[u8]| -> Vec<Row> {
        par_rows(keys, |r| if r >= 56 { vec![8, r] } else { vec![r % 7, r] })
            .into_iter()
            .map(|(v, mut n)| {
                let r = v[1];
                n[1] = (r >= 40 && r % 4 == 0 || r >= 56) as u8;
                (v, n)
            })
            .collect()
    };
    let mut expect: Vec<(Vec<Datum>, Vec<u8>)> = (0..7u64)
        .map(|k| {
            let rows: Vec<u64> = (0..56).filter(|r| r % 7 == k).collect();
            let sum: u64 = rows.iter().filter(|&&r| !(r >= 40 && r % 4 == 0)).sum();
            (vec![k, sum, rows.len() as u64], vec![0, 0, 0])
        })
        .collect();
    expect.push((vec![8, 0, 4], vec![0, 1, 0]));
    // 10_000: past the old fixed 4096-cell cap (the share decides the room); 1 << 40: past the front's u32 count (hashed)
    for (lo, hi) in [(0i64, 8i64), (0, 6), (-5, 20), (0, 10_000), (0, 1 << 40)] {
        let mut params = DEFAULT_PARAMS;
        params[PARAM_DENSE_LO] = lo as u64;
        params[PARAM_DENSE_HI] = hi as u64;
        let mut rig = Rig::new(programs::par_hashagg_with(Oids::T_KV, 64, 0), params.to_vec());
        rig.site(0).rows_for = gen;
        rig.site(0).granule = 8;
        let (rows, rets) = rig.run_pipelines();
        assert_eq!(rets, vec![Ret::Done, Ret::Done], "domain [{lo}, {hi}]");
        let mut got: Vec<(Vec<Datum>, Vec<u8>)> = rows.into_iter().zip(rig.host.pushed_nulls.clone()).collect();
        got.sort();
        assert_eq!(got, expect, "domain [{lo}, {hi}]");
    }
}

#[test]
fn par_hashagg_zone_topk_emits_the_n_smallest_keys() {
    use gravity_kernels::table::params::{DEFAULT_PARAMS, PARAM_TOPK_N};
    let mut params = DEFAULT_PARAMS;
    params[PARAM_TOPK_N] = 3;
    let mut rig = Rig::new(programs::par_hashagg_with(Oids::T_KV, 64, 0), params.to_vec());
    // unique keys descending (6000 - r over the rig's 60 rows), so every key is smaller than the ones before it
    rig.site(0).rows_for = |keys, _| par_rows(keys, |r| vec![6000 - r, r]);
    let (mut rows, rets) = rig.run_pipelines();
    assert_eq!(rets, vec![Ret::Done, Ret::Done]);
    rows.sort();
    // the width-1 arm cuts the sorted groups at n: the three smallest keys and nothing else
    let expect: Vec<Vec<Datum>> = (5941..=5943u64).map(|k| vec![k, 6000 - k, 1]).collect();
    assert_eq!(rows, expect);
}
