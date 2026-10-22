//! D3962: the time inside an interruptible wait (`cancelpoll::waiting`: a latch wait the cancel's signal wakes) is no
//! poll gap; its start ends the stretch before it, as a poll does. One test: the switch is process-wide.
//! D4079: named cancel_* so its binary runs in the crate tests' serial tail (CHECK_CT_SERIAL 'cancel'), alone after e2e;
//! the stretches are wall-clock time, and nice-10 preemption beside e2e booked the 1 ms stretch after a wake as a 3 ms
//! gap. The threshold is 20 ms and every wait lasts 40 ms (2x the threshold), so a probe that booked a wait's time, or
//! that did not start the stretch again at the wake, still books a gap here.
use core::sync::atomic::AtomicU8;

const MS: u64 = 20; // the probe's threshold
const LONG: u64 = 2 * MS; // every wait and every gap the test makes: twice the threshold

fn busy(ms: u64) {
    let t0 = pg_clock::mono_ns();
    while pg_clock::mono_ns() - t0 < ms * 1_000_000 {
        core::hint::spin_loop();
    }
}

#[test]
fn an_interruptible_wait_is_no_gap_and_its_start_ends_one() {
    let byte = AtomicU8::new(0);
    cancelpoll::set_probe_ms(MS);
    cancelpoll::scoped(&byte, || {
        for _ in 0..3 {
            let wait = cancelpoll::waiting();
            busy(LONG); // the wait: a lock wait's sleep between two wakes
            drop(wait);
            busy(1);
        }
    });
    assert_eq!(cancelpoll::probe_max().1, 0, "the waits booked no gap, and each wake starts the stretch again");
    cancelpoll::scoped(&byte, || {
        busy(LONG);
        let _wait = cancelpoll::waiting();
    });
    assert_eq!(cancelpoll::probe_max().1, 1, "the busy stretch before a wait is a gap, ended at the wait's start");
    cancelpoll::set_probe_ms(0);
    cancelpoll::scoped(&byte, || drop(cancelpoll::waiting()));
    assert_eq!(cancelpoll::probe_max().1, 1, "off: nothing");
}
