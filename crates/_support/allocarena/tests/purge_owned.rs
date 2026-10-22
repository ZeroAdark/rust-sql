//! C3 purge owned: with the cache arena owned by its purger (`set_cache_purge_owned`), a
//! forced collect on another thread (mi_collect(true): a session's statement end, the postmaster's connection exit) leaves
//! the cache arena's freed range alone, and the purger's `purge_cache_now` then gives it back by the CVE-2026-74674
//! decommit; unowned, the same collect purges it (today's behavior); and when the owner has not purged for over 1 s while
//! an expired range waits, an ordinary collect purges it (the dead-man fallback, counted). Its own test binary: the
//! owned flag and the purge delay are process-wide.

#[global_allocator]
static GLOBAL: init_small::oom::Rescue<mimalloc::MiMalloc> = init_small::oom::Rescue(mimalloc::MiMalloc);

use std::sync::OnceLock;
use std::time::Duration;

// The arenas before mimalloc's constructor, as the server's; a 200 ms purge delay (v3 mi_option_purge_delay = 15), so a
// freed range is pending at first and expired after the dead-man wait.
extern "C" fn before_mimalloc() {
    // SAFETY: a plain option store, before any thread exists.
    unsafe { libmimalloc_sys::mi_option_set(15, 200) };
    let _ = allocarena::install();
}
#[used]
#[unsafe(link_section = ".init_array.00100")]
static BEFORE_MIMALLOC: extern "C" fn() = before_mimalloc;

fn setup() {
    static DONE: OnceLock<bool> = OnceLock::new();
    DONE.get_or_init(|| {
        init_small::oom::set_cache_alloc(allocarena::cache_alloc, allocarena::cache_realloc);
        init_small::oom::set_own_pages(allocarena::own::range(), allocarena::own_free, allocarena::own_resize);
        allocarena::start_cache_heap()
    });
}

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

/// A cache image of SIZE written, freed, and its pages handed back to the cache arena (this thread's collect, no purge).
/// Its address.
fn freed_cache_range() -> usize {
    let img = {
        let _scope = memledger::arena::cache_scope_for(memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::PartCache)), memledger::Kind::Region);
        vec![3u8; SIZE]
    };
    let p = img.as_ptr() as usize;
    assert!(allocarena::in_cache_arena(img.as_ptr()), "the image is on the cache arena");
    drop(img);
    allocarena::collect_cache_local();
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

#[test]
fn an_owned_cache_arena_is_purged_only_by_its_purger_and_by_the_dead_man_fallback() {
    setup();
    // unowned (today): another thread's forced collect purges the cache arena's freed range
    let p = freed_cache_range();
    forced_collect_elsewhere();
    assert!(resident(p, SIZE) <= 64 << 10, "unowned: the forced collect purged it: {} resident", resident(p, SIZE));
    // owned: the same collect leaves it, the purger's call gives it back by the CVE decommit
    allocarena::set_cache_purge_owned(true);
    let p = freed_cache_range();
    forced_collect_elsewhere();
    assert!(resident(p, SIZE) >= SIZE * 9 / 10, "owned: the forced collect skipped it: {} resident", resident(p, SIZE));
    let (_, remaps0, whole0) = allocarena::decommit_counts();
    assert!(allocarena::purge_cache_now(true), "the purger's purge found the range");
    assert!(resident(p, SIZE) <= 64 << 10, "the purger gave it back: {} resident", resident(p, SIZE));
    let (_, remaps1, whole1) = allocarena::decommit_counts();
    assert!(remaps1 > remaps0, "a whole 2 MiB-aligned range was mapped over afresh (MAP_FIXED)");
    assert_eq!((whole0, whole1), (0, 0), "decommit_never_dontneeds_a_whole_2mib_range on the purger's path");
    assert_eq!(allocarena::purge_owned_fallbacks(), 0, "no fallback while the purger is alive");
    // dead man: the owner silent for over 1 s and the range's 200 ms delay past: an ordinary collect purges it, counted
    let p = freed_cache_range();
    std::thread::sleep(Duration::from_millis(1300));
    forced_collect_elsewhere();
    assert!(resident(p, SIZE) <= 64 << 10, "the dead-man fallback purged it: {} resident", resident(p, SIZE));
    assert!(allocarena::purge_owned_fallbacks() >= 1, "the fallback purge counted: {}", allocarena::purge_owned_fallbacks());
    allocarena::set_cache_purge_owned(false);
}
