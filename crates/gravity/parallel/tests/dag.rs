mod common;

use std::sync::Arc;

use common::*;
use gravity_driver::Entry;
use gravity_parallel::{Barrier, FixturePool};

#[test]
fn two_producers_into_one_consumer_run_as_two_sections_and_one_emit() {
    let table = FixtureTable::new(T_A, 120, 50, gen_a);
    let mut derive = FixtureDerive::new(&[&table]);
    let rows = 120 * 50;
    let program = Arc::new(p_two_slots(T_A, rows));
    let mut barrier = Barrier::new(&program);
    assert_eq!((barrier.producers(0).count(), barrier.producers(1).count(), barrier.producers(2).collect::<Vec<_>>()), (0, 0, vec![0, 1]), "the emit waits for both producers");
    assert_eq!(barrier.next(), Some(0));
    barrier.done(0);
    assert_eq!(barrier.next(), Some(1));
    barrier.done(1);
    assert_eq!(barrier.next(), Some(2));
    barrier.done(2);
    assert!(barrier.finished());
    assert_eq!((program.slots.live_mask(0), program.slots.live_mask(1), program.slots.live_mask(2), program.slots.max_live), (0b01, 0b11, 0b11, 2));
    let pool = FixturePool::new(8);
    let mut entry = Entry::new(2);
    let thr = 700u64;
    let want = table.rows().filter(|[a, _]| (*a as u64) < thr).count() as u64;
    for w in WIDTHS {
        for claim in [3u64, 40] {
            table.fresh();
            let mut sink = LegSink::new(true);
            let n = run_once(&mut entry, &program, &mut derive, &pool, w, claim, &[thr], &NO_ABORT, &mut sink).unwrap();
            assert_eq!(n, 1, "width {w}");
            assert_eq!(sink.sorted(), vec![vec![2 * want, want, want]], "width {w} claim {claim}: both slots' counts reach the emit");
            assert!(table.ledger.consumed() == 120, "width {w}: every block claimed (twice: once per producer)");
            assert_eq!(table.ledger.twice(), 120, "width {w}: each producer claimed every block once");
        }
    }
}

#[test]
#[should_panic(expected = "started before its producers")]
fn the_barrier_refuses_a_consumer_before_its_producers() {
    let table = FixtureTable::new(T_A, 4, 5, gen_a);
    let _derive = FixtureDerive::new(&[&table]);
    let program = Arc::new(p_two_slots(T_A, 20));
    let mut barrier = Barrier::new(&program);
    assert_eq!(barrier.next(), Some(0));
    barrier.done(0);
    assert_eq!(barrier.next(), Some(1));
    barrier.skip_for_test(1);
    barrier.next();
}
