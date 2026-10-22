//! pppress (`PGRUST_POOL_SPAN_FIT`): a span take under `pool_lazy_min` whose last superpage the ask fills only in part is
//! served on base pages: a touch of its last superpage makes one page resident, not 2 MiB; a span the ask fills whole,
//! and every span with the switch off, stays a huge-paged pool span. Freed, the base span goes back balanced.
#![cfg(target_os = "linux")]

mod common;
use common::*;
use pagepool::hooks;
use pagepool::*;

fn thp_host() -> bool {
    std::fs::read_to_string("/sys/kernel/mm/transparent_hugepage/enabled").is_ok_and(|s| !s.contains("[never]"))
}

fn one(on: bool, bytes: usize) -> (bool, usize, usize) {
    hooks::span_fit_switch(on);
    let h = Hint::new(Class::Task, Tag::OTHER);
    let (_, f0, t0) = span_fits();
    let p = take(bytes, 64, h).expect("a span");
    let a = p.cast::<u8>().as_ptr() as usize;
    assert_eq!(p.len(), bytes.next_multiple_of(SUPER), "the span reads whole superpages long");
    // the ask's last byte, touched: its superpage's residency
    let last = a + bytes - 1;
    // SAFETY: inside the block we hold.
    unsafe { core::ptr::write_volatile(last as *mut u8, 7) };
    let r = resident(last & !(SUPER - 1), SUPER);
    let (_, f1, t1) = span_fits();
    let lazy = !matches!(run_of(a as *const u8).map(|i| i.kind), Some(Kind::Span { .. })) || f1 > f0;
    // SAFETY: ours, freed once.
    unsafe { give(p.cast(), bytes) };
    flush_shelf();
    assert_eq!((t1 - t0) as usize, if f1 > f0 { bytes.next_multiple_of(SUPER) - bytes } else { 0 });
    (f1 > f0 && lazy, r, (f1 - f0) as usize)
}

#[test]
fn span_fit() {
    // SPAN_FIT alone: PGRUST_POOL_FIT_TAIL (default ON on the branch since the user's defaults) reports a fitted
    // span's ask rounded to the page instead of whole superpages, so it is pinned off here, before the pool's first
    // span (read once); tests/fit_tail.rs covers the two together
    hooks::fit_tail_switch(false);
    setup();
    let page = hooks::page_size();
    // 5 MiB + 1: 3 superpages, ~1 MiB unused past the ask
    let (fit, r, _) = one(true, 5 * MIB + 1);
    assert!(fit, "on: a partly filled last superpage sends the span to base pages");
    assert!(r >= page && r < SUPER, "on: the touched page resident in the last superpage, not all 2 MiB: {r}");
    // a span the ask fills whole stays a pool span
    let (fit, _, n) = one(true, 4 * MIB);
    assert!(!fit && n == 0, "on: a whole-superpage ask stays a pool span");
    // off: as before
    let (fit, r, _) = one(false, 5 * MIB + 1);
    assert!(!fit, "off: a pool span");
    if thp_host() {
        assert!(r >= page, "off: the touch faulted (a huge page where the host gives one: {r})");
    }
    purge_now(usize::MAX, None);
    lazy_pass_now();
    balanced();
}
