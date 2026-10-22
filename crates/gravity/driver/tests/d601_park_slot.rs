//! D601: an idle suspended portal gives its statement's in-flight slot back (`Reservation::park_slot`) and re-takes it,
//! forced, at its next fetch (`unpark_slot`); a portal closed while parked leaks no slot, neither the pool's count nor the
//! thread's own (`SLOTS_HERE`: the thread's next statement takes a real slot). One process-wide pool: one lock.

use std::sync::mpsc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use gravity_driver::{Grant, POOL, REGIONS};

static LOCK: Mutex<()> = Mutex::new(());
const MIB: u64 = 1 << 20;

fn armed() -> std::sync::MutexGuard<'static, ()> {
    let g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    POOL.gate(0);
    REGIONS.trim();
    POOL.arm(1 << 30);
    POOL.gate(1);
    g
}

fn done() {
    POOL.gate(0);
    POOL.arm(0);
}

#[test]
fn a_parked_slot_lets_another_session_in_and_comes_back_forced() {
    let _g = armed();
    let want = Grant::of(8 * MIB);
    let a = POOL.admit(want, 1, 1, Duration::from_millis(10)).unwrap();
    assert!(a.holds_slot());
    assert_eq!(POOL.inflight(), 1);
    a.park_slot();
    assert!(!a.holds_slot());
    assert_eq!(POOL.inflight(), 0, "the idle portal gave its slot back");
    // another session's statement takes the one slot at once, and holds it until told
    let (took, go) = (mpsc::channel(), mpsc::channel::<()>());
    let t = std::thread::spawn(move || {
        let t0 = Instant::now();
        let b = POOL.admit(want, 1, 1, Duration::from_millis(2000)).unwrap();
        took.0.send((b.holds_slot(), t0.elapsed())).unwrap();
        go.1.recv().unwrap();
        drop(b);
    });
    let (slot, waited) = took.1.recv().unwrap();
    assert!(slot && waited < Duration::from_millis(500), "slot {slot}, waited {waited:?}");
    assert_eq!(POOL.inflight(), 1);
    // the next fetch re-takes the slot past the full gate, without waiting
    a.unpark_slot();
    assert!(a.holds_slot());
    assert_eq!(POOL.inflight(), 2);
    go.0.send(()).unwrap();
    t.join().unwrap();
    assert_eq!(POOL.inflight(), 1);
    drop(a);
    assert_eq!(POOL.inflight(), 0);
    done();
}

#[test]
fn a_portal_closed_while_parked_leaks_no_slot() {
    let _g = armed();
    let want = Grant::of(8 * MIB);
    let a = POOL.admit(want, 1, 1, Duration::from_millis(10)).unwrap();
    a.park_slot();
    a.park_slot();
    drop(a);
    assert_eq!(POOL.inflight(), 0, "the close gave back nothing twice");
    // this thread holds no slot any more: its next statement takes a real one (not the thread's own, `Some(false)`)
    let c = POOL.admit(want, 1, 1, Duration::from_millis(10)).unwrap();
    assert!(c.holds_slot());
    assert_eq!(POOL.inflight(), 1);
    // parked, re-taken, parked again and closed: back to zero
    c.park_slot();
    c.unpark_slot();
    c.unpark_slot();
    assert_eq!(POOL.inflight(), 1);
    c.park_slot();
    drop(c);
    assert_eq!(POOL.inflight(), 0);
    let d = POOL.admit(want, 1, 1, Duration::from_millis(10)).unwrap();
    assert!(d.holds_slot());
    drop(d);
    assert_eq!(POOL.inflight(), 0);
    done();
}
