//! The final-function shims the kernels call at emit (docs/gravity/expr.md §2.1): the aggregate's
//! transition state is the kernel's own record (a sum and a count in registers or a breaker row),
//! and only the PostgreSQL result type is produced here — through the same numeric routines the
//! fmgr final functions use.

use adt_numeric::Num;

use crate::{fail, null_ret, ok, region_copy, region_cstring, ErrSlot, Region, Ret, View};

mod raw;

gravity_abi::host! {
    #[unwind]
    pub fn int_avg(e: *mut ErrSlot, r: *mut Region, sum: i64, count: i64) -> Ret {
        if count == 0 {
            return null_ret();
        }
        match adt_numeric::int64_avg_div(sum, count) {
            Ok(img) => region_copy(r, img.as_bytes()),
            Err(err) => fail(e, err),
        }
    }
}

gravity_abi::host! {
    #[unwind]
    pub fn int8_avg(e: *mut ErrSlot, r: *mut Region, sum_lo: i64, sum_hi: i64, count: i64) -> Ret {
        if count == 0 {
            return null_ret();
        }
        match adt_numeric::int128_avg_div(i128_of(sum_lo, sum_hi), count) {
            Ok(img) => region_copy(r, img.as_bytes()),
            Err(err) => fail(e, err),
        }
    }
}

gravity_abi::host! {
    /// D4565: `numeric_poly_{var,stddev}_{pop,samp}` over a register Int128AggState (sumX, sumX2, N) of int2 / int4
    /// inputs: `numeric_poly_stddev_internal` itself; `stddev` and `sample` pick the catalog final. NULL as PG's (no row,
    /// or one row for a sample form).
    #[unwind]
    pub fn int128_stat(e: *mut ErrSlot, r: *mut Region, sx_lo: i64, sx_hi: i64, sq_lo: i64, sq_hi: i64, n: i64, stddev: bool, sample: bool) -> Ret {
        let st = adt_numeric::Int128AggState { calc_sum_x2: true, n, sum_x: i128_of(sx_lo, sx_hi), sum_x2: i128_of(sq_lo, sq_hi) };
        match adt_numeric::aggregates::numeric_poly_stddev_internal(Some(&st), !stddev, sample) {
            Ok(Some(img)) => region_copy(r, img.as_bytes()),
            Ok(None) => null_ret(),
            Err(err) => fail(e, err),
        }
    }
}

gravity_abi::host! {
    #[unwind]
    pub fn int8_sum(e: *mut ErrSlot, r: *mut Region, sum_lo: i64, sum_hi: i64, count: i64) -> Ret {
        if count == 0 {
            return null_ret();
        }
        let sum = i128_of(sum_lo, sum_hi);
        if let Ok(v) = i64::try_from(sum) {
            return region_copy(r, adt_numeric::int64_to_numeric(v).as_bytes());
        }
        // Beyond i64: the exact quotient by 1 is the number itself at scale 0.
        match adt_numeric::int128_avg_div(sum, 1) {
            Ok(img) => match adt_numeric::numeric_trunc_common(img.num(), 0) {
                Ok(t) => region_copy(r, t.as_bytes()),
                Err(err) => fail(e, err),
            },
            Err(err) => fail(e, err),
        }
    }
}

gravity_abi::host! {
    /// `sum(numeric(p, s))` over an i128 mantissa at `scale`: the image `fixed_scale_unpack_i128` builds (PostgreSQL's
    /// sum carries the inputs' shared dscale). # Safety — as `int8_avg`.
    #[unwind]
    pub fn num_sum(e: *mut ErrSlot, r: *mut Region, sum_lo: i64, sum_hi: i64, scale: i64) -> Ret {
        if sum_hi == SENT_HI {
            return special_ret(e, r, sum_lo);
        }
        match adt_numeric::fixed_scale::fixed_scale_unpack_i128(i128_of(sum_lo, sum_hi), scale as i32) {
            Ok(img) => region_copy(r, img.as_bytes()),
            Err(err) => fail(e, err),
        }
    }
}

gravity_abi::host! {
    /// D4567: `num_sum` of a mantissa at `scale` whose display scale `d` is past it (a sum of quotients exact at `scale`
    /// whose select_div_scale display scales are larger): the image at `scale`, its dscale `d` (numeric.c prints the
    /// digits it lacks as zeros). # Safety — as `num_sum`.
    #[unwind]
    pub fn num_sum_pad(e: *mut ErrSlot, r: *mut Region, sum_lo: i64, sum_hi: i64, scale: i64, d: i64) -> Ret {
        if sum_hi == SENT_HI {
            return special_ret(e, r, sum_lo);
        }
        match padded(i128_of(sum_lo, sum_hi), scale, d) {
            Ok(img) => region_copy(r, img.as_bytes()),
            Err(err) => fail(e, err),
        }
    }
}

gravity_abi::host! {
    /// D4567: `num_avg` over `num_sum_pad`'s image: numeric_avg divides the sum at its display scale `d`
    /// (`select_div_scale` reads it). # Safety — as `num_sum`.
    #[unwind]
    pub fn num_avg_pad(e: *mut ErrSlot, r: *mut Region, sum_lo: i64, sum_hi: i64, count: i64, scale: i64, d: i64) -> Ret {
        if count == 0 {
            return null_ret();
        }
        if sum_hi == SENT_HI {
            return special_ret(e, r, sum_lo);
        }
        match padded(i128_of(sum_lo, sum_hi), scale, d).and_then(|img| adt_numeric::numeric_avg_div(img.num(), count)) {
            Ok(img) => region_copy(r, img.as_bytes()),
            Err(err) => fail(e, err),
        }
    }
}

/// D4567: mantissa `m` at `scale` as an image of display scale `d` (at least `scale`): its digits, then zeros.
fn padded(m: i128, scale: i64, d: i64) -> types_error::PgResult<adt_numeric::NumericImage> {
    let img = adt_numeric::fixed_scale::fixed_scale_unpack_i128(m, scale as i32)?;
    let mut v = adt_numeric::NumericVar::from_view(img.num().view());
    v.dscale = d.max(scale) as i32;
    adt_numeric::make_result(v.view())
}

/// A sum slot's sticky special (lowerv2 M6 §4.10): `hi == SENT_HI`, `lo` the code — a NaN, `Infinity` or `-Infinity` sum as PostgreSQL's.
pub const SENT_HI: i64 = i64::MIN;
pub const SP_NAN: i64 = 1;
pub const SP_PINF: i64 = 2;
pub const SP_NINF: i64 = 3;
/// D299: a finite sum past the mantissa sum's range (|sum| >= 2^126, `SUM_HI_LIMIT`): sticky under a finite add, lost to
/// NaN and to an infinity as a finite sum is; the final raises (22003) instead of printing a value.
pub const SP_OVERFLOW: i64 = 4;
/// D299: a finite mantissa sum keeps its hi word in [-SUM_HI_LIMIT, SUM_HI_LIMIT): the packed sum's format bound, one
/// definition in the layout home (gravity_abi breaker.rs, beside the sum add that tests it).
pub use gravity_abi::breaker::NUM_SUM_HI_LIMIT as SUM_HI_LIMIT;

fn special_ret(e: *mut ErrSlot, r: *mut Region, code: i64) -> Ret {
    let sign = match code {
        SP_NAN => adt_numeric::NUMERIC_NAN,
        SP_PINF => adt_numeric::NUMERIC_PINF,
        SP_NINF => adt_numeric::NUMERIC_NINF,
        _ => return fail(e, Box::new(crate::PgError::error("gravity: a numeric sum past the packed sum's range (2^126)".to_string()).with_sqlstate(types_error::ERRCODE_NUMERIC_VALUE_OUT_OF_RANGE))), // alloc: Cold
    };
    let mut var = adt_numeric::NumericVar::new();
    var.sign = sign;
    match adt_numeric::make_result(var.view()) {
        Ok(img) => region_copy(r, img.as_bytes()),
        Err(err) => fail(e, err),
    }
}

gravity_abi::host! {
    /// `avg(numeric(p, s))`: the mantissa sum's image divided by the count as `numeric_avg` divides (`select_div_scale`).
    #[unwind]
    pub fn num_avg(e: *mut ErrSlot, r: *mut Region, sum_lo: i64, sum_hi: i64, count: i64, scale: i64) -> Ret {
        if count == 0 {
            return null_ret();
        }
        if sum_hi == SENT_HI {
            return special_ret(e, r, sum_lo);
        }
        match adt_numeric::fixed_scale::fixed_scale_unpack_i128(i128_of(sum_lo, sum_hi), scale as i32).and_then(|img| adt_numeric::numeric_avg_div(img.num(), count)) {
            Ok(img) => region_copy(r, img.as_bytes()),
            Err(err) => fail(e, err),
        }
    }
}

gravity_abi::host! {
    /// `min` / `max(numeric(p, s))`: one mantissa rendered back (byte-identical to the stored datum: the digits are canonical,
    /// the dscale the column's), a sentinel as its special image.
    #[unwind]
    pub fn num_word(e: *mut ErrSlot, r: *mut Region, m: i64, scale: i64) -> Ret {
        let special = match m {
            MANT_NAN => Some(adt_numeric::NUMERIC_NAN),
            MANT_PINF => Some(adt_numeric::NUMERIC_PINF),
            MANT_NINF => Some(adt_numeric::NUMERIC_NINF),
            _ => None,
        };
        let img = match special {
            Some(sign) => {
                let mut var = adt_numeric::NumericVar::new();
                var.sign = sign;
                adt_numeric::make_result(var.view())
            }
            None => adt_numeric::fixed_scale::fixed_scale_unpack_i128(m as i128, scale as i32),
        };
        match img {
            Ok(img) => region_copy(r, img.as_bytes()),
            Err(err) => fail(e, err),
        }
    }
}

/// `min` / `max` sentinels in a mantissa word: `-Infinity` below every finite value, `Infinity` above, `NaN` above that
/// (`cmp_numerics`' order); `NO_MANTISSA` (a value the lane does not carry) is never a legal mantissa.
pub const MANT_NINF: i64 = i64::MIN + 1;
pub const MANT_PINF: i64 = i64::MAX - 1;
pub const MANT_NAN: i64 = i64::MAX;
pub const NO_MANTISSA: i64 = i64::MIN;

/// D3139: numeric_float8 of mantissa `m` at `scale` (a packed lane's code: the value `num_word` renders), through adt_numeric's
/// exact scaled-integer cast (D3106, bit for bit the cast of that image); `None` for a sentinel or outside its exact path.
pub fn mantissa_float8(m: i64, scale: i64) -> Option<f64> {
    if matches!(m, MANT_NAN | MANT_PINF | MANT_NINF | NO_MANTISSA) {
        return None;
    }
    adt_numeric::fixed_scale::fixed_scale_to_f64(m as i128, i32::try_from(scale).ok()?)
}

/// D3139: numeric_float4 of mantissa `m` at `scale`, as [`mantissa_float8`].
pub fn mantissa_float4(m: i64, scale: i64) -> Option<f32> {
    if matches!(m, MANT_NAN | MANT_PINF | MANT_NINF | NO_MANTISSA) {
        return None;
    }
    adt_numeric::fixed_scale::fixed_scale_to_f32(m as i128, i32::try_from(scale).ok()?)
}

gravity_abi::host! {
    /// A numeric varlena's mantissa at `scale` (`value × 10^scale` as an int8: the digit walk of `fixed_scale_fit` at the
    /// int8 budget), the special values as the sentinels, `NO_MANTISSA` for another display scale, a toasted image or a
    /// mantissa past int8. # Safety — `datum` is a live varlena (1- or 4-byte header).
    #[unwind]
    pub unsafe fn num_mantissa(_e: *mut ErrSlot, datum: *const u8, scale: i64) -> Ret {
        ok(datum_mantissa(datum, scale, false))
    }
}

pub use raw::num_mantissa_within;
use raw::datum_mantissa;

gravity_abi::host! {
    /// The order of the mantissa `m` at `scale` (a packed lane's word, or `num_mantissa`'s sentinel for a special) against the numeric
    /// varlena `datum` at any scale, `cmp_numerics`' order: -1 / 0 / 1, or 2 for an image it cannot read (toasted). No allocation: the
    /// datum's digits are folded to `floor(|value| × 10^scale)` and whether a fraction is left (`x < 0.2 * avg(x)`).
    /// # Safety — `datum` is a live varlena (1- or 4-byte header).
    #[unwind]
    pub unsafe fn num_cmp_mantissa(_e: *mut ErrSlot, m: i64, scale: i64, datum: *const u8) -> Ret {
        let b0 = *datum;
        let mut short = [0u16; 64];
        let bytes = if b0 & 1 != 0 {
            if b0 == 1 {
                return ok(2);
            }
            let n = (b0 >> 1) as usize - 1;
            core::ptr::copy_nonoverlapping(datum.add(1), short.as_mut_ptr().cast::<u8>(), n);
            core::slice::from_raw_parts(short.as_ptr().cast::<u8>(), n)
        } else {
            let w = datum.cast::<u32>().read_unaligned();
            if w & 3 != 0 {
                return ok(2);
            }
            core::slice::from_raw_parts(datum.add(4), (w >> 2) as usize - 4)
        };
        ok(cmp_mantissa(m, scale as i32, Num::from_payload(bytes)))
    }
}

/// `num_cmp_mantissa`'s order over a read image: the specials by rank (-Infinity < finite < Infinity < NaN), else `m` against
/// `sign × (q + frac)` with `q` saturated past every int8 mantissa.
pub fn cmp_mantissa(m: i64, scale: i32, num: Num<'_>) -> i64 {
    let rank = |m: i64| match m {
        MANT_NINF => 0,
        MANT_PINF => 2,
        MANT_NAN => 3,
        _ => 1,
    };
    let nrank = if num.is_nan() {
        3
    } else if num.is_pinf() {
        2
    } else if num.is_ninf() {
        0
    } else {
        1
    };
    if rank(m) != 1 || nrank != 1 {
        return (rank(m) as i64 - nrank as i64).signum();
    }
    // q saturates at 2^100, far past every int8 mantissa m (< 2^63): the comparison's answer is unchanged and the i128
    // sum cannot overflow.
    const CAP: i128 = 1 << 100;
    let (mut q, mut frac) = (0i128, false);
    for (i, &d) in num.digits().iter().enumerate() {
        let e = 4 * (num.weight() - i as i32) + scale;
        if e >= 0 {
            q = 10i128.checked_pow(e as u32).and_then(|p| p.checked_mul(d as i128)).and_then(|t| q.checked_add(t)).map_or(CAP, |v| v.min(CAP));
        } else if e > -38 {
            let p = 10i128.pow((-e) as u32);
            q += d as i128 / p;
            frac |= d as i128 % p != 0;
        } else {
            frac |= d != 0;
        }
    }
    let m = m as i128;
    if num.sign() == adt_numeric::NUMERIC_NEG && (q != 0 || frac) {
        // the value is -(q + frac): above -q only when m is; at -q, m is above by the fraction
        return if m > -q || (m == -q && frac) { 1 } else if m == -q { 0 } else { -1 };
    }
    if m < q || (m == q && frac) {
        -1
    } else if m == q {
        0
    } else {
        1
    }
}

fn mantissa_of(num: Num<'_>, scale: i32) -> i64 {
    if num.is_nan() {
        return MANT_NAN;
    }
    if num.is_pinf() {
        return MANT_PINF;
    }
    if num.is_ninf() {
        return MANT_NINF;
    }
    if num.dscale() != scale {
        return NO_MANTISSA;
    }
    mantissa_digits(num, scale)
}

/// A finite `num`'s digits as a mantissa at `scale` (`NO_MANTISSA` for a digit below it or past the word).
fn mantissa_digits(num: Num<'_>, scale: i32) -> i64 {
    let mut m: i128 = 0;
    for (i, &d) in num.digits().iter().enumerate() {
        let e = 4 * (num.weight() - i as i32) + scale;
        let term = if e >= 0 {
            match 10i128.checked_pow(e as u32).and_then(|p| p.checked_mul(d as i128)) {
                Some(t) => t,
                None => return NO_MANTISSA,
            }
        } else {
            let p = 10i128.pow((-e) as u32);
            if d as i128 % p != 0 {
                return NO_MANTISSA;
            }
            d as i128 / p
        };
        m = match m.checked_add(term) {
            Some(v) => v,
            None => return NO_MANTISSA,
        };
    }
    if m >= MANT_PINF as i128 {
        return NO_MANTISSA;
    }
    if num.sign() == adt_numeric::NUMERIC_NEG { -(m as i64) } else { m as i64 }
}

gravity_abi::host! {
    #[unwind]
    pub fn float8_avg(_e: *mut ErrSlot, n: i64, sum: f64) -> Ret {
        if n == 0 {
            return null_ret();
        }
        ok((sum / n as f64).to_bits() as i64)
    }
}

gravity_abi::host! {
    #[unwind]
    pub fn float8_sum(_e: *mut ErrSlot, n: i64, sum: f64) -> Ret {
        if n == 0 {
            return null_ret();
        }
        ok(sum.to_bits() as i64)
    }
}

gravity_abi::host! {
    #[unwind]
    pub fn numeric_cmp(_e: *mut ErrSlot, a: View, b: View) -> Ret {
        ok(adt_numeric::cmp_numerics(Num::from_payload(a.bytes()), Num::from_payload(b.bytes())) as i64)
    }
}

gravity_abi::host! {
    #[unwind]
    pub fn numeric_text(_e: *mut ErrSlot, r: *mut Region, a: View) -> Ret {
        crate::shims::numeric::with_out_buf(|buf| {
            adt_numeric::numeric_out_into(Num::from_payload(a.bytes()), buf);
            region_cstring(r, buf)
        })
    }
}

gravity_abi::host! {
    /// V4R-38 (B-lite): an i256 numeric mantissa sum (`gravity_abi::breaker::NumWide`: four little-endian words of two's
    /// complement, a special the `SENT_HI` sentinel on the top word with its code in word 0) as PostgreSQL's numeric at
    /// `scale`: its decimal digits read back by `numeric_in` (dscale = the digits written after the point = `scale`, the image
    /// canonical as `make_result` builds PG's sum). # Safety — `w` points at four readable words.
    #[unwind]
    #[inline(always)]
    pub unsafe fn num_sum256(e: *mut ErrSlot, r: *mut Region, w: *const u64, scale: i64) -> Ret {
        let w = core::ptr::read_unaligned(w.cast::<[u64; 4]>());
        if w[3] as i64 == SENT_HI {
            return special_ret(e, r, w[0] as i64);
        }
        match wide_image(w, scale) {
            Ok(img) => region_copy(r, img.as_bytes()),
            Err(err) => fail(e, err),
        }
    }
}

gravity_abi::host! {
    /// `avg(numeric)` over an i256 sum: its image divided by the count as `numeric_avg` divides (`num_avg`'s rule).
    /// # Safety — as `num_sum256`.
    #[unwind]
    pub unsafe fn num_avg256(e: *mut ErrSlot, r: *mut Region, w: *const u64, count: i64, scale: i64) -> Ret {
        if count == 0 {
            return null_ret();
        }
        let w = core::ptr::read_unaligned(w.cast::<[u64; 4]>());
        if w[3] as i64 == SENT_HI {
            return special_ret(e, r, w[0] as i64);
        }
        match wide_image(w, scale).and_then(|img| adt_numeric::numeric_avg_div(img.num(), count)) {
            Ok(img) => region_copy(r, img.as_bytes()),
            Err(err) => fail(e, err),
        }
    }
}

/// The i256 `w` × 10^-`scale` as a numeric image at dscale `scale`.
fn wide_image(w: [u64; 4], scale: i64) -> types_error::PgResult<adt_numeric::NumericImage> {
    let text = wide_text(w, scale as usize);
    adt_numeric::numeric_in(&text, -1, None).map(|v| v.expect("a well-formed numeric literal"))
}

/// V4R-38: `w`'s magnitude (its two's complement negated when the top bit is set) and sign.
pub fn wide_abs(w: [u64; 4]) -> ([u64; 4], bool) {
    if (w[3] as i64) >= 0 {
        return (w, false);
    }
    let mut m = [0u64; 4];
    let mut carry = 1u128;
    for i in 0..4 {
        let v = (!w[i]) as u128 + carry;
        (m[i], carry) = (v as u64, v >> 64);
    }
    (m, true)
}

/// V4R-38: magnitude `m` divided in place by `d`, the remainder returned.
pub fn wide_divrem(m: &mut [u64; 4], d: u64) -> u64 {
    let mut rem = 0u128;
    for i in (0..4).rev() {
        let cur = rem << 64 | m[i] as u128;
        (m[i], rem) = ((cur / d as u128) as u64, cur % d as u128);
    }
    rem as u64
}

/// V4R-38: magnitude `m` × `k` + `add` in place; false when it passes 256 bits.
pub fn wide_mul_add(m: &mut [u64; 4], k: u64, add: u64) -> bool {
    let mut carry = add as u128;
    for i in 0..4 {
        let v = m[i] as u128 * k as u128 + carry;
        (m[i], carry) = (v as u64, v >> 64);
    }
    carry == 0
}

/// The i256 `w` as decimal text with `scale` digits after the point (`-12.345000`).
pub fn wide_text(w: [u64; 4], scale: usize) -> String {
    let (mut m, neg) = wide_abs(w);
    let mut digits = Vec::with_capacity(80); // alloc: Cold
    while m != [0; 4] || digits.len() <= scale {
        digits.push(b'0' + wide_divrem(&mut m, 10) as u8); // alloc: Cold
    }
    let mut s = String::with_capacity(digits.len() + 2); // alloc: Cold
    if neg {
        s.push('-');
    }
    for (i, d) in digits.iter().rev().enumerate() {
        if scale > 0 && i == digits.len() - scale {
            s.push('.');
        }
        s.push(*d as char);
    }
    s
}

fn i128_of(lo: i64, hi: i64) -> i128 {
    ((hi as i128) << 64) | (lo as u64 as i128)
}

/// PG's "interval out of range" (22008), the error `finite_interval_pl` raises: an `IntervalSum` fold's overflow at a row
/// or at a merge, raised with PG's text (lowerv4 class_interval_sum.rs).
#[cold]
pub fn interval_out_of_range() -> Box<crate::PgError> {
    Box::new(crate::PgError::error("interval out of range").with_sqlstate(types_error::ERRCODE_DATETIME_VALUE_OUT_OF_RANGE)) // alloc: Cold
}

/// `sum / avg(interval)`'s final over an `IntervalSum` slot's state, by the row executor's own finals (`interval_sum_final`
/// / `interval_avg_final` over PG's IntervalAggState: NULL for no input, PG's raise for both infinities, an infinity, the
/// sum, or the sum divided by N through `interval_div`): PG's 16-byte Interval image (time, day, month), or `None` (NULL).
pub fn interval_agg_final(avg: bool, time: i64, day: i32, month: i32, n: i64, pinf: i64, ninf: i64) -> Result<Option<[u8; 16]>, Box<crate::PgError>> {
    let state = adt_timestamp::interval::IntervalAggState { N: n, pInfcount: pinf, nInfcount: ninf, sumX: adt_datetime::Interval { time, day, month } };
    let r = if avg { adt_timestamp::interval::interval_avg_final(&state)? } else { adt_timestamp::interval::interval_sum_final(&state)? };
    Ok(r.map(|iv| {
        let mut img = [0u8; 16];
        img[..8].copy_from_slice(&iv.time.to_le_bytes());
        img[8..12].copy_from_slice(&iv.day.to_le_bytes());
        img[12..].copy_from_slice(&iv.month.to_le_bytes());
        img
    }))
}
