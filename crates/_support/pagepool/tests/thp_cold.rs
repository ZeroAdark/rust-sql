//! pppress card 8 (`pool_thp_partial` 3, split only cold superpages): the age pass gives back the free runs of a
//! superpage in use only once a whole decay passed with nothing taken from or given back to it (split first, as mode 2);
//! a superpage used since the last mark keeps them held (counted, `thp_hot_skips`) until a quiet decay; with the ladder
//! pressing (no claim room) the pass does not wait. Its own binary: the mode and the decay are process-wide.
#![cfg(target_os = "linux")]

mod common;
use common::*;
use core::ptr::NonNull;
use pagepool::hooks::{self, Op};
use pagepool::*;
use std::collections::HashSet;

const RUN: usize = 64 << 10;
/// Past `LIST_RUNS` (8): a block taken comes from its superpage's free runs and one given goes straight back to them
/// (a shorter one sits on the shelves and central lists, which neither touches the superpage).
const BLOCK: usize = 9 * RUN;
const MADV_COLD: i32 = 20;

fn thp_host() -> bool {
    std::fs::read_to_string("/sys/kernel/mm/transparent_hugepage/enabled").is_ok_and(|s| !s.contains("[never]"))
}

/// Blocks of 9 runs, the first per superpage kept, the rest freed: free runs inside superpages in use.
fn free_runs_in_used_superpages(h: Hint) -> Vec<NonNull<u8>> {
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

fn cold_splits() -> usize {
    let mut n = 0;
    let _ = hooks::each(|op, _, _, advice| {
        if op == Op::Madvise && advice == MADV_COLD {
            n += 1;
        }
    });
    n
}

#[test]
fn thp_cold_only() {
    init_with(|c| c.lazy_decay_ms = 60_000);
    if !thp_host() {
        eprintln!("SKIP: no THP on this host (mode 3 reads as 0)");
        return;
    }
    set_thp_partial(3);
    let h = Hint::new(Class::Task, Tag::OTHER);
    let kept = free_runs_in_used_superpages(h);
    // the mark sees the free runs (none seen before: nothing released) and opens a decay
    let r0 = stats().released_bytes;
    lazy_pass_now();
    assert_eq!(stats().released_bytes, r0, "the first mark only sees");
    // one superpage used within the decay: a block taken and given back there
    let b = take(BLOCK, 64, h).expect("a block").cast::<u8>();
    // SAFETY: ours, freed once.
    unsafe { give(b, BLOCK) };
    flush_shelf();
    let (s0, r1) = (thp_hot_skips(), stats().released_bytes);
    hooks::clear();
    lazy_pass_now();
    assert!(thp_hot_skips() > s0, "the used superpage kept its free runs");
    assert!(stats().released_bytes > r1, "the quiet ones gave theirs back");
    assert!(cold_splits() > 0, "split first, as mode 2");
    // a whole decay with no use: the last one goes too
    let (s1, r2) = (thp_hot_skips(), stats().released_bytes);
    lazy_pass_now();
    assert_eq!(thp_hot_skips(), s1, "nothing hot any more");
    assert!(stats().released_bytes > r2, "the once-hot superpage's free runs went back");
    for b in kept {
        // SAFETY: ours, freed once.
        unsafe { give(b, BLOCK) };
    }
    flush_shelf();
    purge_now(usize::MAX, None);
    // pressed: a superpage used within the decay does not wait
    set_claim_room(0);
    let kept = free_runs_in_used_superpages(h);
    lazy_pass_now();
    let b = take(BLOCK, 64, h).expect("a block").cast::<u8>();
    // SAFETY: ours, freed once.
    unsafe { give(b, BLOCK) };
    flush_shelf();
    let (s2, r3) = (thp_hot_skips(), stats().released_bytes);
    lazy_pass_now();
    assert_eq!(thp_hot_skips(), s2, "pressed: no hot skip");
    assert!(stats().released_bytes > r3, "pressed: the free runs go back at once");
    set_claim_room(usize::MAX);
    for b in kept {
        // SAFETY: ours, freed once.
        unsafe { give(b, BLOCK) };
    }
    flush_shelf();
    purge_now(usize::MAX, None);
    // a lazy span used in mode 3 (its commit marks its pieces active) is no base span: given, it goes warm as any lazy
    // span and the purge gives it back (the activity bit once shared the slot word's BASE bit: the give went to the
    // base sub-zone's dirty list, which no clean reaches, and its claim stayed)
    let (d0, c0) = (hooks::base_dirty(), stats().lazy_committed);
    let len = 64 * MIB;
    let s = take(len, SUPER, Hint::new(Class::Process, Tag::REGION)).expect("a lazy span").cast::<u8>();
    // SAFETY: ours.
    unsafe { core::ptr::write_bytes(s.as_ptr(), 0x5a, len) };
    // SAFETY: inside our own span: a piece given and taken back, both uses
    unsafe { give_pages(s, 8 * MIB, 4 * MIB) };
    // SAFETY: as above.
    let _ = unsafe { take_pages(s, 8 * MIB, 4 * MIB) };
    // SAFETY: ours, freed once.
    unsafe { give(s, len) };
    flush_shelf();
    assert_eq!(hooks::base_dirty(), d0, "a used lazy span's give is no base give");
    purge_now(usize::MAX, None);
    assert_eq!(stats().lazy_committed, c0, "the used lazy span's commit went back");
    set_thp_partial(2);
    balanced();
}
