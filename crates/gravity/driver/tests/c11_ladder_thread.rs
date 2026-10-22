//! pagepool P3 (C.11): `memledger::ladder_pass::on_ladder_thread` names the memory runtime's loop thread only (marked at
//! `memruntime::loops::idle_loop`'s start; its true there is memruntime's loops test). A reclaim a query thread runs reads
//! false: the inline R2a (`reclaim::ladder_room` -> `ladder_room_evict`, inside a ladder pass) and a draw's inline reclaim
//! (`reclaim::make_room`), each seen from inside the cache trim it reaches. One test: the budget's registry, the ladder's
//! sample and the knobs are process-wide.

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering::Relaxed};

use gravity_driver::mem::budget::{self, actuator, Cfg, Consumer, Hooks, Mode};
use gravity_driver::mem::reclaim;
use memledger::ladder_pass::{in_pass, on_ladder_thread};
use mempolicy::consts::{set, K};

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

static HELD: AtomicU64 = AtomicU64::new(0);
/// What the trim saw: bit 0 it ran, bit 1 `on_ladder_thread`, bit 2 `in_pass`.
static SEEN: AtomicU8 = AtomicU8::new(0);

fn held() -> u64 {
    HELD.load(Relaxed)
}
fn zero() -> u64 {
    0
}
fn cap() -> u64 {
    4 * GIB
}
fn trim_to(to: u64) -> u64 {
    SEEN.fetch_or(1 | (on_ladder_thread() as u8) << 1 | (in_pass() as u8) << 2, Relaxed);
    let cur = held();
    if to >= cur {
        return 0;
    }
    HELD.store(to, Relaxed);
    cur - to
}

#[test]
fn a_reclaim_on_a_query_thread_never_reads_as_the_ladder_thread() {
    assert!(budget::register(Consumer::PartCache, Hooks { current: held, floor: zero, cap, reclaim: Some(trim_to) }));
    budget::note_b(GIB);
    assert!(!on_ladder_thread(), "a test thread is no ladder thread");

    // the inline R2a: C1 on (cache_floor, the ladder on), the room under its line 512 MiB of 8 GiB, a 1 GiB draw
    set(K::CacheFloor, 1);
    mcx::ladder::publish(mcx::ladder::ON, 7 * GIB);
    mcx::ladder::set_room(512 * MIB, 8 * GIB);
    HELD.store(2 * GIB, Relaxed);
    SEEN.store(0, Relaxed);
    reclaim::ladder_room(GIB);
    mcx::ladder::publish(mcx::ladder::OFF, 0);
    set(K::CacheFloor, 0);
    assert_eq!(SEEN.load(Relaxed), 1 | 4, "ladder_room_evict trimmed inside a ladder pass, on no ladder thread");

    // a draw's inline reclaim: the reclaimer on, a draw past every free byte
    budget::set_fallback_limit(32 * GIB);
    budget::configure(Cfg { mode: Mode::On, actuators: actuator::ALL, ..Cfg::DEFAULT });
    budget::declare_managed(Consumer::PartCache, true);
    HELD.store(2 * GIB, Relaxed);
    SEEN.store(0, Relaxed);
    let free = budget::snapshot().expect("the budget is on").free_b;
    let _ = reclaim::make_room(free + 64 * MIB);
    budget::configure(Cfg::DEFAULT);
    assert_eq!(SEEN.load(Relaxed), 1, "make_room trimmed outside any pass, on no ladder thread");
}
