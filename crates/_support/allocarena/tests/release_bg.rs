//! D4692 (ladder L4, `cache_release_bg`), through the real global allocator wrapper as the server runs it. A part-cache
//! trim frees cache images that live on their own runs; past the hot pool each run's pages used to go back to the kernel
//! (MADV_DONTNEED) on the trimming thread: a statement's or an executor's, C1's inline take inside a LadderPass on the
//! asker's thread included. With a releaser registered, a thread not marked to release inline gives nothing back: the
//! runs wait on the landing stack, their bytes on the named REAPING row (memory accounting's condition 1), a reuse takes
//! one first and its claim lands once, on the claimer (condition 2), and the releaser gives the rest back with REAPING's
//! minus and nothing on its own account (condition 2). A marked thread still gives its runs back itself; a cache
//! give-back's flush of the hot pool on a thread that defers (C1's inline take, `mcx::give_back_caches` on the asker's
//! thread) hands the hot runs to the releaser too.

#[global_allocator]
static GLOBAL: init_small::oom::Rescue<mimalloc::MiMalloc> = init_small::oom::Rescue(mimalloc::MiMalloc);

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

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
/// The images freed past the hot pool, and of those the ones a later take reuses.
const LANDED: usize = 8;
const REUSED: usize = 2;
/// Bytes a thread's own bookkeeping may move beside the images (a vector's header block): far under one run.
const SLACK: i64 = 64 << 10;

static WAKES: AtomicU64 = AtomicU64::new(0);

fn wake() {
    WAKES.fetch_add(1, Relaxed);
}

/// A part-cache image's build: its block on the cache road (a run of its own at this size).
fn image(fill: u8) -> Vec<u8> {
    let _scope = memledger::arena::cache_scope_for(memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::PartCache)), memledger::Kind::Region);
    vec![fill; RUN]
}

fn account(a: Acct) -> i64 {
    memledger::account_row(a).account_bytes
}

/// Resident bytes of `[p, p + len)` (mincore).
fn resident(p: usize, len: usize) -> usize {
    let page = 4096usize;
    let mut v = vec![0u8; len.div_ceil(page)];
    // SAFETY: mincore over a page-aligned range of this process (a run is slice-aligned) into a vector of the right length.
    let rc = unsafe { libc::mincore(p as *mut libc::c_void, len, v.as_mut_ptr()) };
    assert_eq!(rc, 0, "mincore");
    v.iter().filter(|b| **b & 1 != 0).count() * page
}

#[test]
fn d4692_a_trim_off_the_releaser_gives_no_page_back_on_its_thread_and_the_releaser_does() {
    init_small::oom::set_cache_alloc(allocarena::cache_alloc, allocarena::cache_realloc);
    init_small::oom::set_own_pages(allocarena::own::range(), allocarena::own_free, allocarena::own_resize);
    assert!(allocarena::start_cache_heap(), "the cache heap");
    let _ = allocarena::own::flush();
    allocarena::own::set_release_wake(wake);
    let reaping0 = account(Acct::REAPING);
    // the part cache's images: the hot pool's worth and LANDED past it, then two a marked thread frees
    let images: Vec<Vec<u8>> = (0..HOT_RUNS + LANDED).map(|i| image(i as u8 | 1)).collect();
    let marked: Vec<Vec<u8>> = (0..2).map(|_| image(0x5a)).collect();
    assert!(images.iter().chain(&marked).all(|v| allocarena::own::owns(v.as_ptr())), "every image is an own run");
    let landed: Vec<usize> = images[HOT_RUNS..].iter().map(|v| v.as_ptr() as usize).collect();
    let addrs: Vec<usize> = images.iter().map(|v| v.as_ptr() as usize).collect();

    // the trim on a statement's thread, inside a LadderPass (C1's inline take): not marked
    let (gives, freed_here) = std::thread::spawn(move || {
        memledger::bind_backend(3, 4003);
        let _pass = memledger::ladder_pass::LadderPass::enter();
        memledger::fold();
        let (g0, a0) = (allocarena::own::thread_gives(), account(Acct::backend(3)));
        drop(images);
        memledger::fold();
        let r = (allocarena::own::thread_gives() - g0, account(Acct::backend(3)) - a0);
        memledger::unbind_backend();
        r
    })
    .join()
    .unwrap();
    assert_eq!(gives, 0, "the trimming thread gave no page back (it gave {gives} runs back inline)");
    assert_eq!(allocarena::own::landing_bytes(), LANDED * RUN, "the runs past the hot pool wait for the releaser");
    assert_eq!(account(Acct::REAPING) - reaping0, (LANDED * RUN) as i64, "condition 1: their bytes are on the REAPING row");
    let all = ((HOT_RUNS + LANDED) * RUN) as i64;
    assert!(freed_here <= -all && freed_here > -all - SLACK, "the trimming thread's account lost its frees and kept no parked byte: {freed_here}");
    assert_eq!(WAKES.load(Relaxed), 1, "one wake: the landing count left 0 once");
    assert!(landed.iter().all(|&p| resident(p, RUN) == RUN), "a waiting run's pages are still present");

    // a thread marked to release inline (the releaser, the idle loop) gives its runs back itself: the hot pool is full
    let g = std::thread::spawn(move || {
        allocarena::own::mark_inline_release_ok();
        let g0 = allocarena::own::thread_gives();
        drop(marked);
        allocarena::own::thread_gives() - g0
    })
    .join()
    .unwrap();
    assert_eq!(g, 2, "a marked thread gives back inline");
    assert_eq!(allocarena::own::landing_bytes(), LANDED * RUN, "and leaves nothing waiting");

    // a cache give-back on a thread that defers (C1's inline take on the asker's thread): the hot pool goes to the releaser
    let reaping1 = account(Acct::REAPING);
    let (gives, n, own) = std::thread::spawn(|| {
        memledger::bind_backend(6, 4006);
        memledger::fold();
        let (g0, a0) = (allocarena::own::thread_gives(), account(Acct::backend(6)));
        let n = allocarena::own::flush();
        memledger::fold();
        let r = (allocarena::own::thread_gives() - g0, n, account(Acct::backend(6)) - a0);
        memledger::unbind_backend();
        r
    })
    .join()
    .unwrap();
    assert_eq!((gives, n, own), (0, HOT_RUNS * RUN, 0), "the flush gave nothing back on its thread and posted nothing there");
    assert_eq!(allocarena::own::landing_bytes(), (HOT_RUNS + LANDED) * RUN, "the hot runs wait for the releaser too");
    assert_eq!(account(Acct::REAPING) - reaping1, (HOT_RUNS * RUN) as i64, "condition 1: on the REAPING row");
    assert_eq!(WAKES.load(Relaxed), 1, "the count was past 0: no second wake");

    // a reuse: the hot pool empty, the next takes of the length find the waiting runs first
    let (claimed, reaping_moved, got, zeroed) = std::thread::spawn(|| {
        memledger::bind_backend(4, 4004);
        memledger::fold();
        let (a0, r0) = (account(Acct::backend(4)), account(Acct::REAPING));
        let got: [Vec<u8>; REUSED] = core::array::from_fn(|_| {
            let _scope = memledger::arena::cache_scope_for(memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::PartCache)), memledger::Kind::Region);
            vec![0u8; RUN]
        });
        memledger::fold();
        let r = (account(Acct::backend(4)) - a0, account(Acct::REAPING) - r0, got.each_ref().map(|v| v.as_ptr() as usize), got.iter().all(|v| v.iter().all(|&b| b == 0)));
        drop(got);
        memledger::unbind_backend();
        r
    })
    .join()
    .unwrap();
    assert!(got.iter().all(|p| addrs.contains(p)), "the takes reused waiting runs");
    assert!(zeroed, "a zeroed take of a waiting run reads zero");
    let want = (REUSED * RUN) as i64;
    assert!(claimed >= want && claimed < want + SLACK, "condition 2: the claim is on the claimer once: {claimed}");
    assert_eq!(reaping_moved, -want, "condition 2: and off the REAPING row");
    assert_eq!(allocarena::own::landing_bytes(), (HOT_RUNS + LANDED - REUSED) * RUN);

    // the releaser: marked, it gives the rest back, the minus on REAPING and nothing on its own account
    let (n, gives, own, reaping_moved) = std::thread::spawn(|| {
        memledger::bind_backend(5, 4005);
        allocarena::own::mark_inline_release_ok();
        memledger::fold();
        let (g0, a0, r0) = (allocarena::own::thread_gives(), account(Acct::backend(5)), account(Acct::REAPING));
        // the releaser's batches: a cap of one run gives back one run and stops; a cap one byte past it takes a second
        // run whole (a run is never split) and stops there
        let first = allocarena::own::drain_landing_upto(RUN);
        assert_eq!((first, allocarena::own::thread_gives() - g0), (RUN, 1), "a batch stops once its cap is reached");
        let second = allocarena::own::drain_landing_upto(RUN + 1);
        assert_eq!((second, allocarena::own::thread_gives() - g0), (2 * RUN, 3), "the last run of a batch is never split");
        let n = first + second + allocarena::own::drain_landing();
        memledger::fold();
        let r = (n, allocarena::own::thread_gives() - g0, account(Acct::backend(5)) - a0, account(Acct::REAPING) - r0);
        memledger::unbind_backend();
        r
    })
    .join()
    .unwrap();
    let rest = HOT_RUNS + LANDED - REUSED;
    assert_eq!((n, gives), (rest * RUN, rest as u64), "the releaser gave every waiting run back");
    assert_eq!(own, 0, "condition 2: nothing on the releaser's own account");
    assert_eq!(reaping_moved, -((rest * RUN) as i64), "condition 2: the minus is on the REAPING row");
    assert_eq!((allocarena::own::landing_bytes(), account(Acct::REAPING)), (0, reaping0), "nothing waits, REAPING is back");
    assert!(addrs.iter().filter(|p| !got.contains(p)).all(|&p| resident(p, RUN) == 0), "the given-back runs' pages are gone");
}
