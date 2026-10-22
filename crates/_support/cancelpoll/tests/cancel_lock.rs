//! D4210 / F8 (main 2026-10-12T19:09:08Z): a lock wait polls its statement's cancel (`cancelpoll::lock_or_cancel`). A
//! holder that keeps the lock 2 s (a worker's spill epoch write under an I/O stall) no longer holds a canceled waiter: the
//! cancel set 100 ms into the wait ends it within the poll bound, as the stop of the waiter's step (`cancelpoll::seen`). A
//! free lock is taken at once, and again once the holder is gone. One test: the abort byte is the process's.
//! D4079: named cancel_* so its binary runs in the crate tests' serial tail; the bound is wall-clock time.
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The waiter's statement's abort byte, lent to it (`cancelpoll::scoped`) as a kernel call lends its context's.
static ABORT: AtomicU8 = AtomicU8::new(0);

/// When the canceller sets the abort bit, after the waiter met the held lock.
const CANCEL_AT: Duration = Duration::from_millis(100);

/// The poll bound past the cancel: one park (`LOCK_PARK`, 1 ms) and a tick; the rest is a loaded box's slack, far under
/// the holder's 2 s.
const BOUND: Duration = Duration::from_millis(150);

#[test]
fn a_canceled_lock_wait_ends_within_the_poll_bound() {
    let m = Arc::new(Mutex::new(7u32));
    cancelpoll::scoped(&ABORT, || {
        let mut step = cancelpoll::step("cancel_lock: a free lock");
        assert_eq!(*cancelpoll::lock_or_cancel(&mut step, &m).expect("a free lock is taken"), 7);
    });

    // a holder keeps the lock 2 s
    let (held, theirs) = (Arc::new(AtomicBool::new(false)), m.clone());
    let h = held.clone();
    let holder = std::thread::spawn(move || {
        let _g = theirs.lock().unwrap();
        h.store(true, Relaxed);
        std::thread::sleep(Duration::from_secs(2));
    });
    let t0 = Instant::now();
    while !held.load(Relaxed) {
        assert!(t0.elapsed() < Duration::from_secs(5), "the holder did not take the lock");
        std::thread::sleep(Duration::from_millis(1));
    }
    let canceller = std::thread::spawn(|| {
        std::thread::sleep(CANCEL_AT);
        ABORT.store(cancelpoll::ABORT_BIT, Relaxed);
    });
    let t0 = Instant::now();
    let (r, seen) = cancelpoll::scoped(&ABORT, || {
        let mut step = cancelpoll::step("cancel_lock: a held lock");
        let r = cancelpoll::lock_or_cancel(&mut step, &m).map(|g| *g);
        (r, cancelpoll::seen())
    });
    let took = t0.elapsed();
    canceller.join().expect("the canceller");
    assert_eq!(r, Err(cancelpoll::Canceled), "the cancel ended the wait ({took:?})");
    assert!(seen, "the stop is the kernel call's (cancelpoll::seen)");
    assert!(took >= CANCEL_AT.saturating_sub(Duration::from_millis(10)), "it waited for the holder until the cancel: {took:?}");
    assert!(took < CANCEL_AT + BOUND, "the cancel ended the wait within the poll bound, not the holder's 2 s: {took:?}");

    holder.join().expect("the holder");
    ABORT.store(0, Relaxed);
    cancelpoll::scoped(&ABORT, || {
        let mut step = cancelpoll::step("cancel_lock: the lock freed");
        assert_eq!(*cancelpoll::lock_or_cancel(&mut step, &m).expect("the freed lock is taken"), 7);
    });
}
