//! The case of tests/idle_tails_held.rs and tests/idle_tails_held_budgeted.rs (pppress card 10, memacct's): the purger
//! never makes the splits an idle cut asks for (held, as if it never ran), so every pass leaves its PMD-mapped tails and
//! TAIL_AGAIN runs out. The idle period's cut ends with those tails still resident and counted where they were (the small
//! tier's in-use bytes, which the reconcile names in pool_small_slack), no byte counted given back. A later idle period,
//! after the purger's splits, gives them back, contents of the kept blocks intact throughout. Each test is a binary of its
//! own with `PoolAlloc` as its global allocator.

use core::alloc::{GlobalAlloc, Layout};
use pagepool::{PoolAlloc, Tag};
use std::collections::BTreeMap;

const RUN: usize = 64 << 10;
const SIZE: usize = 768;
const PAGE: usize = 4096;

fn fill(p: usize, v: u8) {
    // SAFETY: a live block of SIZE bytes the test holds.
    unsafe { core::ptr::write_bytes(p as *mut u8, v, SIZE) };
}

fn holds(p: usize, v: u8) -> bool {
    // SAFETY: as `fill`.
    unsafe { core::slice::from_raw_parts(p as *const u8, SIZE) }.iter().all(|&b| b == v)
}

fn thp_host() -> bool {
    std::fs::read_to_string("/sys/kernel/mm/transparent_hugepage/enabled").is_ok_and(|s| !s.contains("[never]"))
}

/// pswpin + pswpout so far (a page on swap is not resident, so a residency read is not exact once either moved).
fn swapped() -> u64 {
    let v = std::fs::read_to_string("/proc/vmstat").unwrap_or_default();
    v.lines().filter(|l| l.starts_with("pswpin ") || l.starts_with("pswpout ")).filter_map(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok()).sum()
}

/// The pool's threads (pg:poolclaim, pg:poolpurge) take their own small runs when they first run, which on a loaded host
/// comes after this test's first in-use read (precheck 20: +2 runs, 128 KiB). Wait until both have run and sleep and the
/// small tier's in-use bytes hold still, so the in-use reads below see this thread's runs only.
fn settle_pool_threads() {
    let small = || pagepool::stats().in_use[Tag::SMALL.0 as usize];
    let parked = || {
        let tasks = std::fs::read_dir("/proc/self/task").into_iter().flatten().flatten();
        tasks
            .filter(|t| {
                let p = t.path();
                let comm = std::fs::read_to_string(p.join("comm")).unwrap_or_default();
                let stat = std::fs::read_to_string(p.join("stat")).unwrap_or_default();
                let ran = std::fs::read_to_string(p.join("schedstat")).ok().and_then(|s| s.split_whitespace().next()?.parse::<u64>().ok()).is_none_or(|ns| ns > 0);
                matches!(comm.trim(), "pg:poolclaim" | "pg:poolpurge") && ran && stat.rsplit_once(')').is_some_and(|(_, r)| r.trim_start().starts_with('S'))
            })
            .count()
            == 2
    };
    let (mut last, mut still) = (small(), 0);
    for _ in 0..500 {
        std::thread::sleep(std::time::Duration::from_millis(10));
        let now = small();
        still = if now == last && parked() { still + 1 } else { 0 };
        last = now;
        if still >= 5 {
            return;
        }
    }
    eprintln!("NOTE: the pool's threads did not settle in 5 s: the in-use reads below may count their runs");
}

/// Calls of one idle period (`slice_ms` apart, each resuming), until the cut says nothing is left: how many.
fn idle_period(slice_ms: u64) -> u32 {
    let mut n = 0u32;
    loop {
        let (_, more) = pagepool::idle_tails(n > 0);
        n += 1;
        assert!(n < 1000, "the cut ends");
        if !more {
            return n;
        }
        if slice_ms == 0 {
            std::thread::yield_now();
        } else {
            std::thread::sleep(std::time::Duration::from_millis(slice_ms));
        }
    }
}

/// The case at an idle cut budget of `budget` ns (`hooks::idle_tail_budget`; 0: one run a call).
pub fn run(budget: u64) {
    if !thp_host() {
        eprintln!("SKIP: no THP on this host (no tail is PMD-mapped, nothing is left to the purger)");
        return;
    }
    std::thread::spawn(move || {
        pagepool::set_thp_partial(2);
        assert!(pagepool::start(), "the pool's threads run");
        pagepool::hooks::idle_tail_switch(true);
        // u64::MAX: no budget stops, whatever the host's load; 0: every pass is cut at each run after its first, so its
        // TAIL_AGAIN reruns ride on the deferrals its resumes carry
        pagepool::hooks::idle_tail_budget(budget);
        let sw0 = swapped();
        settle_pool_threads();
        let l = Layout::from_size_align(SIZE, 8).unwrap();
        // SAFETY: a non-zero layout.
        let blocks: Vec<usize> = (0..400).map(|_| unsafe { PoolAlloc.alloc(l) } as usize).collect();
        let mut kept = Vec::new();
        for (i, &p) in blocks.iter().enumerate() {
            assert!(p != 0 && pagepool::owns(p as *const u8));
            fill(p, i as u8);
        }
        for (i, &p) in blocks.iter().enumerate() {
            if i % 40 == 0 {
                kept.push((i, p));
            } else {
                // SAFETY: ours, freed once.
                unsafe { PoolAlloc.dealloc(p as *mut u8, l) };
            }
        }
        // per run, the pages past its last kept block
        let mut hi: BTreeMap<usize, usize> = BTreeMap::new();
        for &(_, p) in &kept {
            let e = hi.entry(p & !(RUN - 1)).or_default();
            *e = (*e).max((p + SIZE).next_multiple_of(PAGE));
        }
        let tails: Vec<(usize, usize)> = hi.into_iter().filter(|&(run, keep)| keep < run + RUN).map(|(run, keep)| (keep, run + RUN - keep)).collect();
        let mapped = |a: usize, n: usize| pagepool::residency_seam(a, n, false).map(|(r, _, _)| r).expect("a reading");
        let resident = || tails.iter().map(|&(a, n)| mapped(a, n)).sum::<usize>();
        let small_in_use = || pagepool::stats().in_use[Tag::SMALL.0 as usize];

        // the purger holds its splits: every pass leaves the PMD-mapped tails, TAIL_AGAIN runs out, the cut ends
        pagepool::hooks::purger_split_hold(true);
        let before = resident();
        let (in_use0, (_, _, bytes0, d0, s0)) = (small_in_use(), pagepool::idle_tail_counts());
        let stops0 = pagepool::idle_tail_times().3;
        // at budget 0 a pass is one call per run: a timer slice apart, the held period would outlast the purger's age
        // pass, which in mode 2 splits the test's superpage itself, so its calls follow at once
        let slice = if budget == 0 { 0 } else { 10 };
        let split0 = pagepool::thp_partial_counts().3;
        let calls = idle_period(slice);
        let stops = pagepool::idle_tail_times().3 - stops0;
        let (_, runs1, bytes1, d1, s1) = pagepool::idle_tail_counts();
        if d1 == d0 {
            pagepool::hooks::purger_split_hold(false);
            eprintln!("SKIP: no tail was PMD-mapped here (THP fell back), nothing was left to the purger");
            return;
        }
        assert_eq!(s1, s0, "held: the purger made no split");
        assert_eq!(u64::from(calls) - stops, 1 + 4, "a pass that left tails ran again TAIL_AGAIN (4) times, then the cut ended: {calls} calls, {stops} budget stops");
        let left = resident();
        assert_eq!(small_in_use(), in_use0, "and counted where they were: the small tier's in-use bytes (pool_small_slack)");
        assert!(kept.iter().all(|&(i, p)| holds(p, i as u8)), "kept blocks read as written");
        let still = left > 0 && left + (bytes1 - bytes0) as usize >= before;
        if !still && swapped() != sw0 {
            pagepool::hooks::purger_split_hold(false);
            eprintln!("SKIP: the host swapped during the test (pswpin / pswpout moved): the deferred tails' residency is not exact here: {left} of {before}");
            return;
        }
        let splits = pagepool::thp_partial_counts().3 - split0;
        if !still && splits > 0 {
            pagepool::hooks::purger_split_hold(false);
            eprintln!("SKIP: {splits} split(s) the case did not ask for (the purger's own age pass in mode 2) freed the tails while the asked ones were held: {left} of {before} resident");
            return;
        }
        assert!(still, "the deferred tails are still resident: {left} of {before}, {} given", bytes1 - bytes0);

        // the purger splits; a later idle period gives the tails back
        pagepool::hooks::purger_split_hold(false);
        let mut n = 0;
        while pagepool::idle_tail_counts().4 == s1 {
            n += 1;
            assert!(n < 500, "the purger makes the held splits");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
        idle_period(slice);
        let (_, runs2, bytes2, ..) = pagepool::idle_tail_counts();
        assert!(runs2 > runs1 && bytes2 > bytes1, "the later idle period gave tails back: runs {runs1} -> {runs2}, bytes {bytes1} -> {bytes2}");
        assert!(resident() < left, "fewer tail pages resident: {} of {left}", resident());
        assert_eq!(small_in_use(), in_use0, "in-use bytes unchanged: a give-back moves no owner's count");
        assert!(kept.iter().all(|&(i, p)| holds(p, i as u8)), "kept blocks read as written after the give-back");
        for &(_, p) in &kept {
            // SAFETY: ours, freed once.
            unsafe { PoolAlloc.dealloc(p as *mut u8, l) };
        }
        pagepool::hooks::idle_tail_budget(500_000);
        pagepool::release_heap();
        pagepool::flush_shelf();
    })
    .join()
    .unwrap();
}
