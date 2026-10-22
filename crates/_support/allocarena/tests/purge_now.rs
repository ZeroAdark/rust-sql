//! C3 (memacct2, Broker's conditions 2026-10-15): `allocarena::purge_cache_now` purges the cache arena's freed ranges at
//! once while a general arena's freed range keeps waiting out mimalloc's process-wide purge delay, and the new path's
//! decommit keeps the CVE-2026-74674 rule (a whole 2 MiB-aligned range is mapped over afresh, never MADV_DONTNEED).
//! Its own test binary: the purge delay here is long (the server's default kind), unlike cache_arena.rs's 0.

#[global_allocator]
static GLOBAL: init_small::oom::Rescue<mimalloc::MiMalloc> = init_small::oom::Rescue(mimalloc::MiMalloc);

use std::sync::OnceLock;

// The arenas go in before mimalloc's own constructor and before the first allocation, as the server's do; the purge
// delay is 60 s (v3 mi_option_purge_delay = 15, in ms), so nothing a test frees is purged by mimalloc's own schedule.
extern "C" fn before_mimalloc() {
    // SAFETY: a plain option store, before any thread exists.
    unsafe { libmimalloc_sys::mi_option_set(15, 60_000) };
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

#[test]
fn a_freed_cache_range_is_purged_now_while_a_general_one_waits_its_delay() {
    setup();
    const SIZE: usize = 16 << 20;
    let img = {
        let _scope = memledger::arena::cache_scope_for(memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::PartCache)), memledger::Kind::Region);
        vec![1u8; SIZE]
    };
    let gen = vec![2u8; SIZE];
    let (ip, gp) = (img.as_ptr() as usize, gen.as_ptr() as usize);
    assert!(allocarena::in_cache_arena(img.as_ptr()), "the image is on the cache arena");
    assert!(!allocarena::in_cache_arena(gen.as_ptr()) && allocarena::in_arenas(gen.as_ptr()), "the other block on a general arena");
    assert!(resident(ip, SIZE) >= SIZE * 9 / 10 && resident(gp, SIZE) >= SIZE * 9 / 10, "both written, both resident");
    drop(img);
    drop(gen);
    // the freed pages back to their arenas (this thread's collects), with no forced purge: inside the 60 s delay
    allocarena::collect_cache_local();
    // SAFETY: mimalloc's collect on this thread, not forced (purges only expired ranges: none).
    unsafe { libmimalloc_sys::mi_collect(false) };
    assert!(resident(ip, SIZE) >= SIZE * 9 / 10, "the freed image waits for a purge: {} resident", resident(ip, SIZE));
    let (_, cache_before) = memledger::arena::taken().expect("arenas installed");
    let (_, remaps0, whole0) = allocarena::decommit_counts();
    assert!(allocarena::purge_cache_now(true), "the cache arena's freed range was purged");
    let (_, cache_after) = memledger::arena::taken().unwrap();
    assert!(resident(ip, SIZE) <= 64 << 10, "the image's pages went back at the call: {} resident", resident(ip, SIZE));
    assert!(resident(gp, SIZE) >= SIZE * 9 / 10, "the general arena's freed block still waits out its delay: {} resident", resident(gp, SIZE));
    assert!(cache_before - cache_after >= (SIZE * 9 / 10) as u64, "CACHE_TAKEN fell: {cache_before} -> {cache_after}");
    let (_, remaps1, whole1) = allocarena::decommit_counts();
    assert!(remaps1 > remaps0, "a whole 2 MiB-aligned range was mapped over afresh (MAP_FIXED)");
    assert_eq!(whole1, whole0, "decommit_never_dontneeds_a_whole_2mib_range: no MADV_DONTNEED piece covered one");
    assert_eq!(whole1, 0);
}
