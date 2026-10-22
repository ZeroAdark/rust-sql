//! C4 (query_purge_bg, D3257, Broker 2026-10-16): with the general arenas owned by the purger
//! (`set_general_purge_owned`), another thread's forced collect (a statement's end, a thread's exit, the postmaster's
//! connection exit) leaves a general arena's freed range alone; the purger's `purge_general_expired` purges it only once
//! its purge delay ran out (no forced purge of unexpired ranges: the q11 guard); unowned, the same collect purges it
//! (today); and when the owner has not purged for over 1 s while an expired range waits, an ordinary collect purges it (the
//! dead-man fallback, counted per arena). Its own test binary: the owned flags and the purge delay are process-wide.

#[global_allocator]
static GLOBAL: init_small::oom::Rescue<mimalloc::MiMalloc> = init_small::oom::Rescue(mimalloc::MiMalloc);

use std::time::Duration;

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
// "purged" reads at most SIZE / 16 resident: a general arena is shared, so the collecting thread's own first blocks (its
// heap, the warm-up block) can land in the freed range (192 KiB seen)

/// A block of SIZE written and freed on this thread: its slices back in its general arena, purge pending. Its address.
fn freed_general_range() -> usize {
    let b = std::hint::black_box(vec![3u8; SIZE]);
    let p = b.as_ptr() as usize;
    assert!(allocarena::in_arenas(b.as_ptr()) && !allocarena::in_cache_arena(b.as_ptr()), "the block is on a general arena");
    drop(b);
    // this thread's pages back to the arena, no forced purge (an owned arena's expired purge is skipped too)
    // SAFETY: mimalloc's collect on this thread.
    unsafe { libmimalloc_sys::mi_collect(false) };
    assert!(resident(p, SIZE) >= SIZE * 9 / 10, "freed and pending: {} resident", resident(p, SIZE));
    p
}

/// mi_collect(true) on another thread: the forced purge of every arena, as a statement's end or a connection's exit runs it.
fn forced_collect_elsewhere() {
    std::thread::spawn(|| {
        // a block first: mimalloc's collect on a thread whose heap never allocated does nothing
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
fn owned_general_arenas_are_purged_only_by_the_purgers_expired_sweep_and_the_dead_man_fallback() {
    assert!(allocarena::general_arenas() >= 1, "the general arenas are installed");
    // unowned (today): another thread's forced collect purges the general arena's freed range
    let p = freed_general_range();
    forced_collect_elsewhere();
    assert!(resident(p, SIZE) <= SIZE / 16, "unowned: the forced collect purged it: {} resident", resident(p, SIZE));
    // owned: the same collect leaves it; the sweep leaves it while unexpired, then purges it once the delay ran out
    allocarena::set_general_purge_owned(true);
    let p = freed_general_range();
    forced_collect_elsewhere();
    assert!(resident(p, SIZE) >= SIZE * 9 / 10, "owned: the forced collect skipped it: {} resident", resident(p, SIZE));
    let _ = allocarena::purge_general_expired();
    assert!(resident(p, SIZE) >= SIZE * 9 / 10, "unexpired: the sweep left it (no forced purge): {} resident", resident(p, SIZE));
    std::thread::sleep(Duration::from_millis(300));
    assert!(allocarena::purge_general_expired() >= 1, "the sweep purged an arena");
    assert!(resident(p, SIZE) <= SIZE / 16, "expired: the sweep purged it: {} resident", resident(p, SIZE));
    assert_eq!(fallbacks(), 0, "no fallback while the owner sweeps");
    // dead man: the owner silent for over 1 s and the range's 200 ms delay past: an ordinary collect purges it, counted
    let p = freed_general_range();
    std::thread::sleep(Duration::from_millis(1300));
    forced_collect_elsewhere();
    assert!(resident(p, SIZE) <= SIZE / 16, "the dead-man fallback purged it: {} resident", resident(p, SIZE));
    assert!(fallbacks() >= 1, "the fallback purge counted on its arena: {}", fallbacks());
    allocarena::set_general_purge_owned(false);
}
