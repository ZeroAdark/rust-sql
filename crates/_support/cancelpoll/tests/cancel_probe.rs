//! D3887: the poll-gap probe logs a stretch between abort polls inside a kernel call past its threshold, with the
//! labelled host steps that held it; the time outside a kernel call is no gap. One test: the switch is process-wide.
//! D4079: named cancel_* so its binary runs in the crate tests' serial tail (CHECK_CT_SERIAL 'cancel'), alone after e2e;
//! the stretches are wall-clock time, and nice-10 preemption beside e2e booked a 1 ms stretch as a 3 ms gap. The
//! threshold is 20 ms; every stretch the test calls no gap is 1 ms of a 40 ms run (2x the threshold), so a probe that
//! stopped ending a stretch at a poll, or that counted time outside a kernel call or while off, still books a gap here.
use core::sync::atomic::AtomicU8;

const MS: u64 = 20; // the probe's threshold
const LONG: u64 = 2 * MS; // every run the test checks: twice the threshold

fn busy(ms: u64) {
    let t0 = pg_clock::mono_ns();
    while pg_clock::mono_ns() - t0 < ms * 1_000_000 {
        core::hint::spin_loop();
    }
}

#[test]
fn the_probe_finds_a_long_host_step_between_polls_and_nothing_when_off() {
    let byte = AtomicU8::new(0);
    cancelpoll::scoped(&byte, || busy(LONG));
    assert_eq!(cancelpoll::probe_max(), (0, 0), "off by default");
    assert!(!cancelpoll::probe_set());

    cancelpoll::set_probe_ms(MS);
    assert!(cancelpoll::probe_on() && cancelpoll::probe_set());
    busy(LONG); // outside a kernel call: no gap
    assert_eq!(cancelpoll::probe_max().1, 0);
    cancelpoll::scoped(&byte, || {
        for _ in 0..LONG {
            busy(1);
            cancelpoll::polled();
        }
    });
    assert_eq!(cancelpoll::probe_max().1, 0, "polls every ms under the threshold");
    cancelpoll::scoped(&byte, || {
        let _host = cancelpoll::phase("a host call");
        let step = cancelpoll::phase("a long step");
        busy(LONG);
        drop(step);
        assert!(!cancelpoll::canceled(), "a host loop's poll ends the gap");
    });
    let (max, n) = cancelpoll::probe_max();
    assert_eq!(n, 1, "one gap: the long step's");
    assert!((LONG..1000).contains(&max), "{max} ms");
    cancelpoll::scoped(&byte, || busy(LONG));
    assert_eq!(cancelpoll::probe_max().1, 2, "a call that never polls ends its gap at its end");

    cancelpoll::set_probe_ms(0);
    cancelpoll::scoped(&byte, || busy(LONG));
    assert_eq!(cancelpoll::probe_max().1, 2, "off again");
}
