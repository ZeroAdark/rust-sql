//! pagepool P3 (F1; ppsmall's 19:11Z finding): with age release on (pool_release_mode 0) and the claimer on, an idle
//! pool's prefaulted bytes stay flat: the age pass keeps WarmZero up to the claimer's target, so the claimer has nothing
//! to fill again. (Before: each pass gave back the target's superpages once they were a decay old and the claimer faulted
//! them in again, ~13 MB/s on an idle server.)

mod common;
use common::*;
use pagepool::*;
use std::time::{Duration, Instant};

#[test]
fn an_idle_pool_does_not_prefault_again_what_age_release_gave_back() {
    init_with(|c| {
        c.release_mode = 0;
        c.decay_ms = 20;
        c.warm_zero = 8 * MIB;
        c.warm_zero_max = 8 * MIB;
    });
    assert!(start(), "the pool's threads started");
    let t0 = Instant::now();
    while stats().warm_zero < 8 * MIB && t0.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(stats().warm_zero >= 8 * MIB, "the claimer met its target: {:?}", stats());
    let p0 = stats().prefaulted;
    // past a hundred decays and two of the claimer's own wakes
    std::thread::sleep(Duration::from_millis(2500));
    let s = stats();
    assert_eq!(s.prefaulted, p0, "an idle pool prefaults nothing again: {s:?}");
    assert!(s.warm_zero >= 8 * MIB, "and keeps its warm-zero target: {s:?}");
    stop();
}
