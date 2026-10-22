//! ppround (card 12, `PGRUST_POOL_FIT_TAIL`, memacct's condition (a)): a fitted ask that misses the base sub-zone (here the
//! lazy zone is too small for the sub-zone's first chunk, so every fitted ask misses) gets a huge-paged pool span held
//! whole, while the take hands out, and `usable` (door 1's size) says, the ask to the page. The tag's in-use bytes move by
//! that, the span's excess sits in `Stats::waste` (the reconcile's `pool_run_tails`), `fit_tail` counts the miss and its
//! live bytes, the span never resizes in place, and its give puts everything back.
#![cfg(target_os = "linux")]

mod common;
use common::*;
use pagepool::hooks;
use pagepool::*;

#[test]
fn a_missed_fit_names_its_excess() {
    init_with(|c| c.lazy_va = 64 * MIB);
    let page = hooks::page_size();
    let h = Hint::new(Class::Task, Tag::OTHER);
    hooks::span_fit_switch(true);
    hooks::fit_tail_switch(true);
    let in_use = || stats().in_use[Tag::OTHER.0 as usize];
    let ask = 5 * MIB + 1;
    let want = ask.next_multiple_of(page);
    let excess = 3 * SUPER - want;
    assert_eq!(usable(ask, 64), want, "door 1's size: the ask to the page");
    let (u0, w0) = (in_use(), stats().waste);
    let p = take(ask, 64, h).expect("a pool span");
    let a = p.cast::<u8>().as_ptr() as usize;
    assert_eq!(p.len(), want, "handed out to the page");
    assert!(matches!(run_of(a as *const u8).map(|i| i.kind), Some(Kind::Span { superpages: 3 })), "{:?}", run_of(a as *const u8));
    assert_eq!(in_use() - u0, want, "in_use: what door 1 charges");
    assert_eq!(stats().waste - w0, excess, "the excess: named in waste");
    assert_eq!(fit_tail(), (true, 1, excess, 0));
    #[cfg(feature = "global")]
    assert_eq!(usable_size(a as *const u8), want, "usable_size by pointer: what it handed out, less the excess");
    balanced();
    touch(a, want);
    // a missed span moves on any resize (the caller copies)
    // SAFETY: ours, `ask` bytes.
    assert!(unsafe { resize(p.cast(), ask, ask + 4096, 64) }.is_none(), "no grow in place");
    // SAFETY: as above.
    assert!(unsafe { resize(p.cast(), ask, ask - 600_000, 64) }.is_none(), "no shrink in place");
    assert_eq!((in_use() - u0, stats().waste - w0), (want, excess), "a refused resize moves nothing");
    // a whole-superpage ask is no fit and no miss: a pool span, its size
    let whole = take(4 * MIB, 64, h).expect("a pool span");
    assert_eq!(whole.len(), 4 * MIB);
    // SAFETY: ours, freed once.
    unsafe { give(whole.cast(), 4 * MIB) };
    // SAFETY: ours, freed once.
    unsafe { give(p.cast(), ask) };
    flush_shelf();
    assert_eq!((in_use(), stats().waste), (u0, w0), "given: in_use and waste back");
    assert_eq!(fit_tail(), (true, 1, 0, 0));
    purge_now(usize::MAX, None);
    balanced();
}
