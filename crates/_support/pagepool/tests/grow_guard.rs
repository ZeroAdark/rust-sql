//! pppress card 6 (`PGRUST_POOL_GROW_GUARD`): RELEASE BEFORE GROW. While the ladder presses the pool (its last sample
//! left no room, `set_claim_room(0)`), a thread that claims pool charge gives the pool's own free memory back inline at
//! the take's settle point, up to what it claimed; with room, or the switch off, nothing goes back.
#![cfg(target_os = "linux")]

mod common;
use common::*;
use core::ptr::NonNull;
use pagepool::hooks;
use pagepool::*;

const RUN: usize = 64 << 10;
/// Past `LIST_RUNS` (8): a block given back goes straight to its superpage's free runs.
const BLOCK: usize = 9 * RUN;

/// A fresh superpage of three touched `BLOCK` blocks, two given back: 18 touched free runs. The blocks kept (that
/// superpage's third and the others taken on the way), the superpage's base and the two given back.
fn free_runs(h: Hint) -> (Vec<NonNull<[u8]>>, usize, Vec<usize>) {
    let mut by_super: std::collections::BTreeMap<usize, Vec<NonNull<[u8]>>> = Default::default();
    let sp = loop {
        let b = take(BLOCK, 64, h).expect("a runs block");
        let p = b.cast::<u8>().as_ptr() as usize;
        touch(p, BLOCK);
        let v = by_super.entry(p / SUPER).or_default();
        v.push(b);
        if v.len() == 3 {
            break p / SUPER;
        }
    };
    let mut kept = Vec::new();
    let mut freed = Vec::new();
    for (s, blocks) in by_super {
        for (j, b) in blocks.into_iter().enumerate() {
            if s == sp && j > 0 {
                freed.push(b.cast::<u8>().as_ptr() as usize);
                // SAFETY: ours, freed once.
                unsafe { give(b.cast(), BLOCK) };
            } else {
                kept.push(b);
            }
        }
    }
    flush_shelf();
    (kept, sp * SUPER, freed)
}

/// The superpages (by base) a give-back of part of one touched since `hooks::clear`.
fn given_back_in() -> std::collections::BTreeSet<usize> {
    let mut sps = std::collections::BTreeSet::new();
    let complete = hooks::each(|op, at, len, _| {
        if op == hooks::Op::GiveBack && len < SUPER {
            sps.insert(at / SUPER * SUPER);
        }
    });
    assert!(complete, "the syscall log overflowed");
    sps
}

/// A lazy span's commit claims; with `on` and no room, the 18 free runs go back at its settle point.
fn grow(on: bool, room: usize) {
    hooks::grow_guard_switch(on);
    let h = Hint::new(Class::Task, Tag::OTHER);
    let (kept, sp, freed) = free_runs(h);
    set_claim_room(room);
    let g0 = guard_counts();
    hooks::clear();
    let len = 64 * MIB;
    let s = take(len, SUPER, Hint::new(Class::Process, Tag::REGION)).expect("a lazy span");
    let g1 = guard_counts();
    set_claim_room(usize::MAX);
    let sps = given_back_in();
    if on && room == 0 {
        assert_eq!(g1.releases + g1.skips, g0.releases + g0.skips + 1, "one settle: {g0:?} -> {g1:?}");
        if g1.releases > g0.releases {
            assert!(g1.bytes - g0.bytes >= (18 * RUN) as u64, "the free runs went back: {g0:?} -> {g1:?}");
            assert!(sps.contains(&sp), "the free runs' superpage gave back: {sps:x?} ({sp:x})");
            assert_eq!(g1.bins.iter().sum::<u64>(), g1.releases, "every release in one bin");
            assert!(g1.max_ns >= g1.ns - g0.ns || g0.releases > 0);
            for &p in &freed {
                assert_eq!(resident(p, BLOCK), 0, "the free runs' pages left the process");
            }
        }
    } else {
        assert_eq!((g1.releases, g1.skips, g1.bytes), (g0.releases, g0.skips, g0.bytes), "off or with room: nothing");
        assert!(!sps.contains(&sp), "nothing given back: {sps:x?} ({sp:x})");
    }
    // SAFETY: ours, not used again.
    unsafe { give(s.cast(), len) };
    for b in kept {
        // SAFETY: ours, freed once.
        unsafe { give(b.cast(), BLOCK) };
    }
    flush_shelf();
    purge_now(usize::MAX, None);
    balanced();
}

#[test]
fn grow_guard() {
    setup();
    grow(false, 0);
    grow(true, usize::MAX);
    grow(true, 0);
    hooks::grow_guard_switch(false);
    assert!(!guard_counts().on);
}
