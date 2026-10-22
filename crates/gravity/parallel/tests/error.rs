mod common;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use common::*;
use gravity_driver::{Entry, Error};
use gravity_program::hand as programs;
use gravity_parallel::{ErrorFunnel, FirstError, FixturePool, Job, MorselRange, MorselSource, Pool, Unit};

/// The fixture pool with every block claim handed out past block `past` held until a worker's error closed the
/// hand-out (D2588): the test waits on that event, not on the failing worker's speed. Unheld, "the cursor closed"
/// was a race: on a loaded box the failing worker could sleep between its error and the close while the others
/// claimed every block. The claims in flight at the close are the only ones that run past the failure.
struct HoldPastFailure<'p> {
    pool: &'p FixturePool,
    past: AtomicU64,
    /// A held claim never saw the close (the safety cap; no deadline is part of the assertion).
    unreleased: AtomicBool,
}

/// The hand-out the pool runs: the section's cursor, every call forwarded, a claim past `past` held until `close`.
struct Held<'a> {
    claims: &'a dyn MorselSource,
    past: u64,
    closed: Mutex<bool>,
    wake: Condvar,
    unreleased: &'a AtomicBool,
}

impl MorselSource for Held<'_> {
    fn total(&self) -> u64 {
        self.claims.total()
    }
    fn unit(&self) -> Unit {
        self.claims.unit()
    }
    fn claim(&self) -> Option<MorselRange> {
        let r = self.claims.claim()?;
        if self.claims.unit() == Unit::Block && r.lo > self.past {
            let mut closed = self.closed.lock().unwrap_or_else(|e| e.into_inner());
            while !*closed {
                let (g, t) = self.wake.wait_timeout(closed, Duration::from_secs(120)).unwrap_or_else(|e| e.into_inner());
                closed = g;
                if t.timed_out() && !*closed {
                    self.unreleased.store(true, Ordering::Relaxed);
                    break;
                }
            }
        }
        Some(r)
    }
    fn close(&self) {
        self.claims.close();
        *self.closed.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.wake.notify_all();
    }
    fn remaining(&self) -> u64 {
        self.claims.remaining()
    }
    fn reset(&self) {
        self.claims.reset()
    }
    fn startup_c0(&self) -> u64 {
        self.claims.startup_c0()
    }
    fn next_boundary_after(&self, unit: u64) -> u64 {
        self.claims.next_boundary_after(unit)
    }
    fn first_live_at(&self, unit: u64) -> u64 {
        self.claims.first_live_at(unit)
    }
    fn whole_boundary_claims(&self) -> bool {
        self.claims.whole_boundary_claims()
    }
    fn observe_bound(&self, bound: i64) {
        self.claims.observe_bound(bound)
    }
    fn beyond_bound(&self, unit: u64) -> bool {
        self.claims.beyond_bound(unit)
    }
}

impl Pool for HoldPastFailure<'_> {
    fn run(&self, width: u16, claims: &dyn MorselSource, job: &Job<'_>) -> Result<(), FirstError> {
        let past = self.past.load(Ordering::Relaxed);
        if past == u64::MAX {
            return self.pool.run(width, claims, job);
        }
        let held = Held { claims, past, closed: Mutex::new(false), wake: Condvar::new(), unreleased: &self.unreleased };
        self.pool.run(width, &held, job)
    }
    fn claims_run(&self) -> u64 {
        self.pool.claims_run()
    }
    fn threads(&self) -> u16 {
        self.pool.threads()
    }
}

#[test]
fn a_worker_error_surfaces_once_as_the_leaders_error_with_no_leaked_claims() {
    let table = FixtureTable::new(T_KV, 300, 30, gen_kv);
    let mut derive = FixtureDerive::new(&[&table]);
    let program = Arc::new(programs::par_hashagg_with(T_KV, 16, 9000));
    let fixture = FixturePool::new(8);
    let pool = HoldPastFailure { pool: &fixture, past: AtomicU64::new(u64::MAX), unreleased: AtomicBool::new(false) };
    let mut entry = Entry::new(2);
    for w in [2u16, 4, 8] {
        table.fresh();
        table.fail_block.store(137, Ordering::Relaxed);
        pool.past.store(137, Ordering::Relaxed);
        let mut sink = LegSink::new(true);
        let r = run_keyed(&mut entry, &program, &mut derive, &pool, w, 4, &gravity_kernels::table::params::DEFAULT_PARAMS, &NO_ABORT, &mut sink);
        pool.past.store(u64::MAX, Ordering::Relaxed);
        assert_eq!(r, Err(Error::host("range read failed")), "width {w}");
        assert!(!pool.unreleased.load(Ordering::Relaxed), "width {w}: a claim past the failing block waited 120 s and no error closed the hand-out");
        assert!(table.ledger.consumed() < table.blocks, "width {w}: the cursor closed");
        assert_eq!(table.ledger.twice(), 0, "width {w}");
        assert_eq!(table.pins.load(Ordering::Relaxed), 0, "width {w}");
        assert!(sink.sorted().is_empty(), "width {w}: no rows after a failed section");
        table.fail_block.store(u64::MAX, Ordering::Relaxed);
        table.fresh();
        let mut sink = LegSink::new(true);
        let rows = run_keyed(&mut entry, &program, &mut derive, &pool, w, 4, &gravity_kernels::table::params::DEFAULT_PARAMS, &NO_ABORT, &mut sink).unwrap();
        assert_eq!(rows, 97, "width {w}: the parked node runs again after the failure");
        assert!(table.ledger.consumed_once());
    }
    let st = entry.stats();
    assert_eq!((st.ended_failed, st.built), (3, 1), "one node, parked across every failure");
}

#[test]
fn the_funnel_keeps_the_error_of_the_lowest_claim() {
    let f = ErrorFunnel::new();
    assert!(f.take().is_none());
    f.offer(40, 1, Error::host("later"));
    f.offer(8, 3, Error::host("first"));
    f.offer(8, 4, Error::host("same claim, higher ordinal"));
    f.offer(9, 0, Error::Cancelled);
    let first = f.take().unwrap();
    assert_eq!((first.claim_lo, first.ordinal, first.error), (8, 3, Error::host("first")));
    assert!(f.take().is_none());
}
