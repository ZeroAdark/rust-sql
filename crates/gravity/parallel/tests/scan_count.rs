mod common;

use std::sync::atomic::Ordering;
use std::sync::Arc;

use common::*;
use gravity_driver::Entry;
use gravity_program::hand as programs;
use gravity_parallel::{startup_c0, ClaimLedger, ClaimSize, FixturePool, GranuleRange, MorselSource, Pool};

#[test]
fn scan_count_is_identical_at_every_width_and_every_claim_is_consumed_once() {
    let table = FixtureTable::new(T_A, 200, 50, gen_a);
    let mut derive = FixtureDerive::new(&[&table]);
    let program = Arc::new(programs::par_scan_count_with(T_A, 10_000));
    let pool = FixturePool::new(8);
    let mut entry = Entry::new(2);
    let thr = 400u64;
    let expect = table.rows().filter(|r| r[0] < thr as i64).count() as u64;
    for w in WIDTHS {
        for claim in [1u64, 7, 64] {
            table.fresh();
            let mut sink = LegSink::new(true);
            let rows = run_once(&mut entry, &program, &mut derive, &pool, w, claim, &[thr], &NO_ABORT, &mut sink).unwrap();
            assert_eq!(rows, 1, "width {w}");
            assert_eq!(sink.sorted(), vec![vec![expect]], "width {w} claim {claim}");
            assert!(table.ledger.consumed_once(), "width {w} claim {claim}: consumed {} of {}, twice {}", table.ledger.consumed(), table.blocks, table.ledger.twice());
            assert_eq!(table.pins.load(Ordering::Relaxed), 0);
        }
    }
    assert!(pool.claims_run() > 0);
}

/// §1.10 increment 3 item 1: an adaptive cursor reports its C0 to the pool
/// and, self-served, hands out C0-unit claims that stop at a part edge; a
/// multi-block claim is consumed window by window with every block seen once.
#[test]
fn adaptive_claims_report_c0_stop_at_part_edges_and_are_consumed_window_by_window() {
    let g = GranuleRange::over_parts(40, ClaimSize::Adaptive { startup_c0: 16 }, &[0, 20, 25]);
    assert_eq!(g.startup_c0(), 16);
    assert!(!g.whole_boundary_claims());
    let mut claims = Vec::new();
    while let Some(c) = g.claim() {
        claims.push((c.lo, c.hi));
    }
    assert_eq!(claims, vec![(0, 16), (16, 20), (20, 25), (25, 40)], "16 units, cut at the part edges 20 and 25, the tail short");
    assert_eq!(GranuleRange::new(10, ClaimSize::Fixed(3)).startup_c0(), 1, "a fixed size is not the sizer's");

    let table = FixtureTable::new(T_A, 200, 50, gen_a);
    let mut derive = FixtureDerive::new(&[&table]);
    let program = Arc::new(programs::par_scan_count_with(T_A, 10_000));
    let pool = FixturePool::new(8);
    let mut entry = Entry::new(2);
    let thr = 400u64;
    let expect = table.rows().filter(|r| r[0] < thr as i64).count() as u64;
    for w in WIDTHS {
        table.fresh();
        let mut sink = LegSink::new(true);
        let before = Pool::claims_run(&pool);
        let rows = run_sized(&mut entry, &program, &mut derive, &pool, w, ClaimSize::Adaptive { startup_c0: 16 }, &[thr], &NO_ABORT, &mut sink, None).unwrap();
        assert_eq!(rows, 1, "width {w}");
        assert_eq!(sink.sorted(), vec![vec![expect]], "width {w}");
        assert!(table.ledger.consumed_once(), "width {w}: consumed {} of {}, twice {}", table.ledger.consumed(), table.blocks, table.ledger.twice());
        if w >= 2 {
            assert_eq!(Pool::claims_run(&pool) - before, table.blocks.div_ceil(16), "width {w}: {} blocks in claims of 16", table.blocks);
        }
        assert_eq!(table.pins.load(Ordering::Relaxed), 0);
    }
}

/// §1.10 increment 3 item 2: the startup claim comes from the geometry — about
/// four claims per worker — so a short statement is right-sized at once.
#[test]
fn a_geometry_derived_startup_claim_gives_four_claims_per_worker_and_every_granule_once() {
    let total = 12_207u64;
    let c0 = startup_c0(total, 16, 16, 1 << 20);
    assert_eq!(c0, total / 64, "12,207 granules at W = 16: {c0} granules per startup claim");
    let g = GranuleRange::new(total, ClaimSize::Adaptive { startup_c0: c0 });
    assert_eq!(g.startup_c0(), c0);
    let ledger = ClaimLedger::new(total);
    let mut claims = 0u64;
    while let Some(c) = g.claim() {
        ledger.record(c);
        claims += 1;
    }
    assert!((64..=65).contains(&claims), "{claims} claims for 12,207 granules at W = 16 (4 per worker)");
    assert!(ledger.consumed_once(), "every granule claimed exactly once");
    assert_eq!(startup_c0(12_207, 16, 16, 127), 127, "a claim never starts wider than a part");
    assert_eq!(startup_c0(40, 16, 16, 1 << 20), 16, "a small scan keeps the 16-granule floor");
    assert_eq!(startup_c0(1_220, 4, 64, 1_220), 76, "the heap analogue: 1,220 blocks at W = 4");
    assert_eq!(startup_c0(100, 4, 64, 100), 64);
    assert_eq!(startup_c0(1 << 30, 0, 16, 1 << 20), 1 << 20, "a width of 0 counts as 1; the bound holds");
}

#[test]
fn a_sink_without_legs_still_sections_a_breaker_pipeline() {
    let table = FixtureTable::new(T_A, 40, 25, gen_a);
    let mut derive = FixtureDerive::new(&[&table]);
    let program = Arc::new(programs::par_scan_count_with(T_A, 1000));
    let pool = FixturePool::new(4);
    let mut entry = Entry::new(2);
    let mut sink = LegSink::new(true);
    let before = pool.claims_run();
    run_once(&mut entry, &program, &mut derive, &pool, 4, 3, &[2000], &NO_ABORT, &mut sink).unwrap();
    assert_eq!(sink.sorted(), vec![vec![1000]]);
    assert_eq!(pool.claims_run() - before, 14, "40 blocks in claims of 3");
}

/// perfdiag3 fix 1: `gc`'s geometry (245 granules in 2 parts) at W8 — whole-part claims engage 2 workers; the
/// sizer-cut cursor the runner builds below the width hands out ≥ 8 claims inside the parts, every granule once.
#[test]
fn claims_split_inside_a_part_when_parts_are_fewer_than_the_width() {
    let (granules, starts, width) = (245u64, [0u64, 122], 8u16);
    let c0 = startup_c0(granules, width, 16, 123);
    assert_eq!(c0, 16, "clamp(245 / 32, 16, 123)");
    let mut whole = GranuleRange::over_parts(granules, ClaimSize::Adaptive { startup_c0: c0 }, &starts);
    whole.set_whole_boundary_claims(true);
    let mut n = 0;
    while whole.claim().is_some() {
        n += 1;
    }
    assert_eq!(n, 2, "whole-part claims: one per part");
    let cut = GranuleRange::over_parts(granules, ClaimSize::Adaptive { startup_c0: c0 }, &starts);
    let mut seen = vec![0u8; granules as usize];
    let mut claims = Vec::new();
    while let Some(c) = cut.claim() {
        assert!(c.hi <= cut.next_boundary_after(c.lo), "claim {c:?} crosses the part edge");
        assert!(c.hi - c.lo <= c0);
        (c.lo..c.hi).for_each(|g| seen[g as usize] += 1);
        claims.push((c.lo, c.hi));
    }
    assert!(claims.len() >= width as usize, "{} claims for {width} workers: {claims:?}", claims.len());
    assert!(seen.iter().all(|&s| s == 1), "every granule claimed once");
}

/// The tail rule's cursor (`set_tail_from`): 10 parts of 13 granules at W8, the tail at part 8. Self-served, the claims
/// before the tail are one whole part each, the tail's are `startup_c0`-unit claims cut inside each part (none crossing a
/// part edge), and every granule is claimed once; a dead part in the tail is skipped.
#[test]
fn a_tail_cursor_claims_whole_parts_then_cuts_the_tail() {
    let starts: Vec<u64> = (0..10).map(|p| p * 13).collect();
    let mut c = GranuleRange::over_parts(130, ClaimSize::Adaptive { startup_c0: 4 }, &starts);
    c.set_whole_boundary_claims(true);
    c.set_tail_from(8 * 13);
    assert_eq!((c.whole_until(), c.endgame_split()), (104, true));
    let mut seen = vec![0u8; 130];
    let mut claims = Vec::new();
    while let Some(r) = c.claim() {
        assert!(r.hi <= c.next_boundary_after(r.lo), "claim {r:?} crosses a part edge");
        (r.lo..r.hi).for_each(|g| seen[g as usize] += 1);
        claims.push((r.lo, r.hi));
    }
    assert!(seen.iter().all(|&s| s == 1), "every granule once: {claims:?}");
    let (whole, tail): (Vec<(u64, u64)>, Vec<(u64, u64)>) = claims.iter().partition(|&&(lo, _)| lo < 104);
    assert_eq!(whole, (0..8).map(|p| (p * 13, p * 13 + 13)).collect::<Vec<_>>(), "one claim per part before the tail");
    assert_eq!(tail, vec![(104, 108), (108, 112), (112, 116), (116, 117), (117, 121), (121, 125), (125, 129), (129, 130)], "c0-unit claims cut at the part edge");
    let mut d = GranuleRange::over_parts(130, ClaimSize::Adaptive { startup_c0: 4 }, &starts);
    d.set_whole_boundary_claims(true);
    d.set_tail_from(8 * 13);
    d.set_dead(&[(104, 117)]);
    d.set_endgame_split(false);
    let mut n = 0;
    while let Some(r) = d.claim() {
        assert!(r.lo >= 117 || r.lo < 104, "a dead part was claimed: {r:?}");
        n += 1;
    }
    assert_eq!((n, MorselSource::endgame_split(&d)), (8 + 4, false), "8 whole parts and the live tail part in 4 claims");
}

/// D3183: a cursor sized before its units were known (a bitmap's) re-aims its startup claim with the total; a fixed size
/// keeps its own.
#[test]
fn a_reaimed_adaptive_cursor_reports_and_hands_out_the_new_c0() {
    let b = gravity_parallel::BlockRange::new(0, ClaimSize::Adaptive { startup_c0: 6443 });
    b.0.set_total_aimed(10_310, startup_c0(10_310, 8, 64, 10_310));
    assert_eq!(MorselSource::startup_c0(&b), 322, "10,310 entries over 4 claims per worker at width 8");
    let mut claims = Vec::new();
    while let Some(c) = b.claim() {
        claims.push(c.hi - c.lo);
    }
    assert_eq!(claims.iter().sum::<u64>(), 10_310);
    assert!(claims.iter().all(|&n| n <= 322), "{claims:?}");
    let f = gravity_parallel::BlockRange::new(0, ClaimSize::Fixed(3));
    f.0.set_total_aimed(10, 5);
    assert_eq!(MorselSource::startup_c0(&f), 1, "a fixed size is not the sizer's");
    assert_eq!(f.claim().map(|c| c.hi - c.lo), Some(3));
}
