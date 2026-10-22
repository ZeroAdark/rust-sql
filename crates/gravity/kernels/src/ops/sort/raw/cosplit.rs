//! D3190 (merge.cosplit): the sort emit's open, total and next over a co-split range table (its layout `COSPLIT_*` in
//! gravity_abi program.rs; exec lays it, the merge join's lowering declares it). A claim of parts over the outer's put runs
//! (`COSPLIT_MODE_SLICES`) reads the merge of those parts' slice of each run, laid over its owner's merge area; any other
//! claim reads the run as `sort_begin_at` / `sort_total_at` / `sort_next_at` do.
use gravity_abi::program::{COSPLIT_HDR_WORDS, COSPLIT_MODE, COSPLIT_MODE_SLICES, COSPLIT_PARTS, COSPLIT_PART_WORDS, COSPLIT_RUNS, COSPLIT_SLICES_AT, COSPLIT_WHOLE, SPILL_MERGE_BIG, SPILL_RUN_SLICE, STREAM_CONSUMED};
use gravity_abi::status::{DONE, ERROR};
use gravity_abi::Ctx;

use super::rowsort::{begin_slices, next_slices, slices_area_bytes};
use crate::ops::sort::runmerge::RunMerge;
use crate::host_abi::{bare, sort_begin_at, sort_next_at, sort_total_at, SORT_MODE_MERGE};

/// `sort_cosplit_begin`'s mode for a part's claim over the outer's put runs: the merge of its slices over the claim's
/// owner's area (`sort_cosplit_next` drains it); past the emit's `SORT_MODE_MEM` / `SORT_MODE_MERGE`.
pub const SORT_MODE_SLICES: i64 = 3;

/// Owner `ord`'s word of co-split table `ctab` (its merge area's address, 0 before its first claim over a put outer).
unsafe fn owner_word(ctab: *const u64, ord: u16) -> *mut u64 {
    let parts = *ctab.add(COSPLIT_PARTS) as usize;
    ctab.add(COSPLIT_HDR_WORDS + parts * COSPLIT_PART_WORDS + ord as usize) as *mut u64
}

/// Word `w` of part `p`'s entry in co-split table `ctab`.
unsafe fn part_word(ctab: *const u64, p: usize, w: usize) -> *const u64 {
    ctab.add(COSPLIT_HDR_WORDS + p * COSPLIT_PART_WORDS + w)
}

/// The parts `[a, b)` the running claim covers (`a < b`, `b` at most the table's parts: the pool's sizer may hand out
/// several at once), else `None` (the section's end, a first call, `ALL`).
unsafe fn claim_parts(ctx: *mut Ctx, ctab: *const u64) -> Option<(usize, usize)> {
    let c = (*ctx).claim;
    (c.lo < c.hi && c.hi <= *ctab.add(COSPLIT_PARTS)).then_some((c.lo as usize, c.hi as usize))
}

/// Parts `[a, b)`'s claim over the outer's put runs (`sort_cosplit_begin`): this owner's merge area (drawn once and kept
/// for the statement: `SPILL_MERGE_BIG` keyed by the table, its header zeroed when fresh, its address in the owner's
/// word), then the merge of each run's slice of those parts (consecutive key ranges: part a's start to part b - 1's end of
/// each run) laid over it (`SPILL_RUN_SLICE` readers).
unsafe fn outer_slices(ctx: *mut Ctx, tbl: *const u32, k: u64, ctab: *const u64, a: usize, b: usize) -> i64 {
    let (nruns, slices) = (*ctab.add(COSPLIT_RUNS) as usize, *ctab.add(COSPLIT_SLICES_AT) as *const u64);
    if slices.is_null() {
        return bare("Bug: a co-split claim over put runs with no slices");
    }
    let bytes = slices_area_bytes(tbl, nruns);
    let key = ctab as u64 + 1;
    let mut area = crate::util::spill(ctx, SPILL_MERGE_BIG, [key, u64::MAX].as_ptr() as i64);
    if area < 0 {
        return area;
    }
    if area == 0 {
        area = crate::util::spill(ctx, SPILL_MERGE_BIG, [key, bytes as u64].as_ptr() as i64);
        if area == 0 {
            return ERROR; // the driver holds the allocation's error
        }
        core::ptr::write_bytes(area as usize as *mut u8, 0, core::mem::size_of::<RunMerge>());
    }
    *owner_word(ctab, (*ctx).worker_ordinal) = area as u64;
    let (first, last) = (slices.add(2 * a * nruns), slices.add(2 * (b - 1) * nruns));
    let open = |r: usize| {
        let w = [STREAM_CONSUMED | k, r as u64, *first.add(2 * r), *last.add(2 * r + 1)];
        crate::util::spill(ctx, SPILL_RUN_SLICE, w.as_ptr() as i64)
    };
    match begin_slices(ctx, area as usize as *mut u8, bytes, tbl, nruns, &open) {
        SORT_MODE_MERGE => SORT_MODE_SLICES,
        st => st,
    }
}

crate::contract::host! {
    /// The emit's open in the co-split form, `sort_begin_at`'s over consumed region `k`, `ctab` the co-split's range
    /// table: the section's end claim and a first call with no claim open nothing (`DONE`: no merge laid over the run
    /// every worker reads); a claim of parts over the outer's put runs (`COSPLIT_MODE_SLICES`) lays the merge of those
    /// parts' slice of each run over this owner's merge area (`SORT_MODE_SLICES`); D3336 (A1a): a parted window's fill
    /// claim whose first part reads no rows of an uncut run (`[WHOLE, WHOLE)`: exec ops/sort/cosplit.rs lays a window's
    /// parts past the first so when it leaves the run uncut, as over put runs) opens nothing too, so the run's one reader
    /// is part 0's claim (claims each laying a merge over one put run's stream at once share its merge state: SIGSEGV in
    /// runmerge `merge_pass`); any other claim is `sort_begin_at`'s.
    pub unsafe fn sort_cosplit_begin(ctx: *mut Ctx, tbl: *const u32, k: u64, ctab: *const u64) -> i64 {
        let c = (*ctx).claim;
        if c.lo >= c.hi {
            return DONE;
        }
        match (*ctab.add(COSPLIT_MODE), claim_parts(ctx, ctab)) {
            (COSPLIT_MODE_SLICES, Some((a, b))) => guard!(ERROR, outer_slices(ctx, tbl, k, ctab, a, b)),
            (_, Some((a, _))) if *part_word(ctab, a, 0) == COSPLIT_WHOLE => DONE,
            _ => sort_begin_at(ctx, tbl, k, 0),
        }
    }

    /// `sort_total_at` of `sort_cosplit_begin`'s open: under `SORT_MODE_SLICES` the claim's last part's end (a part's
    /// outer range is its rows' place in the parts' order, `[the rows before it, and its own)`).
    pub unsafe fn sort_cosplit_total(ctx: *mut Ctx, tbl: *const u32, mode: u64, k: u64, ctab: *const u64) -> i64 {
        match (mode as i64 == SORT_MODE_SLICES, claim_parts(ctx, ctab)) {
            (true, Some((_, b))) => *part_word(ctab, b - 1, 1) as i64,
            _ => sort_total_at(ctx, tbl, mode, k, 0),
        }
    }

    /// `sort_next_at` of `sort_cosplit_begin`'s open: under `SORT_MODE_SLICES` row `i` of the claim's range (the emit's
    /// cursor starts at its first part's place) is the merge's row `i` less that place, over this owner's area.
    pub unsafe fn sort_cosplit_next(ctx: *mut Ctx, tbl: *const u32, mode: u64, i: u64, k: u64, ctab: *const u64) -> *const u8 {
        match (mode as i64 == SORT_MODE_SLICES, claim_parts(ctx, ctab)) {
            (true, Some((a, _))) => {
                let area = *owner_word(ctab, (*ctx).worker_ordinal) as usize as *mut u8;
                let i = i.wrapping_sub(*part_word(ctab, a, 0));
                guard!(ERROR as *const u8, next_slices(ctx, area, tbl, i, STREAM_CONSUMED | k))
            }
            _ => sort_next_at(ctx, tbl, mode, i, k, 0),
        }
    }
}
