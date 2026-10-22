//! NEW-43: an INCSTREAM reader past the merge of its fallen-back stream's runs (lowerv4 ops/sort/inc_stream.rs) re-arms
//! the region its merge was laid over (`SortStreamRearm`): the runs dropped, the merge given up when still in flight, the
//! region an empty run of the room it had, so the producer's next rows fill it and its last call seals it in place (no
//! runs left: read in place, `SORT_MODE_MEM`).
use gravity_kernels_rig as common;

use common::*;
use gravity_abi::breaker::{SortRun, TABLE_SORT};
use gravity_abi::ctx::RESUME_NONE;
use gravity_kernels::contract::entry2;
use gravity_kernels::host_abi::{sort_begin, sort_full, sort_last, sort_next, sort_open, SORT_MODE_MEM, SORT_MODE_MERGE};
use gravity_kernels::ops::sort::entries::SortStreamRearm;
use gravity_program::hand::{self as programs, Oids};

const ROW_BYTES: u32 = 40;
const SORT_TBL: [u32; 6] = [ROW_BYTES, 2, 2, 0, 0, 0];

unsafe fn push_row(run: *mut u8, key: i64) {
    let p = (*(run as *mut SortRun)).push() as *mut u64;
    (*p, *p.add(1), *p.add(2), *p.add(3), *p.add(4)) = (1, (key ^ i64::MIN) as u64, key as u64, 0, 0);
}

/// Rows `keys` into the own run, a run put after each `per` rows.
unsafe fn fill(rig: &mut Rig, keys: impl Iterator<Item = i64>, per: usize) -> *mut u8 {
    let ctx = rig.ctx();
    let run = sort_open(ctx, ROW_BYTES as u64, 2, 0);
    for (i, k) in keys.enumerate() {
        if i > 0 && i % per == 0 {
            assert_eq!(sort_full(ctx, run, 2, 0), 0);
        }
        push_row(run, k);
    }
    run
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

unsafe fn rearm(rig: &mut Rig) -> i64 {
    entry2::<SortStreamRearm>(rig.ctx() as u64, u64::MAX, SORT_TBL.as_ptr() as u64) as i64
}

#[test]
fn a_merge_read_to_its_end_then_the_region_streams_again() {
    let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
    unsafe {
        let own = gravity_abi::program::partial(rig.ctx());
        let run = fill(&mut rig, (0..90).rev(), 30);
        let cap = (*(run as *const SortRun)).cap;
        assert_eq!(sort_full(rig.ctx(), run, 2, 0), 0);
        assert_eq!(rig.host.runs.len(), 3, "three runs put");
        assert_eq!(begin(&mut rig), SORT_MODE_MERGE);
        let got: Vec<i64> = (0..90).map(|i| row(&mut rig, SORT_MODE_MERGE, i).expect("a row")).collect(); // alloc: Cold
        assert_eq!(got, (0..90).collect::<Vec<i64>>());
        assert_eq!(rearm(&mut rig), 0);
        assert!(rig.host.runs.is_empty(), "the runs dropped");
        let r = &*(own as *const SortRun);
        assert_eq!((r.cap, r.len, r.row_bytes, r.kind), (cap, 0, ROW_BYTES, TABLE_SORT), "an empty run of the same room");
        // the producer streams on into the region: its last call seals the run in place (no runs left)
        let run = fill(&mut rig, (100..140).rev(), usize::MAX);
        assert_eq!(run, own);
        assert_eq!(sort_last(rig.ctx(), run, 2, 0), 0);
        assert!(rig.host.runs.is_empty(), "sealed in place, not put");
        assert_eq!(begin(&mut rig), SORT_MODE_MEM);
        let got: Vec<i64> = (0..40).map(|i| row(&mut rig, SORT_MODE_MEM, i).expect("a row")).collect(); // alloc: Cold
        assert_eq!(got, (100..140).collect::<Vec<i64>>());
    }
}

#[test]
fn a_merge_in_flight_is_given_up_and_a_second_fallback_merges_only_its_own_runs() {
    let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
    unsafe {
        let run = fill(&mut rig, (0..60).rev(), 20);
        assert_eq!(sort_full(rig.ctx(), run, 2, 0), 0);
        assert_eq!(begin(&mut rig), SORT_MODE_MERGE);
        assert_eq!(row(&mut rig, SORT_MODE_MERGE, 0), Some(0));
        assert_eq!(rearm(&mut rig), 0, "the merge in flight given up");
        assert!(rig.host.runs.is_empty());
        // the next fallback's runs: the merge reads them alone
        let run = fill(&mut rig, (200..250).rev(), 25);
        assert_eq!(sort_full(rig.ctx(), run, 2, 0), 0);
        assert_eq!(rig.host.runs.len(), 2);
        assert_eq!(begin(&mut rig), SORT_MODE_MERGE);
        let got: Vec<i64> = (0..50).map(|i| row(&mut rig, SORT_MODE_MERGE, i).expect("a row")).collect(); // alloc: Cold
        assert_eq!(got, (200..250).collect::<Vec<i64>>());
        assert_eq!(row(&mut rig, SORT_MODE_MERGE, 50), None);
    }
}

#[test]
fn a_region_with_no_merge_is_a_bug() {
    let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
    unsafe {
        let _ = fill(&mut rig, 0..10, usize::MAX);
        let _ = gravity_fn::take_error();
        assert!(rearm(&mut rig) < 0);
        let msg = gravity_fn::take_error().map(|e| e.message().to_owned()); // alloc: Cold
        assert!(msg.as_deref().is_some_and(|m| m.contains("re-armed with no merge")), "{msg:?}");
    }
}
