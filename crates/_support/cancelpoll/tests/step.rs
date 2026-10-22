//! C9: a Step reads the byte its kernel call lent, stops with Canceled at the first tick that sees the abort bit, and
//! marks the call's stop as seen; its forms (each, pieces, batch, sort_by) tick before every unit or batch.
use core::sync::atomic::{AtomicU8, Ordering};

use cancelpoll::{sort_by, step, Canceled, BATCH, SORT_RUN};

#[test]
fn a_step_stops_at_the_abort_bit_of_its_kernel_call_and_never_outside_one() {
    let mut s = step("outside");
    assert_eq!(s.tick(), Ok(()), "outside a kernel call there is no byte to read");
    drop(s);
    let byte = AtomicU8::new(cancelpoll::PROBE_BIT | 2);
    cancelpoll::scoped(&byte, || {
        let mut s = step("quiet");
        assert_eq!(s.tick(), Ok(()), "the probe and the service bits are no cancel");
        byte.store(cancelpoll::ABORT_BIT, Ordering::Release);
        assert!(!cancelpoll::seen());
        assert_eq!(s.tick(), Err(Canceled));
        assert!(cancelpoll::seen(), "the stop is the rest of the call's");
        byte.store(0, Ordering::Release);
        assert_eq!(s.tick(), Ok(()));
        assert!(cancelpoll::seen());
    });
    assert!(!cancelpoll::seen());
}

#[test]
fn a_step_on_a_held_byte_reads_that_byte() {
    let byte = AtomicU8::new(0);
    let mut s = unsafe { cancelpoll::step_on(byte.as_ptr(), "held") };
    assert_eq!(s.tick(), Ok(()));
    byte.store(cancelpoll::ABORT_BIT, Ordering::Release);
    assert_eq!(s.tick(), Err(Canceled));
    let mut none = unsafe { cancelpoll::step_on(core::ptr::null(), "none") };
    assert_eq!(none.tick(), Ok(()));
    static HELD: AtomicU8 = AtomicU8::new(0);
    let mut s = cancelpoll::step_static(&HELD, "static");
    assert_eq!(s.tick(), Ok(()));
    HELD.store(cancelpoll::ABORT_BIT, Ordering::Release);
    assert_eq!(s.tick(), Err(Canceled));
}

/// D4159: a held step's stop is its caller's to raise. Outside a kernel call nothing resets `seen`, so a stop marked there
/// stayed set on the thread, and the host error path dropped the statement's own error (a statement_timeout's) for a
/// cancel; inside a call a held step's stop is not the call's either.
#[test]
fn a_held_steps_stop_never_marks_the_kernel_calls_stop() {
    static HELD: AtomicU8 = AtomicU8::new(cancelpoll::ABORT_BIT);
    let mut s = cancelpoll::step_static(&HELD, "static, outside a call");
    assert_eq!(s.tick(), Err(Canceled));
    assert!(!cancelpoll::seen(), "no kernel call runs: nothing would reset the mark");
    let held = AtomicU8::new(cancelpoll::ABORT_BIT);
    let mut s = unsafe { cancelpoll::step_on(held.as_ptr(), "held, outside a call") };
    assert_eq!(s.tick(), Err(Canceled));
    assert!(!cancelpoll::seen());
    let byte = AtomicU8::new(0);
    cancelpoll::scoped(&byte, || {
        let mut s = unsafe { cancelpoll::step_on(held.as_ptr(), "held, inside a call") };
        assert_eq!(s.tick(), Err(Canceled));
        assert!(!cancelpoll::seen(), "the call's own byte is quiet: the stop is the held byte's owner's");
        byte.store(cancelpoll::ABORT_BIT, Ordering::Release);
        assert_eq!(step("lent").tick(), Err(Canceled));
        assert!(cancelpoll::seen(), "a step on the lent byte marks its call's stop");
    });
    assert!(!cancelpoll::seen(), "the call's end clears its mark");
}

#[test]
fn each_yields_the_cancel_as_an_item_and_then_ends() {
    let byte = AtomicU8::new(0);
    cancelpoll::scoped(&byte, || {
        let mut s = step("each");
        let mut got = Vec::new();
        for r in s.each(0..10) {
            match r {
                Ok(i) => {
                    got.push(i);
                    if i == 3 {
                        byte.store(cancelpoll::ABORT_BIT, Ordering::Release);
                    }
                }
                Err(Canceled) => got.push(-1),
            }
        }
        assert_eq!(got, [0, 1, 2, 3, -1]);
    });
}

#[test]
fn pieces_cap_each_unit_and_stop_before_the_next() {
    let byte = AtomicU8::new(0);
    cancelpoll::scoped(&byte, || {
        let mut s = step("pieces");
        let mut seen = Vec::new();
        let r: Result<(), Canceled> = s.pieces(10, 4, |r| {
            seen.push(r);
            Ok(())
        });
        assert_eq!(r, Ok(()));
        assert_eq!(seen, [0..4, 4..8, 8..10]);
        seen.clear();
        let r: Result<(), Canceled> = s.pieces(10, 4, |r| {
            seen.push(r);
            byte.store(cancelpoll::ABORT_BIT, Ordering::Release);
            Ok(())
        });
        assert_eq!(r, Err(Canceled));
        assert_eq!(seen, [0..4]);
    });
}

#[test]
fn reserve_moves_a_growing_run_in_pieces_and_a_stop_leaves_it_as_it_was() {
    let byte = AtomicU8::new(0);
    cancelpoll::scoped(&byte, || {
        let mut s = step("reserve");
        // a run of 2.5 pieces with no spare room
        let mut v: Vec<u64> = (0..(5 * cancelpoll::PIECE / 16) as u64).collect();
        v.shrink_to_fit();
        let want = v.clone();
        assert_eq!(s.reserve(&mut v, 10), Ok(()));
        assert!(v.capacity() >= want.len() + 10);
        assert_eq!(v, want, "the run moved whole");
        // room enough: nothing moves, so a set abort bit is not read
        byte.store(cancelpoll::ABORT_BIT, Ordering::Release);
        let (cap, room) = (v.capacity(), v.capacity() - v.len());
        assert_eq!(s.reserve(&mut v, room), Ok(()));
        // a growth under the abort bit stops before its first piece: the run, its buffer and its room stay
        let at = v.as_ptr();
        assert_eq!(s.reserve(&mut v, cap), Err(Canceled));
        assert_eq!((v.as_ptr(), v.capacity()), (at, cap));
        assert_eq!(v, want);
    });
}

#[test]
fn batch_ticks_once_per_batch() {
    let byte = AtomicU8::new(cancelpoll::ABORT_BIT);
    cancelpoll::scoped(&byte, || {
        let mut s = step("batch");
        let stop = (0..3 * BATCH).position(|i| s.batch(i).is_err());
        assert_eq!(stop, Some(BATCH - 1));
    });
}

#[test]
fn sort_by_sorts_and_a_canceled_sort_keeps_every_element() {
    let n = 3 * SORT_RUN + 17;
    let input: Vec<u32> = (0..n as u32).map(|i| i.wrapping_mul(2654435761) % 100_003).collect();
    let mut want = input.clone();
    want.sort_unstable();
    let byte = AtomicU8::new(0);
    cancelpoll::scoped(&byte, || {
        let mut s = step("sort");
        let mut small: Vec<u32> = input[..100].to_vec();
        assert_eq!(sort_by(&mut s, &mut small, &mut [], |a, b| a.cmp(b)), Ok(()));
        let mut w = input[..100].to_vec();
        w.sort_unstable();
        assert_eq!(small, w);
        let (mut v, mut scratch) = (input.clone(), vec![0u32; n]);
        let calls = core::cell::Cell::new(0usize);
        let counted = |a: &u32, b: &u32| {
            calls.set(calls.get() + 1);
            a.cmp(b)
        };
        assert_eq!(sort_by(&mut s, &mut v, &mut scratch, counted), Ok(()));
        assert_eq!(v, want);
        // the last merge pass compares about n times: half of them before the end lands the cancel inside it
        let at = calls.replace(0) - n / 2;
        let mut v = input.clone();
        let r = sort_by(&mut s, &mut v, &mut scratch, |a, b| {
            if calls.replace(calls.get() + 1) == at {
                byte.store(cancelpoll::ABORT_BIT, Ordering::Release);
            }
            a.cmp(b)
        });
        assert_eq!(r, Err(Canceled), "the cancel lands in a merge pass");
        v.sort_unstable();
        assert_eq!(v, want, "a canceled sort leaves every element in v");
    });
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "outside the kernel call it was opened in")]
fn a_step_ticked_inside_another_kernel_call_is_a_bug() {
    let (outer, inner) = (AtomicU8::new(0), AtomicU8::new(0));
    cancelpoll::scoped(&outer, || {
        let mut s = step("outer");
        cancelpoll::scoped(&inner, || {
            let _ = s.tick();
        });
    });
}
