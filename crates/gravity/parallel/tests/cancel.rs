mod common;

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Instant;

use common::*;
use gravity_driver::{Entry, Error};
use gravity_program::hand as programs;
use gravity_parallel::FixturePool;

static ABORT: AtomicU8 = AtomicU8::new(0);

#[test]
fn cancel_mid_section_stops_every_worker_within_one_claim() {
    let table = FixtureTable::new(T_A, 4000, 20, gen_a).with_abort(&ABORT, 30_000);
    let mut derive = FixtureDerive::new(&[&table]);
    let program = Arc::new(programs::par_scan_count_with(T_A, 80_000));
    let pool = FixturePool::new(8);
    let mut entry = Entry::new(2);
    for w in [2u16, 4, 8] {
        table.fresh();
        ABORT.store(0, Ordering::Release);
        let mut sink = LegSink::new(true);
        let t = Instant::now();
        let r = run_once(&mut entry, &program, &mut derive, &pool, w, 2, &[500], &ABORT, &mut sink);
        assert_eq!(r, Err(Error::Cancelled), "width {w}");
        assert!(t.elapsed().as_millis() < 100, "width {w}: cancel took {:?}", t.elapsed());
        assert!(ABORT.load(Ordering::Acquire) == 1, "the fixture fired the byte");
        let started_after = table.started_after_abort.load(Ordering::Relaxed);
        assert!(started_after <= w as u64, "width {w}: {started_after} claims started after the byte was set");
        assert!(table.ledger.consumed() < table.blocks, "width {w}: remaining claims were dropped");
        assert_eq!(table.ledger.twice(), 0);
        assert_eq!(table.pins.load(Ordering::Relaxed), 0, "width {w}: pins at settle after cancel");
        assert!(sink.sorted().is_empty(), "nothing is emitted after a cancelled section");
    }
    ABORT.store(0, Ordering::Release);
    table.abort_after.store(u64::MAX, Ordering::Relaxed);
    table.fresh();
    let mut sink = LegSink::new(true);
    run_once(&mut entry, &program, &mut derive, &pool, 4, 2, &[500], &ABORT, &mut sink).unwrap();
    assert_eq!(sink.sorted(), vec![vec![table.rows().filter(|r| r[0] < 500).count() as u64]]);
}

#[test]
fn a_byte_set_before_the_section_cancels_before_any_claim() {
    let table = FixtureTable::new(T_A, 100, 20, gen_a);
    let mut derive = FixtureDerive::new(&[&table]);
    let program = Arc::new(programs::par_scan_count_with(T_A, 2000));
    let pool = FixturePool::new(4);
    let mut entry = Entry::new(2);
    let abort = AtomicU8::new(1);
    let mut sink = LegSink::new(true);
    let before = pool.claims_run();
    assert_eq!(run_once(&mut entry, &program, &mut derive, &pool, 4, 5, &[500], &abort, &mut sink), Err(Error::Cancelled));
    assert!(table.ledger.consumed() == 0, "no claim ran a kernel");
    assert!(pool.claims_run() - before <= 4, "each worker observed the byte on its first claim");
}
