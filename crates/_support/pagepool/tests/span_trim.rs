//! D4787 (`PGRUST_POOL_SPAN_TRIM`): a warm lazy span taken again keeps only the committed pages its ask expects
//! (`Hint::expect_kib`, else its length). The pages past them go back to the OS before the new owner has the span (none
//! resident there, by remaps only: never a MADV_DONTNEED over a whole 2 MiB range), the counts come down by them, and
//! what stays reads zero. Among warm spans the one with the least to give back is taken first. With the switch off a
//! span comes back whole, as before.

mod common;
use common::*;
use pagepool::hooks::{self, Op};
use pagepool::*;

const ADVICE_DONTNEED: i32 = 4;

fn h() -> Hint {
    Hint::new(Class::Task, Tag::PROGRAM).zero(Zero::Dense)
}

fn program(s: &Stats) -> usize {
    s.in_use[Tag::PROGRAM.0 as usize]
}

/// A live lazy span of `len` with its first `c` bytes committed and written (0xab).
fn written(len: usize, c: usize) -> Span {
    let mut s = span(len, h()).unwrap();
    assert!(s.is_lazy());
    // SAFETY: ours.
    unsafe {
        commit_to(&mut s, c);
        core::ptr::write_bytes(s.as_ptr(), 0xab, c);
    }
    s
}

fn zero(s: &Span, len: usize) -> bool {
    // SAFETY: the span's committed pages, ours.
    unsafe { core::slice::from_raw_parts(s.as_ptr(), len) }.iter().all(|&b| b == 0)
}

#[test]
fn span_trim() {
    setup();
    hooks::span_trim_switch(true);
    trims_to_the_expected_pages();
    takes_the_least_to_trim();
    keeps_a_commit_inside_the_ask();
    hooks::span_trim_switch(false);
    off_the_span_comes_back_whole();
    purge_now(usize::MAX >> 1, None);
    flush_shelf();
    balanced();
}

fn trims_to_the_expected_pages() {
    let sw = swapped_out();
    let a = written(64 * MIB, 60 * MIB);
    let base = a.as_ptr() as usize;
    // SAFETY: ours, not used again.
    unsafe { give_span(a) };
    let (s0, r0) = (stats(), span_reuse());
    assert!(r0.trim_on);
    hooks::clear();
    // a 48 MiB ask (24 slots) that expects to touch 3 MiB: the 32-slot warm span, 4 MiB of it kept (whole superpages)
    let b = span(48 * MIB, h().expect_kib(3 << 10)).unwrap();
    assert_eq!(b.as_ptr() as usize, base, "the warm span came back");
    assert_eq!(b.committed(), 4 * MIB, "its commit cut to the ask's expected pages, in whole superpages");
    let (s1, r1) = (stats(), span_reuse());
    assert_eq!(s0.lazy_committed - s1.lazy_committed, 56 * MIB, "the lazy commit down by the pages given back");
    assert_eq!(s0.claimed - s1.claimed, 56 * MIB, "and the pool's claim");
    assert_eq!(program(&s1) - program(&s0), 4 * MIB, "the new owner's tag holds what stays");
    assert_eq!((r1.trims - r0.trims, r1.trimmed - r0.trimmed), (1, 56 * MIB as u64));
    assert!(
        whole_but_swap((r1.trim_resident - r0.trim_resident) as usize, 56 * MIB, sw),
        "every page past the kept ones was resident and went back"
    );
    assert_eq!(resident(base + 4 * MIB, 56 * MIB), 0, "none of them resident now");
    assert_eq!((r1.zero_takes - r0.zero_takes, r1.zero_bytes - r0.zero_bytes), (1, 4 * MIB as u64), "only the kept pages zeroed");
    assert!(zero(&b, 4 * MIB), "the kept pages read zero");
    let mut bad = Vec::new();
    let complete = hooks::each(|op, at, len, arg| {
        if matches!(op, Op::Madvise) && arg == ADVICE_DONTNEED && at.next_multiple_of(SUPER) + SUPER <= at + len {
            bad.push((at, len));
        }
    });
    assert!(complete && bad.is_empty(), "CVE-2026-74674: a MADV_DONTNEED over a whole 2 MiB range: {bad:x?}");
    // SAFETY: ours, not used again.
    unsafe { give_span(b) };
    purge_now(usize::MAX >> 1, None);
}

fn takes_the_least_to_trim() {
    let x = written(64 * MIB, 40 * MIB);
    let y = written(64 * MIB, 8 * MIB);
    let yb = y.as_ptr() as usize;
    // SAFETY: ours, not used again.
    unsafe {
        give_span(x);
        give_span(y);
    }
    let r0 = span_reuse();
    let z = span(48 * MIB, h().expect_kib(4 << 10)).unwrap();
    assert_eq!(z.as_ptr() as usize, yb, "of two warm spans with the expected pages, the one with the least past them");
    assert_eq!(z.committed(), 4 * MIB);
    assert_eq!(span_reuse().trimmed - r0.trimmed, 4 * MIB as u64);
    // SAFETY: ours, not used again.
    unsafe { give_span(z) };
    purge_now(usize::MAX >> 1, None);
}

fn keeps_a_commit_inside_the_ask() {
    let w = written(64 * MIB, 30 * MIB);
    let base = w.as_ptr() as usize;
    // SAFETY: ours, not used again.
    unsafe { give_span(w) };
    let r0 = span_reuse();
    // no expectation: the ask's length (48 MiB) is the bound, and the 30 MiB commit is inside it
    let v = span(48 * MIB, h()).unwrap();
    assert_eq!(v.as_ptr() as usize, base);
    assert_eq!(v.committed(), 30 * MIB, "nothing past the ask: nothing given back");
    assert_eq!(span_reuse().trims, r0.trims);
    assert!(zero(&v, 30 * MIB), "the warm pages read zero");
    // SAFETY: ours, not used again.
    unsafe { give_span(v) };
    purge_now(usize::MAX >> 1, None);
}

fn off_the_span_comes_back_whole() {
    let u = written(64 * MIB, 60 * MIB);
    let base = u.as_ptr() as usize;
    // SAFETY: ours, not used again.
    unsafe { give_span(u) };
    let r0 = span_reuse();
    assert!(!r0.trim_on);
    let t = span(48 * MIB, h().expect_kib(4 << 10)).unwrap();
    assert_eq!(t.as_ptr() as usize, base);
    assert_eq!(t.committed(), 60 * MIB, "switch off: every committed page handed over");
    assert_eq!(span_reuse().trims, r0.trims);
    assert!(zero(&t, 60 * MIB), "and zeroed");
    // SAFETY: ours, not used again.
    unsafe { give_span(t) };
}
