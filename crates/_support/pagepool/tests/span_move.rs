//! pppress card 7 (`PGRUST_POOL_SPAN_MOVE`): a lazy span's commit takes its whole untouched superpages from the pool's
//! free warm ones by remap: they read zero (a dirty one zeroed first), are resident without a fault, claim nothing new,
//! and leave their pool superpages Cold; with the switch off the commit faults fresh as before. A span with moved pieces
//! still grows by remap where the kernel moves several mappings.
#![cfg(target_os = "linux")]

mod common;
use common::*;
use pagepool::hooks;
use pagepool::*;

fn h() -> Hint {
    Hint::new(Class::Process, Tag::REGION)
}

/// Four free warm superpages: a touched pool span of four given back, its pages dirty.
fn warm_four() {
    let s = span(4 * SUPER, Hint::new(Class::Task, Tag::PROGRAM)).expect("a pool span");
    assert!(!s.is_lazy());
    // SAFETY: ours.
    unsafe { core::ptr::write_bytes(s.as_ptr(), 0xa5, 4 * SUPER) };
    // SAFETY: ours, not used again.
    unsafe { give_span(s) };
}

fn commit(on: bool) {
    hooks::span_move_switch(on);
    warm_four();
    let warm0 = stats().warm_free();
    let m0 = span_moves();
    let c0 = stats().claimed;
    let len = 64 * MIB;
    let mut s = span(len, h()).expect("a lazy span");
    assert!(s.is_lazy());
    hooks::clear();
    // SAFETY: ours, nothing touched yet.
    unsafe { commit_to(&mut s, len) };
    let m1 = span_moves();
    let base = s.as_ptr() as usize;
    let moved = (m1.bytes - m0.bytes) as usize;
    if on {
        assert!(warm0 >= 4 * SUPER, "four warm superpages to move: {warm0}");
        assert_eq!(m1.moves, m0.moves + 1, "one commit moved");
        assert!(moved >= 4 * SUPER && moved <= len, "the warm superpages moved in: {moved}");
        assert!(m1.zeroed - m0.zeroed >= 4 * SUPER as u64, "the dirty ones zeroed first");
        assert_eq!(resident(base, moved), moved, "moved pages are resident with no fault");
        assert_eq!(stats().warm_free(), warm0 - moved, "warm free drops by exactly the bytes moved");
        assert_eq!(stats().claimed, c0 + len - moved, "moved pages claim nothing new");
        let mut remaps = 0;
        let _ = hooks::each(|op, at, l, _| {
            if op == hooks::Op::Mremap && at % SUPER == 0 && l == SUPER {
                remaps += 1;
            }
        });
        assert_eq!(remaps * SUPER, moved, "one remap per superpage moved");
    } else {
        assert_eq!(m1, m0, "off: nothing moved");
        assert_eq!(stats().claimed, c0 + len);
        assert_eq!(resident(base, len), 0, "off: nothing resident before a touch");
    }
    // every page of the commit reads zero, moved or fresh
    for k in 0..len / SUPER {
        for off in [0, SUPER / 2, SUPER - 1] {
            // SAFETY: inside our committed span.
            assert_eq!(unsafe { (base as *const u8).add(k * SUPER + off).read_volatile() }, 0, "zero at {k}:{off}");
        }
    }
    // the span's own bytes, then a grow by remap where the neighbour is taken
    for k in 0..len / SUPER {
        // SAFETY: inside our committed span.
        unsafe { (base as *mut u8).add(k * SUPER).write(k as u8 + 1) };
    }
    let other = span(64 * MIB, h()).expect("the neighbour");
    // SAFETY: ours.
    let grown = unsafe { grow_span(&mut s, 2 * len) };
    if grown {
        // SAFETY: ours, moved with its pages.
        for k in 0..len / SUPER {
            assert_eq!(unsafe { s.as_ptr().add(k * SUPER).read() }, k as u8 + 1, "piece {k} moved with the span");
        }
    } else {
        eprintln!("SKIP grow by remap refused (moves on: {on}; the kernel's refusals {} -> {})", m1.grow_refused, span_moves().grow_refused);
    }
    // SAFETY: ours, not used again.
    unsafe {
        give_span(s);
        give_span(other);
    }
    flush_shelf();
    purge_now(usize::MAX, None);
    balanced();
}

/// 9d's witness: a move the map budget refuses leaves claimed and warm free as they were (the commit faults fresh).
fn refused() {
    hooks::span_move_switch(true);
    warm_four();
    let (warm0, c0, m0) = (stats().warm_free(), stats().claimed, span_moves());
    hooks::frame_budget(0);
    let len = 8 * MIB;
    let mut s = span(len + 32 * MIB, h()).expect("a lazy span");
    // SAFETY: ours, nothing touched yet.
    unsafe { commit_to(&mut s, len) };
    let m1 = span_moves();
    assert_eq!((m1.moves, m1.bytes), (m0.moves, m0.bytes), "refused: nothing moved");
    assert!(m1.refused > m0.refused, "the refusal counted");
    assert_eq!(stats().warm_free(), warm0, "refused: warm free unchanged");
    assert_eq!(stats().claimed, c0 + len, "refused: the commit claims as a fresh fault");
    hooks::frame_budget(usize::MAX);
    // SAFETY: ours, not used again.
    unsafe { give_span(s) };
    flush_shelf();
    purge_now(usize::MAX, None);
    balanced();
}

#[test]
fn span_move() {
    setup();
    commit(false);
    commit(true);
    refused();
    hooks::span_move_switch(false);
    assert!(!span_moves().on);
}
