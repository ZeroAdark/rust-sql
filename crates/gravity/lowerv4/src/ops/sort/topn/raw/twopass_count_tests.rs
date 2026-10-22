//! D-06c: `admitted` under a count top's rule (`Rule::top_count`, twopass.count) admits exactly the pass-1 records whose
//! count ranks with the n-th best of them all, ties at that count included: DESC and ASC, n under, at and past the
//! record count, one owner (W0) and eight, records spread over the owners' runs as the full image routes them. T comes
//! from the records, before any export merges two keys. Declared by raw.rs (tests only).
#![allow(unsafe_code)]
use std::collections::BTreeSet;

use gravity_abi::breaker::{KeyKind, Owned, OwnerTop, RecordLayout, SlotKind, OWNED_HDR, OWNERTOP_HDR, OWNER_RUN_OFF, TABLE_OWNED};
use gravity_abi::hash::KeyHash;

use super::admitted;
use crate::ops::shared::twopass::Rule;

fn layout() -> RecordLayout {
    RecordLayout::with_sentinel(&[KeyKind::I64], &[SlotKind::Count], false, Some(0)).partitioned_by(1)
}

/// The runs form of `recs` (`(key, count)`), each record in the owner its image routes to.
fn region(l: &RecordLayout, recs: &[(u64, i64)], nowners: usize) -> Vec<u64> {
    let rec = l.size() as usize;
    let obytes = OWNER_RUN_OFF + (recs.len() + 1) * rec;
    let mut v = vec![0u64; (OWNED_HDR + nowners * obytes).div_ceil(8)];
    let o = v.as_mut_ptr() as *mut Owned;
    unsafe { *o = Owned { nowners: nowners as u32, owner_bytes: obytes as u32, limit: 0, pad0: [0; 3], kind: TABLE_OWNED, base: 0, pad1: [0; 5], cell: core::cell::UnsafeCell::new([]) } };
    let o = unsafe { Owned::view(v.as_mut_ptr() as *const u8) };
    let mut fill = vec![0usize; nowners];
    for &(k, c) in recs {
        let j = (KeyHash::of(&[k]).part(8) as usize) % nowners;
        let base = o.owner(j);
        unsafe {
            let d = base.add(OWNER_RUN_OFF + fill[j] * rec);
            *(d as *mut u64) = k;
            *(d.add(l.sentinel_off() as usize) as *mut i64) = c;
            fill[j] += 1;
            (*(base as *mut OwnerTop)).worst = 1;
            let dir = base.add(OWNERTOP_HDR) as *mut u32;
            (*dir, *dir.add(1)) = (OWNER_RUN_OFF as u32, fill[j] as u32);
        }
    }
    v
}

/// The keys `admitted` hands on under a count top's rule of bound `n`.
fn admit(recs: &[(u64, i64)], nowners: usize, n: u32, desc: bool) -> BTreeSet<u64> {
    let l = layout();
    let r = region(&l, recs, nowners);
    let rule = Rule { top_count: true, desc, n, img_words: 1, rec_words: (l.size() / 8) as u8, count_word: (l.sentinel_off() / 8) as u8, ..Rule::default() };
    let words = [rule.pack(None)];
    assert_eq!(Rule::unpack(words[0]), rule, "the rule round-trips its arg word");
    let mut got = BTreeSet::new();
    let m = unsafe { admitted(r.as_ptr() as *const u8, &words, None, &mut |p, _| assert!(got.insert(*p), "a record admitted once")) };
    assert_eq!(m, Some(got.len()));
    got
}

/// Every key whose count ranks at least as well as the n-th best of all (every key when there are fewer than n).
fn want(recs: &[(u64, i64)], n: u32, desc: bool) -> BTreeSet<u64> {
    let rank = |c: i64| if desc { c } else { -c };
    let mut ranks: Vec<i64> = recs.iter().map(|r| rank(r.1)).collect();
    ranks.sort_unstable_by(|a, b| b.cmp(a));
    let t = ranks.get(n as usize - 1).copied().unwrap_or(i64::MIN);
    recs.iter().filter(|r| rank(r.1) >= t).map(|r| r.0).collect()
}

#[test]
fn a_count_top_admits_the_records_ranking_with_its_n_th_count() {
    // 60 keys, counts k % 17 + 1: ties at every count, the n-th row often inside one
    let recs: Vec<(u64, i64)> = (1..=60u64).map(|k| (k.wrapping_mul(7919) % 100_003, (k % 17 + 1) as i64)).collect();
    for nowners in [1usize, 8] {
        for n in [1u32, 2, 3, 4, 10, 59, 60, 61, 500] {
            for desc in [true, false] {
                let (got, w) = (admit(&recs, nowners, n, desc), want(&recs, n, desc));
                assert_eq!(got, w, "owners {nowners} n {n} desc {desc}");
                assert!(got.len() >= (n as usize).min(recs.len()), "the n best are all admitted");
            }
        }
    }
}

#[test]
fn t_comes_from_the_records_never_from_a_merged_export() {
    // A = 10, B = 6, C = 6: pass 1 keys each group by its own image, three records; an export that merged B and C (a
    // fingerprint's NULL, raw.rs `key_of`) would read 12 and admit B and C alone at n = 1
    let recs = [(1u64, 10i64), (2, 6), (3, 6)];
    for nowners in [1usize, 8] {
        assert_eq!(admit(&recs, nowners, 1, true), BTreeSet::from([1]), "n = 1: the 10 alone");
        assert_eq!(admit(&recs, nowners, 2, true), BTreeSet::from([1, 2, 3]), "n = 2: the 10 and both 6s (a tie at T)");
        assert_eq!(admit(&recs, nowners, 1, false), BTreeSet::from([2, 3]), "ASC n = 1: both 6s");
        assert_eq!(admit(&recs, nowners, 3, false), BTreeSet::from([1, 2, 3]), "ASC n = 3: all");
    }
}
