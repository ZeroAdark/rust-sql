//! The purger gives back a dead thread's runs once remote frees bring every block home (no `release_heap`; own binary).
#![cfg(all(feature = "global", not(target_family = "wasm")))]

use pagepool::PoolAlloc;
use std::time::{Duration, Instant};

#[global_allocator]
static A: PoolAlloc = PoolAlloc;

#[test]
fn the_purger_gives_back_dead_threads_runs() {
    let warm = vec![1u8; 64];
    assert!(pagepool::global_on() && pagepool::start(), "PoolAlloc serves and the pool's threads run");
    drop(warm);
    let a0 = pagepool::small_counts().abandoned;
    let blocks = std::thread::spawn(|| (0..20_000).map(|i| vec![i as u8; 200].into_boxed_slice()).collect::<Vec<_>>()).join().unwrap();
    let left = pagepool::small_counts().abandoned - a0;
    assert!(left >= 32, "the dead thread left its runs with live blocks: {left}");
    let r0 = pagepool::small_counts().retired;
    std::thread::spawn(move || drop(blocks)).join().unwrap();
    let t0 = Instant::now();
    while pagepool::small_counts().retired - r0 < left && t0.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let c = pagepool::small_counts();
    assert!(c.retired - r0 >= left, "the purger gave back the {left} dead runs: {c:?}");
}
