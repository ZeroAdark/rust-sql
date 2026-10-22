//! D4872, the press's churn guard (`pool_host_age_ms`): pressed by the host, the pool gives back only whole free memory
//! free that long or more; what was freed just now stays (on RO OLTP over a dataset past RAM the press never lets go, and
//! the next statement takes again what the last one freed). Born red: the first cut released everything free at each
//! pressed sample, the young superpages with the old.

mod common;
use common::*;
use pagepool::*;
use std::time::{Duration, Instant};

const AGE_MS: u32 = 400;

/// Wait (at most 10 s) for `want` bytes released since `r0`, then a little more for the purger's pass to end.
fn released_since(r0: u64, want: usize) -> u64 {
    let t0 = Instant::now();
    while stats().released_bytes - r0 < want as u64 && t0.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(1));
    }
    std::thread::sleep(Duration::from_millis(200));
    stats().released_bytes - r0
}

#[test]
fn the_press_gives_back_only_memory_free_past_the_age() {
    init_with(|c| c.host_age_ms = AGE_MS);
    assert!(start(), "the pool's threads started");
    hooks::host_pressure_switch(true);
    let half = 64 * MIB;
    let h = Hint::new(Class::Process, Tag::BUFFER);
    // all taken first, so the young half cannot reuse the old half's superpages
    let mut spans: Vec<_> = (0..2 * half / (16 * MIB)).map(|_| span(16 * MIB, h).expect("span")).collect();
    for s in &spans {
        touch(s.as_ptr() as usize, s.len());
    }
    let young = spans.split_off(spans.len() / 2);
    for s in spans {
        // SAFETY: ours, not used again.
        unsafe { give_span(s) };
    }
    std::thread::sleep(Duration::from_millis(u64::from(AGE_MS) + 200));
    for s in young {
        // SAFETY: ours, not used again.
        unsafe { give_span(s) };
    }

    // pressed now: the old half goes, the young half (freed just now) stays
    let r0 = stats().released_bytes;
    set_host_pressed(true);
    let got = released_since(r0, half / 10 * 9);
    eprintln!("MEASURED host press, age {AGE_MS} ms: {} MiB of {} MiB free released", got >> 20, (2 * half) >> 20);
    assert!(got >= (half / 10 * 9) as u64, "the old half went: {got} of {half}");
    assert!(got <= (half + SUPER) as u64, "the young half stayed: {got} released, {half} old");

    // a pressed sample once the young half has aged too: it goes
    std::thread::sleep(Duration::from_millis(u64::from(AGE_MS) + 100));
    let r1 = stats().released_bytes;
    set_host_pressed(true);
    let more = released_since(r1, half / 10 * 9);
    assert!(more >= (half / 10 * 9) as u64, "aged, the young half went at the next pressed sample: {more} of {half}");
    assert_eq!(host_counts().presses, 1, "one press throughout");
    set_host_pressed(false);
}
