use gravity_abi::PtrWord;
use gravity_abi::ctx::{hdr, resume, RESUME_NONE};
use gravity_abi::{Ctx, Datum};
use gravity_abi::status::{DONE, ERROR};
use gravity_abi::breaker::{Breaker, BreakerKind, Budget, SortRun, Spill};
use gravity_abi::program::{SPILL_RUN_PUT};
use crate::util::{HOST_STOP};

use crate::ops::sort::runmerge;
use crate::ops::sort::runmerge::{norm_i64, MERGING};
use crate::util::{advance, emit, end, flush, for_each_row, lane, partial, range_seek, site as site_of};
use crate::{Touch, SITE_EXEC_CELLS};

/// `SELECT cols FROM t ORDER BY cols[0] [DESC] [NULLS FIRST]` (§2 decision 12, `breakers.md`):
/// the worker's rows are collected in its share as normalised rows across its claims, sorted and
/// written as one run at the section's end (`SECTION_END`, item 20; at overflow too), and the
/// emit k-way merges the runs.
pub const TOUCH: Touch = Touch { hdr: &[hdr::RESUME], sites: &[(0, SITE_EXEC_CELLS)] };
/// The kernel words: the output width, the key's direction (1 = DESC), its null placement (1 = NULLS FIRST).
pub const PARAM_NATTS: usize = 0;
pub const PARAM_DESC: usize = 1;
pub const PARAM_NULLS_FIRST: usize = 2;
pub const NPARAMS: usize = 3;

pub const fn params(natts: u32, desc: bool, nulls_first: bool) -> [u64; NPARAMS] {
    [natts as u64, desc as u64, nulls_first as u64]
}

pub const BREAKER: Breaker = Breaker { kind: BreakerKind::Sort, slot: 0, bytes: 0, budget: Budget::Share, spill: Spill::Runs };

/// A row: `[flags][key][natts datums][natts null bytes]`, 8-aligned; `(flags, key)` compare as two
/// unsigned words — a NULL key sorts by `flags` alone (0 first, 2 last), a value by `key` (its
/// order-preserving image, inverted for DESC).
pub const fn row_bytes(natts: usize) -> usize {
    16 + natts * 8 + ((natts + 7) & !7)
}

struct Params {
    natts: usize,
    desc: bool,
    nulls_first: bool,
}

unsafe fn params_of(ctx: *mut Ctx) -> Params {
    let p = (*ctx).params.ptr();
    Params { natts: (*p.add(PARAM_NATTS) as usize).max(1), desc: *p.add(PARAM_DESC) != 0, nulls_first: *p.add(PARAM_NULLS_FIRST) != 0 }
}

/// # Safety
/// `ctx` is laid out per `programs::par_sort`: `partial` at a share-armed region, `NPARAMS` words.
pub unsafe extern "C-unwind" fn par_sort(ctx: *mut Ctx) -> i64 {
    let p = params_of(ctx);
    let rb = row_bytes(p.natts);
    // the run buffer as a raw pointer: the put host clears it mid-claim
    let run = SortRun::at_share(partial(ctx), rb as u32) as *mut SortRun;
    if (*ctx).claim.is_section_end() {
        return put_run(ctx, run);
    }
    if range_seek(ctx, 0) < 0 {
        return ERROR;
    }
    loop {
        let r = advance(ctx, 0);
        if r < 0 {
            return r;
        }
        if r == 0 {
            return DONE;
        }
        let s = site_of(ctx, 0);
        let key = lane(s, 0);
        let mut out = 0u64;
        let mut code = 0i64;
        let ok = for_each_row(s, |i| {
            let (flags, k) = if key.is_valid(i) { (1u64, if p.desc { !norm_i64(key.datum(i) as i64) } else { norm_i64(key.datum(i) as i64) }) } else { (if p.nulls_first { 0 } else { 2 }, 0) };
            if (*run).len == (*run).cap && put_run(ctx, run) < 0 {
                code = ERROR;
                return false;
            }
            let row = (*run).push();
            *(row as *mut u64) = flags;
            *(row.add(8) as *mut u64) = k;
            let (dv, dn) = (row.add(16) as *mut u64, row.add(16 + p.natts * 8));
            for c in 0..p.natts {
                let l = lane(s, c);
                *dv.add(c) = l.datum(i);
                *dn.add(c) = !l.is_valid(i) as u8;
            }
            out += 1;
            true
        });
        s.rows_out += out;
        if !ok {
            return code;
        }
        s.cursor = end(s);
    }
}

/// The buffer sorted and put as the worker's next run (the host clears it); nothing for an empty one.
pub unsafe fn put_run(ctx: *mut Ctx, run: *mut SortRun) -> i64 {
    if (*run).len == 0 {
        return DONE;
    }
    sort_rows(run);
    if crate::util::spill(ctx, SPILL_RUN_PUT, 0) < 0 {
        return ERROR;
    }
    DONE
}

/// The rows ordered by their two key words: `sort_unstable` over `[u64; N]` views at the served
/// widths (16 B to 96 B, 8-aligned), the abi's heap sort past them.
unsafe fn sort_rows(run: *mut SortRun) {
    let (n, rb) = ((*run).len as usize, (*run).row_bytes as usize);
    let base = (*run).row(0) as *mut u64;
    macro_rules! fixed {
        ($($w:literal),*) => {
            match rb / 8 {
                $($w => core::slice::from_raw_parts_mut(base as *mut [u64; $w], n).sort_unstable_by_key(|r| (r[0], r[1])),)*
                _ => (*run).sort(),
            }
        };
    }
    fixed!(2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12)
}

unsafe fn key_of(row: *const u8) -> (u64, u64) {
    (*(row as *const u64), *(row.add(8) as *const u64))
}

unsafe fn emit_row(ctx: *mut Ctx, row: *const u8) -> i64 {
    let natts = params_of(ctx).natts;
    let f = emit(ctx, natts, &[(row.add(16) as *const Datum, row.add(16 + natts * 8), natts)]);
    if f < 0 {
        return ERROR;
    }
    f
}

/// # Safety
/// `ctx` is laid out per `programs::par_sort` with `partial` at the sort slot's leader region.
pub unsafe extern "C-unwind" fn par_sort_emit(ctx: *mut Ctx) -> i64 {
    let own = partial(ctx);
    let word = (*ctx).resume;
    if word == RESUME_NONE {
        let rb = row_bytes(params_of(ctx).natts);
        let usable = SortRun::region_bytes(own, rb as u32);
        match runmerge::open(ctx, own, usable, rb, key_of) {
            Err(e) => return e,
            Ok(false) => {
                (*ctx).resume = RESUME_NONE;
                return if flush(ctx) < 0 { ERROR } else { DONE };
            }
            Ok(true) => {}
        }
    } else if resume::position(word) & MERGING == 0 {
        return ERROR;
    }
    let r = runmerge::drain(ctx, own, key_of, emit_row);
    let _ = HOST_STOP;
    r
}
