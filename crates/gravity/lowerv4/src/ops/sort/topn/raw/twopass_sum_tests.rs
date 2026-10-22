//! 5c-F6: `admitted` under a sum top's rule (`Rule::top_count` with a sum `lead`, twopass.count over `sum(x)`) admits
//! every pass-1 record whose sum ranks with the n-th best of them all, ties included: integer and numeric sums exact
//! (x of either sign, NULL sums by the order's NULLS FIRST / LAST, numeric NaN / Infinity on PostgreSQL's numeric
//! order, the OVERFLOW sentinel always), a float sum every record some fold order could put in the top. One owner (W0)
//! and eight. Declared by raw.rs (tests only).
#![allow(unsafe_code)]
use std::collections::BTreeSet;

use gravity_abi::breaker::{KeyKind, Owned, OwnerTop, RecordLayout, SlotKind, NUM_SENT_HI, OWNED_HDR, OWNERTOP_HDR, OWNER_RUN_OFF, TABLE_OWNED};
use gravity_abi::hash::KeyHash;

use super::admitted;
use crate::ops::shared::twopass::{lead_of, Rule};

/// A pass-1 record: its key, its sum's words (`None`: NULL), its scaled absolute sum and its row count.
#[derive(Clone)]
struct R {
    key: u64,
    sum: Option<[u64; 4]>,
    abs: f64,
    rows: i64,
}

/// `GroupRec::led`'s layout over `kind`: the sum at slot 0, a float's absolute sum at slot 1, the row count after them.
fn layout(kind: SlotKind) -> RecordLayout {
    let float = matches!(kind, SlotKind::SumF64 | SlotKind::SumF32);
    let slots: &[SlotKind] = if float { &[kind, SlotKind::SumF64, SlotKind::Count] } else { &[kind, SlotKind::Count] };
    RecordLayout::with_sentinel(&[KeyKind::I64], slots, true, Some(slots.len() - 1)).partitioned_by(1)
}

/// The owner runs of `recs`, each record in the owner its key routes to.
fn region(l: &RecordLayout, recs: &[R], nowners: usize) -> Vec<u64> {
    let rec = l.size() as usize;
    let obytes = OWNER_RUN_OFF + (recs.len() + 1) * rec;
    let mut v = vec![0u64; (OWNED_HDR + nowners * obytes).div_ceil(8)];
    let o = v.as_mut_ptr() as *mut Owned;
    unsafe { *o = Owned { nowners: nowners as u32, owner_bytes: obytes as u32, limit: 0, pad0: [0; 3], kind: TABLE_OWNED, base: 0, pad1: [0; 5], cell: core::cell::UnsafeCell::new([]) } };
    let o = unsafe { Owned::view(v.as_mut_ptr() as *const u8) };
    let mut fill = vec![0usize; nowners];
    let float = matches!(l.slot_kind(0), SlotKind::SumF64 | SlotKind::SumF32);
    for r in recs {
        let j = (KeyHash::of(&[r.key]).part(8) as usize) % nowners;
        let base = o.owner(j);
        unsafe {
            let d = base.add(OWNER_RUN_OFF + fill[j] * rec);
            *(d as *mut u64) = r.key;
            if let Some(w) = r.sum {
                let s = d.add(l.slot_off(0) as usize) as *mut u64;
                let n = if l.slot_kind(0) == SlotKind::SumNumWide { 4 } else { 2 };
                (0..n.min(l.slot_kind(0).bytes() as usize / 8)).for_each(|i| *s.add(i) = w[i]);
                let (byte, bit) = l.valid_bit(0);
                *d.add(byte) |= 1 << bit;
            }
            if float {
                *(d.add(l.slot_off(1) as usize) as *mut f64) = r.abs;
            }
            *(d.add(l.sentinel_off() as usize) as *mut i64) = r.rows;
            fill[j] += 1;
            (*(base as *mut OwnerTop)).worst = 1;
            let dir = base.add(OWNERTOP_HDR) as *mut u32;
            (*dir, *dir.add(1)) = (OWNER_RUN_OFF as u32, fill[j] as u32);
        }
    }
    v
}

/// The keys `admitted` hands on under a sum top's rule over `kind`.
fn admit(kind: SlotKind, recs: &[R], nowners: usize, n: u32, desc: bool, nulls_first: bool) -> BTreeSet<u64> {
    let l = layout(kind);
    let r = region(&l, recs, nowners);
    let lead = lead_of(kind).expect("a sum lead");
    let rule = Rule { top_count: true, lead, desc, nulls_first, n, img_words: 1, rec_words: (l.size() / 8) as u8, count_word: (l.slot_off(0) / 8) as u8, ..Rule::default() };
    let words = [rule.pack(None)];
    assert_eq!(Rule::unpack(words[0]), rule, "the rule round-trips its arg word");
    let mut got = BTreeSet::new();
    let m = unsafe { admitted(r.as_ptr() as *const u8, &words, None, &mut |p, _| assert!(got.insert(*p), "a record admitted once")) };
    assert_eq!(m, Some(got.len()));
    got
}

/// `a` against `b` on a top's order, `Greater` where `a` ranks first (`Ord` on values = SQL's ascending order, NULL
/// as `None` placed by `nulls_first`).
fn cmp_rank<V: Ord>(a: &Option<V>, b: &Option<V>, desc: bool, nulls_first: bool) -> core::cmp::Ordering {
    use core::cmp::Ordering::{Equal, Greater, Less};
    match (a, b) {
        (None, None) => Equal,
        (None, Some(_)) => if nulls_first { Greater } else { Less },
        (Some(_), None) => if nulls_first { Less } else { Greater },
        (Some(x), Some(y)) => if desc { x.cmp(y) } else { y.cmp(x) },
    }
}

/// Every key ranking at least as well as the n-th best of all: a reference top's rows with their ties (every key
/// when there are fewer than n).
fn want<V: Ord + Copy>(vals: &[(u64, Option<V>)], n: u32, desc: bool, nulls_first: bool) -> BTreeSet<u64> {
    let mut ord: Vec<_> = vals.to_vec();
    ord.sort_by(|a, b| cmp_rank(&b.1, &a.1, desc, nulls_first));
    let Some(&(_, t)) = ord.get(n as usize - 1) else { return vals.iter().map(|v| v.0).collect() };
    vals.iter().filter(|v| cmp_rank(&v.1, &t, desc, nulls_first) != core::cmp::Ordering::Less).map(|v| v.0).collect()
}

fn int_words(v: i128) -> [u64; 4] {
    [v as u64, (v >> 64) as u64, (v >> 127) as u64, (v >> 127) as u64]
}

/// Integer sums of either sign, many ties, every eleventh NULL.
fn int_recs() -> Vec<(u64, Option<i128>)> {
    (1..=90u64).map(|k| (k.wrapping_mul(7919) % 100_003, (k % 11 != 0).then_some((k as i128 * 37 % 23) - 11))).collect()
}

#[test]
fn an_integer_sum_top_admits_the_records_ranking_with_its_n_th_sum() {
    let vals = int_recs();
    for kind in [SlotKind::SumI64, SlotKind::SumI128, SlotKind::SumNum, SlotKind::SumNumWide] {
        let recs: Vec<R> = vals.iter().map(|&(key, v)| R { key, sum: v.map(int_words), abs: 0.0, rows: 3 }).collect();
        for nowners in [1usize, 8] {
            for n in [1u32, 2, 5, 9, 40, 89, 90, 91, 500] {
                for (desc, nulls_first) in [(true, true), (true, false), (false, false), (false, true)] {
                    let (got, w) = (admit(kind, &recs, nowners, n, desc, nulls_first), want(&vals, n, desc, nulls_first));
                    assert_eq!(got, w, "{kind:?} owners {nowners} n {n} desc {desc} nulls_first {nulls_first}");
                }
            }
        }
    }
}

#[test]
fn a_numeric_sum_ranks_its_specials_and_admits_its_overflow() {
    let sent = |c: u64| Some([c, NUM_SENT_HI as u64, 0, 0]);
    let wide = |c: u64| Some([c, 0, 0, NUM_SENT_HI as u64]);
    // 1: NaN, 2: +Infinity, 3: -Infinity, 4: 5, 5: -7, 6: NULL, 7: OVERFLOW
    for (kind, sp) in [(SlotKind::SumNum, sent as fn(u64) -> Option<[u64; 4]>), (SlotKind::SumNumWide, wide)] {
        let recs: Vec<R> = [(1u64, sp(1)), (2, sp(2)), (3, sp(3)), (4, Some(int_words(5))), (5, Some(int_words(-7))), (6, None), (7, sp(4))]
            .iter()
            .map(|&(key, sum)| R { key, sum, abs: 0.0, rows: 1 })
            .collect();
        let got = |n, desc, nf| admit(kind, &recs, 1, n, desc, nf);
        assert_eq!(got(1, true, false), BTreeSet::from([1, 7]), "{kind:?} DESC NULLS LAST: NaN first, the D299 sentinel always");
        assert_eq!(got(2, true, false), BTreeSet::from([1, 2, 7]), "{kind:?}: then +Infinity");
        assert_eq!(got(1, true, true), BTreeSet::from([6, 7]), "{kind:?} DESC NULLS FIRST: the NULL sum");
        assert_eq!(got(1, false, false), BTreeSet::from([3, 7]), "{kind:?} ASC: -Infinity first");
        assert_eq!(got(2, false, false), BTreeSet::from([3, 5, 7]), "{kind:?} ASC: then -7");
        assert_eq!(got(7, false, false).len(), 7, "{kind:?}: n past the records admits every one");
    }
}

/// A deterministic shuffle of `xs` (`seed`), for another fold order.
fn shuffled(xs: &[f64], seed: u64) -> Vec<f64> {
    let mut v = xs.to_vec();
    let mut s = seed | 1;
    for i in (1..v.len()).rev() {
        s = s.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        v.swap(i, (s >> 33) as usize % (i + 1));
    }
    v
}

fn fsum(xs: &[f64], f32: bool) -> f64 {
    match f32 {
        true => xs.iter().fold(0f32, |a, &x| a + x as f32) as f64,
        false => xs.iter().fold(0f64, |a, &x| a + x),
    }
}

/// PostgreSQL's float order as an `Ord` key (NaN greatest, -0 as 0).
fn fkey(x: f64) -> (u8, i64) {
    if x.is_nan() {
        return (3, 0);
    }
    let b = (x + 0.0).to_bits() as i64;
    (1, b ^ ((b >> 63) as u64 >> 1) as i64)
}

#[test]
fn a_float_sum_top_admits_every_group_another_fold_order_could_rank_in_it() {
    // groups whose sums cancel to near-ties: big terms of either sign and a small spread, so fold orders disagree in
    // the last bits; pass 1 sums in one order, the reference top in others
    for f32 in [false, true] {
        let kind = if f32 { SlotKind::SumF32 } else { SlotKind::SumF64 };
        let groups: Vec<(u64, Vec<f64>)> = (1..=60u64)
            .map(|k| {
                let big = 1e8 * (1 + k % 3) as f64;
                let tiny = (k % 7) as f64 * 1e-9 + 0.1 * (k % 2) as f64;
                let terms = vec![big, 0.3, -big, tiny, 1.0 / 3.0, -0.25 * (k % 5) as f64, big * 0.5, -big * 0.5];
                let terms = if f32 { terms.iter().map(|&x| x as f32 as f64).collect() } else { terms };
                (k, terms)
            })
            .collect();
        let recs: Vec<R> = groups
            .iter()
            .map(|(k, xs)| R { key: *k, sum: Some([fsum(xs, f32).to_bits(), 0, 0, 0]), abs: xs.iter().map(|x| x.abs() * 2f64.powi(-128)).sum(), rows: xs.len() as i64 })
            .collect();
        for nowners in [1usize, 8] {
            for n in [1u32, 3, 10, 30, 59] {
                for desc in [true, false] {
                    let got = admit(kind, &recs, nowners, n, desc, false);
                    for seed in 1..20u64 {
                        let vals: Vec<(u64, Option<(u8, i64)>)> = groups.iter().map(|(k, xs)| (*k, Some(fkey(fsum(&shuffled(xs, seed * 977 + *k), f32))))).collect();
                        let w = want(&vals, n, desc, false);
                        assert!(w.is_subset(&got), "{kind:?} n {n} desc {desc} seed {seed}: missed {:?}", w.difference(&got).collect::<Vec<_>>());
                    }
                }
            }
        }
        // well-separated sums admit exactly the top
        let recs: Vec<R> = (1..=50u64).map(|k| R { key: k, sum: Some([((k * 37 % 50) as f64 - 20.5).to_bits(), 0, 0, 0]), abs: 30.0 * 2f64.powi(-128), rows: 4 }).collect();
        assert_eq!(admit(kind, &recs, 1, 5, true, false).len(), 5, "{kind:?}: no slack past rounding");
    }
}

#[test]
fn a_float_sum_ranks_nan_and_the_infinities_exactly() {
    let f = |x: f64| Some([x.to_bits(), 0, 0, 0]);
    let recs: Vec<R> = [(1u64, f(f64::NAN)), (2, f(f64::INFINITY)), (3, f(f64::NEG_INFINITY)), (4, f(2.5)), (5, f(-0.0)), (6, None), (7, f(0.0))]
        .iter()
        .map(|&(key, sum)| R { key, sum, abs: 2.5 * 2f64.powi(-128), rows: 1 })
        .collect();
    let got = |n, desc, nf| admit(SlotKind::SumF64, &recs, 1, n, desc, nf);
    assert_eq!(got(1, true, false), BTreeSet::from([1]), "DESC: NaN above every value");
    assert_eq!(got(3, true, false), BTreeSet::from([1, 2, 4]), "then +Infinity, 2.5");
    assert_eq!(got(4, true, false), BTreeSet::from([1, 2, 4, 5, 7]), "-0 ties 0");
    assert_eq!(got(1, false, true), BTreeSet::from([6]), "ASC NULLS FIRST: the NULL sum");
    assert_eq!(got(1, false, false), BTreeSet::from([3]), "ASC: -Infinity first");
}
