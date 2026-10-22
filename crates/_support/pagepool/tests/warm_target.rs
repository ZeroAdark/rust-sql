//! The claimer's warm-zero target adapts: it starts at pool_warm_zero, a burst of takes that run the warm lists dry
//! doubles it (once per refill), it never passes pool_warm_zero_max, and after idle windows it halves back to the floor.
//! The claimer is held during each burst and the idle windows are the decay's own clock, so no step races a thread.
#![cfg(target_os = "linux")]

mod common;
use common::*;
use pagepool::*;
use std::time::{Duration, Instant};

const FLOOR: usize = 4 * MIB;
const MAX: usize = 16 * MIB;

fn wait_for(what: &str, secs: u64, f: impl Fn(&Stats) -> bool) -> Stats {
    let t0 = Instant::now();
    loop {
        let s = stats();
        assert!(s.warm_target <= MAX, "the target passed its max: {s:?}");
        if f(&s) {
            return s;
        }
        assert!(t0.elapsed() < Duration::from_secs(secs), "{what}: {s:?}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn burst(n: usize) -> Vec<Span> {
    let h = Hint::new(Class::Task, Tag::OTHER);
    (0..n).map(|_| span(SUPER, h).expect("span")).collect()
}

#[test]
fn warm_target_grows_and_decays() {
    init_with(|c| {
        c.warm_zero = FLOOR;
        c.warm_zero_max = MAX;
        c.idle_drain_ms = 600_000;
    });
    assert!(start());
    let s = wait_for("the claimer met its floor", 5, |s| s.warm_zero >= FLOOR);
    assert_eq!(s.warm_target, FLOOR);
    let mut held = Vec::new();
    let mut want = FLOOR;
    for round in 0..4 {
        wait_for("the claimer marked its target met", 5, |_| hooks::claimer_met());
        hooks::claimer_hold(true);
        let m0 = stats().warm_misses;
        held.extend(burst(want / SUPER + 2));
        let s = stats();
        assert!(s.warm_misses > m0, "round {round}: the burst ran the warm list dry: {s:?}");
        want = (want * 2).min(MAX);
        assert_eq!(s.warm_target, want, "round {round}: one doubling per refill, capped");
        hooks::claimer_hold(false);
        wait_for("the claimer met the new target", 5, |s| s.warm_zero >= want);
    }
    for s in held {
        // SAFETY: ours, not used again.
        unsafe { give_span(s) };
    }
    hooks::claimer_idle(4);
    let s = stats();
    assert_eq!(s.warm_target, FLOOR, "idle halved the target back to the floor: {s:?}");
    assert_eq!(s.releases, 0, "the decay lowers the target; it releases nothing (pressure-only mode)");
    stop();
    balanced();
}
