//! ppround (card 12, `PGRUST_POOL_FIT_TAIL`) with `PGRUST_POOL_FENCE=1`, under `PoolAlloc` as this binary's global
//! allocator: `usable_size` by pointer is what the block handed out (`good_size`), a fitted span's commit and not its
//! superpages, so a copy of that length (`cache_realloc`, a C realloc) reads only the pages its owner had: the copy faults
//! the new span's pages and none of the old span's uncommitted tail. A lazy span (past `pool_lazy_min`, fenced) is its
//! superpages, as before.
#![cfg(all(feature = "global", target_os = "linux"))]

use core::alloc::{GlobalAlloc, Layout};
use pagepool::PoolAlloc;

#[global_allocator]
static A: PoolAlloc = PoolAlloc;

const MIB: usize = 1 << 20;
const SUPER: usize = 2 * MIB;

fn minflt() -> i64 {
    // SAFETY: a zeroed rusage the call fills.
    let mut r: libc::rusage = unsafe { core::mem::zeroed() };
    // SAFETY: `r` is valid for the call to write.
    assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_THREAD, &mut r) }, 0);
    r.ru_minflt as i64
}

#[test]
#[cfg_attr(pagepool_tsan, ignore = "TSan: its shadow memory faults too, so the minor-fault counts run high")]
fn a_fitted_spans_usable_size_is_its_commit() {
    pagepool::hooks::fence_switch(true);
    pagepool::hooks::span_fit_switch(true);
    pagepool::hooks::fit_tail_switch(true);
    let page = pagepool::hooks::page_size();
    let (n0, n1) = (5 * MIB + 1, 9 * MIB + 1);
    let want = n0.next_multiple_of(page);

    // the global road
    let l0 = Layout::from_size_align(n0, 8).unwrap();
    // SAFETY: a valid layout; freed below.
    let p = unsafe { A.alloc(l0) };
    assert!(!p.is_null() && pagepool::owns(p));
    assert_eq!((pagepool::usable_size(p), pagepool::good_size(n0, 8)), (want, want), "usable_size: the commit, as good_size");
    // SAFETY: our block of layout l0, freed once.
    unsafe { A.dealloc(p, l0) };

    // the cache road: the realloc copies usable_size's bytes, and faults only the new span's pages
    let p = pagepool::cache_alloc(n0, 64, false);
    assert!(!p.is_null());
    assert_eq!(pagepool::usable_size(p), want);
    // SAFETY: `n0` bytes of our block.
    unsafe { core::ptr::write_bytes(p, 0x6c, n0) };
    let f0 = minflt();
    let q = pagepool::cache_realloc(p, 64, n1);
    let faults = minflt() - f0;
    assert!(!q.is_null() && q != p, "moved: past its superpages");
    // SAFETY: the first n0 bytes were copied.
    assert!(unsafe { core::slice::from_raw_parts(q, n0) }.iter().all(|&b| b == 0x6c), "the bytes moved with it");
    let tail = (3 * SUPER - want) / page;
    assert!(faults < (want / page + tail / 2) as i64, "the copy faulted {faults} pages: the commit is {} pages, the old tail {tail}", want / page);
    assert_eq!(pagepool::usable_size(q), n1.next_multiple_of(page));
    // SAFETY: our live cache block, freed once.
    unsafe { pagepool::give_unsized(core::ptr::NonNull::new(q).unwrap()) };

    // a lazy span (fenced: the slot past it PROT_NONE): its superpages, `good_size` too; a realloc copies them, inside it
    let big = 40 * MIB + 1;
    let p = pagepool::cache_alloc(big, 64, false);
    assert!(!p.is_null());
    assert_eq!((pagepool::usable_size(p), pagepool::good_size(big, 64)), (42 * MIB, 42 * MIB));
    // SAFETY: `big` bytes of our block.
    unsafe { core::ptr::write_bytes(p, 0x2e, big) };
    let q = pagepool::cache_realloc(p, 64, 45 * MIB + 1);
    assert!(!q.is_null() && q != p, "moved: past its superpages");
    // SAFETY: the first `big` bytes were copied.
    assert!(unsafe { core::slice::from_raw_parts(q, big) }.iter().all(|&b| b == 0x2e));
    // SAFETY: our live cache block, freed once.
    unsafe { pagepool::give_unsized(core::ptr::NonNull::new(q).unwrap()) };
    let (on, misses, miss_bytes, unnamed) = pagepool::fit_tail();
    assert!(on && misses == 0 && miss_bytes == 0 && unnamed == 0);
}
