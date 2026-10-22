//! H3 step (2) (`give_back_service`): the arenas' purges as the give-back service's jobs, through the real global
//! allocator wrapper and the hooks the server binary installs (`mcx::set_allocator_release(allocarena::release)`, the
//! cache give-back, the cancel line read). With the knob on, the service (pg:memrelease, started by the knob) owns every
//! pgrust arena's purge. Each routed entry point then leaves a freed range alone on its calling thread (0 decommits
//! there, the thread's own count) and the service gives it back:
//! - mcx::give_back_caches (A1a);
//! - mcx::release_retained (the idle timer A1b, a statement's end A1d);
//! - mcx::passivate_trim (A1c);
//! - mcx::purge_after_ask (the watchdog's second purge, A1f);
//! - mcx::release_retained_at_exit (a session's exit / park / retire, the init-file load, the postmaster after the join;
//!   A1e);
//! - a page free past the purge delay (mimalloc's expired purge on the freeing thread, A1(a)).
//! The broker's C2: past the watchdog's cancel line the release purges on its caller as today; under it, the service does.
//! Its own test binary: the knob, the service and the owner slots are process-wide.
#![cfg(target_os = "linux")]

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

/// A block of SIZE written and freed on this thread: its slices back in its general arena, purge pending. Its address.
fn freed_general_range() -> usize {
    let b = std::hint::black_box(vec![3u8; SIZE]);
    let p = b.as_ptr() as usize;
    assert!(allocarena::in_arenas(b.as_ptr()) && !allocarena::in_cache_arena(b.as_ptr()), "the block is on a general arena");
    drop(b);
    // SAFETY: mimalloc's collect on this thread (its pages back to the arena; no forced purge).
    unsafe { libmimalloc_sys::mi_collect(false) };
    assert!(resident(p, SIZE) >= SIZE * 9 / 10, "freed and pending: {} resident", resident(p, SIZE));
    p
}

fn gone(p: usize) -> bool {
    resident(p, SIZE) <= SIZE / 16
}

/// The server's hooks and the knob, then the service (once).
fn setup() {
    static DONE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    DONE.get_or_init(|| {
        init_small::oom::set_cache_alloc(allocarena::cache_alloc, allocarena::cache_realloc);
        init_small::oom::set_own_pages(allocarena::own::range(), allocarena::own_free, allocarena::own_resize);
        assert!(allocarena::start_cache_heap(), "the cache heap");
        mcx::set_allocator_release(allocarena::release);
        mcx::set_cache_give_back(|| {
            let _ = allocarena::give_back();
        });
        allocarena::set_cancel_line_probe(mcx::ladder::past_cancel_line);
        mempolicy::consts::set(mempolicy::consts::K::GiveBackService, 1);
        assert!(memruntime::release::start(), "the knob starts the service");
        assert_eq!(allocarena::purge_owner_counts().1, 0, "no take refused (no purger here)");
    });
}

/// `call` on a fresh thread right after it freed a range: (the range, the thread's own decommits in `call`).
fn on_a_thread(call: fn()) -> (usize, u64) {
    std::thread::spawn(move || {
        let p = freed_general_range();
        let (d0, _) = allocarena::thread_decommits();
        call();
        let (d1, _) = allocarena::thread_decommits();
        (p, d1 - d0)
    })
    .join()
    .unwrap()
}

/// Whether `done` holds within 2 s.
fn within_2s(done: impl Fn() -> bool) -> bool {
    let t0 = Instant::now();
    while !done() {
        if t0.elapsed() > Duration::from_secs(2) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    true
}

/// A routed entry point: 0 decommits on the calling thread, and the service gives the range back (its own decommits,
/// published after its pass).
fn routed(name: &str, call: fn()) {
    let s0 = memruntime::service::counts().purged_calls;
    let (p, here) = on_a_thread(call);
    assert_eq!(here, 0, "{name}: the calling thread made {here} decommits; the service owns {} arenas' purge", allocarena::purge_owner_counts().0);
    assert!(within_2s(|| gone(p)), "{name}: the service gave the range back: {} resident", resident(p, SIZE));
    assert!(within_2s(|| memruntime::service::counts().purged_calls > s0), "{name}: the service thread made the decommits");
}

#[test]
fn h3_each_routed_release_leaves_its_decommits_to_the_service_and_past_the_cancel_line_runs_inline() {
    setup();
    routed("give_back_caches (A1a)", mcx::give_back_caches);
    routed("release_retained (A1b, A1d)", || {
        let _ = mcx::release_retained();
    });
    routed("passivate_trim (A1c)", || {
        let _ = mcx::passivate_trim();
    });
    routed("purge_after_ask (A1f)", || {
        // a give-back arms the watchdog's second purge; two samples later it releases again
        mcx::give_back_caches();
        let _ = mcx::purge_after_ask();
        let _ = mcx::purge_after_ask();
    });
    routed("release_retained_at_exit (A1e)", || {
        let _ = mcx::release_retained_at_exit();
    });
    routed("a page free past the purge delay (A1(a))", || {
        std::thread::sleep(Duration::from_millis(300));
        let b = std::hint::black_box(vec![5u8; SIZE]);
        drop(b);
        // SAFETY: mimalloc's collect on this thread (expired purges only).
        unsafe { libmimalloc_sys::mi_collect(false) };
    });

    // C2: past the watchdog's cancel line the release purges on its caller, as today
    mcx::ladder::publish_cancel_line(1000);
    mcx::ladder::publish_read(2000);
    assert!(mcx::ladder::past_cancel_line());
    let inline0 = allocarena::purge_owner_counts().3;
    let (p, here) = on_a_thread(|| {
        let _ = mcx::release_retained();
    });
    assert!(here > 0 && gone(p), "C2: past the cancel line the caller made the decommit ({here}), as today");
    assert_eq!(allocarena::purge_owner_counts().3, inline0 + 1, "counted as a forced purge run past the cancel line");
    // past the line with mimalloc's purge guard held (the service or another thread purging just then): the caller's
    // forced purge purges nothing, is counted, and the service's next pass gives the range back
    let c0 = memruntime::service::counts();
    let (busy0, s0, f0) = (allocarena::forced_guard_busy(), c0.purged_calls, c0.forced_passes);
    allocarena::test_guard_busy_next_inline();
    let (p, here) = on_a_thread(|| {
        let _ = mcx::release_retained();
    });
    assert_eq!((here, allocarena::forced_guard_busy() - busy0), (0, 1), "the guard was held: nothing purged on the caller, counted once");
    assert!(within_2s(|| memruntime::service::counts().forced_passes > f0), "the refused forced purge went to the service (a forced pass)");
    assert!(within_2s(|| gone(p)), "the service's forced pass gave the range back: {} resident", resident(p, SIZE));
    assert!(within_2s(|| memruntime::service::counts().purged_calls > s0), "on the service's thread");
    let c = memruntime::service::counts();
    assert!(c.forced_handoffs > c0.forced_handoffs && c.forced_handoff_max_us > 0, "C2's measure: the hand-off timed to its forced pass ({} us)", c.forced_handoff_max_us);
    // under it, the service
    mcx::ladder::publish_read(500);
    assert!(!mcx::ladder::past_cancel_line());
    routed("release_retained under the cancel line (C2)", || {
        let _ = mcx::release_retained();
    });
    mcx::ladder::publish_cancel_line(0);
    mcx::ladder::publish_read(0);
    assert_eq!(allocarena::purge_owner_counts().1, 0, "no take refused: the service is the arenas' one owner");
}
