//! D3147: the admitted count (`grant::running`, the grant end line's) is the statements admitted to the big lane, not the
//! in-flight slots. An ungated pool hands a statement its slot BEFORE the bytes wait, so a statement queued for bytes holds a
//! slot and none of the lane. Nothing bounds a running statement by that count: past its grant it draws what the pool has
//! room for (`Reservation::grow_fits` reads the user's cap alone). One process-wide pool: one lock.

use std::sync::mpsc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use gravity_driver::mem::grant::running;
use gravity_driver::{Grant, POOL, REGIONS};

static LOCK: Mutex<()> = Mutex::new(());
const MIB: u64 = 1 << 20;

#[test]
fn a_statement_queued_for_bytes_is_not_counted_admitted() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    POOL.gate(0);
    REGIONS.trim();
    POOL.arm(1 << 30);
    // live 1, width 1: a grant maps twice its bytes (leader + one worker, `regions_of`): 512 of 1024 MiB
    let a = POOL.admit(Grant::of(256 * MIB), 1, 1, Duration::from_millis(10)).unwrap();
    assert_eq!(a.grant, Grant::of(256 * MIB));
    assert!(a.holds_slot());
    assert_eq!(running(), 1);
    // another session asks more than the rest of the pool (768 MiB beside 512): it takes an in-flight slot, then queues for
    // the bytes (alone it fits whole, so it holds the turn and waits; it is never halved inside its 10 s)
    let (got, go) = (mpsc::channel(), mpsc::channel::<()>());
    let t = std::thread::spawn(move || {
        let b = POOL.admit(Grant::of(384 * MIB), 1, 1, Duration::from_millis(10_000));
        got.0.send(b.as_ref().map(|b| b.grant == Grant::of(384 * MIB)).unwrap_or(false)).unwrap();
        go.1.recv().unwrap();
        drop(b);
    });
    let t0 = Instant::now();
    while POOL.inflight() < 2 && t0.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(POOL.inflight(), 2, "the waiter holds an in-flight slot");
    assert_eq!(running(), 1, "but is not admitted");
    drop(a);
    assert!(got.1.recv().unwrap(), "the waiter is admitted whole once the bytes come back");
    assert_eq!(running(), 1);
    go.0.send(()).unwrap();
    t.join().unwrap();
    assert_eq!((running(), POOL.inflight()), (0, 0), "nothing leaks");
    POOL.arm(0);
}
