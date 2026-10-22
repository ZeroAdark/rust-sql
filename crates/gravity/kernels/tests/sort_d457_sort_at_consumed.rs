//! D457 (V4R-124): `sort_begin` / `sort_next` / `sort_stop` read `partial(ctx).own`, which the driver points at a
//! pipeline's own breaker slot: a sort whose rows feed a fold or a group read the fold's region ("unsupported execution
//! (kind 2)", or the rows lost). `sort_begin_at` / `sort_next_at` / `sort_stop_at` read consumed region `k` and the
//! consumed slot's run stream, the merge's fills based in that region.
use gravity_kernels_rig as common;

use common::*;
use gravity_abi::breaker::{HashAgg, SortRun, SORTRUN_HDR};
use gravity_abi::ctx::resume;
use gravity_kernels::host_abi::{sort_begin, sort_begin_at, sort_full, sort_last, sort_next_at, sort_open, sort_stop_at, SORT_MODE_MEM, SORT_MODE_MERGE};
use gravity_program::hand::{self as programs, Oids};

const ROW_BYTES: u32 = 40;
const SORT_TBL: [u32; 6] = [ROW_BYTES, 2, 2, 0, 0, 0];

unsafe fn push_row(run: *mut u8, key: i64, tag: u64) {
    let p = (*(run as *mut SortRun)).push() as *mut u64;
    (*p, *p.add(1), *p.add(2), *p.add(3)) = (1, (key ^ i64::MIN) as u64, key as u64, tag);
    *p.add(4) = 0;
}

/// 100 rows in shuffled order into the rig's sort, `put` of them in a first run put to the stream.
unsafe fn produce(rig: &mut Rig, put: usize) {
    let ctx = rig.ctx();
    let run = sort_open(ctx, ROW_BYTES as u64, 2, 0);
    for (i, k) in (0..100i64).map(|i| (i * 37 + 11) % 100).enumerate() {
        if i == put && put != 0 {
            assert_eq!(sort_full(ctx, run, 2, 0), 0);
        }
        push_row(run, k, i as u64);
    }
    assert_eq!(sort_last(ctx, run, 2, 0), 0);
}

/// The consumer's view: its own region a fold's (zeroed), the sort's region at `consumed[0]`.
unsafe fn consume(rig: &mut Rig, sort: *mut u8, fold: &mut [u64]) {
    let p = &mut *(*(*rig.ctx()).program).partial;
    (p.own, p.consumed[0]) = (fold.as_mut_ptr() as *mut u8, sort);
}

unsafe fn drain(rig: &mut Rig, mode: i64) -> Vec<i64> {
    let ctx = rig.ctx();
    let mut got = Vec::new(); // alloc: Cold
    loop {
        let p = sort_next_at(ctx, SORT_TBL.as_ptr(), mode as u64, got.len() as u64, 0, 3);
        assert!(p as i64 >= 0, "a status below zero");
        if p.is_null() {
            return got;
        }
        got.push(*(p as *const u64).add(2) as i64);
    }
}

#[test]
fn a_sealed_sort_feeding_a_fold_is_read_from_its_consumed_region() {
    let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
    unsafe {
        produce(&mut rig, 0);
        let ctx = rig.ctx();
        assert_eq!(rig.host.runs.len(), 0, "the run sealed in the region");
        let own = gravity_abi::program::partial(ctx) as *const SortRun;
        let words = (SORTRUN_HDR + (*own).cap as usize * ROW_BYTES as usize) / 8 + 2;
        let mut sorted: Vec<u64> = core::slice::from_raw_parts(own as *const u64, words).to_vec(); // alloc: Cold
        let mut fold = vec![0u64; 1 << 12]; // alloc: Cold
        consume(&mut rig, sorted.as_mut_ptr() as *mut u8, &mut fold);
        assert!(sort_begin(ctx, SORT_TBL.as_ptr()) < 0, "the old open reads the fold's region: no sealed run there");
        assert_eq!(sort_begin_at(ctx, SORT_TBL.as_ptr(), 0, 3), SORT_MODE_MEM);
        assert_eq!(drain(&mut rig, SORT_MODE_MEM), (0..100).collect::<Vec<i64>>());
        sort_stop_at(ctx, SORT_TBL.as_ptr(), SORT_MODE_MEM as u64, 8, 1, 0);
        assert_eq!((*ctx).resume, resume::word(resume::EMIT, 7), "the held row is re-delivered");
        assert!(fold.iter().all(|&w| w == 0), "the fold's region untouched");
    }
}

#[test]
fn a_spilled_sort_feeding_a_fold_merges_in_its_consumed_region() {
    let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
    unsafe {
        produce(&mut rig, 50);
        let ctx = rig.ctx();
        assert_eq!(rig.host.runs.len(), 2, "two runs put");
        let mut region = vec![0u64; 1 << 13]; // alloc: Cold
        HashAgg::arm(region.as_mut_ptr() as *mut u8, (region.len() * 8) as u32);
        let mut fold = vec![0u64; 1 << 12]; // alloc: Cold
        consume(&mut rig, region.as_mut_ptr() as *mut u8, &mut fold);
        assert_eq!(sort_begin_at(ctx, SORT_TBL.as_ptr(), 0, 3), SORT_MODE_MERGE);
        assert_eq!(drain(&mut rig, SORT_MODE_MERGE), (0..100).collect::<Vec<i64>>());
        assert!(fold.iter().all(|&w| w == 0), "the merge and its fills in the sort's region, none in the fold's");
        assert!(region[SORTRUN_HDR / 8..].iter().any(|&w| w != 0));
    }
}
