//! Pressure gives back the free runs of superpages still in use: one release per stretch inside its superpage (the ring
//! says), the pages leave the process and `claimed`; a run taken again is zero and claimed again.
#![cfg(target_os = "linux")]

mod common;
use common::*;
use pagepool::hooks::{self, Op};
use pagepool::*;
use std::collections::HashSet;

const RUN: usize = 64 << 10;
const BLOCK: usize = 4 * RUN;

fn resident_free_runs() -> (usize, usize) {
    let o = occupancy();
    let free: usize = o.free_run_bytes.iter().sum();
    let gone: usize = o.gone_run_bytes.iter().sum();
    (free - gone, gone)
}

#[test]
fn free_runs_inside_used_superpages_go_back_under_pressure() {
    setup();
    let h = Hint::new(Class::Task, Tag::OTHER);
    let mut kept = Vec::new();
    let mut freed = Vec::new();
    let mut seen = HashSet::new();
    for _ in 0..128 {
        let b = take(BLOCK, 64, h).expect("a runs block");
        let p = b.cast::<u8>().as_ptr() as usize;
        touch(p, BLOCK);
        if seen.insert(p / SUPER) { kept.push(b) } else { freed.push(p) }
    }
    let supers = seen.len();
    for &p in &freed {
        // SAFETY: ours, freed once.
        unsafe { give(core::ptr::NonNull::new(p as *mut u8).unwrap(), BLOCK) };
    }
    flush_shelf();
    hooks::clear();
    let before = stats();
    let got = purge_now(usize::MAX, None);
    let after = stats();
    let (left, gone) = resident_free_runs();
    let want = freed.len() * BLOCK;
    assert!(got >= want, "released {got} < the freed runs' {want}");
    assert_eq!(left, 0, "no resident free run is left in a used superpage");
    assert!(gone >= want, "the freed runs are gone: {gone} < {want}");
    assert!(before.claimed - after.claimed >= want, "claimed fell by the gone runs: {before:?} -> {after:?}");
    for &p in &freed {
        assert_eq!(resident(p, BLOCK), 0, "a gone run's pages left the process");
    }
    let (mut inside, mut bytes, mut whole) = (0, 0, 0);
    let complete = hooks::each(|op, at, len, _| {
        if op == Op::GiveBack && len < SUPER {
            inside += 1;
            bytes += len;
            whole += (at / SUPER != (at + len - 1) / SUPER) as usize;
        }
    });
    assert!(complete, "the syscall log overflowed");
    assert!(inside >= supers && bytes >= want && whole == 0, "{inside} releases inside {supers} superpages, {bytes} bytes, {whole} across a superpage");
    balanced();
    let again: Vec<_> = (0..freed.len()).map(|_| take(BLOCK, 64, h).expect("a runs block again")).collect();
    for b in &again {
        let p = b.cast::<u8>().as_ptr() as usize;
        // SAFETY: a fresh block of BLOCK bytes, ours.
        let z = unsafe { core::slice::from_raw_parts(p as *const u8, BLOCK) };
        assert!(z.iter().all(|&x| x == 0), "a run taken again from the OS reads zero");
    }
    assert!(stats().claimed >= after.claimed + want, "the runs taken again are claimed again");
    balanced();
    for b in again.into_iter().chain(kept) {
        // SAFETY: ours, freed once.
        unsafe { give(b.cast(), BLOCK) };
    }
    flush_shelf();
    balanced();
}
