//! D4760 (pppress, `PGRUST_POOL_FAST_RELEASE`): an R1 serve takes the emptiest superpages' free runs first (off: the
//! fullest first, as before), finds dead runs below the lazy bump with the switch on, and R1's clock counts each step,
//! each `return_warm_now` and each ask the purger served.
#![cfg(target_os = "linux")]

mod common;
use common::*;
use core::ptr::NonNull;
use pagepool::hooks;
use pagepool::*;

const RUN: usize = 64 << 10;
/// Past `LIST_RUNS` (8): a block given back goes straight to its superpage's free runs. Three fill 27 of 32 runs.
const BLOCK: usize = 9 * RUN;

/// Two fresh superpages of three `BLOCK` blocks: in the first two blocks given back (9 runs used, 23 free: bin 3), the
/// second kept whole (27 used, 5 free: bin 1, the fullest). The blocks kept and the two superpages' addresses.
fn two_superpages(h: Hint) -> (Vec<NonNull<[u8]>>, usize, usize) {
    let mut by_super: std::collections::BTreeMap<usize, Vec<NonNull<[u8]>>> = Default::default();
    while by_super.values().filter(|v| v.len() == 3).count() < 2 {
        let b = take(BLOCK, 64, h).expect("a runs block");
        let p = b.cast::<u8>().as_ptr() as usize;
        touch(p, BLOCK);
        by_super.entry(p / SUPER).or_default().push(b);
    }
    let mut kept = Vec::new();
    let mut full = Vec::new();
    for (sp, blocks) in by_super {
        if blocks.len() == 3 && full.len() < 2 {
            full.push((sp, blocks));
        } else {
            kept.extend(blocks);
        }
    }
    let (b_sp, b_blocks) = full.pop().unwrap();
    let (a_sp, a_blocks) = full.pop().unwrap();
    kept.extend(b_blocks);
    for (j, b) in a_blocks.into_iter().enumerate() {
        if j == 0 {
            kept.push(b);
        } else {
            // SAFETY: ours, freed once.
            unsafe { give(b.cast(), BLOCK) };
        }
    }
    flush_shelf();
    (kept, a_sp * SUPER, b_sp * SUPER)
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

fn release_order(fast: bool) {
    hooks::fast_release_switch(fast);
    let h = Hint::new(Class::Task, Tag::OTHER);
    let (kept, a, b) = two_superpages(h);
    let before = r1_counts();
    hooks::clear();
    // one run's worth: the free-run step stops at the first superpage it takes
    let got = return_warm_now(RUN);
    let after = r1_counts();
    assert!(got >= RUN, "released {got}");
    let sps = given_back_in();
    if fast {
        assert!(sps.contains(&a) && !sps.contains(&b), "fast: the emptier superpage's free runs went first: {sps:x?} (a {a:x}, b {b:x})");
    } else {
        assert!(sps.contains(&b) && !sps.contains(&a), "off: the fullest superpage's free runs went first, as before: {sps:x?} (a {a:x}, b {b:x})");
    }
    assert_eq!(after.calls, before.calls + 1, "one return_warm_now counted");
    assert_eq!(after.step_calls[4], before.step_calls[4] + 1, "the free-run step ran once");
    assert!(after.step_bytes[4] - before.step_bytes[4] >= RUN as u64, "the free-run step's bytes counted");
    for b in kept {
        // SAFETY: ours, freed once.
        unsafe { give(b.cast(), BLOCK) };
    }
    flush_shelf();
    purge_now(usize::MAX, None);
    balanced();
}

fn dead_runs_below_the_bump() {
    hooks::fast_release_switch(true);
    let h = Hint::new(Class::Process, Tag::REGION);
    let len = 64 * MIB;
    let s = take(len, SUPER, h).expect("a lazy span");
    let p = s.cast::<u8>();
    touch(p.as_ptr() as usize, len);
    let s0 = stats();
    // SAFETY: inside our own span.
    unsafe { give_pages(p, 8 * MIB, 4 * MIB) };
    assert_eq!(stats().dead, s0.dead + 4 * MIB);
    let before = r1_counts();
    let got = return_warm_now(usize::MAX);
    let after = r1_counts();
    assert!(after.step_bytes[1] - before.step_bytes[1] >= (4 * MIB) as u64, "fast: the dead step found the span's dead runs ({got} released)");
    assert_eq!(resident(p.as_ptr() as usize + 8 * MIB, 4 * MIB), 0, "the dead runs' pages left the process");
    // SAFETY: as above.
    assert_eq!(unsafe { take_pages(p, 8 * MIB, 4 * MIB) }, Zeroed::Yes);
    // nothing dead left: the fast step gives nothing and scans nothing
    let again = r1_counts();
    return_warm_now(usize::MAX);
    assert_eq!(r1_counts().step_bytes[1], again.step_bytes[1]);
    // SAFETY: ours, not used again.
    unsafe { give(p, len) };
    flush_shelf();
    purge_now(usize::MAX, None);
    balanced();
}

fn an_r1_ask_lands() {
    hooks::fast_release_switch(true);
    let h = Hint::new(Class::Task, Tag::OTHER);
    let (kept, _, _) = two_superpages(h);
    let before = r1_counts();
    let asked = return_warm();
    assert!(asked > 0 && landing(60_000) == asked, "R1 asked {asked} of the purger");
    serve_ask_now();
    let after = r1_counts();
    assert_eq!(landing(60_000), 0, "served: no longer landing");
    assert_eq!(after.lands, before.lands + 1, "one ask -> land counted");
    assert!(after.land_bytes > before.land_bytes && after.land_ns > before.land_ns && after.land_max_ns >= after.land_ns - before.land_ns);
    assert_eq!(after.land_bins.iter().sum::<u64>(), after.lands, "every land in one bin");
    // a serve with no ask counts no land
    serve_ask_now();
    assert_eq!(r1_counts().lands, after.lands);
    for b in kept {
        // SAFETY: ours, freed once.
        unsafe { give(b.cast(), BLOCK) };
    }
    flush_shelf();
    purge_now(usize::MAX, None);
    balanced();
}

#[test]
fn fast_release() {
    setup();
    release_order(false);
    release_order(true);
    dead_runs_below_the_bump();
    an_r1_ask_lands();
    hooks::fast_release_switch(false);
    assert!(!r1_counts().fast_on);
}
