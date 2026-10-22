//! H3 step (1) (`give_back_service`), through the real global allocator wrapper as the server runs it: the give-back
//! service's admission at the own runs' enqueue (`allocarena::own::set_service`) and its accounting terms.
//! - The broker's condition 2. The backlog cap: a run that would take the landing past it is given back on its freeing
//!   thread, counted. C3's dead-man rule: a run that finds runs waiting while the service made no pass for its stretch
//!   is given back the same way, counted. Neither is silent.
//! - Memory accounting's A1: REAPING == LANDING_BYTES at each batch boundary of the service's paced drain.
//! - Memory accounting's A5: a drain of the landing on a drawing thread (make_room past the governor's bar) leaves that
//!   thread's account unchanged and is counted.
//!
//! Its own test binary: the release wake and the service are process-wide.

#[global_allocator]
static GLOBAL: init_small::oom::Rescue<mimalloc::MiMalloc> = init_small::oom::Rescue(mimalloc::MiMalloc);

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use memledger::Acct;

// As the server does: the arenas go in before mimalloc's own constructor and before the first allocation.
extern "C" fn before_mimalloc() {
    // SAFETY: a plain option store (v3 mi_option_purge_delay = 15), before any thread exists.
    unsafe { libmimalloc_sys::mi_option_set(15, 0) };
    let _ = allocarena::install();
}
#[used]
#[unsafe(link_section = ".init_array.00100")]
static BEFORE_MIMALLOC: extern "C" fn() = before_mimalloc;

/// The own range's largest run (8 slices): one image each.
const RUN: usize = 512 << 10;
/// The hot pool's runs of that length at its floor cap (no memory budget in a test: `memledger::b_at` is 0).
const HOT_RUNS: usize = allocarena::own::HOT_CAP / RUN;
/// The service's backlog cap here: one hot pool (the server's is `memruntime::service::BACKLOG_HOT_CAPS`).
const CAP: usize = allocarena::own::HOT_CAP;
/// The service's paced batch (memruntime's release.rs BATCH_BYTES).
const BATCH: usize = 4 << 20;

/// The service's liveness as the test sets it.
static STALLED: AtomicBool = AtomicBool::new(false);

fn stalled() -> bool {
    STALLED.load(Relaxed)
}

fn wake() {}

/// A part-cache image's build: its block on the cache road (a run of its own at this size).
fn image(fill: u8) -> Vec<u8> {
    let _scope = memledger::arena::cache_scope_for(memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::PartCache)), memledger::Kind::Region);
    vec![fill; RUN]
}

fn account(a: Acct) -> i64 {
    memledger::account_row(a).account_bytes
}

/// `f` on a fresh thread bound to backend `slot`, marked inline-ok (the service's thread) or not (a statement's).
fn on_thread<R: Send + 'static>(slot: u32, marked: bool, f: impl FnOnce() -> R + Send + 'static) -> R {
    std::thread::spawn(move || {
        memledger::bind_backend(slot, 5000 + slot);
        if marked {
            allocarena::own::mark_inline_release_ok();
        }
        memledger::fold();
        let r = f();
        memledger::fold();
        memledger::unbind_backend();
        r
    })
    .join()
    .unwrap()
}

/// The fallbacks since `f0`: `[at the cap: runs, bytes, dead-man: runs, bytes]`.
fn fallbacks_since(f0: [u64; 4]) -> [u64; 4] {
    let f = allocarena::own::service_fallbacks();
    core::array::from_fn(|i| f[i] - f0[i])
}

#[test]
fn h3_the_service_turns_runs_away_at_its_cap_and_when_stalled_counted_and_reaping_is_the_landing_at_each_batch() {
    init_small::oom::set_cache_alloc(allocarena::cache_alloc, allocarena::cache_realloc);
    init_small::oom::set_own_pages(allocarena::own::range(), allocarena::own_free, allocarena::own_resize);
    assert!(allocarena::start_cache_heap(), "the cache heap");
    let _ = allocarena::own::flush();
    allocarena::own::set_service(stalled, 1);
    allocarena::own::set_release_wake(wake);
    let reaping0 = account(Acct::REAPING);
    let f0 = allocarena::own::service_fallbacks();
    let d0 = allocarena::own::drawer_drains();

    // 1. the cap: a trim frees the hot pool's worth and two past it (those two wait), then a cache give-back's flush on
    // the same thread (C1's inline take) hands the hot runs over: the backlog fills to its cap, the last two runs are
    // turned away and given back on the flushing thread
    let images: Vec<Vec<u8>> = (0..HOT_RUNS + 2).map(|i| image(i as u8 | 1)).collect();
    assert!(images.iter().all(|v| allocarena::own::owns(v.as_ptr())), "every image is an own run");
    let (freed_gives, flush_gives, flushed) = on_thread(3, false, move || {
        let g0 = allocarena::own::thread_gives();
        drop(images);
        let g1 = allocarena::own::thread_gives();
        let n = allocarena::own::flush();
        (g1 - g0, allocarena::own::thread_gives() - g1, n)
    });
    assert_eq!((freed_gives, flushed), (0, HOT_RUNS * RUN), "the trim gave nothing back on its thread; the flush took the hot pool");
    assert_eq!(allocarena::own::landing_bytes(), CAP, "the backlog never passes its cap");
    assert_eq!(flush_gives, 2, "the two runs past the cap were given back on the flushing thread");
    assert_eq!(fallbacks_since(f0), [2, 2 * RUN as u64, 0, 0], "and counted at the cap");
    assert_eq!(allocarena::own::service_cap(), CAP);
    assert_eq!(account(Acct::REAPING) - reaping0, CAP as i64, "REAPING is the landing");

    // the service's drain (marked): nothing left, no drawer drain counted
    let n = on_thread(4, true, allocarena::own::drain_landing);
    assert_eq!((n, allocarena::own::landing_bytes()), (CAP, 0));
    assert_eq!(allocarena::own::drawer_drains(), d0, "the service's own drains are no drawer drains");

    // 2. the dead-man rule: a run waits, the service stalls, the next run past the hot pool is given back on its thread
    let images: Vec<Vec<u8>> = (0..HOT_RUNS + 1).map(|i| image(i as u8 | 1)).collect();
    let last = image(0x5a);
    let gives = on_thread(5, false, move || {
        drop(images);
        STALLED.store(true, Relaxed);
        let g0 = allocarena::own::thread_gives();
        drop(last);
        allocarena::own::thread_gives() - g0
    });
    STALLED.store(false, Relaxed);
    assert_eq!(allocarena::own::landing_bytes(), RUN, "the run before the stall waits");
    assert_eq!(gives, 1, "the run that met the stalled service was given back on its thread");
    assert_eq!(fallbacks_since(f0), [2, 2 * RUN as u64, 1, RUN as u64], "and counted past the dead-man stretch");

    // 3. A1: the hot pool handed over (exactly the cap, nothing turned away), then the service's paced batches: REAPING
    // equals the landing at each batch boundary
    let n = on_thread(4, true, allocarena::own::drain_landing);
    assert_eq!(n, RUN);
    let flushed = on_thread(6, false, allocarena::own::flush);
    assert_eq!((flushed, allocarena::own::landing_bytes()), (HOT_RUNS * RUN, CAP));
    assert_eq!(fallbacks_since(f0), [2, 2 * RUN as u64, 1, RUN as u64], "a backlog at its cap turns nothing away");
    let left = 2 * BATCH;
    let boundaries = on_thread(4, true, move || {
        let mut seen = Vec::new();
        while allocarena::own::landing_bytes() > left {
            let n = allocarena::own::drain_landing_upto(BATCH);
            memledger::fold();
            seen.push((n, allocarena::own::landing_bytes() as i64, account(Acct::REAPING)));
        }
        seen
    });
    assert_eq!(boundaries.len(), (CAP - left) / BATCH);
    for (n, landing, reaping) in &boundaries {
        assert_eq!(*n, BATCH, "a batch is the service's cap");
        assert_eq!(reaping - reaping0, *landing, "A1: REAPING == LANDING_BYTES at the batch boundary: {boundaries:?}");
    }
    assert_eq!(allocarena::own::drawer_drains(), d0);

    // 4. A5: make_room past the governor's bar drains the rest on the drawing thread: its account unchanged, counted
    let (n, own) = on_thread(7, false, || {
        let a0 = account(Acct::backend(7));
        let n = allocarena::own::drain_landing();
        memledger::fold();
        (n, account(Acct::backend(7)) - a0)
    });
    assert_eq!((n, own), (left, 0), "A5: the drawing thread's account is unchanged across its drain");
    let d = allocarena::own::drawer_drains();
    assert_eq!((d.0 - d0.0, d.1 - d0.1), (1, left as u64), "the drawer drain is counted");
    assert_eq!((allocarena::own::landing_bytes(), account(Acct::REAPING)), (0, reaping0), "nothing waits, REAPING is back");
}
