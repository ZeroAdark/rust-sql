//! D3910 (b): a suspended portal lends its grant's room back to the pool with its slot (`Reservation::park_slot`), so
//! another session's statement that fits only in that room admits at once, whole; the next fetch re-takes it forced (`unpark_slot`), and a
//! portal closed while suspended gives back exactly what it still held. One process-wide pool: one lock.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use gravity_driver::{Grant, POOL, REGIONS};

static LOCK: Mutex<()> = Mutex::new(());
const MIB: u64 = 1 << 20;

fn armed() -> std::sync::MutexGuard<'static, ()> {
    let g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    POOL.gate(0);
    REGIONS.trim();
    POOL.arm(64 * MIB);
    POOL.gate(1);
    g
}

fn done() {
    POOL.gate(0);
    POOL.arm(0);
}

#[test]
fn a_suspended_portal_lends_its_room_and_the_next_fetch_takes_it_back() {
    let _g = armed();
    let base = POOL.held();
    let a = POOL.admit(Grant::of(16 * MIB), 1, 1, Duration::from_millis(10)).unwrap();
    let mine = a.reserved;
    assert!(mine >= 16 * MIB && POOL.held() == base + mine, "reserved {mine}, held {}", POOL.held());
    a.park_slot();
    assert_eq!(POOL.held(), base, "the idle portal drew nothing: its whole room is lent");
    // another session's statement that fits only in the lent bytes admits at once and whole
    let t = std::thread::spawn(move || {
        let t0 = Instant::now();
        let b = POOL.admit(Grant::of(20 * MIB), 1, 1, Duration::from_millis(1500)).unwrap();
        (b.reserved, b.grant.bytes == b.asked.bytes, t0.elapsed())
    });
    let (theirs, whole, waited) = t.join().unwrap();
    assert!(mine + theirs > POOL.capacity(), "{mine} + {theirs} fit the pool without the lent room");
    assert!(whole && waited < Duration::from_millis(300), "whole {whole}, reserved {theirs}, waited {waited:?}");
    assert_eq!(POOL.held(), base);
    // the next fetch re-takes the room without waiting
    a.unpark_slot();
    assert_eq!(POOL.held(), base + mine);
    // suspended again and closed: the pool is back where it started
    a.park_slot();
    drop(a);
    assert_eq!(POOL.held(), base);
    done();
}

/// The forced re-take may land the pool above its capacity; it is charged on the statement's ledger account at once, so
/// the watchdog and the reclaim ladder see the overshoot.
#[test]
fn the_forced_retake_lands_above_the_pool_and_shows_on_its_account() {
    let _g = armed();
    memledger::bind_backend(7, 7007);
    let acct = memledger::current();
    let reserved = || memledger::account_row(acct).pool_reserved_bytes;
    let a = POOL.admit(Grant::of(16 * MIB), 1, 1, Duration::from_millis(10)).unwrap();
    let mine = a.reserved;
    assert_eq!(reserved(), mine as i64);
    a.park_slot();
    assert_eq!(reserved(), 0, "the lent room leaves the account");
    // another session's statement admits into the lent room and holds it
    let (took, go) = (std::sync::mpsc::channel(), std::sync::mpsc::channel::<()>());
    let t = std::thread::spawn(move || {
        let b = POOL.admit(Grant::of(20 * MIB), 1, 1, Duration::from_millis(1500)).unwrap();
        took.0.send(b.reserved).unwrap();
        go.1.recv().unwrap();
        drop(b);
    });
    let theirs = took.1.recv().unwrap();
    assert!(mine + theirs > POOL.capacity(), "{mine} + {theirs} fit the pool without the lent room");
    a.unpark_slot();
    assert!(POOL.held() > POOL.capacity(), "held {} of {}", POOL.held(), POOL.capacity());
    assert_eq!(reserved(), mine as i64, "the re-take is on the account at once");
    go.0.send(()).unwrap();
    t.join().unwrap();
    drop(a);
    assert_eq!(reserved(), 0);
    memledger::unbind_backend();
    done();
}
