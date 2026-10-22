//! D4235: a statement's cancel lands inside the grant pool's wait (`GrantPool::take_wait`) within its poll bound. A leader
//! arena chunk past G (the hosted aggregate's road, D3239's) beside a second admitted statement that keeps the pool full
//! waits for that statement's bytes (`BYTES_WAIT`, 2 s); the statement's abort byte set 100 ms into that wait ends it
//! (the step's tick, D3887 / D3902 / D4098), and the draw is refused as the cancel (`cancelpoll::seen`).
//!
//! The e2e case that witnessed this end to end (e2e/gravity/v5/memcancel_d3902_owner_wait) no longer reaches the wait: C10
//! 2b's one-batch refill (D3582) keeps its owner phase inside its grant, and no shape found on trunk reaches the pool wait
//! from SQL (D4235). This test reaches it directly: the pool's wait count moves, and the draw takes about the cancel's
//! 100 ms, not the 2 s wait. One test per process (the pool is process-wide).

use gravity_driver_rig as common;

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use gravity_driver::{footprint, Entry, Grant, POOL, REGIONS};

const MIB: u64 = 1 << 20;
const NULLS: [u8; 1] = [0];

/// The statement's abort byte, lent to the draw (`cancelpoll::scoped`) as a kernel call lends its context's.
static ABORT: AtomicU8 = AtomicU8::new(0);

/// When the canceller sets the abort bit, after the draw entered the pool's wait.
const CANCEL_AT: Duration = Duration::from_millis(100);

/// The poll bound past the cancel: the wait ticks its step every pause (2 ms); the rest is a loaded box's slack, far under
/// the 2 s the wait would last without the tick.
const BOUND: Duration = Duration::from_millis(150);

#[test]
fn a_cancel_lands_inside_the_pools_wait_for_another_statements_bytes() {
    let program = Arc::new(p_group_share(4));
    let want = Grant::of(64 * MIB);
    let need = footprint(want, &program, 1);
    let spare = 16 * MIB;
    POOL.gate(0);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    POOL.arm(2 * need + spare);
    let res = std::rc::Rc::new(POOL.admit_need(want, &|g| footprint(g, &program, 1), Duration::from_millis(10)).unwrap());
    res.set_pool_fraction(gravity_driver::POOL_FRACTION_WHOLE);
    let mut derive = FakeDerive::ranges(RangeTable::new(24, 10));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[8], &NULLS, &mut sink, &flag);
    let exec = entry.start(None, &program, &mut derive, &mut b, res.clone()).unwrap();
    let ctx = exec.node().ctx();

    // G drawn down to a MiB or less (a chunk inside G never meets the pool's wait), as D3239's test does
    let fill = res.left().saturating_sub(MIB);
    if fill > 0 {
        assert_eq!(unsafe { gravity_abi::program::arena_grow(ctx, fill) }, fill as i64, "a chunk inside G");
    }
    // another session's statement holds the rest of the pool but `spare` until told to end (past BYTES_WAIT, so only the
    // cancel can end the draw's wait early)
    let (admitted, end) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
    let (a, e) = (admitted.clone(), end.clone());
    let other = std::thread::spawn(move || {
        let r = POOL.admit_need(Grant::of(need), &|g| g.bytes, Duration::from_millis(10)).expect("the other statement");
        a.store(true, Relaxed);
        let t0 = Instant::now();
        while !e.load(Relaxed) && t0.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(r);
    });
    let t0 = Instant::now();
    while !admitted.load(Relaxed) {
        assert!(t0.elapsed() < Duration::from_secs(5), "the other statement was not admitted");
        std::thread::sleep(Duration::from_millis(1));
    }
    let past = res.left() + spare + MIB;
    let (waits, held) = (POOL.stats().waits, POOL.held());
    let canceller = std::thread::spawn(|| {
        std::thread::sleep(CANCEL_AT);
        ABORT.store(cancelpoll::ABORT_BIT, Relaxed);
    });
    let t0 = Instant::now();
    let (got, seen) = cancelpoll::scoped(&ABORT, || (unsafe { gravity_abi::program::arena_grow(ctx, past) }, cancelpoll::seen()));
    let took = t0.elapsed();
    canceller.join().expect("the canceller");
    let still = !end.load(Relaxed);
    end.store(true, Relaxed);
    other.join().expect("the other statement");
    assert!(still, "the other statement held the pool through the draw");
    assert_eq!(POOL.stats().waits, waits + 1, "the draw reached the pool's wait (take_wait counted it)");
    assert_eq!(got, 0, "the canceled draw is refused ({took:?})");
    assert!(seen, "refused as the statement's cancel (cancelpoll::seen)");
    assert!(took >= CANCEL_AT.saturating_sub(Duration::from_millis(10)), "it waited in the pool until the cancel: {took:?}");
    assert!(took < CANCEL_AT + BOUND, "the cancel ended the pool's wait within the poll bound: {took:?}");
    assert!(POOL.held() <= held, "the refused draw took nothing: held {} of {held} before", POOL.held());

    ABORT.store(0, Relaxed);
    entry.end(exec, false);
    drop(res);
    drop(entry);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    assert_eq!(POOL.held(), 0, "every chunk and reservation back at the statement's end");
    POOL.arm(0);
}
