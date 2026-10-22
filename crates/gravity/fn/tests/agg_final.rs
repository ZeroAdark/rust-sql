//! gravity_fn's aggregate finals (agg_final.rs) at their edges.
use gravity_fn::agg_final::{num_avg, num_sum, SENT_HI};
use gravity_fn::{ErrSlot, Region, Ret, ERROR};

/// D299: a numeric sum past the packed sum's range (the OVERFLOW code, 4) raises 22003 from sum and avg; it printed
/// `-Infinity` before (every code past NaN / Infinity rendered as -Infinity).
#[test]
fn d299_an_overflowed_sum_raises() {
    let finals: [fn(*mut ErrSlot, *mut Region) -> Ret; 2] = [|e, r| num_sum(e, r, 4, SENT_HI, 2), |e, r| num_avg(e, r, 4, SENT_HI, 3, 2)];
    for ret in finals {
        let (mut e, mut bytes) = (ErrSlot::NONE, [0u8; 64]);
        let mut r = Region::over(&mut bytes);
        assert_eq!(ret(&mut e, &mut r).status, ERROR);
        let err = e.take().expect("the error");
        assert_eq!(err.sqlstate(), types_error::ERRCODE_NUMERIC_VALUE_OUT_OF_RANGE);
    }
}

/// The image a final wrote (its `Ret` points at a 4-byte-header varlena in the region).
fn image(ret: Ret) -> Vec<u8> {
    assert_eq!(ret.status, gravity_fn::OK);
    let p = ret.val as usize as *const u8;
    // SAFETY: an OK numeric final returns a live 4-byte-header varlena.
    unsafe { core::slice::from_raw_parts(p, (p.cast::<u32>().read_unaligned() >> 2) as usize).to_vec() }
}

fn pg(text: &str) -> Vec<u8> {
    adt_numeric::numeric_in(text, -1, None).unwrap().unwrap().as_bytes().to_vec()
}

fn wide(v: i128, top: i64) -> [u64; 4] {
    [v as u64, (v >> 64) as u64, top as u64, (top >> 63) as u64]
}

/// V4R-38: an i256 sum renders as PostgreSQL's numeric at its scale (every digit, the dscale kept, a sign, zero), past
/// i128 too; avg divides as numeric_avg (20 digits of 1.000000 / 3; a 33-digit quotient rounded half away at dscale 6); the OVERFLOW code raises 22003, a NaN sum is NaN.
#[test]
fn v4r38_i256_finals_are_pg_numerics() {
    use gravity_fn::agg_final::{num_avg256, num_sum256, SP_NAN, SP_OVERFLOW};
    let sum = |w: [u64; 4], scale: i64| {
        let (mut e, mut bytes) = (ErrSlot::NONE, [0u8; 128]);
        let mut r = Region::over(&mut bytes);
        let ret = unsafe { num_sum256(&mut e, &mut r, w.as_ptr(), scale) };
        (ret.status == gravity_fn::OK).then(|| image(ret)).ok_or_else(|| e.take().expect("the error"))
    };
    let avg = |w: [u64; 4], n: i64, scale: i64| {
        let (mut e, mut bytes) = (ErrSlot::NONE, [0u8; 128]);
        let mut r = Region::over(&mut bytes);
        let ret = unsafe { num_avg256(&mut e, &mut r, w.as_ptr(), n, scale) };
        (ret.status == gravity_fn::OK).then(|| image(ret)).ok_or_else(|| e.take().expect("the error"))
    };
    assert_eq!(sum(wide(12_345_678_900, 0), 6).unwrap(), pg("12345.678900"));
    assert_eq!(sum(wide(-7_250_000, -1), 6).unwrap(), pg("-7.250000"));
    assert_eq!(sum(wide(0, 0), 6).unwrap(), pg("0.000000"));
    assert_eq!(sum(wide(5, 0), 0).unwrap(), pg("5"));
    // 2^128 + 1 at scale 6: past i128's words
    assert_eq!(sum([1, 0, 1, 0], 6).unwrap(), pg("340282366920938463463374607431768.211457"));
    assert_eq!(sum([u64::MAX, u64::MAX, u64::MAX - 1, u64::MAX], 2).unwrap(), pg("-3402823669209384634633746074317682114.57"));
    assert_eq!(avg(wide(1_000_000, 0), 3, 6).unwrap(), pg("0.33333333333333333333"));
    assert_eq!(avg([1, 0, 1, 0], 2, 6).unwrap(), pg("170141183460469231731687303715884.105729"));
    assert_eq!(sum([SP_NAN as u64, 0, 0, SENT_HI as u64], 6).unwrap(), pg("NaN"));
    for err in [sum([SP_OVERFLOW as u64, 0, 0, SENT_HI as u64], 6).unwrap_err(), avg([SP_OVERFLOW as u64, 0, 0, SENT_HI as u64], 4, 6).unwrap_err()] {
        assert_eq!(err.sqlstate(), types_error::ERRCODE_NUMERIC_VALUE_OUT_OF_RANGE);
    }
}

/// D4567: a sum of quotients exact at its register's scale prints at the largest select_div_scale display scale its rows
/// had (617.28 at 3 places shown at 16, a zero at 20), its avg divides that image as numeric_avg; a NaN sum is NaN, an
/// empty avg NULL. A quotient's image reads back at the register's scale while every digit past it is zero.
#[test]
fn d4567_padded_finals_are_pg_numerics() {
    use gravity_fn::agg_final::{num_avg_pad, num_mantissa_within, num_sum_pad, MANT_NAN, NO_MANTISSA, SP_NAN};
    let sum = |m: i128, scale: i64, d: i64| {
        let (mut e, mut bytes) = (ErrSlot::NONE, [0u8; 128]);
        let mut r = Region::over(&mut bytes);
        image(num_sum_pad(&mut e, &mut r, m as i64, (m >> 64) as i64, scale, d))
    };
    let avg = |m: i128, n: i64, scale: i64, d: i64| {
        let (mut e, mut bytes) = (ErrSlot::NONE, [0u8; 128]);
        let mut r = Region::over(&mut bytes);
        let ret = num_avg_pad(&mut e, &mut r, m as i64, (m >> 64) as i64, n, scale, d);
        (ret.status == gravity_fn::OK && ret.val != 0).then(|| image(ret))
    };
    assert_eq!(sum(617_280, 3, 16), pg("617.2800000000000000"));
    assert_eq!(sum(-617_285, 3, 16), pg("-617.2850000000000000"));
    assert_eq!(sum(0, 3, 20), pg("0.00000000000000000000"));
    assert_eq!(sum(5, 3, 24), pg("0.005000000000000000000000"));
    assert_eq!(sum(i64::MAX as i128 * 1000, 12, 16), pg("9223372036.8547758070000000"));
    assert_eq!(sum(SP_NAN as i128 | ((SENT_HI as i128) << 64), 3, 16), pg("NaN"));
    assert_eq!(avg(1_234_560, 3, 3, 16), Some(pg("411.5200000000000000")));
    assert_eq!(avg(1_000, 3, 3, 20), Some(pg("0.33333333333333333333")));
    assert_eq!(avg(0, 0, 3, 16), None);
    let within = |t: &str, scale: i64| unsafe { num_mantissa_within(pg(t).as_ptr(), scale) };
    assert_eq!(within("617.2800000000000000", 3), 617_280);
    assert_eq!(within("-0.0050000000000000000000", 3), -5);
    assert_eq!(within("0.00000000000000000000", 3), 0);
    assert_eq!(within("617.2810000000000000", 2), NO_MANTISSA);
    assert_eq!(within("617.28", 2), 61_728);
    assert_eq!(within("617.28", 3), NO_MANTISSA);
    assert_eq!(within("NaN", 3), MANT_NAN);
}
