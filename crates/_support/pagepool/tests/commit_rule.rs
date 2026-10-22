//! pppress card 13 (2), the ONE rule with ppround's FIT_TAIL: a lazy span commits (the pool's in_use and claim) and
//! hands out what `usable` reports for its ask, the size door 1 charges. A warm lazy span taken again for a shorter ask
//! (it may be up to twice the ask) commits only the ask's superpages, a grow in place or past its slots and a shrink
//! commit what `usable` says for the new size, and given back, in_use returns to its start. With D4787's trim off, a
//! warm commit already past the ask stays committed and handed out (D4787 is what gives those pages back), and
//! `Stats::commit_past_ask` (the view's 'pool_commit_past_ask') names it; with the trim on it stays 0.

mod common;
use common::*;
use pagepool::hooks;
use pagepool::*;

fn h() -> Hint {
    Hint::new(Class::Task, Tag::PROGRAM)
}

fn in_use() -> usize {
    stats().in_use[Tag::PROGRAM.0 as usize]
}

/// The tag's live commit past its takes' asks (the view's 'pool_commit_past_ask').
fn past() -> usize {
    stats().commit_past_ask[Tag::PROGRAM.0 as usize]
}

/// A warm lazy span of `len`, its first `c` bytes committed and written: its start.
fn warm(len: usize, c: usize) -> usize {
    let mut s = span(len, h()).unwrap();
    assert!(s.is_lazy());
    // SAFETY: ours.
    unsafe {
        commit_to(&mut s, c);
        core::ptr::write_bytes(s.as_ptr(), 0xab, c);
    }
    let a = s.as_ptr() as usize;
    // SAFETY: ours, not used again.
    unsafe { give_span(s) };
    a
}

#[test]
fn commit_rule() {
    setup();
    for trim in [true, false] {
        hooks::span_trim_switch(trim);
        let u0 = in_use();
        let a = warm(64 * MIB, 10 * MIB);
        assert_eq!(in_use(), u0, "a warm span is no one's");
        let ask = 40 * MIB;
        assert_eq!(usable(ask, 64), ask);
        let p = take(ask, 64, h()).expect("a lazy span");
        assert_eq!(p.cast::<u8>().as_ptr() as usize, a, "the 64 MiB warm span came back for a 40 MiB ask (trim {trim})");
        assert_eq!(p.len(), ask, "it hands out what usable says (trim {trim})");
        assert_eq!(in_use() - u0, ask, "in_use: the ask's superpages, not the span's 64 MiB (trim {trim})");
        assert_eq!(past(), 0, "a warm commit under the ask leaves nothing past it (trim {trim})");
        #[cfg(feature = "global")]
        assert_eq!(usable_size(a as *const u8), ask, "usable_size by pointer: the commit (trim {trim})");
        // a grow inside its slots: in place, committed to what usable says
        // SAFETY: ours, `ask` bytes.
        let q = unsafe { resize(p.cast(), ask, 50 * MIB, 64) }.expect("in place");
        assert_eq!(q.cast::<u8>().as_ptr() as usize, a);
        assert_eq!((q.len(), in_use() - u0), (50 * MIB, 50 * MIB), "a grow commits what usable says (trim {trim})");
        // past its slots: grown (maybe moved), committed to the new size's superpages
        // SAFETY: ours, 50 MiB now.
        let r = unsafe { resize(q.cast(), 50 * MIB, 70 * MIB + 1, 64) }.expect("grown");
        assert_eq!((r.len(), in_use() - u0), (72 * MIB, 72 * MIB), "a grow past the slots (trim {trim})");
        // a shrink: its superpages past the new size dropped, the commit with them
        // SAFETY: ours, 70 MiB + 1 now.
        let r = unsafe { resize(r.cast(), 70 * MIB + 1, 36 * MIB, 64) }.expect("shrunk in place");
        assert_eq!((r.len(), in_use() - u0), (36 * MIB, 36 * MIB), "a shrink (trim {trim})");
        // SAFETY: ours, freed once.
        unsafe { give(r.cast(), 36 * MIB) };
        flush_shelf();
        assert_eq!(in_use(), u0, "given back: in_use returns to its start (trim {trim})");
        purge_now(usize::MAX >> 1, None);
    }
    // trim on: a warm commit past the ask goes back at the take (D4787), so nothing is committed past the ask
    hooks::span_trim_switch(true);
    let u0 = in_use();
    let a = warm(64 * MIB, 60 * MIB);
    let p = take(40 * MIB, 64, h()).expect("a lazy span");
    assert_eq!(p.cast::<u8>().as_ptr() as usize, a);
    assert_eq!((p.len(), in_use() - u0, past()), (40 * MIB, 40 * MIB, 0), "trim on: the ask alone, nothing past it");
    // SAFETY: ours, freed once.
    unsafe { give(p.cast(), 40 * MIB) };
    flush_shelf();
    assert_eq!((in_use(), past()), (u0, 0));
    purge_now(usize::MAX >> 1, None);
    // trim off: a warm commit past the ask stays, handed out and counted, named in commit_past_ask, and goes back with
    // the span
    hooks::span_trim_switch(false);
    let u0 = in_use();
    let a = warm(64 * MIB, 60 * MIB);
    let p = take(40 * MIB, 64, h()).expect("a lazy span");
    assert_eq!(p.cast::<u8>().as_ptr() as usize, a);
    assert_eq!((p.len(), in_use() - u0), (60 * MIB, 60 * MIB), "trim off: the warm commit stays");
    assert_eq!(past(), 20 * MIB, "trim off: the 20 MiB committed past the 40 MiB ask is named");
    // SAFETY: ours, freed once.
    unsafe { give(p.cast(), 40 * MIB) };
    flush_shelf();
    assert_eq!((in_use(), past()), (u0, 0), "given back: in_use returns to its start, nothing past an ask");
    // trim off, a shrink drops the superpages past the new size and the commit past the ask with them
    let a = warm(64 * MIB, 60 * MIB);
    let p = take(40 * MIB, 64, h()).expect("a lazy span");
    assert_eq!((p.cast::<u8>().as_ptr() as usize, past()), (a, 20 * MIB));
    // SAFETY: ours, 40 MiB asked.
    let q = unsafe { resize(p.cast(), 40 * MIB, 50 * MIB, 64) }.expect("in place");
    assert_eq!((q.len(), in_use() - u0, past()), (50 * MIB, 50 * MIB, 0), "a resize hands out what usable says, nothing past it");
    // SAFETY: ours, freed once.
    unsafe { give(q.cast(), 50 * MIB) };
    flush_shelf();
    assert_eq!((in_use(), past()), (u0, 0));
    purge_now(usize::MAX >> 1, None);
    lazy_pass_now();
    balanced();
}
