use gravity_abi::PtrWord;
use gravity_abi::ctx::{hdr, lane};
use gravity_abi::{Ctx, Lane, Site};
use gravity_abi::status::{DONE, ERROR};
use gravity_abi::breaker::{HashAgg, RecordLayout, SlotKind};

use crate::util::{advance, done, emit, end, int4_of, int8_datum, lane, partial, range_seek, row_valid, site as site_of, Cell};
use crate::{Touch, SITE_EXEC_CELLS};

pub const THR_PARAM: usize = 0;
/// The plain aggregate's record: no keys, one `Count` slot (16 B; a one-record table).
pub const LAYOUT: RecordLayout = RecordLayout::new(&[], &[SlotKind::Count]);
pub const BYTES: u32 = HashAgg::bytes(&LAYOUT, 1);
pub const TOUCH: Touch = Touch { hdr: &[hdr::RESUME], sites: &[(0, SITE_EXEC_CELLS)] };

/// # Safety
/// `ctx` is laid out per `programs::par_scan_count` with `partial` at a one-record table of `LAYOUT`.
pub unsafe extern "C-unwind" fn par_scan_count(ctx: *mut Ctx) -> i64 {
    if range_seek(ctx, 0) < 0 {
        return ERROR;
    }
    let thr = int4_of(*(*ctx).params.ptr().add(THR_PARAM));
    let thr_null = *(*ctx).params_null.ptr().add(THR_PARAM) != 0;
    let agg = HashAgg::at(partial(ctx), &LAYOUT, 1, BYTES).single();
    loop {
        let r = advance(ctx, 0);
        if r <= 0 {
            return if r == 0 { DONE } else { r };
        }
        let s = site_of(ctx, 0);
        if !thr_null {
            // an answered window (rows, no lanes) and an AllMatch verdict both count the window's
            // rows without reading the lane (which may be undecoded)
            let hits = if s.answered != 0 {
                (end(s) - s.cursor) as i64
            } else {
                let l = lane(s, 0);
                if l.flags & lane::VERDICT_MASK == lane::VERDICT_ALL_MATCH { (end(s) - s.cursor) as i64 } else { crate::by_width!(l.width, count_lt(s, l, thr)) }
            };
            agg.add_count(0, hits);
            s.rows_out += hits as u64;
        }
        s.cursor = end(s);
    }
}

#[inline(always)]
unsafe fn count_lt<T: Cell>(s: &Site, l: &Lane, thr: i32) -> i64 {
    let v = l.ptr.ptr() as *const T;
    let from = s.cursor as usize;
    if s.sel.ptr().is_null() {
        let vals = core::slice::from_raw_parts(v.add(from), s.rows as usize - from);
        if l.valid.ptr().is_null() {
            return vals.iter().filter(|x| x.i32() < thr).count() as i64;
        }
        return vals.iter().enumerate().map(|(i, x)| (row_valid(l.valid.ptr(), from + i) && x.i32() < thr) as i64).sum();
    }
    let mut c = 0i64;
    for j in s.cursor..s.sel_n {
        let i = *s.sel.ptr().add(j as usize) as usize;
        c += (row_valid(l.valid.ptr(), i) && (*v.add(i)).i32() < thr) as i64;
    }
    c
}

/// # Safety
/// `ctx` is laid out per `programs::par_scan_count` with `partial` at the merged one-record table.
pub unsafe extern "C-unwind" fn par_scan_count_emit(ctx: *mut Ctx) -> i64 {
    let agg = HashAgg::at(partial(ctx), &LAYOUT, 1, BYTES).single();
    let v = [int8_datum(agg.count(0))];
    let n = [0u8];
    if emit(ctx, 1, &[(v.as_ptr(), n.as_ptr(), 1)]) < 0 {
        return ERROR;
    }
    done(ctx)
}
