//! D3127: an admission the pool alone would seat but the memory governor refuses, with nothing held to wait for, may be a
//! release the governor's 100 ms sample has not seen yet. A fresh sample decides: the whole need is admitted when it fits,
//! waits while the resident bytes keep falling (`SETTLE_FALL`; `GOVERNOR_SETTLE` at most), and halves at once, as before,
//! when they hold (`SETTLE_FLAT_POLLS`): a real refusal costs about two poll pauses, not the settle's bound. The governor and
//! the pool are process-wide: the tests run under one lock, and each disarms both at its end. D4764: the settle's clock
//! moves one 2 ms poll per fresh sample (`POLL_NS`), never with the box's load; waits are bounded in samples, not wall time.

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::Duration;

use gravity_driver::mem::grant::{set_settle_clock_for_test, GOVERNOR_SETTLE, SETTLE_FLAT_POLLS};
use gravity_driver::{Admitted, Grant, GRANT_FLOOR, POOL, REGIONS};

static LOCK: Mutex<()> = Mutex::new(());
const MIB: u64 = 1 << 20;
const BAR: usize = 1 << 30;
const STALE: usize = 900 << 20;
const SETTLED: usize = 100 << 20;
const POLL_NS: u64 = 2_000_000;

/// What the resident probe reads now (the process's real resident bytes, in the server), how far each read falls and for
/// how many reads (a release in progress), and whether it answers at all.
static NOW: AtomicUsize = AtomicUsize::new(0);
static FALL: AtomicUsize = AtomicUsize::new(0);
static FALLS: AtomicU32 = AtomicU32::new(0);
static READS: AtomicU32 = AtomicU32::new(0);
static ANSWERS: AtomicU32 = AtomicU32::new(1);

fn resident_now() -> Option<usize> {
    let n = READS.fetch_add(1, Relaxed) + 1;
    set_settle_clock_for_test(Some(u64::from(n) * POLL_NS));
    if ANSWERS.load(Relaxed) == 0 {
        return None;
    }
    if FALLS.load(Relaxed) > 0 {
        FALLS.fetch_sub(1, Relaxed);
        NOW.fetch_sub(FALL.load(Relaxed), Relaxed);
    }
    Some(NOW.load(Relaxed))
}

/// The pool armed at 1 GiB; the governor's last sample at `sampled` resident bytes of a 1 GiB bar; the probe at `now`,
/// falling `fall` bytes at each of its next `falls` reads.
fn armed(sampled: usize, now: usize, fall: usize, falls: u32) -> std::sync::MutexGuard<'static, ()> {
    let g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    POOL.gate(0);
    REGIONS.trim();
    assert_eq!(POOL.held(), 0, "a previous test left bytes held");
    POOL.arm(1 << 30);
    (NOW.store(now, Relaxed), FALL.store(fall, Relaxed), FALLS.store(falls, Relaxed), READS.store(0, Relaxed), ANSWERS.store(1, Relaxed));
    set_settle_clock_for_test(Some(0));
    mcx::governor::set_resident_probe(resident_now);
    mcx::governor::publish(BAR, sampled, mcx::global_footprint::bytes());
    g
}

fn disarm() {
    mcx::governor::publish(0, 0, mcx::global_footprint::bytes());
    POOL.arm(0);
    set_settle_clock_for_test(None);
}

fn admit_256() -> gravity_driver::Reservation {
    POOL.admit_need_until(Grant::of(256 * MIB), &|g| g.bytes, Duration::from_secs(5), &|| false).expect("admitted")
}

fn reads() -> u32 {
    READS.load(Relaxed)
}

#[test]
fn a_fresh_sample_admits_the_whole_need_at_once() {
    let _g = armed(STALE, SETTLED, 0, 0);
    assert!(!mcx::governor::fits_raw((256 * MIB) as usize), "the stale sample refuses 256 MiB");
    let a = admit_256();
    assert_eq!((a.grant.bytes, a.how), (256 * MIB, Admitted::Whole), "the fresh sample seats the whole need, no halving");
    assert_eq!(reads(), 1, "one fresh sample, no wait");
    drop(a);
    disarm();
}

#[test]
fn a_release_in_progress_is_waited_out_and_the_whole_need_admitted() {
    // 900 MiB resident falling 32 MiB a read: 256 MiB fits under the 1 GiB bar from 768 MiB, the fifth read on
    let _g = armed(STALE, STALE, 32 << 20, 1000);
    let a = admit_256();
    assert_eq!((a.grant.bytes, a.how), (256 * MIB, Admitted::Whole), "waited for the release, no halving");
    assert_eq!(reads(), 5, "fresh samples while it fell, the fifth seats it");
    assert!(Duration::from_nanos(u64::from(reads()) * POLL_NS) < GOVERNOR_SETTLE);
    drop(a);
    disarm();
}

#[test]
fn a_refusal_that_holds_halves_after_the_flat_samples_not_the_settle_bound() {
    let _g = armed(STALE, STALE, 0, 0);
    let a = admit_256();
    // 900 MiB resident of a 1 GiB bar: 128 MiB more is refused, the 64 MiB floor fits
    assert_eq!((a.grant.bytes, a.how), (GRANT_FLOOR, Admitted::Idle), "halved with nothing held to wait for");
    assert!(reads() <= 1 + SETTLE_FLAT_POLLS, "about two poll pauses, not the settle's bound: {} samples", reads());
    drop(a);
    disarm();
}

#[test]
fn a_release_that_stops_short_of_the_need_halves_once_it_holds() {
    // 900 MiB falling 32 MiB for 3 reads, then holding at 804 MiB: 256 MiB never fits, its half (128 MiB) does
    let _g = armed(STALE, STALE, 32 << 20, 3);
    let a = admit_256();
    assert_eq!((a.grant.bytes, a.how), (128 * MIB, Admitted::Idle));
    assert!(reads() >= 4, "waited while it fell: {}", reads());
    assert!(reads() <= 3 + SETTLE_FLAT_POLLS, "then the flat samples, not the settle's bound: {}", reads());
    drop(a);
    disarm();
}

#[test]
fn without_a_fresh_sample_the_need_halves_at_once() {
    // no probe answer (the governor's sampler not running here): the last sample stands and the need halves as before
    let _g = armed(STALE, SETTLED, 0, 0);
    ANSWERS.store(0, Relaxed);
    let a = admit_256();
    assert_eq!((a.grant.bytes, a.how), (GRANT_FLOOR, Admitted::Idle));
    assert_eq!(reads(), 1, "one try at a fresh sample, no wait");
    drop(a);
    disarm();
}

#[test]
fn an_admission_the_pool_cannot_seat_halves_without_a_fresh_sample() {
    let _g = armed(STALE, SETTLED, 0, 0);
    // 2 GiB of a 1 GiB pool: the pool refuses it whatever the governor says, so no fresh sample is taken and the stale one
    // halves the grant to the floor
    let a = POOL.admit_need_until(Grant::of(2048 * MIB), &|g| g.bytes, Duration::from_secs(5), &|| false).expect("admitted");
    assert_eq!((a.grant.bytes, a.how), (GRANT_FLOOR, Admitted::Idle));
    assert_eq!(reads(), 0, "no fresh sample");
    drop(a);
    disarm();
}

#[test]
fn a_release_too_slow_to_cover_the_deficit_halves_at_once() {
    // a 64 GiB bar with 1 GiB of room and a 41 GiB ask: 40 GiB missing, the resident falling 17 MiB a read (~8 MiB/ms at the
    // 2 ms poll, ~8 GiB over the whole bound): the second fresh sample shows the fall cannot cover it, so no settle wait
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    POOL.gate(0);
    REGIONS.trim();
    POOL.arm(64 << 30);
    let resident = (63usize << 30) as usize;
    (NOW.store(resident, Relaxed), FALL.store(17 << 20, Relaxed), FALLS.store(1000, Relaxed), READS.store(0, Relaxed), ANSWERS.store(1, Relaxed));
    set_settle_clock_for_test(Some(0));
    mcx::governor::set_resident_probe(resident_now);
    mcx::governor::publish(64 << 30, resident, mcx::global_footprint::bytes());
    let a = POOL.admit_need_until(Grant::of(41 << 30), &|g| g.bytes, Duration::from_secs(5), &|| false).expect("admitted");
    assert_eq!(a.how, Admitted::Idle);
    assert!(a.grant.bytes < 1 << 30, "halved into the room: {} MiB", a.grant.bytes >> 20);
    assert_eq!(reads(), 2, "the first fresh sample and one more, no wait past it");
    drop(a);
    disarm();
}
