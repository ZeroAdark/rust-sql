//! D4872 (`PGRUST_POOL_HOLE_FIT`, one bin per longest free stretch): a take of 24 runs finds the superpage whose longest
//! free stretch is 24 behind 20 whose longest is 23. With 7 hole bins (22-26 one bin) the 23s and the 24 shared a list and
//! the take's 16 probes saw only 23s: it claimed a fresh superpage (an x86 bulk load: the k24-26 takes' misses).

mod common;
use common::*;
use core::ptr::NonNull;
use pagepool::*;

const RUN: usize = 64 << 10;

fn take_runs(k: usize, h: Hint) -> usize {
    let b = take(k * RUN, 16, h).expect("a runs take");
    let p = b.cast::<u8>().as_ptr() as usize;
    touch(p, k * RUN);
    p
}

fn give_runs(p: usize, k: usize) {
    // SAFETY: taken by `take_runs`, given once.
    unsafe { give(NonNull::new(p as *mut u8).unwrap(), k * RUN) };
}

#[test]
fn a_24_run_take_finds_the_24_run_hole_behind_23s() {
    setup();
    hooks::hole_fit_switch(true);
    let h = Hint::new(Class::Session, Tag::OTHER);
    let x = take_runs(24, h);
    let w = take_runs(8, h);
    assert_eq!(w / SUPER, x / SUPER, "the 8 runs fill S0's tail");
    let mut held = Vec::new();
    let mut freed = Vec::new();
    for _ in 0..20 {
        let a = take_runs(23, h);
        let b = take_runs(9, h);
        assert_eq!(a / SUPER, b / SUPER, "one superpage a decoy");
        held.push(b);
        freed.push(a);
    }
    // S0 settles first (longest stretch 24), then the 20 decoys (longest 23) in front of it
    give_runs(x, 24);
    for &a in &freed {
        give_runs(a, 23);
    }
    let y = take_runs(24, h);
    assert_eq!(y, x, "the 24-run take landed on S0's 24 free runs, not a fresh superpage");
    give_runs(y, 24);
    give_runs(w, 8);
    for b in held {
        give_runs(b, 9);
    }
    flush_shelf();
    purge_now(usize::MAX, None);
}
