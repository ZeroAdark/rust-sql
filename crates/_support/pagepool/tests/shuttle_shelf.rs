//! Shuttle model, card 2 (a) + (b): blocks taken and given across threads (shelves spilling half a list to the central
//! stacks, refills popping them back) while a third thread homes the central lists and releases (`purge_now`). No block
//! is handed out twice, every block keeps its owner's stamp while held, and at quiescence the pool's claim is its parts
//! and every superpage sits in the bitmap its state names (`hooks::check`). Real crate; every atomic of the shared state
//! is a scheduling point (src/sync.rs). Runs only with RUSTFLAGS="--cfg shuttle".
#![cfg(shuttle)]

use pagepool::{Class, Config, Hint, Tag};
use shuttle::thread;
use std::collections::HashSet;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

const BLOCK: usize = 1024;
/// Past the 1 KiB key's shelf cap (shelf_max / 8 / 1 KiB = 8): every thread spills to the central stack.
const K: usize = 10;

/// Bytes the third thread's purge gave back over all rounds (a std atomic: outside the model).
static PURGED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn setup() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let cfg = Config {
            va: 8 << 20,
            chunk: 8 << 20,
            lazy_va: 0,
            frame_va: 0,
            shelf_max: 64 << 10,
            warm_zero: 0,
            warm_zero_max: 0,
            warm_cap: 0,
            decay_ms: 0,
            purge_batch: 2 << 20,
            lazy_min: 32 << 20,
            idle_drain_ms: 1000,
            block_min: 1024,
            release_mode: 2,
            lazy_decay_ms: 0,
            lazy_cap: 0,
            age_pass_max: 0,
            thp_partial: 0,
            host_age_ms: 0,
        };
        assert!(pagepool::init(&cfg));
    });
}

type Live = Arc<Mutex<HashSet<usize>>>;

fn take(live: &Live, who: u64, seq: u64) -> usize {
    let p = pagepool::take(BLOCK, 16, Hint::new(Class::Task, Tag::OTHER)).expect("take").cast::<u8>().as_ptr() as usize;
    assert!(live.lock().unwrap().insert(p), "block {p:#x} handed out twice");
    // SAFETY: a block we hold, past its 16 link bytes.
    unsafe { ((p + 64) as *mut u64).write(who << 32 | seq) };
    p
}

fn give(live: &Live, p: usize, who: u64, seq: u64) {
    // SAFETY: a block we hold.
    let stamp = unsafe { ((p + 64) as *const u64).read() };
    assert_eq!(stamp, who << 32 | seq, "block {p:#x} written by another holder");
    assert!(live.lock().unwrap().remove(&p));
    // SAFETY: taken with BLOCK bytes, not used again.
    unsafe { pagepool::give(NonNull::new(p as *mut u8).unwrap(), BLOCK) };
}

fn one_round() {
    setup();
    let live: Live = Arc::new(Mutex::new(HashSet::new()));
    let handed: Arc<Mutex<Vec<(usize, u64, u64)>>> = Arc::new(Mutex::new(Vec::new()));
    let (l1, h1) = (live.clone(), handed.clone());
    let t1 = thread::spawn(move || {
        let mine: Vec<_> = (0..K as u64).map(|s| (take(&l1, 1, s), 1, s)).collect();
        // half to the other thread, which gives them from its own shelf
        let (keep, pass) = mine.split_at(K / 2);
        h1.lock().unwrap().extend_from_slice(pass);
        for &(p, w, s) in keep {
            give(&l1, p, w, s);
        }
        pagepool::flush_shelf();
    });
    let (l2, h2) = (live.clone(), handed.clone());
    let t2 = thread::spawn(move || {
        let mine: Vec<_> = (0..K as u64).map(|s| (take(&l2, 2, s), 2, s)).collect();
        for (p, w, s) in mine {
            give(&l2, p, w, s);
        }
        let got: Vec<_> = h2.lock().unwrap().drain(..).collect();
        for (p, w, s) in got {
            give(&l2, p, w, s);
        }
        pagepool::flush_shelf();
    });
    let t3 = thread::spawn(|| {
        pagepool::flush_shelf();
        PURGED.fetch_add(pagepool::purge_now(2 << 20, None), std::sync::atomic::Ordering::Relaxed);
    });
    t1.join().unwrap();
    t2.join().unwrap();
    t3.join().unwrap();
    // what t1 handed and t2 had already drained is given; the rest (t2 drained before t1 handed) here
    let rest: Vec<_> = handed.lock().unwrap().drain(..).collect();
    for (p, w, s) in rest {
        give(&live, p, w, s);
    }
    pagepool::flush_shelf();
    assert!(live.lock().unwrap().is_empty());
    if let Err(e) = pagepool::hooks::check() {
        panic!("{e}: {:?}", pagepool::stats());
    }
}

#[test]
fn shelves_and_central_across_threads() {
    let iters = std::env::var("SHUTTLE_ITERS").ok().and_then(|v| v.parse().ok()).unwrap_or(2000);
    shuttle::check_random(one_round, iters);
    shuttle::check_pct(one_round, iters, 3);
    eprintln!("MODEL shelves: {} rounds, the racing purge gave {} bytes", 2 * iters, PURGED.load(std::sync::atomic::Ordering::Relaxed));
}
