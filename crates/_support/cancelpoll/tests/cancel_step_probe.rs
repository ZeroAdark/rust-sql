//! C9: with the poll-gap probe on, every tick of a Step is a poll (it ends the stretch) and still stops at the abort
//! bit; a Step's label is what held a gap, as `phase()`'s was. One test: the switch is process-wide. Named cancel_* for
//! the crate tests' serial tail (D4079): the stretches are wall-clock time.
use core::sync::atomic::{AtomicU8, Ordering};

const MS: u64 = 20;
const LONG: u64 = 2 * MS;

fn busy(ms: u64) {
    let t0 = pg_clock::mono_ns();
    while pg_clock::mono_ns() - t0 < ms * 1_000_000 {
        core::hint::spin_loop();
    }
}

#[test]
fn a_ticking_step_ends_every_stretch_and_a_silent_one_holds_a_gap() {
    cancelpoll::set_probe_ms(MS);
    let byte = AtomicU8::new(cancelpoll::PROBE_BIT);
    cancelpoll::scoped(&byte, || {
        let mut s = cancelpoll::step("a ticking step");
        for _ in 0..LONG {
            busy(1);
            assert!(s.tick().is_ok());
        }
    });
    assert_eq!(cancelpoll::probe_max().1, 0, "a tick every ms is a poll every ms");
    cancelpoll::scoped(&byte, || {
        let mut s = cancelpoll::step("a silent step");
        busy(LONG);
        assert!(s.tick().is_ok(), "the probe bit is no cancel");
    });
    let (max, n) = cancelpoll::probe_max();
    assert_eq!(n, 1, "one gap: the silent step's");
    assert!((LONG..1000).contains(&max), "{max} ms");
    cancelpoll::scoped(&byte, || {
        let mut s = cancelpoll::step("a canceled step");
        assert!(s.tick().is_ok());
        byte.fetch_or(cancelpoll::ABORT_BIT, Ordering::Release);
        assert!(s.tick().is_err() && cancelpoll::seen(), "the probe's path still reads the abort bit");
    });
    cancelpoll::set_probe_ms(0);
}
