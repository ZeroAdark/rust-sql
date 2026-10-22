//! C9: `scoped_seen` tells the driver whether a host loop of a kernel call stopped for the abort bit, so a call that
//! failed after such a stop settles as the statement's cancel. It reads the stop and never consumes it: the outer call
//! still sees it, and the byte keeps its abort bit for the statement's owner.
use core::sync::atomic::{AtomicU8, Ordering};

#[test]
fn a_call_reports_its_stop_and_leaves_it_for_the_outer_call_and_the_statement() {
    let quiet = AtomicU8::new(0);
    let ((), stopped) = cancelpoll::scoped_seen(&quiet, || assert!(!cancelpoll::canceled()));
    assert!(!stopped, "no abort bit, no stop");
    let byte = AtomicU8::new(cancelpoll::ABORT_BIT);
    let (r, stopped) = cancelpoll::scoped_seen(&byte, || cancelpoll::canceled());
    assert!(r && stopped, "a loop that stopped is the call's stop");
    assert_eq!(byte.load(Ordering::Relaxed) & cancelpoll::ABORT_BIT, cancelpoll::ABORT_BIT, "the statement's byte keeps its abort bit");
    let ((), stopped) = cancelpoll::scoped_seen(&byte, || {});
    assert!(!stopped, "a call whose host code polled nothing saw no stop, whatever the byte holds");
    // a Step's tick stops the same way
    let ((), stopped) = cancelpoll::scoped_seen(&byte, || assert!(cancelpoll::step("a funnel's rows").tick().is_err()));
    assert!(stopped, "a lent step's stop is the call's");
    // a step on a byte its caller holds (the leader's drain, an owner's fold) leaves the call's stop alone
    let ((), stopped) = cancelpoll::scoped_seen(&quiet, || assert!(cancelpoll::step_static(Box::leak(Box::new(AtomicU8::new(cancelpoll::ABORT_BIT))), "a held byte").tick().is_err()));
    assert!(!stopped, "a held byte's stop is its caller's to raise, not the call's");
}

#[test]
fn a_nested_calls_stop_stays_seen_by_the_outer_call() {
    let outer = AtomicU8::new(0);
    let inner = AtomicU8::new(cancelpoll::ABORT_BIT);
    let ((), outer_stopped) = cancelpoll::scoped_seen(&outer, || {
        let ((), stopped) = cancelpoll::scoped_seen(&inner, || assert!(cancelpoll::canceled()));
        assert!(stopped, "the nested call reports its stop");
        assert!(cancelpoll::seen(), "reading it did not consume it: the outer call sees it");
    });
    assert!(outer_stopped, "the outer call settles with the nested call's stop");
    assert_eq!(inner.load(Ordering::Relaxed), cancelpoll::ABORT_BIT, "the nested statement's byte keeps its abort bit");
    assert!(!cancelpoll::seen(), "outside every call nothing is lent");
}
