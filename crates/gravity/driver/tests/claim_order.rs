//! R1D (b): the ordered output's claim order. The leader releases only the finished prefix from the head (units no claim
//! covers bridged by `first_live_at`), a failed claim ends it, and past the window only the head is admitted. (The width
//! 0 of most tests: the window alone bounds them; `admitted_claims_count_against_the_window_plus_the_width` has one.)
use std::sync::Arc;

use gravity_driver::{ClaimOrder, MorselRange};

fn c(lo: u64, hi: u64) -> MorselRange {
    MorselRange::new(lo, hi)
}

#[test]
fn only_the_finished_prefix_is_released_in_claim_order() {
    let o = ClaimOrder::new(8, 0, 0);
    // units 10..12 are dead: the claim after [6, 10) starts at 12
    let live = |u: u64| if (10..12).contains(&u) { 12 } else { u };
    let mut out = Vec::new();
    o.finish(c(3, 6), true);
    o.finish(c(12, 20), true);
    o.ready(&live, &mut out);
    assert!(out.is_empty(), "the head [0, 3) is still running");
    o.finish(c(0, 3), true);
    o.ready(&live, &mut out);
    assert_eq!(out, [0, 3]);
    o.finish(c(6, 10), true);
    o.ready(&live, &mut out);
    assert_eq!(out, [0, 3, 6, 12]);
}

#[test]
fn a_failed_claim_is_the_last_released() {
    let o = ClaimOrder::new(8, 0, 0);
    let mut out = Vec::new();
    o.finish(c(4, 8), true);
    o.finish(c(2, 4), false);
    o.finish(c(0, 2), true);
    o.ready(&|u| u, &mut out);
    assert_eq!(out, [0, 2], "the failed claim's rows before its error, then nothing after it");
    assert!(!o.admit(c(8, 10)), "no claim runs after the failure");
}

/// The test's parking (the pool lends the waiter's): a thread's word is its index in `THREADS` plus one.
static THREADS: std::sync::Mutex<Vec<std::thread::Thread>> = std::sync::Mutex::new(Vec::new());

fn current() -> u64 {
    let mut t = THREADS.lock().unwrap();
    t.push(std::thread::current());
    t.len() as u64
}

fn park_ms(ms: u64) {
    std::thread::park_timeout(std::time::Duration::from_millis(ms));
}

/// An order whose parked claims wait for a wake, not the clock: `ROOM_WAIT` (20 ms) is shorter than a loaded box can
/// leave the releasing thread unscheduled, so the tests that check "a release / stop woke it" give claims a minute
/// (D278 load flake): a missing wake still fails them (the overrun count, or a claim admitted after a stop).
fn waiting(window: usize, width: usize) -> Arc<ClaimOrder> {
    Arc::new(ClaimOrder::new(window, width, 0).with_room_wait(std::time::Duration::from_secs(60)))
}

fn unpark(w: u64) {
    if let Some(t) = THREADS.lock().unwrap().get(w as usize - 1) {
        t.unpark();
    }
}

#[test]
fn past_the_window_only_the_head_starts() {
    let o = waiting(2, 0);
    o.wake.arm(u64::MAX, unpark);
    o.wake.arm_park(current, park_ms);
    o.finish(c(1, 2), true);
    o.finish(c(2, 3), true);
    assert!(o.admit(c(0, 1)), "the head never waits");
    let waiter = {
        let o = o.clone();
        std::thread::spawn(move || o.admit(c(3, 4)))
    };
    // the claim parks for room: the window holds two finished claims and it is not the head
    while o.waits.load(std::sync::atomic::Ordering::Relaxed) == 0 {
        std::thread::yield_now();
    }
    o.finish(c(0, 1), true);
    let mut out = Vec::new();
    o.ready(&|u| u, &mut out);
    assert_eq!(out, [0, 1, 2]);
    assert!(waiter.join().unwrap());
    assert_eq!(o.overruns.load(std::sync::atomic::Ordering::Relaxed), 0, "the release woke it, not the 20 ms timeout");
    o.stop();
    assert!(!o.admit(c(4, 5)));
}

/// D2985: G refused the window's bytes (narrowed) and the head is finished, waiting for the leader whose sink is slow (a
/// COPY TO a client that reads slower than the workers produce): past the overrun cap (2 x W claims finished or admitted) a
/// claim keeps waiting through `ROOM_WAIT` expiries instead of overrunning, so the legs stay bounded; the leader's release
/// admits it. (Before: each expiry ran a claim, and the legs grew to the table, 28.9 GB.)
#[test]
fn a_finished_head_waiting_for_the_leader_never_overruns_a_narrowed_window() {
    let o = Arc::new(ClaimOrder::new(2, 0, 0).with_room_wait(std::time::Duration::from_millis(5)));
    o.wake.arm(u64::MAX, unpark);
    o.wake.arm_park(current, park_ms);
    assert!(o.admit(c(0, 1)), "the head never waits");
    o.finish(c(0, 1), true);
    o.finish(c(1, 2), true);
    o.narrow();
    let waiter = {
        let o = o.clone();
        std::thread::spawn(move || o.admit(c(2, 3)))
    };
    // 20 room waits' worth of the leader blocked on its sink
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(!waiter.is_finished(), "narrowed, the head finished: the claim still waits for the leader");
    assert_eq!(o.overruns.load(std::sync::atomic::Ordering::Relaxed), 0);
    let mut out = Vec::new();
    o.ready(&|u| u, &mut out);
    assert_eq!(out, [0, 1]);
    assert!(waiter.join().unwrap(), "the release admitted it");
    assert_eq!(o.overruns.load(std::sync::atomic::Ordering::Relaxed), 0);
}

/// D2985: a window G has not narrowed (and a narrowed one under its overrun cap) keeps the overrun behind a slow head (a cold head faulting its part's dictionaries):
/// the claim past the window starts at `ROOM_WAIT`, its bytes in the grant.
#[test]
fn an_unnarrowed_window_overruns_behind_a_running_head() {
    let o = Arc::new(ClaimOrder::new(1, 0, 0).with_room_wait(std::time::Duration::from_millis(5)));
    o.wake.arm(u64::MAX, unpark);
    o.wake.arm_park(current, park_ms);
    assert!(o.admit(c(0, 1)), "the head never waits");
    o.finish(c(1, 2), true);
    let o2 = o.clone();
    assert!(std::thread::spawn(move || o2.admit(c(2, 3))).join().unwrap(), "admitted by the expiry");
    assert_eq!(o.overruns.load(std::sync::atomic::Ordering::Relaxed), 1);
}

/// D2985, narrowed: G refused the window's bytes, so only the head runs; while the head runs (the leader blocked on its
/// sink behind it) and the overrun cap is reached, a claim waits through `ROOM_WAIT` expiries; the head's finish and the
/// release admit it.
#[test]
fn a_running_head_keeps_a_narrowed_window_closed() {
    let o = Arc::new(ClaimOrder::new(2, 0, 0).with_room_wait(std::time::Duration::from_millis(5)));
    o.wake.arm(u64::MAX, unpark);
    o.wake.arm_park(current, park_ms);
    assert!(o.admit(c(0, 1)), "the head never waits");
    assert!(o.admit(c(1, 2)), "room in the window");
    o.finish(c(1, 2), true);
    o.narrow();
    let waiter = {
        let o = o.clone();
        std::thread::spawn(move || o.admit(c(2, 3)))
    };
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(!waiter.is_finished(), "narrowed, the head running and the cap reached: the claim waits");
    assert_eq!(o.overruns.load(std::sync::atomic::Ordering::Relaxed), 0);
    o.finish(c(0, 1), true);
    let mut out = Vec::new();
    o.ready(&|u| u, &mut out);
    assert_eq!(out, [0, 1]);
    assert!(waiter.join().unwrap(), "now the head: admitted");
    assert_eq!(o.overruns.load(std::sync::atomic::Ordering::Relaxed), 0);
}

/// A head that never finishes (a hand-out that skips it) still overruns after `ROOM_WAIT`: the section must not stall.
#[test]
fn a_missed_head_still_overruns_after_the_room_wait() {
    let o = Arc::new(ClaimOrder::new(1, 0, 0).with_room_wait(std::time::Duration::from_millis(5)));
    o.wake.arm(u64::MAX, unpark);
    o.wake.arm_park(current, park_ms);
    o.finish(c(1, 2), true);
    let o2 = o.clone();
    assert!(std::thread::spawn(move || o2.admit(c(2, 3))).join().unwrap(), "admitted by the expiry");
    assert_eq!(o.overruns.load(std::sync::atomic::Ordering::Relaxed), 1);
}

#[test]
fn a_stop_releases_a_parked_claim() {
    let o = waiting(1, 0);
    o.wake.arm(u64::MAX, unpark);
    o.wake.arm_park(current, park_ms);
    o.finish(c(1, 2), true);
    let waiter = {
        let o = o.clone();
        std::thread::spawn(move || o.admit(c(2, 3)))
    };
    while o.waits.load(std::sync::atomic::Ordering::Relaxed) == 0 {
        std::thread::yield_now();
    }
    o.stop();
    assert!(!waiter.join().unwrap(), "a claim parked for room leaves when the leader stops");
}

#[test]
fn unarmed_the_bound_slips_and_nothing_waits() {
    let o = ClaimOrder::new(1, 0, 0);
    o.finish(c(1, 2), true);
    assert!(o.admit(c(2, 3)), "no pool parking: admitted at once (an overrun), the order still holds");
    assert_eq!(o.overruns.load(std::sync::atomic::Ordering::Relaxed), 1);
}

/// D278: the pool admits a claim before its lease, so the admitted claims waiting for a slot count against the bound with
/// the finished ones (window + width), not only the finished ones; the head still never waits, and a finish settles the
/// count (it moves a claim from admitted to finished: no room opens until the leader releases a prefix).
#[test]
fn admitted_claims_count_against_the_window_plus_the_width() {
    let o = waiting(2, 1);
    o.wake.arm(u64::MAX, unpark);
    o.wake.arm_park(current, park_ms);
    assert!(o.admit(c(1, 2)) && o.admit(c(2, 3)) && o.admit(c(3, 4)), "window 2 + width 1: three claims start");
    assert_eq!(o.admitted(), 3);
    assert!(o.admit(c(0, 1)), "the head starts past the bound");
    let waiter = {
        let o = o.clone();
        std::thread::spawn(move || o.admit(c(4, 5)))
    };
    while o.waits.load(std::sync::atomic::Ordering::Relaxed) == 0 {
        std::thread::yield_now();
    }
    o.finish(c(1, 2), true);
    o.finish(c(2, 3), true);
    o.finish(c(3, 4), true);
    assert_eq!(o.admitted(), 1, "the head alone runs; the three wait for the leader, still inside the bound");
    let mut out = Vec::new();
    o.ready(&|u| u, &mut out);
    assert!(out.is_empty());
    o.finish(c(0, 1), true);
    o.ready(&|u| u, &mut out);
    assert_eq!(out, [0, 1, 2, 3]);
    assert!(waiter.join().unwrap(), "the release made room");
    assert_eq!((o.admitted(), o.overruns.load(std::sync::atomic::Ordering::Relaxed)), (1, 0));
}

/// D278: a claim failed (the pool's `fail`): parked claims above it leave unrun, those below it (handed out before it)
/// start at once without room, so their rows and a lower error still come first; none waits out ROOM_WAIT.
#[test]
fn a_failed_claim_skips_the_claims_above_it_and_starts_those_below() {
    let o = waiting(1, 0);
    o.wake.arm(u64::MAX, unpark);
    o.wake.arm_park(current, park_ms);
    o.finish(c(4, 5), true);
    let spawn = |claim: MorselRange| {
        let o = o.clone();
        std::thread::spawn(move || o.admit(claim))
    };
    let (below, above) = (spawn(c(2, 3)), spawn(c(6, 7)));
    while o.waits.load(std::sync::atomic::Ordering::Relaxed) < 2 {
        std::thread::yield_now();
    }
    o.stop_above(5);
    assert!(below.join().unwrap(), "a claim below the failed one starts");
    assert!(!above.join().unwrap(), "a claim above the failed one is skipped");
    assert!(o.admit(c(3, 4)) && !o.admit(c(8, 9)));
    assert_eq!(o.overruns.load(std::sync::atomic::Ordering::Relaxed), 0);
}

std::thread_local! {
    /// Set on the leader's thread around `ready`: the unparks it makes there are counted.
    static LEADING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
static LEADER_UNPARKS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn unpark_counted(w: u64) {
    if LEADING.with(|l| l.get()) {
        LEADER_UNPARKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    unpark(w);
}

/// D278 lever (1): a release wakes at most two parked claims from the leader (each futex wake could preempt it: at W8
/// the leader woke every parked claim, one wake at a time, on each release); every claim admitted passes the wake on
/// while room is left, so all the parked claims still start as room opens, none by the ROOM_WAIT timeout.
#[test]
fn a_release_wakes_two_parked_claims_and_the_admitted_pass_the_wake_on() {
    let o = waiting(2, 1);
    o.wake.arm(u64::MAX, unpark_counted);
    o.wake.arm_park(current, park_ms);
    for lo in 1..3 {
        o.finish(c(lo, lo + 1), true);
    }
    assert!(o.admit(c(3, 4)), "the third claim fills the window + width");
    assert!(o.admit(c(0, 1)), "the head runs past the bound");
    let waiters: Vec<_> = (4..10u64)
        .map(|lo| {
            let o = o.clone();
            std::thread::spawn(move || o.admit(c(lo, lo + 1)))
        })
        .collect();
    while o.waits.load(std::sync::atomic::Ordering::Relaxed) < 6 {
        std::thread::yield_now();
    }
    let mut out = Vec::new();
    let mut next = 0;
    // the leader releases claim by claim: each claim admitted runs and finishes here, in claim order
    while next < 10 {
        o.finish(c(next, next + 1), true);
        let before = LEADER_UNPARKS.load(std::sync::atomic::Ordering::Relaxed);
        LEADING.with(|l| l.set(true));
        o.ready(&|u| u, &mut out);
        LEADING.with(|l| l.set(false));
        assert!(LEADER_UNPARKS.load(std::sync::atomic::Ordering::Relaxed) - before <= 2, "a release woke more than two parked claims");
        next = *out.last().unwrap() + 1;
        // the claims admitted so far (their threads return true) finish before the next release
        let t0 = std::time::Instant::now();
        while o.admitted() < (10 - next).min(3) as usize && t0.elapsed() < std::time::Duration::from_secs(5) {
            std::thread::yield_now();
        }
    }
    assert!(waiters.into_iter().all(|w| w.join().unwrap()), "every parked claim started");
    assert_eq!(o.overruns.load(std::sync::atomic::Ordering::Relaxed), 0, "no claim started by the ROOM_WAIT timeout");
}

/// D278: a section's window is 2 x W with a floor of 16 claims (W2 16, W8 16, W16 32): below W8 the 2 x W window left
/// an ordered emit parking most of its claims. At W2 the 16 + 2 claims past the head start without waiting; the next waits.
#[test]
fn a_sections_window_is_twice_the_width_at_least_sixteen() {
    assert_eq!([1, 2, 4, 8, 12, 16].map(|w| ClaimOrder::for_section(w, 0).window()), [16, 16, 16, 16, 24, 32]);
    let o = ClaimOrder::for_section(2, 0);
    for lo in 1..=18 {
        assert!(o.admit(c(lo, lo + 1)));
    }
    assert_eq!(o.waits.load(std::sync::atomic::Ordering::Relaxed), 0, "the window + W claims started without waiting");
    assert!(o.admit(c(19, 20)), "unarmed: admitted as an overrun");
    assert_eq!(o.waits.load(std::sync::atomic::Ordering::Relaxed), 1, "the nineteenth claim past the head waited for room");
}
