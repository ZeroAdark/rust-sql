//! D4403 (born red): a tiny draw with no reservation, a hashed IN list's set (a 5 Hz broker sampler, `SELECT .. FROM
//! pgrust_memory_broker_stats() x WHERE consumer IN (..)`: 220 B), was refused whenever the big lane could not take it:
//! beside big admissions holding the lane (53200 x1198 in one stress take, 'grant pool: 4785700864 of 4785700864 bytes
//! held'), with B full and the pool 19 % held (B's make_room short 25363 times in another), and past the
//! memory governor's bar. A small buffer no big admission takes now serves it first, its take skipping B's make_room
//! and the bar. A Solo floor no longer waits on the small statements the buffer holds. Only APIs trunk has: the lanes
//! slice (`set_reserve`) is the buffer here. One process-wide pool: the tests run under one lock and each re-arms it.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

use gravity_driver::mem::budget::{self, actuator, Cfg, Consumer, Hooks, Mode};
use gravity_driver::{try_region, Grant, GRANT_FLOOR, POOL, REGIONS};

static LOCK: Mutex<()> = Mutex::new(());
const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;
/// The sampler's set: 220 bytes.
const TINY: usize = 220;

/// The pool armed at `capacity` with a `slice` small buffer (the lanes slice), nothing held; given back at the drop.
struct Armed(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

fn armed(capacity: u64, slice: u64) -> Armed {
    let g = LOCK.lock().unwrap_or_else(|p| {
        LOCK.clear_poison();
        p.into_inner()
    });
    POOL.gate(0);
    REGIONS.trim();
    assert_eq!(POOL.held(), 0, "a previous test left bytes held");
    POOL.arm(capacity);
    POOL.set_reserve(slice);
    assert_eq!(POOL.reserve(), slice);
    Armed(g)
}

impl Drop for Armed {
    fn drop(&mut self) {
        mcx::governor::publish(0, 0, mcx::global_footprint::bytes());
        budget::configure(Cfg::DEFAULT);
        FILL.store(0, Relaxed);
        POOL.set_reserve(0);
        POOL.arm(0);
    }
}

#[test]
fn a_tiny_draw_with_no_statement_is_served_beside_a_full_big_lane() {
    let _g = armed(GIB, 128 * MIB);
    // the big lane (896 MiB) held whole by a draw past the buffer's per-draw cap: it takes the big lane, as before
    let big = try_region((896 * MIB) as usize).expect("the big lane's whole room");
    assert!(try_region((2 * MIB) as usize).is_err(), "a draw past the buffer's cap still reads the big lane alone");
    let set = try_region(TINY).expect("the sampler's set: served from the small buffer, not refused");
    assert_eq!(POOL.small_held(), TINY as u64, "held in the buffer");
    drop(set);
    assert_eq!(POOL.small_held(), 0, "given back to the buffer");
    drop(big);
    assert_eq!(POOL.held(), 0);
}

static FILL: AtomicU64 = AtomicU64::new(0);

fn fill() -> u64 {
    FILL.load(Relaxed)
}

fn no_trim(_: u64) -> u64 {
    0
}

#[test]
fn a_tiny_draw_is_served_when_b_has_no_free_bytes_and_the_pool_is_mostly_free() {
    let _g = armed(GIB, 128 * MIB);
    // a 1536M stress cell: budget on with the reclaimer, B full (here a cache nothing can trim holding all of it),
    // the pool 19 % held: B's make_room came up short at every draw
    budget::set_fallback_limit(2 * GIB);
    budget::configure(Cfg { mode: Mode::On, actuators: actuator::ALL, ..Cfg::DEFAULT });
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| assert!(budget::register(Consumer::CondCache, Hooks { current: fill, floor: fill, cap: fill, reclaim: Some(no_trim) })));
    FILL.store(4 * GIB, Relaxed);
    let s = budget::snapshot().expect("mode on");
    assert_eq!(s.free_b, 0, "B has no free bytes");
    let held = try_region((192 * MIB) as usize);
    assert!(held.is_err(), "a big-lane draw needs B's free bytes (make_room), as before");
    let set = try_region(TINY).expect("the sampler's set: the buffer's take skips make_room");
    assert_eq!(POOL.small_held(), TINY as u64);
    drop(set);
    assert_eq!(POOL.held(), 0);
}

#[test]
fn a_tiny_draw_is_served_past_the_governors_bar() {
    let _g = armed(GIB, 128 * MIB);
    // the resident bytes past the governor's bar: the big lane's take refuses every draw
    mcx::governor::publish(MIB as usize, (2 * MIB) as usize, mcx::global_footprint::bytes());
    assert!(try_region((2 * MIB) as usize).is_err(), "past the bar the big lane refuses, as before");
    let set = try_region(TINY).expect("the buffer's take skips the bar");
    assert_eq!(POOL.small_held(), TINY as u64);
    drop(set);
    assert_eq!(POOL.held(), 0);
}

#[test]
fn a_solo_floor_is_seated_beside_a_small_statement_in_the_buffer() {
    let _g = armed(64 * MIB, 16 * MIB);
    // a small statement on another session holds 4 MiB of the buffer
    let (seated, held) = (mpsc::channel(), mpsc::channel::<()>());
    let (seated_tx, seated_rx) = seated;
    let (end_tx, end_rx) = held;
    let small = std::thread::spawn(move || {
        let a = POOL.admit_small_need(Grant::of(MIB), 4 * MIB).expect("the buffer is empty");
        seated_tx.send(()).unwrap();
        let _ = end_rx.recv_timeout(Duration::from_secs(30));
        drop(a);
    });
    seated_rx.recv().unwrap();
    assert_eq!(POOL.small_held(), 4 * MIB);
    // a floor need past the 48 MiB big lane: it runs alone once no statement holds big-lane bytes; the small one never
    // will (its bytes are the buffer's), so it is seated at once instead of waiting out its bound
    let t0 = Instant::now();
    let a = POOL.admit_need(Grant::of(GRANT_FLOOR), &|_: Grant| 96 * MIB, Duration::from_secs(3));
    let took = t0.elapsed();
    end_tx.send(()).unwrap();
    small.join().unwrap();
    let a = a.expect("alone past the pool beside the small statement");
    assert_eq!(a.how, gravity_driver::Admitted::Solo, "{a:?}");
    assert!(took < Duration::from_secs(1), "at once, not after its 3 s bound: {took:?}");
    drop(a);
    assert_eq!(POOL.held(), 0);
}
