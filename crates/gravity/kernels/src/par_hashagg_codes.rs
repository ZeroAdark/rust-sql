use gravity_abi::PtrWord;
use gravity_abi::ctx::{hdr, lane, RESUME_NONE};
use gravity_abi::status::{unsupported, UNSUPPORTED_SCRATCH, UNSUPPORTED_SHAPE};
use gravity_abi::Ctx;
use gravity_abi::status::{DONE, ERROR};
use gravity_abi::breaker::{HashAgg, KeyCell, KeyKind, RecordLayout, SlotKind};
use crate::util::{HOST_STOP};

use crate::util::{advance, done, emit, end, for_each_row, int8_datum, lane, partial, range_seek, site as site_of, stopped, Cell};
use crate::{Touch, SITE_EXEC_CELLS};

/// `(part, code) → count`; the leader's value fold (`runner::fold_codes_by_value`) leaves the
/// rendered value datum in the key word, which the emit reads.
pub const LAYOUT: RecordLayout = RecordLayout::new(&[KeyKind::Code], &[SlotKind::Count]);
pub const INIT: u32 = crate::table::params::INIT;
pub const SLOTS: u32 = 4096;
pub const BYTES: u32 = HashAgg::bytes(&LAYOUT, SLOTS);
pub const TOUCH: Touch = Touch { hdr: &[hdr::RESUME], sites: &[(0, SITE_EXEC_CELLS)] };
/// The code bits of a two-level key: `part_id << CODE_BITS | code` (step 4's T4 fold).
pub const CODE_BITS: u32 = 20;
pub const MAX_PARTS: u32 = 1 << (31 - CODE_BITS);

/// `SELECT t, count(*) FROM t GROUP BY t` over a dictionary code lane: the partial counts by
/// (part, code); the leader crosses parts by value (`after_pipeline`), the datum in the key word.
/// # Safety
/// `ctx` is laid out per `programs::par_hashagg_codes` with `partial` at a `BYTES` region for a `LAYOUT` table.
pub unsafe extern "C-unwind" fn par_hashagg_codes(ctx: *mut Ctx) -> i64 {
    if range_seek(ctx, 0) < 0 {
        return ERROR;
    }
    let table = HashAgg::at(partial(ctx), &LAYOUT, INIT, BYTES);
    loop {
        let r = advance(ctx, 0);
        if r <= 0 {
            return if r == 0 { DONE } else { r };
        }
        let s = site_of(ctx, 0);
        let l = lane(s, 0);
        if l.kind != lane::KIND_CODES || s.part_id >= MAX_PARTS || l.ncodes_or_scale > 1 << CODE_BITS {
            return unsupported(UNSUPPORTED_SHAPE);
        }
        let part = s.part_id;
        let mut out = 0u64;
        let ok = for_each_row(s, |i| {
            if !l.is_valid(i) {
                return true;
            }
            let code = crate::by_width!(l.width, code_at(l.ptr.ptr(), i));
            out += 1;
            match table.find_or_insert(&[KeyCell::code_word(part, code)]) {
                Some(r) => {
                    r.add_count(0, 1);
                    true
                }
                None => false,
            }
        });
        s.rows_out += out;
        if !ok {
            return unsupported(UNSUPPORTED_SCRATCH);
        }
        s.cursor = end(s);
    }
}

#[inline(always)]
unsafe fn code_at<T: Cell>(p: *const u8, i: usize) -> u32 {
    (*(p as *const T).add(i)).i64() as u32
}

/// # Safety
/// `ctx` is laid out per `programs::par_hashagg_codes` with `partial` at the value-folded table.
pub unsafe extern "C-unwind" fn par_hashagg_codes_emit(ctx: *mut Ctx) -> i64 {
    let table = HashAgg::at(partial(ctx), &LAYOUT, INIT, BYTES);
    let mut i = if (*ctx).resume == RESUME_NONE { 0 } else { (*ctx).resume as usize };
    while i < table.cap as usize {
        let r = table.record(i);
        i += 1;
        if !r.used() {
            continue;
        }
        let v = [r.key(0), int8_datum(r.count(0))];
        let n = [0u8; 2];
        let f = emit(ctx, 2, &[(v.as_ptr(), n.as_ptr(), 2)]);
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
