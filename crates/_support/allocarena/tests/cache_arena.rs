//! D4321 step 1 + ALLOC-LADDER (c), through the real global allocator wrapper (`init_small::oom::Rescue` over mimalloc):
//! cache images live on their own arena, so evicting them gives their pages back while unrelated blocks that were
//! allocated between them stay; mimalloc's takes are counted exactly; a take past the line wakes the ladder; an object
//! past 2 GiB comes from pgrust's arenas.

#[global_allocator]
static GLOBAL: init_small::oom::Rescue<mimalloc::MiMalloc> = init_small::oom::Rescue(mimalloc::MiMalloc);

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

static ONE: Mutex<()> = Mutex::new(());

// As the server does: the arenas go in before mimalloc's own constructor and before the first allocation, and mimalloc
// purges at once (the test reads the arenas' state without waiting out the purge delay).
extern "C" fn before_mimalloc() {
    // SAFETY: a plain option store (v3 mi_option_purge_delay = 15), before any thread exists.
    unsafe { libmimalloc_sys::mi_option_set(15, 0) };
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
fn resident(p: *const u8, len: usize) -> usize {
    let page = 4096usize;
    let lo = (p as usize) & !(page - 1);
    let hi = (p as usize + len).next_multiple_of(page);
    let mut v = vec![0u8; (hi - lo) / page];
    // SAFETY: mincore over a mapped range of this process into a vector of the right length.
    let rc = unsafe { libc::mincore(lo as *mut libc::c_void, hi - lo, v.as_mut_ptr()) };
    assert_eq!(rc, 0, "mincore");
    v.iter().filter(|b| **b & 1 != 0).count() * page
}

/// A cache scope as an image's build opens one: it names the cache that keeps the image.
fn scope() -> memledger::arena::CacheScope {
    memledger::arena::cache_scope_for(memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::PartCache)), memledger::Kind::Region)
}

#[test]
fn evicted_cache_images_give_their_pages_back_beside_live_blocks() {
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    setup();
    const N: usize = 256;
    const SIZE: usize = 48 << 10;
    // a cache image, then an unrelated block, in turn (the part cache's fill between a scan's other allocations)
    let mut images: Vec<Vec<u8>> = Vec::with_capacity(N);
    let mut others: Vec<Vec<u8>> = Vec::with_capacity(N);
    for i in 0..N {
        let img = {
            let _scope = scope();
            vec![(i as u8) | 1; SIZE]
        };
        images.push(img);
        others.push(vec![(i as u8) | 2; SIZE]);
    }
    assert!(images.iter().all(|v| allocarena::in_cache_arena(v.as_ptr())), "every image is on the cache arena");
    assert!(others.iter().all(|v| !allocarena::in_cache_arena(v.as_ptr())), "no other block is");
    let ranges: Vec<(usize, usize)> = images.iter().map(|v| (v.as_ptr() as usize, v.len())).collect();
    let before: usize = ranges.iter().map(|&(p, l)| resident(p as *const u8, l)).sum();
    let (_, cache_before) = memledger::arena::taken().expect("arenas installed");
    // evict every image (freed from another thread, as an eviction on the ladder's thread would), then purge
    std::thread::spawn(move || drop(images)).join().unwrap();
    allocarena::collect_cache();
    // SAFETY: mimalloc's collect on this thread.
    unsafe { libmimalloc_sys::mi_collect(true) };
    let after: usize = ranges.iter().map(|&(p, l)| resident(p as *const u8, l)).sum();
    let (_, cache_after) = memledger::arena::taken().unwrap();
    assert!(before >= N * SIZE * 9 / 10, "the images were resident: {before}");
    assert!(after <= 32 << 10, "the evicted images' pages went back (<= 32 KiB left), not held by the live blocks beside them: {after} of {before} bytes still resident");
    assert!(cache_before - cache_after >= (N * SIZE * 9 / 10) as u64, "CACHE_TAKEN fell by the evicted bytes: {cache_before} -> {cache_after}");
    assert!(others.iter().enumerate().all(|(i, v)| v.iter().all(|b| *b == (i as u8) | 2)), "the live blocks are intact");
}

#[test]
fn a_take_past_the_line_wakes_the_ladder_without_blocking() {
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    setup();
    static WOKE: AtomicU64 = AtomicU64::new(0);
    memledger::arena::set_wake_hook(|| {
        WOKE.fetch_add(1, Relaxed);
    });
    let _ = memledger::arena::take_ladder_due();
    let (g, c) = memledger::arena::taken().expect("arenas installed");
    memledger::arena::set_line(g + c + (16 << 20));
    let before = WOKE.load(Relaxed);
    let big: Vec<u8> = vec![7; 64 << 20];
    assert!(allocarena::in_arenas(big.as_ptr()), "a fresh 64 MiB block comes from pgrust's arenas");
    assert!(WOKE.load(Relaxed) > before, "64 MiB past a line 16 MiB away woke the ladder");
    assert!(memledger::arena::take_ladder_due());
    memledger::arena::set_line(0);
    drop(big);
}

#[test]
fn an_object_past_two_gib_comes_from_the_arenas() {
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    setup();
    let (g0, _) = memledger::arena::taken().expect("arenas installed");
    // 3 GiB of address space, one page touched: NORESERVE, nothing resident past the touch
    let mut v: Vec<u8> = Vec::with_capacity(3 << 30);
    v.push(1);
    assert!(allocarena::in_arenas(v.as_ptr()), "past mimalloc's default 2 GiB object cap, still on pgrust's arenas");
    let (g1, _) = memledger::arena::taken().unwrap();
    // the counter is process-wide: a purge of other freed pages may land between the two reads (seen: 768 KiB), so the
    // check allows 64 MiB of them; an uncounted 3 GiB object would leave the rise near 0
    assert!(g1 + (64 << 20) >= g0 + (3 << 30), "counted as taken: {g0} -> {g1}");
}

/// LADDER-DESIGN 6a (memlead's sizing: 779 of +815 MiB were images freed beside LIVE images on shared 4 MiB pages):
/// 64-512 KiB images interleaved with each other, every other one evicted from another thread, must give their pages
/// back while their neighbours stay live and intact; and a run's realloc keeps or moves the bytes.
#[test]
fn evicted_images_beside_live_images_give_their_pages_back() {
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    setup();
    const N: usize = 128;
    let sizes = [64 << 10, 100 << 10, 200 << 10, 300 << 10, 512 << 10];
    let mut images: Vec<Vec<u8>> = (0..N)
        .map(|i| {
            let _scope = scope();
            vec![(i as u8) | 1; sizes[i % sizes.len()]]
        })
        .collect();
    assert!(images.iter().all(|v| allocarena::own::owns(v.as_ptr())), "every 64-512 KiB image is on pages of its own");
    let mut evicted = Vec::new();
    let mut live = Vec::new();
    for (i, v) in images.drain(..).enumerate() {
        if i % 2 == 0 { evicted.push(v) } else { live.push(v) }
    }
    let ranges: Vec<(usize, usize)> = evicted.iter().map(|v| (v.as_ptr() as usize, v.len())).collect();
    let before: usize = ranges.iter().map(|&(p, l)| resident(p as *const u8, l)).sum();
    let (_, c0) = memledger::arena::taken().expect("arenas installed");
    std::thread::spawn(move || drop(evicted)).join().unwrap();
    // the cache's give-back (its freed runs' pages back, the hot pool's too)
    let _ = allocarena::give_back();
    let after: usize = ranges.iter().map(|&(p, l)| resident(p as *const u8, l)).sum();
    let (_, c1) = memledger::arena::taken().unwrap();
    let total: usize = ranges.iter().map(|r| r.1).sum();
    assert!(before >= total * 9 / 10, "the images were resident: {before} of {total}");
    assert!(after <= 32 << 10, "evicted images beside live ones gave their pages back: {after} of {before} bytes still resident");
    assert!(c0 - c1 >= total as u64, "CACHE_TAKEN fell by the evicted runs: {c0} -> {c1}");
    let i0 = 1usize;
    assert!(live.iter().enumerate().all(|(j, v)| v.iter().all(|b| *b == ((i0 + 2 * j) as u8) | 1)), "the live images are intact");
    // realloc: within the run in place, past it moved with its bytes, out of the range past 512 KiB
    let mut v = live.swap_remove(0);
    let p0 = v.as_ptr();
    let n = v.len();
    v.truncate(n - 100);
    v.shrink_to_fit();
    assert!(allocarena::own::owns(v.as_ptr()));
    v.reserve_exact(1 << 20);
    assert!(!allocarena::own::owns(v.as_ptr()) && v.iter().all(|b| *b == (i0 as u8) | 1), "moved out with its bytes (from {p0:p})");
    drop(v);
    drop(live);
}

/// D3892 guard (cold page faults doubled with own pages): a freed run stays committed for the next image of its length,
/// so its pages are not faulted in again; a give-back returns them.
#[test]
fn a_freed_run_is_reused_without_a_fault_and_given_back_at_the_give_back() {
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    setup();
    let _ = allocarena::give_back();
    let size = 192 << 10;
    let first = {
        let _scope = scope();
        vec![7u8; size]
    };
    let (p, len) = (first.as_ptr() as usize, first.len());
    drop(first);
    assert_eq!(resident(p as *const u8, len), len, "a freed run keeps its pages while the hot pool has room");
    let again = {
        let _scope = scope();
        vec![0u8; size]
    };
    assert_eq!(again.as_ptr() as usize, p, "the next image of its length takes it");
    assert!(again.iter().all(|b| *b == 0), "a zeroed request reads zero");
    drop(again);
    let back = allocarena::give_back();
    assert!(back >= len, "the give-back returned the hot run: {back}");
    assert!(resident(p as *const u8, len) <= 4096, "its pages went back");
}

/// LADDER-DESIGN 6a, (b): scoped images under 64 KiB stay on the cache heap, whose pages a live owner thread frees only
/// itself. After a give-back from another thread, the owner's answer to the collect ask (its part of the cache heap
/// collected) and one purge return them.
#[test]
fn small_images_of_a_live_owner_come_back_when_it_answers_the_collect_ask() {
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    setup();
    let (to_owner, owner_rx) = std::sync::mpsc::channel::<()>();
    let (to_main, main_rx) = std::sync::mpsc::channel::<Vec<Vec<u8>>>();
    let (ack, acked) = std::sync::mpsc::channel::<()>();
    let owner = std::thread::spawn(move || {
        let imgs: Vec<Vec<u8>> = (0..512)
            .map(|i| {
                let _scope = scope();
                vec![(i as u8) | 1; 24 << 10]
            })
            .collect();
        to_main.send(imgs).unwrap();
        // alive and idle until asked, then answers as a pool worker does at its idle park
        owner_rx.recv().unwrap();
        allocarena::collect_cache_local();
        // SAFETY: mimalloc's collect on this thread.
        unsafe { libmimalloc_sys::mi_collect(false) };
        ack.send(()).unwrap();
        owner_rx.recv().unwrap();
    });
    let imgs = main_rx.recv().unwrap();
    assert!(imgs.iter().all(|v| allocarena::in_cache_arena(v.as_ptr())), "small images are on the cache heap");
    let ranges: Vec<(usize, usize)> = imgs.iter().map(|v| (v.as_ptr() as usize, v.len())).collect();
    let before: usize = ranges.iter().map(|&(p, l)| resident(p as *const u8, l)).sum();
    drop(imgs);
    let _ = allocarena::give_back();
    to_owner.send(()).unwrap();
    // the owner answered, then the second purge
    acked.recv().unwrap();
    allocarena::collect_cache();
    // SAFETY: mimalloc's collect on this thread (the purge).
    unsafe { libmimalloc_sys::mi_collect(true) };
    let after: usize = ranges.iter().map(|&(p, l)| resident(p as *const u8, l)).sum();
    to_owner.send(()).unwrap();
    owner.join().unwrap();
    assert!(before >= 512 * (24 << 10) * 9 / 10, "they were resident: {before}");
    assert!(after <= 32 << 10, "the live owner's answer gave the small images' pages back: {after} of {before} bytes still resident");
}
