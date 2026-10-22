//! The `par_topk` kernel and its emit (`MAX_N` and `bytes` stay in `par_topk.rs`).
#![allow(unused_imports)]
use gravity_abi::PtrWord;
use super::*;
use crate::util::{advance, done, emit, end, for_each_row, lane, partial, range_seek, site as site_of, stopped};
use crate::util::HOST_STOP;
use gravity_abi::status::{unsupported, DONE, ERROR, UNSUPPORTED_SCRATCH};
use gravity_abi::{Ctx, Datum};
use gravity_abi::ctx::RESUME_NONE;

/// `SELECT cols FROM t ORDER BY cols[0] LIMIT n` over the zone walk (§1.10 increment 3 step
pub unsafe extern "C-unwind" fn par_topk(ctx: *mut Ctx) -> i64 {
    if range_seek(ctx, 0) < 0 {
        return ERROR;
    }
    let n = *(*ctx).params.ptr() as u32;
    let natts = *(*ctx).params.ptr().add(1) as usize;
    let run = TopK::at(partial(ctx), n, natts as u32);
    // the gathered row's cells, once per call from the arena window (a per-program width, never a cap)
    let vp = gravity_abi::ctx::arena_alloc(ctx, natts * 9);
    if vp.is_null() {
        return unsupported(UNSUPPORTED_SCRATCH);
    }
    let v = core::slice::from_raw_parts_mut(vp as *mut u64, natts);
    let nl = core::slice::from_raw_parts_mut(vp.add(natts * 8), natts);
    loop {
        let r = advance(ctx, 0);
        if r <= 0 {
            return if r == 0 { DONE } else { r };
        }
        let s = site_of(ctx, 0);
        let key = lane(s, 0);
        let mut out = 0u64;
        for_each_row(s, |i| {
            if !key.is_valid(i) {
                return true;
            }
            let k = key.datum(i) as i64;
            if let Some(b) = run.bound() {
                if k >= b {
                    return true;
                }
            }
            for (c, (vc, nc)) in v.iter_mut().zip(nl.iter_mut()).enumerate() {
                let l = lane(s, c);
                *vc = l.datum(i);
                *nc = !l.is_valid(i) as u8;
            }
            out += run.insert(v, nl) as u64;
            true
        });
        s.rows_out += out;
        s.cursor = end(s);
    }
}

/// # Safety
pub unsafe extern "C-unwind" fn par_topk_emit(ctx: *mut Ctx) -> i64 {
    let n = *(*ctx).params.ptr() as u32;
    let natts = *(*ctx).params.ptr().add(1) as usize;
    let run = TopK::at(partial(ctx), n, natts as u32);
    let mut i = if (*ctx).resume == RESUME_NONE { 0 } else { (*ctx).resume as usize };
    while i < run.len as usize {
        let (v, nl) = run.row(i);
        i += 1;
        let f = emit(ctx, natts, &[(v.as_ptr() as *const Datum, nl.as_ptr(), natts)]);
        if f < 0 {
            return ERROR;
        }
        if f == HOST_STOP {
            (*ctx).resume = i as i64;
            return stopped(ctx);
        }
    }
    done(ctx)
}
