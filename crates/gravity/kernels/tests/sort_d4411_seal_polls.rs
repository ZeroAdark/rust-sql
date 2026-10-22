//! D4411: a sort run's order for its seal is a polled step. A serial producer's last var run is ordered where it lies
//! (`sort_last_res_var` -> `seal_var` -> `index_by`: the heads' sort, then the permutation); the sort was one kernel call
//! with no poll, so a statement's cancel waited for the whole run's sort and only the permutation after it saw the cancel.
#![cfg(feature = "hand-kernels")]
use gravity_kernels_rig as common;
use common::*;
use std::time::Instant;
use gravity_abi::breaker::{HashAgg, SortRun};
use gravity_abi::status::CANCELLED;
use gravity_kernels::ops::sort::entries::{sort_last_res_var, sort_open_res_var, RUN_SEALED};
use gravity_kernels::host_abi::{sort_image, SortSpec, VAR_TBL, VAR_WIDTH};
use gravity_program::hand::{self as programs, Oids};

const ROW_BYTES: u32 = 40;
/// Eight of the polled sort's runs (`POLL_SORT_RUN`), so the order merges.
const ROWS: usize = 1 << 19;
/// The worker's region: the rows and their copies.
const SHARE: u32 = 1 << 26;

/// One text key (its image's slot at 16, bytes order), the tag at 24, the record word at 32.
fn vtbl() -> [u32; 12] {
    let spec = SortSpec::pack(&[(Some((16, VAR_WIDTH)), false)]).unwrap().0;
    [ROW_BYTES, 2, 3 | VAR_TBL, 0, spec as u32, (spec >> 32) as u32, 1, 32, 2, 16, 0, 0]
}

unsafe fn push_text(run: *mut u8, x: &[u8], tag: u64) {
    let r = run as *mut SortRun;
    let p = (*r).rows_ptr().add((*r).len as usize * ROW_BYTES as usize) as *mut u64;
    *p.add(4) = 0;
    assert_eq!(sort_image(run, x.as_ptr(), x.len() as u64, 16, 32), 0);
    let mut w = [0u8; 8];
    w[..x.len().min(8)].copy_from_slice(&x[..x.len().min(8)]);
    (*r).push();
    (*p, *p.add(1), *p.add(3)) = (1, u64::from_be_bytes(w), tag);
}

unsafe fn text_at(r: *const SortRun, i: usize) -> Vec<u8> {
    let slot = ((*r).row(i) as *const u8).add(16);
    let (size, off) = ((slot as *const u32).read_unaligned() as usize, (slot.add(4) as *const u32).read_unaligned() as usize);
    core::slice::from_raw_parts(slot.add(off + 4), size - 4).to_vec()
}

unsafe fn tag_at(r: *const SortRun, i: usize) -> u64 {
    *((*r).row(i) as *const u64).add(3)
}

/// Row `i`'s text: distinct (an odd multiplier is a bijection of `0..ROWS`), shuffled, its first 8 bytes shared by many
/// rows (the heads' word ties, so the payloads decide).
fn text(i: usize) -> Vec<u8> {
    let k = (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) & (ROWS as u64 - 1);
    format!("row-{:02}-{k:012}", k % 7).into_bytes()
}

/// A rig whose worker region holds `ROWS` var rows (the rig's share holds a few thousand): `ROWS` texts pushed into a
/// serial producer's run, the run handed back with the region that holds it.
unsafe fn filled(rig: &mut Rig, tbl: &[u32; 12]) -> (*mut u8, Vec<u64>) {
    let ctx = rig.ctx();
    let mut region = vec![0u64; SHARE as usize / 8]; // alloc: Cold
    HashAgg::arm(region.as_mut_ptr() as *mut u8, SHARE);
    (*(*(*ctx).program).partial).own = region.as_mut_ptr() as *mut u8;
    (*ctx).width = 1;
    let run = sort_open_res_var(ctx, tbl.as_ptr());
    assert!((*(run as *const SortRun)).cap as usize >= ROWS, "the region holds the rows");
    (0..ROWS).for_each(|i| push_text(run, &text(i), i as u64));
    (run, region)
}

/// D4411 (born red): with the statement's abort pending when a large run is sealed, the seal answers CANCELLED before
/// it orders the run, not after: it takes a small part of an uncancelled seal's time (on trunk, the whole run's sort
/// ran first, then the permutation saw the abort). The uncancelled seal orders the run by its key, every row once.
#[test]
fn a_pending_abort_stops_a_large_runs_seal_before_its_sort() {
    let tbl = vtbl();
    let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
    let (full, keep) = unsafe {
        let (run, region) = filled(&mut rig, &tbl);
        let t = Instant::now();
        assert_eq!(sort_last_res_var(rig.ctx(), run, tbl.as_ptr()), 0);
        let full = t.elapsed();
        let r = run as *const SortRun;
        assert_eq!(((*r).pad[0], (*r).len as usize), (RUN_SEALED, ROWS));
        let got: Vec<Vec<u8>> = (0..ROWS).map(|i| text_at(r, i)).collect(); // alloc: Cold
        let mut want: Vec<Vec<u8>> = (0..ROWS).map(text).collect(); // alloc: Cold
        want.sort();
        assert!(got == want, "sealed in byte order, each copy with its row");
        (full, region)
    };
    drop(keep);
    let mut rig = Rig::new(programs::par_sort_with(Oids::T_A, 64, 0, &[1, 2]), vec![]);
    unsafe {
        let (run, _region) = filled(&mut rig, &tbl);
        *rig.interrupt = 1;
        let t = Instant::now();
        assert_eq!(sort_last_res_var(rig.ctx(), run, tbl.as_ptr()), CANCELLED);
        let cancelled = t.elapsed();
        let r = run as *const SortRun;
        let mut tags: Vec<u64> = (0..(*r).len as usize).map(|i| tag_at(r, i)).collect(); // alloc: Cold
        tags.sort_unstable();
        assert!(tags == (0..ROWS as u64).collect::<Vec<u64>>(), "a cancelled seal keeps every row");
        assert!(cancelled * 4 < full, "the abort stopped the seal in {cancelled:?}, an uncancelled seal took {full:?}: the order did not poll");
    }
}
