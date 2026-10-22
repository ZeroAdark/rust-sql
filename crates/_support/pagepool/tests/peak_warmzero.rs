//! ppround card 14: the climbing walk (`PGRUST_POOL_PEAK_RELEASE`, any levers) never releases the claimer's WarmZero
//! superpages. Its parts walk free runs of superpages in use, dead runs of live blocks and freed lazy spans, never a free
//! superpage, so the claimer has nothing to prefault again.

mod common;
use common::*;
use pagepool::*;
use std::time::{Duration, Instant};

#[test]
fn a_climbing_walk_keeps_the_claimers_warm_zero() {
    init_with(|c| {
        c.lazy_decay_ms = 600_000;
        c.warm_zero = 8 * MIB;
        c.warm_zero_max = 8 * MIB;
    });
    assert!(start(), "the pool's threads started");
    let t0 = Instant::now();
    while stats().warm_zero < 8 * MIB && t0.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(10));
    }
    let s0 = stats();
    assert!(s0.warm_zero >= 8 * MIB, "the claimer met its target: {s0:?}");
    let p0 = peak_counts().passes;
    hooks::peak_release_switch(true);
    hooks::peak_climb(Some(true));
    for levers in [0, LEVER_SKIP | LEVER_AGE | LEVER_WARM1, LEVER_SKIP | LEVER_AGE | LEVER_DEAD1 | LEVER_WARM1] {
        hooks::peak_levers(levers);
        for _ in 0..3 {
            std::thread::sleep(Duration::from_millis(WALK_MS as u64 + 30));
            lazy_pass_timed_now();
        }
    }
    let s = stats();
    assert!(peak_counts().passes >= p0 + 3, "the climbing walks ran: {:?}", peak_counts());
    assert_eq!((s.warm_zero, s.prefaulted), (s0.warm_zero, s0.prefaulted), "warm zero kept, nothing prefaulted again: {s:?}");
    hooks::peak_levers(0);
    hooks::peak_climb(None);
    hooks::peak_release_switch(false);
    stop();
}
