//! pagepool P3 (F1): `PGRUST_POOL_TOUCH=1`: the claimer faults its WarmZero superpages in by a zero store per page (no
//! MADV_POPULATE_WRITE): it meets its target, and a span taken from them is resident and zero, and faults nothing more
//! when written.
#![cfg(target_os = "linux")]

mod common;
use common::*;
use pagepool::*;
use std::time::{Duration, Instant};

#[test]
fn the_claimer_prefaults_by_touch() {
    hooks::touch_switch(true);
    init_with(|c| {
        c.warm_zero = 8 * MIB;
        c.warm_zero_max = 8 * MIB;
    });
    assert!(start(), "the pool's threads started");
    let t0 = Instant::now();
    while stats().warm_zero < 8 * MIB && t0.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(10));
    }
    let s = stats();
    assert!(s.warm_zero >= 8 * MIB && s.prefaulted >= 8 * MIB as u64, "the claimer met its target by touch: {s:?}");
    hooks::claimer_hold(true);
    let h = Hint::new(Class::Task, Tag::OTHER);
    let sp = span(SUPER, h).expect("a span from the warm-zero list");
    let a = sp.as_ptr() as usize;
    assert_eq!(resident(a, SUPER), SUPER, "prefaulted: every page resident");
    // SAFETY: ours.
    assert!(unsafe { core::slice::from_raw_parts(a as *const u8, SUPER) }.iter().all(|&b| b == 0), "and zero");
    let f0 = minflt();
    touch(a, SUPER);
    assert!(minflt() - f0 <= HOST_SLACK, "a write faults nothing more: {} faults", minflt() - f0);
    // SAFETY: ours, not used again.
    unsafe { give_span(sp) };
    hooks::claimer_hold(false);
    stop();
}
