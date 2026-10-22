//! Age release on (mode 0, decay 50 ms): idle central lists go home on the purger's pass, and age release returns them.

mod common;
use common::*;
use pagepool::*;
use std::time::{Duration, Instant};

#[test]
fn idle_central_lists_go_home_and_age_release_returns_them() {
    init_with(|c| {
        c.release_mode = 0;
        c.decay_ms = 50;
    });
    assert!(start(), "the pool's threads started");
    let h = Hint::new(Class::Task, Tag::OTHER);
    let blocks: Vec<_> = (0..4096).map(|_| take(16 << 10, 16, h).expect("a block")).collect();
    for b in &blocks {
        touch(b.as_ptr().cast::<u8>() as usize, 16 << 10);
    }
    let (c0, r0) = (stats().claimed, stats().released_bytes);
    for b in blocks {
        // SAFETY: ours, not used again.
        unsafe { give(b.cast(), 16 << 10) };
    }
    flush_shelf();
    assert!(stats().central > 0, "the freed blocks wait on the central lists: {:?}", stats());
    let t0 = Instant::now();
    while (stats().central > 0 || stats().released_bytes - r0 < 32 * MIB as u64) && t0.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let s = stats();
    assert_eq!(s.central, 0, "idle lists went home: {s:?}");
    assert!(s.released_bytes - r0 >= 32 * MIB as u64, "age release returned the whole free superpages: {s:?}");
    assert!(s.claimed < c0, "claimed fell: {c0} -> {}", s.claimed);
}
