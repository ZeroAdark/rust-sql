//! A free run serves any class: runs Cache gave back serve Task's takes before any fresh superpage is claimed.

mod common;
use common::*;
use core::ptr::NonNull;
use pagepool::*;
use std::collections::HashSet;

const RUN: usize = 64 << 10;
const K9: usize = 9 * RUN;

fn take9(h: Hint) -> usize {
    take(K9, 16, h).expect("a 9-run take").cast::<u8>().as_ptr() as usize
}

#[test]
fn free_runs_of_one_class_serve_another_before_a_fresh_superpage() {
    setup();
    let x = Hint::new(Class::Cache, Tag::PART);
    let blocks: Vec<usize> = (0..60).map(|_| take9(x)).collect();
    let mut seen = HashSet::new();
    let (mut kept, mut freed) = (Vec::new(), 0);
    for a in blocks {
        if seen.insert(a / SUPER) {
            kept.push(a);
        } else {
            // SAFETY: taken above, not used again.
            unsafe { give(NonNull::new(a as *mut u8).unwrap(), K9) };
            freed += 1;
        }
    }
    assert!(freed >= 30, "the takes shared superpages: {freed} freed of 60");
    let o = occupancy();
    assert!(o.free_run_bytes[2] >= freed * K9, "Cache's partly free superpages hold the freed runs: {o:?}");
    let claimed = stats().claimed;
    let y = Hint::new(Class::Task, Tag::REGION);
    let got: Vec<usize> = (0..freed).map(|_| take9(y)).collect();
    assert_eq!(stats().claimed, claimed, "{freed} Task takes of 9 runs fit the runs Cache gave back");
    assert!(got.iter().all(|a| seen.contains(&(a / SUPER))), "every Task take sits in a superpage Cache claimed");
    for a in got.into_iter().chain(kept) {
        // SAFETY: taken above, not used again.
        unsafe { give(NonNull::new(a as *mut u8).unwrap(), K9) };
    }
    let o = occupancy();
    assert!(o.used_runs.iter().all(|c| c.iter().all(|&n| n == 0)), "every superpage wholly free again: {o:?}");
}
