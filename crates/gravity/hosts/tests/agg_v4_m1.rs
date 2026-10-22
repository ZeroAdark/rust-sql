//! What M1's runtime protocols build from a finished breaker region (hosts v4 m1): the two-pass admitted set and a join's
//! runtime filter, each over a reader standing in for the lowered program's; and the dense domain `facts` reads from a
//! snapshot's parts. A module of gravity_hosts_suite (it shares a process: tests/registered.rs).

use gravity_abi::hash::{Depth, KeyHash};
use gravity_hosts::ops::scan::pgrc::granule_source::part_extrema;
use gravity_hosts::ops::agg::hosts::{admit_set, rt_filter, AdmitSet};
use gravity_kernels::table::entries::{admit_filter_bits, ADMIT_ALL, ADMIT_EMPTY, ADMIT_FILTER_AT, ADMIT_FILTER_BITS};
use pgrc_qa::corpus::{build_fixture, int8_fixture};
use pgrc_write::writer::PartCutPolicy;

#[path = "common/granule_fixtures.rs"]
#[allow(dead_code)]
mod granule_fixtures;
use granule_fixtures::*;

fn set_of(items: &[(u64, u64, u64)]) -> Option<AdmitSet> {
    admit_set(&mut |emit| {
        items.iter().for_each(|&(a, b, c)| emit(a, b, c));
        Some(items.len())
    }, None)
    .expect("no backend installed: no interrupt raises")
}

/// Whether `key` of `nw` words passes `a`'s prefilter (V4R-209): both of its bits set.
fn passes(a: &AdmitSet, key: [u64; 2], nw: usize) -> bool {
    let h = KeyHash::of(&key[..nw]);
    ADMIT_FILTER_AT.iter().all(|&at| {
        let p = h.home(a.filter_bits as u32, at);
        a.filter[p / 64] >> (p % 64) & 1 == 1
    })
}

/// Whether `key` of `nw` words is in `a`, probed as the pass-2 IR probes: the prefilter, then from its KeyHash home, two
/// words per slot.
fn holds(a: &AdmitSet, key: [u64; 2], nw: usize) -> bool {
    if !passes(a, key, nw) {
        return false;
    }
    if key == [ADMIT_EMPTY; 2] {
        return a.empty_key == 1;
    }
    let bits = (a.mask + 1).trailing_zeros();
    let mut i = KeyHash::of(&key[..nw]).home(bits, Depth::TOP);
    loop {
        let s = [a.set[2 * i], a.set[2 * i + 1]];
        if s == key {
            return true;
        }
        if s == [ADMIT_EMPTY; 2] {
            return false;
        }
        i = (i + 1) & a.mask as usize;
    }
}

#[test]
fn the_admitted_set_holds_each_distinct_key_once_at_most_half_full() {
    let one: Vec<(u64, u64, u64)> = (0..300u64).map(|k| (k * 7 % 200, 99, 1)).collect();
    let a = set_of(&one).unwrap();
    assert_eq!((a.count, a.empty_key, a.mask + 1), (200, 0, 1024), "sized by the keys read, duplicates included");
    assert_eq!(a.set.len() as u64, 2 * (a.mask + 1));
    assert!((0..200).all(|k| holds(&a, [k, 0], 1)), "a one-word key sits as (w0, 0)");
    assert!(!holds(&a, [200, 0], 1) && !holds(&a, [5, 99], 1));
    let two = [(1, 2, 2), (ADMIT_EMPTY, ADMIT_EMPTY, 2), (1, 2, 2), (ADMIT_EMPTY, 7, 2), (ADMIT_EMPTY, ADMIT_EMPTY, 2)];
    let a = set_of(&two).unwrap();
    assert_eq!((a.count, a.empty_key, a.mask + 1), (3, 1, 64));
    assert!(holds(&a, [1, 2], 2) && holds(&a, [ADMIT_EMPTY, 7], 2) && holds(&a, [ADMIT_EMPTY; 2], 2) && !holds(&a, [2, 1], 2));
    let none = set_of(&[]).unwrap();
    assert_eq!((none.count, none.mask), (0, 63));
    assert!(admit_set(&mut |_| None, None).expect("no backend installed").is_none(), "a region the reader does not know is no set");
}

/// V4R-209: the prefilter has no false negatives: every key of the exact set passes it, over random keys, keys that share
/// the exact set's home or one filter bit, a saturated set, the empty pair, and the whole sets (`ADMIT_ALL`, nothing).
#[test]
fn every_admitted_key_passes_the_prefilter() {
    let mut x = 0x2545_f491_4f6c_dd1du64;
    let mut rnd = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let random: Vec<(u64, u64, u64)> = (0..5000).map(|i| (rnd(), rnd(), 1 + i % 2)).collect();
    let home0 = |k: u64| KeyHash::of(&[k]).home(6, Depth::TOP);
    let same_home: Vec<(u64, u64, u64)> = (0..200_000u64).filter(|&k| home0(k) == home0(0)).take(40).map(|k| (k, 0, 1)).collect();
    let bit0 = |k: u64| KeyHash::of(&[k]).home(ADMIT_FILTER_BITS, ADMIT_FILTER_AT[0]);
    let same_bit: Vec<(u64, u64, u64)> = (0..2_000_000u64).filter(|&k| bit0(k) == bit0(7)).take(20).map(|k| (k, 0, 1)).collect();
    let strided: Vec<(u64, u64, u64)> = (0..4096u64).map(|k| (k << 32, k << 48, 2)).collect();
    let saturated: Vec<(u64, u64, u64)> = (0..200_000u64).map(|k| (k, 0, 1)).collect();
    let edges = vec![(ADMIT_EMPTY, ADMIT_EMPTY, 2), (ADMIT_EMPTY, 0, 2), (0, ADMIT_EMPTY, 2), (0, 0, 2), (ADMIT_EMPTY, 0, 1)];
    assert!(same_home.len() == 40 && same_bit.len() == 20, "the adversarial sets found their collisions");
    for items in [&random, &same_home, &same_bit, &strided, &saturated, &edges] {
        let a = set_of(items).unwrap();
        assert_eq!((a.filter.len() << 6, a.filter_bits as u32), (1 << a.filter_bits, admit_filter_bits(items.len())), "the filter is sized from the key count");
        let key = |&(w0, w1, nw): &(u64, u64, u64)| ([w0, if nw == 2 { w1 } else { 0 }], nw as usize);
        assert!(items.iter().map(key).all(|(k, nw)| passes(&a, k, nw) && holds(&a, k, nw)), "no false negative");
    }
    let small = set_of(&[(1, 0, 1), (2, 0, 1)]).unwrap();
    assert_eq!(small.filter.len(), 1, "two keys get a one-word filter (B3-14)");
    assert!((3..10_000u64).filter(|&k| passes(&small, [k, 0], 1)).count() < 100, "two keys set at most two bits per depth of 64");
    assert_eq!([0, 1, 8, 9, 1000, 8192, 200_000].map(admit_filter_bits), [6, 6, 6, 7, 13, 16, 16], "8 bits per key, 64 ..= 2^16 bits");
    let (all, none) = (AdmitSet::whole(true), AdmitSet::whole(false));
    assert_eq!((all.count, all.empty_key, none.count, none.empty_key), (1, ADMIT_ALL, 0, 0));
    assert!((0..1000u64).all(|k| passes(&all, [k, k], 2) && !passes(&none, [k, k], 2)));
    assert!(passes(&all, [ADMIT_EMPTY; 2], 2) && !passes(&none, [ADMIT_EMPTY; 2], 2));
    assert!(!passes(&set_of(&[]).unwrap(), [0, 0], 1), "an empty set's filter is clear: it admits nothing");
}

#[test]
fn the_runtime_filter_is_the_build_s_range_and_small_key_set_and_an_empty_build_skips_all() {
    let read = |items: &[(u64, u64, u64)], n: Option<usize>| {
        rt_filter(4, 64, &|| None, &mut |emit| {
            items.iter().for_each(|&(i, a, b)| emit(i, a, b));
            n
        }, None)
    };
    let f = read(&[(0, (-10i64) as u64, 20), (1, 15, 0), (1, (-10i64) as u64, 0), (1, 15, 0)], Some(4)).unwrap();
    assert_eq!((f.attno, f.lo, f.hi, &*f.runs), (4, -10, 20, &[(-10i64, -10i64), (15, 15)][..]));
    let f = read(&[(0, 1, 900)], Some(1)).unwrap();
    assert!(f.runs.is_empty() && (f.lo, f.hi) == (1, 900), "a large build exports its range alone");
    let f = read(&[], Some(0)).unwrap();
    assert!(f.lo > f.hi, "an empty build");
    assert!(read(&[(1, 3, 0)], Some(1)).is_none(), "keys without a range are no filter");
    assert!(read(&[(0, 1, 2)], None).is_none());
    // PS1-22: a text build's fingerprints (tag 2, lo word then hi word), no range; over the cap the count alone is no filter
    let f = read(&[(2, 5, 1), (2, 3, 9), (2, 5, 1)], Some(3)).unwrap();
    assert_eq!((f.lo, f.hi, f.runs.is_empty(), &*f.fps), (i64::MIN, i64::MAX, true, &[5u128 | 1 << 64, 3 | 9 << 64][..]));
    assert!(read(&[], Some(1)).is_none(), "an over-cap text build exports its count alone: no filter");
    assert!(read(&[(0, 1, 2), (2, 3, 4)], Some(2)).is_none(), "a range beside fingerprints is no reader's");
    assert!(read(&[(1, 1, 0), (2, 3, 4)], Some(2)).is_none(), "keys beside fingerprints are no reader's");
    // D3245: keys past the cap fold into runs of consecutive keys, kept while at most the cap; past it the range alone
    let read_clip = |items: &[(u64, u64, u64)], cap: usize, clip: Option<(i64, i64)>| rt_filter(4, cap, &|| clip, &mut |emit| {
        items.iter().for_each(|&(i, a, b)| emit(i, a, b));
        Some(items.len())
    }, None);
    let read_cap = |items: &[(u64, u64, u64)], cap: usize| read_clip(items, cap, None);
    let mut items = vec![(0, 0, 499)];
    items.extend((0..100).chain(200..300).chain(400..500).map(|k| (1, k, 0)));
    assert_eq!(&*read_cap(&items, 3).unwrap().runs, &[(0, 99), (200, 299), (400, 499)][..], "300 keys in 3 runs within a cap of 3");
    assert!(read_cap(&items, 2).unwrap().runs.is_empty(), "3 runs past a cap of 2: the range alone");
    // D3245: keys past the cap clipped to the probe column's range first: 2 runs left within a cap of 2
    let f = read_clip(&items, 2, Some((150, 1_000))).unwrap();
    assert_eq!((&*f.runs, f.lo, f.hi), (&[(200, 299), (400, 499)][..], 0, 499), "the clip keeps the runs the probe can meet");
    let spread: Vec<(u64, u64, u64)> = [(0, 0, 20_000)].into_iter().chain((0..10_000).map(|k| (1, 2 * k, 0))).collect();
    assert_eq!(read_clip(&spread, 64, Some((100, 160))).unwrap().runs.len(), 31, "10,000 spread keys clipped to 31 within the cap");
    assert!(read_clip(&spread, 64, Some((100, 1_000))).unwrap().runs.is_empty(), "451 clipped keys past the cap: the range alone");
}

#[test]
fn a_negative_offset_or_limit_raises_postgres_s_error_and_sqlstate() {
    use types_error::{ERRCODE_INVALID_ROW_COUNT_IN_LIMIT_CLAUSE, ERRCODE_INVALID_ROW_COUNT_IN_RESULT_OFFSET_CLAUSE};
    for (which, msg, code) in [(0, "OFFSET must not be negative", ERRCODE_INVALID_ROW_COUNT_IN_RESULT_OFFSET_CLAUSE), (1, "LIMIT must not be negative", ERRCODE_INVALID_ROW_COUNT_IN_LIMIT_CLAUSE)] {
        let raised = unsafe { gravity_kernels::ops::limit::limit_negative(which) };
        assert_eq!(raised, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST));
        let e = gravity_fn::take_error().expect("stashed");
        assert_eq!((e.message(), e.sqlstate()), (msg, code));
    }
}

/// `Fact::DenseLo` / `DenseHi`'s source: the parts' exact extrema over three parts, the first all NULL (it adds nothing),
/// the others' NULLs ignored; a relation of NULLs alone has none.
#[test]
fn the_dense_domain_is_the_parts_exact_extrema_and_nulls_add_nothing() {
    let policy = PartCutPolicy { max_rows: 16_384, ..PartCutPolicy::default() };
    let value = |i: u64| (i >= 16_384 && i % 7 != 3).then(|| -40 + (i as i64 * 37) % 1_000);
    let bf = build_fixture(int8_fixture("gravity_facts_dense", 7351, 40_000, vec![], policy.clone(), value));
    let list = part_list(&bf);
    assert!(list.len() >= 3, "three parts, got {}", list.len());
    let (lo, hi) = (0..40_000).filter_map(value).fold((i64::MAX, i64::MIN), |(l, h), v| (l.min(v), h.max(v)));
    assert_eq!(part_extrema(&list, 1).unwrap(), Some((lo, hi)));
    let nulls = build_fixture(int8_fixture("gravity_facts_nulls", 7352, 20_000, vec![], policy, |_| None));
    assert_eq!(part_extrema(&part_list(&nulls), 1).unwrap(), None);
}
