//! The one-round distinct fold's count top under an order word (`OwnerDesc::ordered_by` over the accumulator, carried by
//! `distinct_fold_fns_desc`): a grouped count(DISTINCT) top ordered first by a rider's sum either way, or by the pair
//! count ascending, keeps each owner's n best groups by the emit's own order of that slot, ties by the group key, against
//! a `BTreeMap` oracle at several partition counts and bounds. The default (word 0, the pair count descending) is unchanged.

use std::collections::{BTreeMap, BTreeSet};

use gravity_abi::breaker::{HashAgg, KeyKind, OwnerFns, OwnerTop, Rec, RecordLayout, SlotKind, OWNED_HDR, OWNERTOP_HDR};
use gravity_hosts::ops::group::distinct_fold::{distinct_fold_fns_desc, partition_hash};
use gravity_kernels::dict::reps::{MAX_REPS, REP_NONE};
use gravity_kernels::host_abi::{SK_DESC, SK_SLOT};
use gravity_program::{OwnerDesc, ACC_PAIRS, ACC_SLOT_BASE};

/// The pairs `[group, x] -> [y]` folded partition by partition into one owner region; its kept `(group, pairs, sum)`.
fn kept(d: &OwnerDesc, fns: &OwnerFns, pairs: &[(u64, u64, i64)], pbits: u8) -> Vec<(u64, i64, i64)> {
    let layout = d.layout;
    let bytes = 512usize << 10;
    let mut buf = vec![0u64; (OWNED_HDR + bytes) / 8];
    let region = unsafe { (buf.as_mut_ptr() as *mut u8).add(OWNED_HDR) };
    let p = fns.params.as_ptr();
    assert!(unsafe { (fns.open)(p, region, bytes) } >= 0, "the owner region opens");
    for part in 0..1u32 << pbits {
        let recs: Vec<_> = pairs.iter().filter(|r| partition_hash(&layout, &[r.0, r.1]).part(pbits as u32) == part).collect();
        if recs.is_empty() {
            continue;
        }
        let cap = (recs.len() * 2).max(64).next_power_of_two() as u32;
        let mut tb = vec![0u64; HashAgg::bytes(&layout, cap) as usize / 8];
        let t = unsafe { HashAgg::at(tb.as_mut_ptr() as *mut u8, &layout, cap, HashAgg::bytes(&layout, cap)) };
        for r in recs {
            t.find_or_insert(&[r.0, r.1]).expect("room").add_i64(0, r.2);
        }
        assert_eq!(unsafe { (fns.reduce)(p, region, t as *const HashAgg, core::ptr::null_mut(), core::ptr::null(), -1) }, 0, "partition {part}");
    }
    let h = unsafe { OwnerTop::view(region as *const u8) };
    let acc = d.acc;
    (0..h.len as usize)
        .map(|i| {
            let r = unsafe { Rec::at(region.add(OWNERTOP_HDR + i * acc.size() as usize), &acc) };
            (r.key(0), r.count(ACC_PAIRS as usize), r.i64(ACC_SLOT_BASE as usize))
        })
        .collect()
}

#[test]
fn an_order_word_keeps_the_n_best_groups_by_its_slot() {
    let pair = RecordLayout::new_nn(&[KeyKind::I64, KeyKind::I64], &[SlotKind::SumI64]).partitioned_by(1);
    let acc = RecordLayout::new_nn(&[KeyKind::I64], &[SlotKind::Count, SlotKind::Count, SlotKind::SumI64]);
    // 400 groups; the sums and the distinct counts tie across many groups
    let pairs: Vec<(u64, u64, i64)> = (0..30_000u64).map(|r| (r * 7919 % 400, r * 104_729 % 53, (r % 13) as i64 - 6)).collect();
    let mut groups: BTreeMap<u64, (BTreeSet<u64>, i64)> = BTreeMap::new();
    for &(g, x, y) in &pairs {
        let e = groups.entry(g).or_default();
        e.0.insert(x);
        e.1 += y;
    }
    let all: Vec<(u64, i64, i64)> = groups.iter().map(|(g, (xs, s))| (*g, xs.len() as i64, *s)).collect();
    let sum_slot = ACC_SLOT_BASE as u64;
    let orders: [(&str, u16, fn(&(u64, i64, i64), &(u64, i64, i64)) -> core::cmp::Ordering); 4] = [
        ("sum desc", (SK_SLOT | SK_DESC | sum_slot) as u16, |a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0))),
        ("sum asc", (SK_SLOT | sum_slot) as u16, |a, b| a.2.cmp(&b.2).then(a.0.cmp(&b.0))),
        ("pairs asc", (SK_SLOT | ACC_PAIRS as u64) as u16, |a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0))),
        ("pairs desc, word 0", 0, |a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0))),
    ];
    for (name, word, cmp) in orders {
        let mut want = all.clone();
        want.sort_by(cmp);
        for n in [1u32, 9, 120] {
            let slot = if word == 0 { ACC_PAIRS } else { word as u8 };
            let d = OwnerDesc::distinct(pair, acc, 1, 400, slot, n, [(REP_NONE, 0); MAX_REPS]);
            let d = if word == 0 { d } else { d.ordered_by(word) };
            let fns = distinct_fold_fns_desc(&d, 5);
            assert_eq!(fns.rounds, 1, "the one-round fold");
            for pbits in [3u8, 6] {
                let mut got = kept(&d, &fns, &pairs, pbits);
                got.sort_by(cmp);
                assert_eq!(got, want[..n as usize], "{name} n {n} pbits {pbits}");
            }
        }
    }
}
