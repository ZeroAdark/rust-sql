//! D4324, LADDER-DESIGN section 10 step 3: with `pgrust.memory_ladder = on` a gate whose take missed climbs the ladder on
//! its own thread before it waits (`mcx::ladder::climb`: here R2a, the caches' reclaim, then R3's flag), and its wait (R5)
//! ends on the bytes another statement gives back. The D4329 scratch draw is the gate here: with the ladder off it
//! overdrafts at once (no climb, no wait); on, it climbs, waits, and takes the freed bytes from the pool instead.
//! The pool and the ladder are process-wide: the tests take turns, and each disarms both at its end.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use gravity_driver::{Grant, Overdraw, POOL, REGIONS};

static LOCK: Mutex<()> = Mutex::new(());
const MIB: u64 = 1 << 20;

/// The fake server the climb reads: 2 GiB read against a 1 GiB line, 64 MiB reclaimable, a reclaim that frees nothing
/// (so the climb goes on to R3).
static RECLAIMS: AtomicU64 = AtomicU64::new(0);
fn read() -> Option<u64> {
    Some(2048 * MIB)
}
fn reclaimable() -> u64 {
    64 * MIB
}
fn reclaim(_bytes: u64) -> u64 {
    RECLAIMS.fetch_add(1, Relaxed);
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

fn armed() -> std::sync::MutexGuard<'static, ()> {
    let g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    POOL.gate(0);
    REGIONS.trim();
    assert_eq!(POOL.held(), 0, "a previous test left bytes held");
    POOL.arm(128 * MIB);
    mcx::ladder::install(&ACTS);
    mcx::ladder::clear_narrow();
    g
}

fn disarm() {
    mcx::ladder::publish(mcx::ladder::OFF, 0);
    mcx::ladder::clear_narrow();
    POOL.arm(0);
}

/// Two other statements holding the whole pool, admitted on their own thread (a worker's draw counts its own statement as
/// one holder, `Others::here`) and given back after `after`.
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

fn scratch_past() -> u64 {
    POOL.overdraws()[Overdraw::Scratch as usize].0
}

#[test]
fn with_the_ladder_on_a_gate_climbs_then_waits_for_the_bytes_given_back() {
    let _g = armed();
    mcx::ladder::publish(mcx::ladder::ON, 1024 * MIB);
    let (reclaims, climbs, past) = (RECLAIMS.load(Relaxed), mcx::ladder::counts().1, scratch_past());
    let scratches = gate(mcx::ladder::Gate::Scratch);
    let giver = full_until(Duration::from_millis(300));
    let t0 = Instant::now();
    let held = gravity_driver::mem::region::scratch_draw(2 * MIB);
    let waited = t0.elapsed();
    let narrowed = mcx::ladder::narrowed();
    giver.join().expect("giver");
    drop(held);
    disarm();
    assert!(RECLAIMS.load(Relaxed) > reclaims, "R2a ran: the caches were asked first");
    assert!(narrowed, "R2a freed nothing measured: R3 set its flag");
    assert!(mcx::ladder::counts().1 > climbs, "the climb is counted");
    assert!(waited >= Duration::from_millis(250), "it waited (R5) for the bytes the other statement gave back ({waited:?})");
    assert_eq!(scratch_past(), past, "the freed bytes came from the pool: no overdraft");
    assert!(gate(mcx::ladder::Gate::Scratch) > scratches, "the climb is counted as the scratch gate's");
}

#[test]
fn with_the_ladder_off_the_same_gate_overdrafts_at_once() {
    let _g = armed();
    mcx::ladder::publish(mcx::ladder::OFF, 1024 * MIB);
    let (reclaims, past) = (RECLAIMS.load(Relaxed), scratch_past());
    let giver = full_until(Duration::from_millis(300));
    let t0 = Instant::now();
    let held = gravity_driver::mem::region::scratch_draw(2 * MIB);
    let waited = t0.elapsed();
    drop(held);
    giver.join().expect("giver");
    disarm();
    assert_eq!(RECLAIMS.load(Relaxed), reclaims, "no climb");
    assert!(waited < Duration::from_millis(200), "no wait ({waited:?})");
    assert_eq!(scratch_past(), past + 1, "an overdraft past the full pool");
}

fn runs(r: mempolicy::ladder::Rung) -> u64 {
    mcx::ladder::counts().0[r as usize].0
}

fn gate(g: mcx::ladder::Gate) -> u64 {
    mcx::ladder::gate_climbs()[g as usize]
}

fn r5_runs() -> u64 {
    runs(mempolicy::ladder::Rung::Wait)
}

/// As `full_until`, with the holders' statement running on the governor and 64 MiB of it spillable (a revoke target, R4b).
fn full_and_spillable_until(after: Duration) -> std::thread::JoinHandle<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    let h = std::thread::spawn(move || {
        mcx::governor::register(48, 0);
        mcx::governor::statement_start(0, true);
        mcx::revoke::note_spillable(64 * MIB as i64);
        let admit = || POOL.admit_need_until(Grant::of(64 * MIB), &|g| g.bytes, Duration::from_secs(5), &|| false).expect("a statement");
        let others = (admit(), admit());
        tx.send(()).expect("admitted");
        std::thread::sleep(after);
        drop(others);
        mcx::revoke::note_spillable(-(64 * MIB as i64));
        mcx::governor::statement_end();
    });
    rx.recv().expect("the others hold the pool");
    h
}

/// D4353: admission is a gate too (LADDER-DESIGN section 3). A statement admitted while the pool is full climbs (R2a, R3)
/// and asks the holders to spill (R4b) before it waits, and its wait for the bytes another statement gives back counts as R5.
#[test]
fn with_the_ladder_on_an_admission_climbs_then_waits_as_r5() {
    let _g = armed();
    mcx::ladder::publish(mcx::ladder::ON, 1024 * MIB);
    mcx::revoke::set_enabled(true);
    let (reclaims, climbs, r5, r4b) = (RECLAIMS.load(Relaxed), mcx::ladder::counts().1, r5_runs(), runs(mempolicy::ladder::Rung::Revoke));
    let admits = gate(mcx::ladder::Gate::Admit);
    let giver = full_and_spillable_until(Duration::from_millis(300));
    let admitted = POOL.admit_need_until(Grant::of(64 * MIB), &|g| g.bytes, Duration::from_secs(5), &|| false).expect("admitted");
    let narrowed = mcx::ladder::narrowed();
    giver.join().expect("giver");
    drop(admitted);
    mcx::revoke::set_enabled(false);
    disarm();
    assert!(runs(mempolicy::ladder::Rung::Revoke) > r4b, "R4b ran: the holders' spillable bytes were asked");
    assert!(RECLAIMS.load(Relaxed) > reclaims, "R2a ran: the caches were asked first");
    assert!(narrowed, "R2a freed nothing measured: R3 set its flag");
    assert!(mcx::ladder::counts().1 > climbs, "the admission's climb is counted");
    assert!(r5_runs() > r5, "its wait is counted as R5");
    assert!(gate(mcx::ladder::Gate::Admit) > admits, "the climb is counted as admission's");
}

#[test]
fn with_the_ladder_off_an_admission_does_not_climb() {
    let _g = armed();
    mcx::ladder::publish(mcx::ladder::OFF, 1024 * MIB);
    let (reclaims, climbs, r5) = (RECLAIMS.load(Relaxed), mcx::ladder::counts().1, r5_runs());
    let giver = full_until(Duration::from_millis(300));
    let admitted = POOL.admit_need_until(Grant::of(64 * MIB), &|g| g.bytes, Duration::from_secs(5), &|| false).expect("admitted");
    giver.join().expect("giver");
    drop(admitted);
    disarm();
    assert_eq!(RECLAIMS.load(Relaxed), reclaims, "no climb");
    assert_eq!(mcx::ladder::counts().1, climbs, "no climb counted");
    assert_eq!(r5_runs(), r5, "no R5");
}
