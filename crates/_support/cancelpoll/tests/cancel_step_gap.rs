//! C9 checker 2 (debug builds only): a live step's stretch without a poll at the threshold is one gap (`step_gaps`, one
//! `step-gap:` line per label); a step that ticks, a step outside a kernel call and a stretch a nested kernel call breaks
//! are none. One test: the threshold and the count are process-wide. Named cancel_* for the crate tests' serial tail
//! (D4079): the gap is wall-clock time.
#![cfg(debug_assertions)]
use core::sync::atomic::AtomicU8;
use std::time::{Duration, Instant};

fn spin(ms: u64) {
    let t = Instant::now();
    while t.elapsed() < Duration::from_millis(ms) {
        core::hint::spin_loop();
    }
}

#[test]
fn cancel_a_live_step_past_the_threshold_without_a_poll_is_one_gap() {
    let byte = AtomicU8::new(0);
    cancelpoll::set_step_gap_ms(100);
    let n0 = cancelpoll::step_gaps();
    cancelpoll::scoped(&byte, || {
        let mut s = cancelpoll::step("ticks");
        for _ in 0..1000 {
            let _ = s.tick();
        }
    });
    assert_eq!(cancelpoll::step_gaps(), n0, "a step that ticks: no gap");
    cancelpoll::scoped(&byte, || {
        let s = cancelpoll::step("never ticks");
        spin(150);
        drop(s);
    });
    assert_eq!(cancelpoll::step_gaps(), n0 + 1, "one gap, at the step's end");
    cancelpoll::scoped(&byte, || {
        let mut s = cancelpoll::step("one long unit");
        let _ = s.tick();
        spin(150);
        let _ = s.tick();
    });
    assert_eq!(cancelpoll::step_gaps(), n0 + 2, "one gap, at the tick after the long unit");
    let s = cancelpoll::step("outside a kernel call");
    spin(150);
    drop(s);
    assert_eq!(cancelpoll::step_gaps(), n0 + 2, "no byte, no check");
    let inner = AtomicU8::new(0);
    cancelpoll::scoped(&byte, || {
        let mut s = cancelpoll::step("runs nested kernel calls");
        for _ in 0..3 {
            spin(60);
            cancelpoll::scoped(&inner, || {});
        }
        spin(60);
        let _ = s.tick();
    });
    assert_eq!(cancelpoll::step_gaps(), n0 + 2, "a nested kernel call's start and end are polls");
    cancelpoll::set_step_gap_ms(cancelpoll::STEP_GAP_MS);
}
