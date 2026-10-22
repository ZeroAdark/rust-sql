//! Op 14 (JOIN's deferred build error, main 09-24): the spill table's held-error slot. The statement's first held error
//! stays (a later one is dropped); every raise returns a copy and stashes the host payload again on the raising thread
//! (two probes on two threads each raise the same error, as PG's participants may); a clear (every exit path) drops it
//! unraised. One test: it registers the process-wide hooks.

use std::sync::atomic::{AtomicUsize, Ordering};

use gravity_driver::{Error, HeldError, HeldHooks, SpillTable, HELD_HOOKS};

static TAKEN: AtomicUsize = AtomicUsize::new(0);
static RESTASHED: AtomicUsize = AtomicUsize::new(0);

fn take() -> Option<Box<dyn std::any::Any + Send + Sync>> {
    TAKEN.fetch_add(1, Ordering::Relaxed);
    Some(Box::new(String::from("division by zero")))
}

fn restash(p: &(dyn std::any::Any + Send + Sync)) {
    assert_eq!(p.downcast_ref::<String>().map(String::as_str), Some("division by zero"), "the payload held");
    RESTASHED.fetch_add(1, Ordering::Relaxed);
}

#[test]
fn the_first_held_error_is_raised_by_every_participant_and_dropped_unraised_at_the_clear() {
    assert!(HELD_HOOKS.set(HeldHooks { take, restash }).is_ok());
    let mut t = SpillTable::with_slots(2);
    assert_eq!((t.held(), t.raise_held()), (None, None), "nothing held: a raise answers None");

    let payload = (HELD_HOOKS.get().unwrap().take)();
    t.hold(HeldError { err: Error::host("pg"), payload });
    t.hold(HeldError { err: Error::Raised(1), payload: None });
    assert_eq!(t.held(), Some(Error::host("pg")), "the statement's first held error stays");

    let t_ref = &t;
    let raised: Vec<Option<Error>> = std::thread::scope(|s| {
        let a = s.spawn(|| t_ref.raise_held());
        let b = s.spawn(|| t_ref.raise_held());
        vec![a.join().unwrap(), b.join().unwrap()]
    });
    assert_eq!(raised, vec![Some(Error::host("pg")), Some(Error::host("pg"))], "each participant raises the same error");
    assert_eq!((TAKEN.load(Ordering::Relaxed), RESTASHED.load(Ordering::Relaxed)), (1, 2), "the payload taken once, stashed again per raise");
    assert_eq!(t.held(), Some(Error::host("pg")), "a raise leaves it for the next raiser");

    t.clear();
    assert_eq!((t.held(), t.raise_held()), (None, None), "the clear drops it unraised");
    assert_eq!(RESTASHED.load(Ordering::Relaxed), 2);
}

/// D1902 (errorhold): a rerun call's held errors are its own. A member build's error held in call 1 (its outer had no row)
/// gives way to call 2's own, and is dropped at call 2's begin (the member's region is reset then); an error held outside
/// the set stays the statement's, raised again once the calls end, while a call's own raise reads the call's error.
#[test]
fn a_rerun_call_raises_its_own_held_error_and_drops_an_earlier_calls() {
    let t = SpillTable::with_slots(2);
    let member = |b: usize| b == 3;
    t.call_begin(&member);
    t.hold_for(HeldError { err: Error::host("call 1"), payload: None }, 3);
    assert_eq!(t.raise_held(), Some(Error::host("call 1")), "a call's member probe raises its build's error");
    t.call_end();
    assert_eq!(t.held(), Some(Error::host("call 1")), "kept past the call (nothing held around it), finished");
    t.call_begin(&member);
    assert_eq!(t.held(), None, "the set's next call drops it: the member's region is reset");
    t.hold_for(HeldError { err: Error::host("call 2"), payload: None }, 3);
    assert_eq!(t.raise_held(), Some(Error::host("call 2")), "the call's own error, not call 1's");
    t.call_end();

    // an error held outside the set (pipeline 1's region) stays the statement's first
    let t = SpillTable::with_slots(2);
    t.hold_for(HeldError { err: Error::host("outside"), payload: None }, 1);
    t.call_begin(&member);
    assert_eq!(t.raise_held(), Some(Error::host("outside")), "a call that held nothing reads the layer around it");
    t.hold_for(HeldError { err: Error::host("member"), payload: None }, 3);
    t.hold_for(HeldError { err: Error::host("member again"), payload: None }, 3);
    assert_eq!(t.raise_held(), Some(Error::host("member")), "the call's first error, in its own layer");
    t.call_end();
    assert_eq!(t.held(), Some(Error::host("outside")), "the call's error dropped: the statement's first stays");
    t.call_begin(&member);
    assert_eq!(t.held(), Some(Error::host("outside")), "not a member's region: kept at the next call");
    t.call_end();

    // a finished call's error gives way to a later hold outside the set; a clear drops every layer
    let t = SpillTable::with_slots(2);
    t.call_begin(&member);
    t.hold_for(HeldError { err: Error::host("call"), payload: None }, 3);
    t.call_end();
    t.hold_for(HeldError { err: Error::host("later"), payload: None }, 1);
    assert_eq!(t.held(), Some(Error::host("later")), "a finished call's error gives way");
    t.hold(HeldError { err: Error::host("third"), payload: None });
    assert_eq!(t.held(), Some(Error::host("later")), "the statement's first again");
    let mut t = t;
    t.clear();
    assert_eq!(t.held(), None);
}
