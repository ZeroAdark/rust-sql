//! C9's witness lever (`set_cancel_at`): a step whose label starts with the armed text, opened inside a kernel call, has
//! the host deliver its statement's cancel before its first tick, so that tick stops. Its own process: the lever is set
//! once per process.
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

static FIRED: AtomicUsize = AtomicUsize::new(0);

/// The host's delivery: a cancel raises the statement's abort bit (pg_cancel_backend's SIGINT does at delivery).
fn fire(byte: *const AtomicU8) {
    FIRED.fetch_add(1, Ordering::Relaxed);
    // SAFETY: the byte of the kernel call the step opened in, live through it
    unsafe { &*byte }.fetch_or(cancelpoll::ABORT_BIT, Ordering::SeqCst);
}

#[test]
fn a_step_of_the_armed_label_is_canceled_before_its_first_tick() {
    cancelpoll::set_cancel_at("armed_step:", fire);
    assert!(cancelpoll::step("armed_step: its rows").tick().is_ok(), "outside a kernel call the lever has no statement to cancel");
    let byte = AtomicU8::new(0);
    let ((), stopped) = cancelpoll::scoped_seen(&byte, || {
        assert!(cancelpoll::step("another step: armed_step:").tick().is_ok(), "only a label that starts with the armed text fires");
        let mut s = cancelpoll::step("armed_step: its rows");
        assert_eq!(byte.load(Ordering::Relaxed) & cancelpoll::ABORT_BIT, cancelpoll::ABORT_BIT, "the cancel came as the step opened");
        assert!(s.tick().is_err(), "its first tick stops");
    });
    assert!(stopped);
    assert_eq!(FIRED.load(Ordering::Relaxed), 1);
}
