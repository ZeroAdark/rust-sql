//! D3887: the host loops' polls switched off (`PGRUST_GRAVITY_HOST_POLL=0`, the probe's before-arm) read nothing: the loops
//! run to their end as before. One test: the switch is process-wide.
use core::sync::atomic::AtomicU8;

#[test]
fn host_polls_off_never_stop_a_loop() {
    let byte = AtomicU8::new(cancelpoll::ABORT_BIT);
    cancelpoll::set_host_polls(false);
    cancelpoll::scoped(&byte, || assert!(!cancelpoll::canceled() && !cancelpoll::seen()));
    cancelpoll::scoped(&byte, || assert!(cancelpoll::step("off").tick().is_ok() && !cancelpoll::seen()));
    // a byte its caller holds outside a kernel call (the drain's, the merge owner's) was never the switch's
    assert!(unsafe { cancelpoll::step_on(byte.as_ptr(), "held") }.tick().is_err());
    cancelpoll::set_host_polls(true);
    cancelpoll::scoped(&byte, || assert!(cancelpoll::canceled() && cancelpoll::seen()));
    cancelpoll::scoped(&byte, || assert!(cancelpoll::step("on").tick().is_err() && cancelpoll::seen()));
}
