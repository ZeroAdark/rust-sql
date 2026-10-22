//! join.dense's bits form (H23a). Budget 120 lines. Owner M1-JOIN. A 32-bit integer key with at most one
//! one-byte by-value payload (`eligible`) writes compact 8 B records (`store`); keys unique over a span the bitmaps afford
//! lay out `DBITS` (an exist bitmap at `DIR`, a stride `SLOTS - DIR` apart the value and NULL bitmaps for a bool, the NULL
//! bitmap and a 16-bit value plane for any other payload; no slots), else the records expand in place to the full layout
//! (`expand`) for the other forms. A probe's hit is a record built on its frame (`first`).
use core::sync::atomic::Ordering::Relaxed;
use core::sync::atomic::{AtomicU16, AtomicU64, AtomicU8};

use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, IntCond, Type, Value};

use gravity_decompose::catalog::typefacts::TypeFacts;

use crate::frame::env::Env;

use crate::ops::hashjoin::build::{ALLOW, A_DENSE, DBITS, DIR, FLAGS, FULL, F_COMPACT, F_DUP, F_RANGE, HDR, HI, LO, N, PAY, REC, SLOTS, VBITS};
use crate::ops::hashjoin::rt::{rd, wr, Rt};
use crate::ops::hashjoin::build_side::Shape;
use crate::ops::hashjoin::strategies::dense::bitmap;
use crate::ops::hashjoin::probe::Hv;

mod raw;
pub(crate) use raw::{expand, lay, need, prepare, put};

const BOOLOID: u32 = 16;
/// The value bits of a compact payload other than a bool's: its word's low 16 bits.
pub(crate) const WIDE: u8 = 16;

/// The compact payload's value bits when the build writes compact records (`None`: full records): a key the record holds
/// exactly (`key_fits`), payloads `pay` by their type facts (`value_bits`), a probe that reads the dense forms and no key
/// list (lever C: join.mult reads a present key as 1, a duplicate collapses).
pub(crate) fn eligible(sh: &Shape, key: &TypeFacts, pay: &[Option<TypeFacts>]) -> Option<u8> {
    value_bits(pay).filter(|_| may(sh) && key_fits(key))
}

/// JOIN-02: at most one payload, a one-byte by-value word (bool, "char", a domain over one: it stores as its base). A bool's
/// 0 / 1 takes one bit; any other one its word's low 16 bits, which sign-extend back to the word whether its source sign-
/// (`fetch_att`) or zero-extended the byte.
pub(crate) fn value_bits(pay: &[Option<TypeFacts>]) -> Option<u8> {
    match pay {
        [] => Some(1),
        [Some(t)] if t.byval && t.typlen == 1 => Some(if t.base == BOOLOID { 1 } else { WIDE }),
        _ => None,
    }
}

/// A key whose word is the sign extension of at most 32 bits, so the record's low half reads it back (`w as u32 as i32`):
/// by its type facts, a by-value signed raw image of at most 4 bytes (OIDW-8: int2, int4, date, and a domain over one, which
/// keys as its base). An unsigned image (oid, the reg* types, xid) is not: a computed value of one is zero-extended.
pub(crate) fn key_fits(key: &TypeFacts) -> bool {
    key.byval && (1..=4).contains(&key.typlen) && key.image.is_signed_word()
}

/// Whether the probe may meet `DBITS` (the build's `eligible` less its key type, which the probe's own key shares).
pub(crate) fn may(sh: &Shape) -> bool {
    sh.allow & A_DENSE != 0 && !sh.swapped && sh.payload.len() <= 1
}

/// The compact record at `p`: `key`'s low 32 bits, then the payload's value (`I64`, its low `vbits`) and NULL (`I1`).
pub(crate) fn store(b: &mut FunctionBuilder<'_>, p: Value, key: Value, pay: Option<(Value, Value)>, vbits: u8) {
    let low = b.iconst(Type::I64, u32::MAX as i64);
    let mut w = b.int_bin(IntBinOp::And, key, low);
    if let Some((v, null)) = pay {
        let mask = if vbits == WIDE { u16::MAX as i64 } else { 1 };
        let (m, s32, sn) = (b.iconst(Type::I64, mask), b.iconst(Type::I64, 32), b.iconst(Type::I64, 32 + vbits as i64));
        let v = if b.value_type(v) == Type::I64 { v } else { b.conv(ConvOp::Zext, Type::I64, v) };
        let (v1, n1) = (b.int_bin(IntBinOp::And, v, m), b.conv(ConvOp::Zext, Type::I64, null));
        let (vb, nb) = (b.int_bin(IntBinOp::Shl, v1, s32), b.int_bin(IntBinOp::Shl, n1, sn));
        let vn = b.int_bin(IntBinOp::Or, vb, nb);
        w = b.int_bin(IntBinOp::Or, w, vn);
    }
    b.store(p, 0, w);
}

/// The keys' span when a compact build may lay `DBITS`: `n` records over `[lo, hi]`, at most 64 x `n` wide.
pub(crate) fn span(lo: i64, hi: i64, n: usize) -> Option<usize> {
    let span = (hi as i128 - lo as i128 + 1).max(0) as u128;
    (n > 0 && span <= 64 * n as u128).then_some(span as usize)
}

/// A compact payload's value bits from its region's `VBITS` byte (a bool's 1 unstamped).
fn vbits(stamp: u8) -> u8 {
    if stamp == WIDE { WIDE } else { 1 }
}

/// The value and NULL planes of a `DBITS` region at `dir` / `slots` (a stride `slots - dir` apart), once per window (`Hv`):
/// a bool's value bitmap then its NULL one, else the NULL bitmap then the 16-bit value plane.
pub(crate) fn planes(b: &mut FunctionBuilder<'_>, dir: Value, slots: Value, sh: &Shape) -> (Value, Value) {
    let (d0, s0) = (b.conv(ConvOp::PtrToInt, Type::I64, dir), b.conv(ConvOp::PtrToInt, Type::I64, slots));
    let stride = b.int_bin(IntBinOp::Sub, s0, d0);
    let (one, two) = (b.ptr_add(dir, stride, 1, 0), b.ptr_add(dir, stride, 2, 0));
    if sh.vbits.get() == WIDE { (two, one) } else { (one, two) }
}

/// Key `k`'s hit under `DBITS` as values (`I64` 0 / 1): present, the payload's value and NULL bits (0 without a payload),
/// branch-free: a key out of range reads word 0 (a laid form has one) and is absent. V4R-156: the single-row body reads
/// them as they are, the walk as a record (`first`).
pub(crate) fn hit(b: &mut FunctionBuilder<'_>, env: &Env, hv: &Hv, k: Value, sh: &Shape) -> [Value; 3] {
    let off = b.int_bin(IntBinOp::Sub, k, hv.lo);
    let inr = b.icmp(IntCond::Ult, off, hv.span);
    let (o, six, sixty3) = (b.select(inr, off, env.zero), b.iconst(Type::I64, 6), b.iconst(Type::I64, 63));
    let (wi, bi) = (b.int_bin(IntBinOp::Ushr, o, six), b.int_bin(IntBinOp::And, o, sixty3));
    let bit = |b: &mut FunctionBuilder<'_>, p: Value| {
        let wp = b.ptr_add(p, wi, 8, 0);
        let w = b.load(Type::I64, wp, 0);
        let x = b.int_bin(IntBinOp::Ushr, w, bi);
        b.int_bin(IntBinOp::And, x, env.one)
    };
    let (e, i1) = (bit(b, hv.dir), b.conv(ConvOp::Zext, Type::I64, inr));
    let set = b.int_bin(IntBinOp::And, e, i1);
    match sh.payload.is_empty() {
        true => [set, env.zero, env.zero],
        false => {
            let (vp, np) = hv.planes.unwrap_or_else(|| planes(b, hv.dir, hv.slots, sh));
            let v = match sh.vbits.get() == WIDE {
                true => {
                    let at = b.ptr_add(vp, o, 2, 0);
                    let h = b.load(Type::I16, at, 0);
                    b.conv(ConvOp::Sext, Type::I64, h)
                }
                false => bit(b, vp),
            };
            [set, v, bit(b, np)]
        }
    }
}

/// Key `k`'s hit under `DBITS` as a record on the frame (`sh.rec` bytes: the link's NULL bit and the payload word; its
/// image is never read, the form being exact): its address (`I64`), 0 when the key is absent.
pub(crate) fn first(b: &mut FunctionBuilder<'_>, env: &Env, hv: &Hv, k: Value, sh: &Shape) -> Value {
    let ([set, v, nul], slot) = (hit(b, env, hv, k, sh), b.declare_stack_slot(sh.rec.get(), 8));
    let (a, s32) = (b.stack_addr(slot), b.iconst(Type::I64, 32));
    let link = b.int_bin(IntBinOp::Shl, nul, s32);
    (b.store(a, 0, k), b.store(a, sh.link_off(), link));
    if !sh.payload.is_empty() {
        b.store(a, sh.pay_off(0), v);
    }
    let (ai, has) = (b.conv(ConvOp::PtrToInt, Type::I64, a), b.icmp(IntCond::Ne, set, env.zero));
    b.select(has, ai, env.zero)
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use crate::ops::hashjoin::build::{ALLOW, A_DENSE, DBITS, DIR, FLAGS, FORM, FULL, F_COMPACT, F_DUP, F_RANGE, HDR, HI, LO, N, PHASE, REC, SLOTS, VBITS, WORDS};
    use crate::ops::hashjoin::rt::{rd, wr, Rt};

    /// A compact region of `keys` (value: key odd, NULL: key % 5 == 0) with room for its bitmaps and its expansion.
    fn region(keys: &[i64]) -> Vec<u64> {
        let mut v = vec![0u64; (HDR + 24 * keys.len() + 4096) / 8];
        let d = v.as_mut_ptr() as *mut u8;
        // SAFETY: offsets inside `v`.
        unsafe {
            (wr::<u64>(d, N, keys.len() as u64), wr::<i64>(d, LO, *keys.iter().min().unwrap()), wr::<i64>(d, HI, *keys.iter().max().unwrap()));
            (wr::<u8>(d, FLAGS, F_RANGE | F_COMPACT), wr::<u8>(d, ALLOW, A_DENSE), crate::ops::hashjoin::layout::set_pay(d, 1), wr::<u8>(d, WORDS, 1), wr::<u8>(d, PHASE, 1));
            (wr::<u32>(d, REC, 8), wr::<u32>(d, FULL, 24));
            for (i, &k) in keys.iter().enumerate() {
                wr::<u64>(d.add(HDR + 8 * i), 0, (k as u64 & u32::MAX as u64) | ((k & 1) as u64) << 32 | ((k % 5 == 0) as u64) << 33);
            }
        }
        v
    }

    #[test]
    fn partition_jobs_share_the_words_at_their_edges_and_see_each_others_duplicates() {
        let keys: Vec<i64> = (0..300).rev().filter(|k| k % 7 != 3).collect();
        for cut in [1, 50, 64, 130] {
            for dup in [false, true] {
                let mut ks = keys.clone();
                if dup {
                    ks[cut + 2] = ks[cut - 1];
                }
                let mut v = region(&ks);
                let d = v.as_mut_ptr() as *mut u8;
                // SAFETY: `v` holds the header, the records and room for the bitmaps past them.
                unsafe {
                    let t = Rt::of(d, v.len() * 8);
                    assert!(super::need(d) > 0);
                    super::prepare(&t);
                    (super::put(&t, &t, cut..ks.len()), super::put(&t, &t, 0..cut));
                    assert_eq!(rd::<u8>(d, FLAGS) & F_DUP != 0, dup, "cut {cut}: the other job's key seen");
                    let (dir, stride) = (rd::<u64>(d, DIR) as usize, (rd::<u64>(d, SLOTS) - rd::<u64>(d, DIR)) as usize);
                    let bit = |j: usize, k: i64| rd::<u64>(d.add(dir + j * stride + 8 * (k as usize >> 6)), 0) >> (k & 63) & 1 == 1;
                    for k in (0..300).filter(|_| !dup) {
                        let e = k % 7 != 3;
                        assert_eq!((bit(0, k), bit(1, k), bit(2, k)), (e, e && k & 1 == 1, e && k % 5 == 0), "cut {cut} key {k}");
                    }
                }
            }
        }
    }

    #[test]
    fn unique_keys_lay_the_bits_form_and_a_duplicate_expands_to_full_records() {
        let keys: Vec<i64> = (-40..160).filter(|k| k % 3 != 0).collect();
        let mut v = region(&keys);
        let d = v.as_mut_ptr() as *mut u8;
        // SAFETY: `v` holds the header, the records and room past them.
        unsafe {
            let t = Rt::of(d, v.len() * 8);
            assert_eq!((t.key(0), t.key(1)), (-40, -38), "a compact key reads back sign-extended");
            assert!(super::need(d) > 0 && super::lay(&t));
            assert_eq!((rd::<u8>(d, FORM), rd::<u8>(d, PHASE)), (DBITS, 2));
            let (dir, stride) = (rd::<u64>(d, DIR) as usize, (rd::<u64>(d, SLOTS) - rd::<u64>(d, DIR)) as usize);
            let bit = |j: usize, k: i64| rd::<u64>(d.add(dir + j * stride + 8 * ((k + 40) as usize >> 6)), 0) >> ((k + 40) & 63) & 1 == 1;
            for k in -40..160 {
                assert_eq!(bit(0, k), k % 3 != 0, "exist {k}");
                assert_eq!(bit(1, k), k % 3 != 0 && k & 1 == 1, "value {k}");
                assert_eq!(bit(2, k), k % 3 != 0 && k % 5 == 0, "null {k}");
            }
        }
        let mut dup = keys.clone();
        dup[7] = dup[3];
        let mut v = region(&dup);
        let d = v.as_mut_ptr() as *mut u8;
        // SAFETY: as above; the expansion's `HDR + n x 24` bytes fit.
        unsafe {
            assert!(!super::lay(&Rt::of(d, v.len() * 8)) && rd::<u8>(d, FLAGS) & F_DUP != 0 && rd::<u8>(d, PHASE) == 1);
            super::expand(d);
            let t = Rt::of(d, v.len() * 8);
            assert!(!t.compact && t.rec == 24);
            for (i, &k) in dup.iter().enumerate() {
                assert_eq!((t.key(i), rd::<u64>(t.link(i) as *const u8, 0), rd::<u64>(t.rec(i), 16)), (k, ((k % 5 == 0) as u64) << 32, (k & 1) as u64), "record {i}");
            }
        }
    }

    /// JOIN-02: a one-byte word's payload (a byte sign-extended for an even key, zero-extended for an odd one; NULL: key %
    /// 5 == 0) keeps its low 16 bits: laid, its 16-bit plane past the NULL bitmap; expanded, the word itself.
    #[test]
    fn a_wide_payload_lays_its_plane_and_expands_to_its_word() {
        let val = |k: i64| if k & 1 == 0 { (k * 37) as u8 as i8 as i64 as u64 } else { (k * 37) as u8 as u64 };
        let keys: Vec<i64> = (-40..160).filter(|k| k % 3 != 0).collect();
        for dup in [false, true] {
            let mut ks = keys.clone();
            if dup {
                ks[7] = ks[3];
            }
            let mut v = region(&ks);
            let d = v.as_mut_ptr() as *mut u8;
            // SAFETY: offsets inside `v`, which holds the header, the records, the planes and the expansion.
            unsafe {
                wr::<u8>(d, VBITS, super::WIDE);
                for (i, &k) in ks.iter().enumerate() {
                    wr::<u64>(d.add(HDR + 8 * i), 0, (k as u64 & u32::MAX as u64) | (val(k) & 0xFFFF) << 32 | ((k % 5 == 0) as u64) << 48);
                }
                let t = Rt::of(d, v.len() * 8);
                assert!(super::need(d) > 0);
                assert_eq!(super::lay(&t), !dup);
                if dup {
                    super::expand(d);
                    let t = Rt::of(d, v.len() * 8);
                    for (i, &k) in ks.iter().enumerate() {
                        assert_eq!((t.key(i), rd::<u64>(t.link(i) as *const u8, 0), rd::<u64>(t.rec(i), 16)), (k, ((k % 5 == 0) as u64) << 32, val(k)), "record {i}");
                    }
                    continue;
                }
                let (dir, stride) = (rd::<u64>(d, DIR) as usize, (rd::<u64>(d, SLOTS) - rd::<u64>(d, DIR)) as usize);
                let bit = |j: usize, k: i64| rd::<u64>(d.add(dir + j * stride + 8 * ((k + 40) as usize >> 6)), 0) >> ((k + 40) & 63) & 1 == 1;
                for &k in ks.iter() {
                    let w = rd::<u16>(d.add(dir + 2 * stride + 2 * (k + 40) as usize), 0) as i16 as i64 as u64;
                    assert_eq!((bit(0, k), bit(1, k), w), (true, k % 5 == 0, val(k)), "key {k}");
                }
            }
        }
    }

    /// JOIN-02: the payload's value bits by its type facts: none or a bool's (a domain over one too) 1, any other one-byte
    /// by-value word's ("char", a domain over it) 16; a wider payload, an unknown one or two take full records.
    #[test]
    fn a_compact_payload_is_one_byte_by_value() {
        use gravity_decompose::catalog::typefacts::{row_of, TypeFacts};
        let t = |id: u32| row_of(id).copied();
        let dom = |id: u32| Some(TypeFacts { typid: 90001, ..*row_of(id).unwrap() });
        assert_eq!((super::value_bits(&[]), super::value_bits(&[t(16)]), super::value_bits(&[dom(16)])), (Some(1), Some(1), Some(1)));
        assert_eq!((super::value_bits(&[t(18)]), super::value_bits(&[dom(18)])), (Some(super::WIDE), Some(super::WIDE)));
        for other in [vec![t(23)], vec![t(21)], vec![t(16), t(18)], vec![None]] {
            assert_eq!(super::value_bits(&other), None, "{other:?}");
        }
    }

    /// OIDW-8: the compact key is a type-facts property. Of the seeded rows it picks exactly the signed images of at most
    /// 4 bytes (int2, int4, date); a domain over one keys as its base and fits; an unsigned 4-byte image (oid) and a type
    /// the table does not list (a by-value user type: an unsigned raw word) do not.
    #[test]
    fn a_compact_key_is_a_signed_word_of_at_most_32_bits() {
        use gravity_decompose::catalog::typefacts::{opaque, row_of, ROWS};
        let fits: Vec<u32> = ROWS.iter().filter(|r| super::key_fits(r)).map(|r| r.typid).collect();
        assert_eq!(fits, [21, 23, 1082]);
        let domain = gravity_decompose::catalog::typefacts::TypeFacts { typid: 90001, ..*row_of(23).unwrap() };
        assert!(super::key_fits(&domain));
        assert!(!super::key_fits(row_of(26).unwrap()));
        assert!(!super::key_fits(&opaque(90002, 90002, 4, true, false)));
    }
}
