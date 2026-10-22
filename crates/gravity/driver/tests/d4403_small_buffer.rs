//! D4403: the small buffer of every grant mode but lanes (`GrantPool::set_buffer`). Its size is clamped to the
//! capacity / 16 and, with the memory budget on, to B / 16, re-clamped
//! at every arm and every B the budget reads. When it is full a small statement and a tiny draw with no reservation wait
//! for it or the big lane (never refused at once): the statement until its bound or its interrupt, the draw on its
//! cancel-polled step for at most `bytes_wait()` (K::BytesWaitMs, 2000 ms). One process-wide pool: the tests run under
//! one lock and each re-arms it.

use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

use gravity_driver::mem::budget::{self, Cfg, Mode};
use gravity_driver::{try_region, Grant, POOL, REGIONS};

static LOCK: Mutex<()> = Mutex::new(());
const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;
const TINY: usize = 220;

struct Armed(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

fn armed(capacity: u64, buffer: u64) -> Armed {
    let g = LOCK.lock().unwrap_or_else(|p| {
        LOCK.clear_poison();
        p.into_inner()
    });
    POOL.gate(0);
    REGIONS.trim();
    assert_eq!(POOL.held(), 0, "a previous test left bytes held");
    POOL.arm(capacity);
    POOL.set_buffer(buffer);
    Armed(g)
}

impl Drop for Armed {
    fn drop(&mut self) {
        budget::configure(Cfg::DEFAULT);
        POOL.set_buffer(0);
        POOL.arm(0);
    }
}

/// A small statement on its own session holding `bytes` of the buffer until the returned sender sends (or drops).
fn small_holder(bytes: u64) -> (mpsc::Sender<()>, std::thread::JoinHandle<()>) {
    let (seated_tx, seated_rx) = mpsc::channel();
    let (end_tx, end_rx) = mpsc::channel::<()>();
    let h = std::thread::spawn(move || {
        let a = POOL.admit_small_until(Grant::of(MIB), &|_| bytes, Duration::ZERO, &|| false).expect("a buffer").expect("seated");
        assert!(a.small, "in the buffer");
        seated_tx.send(()).unwrap();
        let _ = end_rx.recv_timeout(Duration::from_secs(30));
        drop(a);
    });
    seated_rx.recv().unwrap();
    (end_tx, h)
}

#[test]
fn the_buffer_is_at_most_a_sixteenth_of_the_capacity_and_of_b_re_clamped_as_they_move() {
    let _g = armed(4 * GIB, mempolicy::admit::small_buffer_default());
    assert_eq!(POOL.reserve(), 128 * MIB, "128 MiB under a 4 GiB pool");
    POOL.arm(1150 * MIB);
    assert_eq!(POOL.reserve(), 1150 * MIB / 16, "re-clamped at the arm (the broker's follow): capacity / 16, 71.9 MiB of an 8 GiB stress's 1150 MiB pool");
    POOL.arm(4 * GIB);
    assert_eq!(POOL.reserve(), 128 * MIB, "back with the capacity");
    // the budget on: B / 16 too, re-clamped at every snapshot (cell b's 1536M scope: B 416 MiB, a 26 MiB buffer)
    budget::set_fallback_limit(1536 * MIB);
    budget::configure(Cfg { mode: Mode::On, ..Cfg::DEFAULT });
    let b = budget::snapshot().expect("mode on").budget.b;
    assert!(b != 0 && b / 16 < 128 * MIB, "a tight scope's B: {b}");
    assert_eq!(POOL.reserve(), b / 16, "B / 16 under the budget");
    budget::set_fallback_limit(64 * GIB);
    let wide = budget::snapshot().expect("mode on").budget.b;
    assert!(wide / 16 >= 128 * MIB);
    assert_eq!(POOL.reserve(), 128 * MIB, "B moved: re-clamped; nothing changes at large B");
    budget::set_fallback_limit(1536 * MIB);
    assert!(budget::snapshot().is_some());
    budget::configure(Cfg { mode: Mode::Advise, ..Cfg::DEFAULT });
    assert!(budget::snapshot().is_some());
    assert_eq!(POOL.reserve(), 128 * MIB, "advise: no B clamp");
    POOL.set_buffer(0);
    assert_eq!(POOL.reserve(), 0, "none");
}

#[test]
fn a_small_statement_waits_for_the_full_buffer_and_is_seated_when_a_holder_ends() {
    let _g = armed(512 * MIB, 32 * MIB);
    assert_eq!(POOL.reserve(), 32 * MIB);
    let big = try_region((480 * MIB) as usize).expect("the big lane whole");
    let (end, holder) = small_holder(32 * MIB);
    let before = POOL.stats();
    // the buffer and the big lane both full: interrupted at once, never an out-of-memory refusal
    let r = POOL.admit_small_until(Grant::of(MIB), &|_| 4 * MIB, Duration::from_secs(5), &|| true).expect("a buffer");
    assert!(r.as_ref().is_err_and(|e| e.interrupted), "an interrupt ends the wait: {:?}", r.map(|a| a.how));
    // both full, nobody gives back within the bound: refused after it
    let t0 = Instant::now();
    let r = POOL.admit_small_until(Grant::of(MIB), &|_| 4 * MIB, Duration::from_millis(200), &|| false).expect("a buffer");
    assert!(r.as_ref().is_err_and(|e| !e.interrupted) && t0.elapsed() >= Duration::from_millis(200), "refused after its bound");
    // a holder ends 200 ms in: the waiting statement is seated in the buffer
    let ender = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        end.send(()).unwrap();
    });
    let t0 = Instant::now();
    let a = POOL.admit_small_until(Grant::of(MIB), &|_| 4 * MIB, Duration::from_secs(10), &|| false).expect("a buffer").expect("seated once the holder ended");
    let took = t0.elapsed();
    ender.join().unwrap();
    holder.join().unwrap();
    assert!(a.small && took >= Duration::from_millis(150) && took < Duration::from_secs(5), "waited for the holder: {took:?}");
    let after = POOL.stats();
    assert_eq!((after.small_waits - before.small_waits, after.small_refusals - before.small_refusals), (3, 1), "three waits, one refusal (the interrupt is not one)");
    drop(a);
    drop(big);
    assert_eq!((POOL.held(), POOL.small_held()), (0, 0));
}

#[test]
fn a_small_statement_takes_the_big_lane_at_once_when_the_buffer_is_full() {
    let _g = armed(512 * MIB, 32 * MIB);
    let (end, holder) = small_holder(32 * MIB);
    let a = POOL.admit_small_until(Grant::of(MIB), &|_| 4 * MIB, Duration::from_secs(5), &|| false).expect("a buffer").expect("the big lane has room");
    assert!(!a.small && a.waited_ms == 0, "on the big lane at once: {a:?}");
    assert!(POOL.admit_small_until(Grant::of(MIB), &|_| 64 * MIB, Duration::ZERO, &|| false).is_none(), "a need past the buffer takes the big lane's road");
    drop(a);
    end.send(()).unwrap();
    holder.join().unwrap();
    assert_eq!((POOL.held(), POOL.small_held()), (0, 0));
}

#[test]
fn a_tiny_draw_waits_for_the_full_buffer_and_is_served_when_a_holder_ends() {
    let _g = armed(512 * MIB, 32 * MIB);
    let big = try_region((480 * MIB) as usize).expect("the big lane whole");
    let (end, holder) = small_holder(32 * MIB);
    let before = POOL.stats();
    // nobody gives back: the draw waits bytes_wait() (2000 ms), then is refused as before the buffer
    let t0 = Instant::now();
    assert!(try_region(TINY).is_err(), "refused once its wait ends");
    let took = t0.elapsed();
    assert!(took >= Duration::from_millis(1900) && took < Duration::from_secs(10), "waited bytes_wait(): {took:?}");
    // a holder ends 200 ms in: the draw is served from the buffer
    let ender = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        end.send(()).unwrap();
    });
    let t0 = Instant::now();
    let set = try_region(TINY).expect("served once the holder ended");
    let took = t0.elapsed();
    ender.join().unwrap();
    holder.join().unwrap();
    assert!(took >= Duration::from_millis(150) && took < Duration::from_millis(1900), "waited for the holder: {took:?}");
    assert_eq!(POOL.small_held(), TINY as u64, "in the buffer");
    let after = POOL.stats();
    assert_eq!((after.small_waits - before.small_waits, after.small_refusals - before.small_refusals, after.small_draws - before.small_draws), (2, 1, 1));
    drop(set);
    drop(big);
    assert_eq!((POOL.held(), POOL.small_held()), (0, 0));
}
