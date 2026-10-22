//! `PGRUST_POOL_RESERVE` off: the claimer's target is E1d's (doubled by a miss after each met target, capped, halved
//! back at idle) while the reserve's counters count the same takes: misses by class and size, the drawdown, POPULATE.
#![cfg(target_os = "linux")]

mod common;
use common::*;
use pagepool::*;
use std::time::{Duration, Instant};

const FLOOR: usize = 4 * MIB;
const MAX: usize = 16 * MIB;

fn wait_for(what: &str, f: impl Fn(&Stats) -> bool) -> Stats {
    let t0 = Instant::now();
    loop {
        let s = stats();
        if f(&s) {
            return s;
        }
        assert!(t0.elapsed() < Duration::from_secs(5), "{what}: {s:?} {:?}", reserve_stats());
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn off_keeps_e1d_and_counts() {
    hooks::reserve_switch(false);
    init_with(|c| {
        c.warm_zero = FLOOR;
        c.warm_zero_max = MAX;
        c.idle_drain_ms = 600_000;
    });
    assert!(start());
    wait_for("the claimer met its floor", |s| s.warm_zero >= FLOOR && hooks::claimer_met());
    let r0 = reserve_stats();
    assert!(!r0.on);
    assert!(r0.populate_n >= (FLOOR / SUPER) as u64 && r0.populate_ns > 0, "POPULATE timed: {r0:?}");
    assert_eq!(r0.populate_us.iter().sum::<u64>(), r0.populate_n);
    // the claimer ends its episode just after it marks the target met
    std::thread::sleep(Duration::from_millis(50));
    hooks::claimer_hold(true);
    let h = Hint::new(Class::Session, Tag::OTHER);
    let held: Vec<Span> = (0..FLOOR / SUPER + 2).map(|_| span(SUPER, h).expect("span")).collect();
    let s = stats();
    assert_eq!(s.warm_target, 2 * FLOOR, "E1d: one doubling per refill");
    let r = reserve_stats();
    assert_eq!(r.miss_class[Class::Session as usize] - r0.miss_class[Class::Session as usize], 2, "{r:?}");
    assert_eq!(r.miss_size[1] - r0.miss_size[1], 2, "one-superpage asks: {r:?}");
    assert_eq!(r.miss_bytes - r0.miss_bytes, 2 * SUPER as u64);
    assert_eq!(r.zero_takes - r0.zero_takes, (FLOOR / SUPER) as u64);
    assert_eq!(r.drawdown, FLOOR + 2 * SUPER, "drew the floor, then missed two: {r:?}");
    assert_eq!(r.target, 2 * FLOOR, "the drawdown moves no target with the switch off");
    hooks::claimer_hold(false);
    wait_for("the claimer met the doubled target", |s| s.warm_zero >= 2 * FLOOR && hooks::claimer_met());
    let r = reserve_stats();
    assert!(r.deepest >= FLOOR + 2 * SUPER && r.refills >= 1, "the episode went into the window: {r:?}");
    for s in held {
        // SAFETY: ours, not used again.
        unsafe { give_span(s) };
    }
    hooks::claimer_idle(4);
    let s = stats();
    assert_eq!(s.warm_target, FLOOR, "idle halved it back: {s:?}");
    assert_eq!(s.releases, 0, "no spare ages out with the switch off (pressure-only mode)");
    assert_eq!(reserve_stats().recycled, 0, "no recycle with the switch off");
    stop();
    balanced();
}
