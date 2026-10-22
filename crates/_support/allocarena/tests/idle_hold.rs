//! The purger's hold (`PGRUST_PURGE_AFTER_IDLE_S` above 1): with the general arenas owned, `purge_after_idle` leaves a
//! general arena's freed range committed while slices keep being claimed from that arena (past its purge delay and past
//! the 1 s dead-man stretch: each call keeps the owner alive, so another thread's forced collect still skips it), and
//! purges it once the arena saw no claim for the idle stretch; those bytes count as the owner's (`purged_bytes`). Its own
//! test binary: the owned flags and the purge delay are process-wide.

#[global_allocator]
static GLOBAL: init_small::oom::Rescue<mimalloc::MiMalloc> = init_small::oom::Rescue(mimalloc::MiMalloc);

use std::time::{Duration, Instant};

// The arenas before mimalloc's constructor, as the server's; a 200 ms purge delay (v3 mi_option_purge_delay = 15).
extern "C" fn before_mimalloc() {
    // SAFETY: a plain option store, before any thread exists.
    unsafe { libmimalloc_sys::mi_option_set(15, 200) };
    let _ = allocarena::install();
}
#[used]
#[unsafe(link_section = ".init_array.00100")]
static BEFORE_MIMALLOC: extern "C" fn() = before_mimalloc;

/// Resident bytes of `[p, p + len)` (mincore over the pages the range touches).
fn resident(p: usize, len: usize) -> usize {
    let page = 4096usize;
    let lo = p & !(page - 1);
    let hi = (p + len).next_multiple_of(page);
    let mut v = vec![0u8; (hi - lo) / page];
    // SAFETY: mincore over a range of this process's arenas (mapped, read-write) into a vector of the right length.
    let rc = unsafe { libc::mincore(lo as *mut libc::c_void, hi - lo, v.as_mut_ptr()) };
    assert_eq!(rc, 0, "mincore");
    v.iter().filter(|b| **b & 1 != 0).count() * page
}

const SIZE: usize = 16 << 20;
const IDLE_MS: u64 = 700;

/// A block of SIZE written and freed on this thread: its slices back in its general arena, purge pending. Its address.
fn freed_general_range() -> usize {
    let b = std::hint::black_box(vec![3u8; SIZE]);
    let p = b.as_ptr() as usize;
    assert!(allocarena::in_arenas(b.as_ptr()) && !allocarena::in_cache_arena(b.as_ptr()), "the block is on a general arena");
    drop(b);
    // SAFETY: mimalloc's collect on this thread (no forced purge).
    unsafe { libmimalloc_sys::mi_collect(false) };
    assert!(resident(p, SIZE) >= SIZE * 9 / 10, "freed and pending: {} resident", resident(p, SIZE));
    p
}

/// mi_collect(true) on another thread: the forced purge of every arena, as a statement's end or a connection's exit runs it.
fn forced_collect_elsewhere() {
    std::thread::spawn(|| {
        let warm = std::hint::black_box(vec![1u8; 4096]);
        drop(warm);
        // SAFETY: mimalloc's collect on this thread.
        unsafe { libmimalloc_sys::mi_collect(true) };
    })
    .join()
    .unwrap();
}

fn fallbacks() -> u64 {
    (0..allocarena::general_arenas()).map(allocarena::general_purge_fallbacks).sum()
}

#[test]
fn the_hold_purges_an_owned_arena_only_after_an_idle_stretch() {
    assert!(allocarena::general_arenas() >= 1, "the general arenas are installed");
    allocarena::set_general_purge_owned(true);
    let owner0 = allocarena::purged_bytes().0;
    let p = freed_general_range();
    // a fresh 1 MiB block claimed from this thread's general arena every 100 ms for 1.5 s, the hold ticking between
    let mut live = Vec::new();
    for _ in 0..15 {
        live.push(std::hint::black_box(vec![5u8; 1 << 20]));
        let _ = allocarena::purge_after_idle(true, false, IDLE_MS);
        std::thread::sleep(Duration::from_millis(100));
    }
    forced_collect_elsewhere();
    assert!(resident(p, SIZE) >= SIZE * 9 / 10, "claims kept coming: the hold kept it: {} resident", resident(p, SIZE));
    assert_eq!(fallbacks(), 0, "the hold kept the owner alive: no dead-man fallback");
    drop(live);
    // SAFETY: mimalloc's collect on this thread (no forced purge).
    unsafe { libmimalloc_sys::mi_collect(false) };
    // quiet from here: purged once the arena saw no claim for IDLE_MS
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(4) && resident(p, SIZE) > SIZE / 16 {
        let _ = allocarena::purge_after_idle(true, false, IDLE_MS);
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(resident(p, SIZE) <= SIZE / 16, "idle: the hold purged it: {} resident", resident(p, SIZE));
    assert!(t.elapsed() >= Duration::from_millis(IDLE_MS / 2), "not before the idle stretch: {:?}", t.elapsed());
    assert!(allocarena::purged_bytes().0 - owner0 >= (SIZE / 2) as u64, "counted as the owner's purge");
    assert_eq!(fallbacks(), 0, "no fallback");
    allocarena::set_general_purge_owned(false);
}
