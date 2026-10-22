mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use common::*;
use gravity_driver::Entry;
use gravity_program::hand as programs;
use gravity_parallel::FixturePool;

struct Counting;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        System.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l)
    }
}

#[global_allocator]
static A: Counting = Counting;

#[test]
fn ten_thousand_claims_across_four_workers_allocate_nothing_after_warm_up() {
    let table = FixtureTable::new(T_A, 10_000, 2, gen_a);
    let mut derive = FixtureDerive::new(&[&table]);
    let program = Arc::new(programs::par_scan_count_with(T_A, 20_000));
    let pool = FixturePool::new(4);
    let mut entry = Entry::new(2);
    let expect = table.rows().filter(|r| r[0] < 300).count() as u64;
    let mut sink = LegSink::new(false);
    run_keyed(&mut entry, &program, &mut derive, &pool, 4, 1, &[300], &NO_ABORT, &mut sink).unwrap();
    run_keyed(&mut entry, &program, &mut derive, &pool, 4, 1, &[300], &NO_ABORT, &mut sink).unwrap();
    let claims = pool.claims_run();
    for round in 0..3 {
        let before = ALLOCS.load(Ordering::Relaxed);
        sink.count.store(0, Ordering::Relaxed);
        run_keyed(&mut entry, &program, &mut derive, &pool, 4, 1, &[300], &NO_ABORT, &mut sink).unwrap();
        assert_eq!(ALLOCS.load(Ordering::Relaxed) - before, 0, "round {round}: a section allocated");
        assert_eq!(sink.count.load(Ordering::Relaxed), 1);
    }
    assert_eq!(pool.claims_run() - claims, 30_000, "10 000 claims per section");
    let _ = expect;
}
