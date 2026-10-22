//! pppress card 16 (65 via b6): `pool_thp_partial` 3 (split cold only) is cold-only for the AGE passes alone. A pressure
//! ask never waits for a superpage to go cold. R1's release (`return_warm_now`, the order the purger serves an R1 ask in:
//! `serve_ask`) and the plain purge (`purge_now`, a `set_pressure` ask's) give back the dead runs and the free runs of
//! a HOT superpage (one used since the last age mark), split first as mode 2, with the claim room open (no ladder press,
//! so the age passes would hold them). Its own binary: the mode is process-wide.
#![cfg(target_os = "linux")]

mod common;
use common::*;
use core::ptr::NonNull;
use pagepool::hooks::{self, Op};
use pagepool::*;
use std::collections::HashSet;

const RUN: usize = 64 << 10;
/// Past `LIST_RUNS` (8): a block taken comes from its superpage's free runs and one given goes straight back to them.
const BLOCK: usize = 9 * RUN;
const MADV_COLD: i32 = 20;
/// The dead runs given in each kept block: `[RUN, 5 * RUN)`.
const DEAD_AT: usize = RUN;
const DEAD_LEN: usize = 4 * RUN;

fn thp_host() -> bool {
    std::fs::read_to_string("/sys/kernel/mm/transparent_hugepage/enabled").is_ok_and(|s| !s.contains("[never]"))
}

/// Blocks of 9 runs, the first per superpage kept and written, the rest freed: superpages in use, with free runs.
fn used_superpages(h: Hint) -> Vec<NonNull<u8>> {
    let mut kept = Vec::new();
    let mut freed = Vec::new();
    let mut seen = HashSet::new();
    for _ in 0..24 {
        let b = take(BLOCK, 64, h).expect("a runs block").cast::<u8>();
        let p = b.as_ptr() as usize;
        // SAFETY: ours.
        unsafe { core::ptr::write_bytes(p as *mut u8, 0x5a, BLOCK) };
        if seen.insert(p / SUPER) { kept.push(b) } else { freed.push(b) }
    }
    for b in freed {
        // SAFETY: ours, freed once.
        unsafe { give(b, BLOCK) };
    }
    flush_shelf();
    kept
}

/// Dead runs in every kept block. `give_pages` marks the superpage used (`state::note_active`): hot until the next age mark.
fn hot_dead(kept: &[NonNull<u8>]) -> usize {
    for &b in kept {
        // SAFETY: inside our own block; nothing reads the range until `take_pages`.
        unsafe { give_pages(b, DEAD_AT, DEAD_LEN) };
    }
    kept.len() * DEAD_LEN
}

fn cold_splits() -> usize {
    let mut n = 0;
    let _ = hooks::each(|op, _, _, advice| {
        if op == Op::Madvise && advice == MADV_COLD {
            n += 1;
        }
    });
    n
}

fn give_all(kept: Vec<NonNull<u8>>) {
    for b in kept {
        // SAFETY: inside our own block, given dead above: live again before the give.
        let _ = unsafe { take_pages(b, DEAD_AT, DEAD_LEN) };
        // SAFETY: ours, freed once.
        unsafe { give(b, BLOCK) };
    }
    flush_shelf();
}

#[test]
fn a_pressure_ask_in_mode_3_releases_hot_superpages_dead_runs() {
    init_with(|c| c.lazy_decay_ms = 60_000);
    if !thp_host() {
        eprintln!("SKIP: no THP on this host (mode 3 reads as 0)");
        return;
    }
    set_thp_partial(3);
    // not pressed: an age pass now would hold every hot superpage's runs (`state::piece_hot`)
    set_claim_room(usize::MAX);
    let h = Hint::new(Class::Task, Tag::OTHER);

    // R1: the purger's order for an R1 ask, on this thread
    let kept = used_superpages(h);
    assert!(kept.len() >= 2, "blocks in at least two superpages");
    let (d0, r0, s0) = (stats().dead, stats().released_bytes, thp_hot_skips());
    let dead = hot_dead(&kept);
    assert_eq!(stats().dead, d0 + dead, "the hot superpages' dead runs counted");
    assert!(releasable() >= dead, "R1 sizes its ask with them: releasable {} < dead {dead}", releasable());
    hooks::clear();
    let got = return_warm_now(usize::MAX);
    assert!(got >= dead, "R1 released {got} B, under the hot dead runs' {dead} B");
    assert!(stats().dead <= d0, "no dead run left held: {} > {d0}", stats().dead);
    assert!(stats().released_bytes >= r0 + dead as u64, "released: {} -> {}", r0, stats().released_bytes);
    assert!(cold_splits() > 0, "split first, as mode 2");
    assert_eq!(thp_hot_skips(), s0, "a pressure ask skips nothing hot");
    give_all(kept);
    purge_now(usize::MAX, None);

    // the plain purge (a `set_pressure` ask the purger serves, the cap's and the ladder's): the same
    let kept = used_superpages(h);
    let (d1, s1) = (stats().dead, thp_hot_skips());
    let dead = hot_dead(&kept);
    hooks::clear();
    let got = purge_now(usize::MAX, None);
    assert!(got >= dead, "the purge released {got} B, under the hot dead runs' {dead} B");
    assert!(stats().dead <= d1, "no dead run left held: {} > {d1}", stats().dead);
    assert!(cold_splits() > 0, "split first, as mode 2");
    assert_eq!(thp_hot_skips(), s1, "a pressure ask skips nothing hot");
    give_all(kept);
    purge_now(usize::MAX, None);
    set_thp_partial(2);
    balanced();
}
