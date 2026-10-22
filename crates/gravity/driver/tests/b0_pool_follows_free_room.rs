//! B0: under the broker on with the work pool's actuator, the pool's capacity follows free room at every watchdog tick
//! (`budget::pool_now`): a cache's pinned bytes stay out of it and come back the tick after they unpin; under advise the
//! tick computes nothing. The tick runs on the memory watchdog's thread: it allocates nothing.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use gravity_driver::mem::budget::{self, actuator, Cfg, Consumer, Hooks, Mode, CONSUMERS};

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

/// Counts the allocations a thread makes while its COUNTING is set.
struct Counting;

// SAFETY: a pass-through to System plus a thread-local counter.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCS.with(|a| a.set(a.get() + 1));
        }
        // SAFETY: forwarded verbatim.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded verbatim.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const GIB: u64 = 1 << 30;
static PART: AtomicU64 = AtomicU64::new(0);

fn part() -> u64 {
    PART.load(Relaxed)
}

fn one_gib() -> u64 {
    GIB
}

fn eight_gib() -> u64 {
    8 * GIB
}

fn trim(_: u64) -> u64 {
    0
}

#[test]
fn b0_the_pool_follows_free_room_each_tick_and_never_counts_pinned_cache_bytes_free() {
    budget::set_fallback_limit(32 * GIB);
    assert!(budget::register(Consumer::PartCache, Hooks { current: part, floor: one_gib, cap: eight_gib, reclaim: Some(trim) }));
    let mut pinned = [0u64; CONSUMERS];
    budget::configure(Cfg { mode: Mode::Advise, ..Cfg::DEFAULT });
    assert_eq!(budget::pool_now(0, &pinned), None, "advise: the tick computes nothing");
    budget::configure(Cfg { mode: Mode::On, actuators: actuator::ALL, ..Cfg::DEFAULT });
    budget::declare_managed(Consumer::PartCache, true);
    PART.store(6 * GIB, Relaxed);
    COUNTING.with(|c| c.set(true));
    let free = budget::pool_now(0, &pinned).expect("on");
    COUNTING.with(|c| c.set(false));
    assert_eq!(ALLOCS.with(Cell::get), 0, "the tick allocated");
    pinned[Consumer::PartCache as usize] = 3 * GIB;
    let scanning = budget::pool_now(0, &pinned).expect("on");
    assert_eq!(free - scanning, 2 * GIB, "3 GiB pinned under a 1 GiB floor: 2 GiB more stay out of the pool");
    pinned[Consumer::PartCache as usize] = 0;
    assert_eq!(budget::pool_now(0, &pinned), Some(free), "the scan ends: the pool has them back at the next tick");
    PART.store(GIB / 2, Relaxed);
    assert_eq!(budget::pool_now(0, &pinned), Some(free + GIB / 2), "a cache under its floor keeps only what it holds");
    budget::configure(Cfg::DEFAULT);
}
