//! V4R-173 (L-FLOORPRUNE): parthaving's owner under a HAVING count floor prunes the groups no part record of which reaches
//! `ceil(floor / P)` before its sort. The kept groups and their summed counts must be exactly those of the floor over
//! every group, at the floor's edges (a group summing to F - 1, F, F + 1), with a (group, part) pair arriving more than
//! once (its records merge into one before the reduce), with groups whose parts all sit just under the bound, with
//! thousands of groups at the bound, with group keys of nine words or more (the prune holds any number and width), and
//! with no record at the bound at all (the table empties at once).
use std::collections::BTreeMap;

use gravity_abi::breaker::{HashAgg, KeyKind, OwnerTop, Rec, RecordLayout, SlotKind, WideLayout, GROUPS_UNCOUNTED, MAX_RECORD_KEYS, OWNERTOP_HDR};
use gravity_kernels::ops::sort::topn::parthaving::{owner_fns, PartHavingSpec};
use gravity_kernels::dict::reps::{MAX_REPS, REP_NONE};

/// One (group words, part, count) arrival; returns the groups kept (group words -> summed count) and `OwnerTop.groups`.
fn reduce(g: usize, floor: i64, arrivals: &[(Vec<u64>, u64, i64)]) -> (BTreeMap<Vec<u64>, i64>, u64) {
    let mut keys = vec![KeyKind::I64; g];
    keys.push(KeyKind::I64);
    // past eight keys a record is wide (breaker.rs `MAX_RECORD_KEYS`)
    let layout = if keys.len() <= MAX_RECORD_KEYS {
        RecordLayout::new(&keys, &[SlotKind::Count])
    } else {
        let off: &'static mut [u32] = Box::leak(Box::new([0u32; 1]));
        let w: &'static WideLayout = Box::leak(Box::new(WideLayout::compute(keys.leak(), &[SlotKind::Count], off, false)));
        RecordLayout::wide(w, false, false)
    }
    .partitioned_by(g as u8);
    let cap = 1u32 << 14;
    let limit = HashAgg::bytes(&layout, cap);
    let mut tbuf = vec![0u64; limit.div_ceil(8) as usize];
    let t = unsafe { HashAgg::at(tbuf.as_mut_ptr() as *mut u8, &layout, cap, limit) };
    for (words, part, n) in arrivals {
        let mut k = words.clone();
        k.push(*part);
        t.find_or_insert(&k).expect("room").add_count(0, *n);
    }
    let spec = PartHavingSpec { layout, mins: [(REP_NONE, 0); MAX_REPS], count_slot: 0, bits: 0, floor };
    let fns = owner_fns(&spec);
    let bytes = (fns.min_bytes)(fns.params.as_ptr(), 1) as usize + 2 * limit as usize;
    let mut obuf = vec![0u64; bytes.div_ceil(8)];
    let owner = obuf.as_mut_ptr() as *mut u8;
    unsafe {
        assert!((fns.open)(fns.params.as_ptr(), owner, bytes) > 0, "the owner opens");
        assert_eq!((fns.reduce)(fns.params.as_ptr(), owner, t as *const HashAgg, core::ptr::null_mut(), core::ptr::null(), -1), 0, "the reduce");
    }
    let h = unsafe { &*(owner as *const OwnerTop) };
    let rec = layout.size() as usize;
    let mut kept = BTreeMap::new();
    for run in 0..h.worst as usize {
        let (off, m) = unsafe { (*(owner.add(OWNERTOP_HDR) as *const u32).add(2 * run), *(owner.add(OWNERTOP_HDR) as *const u32).add(2 * run + 1)) };
        for i in 0..m as usize {
            let r = unsafe { Rec::at(owner.add(off as usize + i * rec), &layout) };
            assert_eq!(r.key(g), 0, "the part word zeroed");
            assert!(kept.insert(r.keys()[..g].to_vec(), r.count(0)).is_none(), "a group kept once");
        }
    }
    (kept, h.groups)
}

fn want(floor: i64, arrivals: &[(Vec<u64>, u64, i64)]) -> (BTreeMap<Vec<u64>, i64>, u64) {
    let mut sums: BTreeMap<Vec<u64>, i64> = BTreeMap::new();
    for (w, _, n) in arrivals {
        *sums.entry(w.clone()).or_default() += n;
    }
    let groups = sums.len() as u64;
    (sums.into_iter().filter(|&(_, s)| s >= floor).collect(), groups)
}

fn check(g: usize, floor: i64, arrivals: &[(Vec<u64>, u64, i64)], pruned: bool) {
    let (got, groups) = reduce(g, floor, arrivals);
    let (w, all) = want(floor, arrivals);
    assert_eq!(got, w, "g {g} floor {floor}: the groups kept and their counts");
    assert_eq!(groups, if pruned { GROUPS_UNCOUNTED } else { all }, "g {g} floor {floor}: the memo's count");
}

/// Parts 0..4 (P = 4) and F = 100: the bound is 25.
fn edges(g: usize) -> Vec<(Vec<u64>, u64, i64)> {
    let key = |i: u64| (0..g as u64).map(|k| i.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ k).collect::<Vec<u64>>();
    let mut a = Vec::new();
    let mut add = |i: u64, parts: &[i64]| {
        for (p, &n) in parts.iter().enumerate() {
            if n > 0 {
                a.push((key(i), p as u64, n));
            }
        }
    };
    add(1, &[25, 25, 25, 24]); // 99
    add(2, &[25, 25, 25, 25]); // 100
    add(3, &[26, 25, 25, 25]); // 101
    add(4, &[24, 24, 24, 28]); // 100, one part over the bound
    add(5, &[24, 24, 24, 24]); // 96, every part under
    add(6, &[0, 0, 0, 101]); // one part
    add(7, &[99, 0, 0, 0]); // one part, one short
    for i in 100..3000u64 {
        add(i, &[(i % 7) as i64 + 1, (i % 5) as i64, 0, (i % 11) as i64]); // many small groups the prune drops
    }
    // a (group, part) pair arriving twice: 12 + 13 reach the bound only merged; group 9's four merged parts sum to 100
    for (p, n) in [(0u64, 12i64), (0, 13), (1, 25), (2, 25), (3, 20), (3, 5)] {
        a.push((key(9), p, n));
    }
    // group 10: every arrival under the bound, merged pairs still under it: 96
    for (p, n) in [(0u64, 12i64), (0, 12), (1, 24), (2, 24), (3, 24)] {
        a.push((key(10), p, n));
    }
    a
}

#[test]
fn the_floor_prune_keeps_exactly_the_groups_at_the_floor() {
    for g in [1usize, 2, 3] {
        let a = edges(g);
        // K = 99 under `>` and K = 100 under `>=` are both F = 100; F - 1 and F + 1 around them
        for floor in [99i64, 100, 101] {
            check(g, floor, &a, true);
        }
        // F <= P: a bound of 1, no prune, every group counted
        check(g, 4, &a, false);
        check(g, 2, &a, false);
    }
}

#[test]
fn any_number_of_groups_at_the_bound_is_pruned() {
    // 300 and 2000 groups (past the old 256-group stack set) each with a part at the bound of F = 100 over P = 4, half
    // of them at the floor, plus as many groups under the bound that the prune drops
    for n in [300u64, 2000] {
        let mut a = Vec::new();
        for i in 0..n {
            let rest = if i % 2 == 0 { 25 } else { 24 };
            for (p, c) in [(0u64, 25i64), (1, rest), (2, 25), (3, 25)] {
                a.push((vec![i * 7919 + 1], p, c));
            }
            a.push((vec![i * 7919 + 2], i % 4, 24));
        }
        check(1, 100, &a, true);
    }
}

#[test]
fn a_wide_group_key_is_pruned() {
    // nine and twelve group words (past the old eight-word set), the leading word the same for every group
    for g in [9usize, 12] {
        let a: Vec<_> = edges(g).into_iter().map(|(mut w, p, n)| {
            w[0] = 7;
            (w, p, n)
        }).collect();
        check(g, 100, &a, true);
    }
}

#[test]
fn a_sparse_part_word_bounds_the_parts_from_above() {
    // parts 0 and 1000 alone: P = 1001, the bound ceil(100 / 1001) = 1 prunes nothing and loses nothing
    let a = vec![(vec![5u64], 0u64, 60i64), (vec![5], 1000, 40), (vec![6], 1000, 99)];
    check(1, 100, &a, false);
    // parts 0 and 3, F = 100: P = 4, the bound 25; group 7's 50 + 50 kept, group 8's 24 + 24 dropped
    let a = vec![(vec![7u64], 0u64, 50i64), (vec![7], 3, 50), (vec![8], 0, 24), (vec![8], 3, 24)];
    check(1, 100, &a, true);
}

#[test]
fn a_table_with_no_record_at_the_bound_keeps_nothing() {
    // the partitioned table of a high floor (a HAVING count(*) > 100000 over 511 parts: F = 100 001, so a bound of 196): every
    // record under the bound, so no group reaches the floor and the table empties at once; then the same with one record
    // at the bound, which keeps its group whole, and a key of nine words
    for g in [1usize, 3, 9] {
        let key = |i: u64| (0..g as u64).map(|k| i.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ k).collect::<Vec<u64>>();
        let mut a: Vec<_> = (0..2000u64).map(|i| (key(i % 700), i % 511, 195i64)).collect();
        a.push((key(5000), 510, 1));
        check(g, 100_001, &a, true);
        a.push((key(6000), 0, 196));
        a.extend((1..511u64).map(|p| (key(6000), p, 196i64)));
        check(g, 100_001, &a, true);
    }
}
