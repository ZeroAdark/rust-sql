//! D4872 (`PGRUST_POOL_HOLE_FIT`): a take of many runs finds the superpage holding them in a row however deep it lies in
//! its class's partial lists. Listed by used runs, superpage S0 (9 used, 23 free in a row) sat behind 20 others with
//! 9 used and no 23-run stretch in the same bin: the 23-run take saw 16 of them and claimed a fresh superpage. Born red
//! with the switch off. A small take (a one-run carve) still packs: it lands in the superpage with the smallest hole, not
//! one with a big hole.

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

/// S0's 23 runs freed first, then those of 20 superpages keeping 9 used runs between holes of 12 and 11: whether the
/// 23-run take landed on S0's freed runs.
fn deep_hole(h: Hint) -> bool {
    let x = take_runs(23, h);
    let w = take_runs(9, h);
    assert_eq!(w / SUPER, x / SUPER, "the 9 runs fill S0's tail");
    let mut held = Vec::new();
    let mut freed = Vec::new();
    for _ in 0..20 {
        let a = take_runs(12, h);
        let b = take_runs(9, h);
        let c = take_runs(11, h);
        assert!(a / SUPER == b / SUPER && b / SUPER == c / SUPER, "one superpage a decoy");
        held.push((b, 9));
        freed.push((a, 12));
        freed.push((c, 11));
    }
    give_runs(x, 23);
    for &(p, k) in &freed {
        give_runs(p, k);
    }
    let y = take_runs(23, h);
    let found = y == x;
    give_runs(y, 23);
    give_runs(w, 9);
    for (p, k) in held {
        give_runs(p, k);
    }
    flush_shelf();
    purge_now(usize::MAX, None);
    found
}

#[test]
fn a_big_take_finds_the_hole_past_the_search() {
    setup();
    let h = Hint::new(Class::Session, Tag::OTHER);
    hooks::hole_fit_switch(false);
    assert!(!deep_hole(h), "off (listed by used runs): the 23-run take missed S0");
    hooks::hole_fit_switch(true);
    assert!(deep_hole(h), "on: the 23-run take found S0's 23 free runs");
    let o = occupancy();
    assert!(o.used_runs.iter().all(|c| c.iter().all(|&n| n == 0)), "every superpage wholly free again: {o:?}");

    // a one-run carve (4 KiB blocks): SA keeps a 2-run hole (hole bin 2), SB a 23-run hole (bin 23); the carve goes to SA
    let sa = take_runs(30, h);
    let sb = take_runs(9, h);
    assert_ne!(sa / SUPER, sb / SUPER, "9 runs do not fit SA's 2-run hole");
    let blk = take(4096, 16, h).expect("a 4 KiB block").cast::<u8>().as_ptr() as usize;
    assert_eq!(blk / SUPER, sa / SUPER, "the carve filled the smallest hole (SA), not SB's 23 runs");
    // SAFETY: taken above, given once.
    unsafe { give(NonNull::new(blk as *mut u8).unwrap(), 4096) };
    give_runs(sa, 30);
    give_runs(sb, 9);
}
