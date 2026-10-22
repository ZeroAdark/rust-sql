//! D3242: a nested admission (a statement inside a function of a running statement, on its thread) whose need cannot fit whole
//! beside its caller's grant takes no turn: it waited `BYTES_WAIT` (2 s) for room only its own caller could give back, then
//! halved, and when even the floor did not fit polled for the whole `wait`. It now waits only while a grant holder besides its
//! own could give bytes back: any session's statement, counted until its bytes went back (holders, not sessions: a session's
//! second statement still holds after its first ended). The pool is process-wide: the tests run under one lock, and each
//! disarms it at its end.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gravity_driver::{Admitted, Grant, POOL, REGIONS};

static LOCK: Mutex<()> = Mutex::new(());
const MIB: u64 = 1 << 20;
const WAIT: Duration = Duration::from_secs(5);

/// The pool armed at 1 GiB, ungated, the governor unarmed.
fn armed() -> std::sync::MutexGuard<'static, ()> {
    let g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    POOL.gate(0);
    REGIONS.trim();
    assert_eq!(POOL.held(), 0, "a previous test left bytes held");
    POOL.arm(1 << 30);
    g
}

fn admit(bytes: u64) -> gravity_driver::Reservation {
    POOL.admit_need_until(Grant::of(bytes), &|g| g.bytes, WAIT, &|| false).expect("a statement")
}

#[test]
fn a_nested_ask_alone_beside_its_callers_grant_halves_at_once() {
    let _g = armed();
    let caller = admit(900 * MIB);
    // 256 MiB cannot fit whole beside the caller's 900 (no turn); 124 MiB are free: the ladder's 64 MiB fits
    let t0 = Instant::now();
    let nested = admit(256 * MIB);
    let took = t0.elapsed();
    assert_eq!((nested.grant.bytes, nested.how), (64 * MIB, Admitted::Short), "{took:?}");
    assert!(took < Duration::from_millis(100), "alone: no 2 s wait for room only its caller holds ({took:?})");
    drop(nested);
    drop(caller);
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}

#[test]
fn a_nested_ask_alone_whose_floor_does_not_fit_is_refused_at_once() {
    let _g = armed();
    let caller = admit(1000 * MIB);
    // 24 MiB free: not even the 64 MiB floor fits, and nothing but the caller holds the pool
    let t0 = Instant::now();
    let nested = POOL.admit_need_until(Grant::of(256 * MIB), &|g| g.bytes, WAIT, &|| false);
    let took = t0.elapsed();
    assert!(nested.is_err(), "the floor does not fit: refused");
    assert!(took < Duration::from_millis(100), "alone: refused at once, not after the {WAIT:?} wait ({took:?})");
    drop(caller);
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}

#[test]
fn a_nested_ask_waits_while_another_statement_could_give_bytes_back() {
    let _g = armed();
    let caller = admit(800 * MIB);
    // another session's statement holds 150 MiB and ends 300 ms on: its end frees what the 128 MiB rung needs (74 MiB free
    // before it, 224 after; 256 never fits whole beside the caller's 800, so no turn)
    let (admitted, done) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
    let (a, d) = (admitted.clone(), done.clone());
    let other = std::thread::spawn(move || {
        let r = admit(150 * MIB);
        a.store(true, Relaxed);
        std::thread::sleep(Duration::from_millis(300));
        d.store(true, Relaxed);
        drop(r);
    });
    let t0 = Instant::now();
    while !admitted.load(Relaxed) {
        assert!(t0.elapsed() < WAIT, "the other statement was not admitted");
        std::thread::sleep(Duration::from_millis(1));
    }
    let t0 = Instant::now();
    let nested = admit(256 * MIB);
    let took = t0.elapsed();
    assert!(done.load(Relaxed), "it waited for the other statement's end ({took:?})");
    assert_eq!((nested.grant.bytes, nested.how), (128 * MIB, Admitted::Short), "{took:?}");
    assert!(took >= Duration::from_millis(250) && took < Duration::from_millis(1900), "waited for that end, not BYTES_WAIT: {took:?}");
    other.join().expect("the other statement");
    drop(nested);
    drop(caller);
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}

#[test]
fn a_nested_ask_waits_for_a_sessions_second_statement_after_its_first_ended() {
    let _g = armed();
    let caller = admit(800 * MIB);
    // another session runs two statements (the first holds its slot, the second is nested beside it): 50 + 150 MiB, 24 MiB
    // free. The first ends at 100 ms (the session leaves RUNNING), the second at 400 ms: only the second's end makes room
    // for the 128 MiB rung, so the nested ask must still count it as a holder that could give bytes back
    let (admitted, done) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
    let (a, d) = (admitted.clone(), done.clone());
    let session = std::thread::spawn(move || {
        let first = admit(50 * MIB);
        let second = admit(150 * MIB);
        a.store(true, Relaxed);
        std::thread::sleep(Duration::from_millis(100));
        drop(first);
        std::thread::sleep(Duration::from_millis(300));
        d.store(true, Relaxed);
        drop(second);
    });
    let t0 = Instant::now();
    while !admitted.load(Relaxed) {
        assert!(t0.elapsed() < WAIT, "the other session's statements were not admitted");
        std::thread::sleep(Duration::from_millis(1));
    }
    let t0 = Instant::now();
    let nested = admit(256 * MIB);
    let took = t0.elapsed();
    assert!(done.load(Relaxed), "it waited for the session's second statement, not only its first ({took:?})");
    assert_eq!((nested.grant.bytes, nested.how), (128 * MIB, Admitted::Short), "{took:?}");
    assert!(took >= Duration::from_millis(350) && took < Duration::from_millis(1900), "waited for the second's end: {took:?}");
    session.join().expect("the other session");
    drop(nested);
    drop(caller);
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}

/// D3258's concern for D3242: a statement unwound mid-grant (a cancel's 53200 is an unwind through its frames) leaves the
/// holders it counted in: its root `StmtCtx` drops, its bytes go back, and a nested waiter after it sees no holder but its
/// own caller, so it halves at once instead of waiting `BYTES_WAIT` (or the floor's whole wait) for bytes no one holds.
#[test]
fn a_holder_unwound_mid_grant_leaves_the_holders_a_nested_ask_counts() {
    let _g = armed();
    let caller = admit(800 * MIB);
    let held = POOL.held();
    // another session's statement: admitted, a region drawn inside its grant, then an unwind with both live
    let reached = Arc::new(AtomicBool::new(false));
    let r = reached.clone();
    let session = std::thread::spawn(move || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let res = admit(150 * MIB);
            let region = gravity_driver::try_region_in(&res, (32 * MIB) as usize).expect("inside its grant");
            std::hint::black_box(&region);
            // the unwind starts here, the grant and its region live (an assert above that failed would not set this)
            r.store(true, Relaxed);
            std::panic::panic_any("a cancel's unwind mid-grant (53200)");
        }))
        .is_err()
    });
    assert!(session.join().expect("the session thread"), "the statement unwound");
    assert!(reached.load(Relaxed), "it unwound with its grant and region live, not at an earlier failure");
    // a region the unwind freed may sit parked (the pool's, no statement's): trimmed, the pool holds the caller's alone
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    assert_eq!(POOL.held(), held, "the unwound statement's grant and region are back");
    // 224 MiB free: 256 cannot fit whole beside the caller's 800 (no turn); with the unwound holder gone it halves at once
    let t0 = Instant::now();
    let nested = admit(256 * MIB);
    let took = t0.elapsed();
    assert_eq!((nested.grant.bytes, nested.how), (128 * MIB, Admitted::Short), "{took:?}");
    assert!(took < Duration::from_millis(100), "no holder left but its caller: no wait ({took:?})");
    drop(nested);
    drop(caller);
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}
