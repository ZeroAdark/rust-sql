//! Numeric's catalog names: fmgroids.h's names for the numeric operators' procedures whose scale and digit arithmetic
//! the numeric fold implements (PostgreSQL 18.6 pg_proc.dat) and numeric's typmod cast, with the tests the fold's trees
//! ask of a procedure or a type by name.
pub const F_NUMERIC_ADD: u32 = 1724;
pub const F_NUMERIC_SUB: u32 = 1725;
pub const F_NUMERIC_MUL: u32 = 1726;
pub const F_NUMERIC_DIV: u32 = 1727;
pub const F_NUMERIC_UMINUS: u32 = 1771;
pub const F_NUMERIC_NUMERIC_INT4: u32 = 1703;
pub const F_NUMERIC_ABS: u32 = 1704;
pub const F_ABS_NUMERIC: u32 = 1705;
pub const F_ROUND_NUMERIC_INT4: u32 = 1707;
pub const F_TRUNC_NUMERIC_INT4: u32 = 1709;
/// numeric's casts to float4 / float8 (`numeric_float4`, `numeric_float8`).
pub const F_FLOAT4_NUMERIC: u32 = 1745;
pub const F_FLOAT8_NUMERIC: u32 = 1746;

/// `typid` is numeric itself (`NUMERICOID`; a domain over numeric is another type).
pub const fn numeric_type(typid: u32) -> bool {
    typid == super::NUMERICOID
}

/// PS4-13: round / trunc (numeric, int4)'s second argument is the result's dscale, which decompose agg `form_at` reads as
/// the form's scale: exec inputs/front.rs keys it by value, as a cast's typmod.
pub const fn numeric_scale_arg(funcid: u32) -> bool {
    matches!(funcid, F_ROUND_NUMERIC_INT4 | F_TRUNC_NUMERIC_INT4)
}

/// D4567: `funcid` is numeric's sum or difference (`numeric_add` / `numeric_sub`), whose result's dscale is the larger of
/// its two values' (add_var / sub_var): decompose agg `form_at`, lowerv4's num_tree and the agg premise read a mixed-scale
/// CASE beside a side of a scale at least its every arm's as that side's scale by it.
pub const fn sum_or_difference(funcid: u32) -> bool {
    matches!(funcid, F_NUMERIC_ADD | F_NUMERIC_SUB)
}

/// round (numeric, int4), which rounds half away from zero (a dropped digit may carry one up); trunc drops it.
pub const fn rounds(funcid: u32) -> bool {
    funcid == F_ROUND_NUMERIC_INT4
}

/// D3139: `funcid` is numeric's cast to a float, `Some(true)` for float4 (`numeric_float4`), `Some(false)` for float8.
pub const fn float_cast(funcid: u32) -> Option<bool> {
    match funcid {
        F_FLOAT4_NUMERIC => Some(true),
        F_FLOAT8_NUMERIC => Some(false),
        _ => None,
    }
}

/// D4567 (a): whether `funcid` is numeric's division (numeric_div, the `/` operator's procedure).
pub const fn is_div(funcid: u32) -> bool {
    funcid == F_NUMERIC_DIV
}

/// `funcid` is numeric's unary minus (`numeric_uminus`), which a numeric tree reads as `0 - x` (D830).
pub const fn negation(funcid: u32) -> bool {
    funcid == F_NUMERIC_UMINUS
}

/// D4567 (a): numeric.c's `select_div_scale` for `x / k` over every finite x of display scale `s` whose mantissa (x x
/// 10^s) is under 10^`digits`, k a positive constant of mantissa `km` at its display scale `ks`, as steps of |x|'s
/// mantissa: the quotient's display scale reads only x's first base-10000 digit and its weight, so it changes only where
/// |x| crosses 10000^w or (k's first digit + 1) x 10000^w, and it never rises as |x| grows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DivScales {
    /// x = 0's quotient scale (numeric.c reads its weight as 0 and its first digit as 0).
    pub zero: u32,
    /// `(from, scale)`, `from` ascending from 1: a mantissa of absolute value at least `from` (and under the next step's)
    /// divides at `scale`; the scales fall step by step.
    pub steps: [(u64, u32); DIV_STEPS],
    pub len: usize,
}

/// The most steps a `DivScales` holds: two per base-10000 weight over 18 digits, and the first.
pub const DIV_STEPS: usize = 16;

impl DivScales {
    pub fn steps(&self) -> &[(u64, u32)] {
        &self.steps[..self.len]
    }

    /// The largest quotient scale of any x.
    pub fn max(&self) -> u32 {
        self.steps().iter().map(|s| s.1).fold(self.zero, u32::max)
    }

    /// The smallest quotient scale of any x.
    pub fn min(&self) -> u32 {
        self.steps().iter().map(|s| s.1).fold(self.zero, u32::min)
    }

    /// The quotient scale of an x of mantissa `m` (its absolute value).
    pub fn scale_of(&self, m: u64) -> u32 {
        match m {
            0 => self.zero,
            _ => self.steps().iter().rev().find(|s| s.0 <= m).map_or(self.zero, |s| s.1),
        }
    }
}

/// The weight and first digit of a value's base-10000 form (numeric.c's NumericVar after strip_var), from its mantissa
/// `m` at scale `s`; zero's are (0, 0).
fn nbase_lead(m: u128, s: i32) -> (i32, u128) {
    if m == 0 {
        return (0, 0);
    }
    let w = (m.ilog10() as i32 - s).div_euclid(4);
    let at = s + 4 * w;
    (w, if at >= 0 { m / 10u128.pow(at as u32) } else { m * 10u128.pow(at.unsigned_abs()) })
}

/// numeric.c `select_div_scale` of an x of mantissa `m` (its absolute value) at scale `s` over k (`km` at `ks`).
pub fn div_scale(m: u128, s: u32, km: u128, ks: u32) -> u32 {
    // numeric.c's NUMERIC_MIN_SIG_DIGITS (16), NUMERIC_MIN_DISPLAY_SCALE (0) and NUMERIC_MAX_DISPLAY_SCALE (1000)
    let ((w1, f1), (w2, f2)) = (nbase_lead(m, s as i32), nbase_lead(km, ks as i32));
    let q = w1 - w2 - (f1 <= f2) as i32;
    (16 - q * 4).max(s as i32).max(ks as i32).clamp(0, 1000) as u32
}

/// D4567 (a): the quotient scales of `x / k` (`DivScales`): x of display scale `s` (at most 18) under 10^`digits` (at
/// most 18), k > 0 of mantissa `km` at display scale `ks` (at most 18). `None` past those bounds.
pub fn div_scales(s: u32, digits: u32, km: i64, ks: u32) -> Option<DivScales> {
    if km <= 0 || s > 18 || ks > 18 || !(1..=18).contains(&digits) {
        return None;
    }
    let (si, km) = (s as i32, km as u128);
    let top = 10u128.pow(digits);
    let (_, f2) = nbase_lead(km, ks as i32);
    // every |x| where the scale may change: 10^(4w + s) and (f2 + 1) x 10^(4w + s), rounded up to a mantissa
    let at = |p: i32, c: u128| if p >= 0 { c * 10u128.pow(p as u32) } else { c.div_ceil(10u128.pow(p.unsigned_abs())) };
    let mut out = DivScales { zero: div_scale(0, s, km, ks), steps: [(0, 0); DIV_STEPS], len: 0 };
    let mut from: u128 = 1;
    let mut w = (-si).div_euclid(4);
    while from < top {
        let scale = div_scale(from, s, km, ks);
        if out.len == 0 || out.steps[out.len - 1].1 != scale {
            if out.len == DIV_STEPS {
                return None;
            }
            out.steps[out.len] = (from as u64, scale);
            out.len += 1;
        }
        from = loop {
            match [at(4 * w + si, 1), at(4 * w + si, f2 + 1)].into_iter().find(|&c| c > from) {
                Some(c) => break c,
                None => w += 1,
            }
        };
    }
    Some(out)
}

/// D4567 (a): 1 / k for a positive constant k of mantissa `km` at display scale `ks`, when it is a finite decimal (km has
/// no prime factor but 2 and 5): `(c, e)` with 1 / k = c / 10^e exactly, e the fewest places, so x / k = x x c / 10^e for
/// every x. `None` for any other k, or a c past one word.
pub fn exact_reciprocal(km: i64, ks: u32) -> Option<(i64, u32)> {
    if km <= 0 || ks > 18 {
        return None;
    }
    let (mut m, mut twos, mut fives) = (km as u64, 0u32, 0u32);
    while m % 2 == 0 {
        m /= 2;
        twos += 1;
    }
    while m % 5 == 0 {
        m /= 5;
        fives += 1;
    }
    if m != 1 {
        return None;
    }
    // 1 / km = 2^(p - twos) x 5^(p - fives) / 10^p with p = max(twos, fives); then x 10^ks
    let p = twos.max(fives);
    let c = 2u128.checked_pow(p - twos)?.checked_mul(5u128.checked_pow(p - fives)?)?;
    let (mut c, mut e) = if ks >= p { (c.checked_mul(10u128.checked_pow(ks - p)?)?, 0) } else { (c, p - ks) };
    // the fewest places: c's trailing zeros stripped while e > 0
    while e > 0 && c % 10 == 0 {
        c /= 10;
        e -= 1;
    }
    Some((i64::try_from(c).ok()?, e))
}
