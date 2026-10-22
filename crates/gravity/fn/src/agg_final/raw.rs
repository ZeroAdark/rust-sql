//! The numeric varlena reads of agg_final.rs's mantissa hosts (`num_mantissa`, D4567's `num_mantissa_within`): a
//! datum's header parsed in place into its payload's digits.
use adt_numeric::Num;

use super::{mantissa_digits, mantissa_of, NO_MANTISSA};

/// D4567: a numeric varlena's mantissa at `scale` when its display scale is past `scale` but no digit lies below it (a
/// quotient exact at a sum's scale, shown at its larger select_div_scale scale); `NO_MANTISSA` where one does, and as
/// `num_mantissa` otherwise. # Safety — `datum` is a live varlena (1- or 4-byte header).
pub unsafe fn num_mantissa_within(datum: *const u8, scale: i64) -> i64 {
    datum_mantissa(datum, scale, true)
}

/// `num_mantissa`'s read; `within`: a display scale past `scale` too. # Safety — as `num_mantissa`.
pub(super) unsafe fn datum_mantissa(datum: *const u8, scale: i64, within: bool) -> i64 {
    let b0 = *datum;
    // a 1-byte header's payload is odd-aligned: `Num` reads 2-byte digits, so it is copied out (at most 126 bytes)
    let mut short = [0u16; 64];
    let bytes = if b0 & 1 != 0 {
        if b0 == 1 {
            return NO_MANTISSA;
        }
        let n = (b0 >> 1) as usize - 1;
        core::ptr::copy_nonoverlapping(datum.add(1), short.as_mut_ptr().cast::<u8>(), n);
        core::slice::from_raw_parts(short.as_ptr().cast::<u8>(), n)
    } else {
        let w = datum.cast::<u32>().read_unaligned();
        if w & 3 != 0 {
            return NO_MANTISSA;
        }
        core::slice::from_raw_parts(datum.add(4), (w >> 2) as usize - 4)
    };
    let num = Num::from_payload(bytes);
    match within && !num.is_special() && num.dscale() > scale as i32 {
        true => mantissa_digits(num, scale as i32),
        false => mantissa_of(num, scale as i32),
    }
}
