//! C1 part 2: an INCSTREAM cursor reader that handed on every closed row empties its run before it pulls again
//! (`SortStreamRecycle`, lowerv4 ops/sort/inc_stream.rs `cursor_pull`) when the run holds those rows only; a row past them
//! (an open group) keeps the run as it is, and a run under a merge (its stream fell back) is a bug.
use gravity_kernels_rig as common;

use common::*;
use gravity_abi::breaker::SortRun;
use gravity_abi::ctx::RESUME_NONE;
use gravity_kernels::contract::entry3;
use gravity_kernels::host_abi::{sort_begin, sort_full, sort_last, sort_next, sort_open, SORT_MODE_MEM, SORT_MODE_MERGE};
use gravity_kernels::ops::sort::entries::SortStreamRecycle;
use gravity_program::hand::{self as programs, Oids};

const ROW_BYTES: u32 = 40;
const SORT_TBL: [u32; 6] = [ROW_BYTES, 2, 2, 0, 0, 0];

unsafe fn fill(rig: &mut Rig, keys: impl Iterator<Item = i64>) -> *mut u8 {
    let run = sort_open(rig.ctx(), ROW_BYTES as u64, 2, 0);
    for k in keys {
        let p = (*(run as *mut SortRun)).push() as *mut u64;
        (*p, *p.add(1), *p.add(2), *p.add(3), *p.add(4)) = (1, (k ^ i64::MIN) as u64, k as u64, 0, 0);
    }
    run
}

unsafe fn recycle(rig: &mut Rig, closed: u64) -> i64 {
    entry3::<SortStreamRecycle>(rig.ctx() as u64, u64::MAX, SORT_TBL.as_ptr() as u64, closed) as i64
}

unsafe fn begin(rig: &mut Rig) -> i64 {
    (*rig.ctx()).resume = RESUME_NONE;
    sort_begin(rig.ctx(), SORT_TBL.as_ptr())
}

unsafe fn row(rig: &mut Rig, mode: i64, i: usize) -> Option<i64> {
    let p = sort_next(rig.ctx(), SORT_TBL.as_ptr(), mode as u64, i as u64);
    assert!(p as i64 >= 0, "a status below zero");
    (!p.is_null()).then(|| *(p as *const u64).add(2) as i64)
}

#[test]
fn a_run_of_closed_rows_only_is_emptied_and_streams_again() {
    let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
    unsafe {
        let run = fill(&mut rig, 0..40);
        let cap = (*(run as *const SortRun)).cap as i64;
        assert_eq!(recycle(&mut rig, 40), cap << 1 | 1, "the room and the emptied bit");
        assert_eq!((*(run as *const SortRun)).len, 0);
        assert!(rig.host.runs.is_empty(), "nothing put");
        let again = fill(&mut rig, 100..130);
        assert_eq!(again, run, "the same region");
        assert_eq!(sort_last(rig.ctx(), run, 2, 0), 0);
        assert_eq!(begin(&mut rig), SORT_MODE_MEM);
        let got: Vec<i64> = (0..30).map(|i| row(&mut rig, SORT_MODE_MEM, i).expect("a row")).collect(); // alloc: Cold
        assert_eq!(got, (100..130).collect::<Vec<i64>>());
        assert_eq!(row(&mut rig, SORT_MODE_MEM, 30), None);
    }
}

#[test]
fn a_row_past_the_closed_rows_keeps_the_run() {
    let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
    unsafe {
        let run = fill(&mut rig, 0..41);
        let cap = (*(run as *const SortRun)).cap as i64;
        assert_eq!(recycle(&mut rig, 40), cap << 1, "not emptied");
        assert_eq!((*(run as *const SortRun)).len, 41);
    }
}

#[test]
fn a_run_under_a_merge_is_a_bug() {
    let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
    unsafe {
        let run = fill(&mut rig, (0..20).rev());
        assert_eq!(sort_full(rig.ctx(), run, 2, 0), 0);
        let run = fill(&mut rig, (20..40).rev());
        assert_eq!(sort_full(rig.ctx(), run, 2, 0), 0);
        assert_eq!(begin(&mut rig), SORT_MODE_MERGE);
        let _ = gravity_fn::take_error();
        assert!(recycle(&mut rig, 0) < 0);
        let msg = gravity_fn::take_error().map(|e| e.message().to_owned()); // alloc: Cold
        assert!(msg.as_deref().is_some_and(|m| m.contains("recycled under a merge")), "{msg:?}");
    }
}
