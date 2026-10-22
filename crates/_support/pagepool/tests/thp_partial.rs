//! D4760 (`pool_thp_partial`): a give-back of part of a superpage the pool advised huge, while the superpage is in use.
//! A MADV_DONTNEED there leaves the 2 MiB folio charged until the kernel splits it; the modes:
//! - 1 (keep): the free runs and dead runs of superpages in use stay held, resident, reusable (a take claims nothing new)
//!   and out of `releasable`; whole dead superpages still go back; `give_back_any` gives back only whole superpages and
//!   zeroes the parts (or, with none inside, refuses);
//! - 2 (split first): the same free runs go back, a MADV_COLD on one page of each superpage first, and read zero after.
//! One test: the mode is process-wide.
#![cfg(target_os = "linux")]

mod common;
use common::*;
use core::ptr::NonNull;
use pagepool::hooks::{self, Op};
use pagepool::*;
use std::collections::HashSet;

const RUN: usize = 64 << 10;
const BLOCK: usize = 4 * RUN;
const MADV_COLD: i32 = 20;

fn thp_host() -> bool {
    std::fs::read_to_string("/sys/kernel/mm/transparent_hugepage/enabled").is_ok_and(|s| !s.contains("[never]"))
}

fn fill(p: usize, len: usize, v: u8) {
    // SAFETY: inside a block or span the test holds.
    unsafe { core::ptr::write_bytes(p as *mut u8, v, len) };
}

fn all(p: usize, len: usize, v: u8) -> bool {
    // SAFETY: as above.
    unsafe { core::slice::from_raw_parts(p as *const u8, len) }.iter().all(|&b| b == v)
}

fn vmstat(key: &str) -> u64 {
    let v = std::fs::read_to_string("/proc/vmstat").unwrap_or_default();
    v.lines().find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix(' '))).and_then(|n| n.trim().parse().ok()).unwrap_or(0)
}

/// Blocks of 4 runs, the first per superpage kept, the rest freed: free runs inside superpages in use.
fn free_runs_in_used_superpages(h: Hint) -> (Vec<NonNull<u8>>, Vec<usize>, usize) {
    let mut kept = Vec::new();
    let mut freed = Vec::new();
    let mut seen = HashSet::new();
    for _ in 0..128 {
        let b = take(BLOCK, 64, h).expect("a runs block").cast::<u8>();
        let p = b.as_ptr() as usize;
        fill(p, BLOCK, 0x5a);
        if seen.insert(p / SUPER) { kept.push(b) } else { freed.push(p) }
    }
    for &p in &freed {
        // SAFETY: ours, freed once.
        unsafe { give(NonNull::new(p as *mut u8).unwrap(), BLOCK) };
    }
    flush_shelf();
    (kept, freed, seen.len())
}

#[test]
fn thp_partial_modes() {
    if !thp_host() {
        eprintln!("SKIP: no THP on this host (the modes act as 0)");
        return;
    }
    setup();
    let h = Hint::new(Class::Task, Tag::OTHER);

    // mode 1: the free runs of superpages in use stay
    set_thp_partial(1);
    let (kept, freed, _) = free_runs_in_used_superpages(h);
    let want = freed.len() * BLOCK;
    assert!(releasable() < want, "mode 1: the free runs of superpages in use are not releasable ({} >= {want})", releasable());
    hooks::clear();
    let (before, k0) = (stats(), thp_partial_counts());
    purge_now(usize::MAX, None);
    let after = stats();
    let mut partial = 0;
    assert!(hooks::each(|op, _, len, _| partial += (op == Op::GiveBack && len < SUPER) as usize), "the syscall log overflowed");
    assert_eq!(partial, 0, "mode 1: no give-back of part of a superpage");
    assert_eq!(after.claimed, before.claimed, "mode 1: nothing went back: {before:?} -> {after:?}");
    for &p in &freed {
        assert_eq!(resident(p, BLOCK), BLOCK, "mode 1: a free run of a superpage in use stays resident");
    }
    let k1 = thp_partial_counts();
    assert!(k1.0 == 1 && k1.1 > k0.1 && k1.2 > k0.2, "mode 1 counted its keeps: {k0:?} -> {k1:?}");
    let again: Vec<_> = (0..freed.len()).map(|_| take(BLOCK, 64, h).expect("a runs block again")).collect();
    assert_eq!(stats().claimed, after.claimed, "mode 1: the kept runs serve the takes again, no new claim");
    for b in again {
        // SAFETY: ours, freed once.
        unsafe { give(b.cast(), BLOCK) };
    }
    flush_shelf();
    balanced();

    // mode 1, dead runs: the dead runs of a block in one superpage stay; a span's whole dead superpages go back
    let p = take(16 * RUN, RUN, h).unwrap().cast::<u8>();
    let a = p.as_ptr() as usize;
    fill(a, 16 * RUN, 0xab);
    let s = span(8 * MIB, h).unwrap();
    let sa = s.as_ptr() as usize;
    fill(sa, 8 * MIB, 0xcd);
    let dead0 = stats().dead;
    // SAFETY: inside our block and span.
    unsafe {
        give_pages(p, 5 * RUN, 7 * RUN);
        give_pages(NonNull::new(s.as_ptr()).unwrap(), 2 * MIB, 4 * MIB);
    }
    set_pressure(Pressure::Floor);
    purge_now(usize::MAX, None);
    set_pressure(Pressure::None);
    assert_eq!(stats().dead - dead0, 7 * RUN, "mode 1: the dead runs inside a superpage in use stay dead, held");
    assert_eq!(resident(a + 5 * RUN, 7 * RUN), 7 * RUN, "mode 1: and resident");
    assert_eq!(resident(sa + 2 * MIB, 4 * MIB), 0, "mode 1: whole dead superpages still go back");
    // SAFETY: the ranges given above.
    unsafe {
        assert_eq!(take_pages(p, 5 * RUN, 7 * RUN), Zeroed::No, "kept: the bytes are the owner's still");
        assert_eq!(take_pages(NonNull::new(s.as_ptr()).unwrap(), 2 * MIB, 4 * MIB), Zeroed::Yes);
    }
    assert!(all(a, 16 * RUN, 0xab) && all(sa + 2 * MIB, 4 * MIB, 0) && all(sa, 2 * MIB, 0xcd));

    // mode 1, give_back_any over the pool's own span: whole superpages back, the parts zeroed; none inside: refused
    fill(sa, 8 * MIB, 0xcd);
    let (lo, end) = (sa + RUN, sa + 6 * MIB - RUN);
    // SAFETY: inside our span, its bytes dead to the test.
    assert_eq!(unsafe { give_back_any(lo, end - lo, true) }, 0);
    // residency first: a read maps the zero page, which mincore counts
    assert_eq!(resident(sa + 2 * MIB, 2 * MIB), 0, "the whole superpage inside went back");
    assert!(resident(lo, 2 * MIB - RUN) > 0 && resident(sa + 4 * MIB, 2 * MIB - RUN) > 0, "the parts stayed mapped");
    assert!(all(lo, end - lo, 0), "the range reads zero after");
    assert!(all(sa, RUN, 0xcd) && all(end, sa + 8 * MIB - end, 0xcd), "outside the range untouched");
    fill(sa, 8 * MIB, 0xcd);
    // SAFETY: as above.
    assert_eq!(unsafe { give_back_any(sa + RUN, MIB, true) }, -1, "no whole superpage inside: refused");
    assert!(all(sa, 8 * MIB, 0xcd), "refused: nothing changed");

    // mode 2: the same free runs go back, each superpage's folio split first
    set_thp_partial(2);
    let (kept2, freed2, supers2) = free_runs_in_used_superpages(h);
    let want2 = freed2.len() * BLOCK;
    hooks::clear();
    let (s0, split0) = (thp_partial_counts(), vmstat("thp_split_page"));
    let got = purge_now(usize::MAX, None);
    assert!(got >= want2, "mode 2: released {got} < the freed runs' {want2}");
    for &p in &freed2 {
        assert_eq!(resident(p, BLOCK), 0, "mode 2: a free run's pages left the process");
    }
    let mut cold = 0;
    assert!(hooks::each(|op, _, _, arg| cold += (op == Op::Madvise && arg == MADV_COLD) as usize), "the syscall log overflowed");
    let s1 = thp_partial_counts();
    assert!(cold >= supers2 && s1.3 >= s0.3 + supers2 as u64, "mode 2: a split asked per superpage: {cold} calls, {s0:?} -> {s1:?}");
    eprintln!("mode 2: {supers2} superpages, {cold} MADV_COLD, host thp_split_page +{}", vmstat("thp_split_page") - split0);
    let again: Vec<NonNull<u8>> = (0..freed2.len()).map(|_| take(BLOCK, 64, h).expect("a runs block again").cast::<u8>()).collect();
    for b in &again {
        let q = b.as_ptr() as usize;
        assert!(all(q, BLOCK, 0), "mode 2: a run taken again from the OS reads zero");
    }

    set_thp_partial(0);
    // SAFETY: ours, each freed once.
    unsafe {
        for b in again.into_iter().chain(kept2).chain(kept) {
            give(b, BLOCK);
        }
        give(p, 16 * RUN);
        give_span(s);
    }
    flush_shelf();
    balanced();
}
