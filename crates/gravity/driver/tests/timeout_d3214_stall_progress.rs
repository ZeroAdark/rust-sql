//! D3214: a queued waiter's `QUEUE_STALL` clock restarts only at a new high of its room (the pool's room beside what is held
//! and the turn's need, and the memory governor's), by `STALL_PROGRESS` or more; a statement's end that raises nothing does
//! not restart it. A waiter holds its ticket for `TURN_CAP` at most, progress or not, then halves; the ladder stays. The
//! bounds run in milliseconds here (`admit_need_within`). The governor and the pool are process-wide: the tests run under
//! one lock, and each disarms both at its end.
//! D4555: a release thread waking late (a loaded box: the crate-test pool beside a guest precheck, load 30 on 16 CPUs) let
//! the stall run out before the cap. The file is named timeout_* for the crate tests' serial tail (check.sh CHECK_CT_SERIAL:
//! it runs alone, after e2e), and every bound and period is twice the first one's, so a release may wake 300 ms late, not
//! 150 ms, before a test misreads it (the 64 MiB progress grain and the 768 MiB need keep the stall / period ratio).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use gravity_driver::mem::grant::{StallBounds, STALL_PROGRESS};
use gravity_driver::{Admitted, Grant, POOL, REGIONS};

static LOCK: Mutex<()> = Mutex::new(());
const MIB: u64 = 1 << 20;
const BAR: usize = 1 << 30;
const NEED: u64 = 768 * MIB;
const BOUNDS: StallBounds = StallBounds { stall: Duration::from_millis(600), cap: Duration::from_millis(1800) };
const WAIT: Duration = Duration::from_secs(5);

/// The pool armed at 1 GiB, ungated; the governor's sample at `resident` bytes of a 1 GiB bar.
fn armed(resident: usize) -> std::sync::MutexGuard<'static, ()> {
    let g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    POOL.gate(0);
    REGIONS.trim();
    assert_eq!(POOL.held(), 0, "a previous test left bytes held");
    POOL.arm(1 << 30);
    sample(resident);
    g
}

fn sample(resident: usize) {
    mcx::governor::publish(BAR, resident, mcx::global_footprint::bytes());
}

fn disarm() {
    mcx::governor::publish(0, 0, mcx::global_footprint::bytes());
    POOL.arm(0);
}

/// Other sessions' statements: one 32 MiB statement held throughout (the waiter is never idle: it has a statement to wait
/// for) and one admitted, held `every` and ended, over and over: each end moves the pool's `ended` count, and the next
/// statement takes the bytes back. Returns the stop flag, the ends counted and the threads.
fn others(every: Duration) -> (Arc<AtomicBool>, Arc<AtomicUsize>, Vec<JoinHandle<()>>) {
    let (stop, ends) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicUsize::new(0)));
    let held = Arc::new(AtomicBool::new(false));
    let mut threads = Vec::new();
    let (s, h) = (stop.clone(), held.clone());
    threads.push(std::thread::spawn(move || {
        let a = POOL.admit_need_until(Grant::of(32 * MIB), &|g| g.bytes, WAIT, &|| false).expect("the anchor statement");
        h.store(true, Relaxed);
        while !s.load(Relaxed) {
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(a);
    }));
    let t0 = Instant::now();
    while !held.load(Relaxed) {
        assert!(!threads[0].is_finished() && t0.elapsed() < WAIT * 2, "the anchor statement was not admitted");
        std::thread::sleep(Duration::from_millis(1));
    }
    let (s, e) = (stop.clone(), ends.clone());
    threads.push(std::thread::spawn(move || {
        while !s.load(Relaxed) {
            let a = POOL.admit_need_until(Grant::of(32 * MIB), &|g| g.bytes, WAIT, &|| false).expect("a short statement");
            assert_eq!(a.grant.bytes, 32 * MIB, "the short statements fit beside the turn's need");
            std::thread::sleep(every);
            drop(a);
            e.fetch_add(1, Relaxed);
        }
    }));
    (stop, ends, threads)
}

fn join(stop: Arc<AtomicBool>, threads: Vec<JoinHandle<()>>) {
    stop.store(true, Relaxed);
    for t in threads {
        t.join().expect("a statement thread");
    }
}

/// A release the governor sees: from `from` resident bytes, `step` less at every `every` (each step a new high of the
/// waiter's room: more than `STALL_PROGRESS`). Returns the stop flag and the thread.
fn release(from: usize, step: usize, every: Duration) -> (Arc<AtomicBool>, JoinHandle<()>) {
    assert!(step as u64 > STALL_PROGRESS);
    let falling = Arc::new(AtomicBool::new(true));
    let f = falling.clone();
    let t = std::thread::spawn(move || {
        let mut resident = from;
        while f.load(Relaxed) && resident > 0 {
            std::thread::sleep(every);
            resident = resident.saturating_sub(step);
            sample(resident);
        }
    });
    (falling, t)
}

fn admit_768() -> (gravity_driver::Reservation, Duration) {
    let t0 = Instant::now();
    let a = POOL.admit_need_within(Grant::of(NEED), &|g| g.bytes, WAIT, &|| false, BOUNDS).expect("admitted");
    (a, t0.elapsed())
}

#[test]
fn a_turn_beside_ends_that_never_free_its_need_halves_at_the_stall() {
    // the governor refuses the 768 MiB need beside 512 MiB resident; the short statements fit and end every 20 ms. Before
    // D3214 each end restarted the clock and the waiter sat to its wait's bound (here 5 s; a server's ~forever)
    let _g = armed(512 << 20);
    let (stop, ends, threads) = others(Duration::from_millis(20));
    let (a, took) = admit_768();
    let n = ends.load(Relaxed);
    join(stop, threads);
    assert_eq!(a.how, Admitted::Stall, "halved at the stall, not at the wait's bound ({took:?}, {n} ends)");
    assert_eq!(a.grant.bytes, NEED / 2, "the ladder's first step: 384 MiB fits under the bar");
    assert!(took >= BOUNDS.stall && took < BOUNDS.stall * 2, "halved at about the stall: {took:?}");
    assert!(n >= 5, "statements ended while it waited: {n}");
    drop(a);
    disarm();
}

#[test]
fn a_room_that_keeps_growing_keeps_the_turn_waiting_for_the_whole_need() {
    // the resident bytes fall 80 MiB every 200 ms (a release the governor sees), the short statements still ending: the
    // 768 MiB need fits at 256 MiB resident, ~1 s in, past the 600 ms stall, and is admitted whole
    let _g = armed(640 << 20);
    let (stop, _ends, threads) = others(Duration::from_millis(20));
    let (falling, rel) = release(640 << 20, 80 << 20, Duration::from_millis(200));
    let (a, took) = admit_768();
    falling.store(false, Relaxed);
    rel.join().expect("the release");
    join(stop, threads);
    assert_eq!((a.grant.bytes, a.how), (NEED, Admitted::Whole), "the whole need after {took:?}");
    assert!(took > BOUNDS.stall, "it waited past the stall: each step was a new high ({took:?})");
    drop(a);
    disarm();
}

#[test]
fn a_ticket_held_past_the_cap_halves_whatever_the_progress() {
    // the resident bytes fall 80 MiB every 300 ms from 900 MiB (the 32 MiB statements fit under the 1 GiB bar): the
    // 768 MiB need would fit only ~2.7 s in; the stall never runs out, the 1.8 s cap does, and the ladder seats 384 MiB
    // under the bar by then (420 MiB resident)
    let _g = armed(900 << 20);
    let (stop, _ends, threads) = others(Duration::from_millis(20));
    let (falling, rel) = release(900 << 20, 80 << 20, Duration::from_millis(300));
    let (a, took) = admit_768();
    falling.store(false, Relaxed);
    rel.join().expect("the release");
    join(stop, threads);
    assert_eq!((a.grant.bytes, a.how), (NEED / 2, Admitted::Capped), "halved at the cap after {took:?}");
    assert!(took >= BOUNDS.cap && took < BOUNDS.cap + BOUNDS.stall, "at about the cap: {took:?}");
    drop(a);
    disarm();
}
