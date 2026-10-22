//! D622 (A): a nested statement's site whose grant does not fit beside its caller's takes its floor
//! past the pool's capacity (`GrantPool::admit_overdrawn`, `Overdraw::Nested`), never refused and never waiting, as PostgreSQL
//! runs the nested statement at work_mem; without the row executor the refusal was a load-dependent 0A000. The bytes count in `held` while the
//! admission lives (admission beside it sees them) and go back with its G; each site counts its overdraws and their bytes
//! (`GrantPool::overdraws`, the grant report's `nested_past` / `seen_past`). The pool is process-wide: the tests run under
//! one lock, and each disarms it at its end.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use gravity_driver::{Admitted, Grant, Overdraw, GRANT_FLOOR, POOL, REGIONS};

static LOCK: Mutex<()> = Mutex::new(());
const MIB: u64 = 1 << 20;

/// The pool armed at 128 MiB, ungated, the governor unarmed.
fn armed() -> std::sync::MutexGuard<'static, ()> {
    let g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    POOL.gate(0);
    REGIONS.trim();
    assert_eq!(POOL.held(), 0, "a previous test left bytes held");
    POOL.arm(128 * MIB);
    g
}

fn admit(bytes: u64) -> gravity_driver::Reservation {
    POOL.admit_need_until(Grant::of(bytes), &|g| g.bytes, Duration::from_secs(5), &|| false).expect("a statement")
}

#[test]
fn a_nested_floor_past_a_full_pool_is_admitted_at_once_and_counted() {
    let _g = armed();
    let before = POOL.overdraws();
    // another statement holds the whole pool: a nested ask at wait 0 is refused (the D622 refusal's road before (A))
    let other = admit(128 * MIB);
    assert!(POOL.admit_need_until(Grant::of(GRANT_FLOOR), &|g| g.bytes, Duration::ZERO, &|| false).is_err(), "nothing fits");
    let t0 = Instant::now();
    let nested = POOL.admit_overdrawn(Grant::of(GRANT_FLOOR), GRANT_FLOOR, Overdraw::Nested);
    assert!(t0.elapsed() < Duration::from_millis(50), "never waits ({:?})", t0.elapsed());
    assert_eq!((nested.grant.bytes, nested.how, nested.reserved), (GRANT_FLOOR, Admitted::Overdrawn, GRANT_FLOOR));
    assert!(!nested.holds_slot(), "no in-flight slot");
    assert_eq!(POOL.held(), 128 * MIB + GRANT_FLOOR, "held past the capacity while it lives");
    let s = POOL.overdraws();
    assert_eq!(s[0], (before[0].0 + 1, before[0].1 + GRANT_FLOOR), "nested_past counts the overdraw and its bytes");
    assert_eq!(s[1], before[1], "seen_past untouched");
    drop(nested);
    assert_eq!(POOL.held(), 128 * MIB, "its bytes go back with its G");
    drop(other);
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}

#[test]
fn an_overdrawn_admission_draws_under_its_own_grant() {
    let _g = armed();
    let other = admit(128 * MIB);
    let nested = POOL.admit_overdrawn(Grant::of(GRANT_FLOOR), GRANT_FLOOR, Overdraw::Nested);
    // its G is sized at the floor: the statement's draws are charged there, as an admitted statement's are
    assert_eq!(nested.left(), GRANT_FLOOR, "the whole floor is the statement's to draw");
    assert_eq!(Admitted::Overdrawn.name(), "past the pool (overdraw)");
    assert_eq!((Overdraw::Nested.name(), Overdraw::Seen.name()), ("nested_past", "seen_past"));
    drop(nested);
    drop(other);
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}
