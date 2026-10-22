//! typesem::numeric: the numeric facts the lowering and its admission walk read (the numeric lane's home, PLAN-v5
//! constructs #6; overflow, dscale and NaN move here as their callers adopt it). Today: a numeric image's mantissa at its
//! own display scale with its exact digit count (`mantissa`), so a plan constant past one i64 mantissa word is told apart
//! by its width (D2356) instead of reading as no value at all.

/// A finite numeric image read at its display scale: the mantissa is value × 10^dscale, an integer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mantissa {
    /// The image's display scale (numeric.c's dscale).
    pub dscale: i32,
    /// The mantissa's decimal digits (1 for zero).
    pub digits: u32,
    /// The mantissa, when it fits an i128.
    pub value: Option<i128>,
}

impl Mantissa {
    /// The mantissa as one i64 mantissa word: it fits and is clear of the four sentinels gravity_fn's words reserve at
    /// the ends of the i64 range (agg_final's `NO_MANTISSA`, `MANT_NINF`, `MANT_PINF`, `MANT_NAN`), as `num_mantissa`
    /// reads it.
    pub fn word(&self) -> Option<i64> {
        let v = self.value?;
        (v.unsigned_abs() < crate::agg_final::MANT_PINF as u128).then_some(v as i64)
    }

    /// The value (the mantissa over 10^dscale) rounded to an integer, down (floor) or up (ceil): an integer `n` is over
    /// the value exactly when it is over its floor, and at least the value exactly when it is at least its ceiling (a
    /// count's floor under a HAVING's numeric bound, lowerv4 topn raw.rs `count_floor`). `None` when the mantissa is not
    /// held or its negation overflows.
    pub fn rounded(&self, down: bool) -> Option<i128> {
        let v = self.value?;
        let Some(p) = u32::try_from(self.dscale).ok().and_then(|d| 10i128.checked_pow(d)) else {
            // 10^dscale past an i128 while the mantissa is held: |value| < 1
            return Some(if down { -((v < 0) as i128) } else { (v > 0) as i128 });
        };
        if down { Some(v.div_euclid(p)) } else { Some(-(v.checked_neg()?.div_euclid(p))) }
    }
}

/// numeric.c's payload (a varlena's data: the short or long header word, the long form's weight, then base-10000 digits,
/// at any byte alignment) read at its dscale. `None` for NaN / ±Infinity or an image numeric.c never writes (an odd
/// length, a digit past 9999, a nonzero digit below the dscale).
pub fn mantissa(payload: &[u8]) -> Option<Mantissa> {
    read(payload, true)
}

/// `mantissa`; `fast` false sums every digit's term in an i128 (the reference its u64 path is tested against).
fn read(payload: &[u8], fast: bool) -> Option<Mantissa> {
    if payload.len() % 2 != 0 {
        return None;
    }
    let rd = |i: usize| payload.get(2 * i..2 * i + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let h = rd(0)?;
    // numeric.c's header word: 0xC000 special; 0x8000 short (sign 0x2000, dscale bits 7..12, weight bits 0..6 with its
    // sign at 0x40); else long (sign 0x4000, dscale the low 14 bits, the weight in the next word)
    let (neg, dscale, mut weight, first) = match h & 0xC000 {
        0xC000 => return None,
        0x8000 => (h & 0x2000 != 0, ((h & 0x1F80) >> 7) as i32, if h & 0x40 != 0 { (h | !0x3F) as i16 as i32 } else { (h & 0x3F) as i32 }, 1),
        s => (s == 0x4000, (h & 0x3FFF) as i32, rd(1)? as i16 as i32, 2),
    };
    // leading zero digits (numeric.c strips them; a hand-built image still reads) each lower the weight by one place
    let n = payload.len() / 2;
    let start = (first..n).find(|&i| rd(i) != Some(0)).unwrap_or(n);
    weight -= (start - first) as i32;
    let Some(lead) = rd(start).filter(|_| start < n) else { return Some(Mantissa { dscale, digits: 1, value: Some(0) }) };
    // the leading digit's place: its own decimal digits, 4 per base-10000 place above it, then the dscale
    let width = 4 * weight + dscale + (lead.checked_ilog10()? + 1) as i32;
    // D4723: a mantissa of at most 18 digits whose last base-10000 digit sits at most 3 places past the dscale (every image
    // numeric.c writes) reads by Horner's rule in one u64: the terms the i128 sum below adds, each partial value under the
    // whole (under 10^width), with no i128 power per digit (a keyed numeric extreme's order key reads one per row: a max
    // over a CTE's stored sums)
    let last = 4 * (weight - (n - 1 - start) as i32) + dscale;
    if fast && (1..=18).contains(&width) && last >= -3 {
        let mut acc = 0u64;
        for d in (start..n - 1).filter_map(rd) {
            if d > 9999 {
                return None;
            }
            acc = acc * 10_000 + d as u64;
        }
        let d = rd(n - 1)?;
        if d > 9999 {
            return None;
        }
        let v = if last >= 0 {
            (acc * 10_000 + d as u64) * 10u64.pow(last as u32)
        } else {
            let p = 10u16.pow(last.unsigned_abs());
            if d % p != 0 {
                return None;
            }
            acc * 10u64.pow((4 + last) as u32) + (d / p) as u64
        };
        return Some(Mantissa { dscale, digits: width as u32, value: Some(if neg { -(v as i128) } else { v as i128 }) });
    }
    let mut value = Some(0i128);
    for (i, d) in (start..n).filter_map(rd).enumerate() {
        if d > 9999 {
            return None;
        }
        let e = 4 * (weight - i as i32) + dscale;
        let term = if e >= 0 {
            10i128.checked_pow(e as u32).and_then(|p| p.checked_mul(d as i128))
        } else {
            let p = 10u32.checked_pow(e.unsigned_abs()).unwrap_or(u32::MAX);
            if d as u32 % p != 0 {
                return None;
            }
            Some((d as u32 / p) as i128)
        };
        value = value.zip(term).and_then(|(v, t)| v.checked_add(t));
    }
    let digits = u32::try_from(width).ok().filter(|&w| w >= 1)?;
    Some(Mantissa { dscale, digits, value: value.map(|v| if neg { -v } else { v }) })
}

/// A numeric payload's order word (numeric.c `numeric_abbrev_convert_var`, the 64-bit abbreviation, its sign turned so the
/// words order ascending as signed integers): the excess-44 base-10000 weight over the first four digits, 14 bits each,
/// negated for a negative value; -Infinity `i64::MIN`, +Infinity and NaN `i64::MAX`. Inexact: equal values have equal
/// words (the dscale is not read) and a lower word is a lower value, but values tied in the first four digits, past
/// weight 83 or under weight -44 share one, so a tie is the comparator's to decide. A payload numeric.c never writes
/// reads 0.
pub fn order_word(payload: &[u8]) -> i64 {
    let rd = |i: usize| payload.get(2 * i..2 * i + 2).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]));
    let h = rd(0);
    let (neg, weight, first) = match h & 0xC000 {
        0xC000 => return if h == 0xF000 { i64::MIN } else { i64::MAX },
        0x8000 => (h & 0x2000 != 0, if h & 0x40 != 0 { (h | !0x3F) as i16 as i32 } else { (h & 0x3F) as i32 }, 1),
        s => (s == 0x4000, rd(1) as i16 as i32, 2),
    };
    let n = (payload.len() / 2).saturating_sub(first);
    let m = if n == 0 || weight < -44 {
        0
    } else if weight > 83 {
        i64::MAX
    } else {
        (0..4).filter(|&i| i < n).fold(((weight + 44) as i64) << 56, |m, i| m | ((rd(first + i) & 0x3FFF) as i64) << (42 - 14 * i))
    };
    if neg { -m } else { m }
}

/// The order key's specials, set apart from every finite key in `key >> 4` (its value part): NaN above +Infinity above
/// every value, -Infinity below.
pub const KEY_NAN: i64 = i64::MAX;
pub const KEY_PINF: i64 = i64::MAX - 16;
pub const KEY_NINF: i64 = i64::MIN + 16;
/// No order key: a value past 16 significant digits or a dscale past 15.
pub const NO_KEY: i64 = i64::MIN;
/// 10^0 ..= 10^19: `order_key_of`'s digit count and coefficient scale, read by the lowered key of a packed code (lowerv4
/// agg numeric num_side `pack_key`).
pub static POW10: [u64; 20] = {
    let mut t = [1u64; 20];
    let mut i = 1;
    while i < 20 {
        t[i] = t[i - 1] * 10;
        i += 1;
    }
    t
};

/// NEW-numeric-1: an exact, order-preserving i64 key of the numeric image at `payload` with its display scale: the value
/// as 16 significant digits `c` and its decimal exponent `x` (the value is `c` x 10^(x - 16)), `(x + 15) * 10^16 + c`
/// for a positive value, negated for a negative one, 0 for zero, times 16 plus the dscale. Two values order as their
/// `key >> 4` (equal values of two dscales tie there, as numeric_cmp ties them), and `order_key_value` gives back the
/// value at its dscale; NaN and +-Infinity are `KEY_NAN` / `KEY_PINF` / `KEY_NINF`. `NO_KEY` past 16 significant digits
/// or a dscale past 15, and for an image numeric.c never writes.
pub fn order_key(payload: &[u8]) -> i64 {
    let Some(m) = mantissa(payload) else {
        return match payload.get(..2).map(|h| u16::from_le_bytes([h[0], h[1]])) {
            Some(0xC000) => KEY_NAN,
            Some(0xD000) => KEY_PINF,
            Some(0xF000) => KEY_NINF,
            _ => NO_KEY,
        };
    };
    let (Some(v), d) = (m.word(), m.dscale as i64) else { return NO_KEY };
    if !(0..=15).contains(&d) {
        return NO_KEY;
    }
    key_at(v, d, m.digits as i64)
}

/// N14: `order_key` of the value whose mantissa at display scale `s` is `m` (the value m x 10^-s, its dscale `s`): a
/// pgrcolumnar PACKED_NUMERIC code at its chunk's elected scale (the uniform-dscale rule: every value of the chunk has
/// dscale `s`), keyed without an image. The same key `order_key` reads off the image numeric.c writes for it; `NO_KEY`
/// past 16 significant digits or a dscale past 15.
pub fn order_key_of(m: i64, s: i64) -> i64 {
    if !(0..=15).contains(&s) || m.unsigned_abs() >= crate::agg_final::MANT_PINF as u64 {
        return NO_KEY;
    }
    let a = m.unsigned_abs();
    key_at(m, s, if a == 0 { 1 } else { a.ilog10() as i64 + 1 })
}

/// The order key of mantissa `v` at dscale `d` (0..=15) with `n` decimal digits (`order_key`'s formula).
fn key_at(v: i64, d: i64, n: i64) -> i64 {
    if v == 0 {
        return d;
    }
    let a = v.unsigned_abs();
    let c = if n > 16 {
        let p = 10u64.pow((n - 16) as u32);
        if a % p != 0 {
            return NO_KEY;
        }
        a / p
    } else {
        a * 10u64.pow((16 - n) as u32)
    };
    // n <= 19 and d <= 15: (n - d + 15) is 1..=34, so the key stays under 5.6e18
    let k = ((n - d + 15) * 10i64.pow(16) + c as i64) * 16;
    if v > 0 { k + d } else { d - k }
}

/// `order_key`'s value back: `(mantissa, dscale)`, the mantissa at the dscale, an agg_final mantissa word (its sentinels
/// for NaN and +-Infinity), as gravity_fn's `num_word` renders it.
pub fn order_key_value(k: i64) -> (i64, i64) {
    use crate::agg_final::{MANT_NAN, MANT_NINF, MANT_PINF};
    match k {
        KEY_NAN => return (MANT_NAN, 0),
        KEY_PINF => return (MANT_PINF, 0),
        KEY_NINF => return (MANT_NINF, 0),
        _ => {}
    }
    let d = k & 15;
    let p = (k - d) / 16;
    if p == 0 {
        return (0, d);
    }
    let (e, c) = ((p.unsigned_abs() / 10u64.pow(16)) as i64, p.unsigned_abs() % 10u64.pow(16));
    let n = e - 15 + d;
    let a = if n > 16 { c * 10u64.pow((n - 16) as u32) } else { c / 10u64.pow((16 - n) as u32) };
    (if p < 0 { -(a as i64) } else { a as i64 }, d)
}

/// A key whose declared type holds at most one character, a `char(1)` or `varchar(1)` (typmod `VARHDRSZ + 1`, varchar.c's
/// length check): num.pair places its block by its first word as it is (P2), a one-byte value's byte.
pub fn one_char_key(typid: u32, typmod: i32) -> bool {
    use types_core::catalog::{BPCHAROID, VARCHAROID};
    (typid == BPCHAROID || typid == VARCHAROID) && typmod == adt_varchar::VARHDRSZ as i32 + 1
}

/// The bytes a num.pair key's value holds at most: one word of its key.tagged inline image `[b0..7, b8..14 | len << 56]`.
pub const PAIR_BYTES: u32 = 8;

/// The num.pair road's plan-time key premise (D1896): a text key whose values are at most `PAIR_BYTES` bytes, so its
/// key.tagged image is one word of bytes and the pair can place its block from them. By the declared type: a `char(n)` or
/// `varchar(n)` of at most `PAIR_BYTES` characters (typmod `VARHDRSZ + n`, varchar.c's length check). Else by the
/// statistics: a text, `varchar` or `char` whose values average (`bytes`, a value's bytes with no header) 1 ..
/// `PAIR_BYTES` bytes. Any other key answers false: a row of it would test its bytes for a pair it never makes. The run
/// time still tests each row (a multibyte character, a value past the average).
pub fn pair_text_key(typid: u32, typmod: i32, bytes: u32) -> bool {
    use types_core::catalog::{BPCHAROID, TEXTOID, VARCHAROID};
    let declared = typmod - adt_varchar::VARHDRSZ as i32;
    match typid {
        BPCHAROID | VARCHAROID if (1..=PAIR_BYTES as i32).contains(&declared) => true,
        BPCHAROID | VARCHAROID | TEXTOID => (1..=PAIR_BYTES).contains(&bytes),
        _ => false,
    }
}

/// numeric's typmod as `(precision, scale)` (numeric.c's NUMERIC_TYPMOD_PRECISION / _SCALE: the scale is 11 bits,
/// signed); `None` for no typmod (-1).
pub fn typmod_parts(typmod: i32) -> Option<(i32, i32)> {
    adt_numeric::is_valid_numeric_typmod(typmod).then(|| (adt_numeric::numeric_typmod_precision(typmod), adt_numeric::numeric_typmod_scale(typmod)))
}

/// PS4-21: the error `x::numeric(p, s)` raises for an `x` that rounds past p digits at scale s: apply_typmod's own
/// (numeric_in applies it), read off 10^(p - s), which rounds to itself and never fits.
#[cold]
pub fn typmod_overflow(typmod: i32) -> Box<crate::PgError> {
    let (p, s) = typmod_parts(typmod).unwrap_or((0, 0));
    match adt_numeric::numeric_in(&format!("1e{}", p - s), typmod, None) { // alloc: Cold
        Err(e) => e,
        Ok(_) => Box::new(crate::PgError::error("gravity: a typmod cast's overflow that apply_typmod passes".to_string())), // alloc: Cold
    }
}

#[cfg(test)]
mod tests {
    /// A numeric rounded for an integer compare: floor under `>`, ceiling under `>=`, exact at integers.
    #[test]
    fn a_value_rounds_down_and_up_to_integers() {
        let m = |value: i128, dscale: i32| super::Mantissa { dscale, digits: 1, value: Some(value) };
        assert_eq!((m(15, 1).rounded(true), m(15, 1).rounded(false)), (Some(1), Some(2))); // 1.5
        assert_eq!((m(20, 1).rounded(true), m(20, 1).rounded(false)), (Some(2), Some(2))); // 2.0
        assert_eq!((m(-5, 1).rounded(true), m(-5, 1).rounded(false)), (Some(-1), Some(0))); // -0.5
        assert_eq!((m(3, 60).rounded(true), m(3, 60).rounded(false)), (Some(0), Some(1))); // 3e-60
        assert_eq!((m(-3, 60).rounded(true), m(-3, 60).rounded(false)), (Some(-1), Some(0)));
        assert_eq!(m(i128::MIN, 0).rounded(false), None);
        assert_eq!(super::Mantissa { dscale: 0, digits: 40, value: None }.rounded(true), None);
    }

    use super::*;

    /// PS4-21: a typmod cast's overflow is apply_typmod's error, its detail naming the field (10^0 shown as 1).
    #[test]
    fn a_typmod_overflow_is_apply_typmods_error() {
        let tm = |p: i32, s: i32| ((p << 16) | (s & 0x7ff)) + 4;
        for (p, s, detail) in [(12, 2, "precision 12, scale 2 must round to an absolute value less than 10^10."), (4, 4, "precision 4, scale 4 must round to an absolute value less than 1."), (2, 4, "precision 2, scale 4 must round to an absolute value less than 10^-2.")] {
            let e = typmod_overflow(tm(p, s));
            assert_eq!((e.message(), e.sqlstate()), ("numeric field overflow", types_error::ERRCODE_NUMERIC_VALUE_OUT_OF_RANGE));
            assert_eq!(e.detail(), Some(format!("A field with {detail}").as_str())); // alloc: Cold
        }
    }

    /// D1896: `char(n)` / `varchar(n)` keys of at most a word's characters may pair, as may a text-like key whose values
    /// average at most a word; texts of wider or unknown widths and other types never do.
    #[test]
    fn only_types_of_a_word_of_bytes_pair() {
        use types_core::catalog::{BPCHAROID, INT4OID, TEXTOID, VARCHAROID};
        let n = |c: i32| adt_varchar::VARHDRSZ as i32 + c;
        assert!([1, 2, 8].iter().all(|&c| pair_text_key(BPCHAROID, n(c), 0) && pair_text_key(VARCHAROID, n(c), 0)), "char / varchar(1 .. 8)");
        assert!(!pair_text_key(BPCHAROID, n(9), 9) && !pair_text_key(VARCHAROID, n(10), 0), "char(9) (padded), varchar(10) without statistics");
        assert!(pair_text_key(VARCHAROID, n(40), 5), "varchar(40) of short values: the statistics");
        assert!(pair_text_key(TEXTOID, -1, 6) && pair_text_key(VARCHAROID, -1, 8) && pair_text_key(BPCHAROID, -1, 1), "values of 6, 8, 1 bytes");
        assert!(!pair_text_key(TEXTOID, -1, 9) && !pair_text_key(TEXTOID, -1, 0), "past a word; no statistics (or empty values)");
        assert!(!pair_text_key(INT4OID, n(1), 4) && !pair_text_key(INT4OID, -1, 4));
        assert!(one_char_key(BPCHAROID, n(1)) && one_char_key(VARCHAROID, n(1)) && !one_char_key(BPCHAROID, n(2)) && !one_char_key(TEXTOID, -1));
    }

    fn payload(text: &str) -> Vec<u8> {
        adt_numeric::numeric_in(text, -1, None).unwrap().unwrap().as_bytes()[4..].to_vec()
    }

    /// D2356: a constant's mantissa, its digits and its dscale, past one i64 word and past an i128 too (the width alone).
    #[test]
    fn a_constant_reads_at_its_dscale_with_its_width() {
        let m = |t: &str| mantissa(&payload(t)).expect(t);
        assert_eq!(m("13619314596827451335.1"), Mantissa { dscale: 1, digits: 21, value: Some(136_193_145_968_274_513_351) });
        assert_eq!(m("1e35"), Mantissa { dscale: 0, digits: 36, value: Some(10i128.pow(35)) });
        assert_eq!(m("-0.050"), Mantissa { dscale: 3, digits: 2, value: Some(-50) });
        assert_eq!(m("0.5"), Mantissa { dscale: 1, digits: 1, value: Some(5) });
        assert_eq!(m("0"), Mantissa { dscale: 0, digits: 1, value: Some(0) });
        assert_eq!(m("0.000"), Mantissa { dscale: 3, digits: 1, value: Some(0) });
        assert_eq!(m("10000"), Mantissa { dscale: 0, digits: 5, value: Some(10_000) });
        assert_eq!(m("1e40"), Mantissa { dscale: 0, digits: 41, value: None });
        assert_eq!(m("-98765432109876543210987654321098765432.5"), Mantissa { dscale: 1, digits: 39, value: None });
        for s in ["NaN", "Infinity", "-Infinity"] {
            assert_eq!(mantissa(&payload(s)), None, "{s}");
        }
    }

    /// One word exactly where gravity_fn's `num_mantissa` reads a mantissa (num_const.rs and props/code.rs relied on it):
    /// the same value for every literal it takes, and no word where it answers NO_MANTISSA.
    #[test]
    fn one_word_is_where_num_mantissa_reads_one() {
        use crate::agg_final::{num_mantissa, NO_MANTISSA};
        let lits = ["1.5", "-1.50", "922337203685477580.5", "9223372036854775805", "-9223372036854775805", "9223372036854775806", "-9223372036854775806",
            "9223372036854775807", "123456789012345678901", "0.000000000000000001", "-0.0000001234", "99999999.9999", "1e18", "1e19", "42"];
        for t in lits {
            let img = adt_numeric::numeric_in(t, -1, None).unwrap().unwrap();
            let w = mantissa(&img.as_bytes()[4..]).expect(t);
            // SAFETY: a live numeric varlena with a 4-byte header; the error slot is unused by the decoder.
            let host = unsafe { num_mantissa(core::ptr::null_mut(), img.as_bytes().as_ptr(), w.dscale as i64) }.val;
            assert_eq!(w.word(), (host != NO_MANTISSA).then_some(host), "{t}");
            if let Some(v) = w.word() {
                assert_eq!(w.digits, v.unsigned_abs().checked_ilog10().map_or(1, |l| l + 1), "{t}");
            }
        }
    }

    /// The order word orders as cmp_numerics wherever two words differ, and equal values (any dscale) share one: specials,
    /// zeros, short and long forms, values tied in their first four digits, past weight 83 and under weight -44.
    #[test]
    fn order_words_order_as_cmp_numerics() {
        let lits = ["NaN", "Infinity", "-Infinity", "0", "0.000", "-0.0", "1", "1.0", "1.00000", "-1", "0.5", "-0.5", "9999", "10000",
            "10000.0001", "12345678.9012345678", "12345678.9012345679", "-12345678.9012345678", "-12345678.9012345679", "1e-170", "-1e-170",
            "1e-176", "1e-180", "-1e-180", "1e330", "1e331", "-1e330", "1e300", "999999999999999999999.5", "-999999999999999999999.5",
            "0.0001", "0.00010001", "-0.0001", "123.45", "123.450", "-123.45", "77", "-77", "1e20", "1.0000000000000001e20"];
        let imgs: Vec<_> = lits.iter().map(|t| adt_numeric::numeric_in(t, -1, None).unwrap().unwrap()).collect(); // alloc: Program
        let p = |i: usize| &imgs[i].as_bytes()[4..];
        for i in 0..lits.len() {
            for j in 0..lits.len() {
                let c = adt_numeric::cmp_numerics(adt_numeric::Num::from_payload(p(i)), adt_numeric::Num::from_payload(p(j)));
                let (wi, wj) = (order_word(p(i)), order_word(p(j)));
                if c == 0 {
                    assert_eq!(wi, wj, "{} = {}", lits[i], lits[j]);
                }
                if wi < wj {
                    assert!(c < 0, "{} word below {} but not the value", lits[i], lits[j]);
                }
            }
        }
        let mut odd = vec![0u8; p(15).len() + 1]; // alloc: Program
        odd[1..].copy_from_slice(p(15));
        assert_eq!(order_word(&odd[1..]), order_word(p(15)), "an odd-aligned payload reads the same");
        assert!(order_word(p(13)) < order_word(p(15)) && order_word(p(16)) == order_word(p(15)), "apart in the first digit, then tied past four");
    }

    /// A 1-byte-header image's payload sits at an odd address: it reads the same there.
    #[test]
    fn an_odd_aligned_payload_reads_the_same() {
        let p = payload("-12345678901234567890123.25");
        let mut buf = vec![0u8; p.len() + 1]; // alloc: Program
        buf[1..].copy_from_slice(&p);
        assert_eq!(mantissa(&buf[1..]), mantissa(&p));
        assert_eq!(mantissa(&p[..p.len() - 1]), None, "an odd length is no image");
    }

    /// NEW-numeric-1: the order key gives each value back at its dscale and orders as numeric_cmp: equal values of two
    /// dscales tie in `key >> 4`, NaN above +Infinity above every value, -Infinity below; past 16 significant digits or
    /// dscale 15 there is no key.
    #[test]
    fn the_order_key_is_exact_and_orders_as_numeric() {
        let vals = ["0", "0.00", "1", "1.0", "1.00", "-1", "-1.000", "0.5", "-0.5", "12.34", "-12.34", "9999999999999999",
            "-9999999999999999", "1234567890123456000", "-1234567890123456000", "0.000000000000001", "-0.000000000000001",
            "123456789.012345", "100", "1e15", "99.99", "-0.01", "3.141592653589793", "7000000000000000000"];
        let keyed: Vec<(i64, i128)> = vals.iter().map(|t| { // alloc: Cold
            let p = payload(t);
            let (k, m) = (order_key(&p), mantissa(&p).unwrap());
            assert_ne!(k, NO_KEY, "{t}");
            assert_eq!(order_key_value(k), (m.word().unwrap(), m.dscale as i64), "{t}");
            (k, m.value.unwrap() * 10i128.pow((15 - m.dscale) as u32))
        }).collect();
        for &(k1, v1) in &keyed {
            for &(k2, v2) in &keyed {
                assert_eq!((k1 >> 4).cmp(&(k2 >> 4)), v1.cmp(&v2), "{k1} {k2}");
            }
            assert!(KEY_NINF >> 4 < k1 >> 4 && k1 >> 4 < KEY_PINF >> 4 && KEY_PINF >> 4 < KEY_NAN >> 4);
        }
        for (t, k) in [("NaN", KEY_NAN), ("Infinity", KEY_PINF), ("-Infinity", KEY_NINF)] {
            assert_eq!(order_key(&payload(t)), k, "{t}");
        }
        for t in ["0.0000000000000001", "12345678901234567", "-1.234567890123456789", "10000000000000000000"] {
            assert_eq!(order_key(&payload(t)), NO_KEY, "{t}");
        }
    }

    /// N14: a packed code's key (mantissa at the chunk's scale) is the key of the image numeric.c writes for it, past 16
    /// digits and past dscale 15 too (`NO_KEY` where the image has none).
    #[test]
    fn a_packed_codes_key_is_its_images_key() {
        let text = |m: i64, s: u32| {
            let a = m.unsigned_abs().to_string(); // alloc: Cold
            let a = if a.len() <= s as usize { format!("{}{a}", "0".repeat(s as usize + 1 - a.len())) } else { a }; // alloc: Cold
            let (i, f) = a.split_at(a.len() - s as usize);
            format!("{}{i}{}{f}", if m < 0 { "-" } else { "" }, if s > 0 { "." } else { "" }) // alloc: Cold
        };
        let ms = [0i64, 1, -1, 7, -7, 10, 100, 12345, -12345, 99_999, 1_000_000, 9_999_999_999_999_999, -9_999_999_999_999_999,
            10_000_000_000_000_000, 12_345_678_901_234_567, 12_345_678_901_234_560, -36_028_797_018_963_967, 36_028_797_018_963_960];
        for s in 0..=17u32 {
            for &m in &ms {
                let t = text(m, s);
                let p = payload(&t);
                assert_eq!(mantissa(&p).map(|x| (x.value, x.dscale)), Some((Some(m as i128), s as i32)), "{t}");
                assert_eq!(order_key_of(m, s as i64), order_key(&p), "{m} at {s} ({t})");
            }
        }
    }

    /// D4723: `mantissa`'s u64 path reads what the i128 sum reads, on every image form: short and long headers, both signs,
    /// dscales 0..=20, weights -4..=5, one to six digits (zeros at either end, 9999, a digit past 9999, a nonzero digit
    /// below the dscale), across the 18-digit edge.
    #[test]
    fn the_u64_mantissa_reads_as_the_i128_sum() {
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let pick = [0u16, 1, 7, 10, 100, 1000, 5000, 9990, 9999, 10_000];
        let (mut fast, mut seen) = (0, 0);
        for long in [false, true] {
            for neg in [false, true] {
                for dscale in 0..=20u16 {
                    for weight in -4..=5i16 {
                        for nd in 1..=6usize {
                            for _ in 0..24 {
                                let mut p: Vec<u8> = Vec::with_capacity(16); // alloc: Cold
                                let h = if long {
                                    (if neg { 0x4000 } else { 0 }) | dscale
                                } else {
                                    0x8000 | (if neg { 0x2000 } else { 0 }) | (dscale << 7 & 0x1F80) | (weight as u16 & 0x7F)
                                };
                                p.extend_from_slice(&h.to_le_bytes());
                                if long {
                                    p.extend_from_slice(&weight.to_le_bytes());
                                }
                                for _ in 0..nd {
                                    let r = next();
                                    let d = if r & 3 == 0 { (r >> 8) as u16 % 10_000 } else { pick[(r >> 8) as usize % pick.len()] };
                                    p.extend_from_slice(&d.to_le_bytes());
                                }
                                let (a, b) = (read(&p, true), read(&p, false));
                                assert_eq!(a, b, "{p:?}");
                                seen += 1;
                                fast += a.is_some_and(|m| m.digits <= 18) as u32;
                            }
                        }
                    }
                }
            }
        }
        assert!(fast > seen / 8, "the u64 path ran: {fast} of {seen}");
        for t in ["999999999999999999", "-999999999999999999", "1000000000000000000", "99999999.9999999999", "0.000000000000000001", "123.4500"] {
            assert_eq!(read(&payload(t), true), read(&payload(t), false), "{t}");
        }
    }
}
