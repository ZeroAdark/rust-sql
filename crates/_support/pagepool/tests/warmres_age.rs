//! ppres (b6 21:24Z, from ppgrav's hunk check): `pool_release_mode` (a) with `PGRUST_POOL_RESERVE` on. WarmDirty is the
//! reserve too (recycled in place), so the age pass takes only the spare past the claimer's target: an idle pass leaves
//! the reserve's bytes held, dirty ones included, and an idle pool prefaults nothing again.
#![cfg(target_os = "linux")]

mod common;
use common::*;
use pagepool::*;
use std::time::{Duration, Instant};

const FLOOR: usize = 4 * MIB;

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
fn an_idle_age_pass_leaves_the_reserve_held() {
    hooks::reserve_switch(true);
    init_with(|c| {
        c.release_mode = 0;
        c.decay_ms = 20;
        c.warm_cap = 0;
        c.warm_zero = FLOOR;
        c.warm_zero_max = 16 * MIB;
        c.idle_drain_ms = 600_000;
    });
    assert!(start(), "the pool's threads started");
    wait_for("the claimer met its target", |s| s.warm_target == FLOOR && s.warm_zero >= FLOOR && hooks::claimer_met());

    // the reserve goes dirty (taken whole, written, given back) with the claimer held, so nothing recycles it meanwhile
    hooks::claimer_hold(true);
    std::thread::sleep(Duration::from_millis(20));
    let h = Hint::new(Class::Task, Tag::OTHER);
    let held: Vec<Span> = (0..FLOOR / SUPER).map(|_| span(SUPER, h).expect("span")).collect();
    for s in held {
        // SAFETY: a span we hold, written then given back whole.
        unsafe { core::ptr::write_bytes(s.as_ptr(), 0xcd, s.len()) };
        unsafe { give_span(s) };
    }
    // the target is FLOOR, or 1.1 x the drawdown in progress once the (held) claimer's tick has run: never under the level
    let s0 = stats();
    assert_eq!((s0.warm_zero, s0.warm_dirty), (0, FLOOR), "{:?}", reserve_stats());
    assert!(s0.warm_target >= FLOOR, "{s0:?}");
    // past twenty decays: every dirty superpage is old enough, and warm free is past warm_cap 0
    std::thread::sleep(Duration::from_millis(400));
    let s = stats();
    assert_eq!((s.warm_dirty, s.releases), (FLOOR, s0.releases), "the age pass took none of the reserve: {s:?}");

    // the claimer zeroes it in place; then an idle pool keeps its target and populates nothing again
    hooks::claimer_hold(false);
    let s = wait_for("refilled", |s| s.warm_zero >= s.warm_target && s.warm_target >= FLOOR && hooks::claimer_met());
    let r = reserve_stats();
    assert!(r.recycled > 0, "{r:?}");
    std::thread::sleep(Duration::from_millis(1_000));
    let s1 = stats();
    assert_eq!(s1.prefaulted, s.prefaulted, "an idle pool prefaults nothing again: {s1:?} {:?}", reserve_stats());
    assert!(s1.warm_zero + s1.warm_dirty >= s1.warm_target, "and keeps its reserve: {s1:?}");
    stop();
}
