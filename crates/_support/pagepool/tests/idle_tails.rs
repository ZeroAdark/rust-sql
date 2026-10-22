//! pppress card 10 (`PGRUST_POOL_IDLE_TAIL`): a session's idle release gives back each small run it owns past the run's
//! last used block. Off, nothing goes; on, those pages read not resident, the kept blocks read as written, a block
//! another thread freed before the cut counts as home, and the runs take again (re-linking the given pages) with every
//! block distinct. With a time budget of 0 each call cuts one run and says runs are left, the next going on from there,
//! until the last. With D4760's split on and the purger running, a tail still PMD-mapped waits for the purger's split
//! and goes on a later call. A binary of its own: `PoolAlloc` is its global allocator.
#![cfg(all(feature = "global", target_os = "linux"))]

mod common;
use common::{swapped_out, whole_but_swap};
use core::alloc::{GlobalAlloc, Layout};
use pagepool::PoolAlloc;
use std::collections::BTreeMap;

#[global_allocator]
static A: PoolAlloc = PoolAlloc;

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

#[test]
fn idle_tails() {
    std::thread::spawn(|| {
        let sw = swapped_out();
        let l = Layout::from_size_align(SIZE, 8).unwrap();
        // SAFETY: a non-zero layout.
        let blocks: Vec<usize> = (0..200).map(|_| unsafe { A.alloc(l) } as usize).collect();
        let mut runs: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (i, &p) in blocks.iter().enumerate() {
            assert!(p != 0 && pagepool::owns(p as *const u8));
            fill(p, i as u8);
            runs.entry(p & !(RUN - 1)).or_default().push(p);
        }
        assert!(runs.len() >= 3, "200 blocks of {SIZE} B span 3 runs: {}", runs.len());
        let tag = |p: usize| blocks.iter().position(|&b| b == p).unwrap() as u8;
        // per run: its 3 lowest blocks kept, one other freed by another thread, the rest here
        let (mut kept, mut remote, mut local) = (Vec::new(), Vec::new(), Vec::new());
        for v in runs.values_mut() {
            v.sort();
            kept.extend_from_slice(&v[..3.min(v.len())]);
            if v.len() > 4 {
                remote.push(v[v.len() - 1]);
                local.extend_from_slice(&v[3..v.len() - 1]);
            }
        }
        for &p in &local {
            // SAFETY: ours, freed once.
            unsafe { A.dealloc(p as *mut u8, l) };
        }
        std::thread::spawn(move || {
            for p in remote {
                // SAFETY: handed over, freed once.
                unsafe { A.dealloc(p as *mut u8, l) };
            }
        })
        .join()
        .unwrap();
        let tails = |kept: &[usize]| -> Vec<(usize, usize)> {
            let mut hi: BTreeMap<usize, usize> = BTreeMap::new();
            for &p in kept {
                let e = hi.entry(p & !(RUN - 1)).or_default();
                *e = (*e).max((p + SIZE).next_multiple_of(PAGE));
            }
            hi.into_iter().map(|(run, keep)| (keep, run + RUN - keep)).collect()
        };
        let cut = tails(&kept);

        pagepool::hooks::idle_tail_switch(false);
        assert_eq!(pagepool::idle_tails(false), (0, false), "off: nothing goes");
        let mapped = |a: usize, n: usize| pagepool::residency_seam(a, n, false).map(|(r, z, _)| (r, z)).expect("a reading");
        let before: usize = cut.iter().map(|&(a, n)| mapped(a, n).0).sum();
        assert!(before > 0, "the freed blocks' pages were resident");

        pagepool::hooks::idle_tail_switch(true);
        // a budget of 0: one run a call, so the cut goes on over calls (the idle timer's slices), each from where the
        // last one stopped
        pagepool::hooks::idle_tail_budget(0);
        let (_, runs0, bytes0, ..) = pagepool::idle_tail_counts();
        let (calls0, _, _, stops0, _) = pagepool::idle_tail_times();
        let (mut gone, mut n) = (0, 0u64);
        loop {
            let (g, more) = pagepool::idle_tails(n > 0);
            (gone, n) = (gone + g, n + 1);
            assert!(n < 10_000, "the cut ends");
            if !more {
                break;
            }
        }
        pagepool::hooks::idle_tail_budget(500_000);
        let (on, runs1, bytes1, ..) = pagepool::idle_tail_counts();
        let (calls1, ns, max_ns, stops1, hist) = pagepool::idle_tail_times();
        assert!(calls1 == calls0 + n && max_ns > 0 && ns >= max_ns && hist.iter().sum::<u64>() == calls1, "timed: {calls1} {ns} {max_ns} {hist:?}");
        assert!(n >= cut.len() as u64 && stops1 - stops0 == n - 1, "one run a call: {n} calls for {} runs with a tail", cut.len());
        // a page swapped out between the two reads is given back but not counted resident
        assert!(on && (gone >= before || whole_but_swap(gone, before, sw)) && runs1 > runs0 && bytes1 - bytes0 == gone as u64, "gone {gone} before {before}");
        for &(a, n) in &cut {
            assert_eq!(mapped(a, n), (0, 0), "the run past its last used block's page is given back: {a:#x}+{n}");
        }
        assert!(kept.iter().all(|&p| holds(p, tag(p))), "kept blocks read as written");
        assert_eq!(pagepool::idle_tails(false).0, 0, "a second cut finds nothing past the kept pages");

        // SAFETY: a non-zero layout.
        let again: Vec<usize> = (0..300).map(|_| unsafe { A.alloc(l) } as usize).collect();
        let mut all: Vec<usize> = kept.iter().chain(again.iter()).copied().collect();
        all.sort();
        assert!(all.windows(2).all(|w| w[0] + SIZE <= w[1]), "blocks are distinct");
        for (i, &p) in again.iter().enumerate() {
            fill(p, 0x80 | i as u8);
        }
        assert!(kept.iter().all(|&p| holds(p, tag(p))), "kept blocks unchanged by the takes after the cut");
        assert!(again.iter().enumerate().all(|(i, &p)| holds(p, 0x80 | i as u8)));
        for &p in kept.iter().chain(again.iter()) {
            // SAFETY: ours, freed once.
            unsafe { A.dealloc(p as *mut u8, l) };
        }

        // D4760's split on and the purger running: a tail in a superpage still PMD-mapped is left for a later call while
        // the purger splits its folio; the calls go on (a slice apart) until every tail is given back, contents intact
        let mode = pagepool::thp_partial_counts().0;
        pagepool::set_thp_partial(2);
        assert!(pagepool::start(), "the pool's threads run");
        // SAFETY: a non-zero layout.
        let more: Vec<usize> = (0..400).map(|_| unsafe { A.alloc(l) } as usize).collect();
        for (i, &p) in more.iter().enumerate() {
            fill(p, i as u8);
        }
        let (held, gone_too): (Vec<(usize, usize)>, Vec<usize>) = {
            let (mut h, mut g) = (Vec::new(), Vec::new());
            for (i, &p) in more.iter().enumerate() {
                if i % 40 == 0 {
                    h.push((i, p))
                } else {
                    g.push(p)
                }
            }
            (h, g)
        };
        for &p in &gone_too {
            // SAFETY: ours, freed once.
            unsafe { A.dealloc(p as *mut u8, l) };
        }
        let (_, _, _, d0, s0) = pagepool::idle_tail_counts();
        let mut n = 0u32;
        loop {
            let (_, more_left) = pagepool::idle_tails(n > 0);
            n += 1;
            assert!(n < 1000, "the cut ends");
            if !more_left {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let (_, _, _, d1, s1) = pagepool::idle_tail_counts();
        assert!(d1 == d0 || s1 > s0, "a tail left for the purger had its split made: deferred {} splits {}", d1 - d0, s1 - s0);
        assert!(held.iter().all(|&(i, p)| holds(p, i as u8)), "kept blocks read as written across the deferred cut");
        for &(_, p) in &held {
            // SAFETY: ours, freed once.
            unsafe { A.dealloc(p as *mut u8, l) };
        }
        pagepool::set_thp_partial(mode);
        pagepool::release_heap();
        pagepool::flush_shelf();
    })
    .join()
    .unwrap();
}
