//! D3887: a host loop's abort poll reads the byte its kernel call lent, and only while the call runs.
use core::sync::atomic::{AtomicU8, Ordering};

#[test]
fn a_host_loop_sees_the_abort_bit_of_the_call_it_runs_in_and_nothing_outside_one() {
    let byte = AtomicU8::new(cancelpoll::ABORT_BIT);
    assert!(!cancelpoll::canceled() && !cancelpoll::seen(), "outside a kernel call there is no byte to read");
    let quiet = AtomicU8::new(cancelpoll::PROBE_BIT | 2);
    cancelpoll::scoped(&quiet, || {
        assert!(!cancelpoll::canceled(), "the probe and the service bits are no cancel");
        assert!(!cancelpoll::seen());
    });
    cancelpoll::scoped(&byte, || {
        assert!(!cancelpoll::seen(), "nothing stopped yet");
        assert!(cancelpoll::canceled());
        assert!(cancelpoll::seen(), "the stop is the rest of the call's");
        byte.store(0, Ordering::Relaxed);
        assert!(!cancelpoll::canceled() && cancelpoll::seen(), "the stop stays seen once a loop stopped");
    });
    assert!(!cancelpoll::seen(), "the call's end ends its stop");
}

#[test]
fn a_nested_kernel_call_lends_its_own_byte_and_hands_its_stop_back() {
    let outer = AtomicU8::new(0);
    let inner = AtomicU8::new(cancelpoll::ABORT_BIT);
    cancelpoll::scoped(&outer, || {
        assert!(!cancelpoll::canceled());
        cancelpoll::scoped(&inner, || assert!(cancelpoll::canceled() && cancelpoll::seen()));
        assert!(!cancelpoll::canceled(), "the outer call's byte is back");
        assert!(cancelpoll::seen(), "the nested call's stop is the outer call's error too");
    });
    assert!(!cancelpoll::seen());
}

#[test]
fn an_unwind_out_of_the_call_puts_the_previous_byte_back() {
    let byte = AtomicU8::new(cancelpoll::ABORT_BIT);
    let r = std::panic::catch_unwind(core::panic::AssertUnwindSafe(|| {
        cancelpoll::scoped(&byte, || {
            assert!(cancelpoll::canceled());
            panic!("a host call unwound");
        })
    }));
    assert!(r.is_err());
    assert!(!cancelpoll::canceled() && !cancelpoll::seen());
}
