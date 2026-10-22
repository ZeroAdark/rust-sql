//! C parses get_str_from_var's text, an exact decimal m x 10^e, with strtod / strtof: the correctly rounded value.
//! With m and 10^|e| both exact in the format (f64: m <= 2^53, |e| <= 22; f32: m <= 2^24, |e| <= 10) one IEEE
//! multiply or divide is correctly rounded too (Clinger), as is int -> float of an integer in u64: the same bits.
//! Anything else keeps the text path.

use crate::io::{get_str_from_var, get_str_raw, str_bound};
use crate::{Num, DEC_DIGITS, NBASE, NUMERIC_NEG};

// 5^22 < 2^53 and 5^10 < 2^24: every entry is exact.
const F64_EXACT_POW10: [f64; 23] = [
    1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16,
    1e17, 1e18, 1e19, 1e20, 1e21, 1e22,
];

const F32_EXACT_POW10: [f32; 11] = [1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10];

const U64_POW10: [u64; 20] = [
    1,
    10,
    100,
    1_000,
    10_000,
    100_000,
    1_000_000,
    10_000_000,
    100_000_000,
    1_000_000_000,
    10_000_000_000,
    100_000_000_000,
    1_000_000_000_000,
    10_000_000_000_000,
    100_000_000_000_000,
    1_000_000_000_000_000,
    10_000_000_000_000_000,
    100_000_000_000_000_000,
    1_000_000_000_000_000_000,
    10_000_000_000_000_000_000,
];

const F64_MANT_LIMIT: u64 = 1 << 53;
const F32_MANT_LIMIT: u64 = 1 << 24;

/// get_str_from_var's decimal (negative, m, e); None past u64 or on a digit outside [0, NBASE).
#[inline(always)]
fn printed_decimal(num: Num<'_>) -> Option<(bool, u64, i32)> {
    let digits = num.digits();
    let mut m: u64 = 0;
    for &d in digits {
        if !(0..NBASE as i16).contains(&d) {
            return None;
        }
        m = m.checked_mul(NBASE as u64)?.checked_add(d as u64)?;
    }
    let mut e = DEC_DIGITS * (num.weight() + 1 - digits.len() as i32);
    // digits past dscale are not printed (cut, not rounded)
    let dscale = num.dscale();
    if -e > dscale {
        let cut = (-e - dscale) as usize;
        m = if cut < U64_POW10.len() {
            m / U64_POW10[cut]
        } else {
            0
        };
        e = -dscale;
    }
    Some((num.sign() == NUMERIC_NEG, m, e))
}

#[inline]
fn strip_fraction_zeros(m: &mut u64, e: &mut i32) {
    while *e < 0 && *m != 0 && *m % 10 == 0 {
        *m /= 10;
        *e += 1;
    }
}

#[inline]
fn decimal_to_f64(neg: bool, mut m: u64, mut e: i32) -> Option<f64> {
    let mag = if m == 0 {
        0.0
    } else if e >= 0 {
        match U64_POW10.get(e as usize).and_then(|p| m.checked_mul(*p)) {
            Some(n) => n as f64,
            None if m <= F64_MANT_LIMIT && e < F64_EXACT_POW10.len() as i32 => {
                m as f64 * F64_EXACT_POW10[e as usize]
            }
            None => return None,
        }
    } else {
        if m > F64_MANT_LIMIT || -e >= F64_EXACT_POW10.len() as i32 {
            strip_fraction_zeros(&mut m, &mut e);
            if m > F64_MANT_LIMIT || -e >= F64_EXACT_POW10.len() as i32 {
                return None;
            }
        }
        m as f64 / F64_EXACT_POW10[-e as usize]
    };
    Some(if neg { -mag } else { mag })
}

#[inline]
fn decimal_to_f32(neg: bool, mut m: u64, mut e: i32) -> Option<f32> {
    let mag = if m == 0 {
        0.0
    } else if e >= 0 {
        let n = U64_POW10.get(e as usize).and_then(|p| m.checked_mul(*p))?;
        n as f32
    } else {
        if m > F32_MANT_LIMIT || -e >= F32_EXACT_POW10.len() as i32 {
            strip_fraction_zeros(&mut m, &mut e);
            if m > F32_MANT_LIMIT || -e >= F32_EXACT_POW10.len() as i32 {
                return None;
            }
        }
        m as f32 / F32_EXACT_POW10[-e as usize]
    };
    Some(if neg { -mag } else { mag })
}

#[inline]
pub(crate) fn float8_exact(num: Num<'_>) -> Option<f64> {
    let (neg, m, e) = printed_decimal(num)?;
    decimal_to_f64(neg, m, e)
}

#[inline]
pub(crate) fn float4_exact(num: Num<'_>) -> Option<f32> {
    let (neg, m, e) = printed_decimal(num)?;
    decimal_to_f32(neg, m, e)
}

/// numeric_float8 of `fixed_scale_unpack_i128(m, scale)`, bit for bit, without the image; None outside the exact path.
#[inline]
pub fn fixed_scale_to_f64(m: i128, scale: i32) -> Option<f64> {
    decimal_to_f64(
        m < 0,
        u64::try_from(m.unsigned_abs()).ok()?,
        scale.checked_neg()?,
    )
}

/// numeric_float4 of `fixed_scale_unpack_i128(m, scale)`, as [`fixed_scale_to_f64`].
#[inline]
pub fn fixed_scale_to_f32(m: i128, scale: i32) -> Option<f32> {
    decimal_to_f32(
        m < 0,
        u64::try_from(m.unsigned_abs()).ok()?,
        scale.checked_neg()?,
    )
}

const TEXT_STACK_BYTES: usize = 128;

#[inline(never)]
pub(crate) fn with_numeric_text<R>(num: Num<'_>, parse: impl FnOnce(&str) -> R) -> R {
    let view = num.view();
    if str_bound(view) <= TEXT_STACK_BYTES {
        let mut buf = [0u8; TEXT_STACK_BYTES];
        // SAFETY: buf is writable for str_bound(view) bytes, the most get_str_raw writes.
        let len = unsafe { get_str_raw(view, buf.as_mut_ptr()) };
        parse(core::str::from_utf8(&buf[..len]).expect("numeric text is ASCII"))
    } else {
        let mut buf = Vec::new();
        get_str_from_var(view, &mut buf);
        parse(core::str::from_utf8(&buf).expect("numeric text is ASCII"))
    }
}
