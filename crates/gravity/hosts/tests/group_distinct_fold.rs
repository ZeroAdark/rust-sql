//! The distinct fold face on a hand-driven owner region (lower.md §15.9): partitions of deduped pair records
//! folded by the driver's protocol (`open`, `reduce` per partition, `end_round`) against a `BTreeMap` oracle —
//! the plain form over several owners, the grouped count-ordered form in both accumulator forms, the run form,
//! slots with validity, a text group key's representative across parts, an empty input, and the region split
//! that keeps the accumulator table as large as the pair table at every region size.

use std::collections::{BTreeMap, BTreeSet};

use gravity_abi::breaker::{HashAgg, KeyKind, Owned, OwnerFns, OwnerTop, Rec, RecordLayout, SlotKind, MINCODE_NONE, OWNED_HDR, OWNERTOP_HDR, TABLE_OWNED};
use gravity_hosts::ops::group::distinct_fold::{accumulator, distinct_fold_fns, distinct_fold_fns_desc, partition_hash, plain_total, DistinctFold, FORM_PAIRS, FORM_SENTINEL};
use gravity_kernels::dict::reps::{MAX_REPS, REP_NONE};
use gravity_program::{ACC_PAIRS, ACC_SLOT_BASE};

/// A pair record as the phase-1 loop writes it: the key words, the slot values (`None`: NULL) in slot order.
#[derive(Clone, Debug)]
struct Pair {
    keys: Vec<u64>,
    slots: Vec<Option<i64>>,
}

/// The owner phase over one owner: the pairs split into `1 << pbits` partitions by the layout's partition hash,
/// each folded into a pair table and reduced; the region (behind an `Owned` header) is returned.
fn own(f: &DistinctFold, fns: &OwnerFns, pairs: &[Pair], pbits: u8, bytes: usize) -> Vec<u64> {
    let layout = f.pair;
    let mut buf = vec![0u64; (OWNED_HDR + bytes) / 8];
    let region = unsafe { (buf.as_mut_ptr() as *mut u8).add(OWNED_HDR) };
    let p = fns.params.as_ptr();
    assert!(bytes as u64 >= (fns.min_bytes)(p, 1), "the region holds the face's floor");
    assert!(unsafe { (fns.open)(p, region, bytes) } >= 0, "the owner region opens");
    for part in 0..1u32 << pbits {
        let recs: Vec<&Pair> = pairs.iter().filter(|r| (partition_hash(&layout, &r.keys)).part(pbits as u32) == part).collect();
        if recs.is_empty() {
            continue;
        }
        let cap = (recs.len() * 2).max(64).next_power_of_two() as u32;
        let mut tb = vec![0u64; HashAgg::bytes(&layout, cap) as usize / 8];
        let t = unsafe { HashAgg::at(tb.as_mut_ptr() as *mut u8, &layout, cap, HashAgg::bytes(&layout, cap)) };
        for r in recs {
            let rec = t.find_or_insert(&r.keys).expect("room");
            for (j, s) in r.slots.iter().enumerate() {
                let Some(v) = *s else { continue };
                match layout.slots[j] {
                    SlotKind::Count => rec.add_count(j, v),
                    SlotKind::SumI64 => rec.add_i64(j, v),
                    SlotKind::MinCode => rec.min_code(j, (v as u64 >> 32) as u32, v as u32),
                    k => panic!("a slot kind the fixture does not write: {k:?}"),
                }
            }
        }
        assert_eq!(unsafe { (fns.reduce)(p, region, t as *const HashAgg, core::ptr::null_mut(), core::ptr::null(), -1) }, 0, "partition {part}");
    }
    assert_eq!(unsafe { (fns.end_round)(p, region) }, 0);
    unsafe { *(buf.as_mut_ptr() as *mut Owned) = Owned { nowners: 1, owner_bytes: bytes as u32, limit: (OWNED_HDR + bytes) as u32, pad0: [0; 3], kind: TABLE_OWNED, base: 0, pad1: [0; 5], cell: core::cell::UnsafeCell::new([]) } };
    buf
}

fn top(region: &[u64]) -> &OwnerTop {
    unsafe { OwnerTop::view((region.as_ptr() as *const u8).add(OWNED_HDR)) }
}

/// The count form's kept records `(keys, pairs, the slots after the pair count)` in the order kept.
fn kept(region: &[u64], acc: &RecordLayout, pairs_slot: usize) -> Vec<(Vec<u64>, i64, Vec<Option<i64>>)> {
    let h = top(region);
    let base = unsafe { (region.as_ptr() as *const u8).add(OWNED_HDR + OWNERTOP_HDR) };
    (0..h.len as usize).map(|i| read(unsafe { Rec::at(base.add(i * acc.size() as usize) as *mut u8, acc) }, pairs_slot)).collect()
}

fn read(r: Rec<'_>, pairs_slot: usize) -> (Vec<u64>, i64, Vec<Option<i64>>) {
    let slots = (pairs_slot + 1..r.layout.nslots()).map(|j| r.valid(j).then(|| match r.layout.slots[j] { SlotKind::Count => r.count(j), SlotKind::SumI64 => r.i64(j), SlotKind::MinCode => { let (p, c) = r.code(j); (p as i64) << 32 | c as i64 } _ => unreachable!() })).collect();
    (r.keys().to_vec(), r.count(pairs_slot), slots)
}

/// The run form's records over the directory.
fn runs(region: &[u64], acc: &RecordLayout, pairs_slot: usize) -> Vec<(Vec<u64>, i64, Vec<Option<i64>>)> {
    let h = top(region);
    let base = unsafe { (region.as_ptr() as *const u8).add(OWNED_HDR) };
    let dir = unsafe { base.add(OWNERTOP_HDR) as *const u32 };
    let mut out = Vec::new();
    for run in 0..h.worst as usize {
        let (off, n) = unsafe { (*dir.add(2 * run) as usize, *dir.add(2 * run + 1) as usize) };
        for i in 0..n {
            out.push(read(unsafe { Rec::at(base.add(off + i * acc.size() as usize) as *mut u8, acc) }, pairs_slot));
        }
    }
    assert_eq!(out.len(), h.len as usize);
    out
}

/// The oracle: per group the distinct arguments, and per group the slots summed over its distinct pairs (a
/// pair's slot counted once, as the phase-1 fold dedups the pair).
fn oracle(pairs: &[Pair], nk: usize) -> BTreeMap<Vec<u64>, (i64, Vec<Option<i64>>)> {
    let mut seen: BTreeSet<Vec<u64>> = BTreeSet::new();
    let mut m: BTreeMap<Vec<u64>, (i64, Vec<Option<i64>>)> = BTreeMap::new();
    let ns = pairs.first().map_or(0, |p| p.slots.len());
    for p in pairs {
        let e = m.entry(p.keys[..nk].to_vec()).or_insert_with(|| (0, vec![None; ns]));
        let fresh = seen.insert(p.keys.clone());
        if fresh {
            e.0 += 1;
        }
        for (j, s) in p.slots.iter().enumerate() {
            if let Some(v) = *s {
                e.1[j] = Some(e.1[j].map_or(v, |a| a + v));
            }
        }
    }
    m
}

fn gen(n: u64, groups: u64, dom: u64) -> Vec<Pair> {
    (0..n).map(|r| Pair { keys: vec![r * 7919 % groups, r * 104729 % dom], slots: vec![] }).collect()
}

#[test]
fn plain_form_counts_the_partitions_and_the_leader_sums_the_owners() {
    let layout = RecordLayout::new_nn(&[KeyKind::I64], &[]);
    let f = DistinctFold { pair: layout, words: 1, form: FORM_PAIRS, topk: 0, reps: [(REP_NONE, 0); MAX_REPS] };
    let fns = distinct_fold_fns(&f, 4);
    assert_eq!(fns.rounds, 1);
    let pairs: Vec<Pair> = (0..5000u64).map(|r| Pair { keys: vec![r * 31 % 977], slots: vec![] }).collect();
    let want = pairs.iter().map(|p| p.keys[0]).collect::<BTreeSet<_>>().len() as u64;
    // one owner over every partition, then the partitions split between two owners: the totals add up
    let one = own(&f, &fns, &pairs, 4, 64 << 10);
    assert_eq!(unsafe { plain_total(one.as_ptr() as *const u8) }, want);
    let (a, b): (Vec<Pair>, Vec<Pair>) = pairs.iter().cloned().partition(|p| (partition_hash(&layout, &p.keys)).part(4 as u32) % 2 == 0);
    let (ra, rb) = (own(&f, &fns, &a, 4, 64 << 10), own(&f, &fns, &b, 4, 64 << 10));
    assert_eq!(top(&ra).total + top(&rb).total, want);
    assert_eq!(top(&ra).len, 0, "the plain form keeps no records");
    let empty = own(&f, &fns, &[], 4, 64 << 10);
    assert_eq!(unsafe { plain_total(empty.as_ptr() as *const u8) }, 0);
    // the identity table (kernel-floors.md §2.1): the fingerprint pair alone, the all-zero key among the keys, several region sizes
    let fp = RecordLayout::new_nn(&[KeyKind::I64, KeyKind::I64], &[]);
    let f2 = DistinctFold { pair: fp, words: 2, form: FORM_SENTINEL, topk: 0, reps: [(REP_NONE, 0); MAX_REPS] };
    let fns2 = distinct_fold_fns(&f2, 4);
    let pairs: Vec<Pair> = (0..20_000u64).map(|r| Pair { keys: vec![r * 31 % 97, r * 131 % 89], slots: vec![] }).collect();
    let want = pairs.iter().map(|p| (p.keys[0], p.keys[1])).collect::<BTreeSet<_>>().len() as u64;
    assert!(pairs.iter().any(|p| p.keys == [0, 0]));
    for bytes in [16usize << 10, 64 << 10, 1 << 20] {
        let r = own(&f2, &fns2, &pairs, 4, bytes);
        assert_eq!(unsafe { plain_total(r.as_ptr() as *const u8) }, want, "region {bytes}");
    }
}

#[test]
fn grouped_count_form_keeps_the_n_best_groups_in_both_accumulator_forms() {
    let layout = RecordLayout::new_nn(&[KeyKind::I64, KeyKind::I64], &[]).partitioned_by(1);
    let pairs = gen(20_000, 300, 4_000);
    let o = oracle(&pairs, 1);
    let mut want: Vec<(Vec<u64>, i64)> = o.iter().map(|(k, v)| (k.clone(), v.0)).collect();
    want.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    for form in [FORM_PAIRS, FORM_SENTINEL] {
        for n in [1u32, 7, 64] {
            let f = DistinctFold { pair: layout, words: 1, form, topk: n, reps: [(REP_NONE, 0); MAX_REPS] };
            let fns = distinct_fold_fns(&f, 6);
            let acc = f.accumulator();
            assert_eq!(acc.nkeys(), 1);
            assert_eq!(acc.slots[f.pairs_slot()], SlotKind::Count);
            if form == FORM_SENTINEL {
                assert_eq!((f.pairs_slot(), f.slot_base()), (ACC_PAIRS as usize, ACC_SLOT_BASE as usize));
            }
            for pbits in [3u8, 7] {
                let region = own(&f, &fns, &pairs, pbits, 256 << 10);
                let mut got: Vec<(Vec<u64>, i64)> = kept(&region, &acc, f.pairs_slot()).into_iter().map(|(k, c, _)| (k, c)).collect();
                got.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
                assert_eq!(got, want[..n as usize], "form {form} n {n} pbits {pbits}");
                if form == FORM_SENTINEL {
                    let base = unsafe { (region.as_ptr() as *const u8).add(OWNED_HDR + OWNERTOP_HDR) };
                    for i in 0..n as usize {
                        assert_eq!(unsafe { Rec::at(base.add(i * acc.size() as usize) as *mut u8, &acc) }.count(0), 1, "the sentinel");
                    }
                }
                assert_eq!(top(&region).records, pairs.iter().map(|p| p.keys.clone()).collect::<BTreeSet<_>>().len() as u64);
            }
        }
    }
}

#[test]
fn grouped_run_form_writes_every_group_once_with_its_slots_and_validity() {
    let layout = RecordLayout::new(&[KeyKind::I64, KeyKind::I64], &[SlotKind::Count, SlotKind::SumI64]).partitioned_by(1);
    let pairs: Vec<Pair> = (0..6000u64).map(|r| Pair { keys: vec![r % 41, r * 7 % 500], slots: vec![Some(1), (r % 3 != 0).then_some(r as i64 % 17)] }).collect();
    let o = oracle(&pairs, 1);
    for form in [FORM_PAIRS, FORM_SENTINEL] {
        let f = DistinctFold { pair: layout, words: 1, form, topk: 0, reps: [(REP_NONE, 0); MAX_REPS] };
        let fns = distinct_fold_fns(&f, 5);
        let acc = f.accumulator();
        assert!(acc.has_validity());
        assert_eq!(&acc.slot_kinds()[f.slot_base()..], &[SlotKind::Count, SlotKind::SumI64]);
        let region = own(&f, &fns, &pairs, 5, 512 << 10);
        let got = runs(&region, &acc, f.pairs_slot());
        assert_eq!(got.len(), o.len(), "form {form}: one record per group");
        let mut seen = BTreeSet::new();
        for (k, c, slots) in got {
            assert!(seen.insert(k.clone()), "a group twice");
            let (want_c, want_s) = &o[&k];
            assert_eq!(c, *want_c, "form {form} group {k:?}");
            assert_eq!(&slots, want_s, "form {form} group {k:?}");
        }
    }
}

#[test]
fn text_group_key_keeps_the_least_representative_across_parts() {
    // (fp lo, fp hi, user) with the phrase's representative `part << 32 | code`, no validity word (MinCode by its word)
    let layout = RecordLayout::new_nn(&[KeyKind::Fp128, KeyKind::Fp128, KeyKind::I64], &[SlotKind::MinCode]).partitioned_by(2);
    let pairs: Vec<Pair> = (0..3000u64).map(|r| {
        let g = r % 23;
        let part = r % 5;
        Pair { keys: vec![g.wrapping_mul(0x9E37_79B9_7F4A_7C15), !g, r * 13 % 200], slots: vec![Some(((part as i64) << 32) | (g as i64 + 100 * part as i64))] }
    }).collect();
    let mut want: BTreeMap<Vec<u64>, (i64, i64)> = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for p in &pairs {
        let e = want.entry(p.keys[..2].to_vec()).or_insert((0, i64::MAX));
        if seen.insert(p.keys.clone()) {
            e.0 += 1;
        }
        e.1 = e.1.min(p.slots[0].unwrap());
    }
    // no representative declared: a count tie orders by the fingerprint words, no dictionary is rendered (the rig
    // has no site; the rendered contract of `FORM_PAIRS` is the lowered program's against the kernel's emit)
    for form in [FORM_PAIRS, FORM_SENTINEL] {
        let f = DistinctFold { pair: layout, words: 1, form, topk: 23, reps: [(REP_NONE, 0); MAX_REPS] };
        let fns = distinct_fold_fns(&f, 4);
        let acc = f.accumulator();
        assert_eq!(&acc.slot_kinds()[f.pairs_slot()..], &[SlotKind::Count, SlotKind::MinCode]);
        assert_eq!(acc, accumulator(&layout, 1, form));
        let region = own(&f, &fns, &pairs, 4, 256 << 10);
        let got = kept(&region, &acc, f.pairs_slot());
        assert_eq!(got.len(), 23);
        for (k, c, slots) in got {
            let (wc, wrep) = want[&k];
            assert_eq!((c, slots[0]), (wc, Some(wrep)), "form {form} group {k:?}");
            assert_ne!(slots[0].unwrap() as u64, MINCODE_NONE);
        }
    }
}

#[test]
fn every_region_size_holds_a_pair_table_at_its_capacity() {
    // a pair table filled to its 7/8 load with one group per pair (the worst case for the accumulator table)
    for (nk, slots) in [(2usize, &[][..]), (3, &[SlotKind::Count, SlotKind::SumI64][..]), (2, &[SlotKind::Count][..])] {
        let keys: Vec<KeyKind> = vec![KeyKind::I64; nk];
        let layout = RecordLayout::new(&keys, slots).partitioned_by(nk as u8 - 1);
        for form in [FORM_PAIRS, FORM_SENTINEL] {
            let f = DistinctFold { pair: layout, words: 1, form, topk: 1, reps: [(REP_NONE, 0); MAX_REPS] };
            let fns = distinct_fold_fns(&f, 4);
            let floor = (fns.min_bytes)(fns.params.as_ptr(), 1) as usize;
            for bytes in [floor, floor + 8, floor + 1000, 40 << 10, 100 << 10, 1 << 20] {
                let bytes = bytes & !7;
                let mut buf = vec![0u64; bytes / 8];
                let region = buf.as_mut_ptr() as *mut u8;
                let table_off = unsafe { (fns.open)(fns.params.as_ptr(), region, bytes) };
                assert!(table_off > 0, "nk {nk} form {form} bytes {bytes}");
                let space = bytes - table_off as usize;
                let cap = HashAgg::cap_of(&layout, space as u32);
                let t = unsafe { HashAgg::at(region.add(table_off as usize), &layout, cap, space as u32) };
                let mut i = 0u64;
                while t.len < cap - cap / 8 {
                    let mut k = vec![i; nk];
                    k[nk - 1] = i * 3;
                    let r = t.find_or_insert(&k).expect("under the load");
                    for j in 0..slots.len() {
                        match slots[j] {
                            SlotKind::Count => r.add_count(j, 1),
                            _ => r.add_i64(j, 2),
                        }
                    }
                    i += 1;
                }
                let n = t.len;
                assert_eq!(unsafe { (fns.reduce)(fns.params.as_ptr(), region, t as *const HashAgg, core::ptr::null_mut(), core::ptr::null(), -1) }, 0, "nk {nk} form {form} bytes {bytes} cap {cap}");
                let h = unsafe { OwnerTop::view(region) };
                assert_eq!(h.groups, n as u64, "one group per pair");
                assert_eq!(h.len, 1, "the best group kept");
            }
        }
    }
}

/// M5 §4.5 (`distinct_fold_fns_desc`): a NULL argument rides the pair as its flag word — the flagged pair folds its riders and adds
/// nothing to the pair count (the plain form counts it out of the total) — and `sum(DISTINCT x)` folds the argument word into the
/// descriptor's widened accumulator once per new pair, in both the run and the count-ordered forms.
#[test]
fn a_null_pair_counts_nothing_and_the_argument_folds_once_per_pair() {
    use gravity_abi::breaker::ORDER_NONE;
    use gravity_program::OwnerDesc;
    // the pair: [group, x, flag] → [rows]; the accumulator: [group] → [sentinel, pairs, rows, sum(DISTINCT x)]
    let pair = RecordLayout::new_nn(&[KeyKind::I64, KeyKind::I64, KeyKind::I64], &[SlotKind::Count]).partitioned_by(1);
    let acc = RecordLayout::new_nn(&[KeyKind::I64], &[SlotKind::Count, SlotKind::Count, SlotKind::Count, SlotKind::SumI64]);
    let pairs: Vec<Pair> = (0..9000u64).map(|r| { let (g, x) = (r % 37, r * 7 % 211); if x % 5 == 0 { Pair { keys: vec![g, 0, 1 << 1], slots: vec![Some(1)] } } else { Pair { keys: vec![g, x, 0], slots: vec![Some(1)] } } }).collect();
    let mut want: BTreeMap<u64, (i64, i64, i64)> = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for p in pairs.iter() {
        let e = want.entry(p.keys[0]).or_default();
        e.1 += 1;
        if p.keys[2] == 0 && seen.insert((p.keys[0], p.keys[1])) {
            e.0 += 1;
            e.2 += p.keys[1] as i64;
        }
    }
    for (topk, pbits) in [(0u32, 4u8), (5, 5)] {
        let d = if topk > 0 { OwnerDesc::distinct(pair, acc, 2, 37, ACC_PAIRS, topk, [(REP_NONE, 0); MAX_REPS]) } else { OwnerDesc::distinct(pair, acc, 2, 37, ORDER_NONE, 0, [(REP_NONE, 0); MAX_REPS]) }.null_arg(2, 1).arg_fold(3);
        let f = DistinctFold::of_desc(&d);
        let fns = distinct_fold_fns_desc(&d, pbits);
        let region = own(&f, &fns, &pairs, pbits, 512 << 10);
        let got = if topk > 0 { kept(&region, &acc, ACC_PAIRS as usize) } else { runs(&region, &acc, ACC_PAIRS as usize) };
        let mut w: Vec<(u64, (i64, i64, i64))> = want.iter().map(|(k, v)| (*k, *v)).collect();
        if topk > 0 {
            w.sort_by(|a, b| b.1.0.cmp(&a.1.0).then(a.0.cmp(&b.0)));
            w.truncate(topk as usize);
        }
        assert_eq!(got.len(), w.len(), "topk {topk}");
        for (k, c, slots) in got {
            let (pairs, rows, sum) = want[&k[0]];
            assert_eq!((c, slots), (pairs, vec![Some(rows), Some(sum)]), "topk {topk} group {}", k[0]);
        }
    }
    // the plain form: [x, flag] alone, the NULL pair counted out of the total
    let plain = RecordLayout::new_nn(&[KeyKind::I64, KeyKind::I64], &[]);
    let pacc = RecordLayout::new_nn(&[], &[SlotKind::Count, SlotKind::Count]);
    let d = OwnerDesc::distinct(plain, pacc, 2, 0, ORDER_NONE, 0, [(REP_NONE, 0); MAX_REPS]).null_arg(1, 0);
    let pp: Vec<Pair> = (0..4000u64).map(|r| { let x = r * 13 % 409; if x % 7 == 0 { Pair { keys: vec![0, 1], slots: vec![] } } else { Pair { keys: vec![x, 0], slots: vec![] } } }).collect();
    let want = pp.iter().filter(|p| p.keys[1] == 0).map(|p| p.keys[0]).collect::<BTreeSet<_>>().len() as u64;
    let f = DistinctFold::of_desc(&d);
    let fns = distinct_fold_fns_desc(&d, 4);
    let region = own(&f, &fns, &pp, 4, 64 << 10);
    assert_eq!(unsafe { plain_total(region.as_ptr() as *const u8) }, want, "the NULL pair is not a distinct value");
    let d0 = OwnerDesc::distinct(plain, pacc, 2, 0, ORDER_NONE, 0, [(REP_NONE, 0); MAX_REPS]);
    let region0 = own(&DistinctFold::of_desc(&d0), &distinct_fold_fns_desc(&d0, 4), &pp, 4, 64 << 10);
    assert_eq!(unsafe { plain_total(region0.as_ptr() as *const u8) }, want + 1, "without the flag the NULL pair is a value");
}
