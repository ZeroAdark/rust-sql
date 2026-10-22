//! D1061: one spilled sort run read by K pipelines (decompose's grouping-sets spool, one per set). Each reader's
//! `sort_begin_at` lays its merge over the run's region, where the first found the run's header; a later reader re-lays
//! it over the bytes that reader recorded, and a reader begun while another's merge is in flight is a named Bug. F3 (b):
//! a reader begun again over a merge it left in flight itself (a Merge Join that stopped reading, then a recursive
//! term's next generation over the Sort kept whole) gives that merge up and reads every row from the start.
use gravity_kernels_rig as common;

use common::*;
use gravity_abi::breaker::SortRun;
use gravity_abi::ctx::RESUME_NONE;
use gravity_kernels::host_abi::{sort_begin_at, sort_full, sort_last, sort_next_at, sort_open, SORT_MODE_MERGE};
use gravity_program::hand::{self as programs, Oids};

const ROW_BYTES: u32 = 40;
const SORT_TBL: [u32; 6] = [ROW_BYTES, 2, 2, 0, 0, 0];

unsafe fn push_row(run: *mut u8, key: i64, tag: u64) {
    let p = (*(run as *mut SortRun)).push() as *mut u64;
    (*p, *p.add(1), *p.add(2), *p.add(3)) = (1, (key ^ i64::MIN) as u64, key as u64, tag);
    *p.add(4) = 0;
}

/// 100 shuffled rows sorted, a run put at row 50 and the last at the end: the region keeps the run's header only.
unsafe fn spilled(rig: &mut Rig) -> Vec<u64> {
    let ctx = rig.ctx();
    let run = sort_open(ctx, ROW_BYTES as u64, 2, 0);
    for (i, k) in (0..100i64).map(|i| (i * 37 + 11) % 100).enumerate() {
        if i == 50 {
            assert_eq!(sort_full(ctx, run, 2, 0), 0);
        }
        push_row(run, k, i as u64);
    }
    assert_eq!(sort_last(ctx, run, 2, 0), 0);
    assert_eq!(rig.host.runs.len(), 2, "two runs put");
    let own = gravity_abi::program::partial(ctx);
    core::slice::from_raw_parts(own as *const u64, SortRun::region_bytes(own, ROW_BYTES) / 8).to_vec() // alloc: Cold
}

unsafe fn consume(rig: &mut Rig, region: &mut [u64], fold: &mut [u64]) {
    let p = &mut *(*(*rig.ctx()).program).partial;
    (p.own, p.consumed[0]) = (fold.as_mut_ptr() as *mut u8, region.as_mut_ptr() as *mut u8);
    (*rig.ctx()).resume = RESUME_NONE;
}

unsafe fn next(rig: &mut Rig, i: usize) -> Option<i64> {
    let p = sort_next_at(rig.ctx(), SORT_TBL.as_ptr(), SORT_MODE_MERGE as u64, i as u64, 0, 3);
    assert!(p as i64 >= 0, "a status below zero");
    (!p.is_null()).then(|| *(p as *const u64).add(2) as i64)
}

#[test]
fn three_readers_of_one_spilled_run_each_read_every_row() {
    let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
    unsafe {
        let mut region = spilled(&mut rig);
        let mut fold = vec![0u64; 1 << 12]; // alloc: Cold
        for reader in 0..3 {
            consume(&mut rig, &mut region, &mut fold);
            assert_eq!(sort_begin_at(rig.ctx(), SORT_TBL.as_ptr(), 0, 3), SORT_MODE_MERGE, "reader {reader}");
            // readers 0 and 2 stop at the total, as an emit does (`sort_total`); reader 1 reads to the end
            let got: Vec<i64> = (0..100).map(|i| next(&mut rig, i).expect("a row")).collect(); // alloc: Cold
            assert_eq!(got, (0..100).collect::<Vec<i64>>(), "reader {reader}");
            if reader == 1 {
                assert_eq!(next(&mut rig, 100), None);
            }
        }
        assert!(fold.iter().all(|&w| w == 0), "every merge in the run's region");
        assert_eq!(rig.host.runs.len(), 2, "the runs outlive their readers");
    }
}

#[test]
fn a_reader_begun_over_a_merge_in_flight_is_a_bug() {
    let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
    unsafe {
        let mut region = spilled(&mut rig);
        let (mut fold, mut other) = (vec![0u64; 1 << 12], vec![0u64; 1 << 12]); // alloc: Cold
        consume(&mut rig, &mut region, &mut fold);
        assert_eq!(sort_begin_at(rig.ctx(), SORT_TBL.as_ptr(), 0, 3), SORT_MODE_MERGE);
        assert_eq!(next(&mut rig, 0), Some(0));
        // another reader (its own region elsewhere) while the first's merge is in flight
        consume(&mut rig, &mut region, &mut other);
        let _ = gravity_fn::take_error();
        assert!(sort_begin_at(rig.ctx(), SORT_TBL.as_ptr(), 0, 3) < 0);
        let msg = gravity_fn::take_error().map(|e| e.message().to_owned()); // alloc: Cold
        assert!(msg.as_deref().is_some_and(|m| m.contains("Bug: a sort emit's merge laid over another reader's merge in flight")), "{msg:?}");
    }
}

#[test]
fn a_reader_begun_again_over_its_own_merge_in_flight_reads_every_row() {
    let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
    unsafe {
        let mut region = spilled(&mut rig);
        let mut fold = vec![0u64; 1 << 12]; // alloc: Cold
        // the reader stops after 1, then 57 rows (its merge left in flight each time), then reads to the end twice
        for stop in [1usize, 57, 100, 100] {
            consume(&mut rig, &mut region, &mut fold);
            assert_eq!(sort_begin_at(rig.ctx(), SORT_TBL.as_ptr(), 0, 3), SORT_MODE_MERGE, "stop {stop}");
            let got: Vec<i64> = (0..stop).map(|i| next(&mut rig, i).expect("a row")).collect(); // alloc: Cold
            assert_eq!(got, (0..stop as i64).collect::<Vec<i64>>(), "stop {stop}");
        }
        assert_eq!(next(&mut rig, 100), None);
        assert!(fold.iter().all(|&w| w == 0), "every merge in the run's region");
        assert_eq!(rig.host.runs.len(), 2, "the runs outlive their readers");
    }
}
