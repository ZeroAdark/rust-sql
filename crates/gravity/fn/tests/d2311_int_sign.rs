//! D2311 (ROOTFIX-D2311): a shim's int2 / int4 result word is sign-extended, the convention the emitted code reads an
//! int2 / int4 word in (prim.rs open-codes int2 / int4 arithmetic and comparisons over the whole word). The hash bodies
//! return a `u32` (PG's `hash_any` / `UInt32GetDatum`), which the shims used to hand back zero-extended: a native
//! `hashtext(t) < 0` then saw no negative value, and `sum` / `max` / `% 2` / a group key over it read 2^32 + h. Every
//! int2 / int4 shim is also pinned by the differential (tests/differential.rs `int_word`); this test names the hash family
//! and requires each to reach a negative value, so a zero-extended word cannot pass by luck.

use gravity_fn::{shim_of, Arg, ErrSlot, Region, View, OK};

/// The word shim `oid` returns for `args` under collation `coll`.
fn word(oid: u32, coll: u32, args: &[Arg]) -> i64 {
    let s = shim_of(oid).unwrap_or_else(|| panic!("no shim {oid}"));
    let mut e = ErrSlot::NONE;
    let mut buf = vec![0u8; 1 << 12];
    let mut r = Region::over(&mut buf);
    let ret = (s.call)(&mut e, &mut r, coll, &[], args);
    assert_eq!(ret.status, OK, "{} over {} args", s.name, args.len());
    ret.val
}

/// `words` are every value a shim returned: each sign-extended from 32 bits, and at least one negative.
fn check(name: &str, words: &[i64]) {
    for &w in words {
        assert_eq!(w, w as i32 as i64, "{name}: an int4 result's word {w:#x} is not sign-extended (D2311)");
    }
    assert!(words.iter().any(|&w| w < 0), "{name}: the inputs reach no negative hash ({words:?})");
}

#[test]
fn hash_shims_sign_extend_their_int4_result() {
    let ints: Vec<i64> = (-40..40).collect();
    let by_value = |oid: u32, f: &dyn Fn(i64) -> i64| ints.iter().map(|&v| word(oid, 0, &[Arg::V(f(v))])).collect::<Vec<_>>();
    // hashint2 449, hashint4 450, hashint8 949, hashoid 453, hashchar 454, hashbool 6417, hashfloat4 451, hashfloat8 452
    check("hashint2", &by_value(449, &|v| v as i16 as i64));
    check("hashint4", &by_value(450, &|v| v * 1000));
    check("hashint8", &by_value(949, &|v| v * 3_000_000_000));
    check("hashoid", &by_value(453, &|v| (v as u32) as i64));
    check("hashchar", &by_value(454, &|v| v as i8 as i64));
    check("hashfloat4", &by_value(451, &|v| (v as f32 * 0.5).to_bits() as i64));
    check("hashfloat8", &by_value(452, &|v| (v as f64 * 0.25).to_bits() as i64));
    let hb = [word(6417, 0, &[Arg::V(0)]), word(6417, 0, &[Arg::V(1)])];
    for w in hb {
        assert_eq!(w, w as i32 as i64, "hashbool: {w:#x} is not sign-extended (D2311)");
    }
    // the text family over views: hashtext 400 (the hand adapter; C collation 950), hashname 455, hashbytea 6413
    let texts: Vec<String> = (0..60).map(|i| format!("w{i}")).collect();
    let over = |oid: u32, coll: u32| texts.iter().map(|t| word(oid, coll, &[Arg::R(View::of(t.as_bytes()))])).collect::<Vec<_>>();
    check("hashtext", &over(400, 950));
    check("hashbytea", &over(6413, 0));
}
