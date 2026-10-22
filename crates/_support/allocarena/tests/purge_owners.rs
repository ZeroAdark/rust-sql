//! H3 step (2) (`give_back_service`): one purge owner per arena, by construction (memory accounting's condition), and
//! the ladder's pressure purge untouched (the broker's C1). Through the real global allocator wrapper.
//! - C1, knob 0 with purge (a) on: pg:mempurge owns the general arenas, and the ladder's pressure purge (`purge_unused`
//!   from a plain thread) still decommits their freed ranges, its bytes over owned arenas counted apart (purged_by_pressure,
//!   M3).
//! - Both on, pg:mempurge first (memruntime starts it first): the service's take of each arena is refused and counted, its
//!   purge of them purges nothing, and pg:mempurge's sweep still does.
//! - The other way round, the service first: pg:mempurge's take is refused and counted, its entry points skip the
//!   service's arenas, the service's purge gives the range back, and the ladder's pressure purge stays owner-agnostic.
//!
//! Its own test binary: the owner slots, the owned flags and the purge delay are process-wide.

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

/// A block of SIZE written and freed on this thread: its slices back in its general arena, purge pending. Its address.
fn freed_general_range() -> usize {
    let b = std::hint::black_box(vec![3u8; SIZE]);
    let p = b.as_ptr() as usize;
    assert!(allocarena::in_arenas(b.as_ptr()) && !allocarena::in_cache_arena(b.as_ptr()), "the block is on a general arena");
    drop(b);
    // this thread's pages back to the arena, no forced purge
    // SAFETY: mimalloc's collect on this thread.
    unsafe { libmimalloc_sys::mi_collect(false) };
    assert!(resident(p, SIZE) >= SIZE * 9 / 10, "freed and pending: {} resident", resident(p, SIZE));
    p
}

fn gone(p: usize) -> bool {
    resident(p, SIZE) <= SIZE / 16
}

fn general_slots() -> std::ops::Range<usize> {
    0..allocarena::general_arenas()
}

#[test]
fn h3_one_purge_owner_per_arena_by_cas_and_the_ladders_pressure_purge_stays_owner_agnostic() {
    let g = allocarena::general_arenas();
    assert!(g >= 1, "the general arenas are installed");
    let refusals = || allocarena::purge_owner_counts().1;
    let pressure_owned = || allocarena::purge_owner_counts().4;

    // C1, knob 0 with purge (a) on: pg:mempurge owns the general arenas; the ladder's pressure purge from a plain thread
    // still gives their freed range back, counted apart (M3)
    allocarena::set_general_purge_owned(true);
    assert!(general_slots().all(|i| allocarena::purge_owner(i) == allocarena::OWNER_PURGER), "(a) took the general arenas");
    let p = freed_general_range();
    let b0 = pressure_owned();
    let n = std::thread::spawn(|| allocarena::purge_unused(false)).join().unwrap();
    assert!(n > 0 && gone(p), "C1: the ladder's pressure purge decommitted the purger-owned arena: {n} bytes, {} resident", resident(p, SIZE));
    assert!(pressure_owned() > b0, "M3: its bytes over owned arenas are counted as purged_by_pressure");

    // both on, pg:mempurge first: every take of the service's is refused and counted, the service owns nothing and purges
    // nothing; pg:mempurge's sweep purges the expired range
    allocarena::set_cache_purge_owned(true);
    let r0 = refusals();
    let (taken, refused) = allocarena::take_service_purge();
    assert_eq!((taken, refused), (0, g as u32 + 1), "the service is refused every arena (purge (a) holds the general arenas and the cache arena)");
    assert_eq!(refusals() - r0, g as u64 + 1, "each refusal counted");
    assert_eq!(allocarena::purge_owner_counts().0, 0, "the service owns no arena");
    let p = freed_general_range();
    assert!((0..allocarena::PURGE_SLOTS).all(|i| !allocarena::service_purge(i, true)) && !gone(p), "the service purges no arena it does not own");
    std::thread::sleep(Duration::from_millis(300));
    assert!(allocarena::purge_general_expired() >= 1 && gone(p), "pg:mempurge's sweep purged its own arena");

    // pg:mempurge lets go, the service takes every arena
    allocarena::set_general_purge_owned(false);
    allocarena::set_cache_purge_owned(false);
    assert!((0..allocarena::PURGE_SLOTS).all(|i| allocarena::purge_owner(i) == allocarena::NO_OWNER), "every slot free");
    let (taken, refused) = allocarena::take_service_purge();
    assert_eq!((taken, refused), (g as u32 + 1, 0), "the service took every arena");

    // the other way round: pg:mempurge's take is refused and counted, its entry points skip the service's arenas
    let r0 = refusals();
    allocarena::set_general_purge_owned(true);
    allocarena::set_cache_purge_owned(true);
    assert_eq!(refusals() - r0, g as u64 + 1, "pg:mempurge is refused every arena the service holds");
    assert!(general_slots().all(|i| allocarena::purge_owner(i) == allocarena::OWNER_SERVICE), "the slots stay the service's");
    let p = freed_general_range();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(allocarena::purge_general_expired(), 0, "pg:mempurge's sweep skips the service's arenas");
    assert!(!allocarena::purge_cache_now(true), "and its cache purge");
    assert_eq!(allocarena::purge_after_idle(true, true, 0), 0, "and its idle purge");
    let _ = allocarena::purge_unused(true);
    assert!(!gone(p), "and its pressure tick: no double purge, {} resident", resident(p, SIZE));
    assert!((0..allocarena::PURGE_SLOTS).any(|i| allocarena::service_purge(i, false)) && gone(p), "the service's purge gave the expired range back");

    // and the ladder's pressure purge stays owner-agnostic over the service's arenas (C1), counted apart (M3)
    let p = freed_general_range();
    let b0 = pressure_owned();
    let n = std::thread::spawn(|| allocarena::purge_unused(false)).join().unwrap();
    assert!(n > 0 && gone(p) && pressure_owned() > b0, "C1 / M3: the pressure purge decommitted the service's arena, counted apart");
}
