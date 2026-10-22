//! ppround (card 12, `PGRUST_POOL_FIT_TAIL`): with `PGRUST_POOL_SPAN_FIT`, a span ask served on base pages commits and
//! reports (`usable`, door 1's size) its ask rounded to the page, not its whole superpages: the pool's in_use moves by
//! that much, a realloc grows it in place up to its superpages (committing the new size), a grow past them or a shrink
//! under the commit moves (None), and freed it goes back balanced. A whole-superpage ask stays a pool span, and with the
//! switch off a fitted span commits all its superpages, as before.
#![cfg(target_os = "linux")]

mod common;
use common::*;
use core::ptr::NonNull;
use pagepool::hooks;
use pagepool::*;

fn in_use() -> usize {
    stats().in_use[Tag::OTHER.0 as usize]
}

#[test]
fn fit_tail_commits_the_page() {
    setup();
    let page = hooks::page_size();
    let h = Hint::new(Class::Task, Tag::OTHER);
    hooks::span_fit_switch(true);

    // on: 5 MiB + 1 is 3 superpages of address space, committed and reported to the page
    hooks::fit_tail_switch(true);
    let ask = 5 * MIB + 1;
    let want = ask.next_multiple_of(page);
    assert_eq!(usable(ask, 64), want, "usable: the ask to the page");
    let u0 = in_use();
    let p = take(ask, 64, h).expect("a base span");
    assert_eq!(p.len(), want, "take hands out what usable says");
    let a = p.cast::<u8>().as_ptr() as usize;
    assert!(matches!(run_of(a as *const u8).map(|i| i.kind), Some(Kind::Span { superpages: 3 })), "{:?}", run_of(a as *const u8));
    assert_eq!(in_use() - u0, want, "in_use: the commit, not 6 MiB");
    #[cfg(feature = "global")]
    assert_eq!(usable_size(a as *const u8), want, "usable_size by pointer: the commit");
    touch(a, ask);
    assert!(resident(a + want, 3 * SUPER - want) == 0, "the tail past the commit is untouched");

    // a realloc within its superpages: in place, the commit follows
    let grow = ask + 300_000;
    // SAFETY: ours, `ask` bytes, taken above.
    let q = unsafe { resize(p.cast(), ask, grow, 64) }.expect("in place within the superpages");
    assert_eq!(q.cast::<u8>().as_ptr() as usize, a);
    assert_eq!(q.len(), grow.next_multiple_of(page));
    assert_eq!(in_use() - u0, grow.next_multiple_of(page), "in_use follows the grow");
    // past its superpages, or under its commit: moved (the caller copies)
    // SAFETY: as above, `grow` bytes now.
    assert!(unsafe { resize(NonNull::new(a as *mut u8).unwrap(), grow, 7 * MIB, 64) }.is_none(), "a base span does not grow past its superpages");
    // SAFETY: as above.
    assert!(unsafe { resize(NonNull::new(a as *mut u8).unwrap(), grow, ask - 600_000, 64) }.is_none(), "a shrink under the commit moves");
    assert_eq!(in_use() - u0, grow.next_multiple_of(page), "a refused resize leaves the commit");
    // SAFETY: ours, freed once.
    unsafe { give(NonNull::new(a as *mut u8).unwrap(), grow) };
    flush_shelf();
    assert_eq!(in_use(), u0, "freed: in_use back");

    // memacct's condition (b): a base span is never re-taken warm. Given dirty, its slots wait for the purger's whole
    // give-back (no carve takes a dirty slot), so a smaller ask after it never finds pages resident past its commit,
    // before the purge (other slots) or after it (the cleaned slots)
    let p = take(ask, 64, h).expect("a base span");
    let a = p.cast::<u8>().as_ptr() as usize;
    touch(a, p.len());
    // SAFETY: ours, freed once.
    unsafe { give(p.cast(), ask) };
    for purge in [false, true] {
        if purge {
            purge_now(usize::MAX, None);
        }
        let small = 4 * MIB + 1;
        let q = take(small, 64, h).expect("a base span");
        let b = q.cast::<u8>().as_ptr() as usize;
        assert_eq!(q.len(), small.next_multiple_of(page));
        assert!(matches!(run_of(b as *const u8).map(|i| i.kind), Some(Kind::Span { superpages: 3 })));
        assert_eq!(resident(b + q.len(), 3 * SUPER - q.len()), 0, "pages past the commit resident (purged: {purge})");
        if !purge {
            assert_ne!(b, a, "a dirty base span's slots are not re-taken before the purge");
        }
        // SAFETY: ours, freed once.
        unsafe { give(q.cast(), small) };
    }
    flush_shelf();
    assert_eq!(in_use(), u0, "freed: in_use back");

    // a span the ask fills whole stays a whole pool span
    let whole = 4 * MIB;
    assert_eq!(usable(whole, 64), whole);
    let p = take(whole, 64, h).expect("a pool span");
    assert_eq!(p.len(), whole);
    // SAFETY: ours, freed once.
    unsafe { give(p.cast(), whole) };

    // off: a fitted span commits and reports all its superpages
    hooks::fit_tail_switch(false);
    assert_eq!(usable(ask, 64), 6 * MIB);
    let u0 = in_use();
    let p = take(ask, 64, h).expect("a base span");
    assert_eq!(p.len(), 6 * MIB);
    assert_eq!(in_use() - u0, 6 * MIB);
    // SAFETY: ours, freed once.
    unsafe { give(p.cast(), ask) };
    flush_shelf();
    assert_eq!(in_use(), u0);
    assert_eq!(fit_tail(), (false, 0, 0, 0));

    purge_now(usize::MAX, None);
    lazy_pass_now();
    balanced();
}
