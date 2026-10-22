//! Lane ladderview: the ladder's read-only counters (`mcx::ladder_stats`, served as `pgrust_memory_ladder_stats()` and
//! `pgrust_memory_ladder_episodes()`) count a gate's wait and its refusal, with `pgrust.memory_ladder` off and on, and the
//! episode ring records each with its outcome and, on, the rungs its climb ran. The pool and the ladder are
//! process-wide: the tests take turns, and each disarms both at its end (d4324_ladder_gate's rig).

use std::sync::Mutex;
use std::time::{Duration, Instant};

use gravity_driver::{Grant, POOL, REGIONS};
use mcx::ladder_stats::{self as stats, Outcome, Site};

static LOCK: Mutex<()> = Mutex::new(());
const MIB: u64 = 1 << 20;

/// The fake server the climb reads: 2 GiB read against a 1 GiB line, 64 MiB reclaimable, a reclaim that frees nothing.
fn read() -> Option<u64> {
    Some(2048 * MIB)
}
fn reclaimable() -> u64 {
    64 * MIB
}
fn reclaim(_bytes: u64) -> u64 {
    0
}
fn cancel(_need: u64) -> bool {
    false
}
fn cancel_in_flight() -> bool {
    false
}
fn protected() -> u64 {
    0
}
static ACTS: mcx::ladder::Acts = mcx::ladder::Acts { read, reclaimable, reclaim, cancel, cancel_in_flight, protected, evict_protected: reclaim };

fn armed(mode: u8) -> std::sync::MutexGuard<'static, ()> {
    let g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    POOL.gate(0);
    REGIONS.trim();
    assert_eq!(POOL.held(), 0, "a previous test left bytes held");
    POOL.arm(128 * MIB);
    mcx::ladder::install(&ACTS);
    mcx::ladder::clear_narrow();
    mcx::ladder::publish(mode, 1024 * MIB);
    g
}

fn disarm() {
    mcx::ladder::publish(mcx::ladder::OFF, 0);
    mcx::ladder::clear_narrow();
    POOL.arm(0);
}

/// Two other statements holding the whole pool, admitted on their own thread and given back after `after`.
fn full_until(after: Duration) -> std::thread::JoinHandle<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    let h = std::thread::spawn(move || {
        let admit = || POOL.admit_need_until(Grant::of(64 * MIB), &|g| g.bytes, Duration::from_secs(5), &|| false).expect("a statement");
        let others = (admit(), admit());
        tx.send(()).expect("admitted");
        std::thread::sleep(after);
        drop(others);
    });
    rx.recv().expect("the others hold the pool");
    h
}

/// The newest episode of `site` in the ring.
fn last(site: Site) -> Option<stats::Episode> {
    let mut out = None;
    stats::for_each_episode(&mut |e| {
        if e.site == site {
            out = Some(*e);
        }
    });
    out
}

/// An admission that waits for the bytes another statement gives back, then one refused at its bound: both counted at
/// `admit`, each an episode with its outcome; on, the waiting one's climb ran rungs.
fn admission_waits_then_refuses(mode: u8) {
    let _g = armed(mode);
    let before = stats::site(Site::Admit);
    let giver = full_until(Duration::from_millis(300));
    let t0 = Instant::now();
    let admitted = POOL.admit_need_until(Grant::of(64 * MIB), &|g| g.bytes, Duration::from_secs(5), &|| false).expect("admitted");
    let waited = t0.elapsed();
    giver.join().expect("giver");
    drop(admitted);
    let mid = stats::site(Site::Admit);
    let covered = last(Site::Admit).expect("the wait is in the ring");
    // the pool stays full past the refused one's bound
    let giver = full_until(Duration::from_millis(1500));
    let refused = POOL.admit_need_until(Grant::of(64 * MIB), &|g| g.bytes, Duration::from_millis(200), &|| false);
    let refused_ep = last(Site::Admit).expect("the refusal is in the ring");
    giver.join().expect("giver");
    let after = stats::site(Site::Admit);
    disarm();
    assert!(waited >= Duration::from_millis(250), "it waited for the bytes given back ({waited:?})");
    assert_eq!((mid.waits, mid.refusals), (before.waits + 1, before.refusals), "one wait, no refusal");
    assert!(mid.wait_ns - before.wait_ns >= 250_000_000, "its wait's length is counted ({} ns)", mid.wait_ns - before.wait_ns);
    assert!(mid.wait_max_ns >= 250_000_000);
    assert_eq!(covered.outcome, Outcome::Covered);
    assert_eq!(covered.need, 64 * MIB);
    assert_eq!(covered.ran != 0, mode == mcx::ladder::ON, "the climb's rungs are the episode's with the ladder on only");
    assert!(refused.is_err(), "the full pool refused the second at its bound");
    assert_eq!((after.waits, after.refusals), (mid.waits + 1, mid.refusals + 1), "one more wait, refused");
    assert!(after.misses >= after.waits);
    assert_eq!(refused_ep.outcome, Outcome::Refused);
    assert!(refused_ep.seq > covered.seq);
}

#[test]
fn with_the_ladder_off_an_admissions_wait_and_refusal_are_counted() {
    admission_waits_then_refuses(mcx::ladder::OFF);
}

#[test]
fn with_the_ladder_on_an_admissions_wait_and_refusal_are_counted_with_its_rungs() {
    admission_waits_then_refuses(mcx::ladder::ON);
}

/// The scratch gate (on: `take_wait`): a wait for the bytes another statement gives back, then a refusal at once when no
/// statement holds the pool (kept bytes: no holder can give, `DrawWait::LastTry`).
#[test]
fn with_the_ladder_on_a_draws_wait_its_last_try_and_its_refusal_are_counted() {
    let _g = armed(mcx::ladder::ON);
    let before = stats::site(Site::Scratch);
    let giver = full_until(Duration::from_millis(300));
    let held = gravity_driver::mem::region::scratch_draw(2 * MIB);
    giver.join().expect("giver");
    drop(held);
    let mid = stats::site(Site::Scratch);
    let covered = last(Site::Scratch).expect("the wait is in the ring");
    let kept = POOL.keep(128 * MIB);
    let over = gravity_driver::mem::region::scratch_draw(2 * MIB);
    let refused = last(Site::Scratch).expect("the refusal is in the ring");
    drop(over);
    drop(kept);
    let after = stats::site(Site::Scratch);
    disarm();
    assert_eq!((mid.waits, mid.refusals), (before.waits + 1, before.refusals));
    assert!(mid.wait_ns - before.wait_ns >= 250_000_000, "its wait's length is counted");
    assert_eq!((covered.outcome, covered.need), (Outcome::Covered, 2 * MIB));
    assert!(covered.ran & (1 << mempolicy::ladder::Rung::EvictElastic as u16) != 0, "R2a ran in its climb: {:#x}", covered.ran);
    assert_eq!((covered.read, covered.line), (2048 * MIB, 1024 * MIB), "the ladder's read and line at its start");
    assert_eq!((after.waits, after.refusals, after.last_tries), (mid.waits + 1, mid.refusals + 1, mid.last_tries + 1));
    assert_eq!(refused.outcome, Outcome::Refused);
}
