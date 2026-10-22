//! V4R-192: `admitted` over pass 1's export (kernels twopass_top: each owner's keys summing to two or more and its `n`
//! best singles) admits exactly what it admits over every pass-1 record (the generic runs form, a key's parts spread
//! over the owners as the full-image route spreads them): the >= 2 rule, the top-n singles' cut with a tie order around
//! it (ASC / DESC, NULLS FIRST / LAST, a nullable key's NULL among the singles), one owner (W0) and eight. Declared by registry/m1.rs beside keyed_testkit (tests only).
#![allow(unsafe_code)]
use std::collections::BTreeSet;

use gravity_abi::breaker::{HashAgg, KeyKind, Owned, OwnerTop, RecordLayout, SlotKind, OWNED_HDR, OWNERTOP_HDR, OWNER_RUN_OFF, TABLE_OWNED};
use gravity_abi::hash::KeyHash;
use gravity_kernels::ops::sort::topn::twopass_top::{owner_fns, TopExportSpec};

use super::admitted;
use crate::ops::shared::twopass::Rule;

struct Case {
    img: usize,
    nullable: bool,
    /// `(image words, part, count)`, one arrival each (a key arriving twice merges).
    arrivals: Vec<(Vec<u64>, u64, i64)>,
}

fn layout(c: &Case) -> RecordLayout {
    let keys = vec![KeyKind::I64; c.img];
    RecordLayout::with_sentinel(&keys, &[SlotKind::Count], false, Some(0)).partitioned_by(c.img as u8)
}

fn full_keys(w: &[u64]) -> Vec<u64> {
    w.to_vec()
}

/// One table of `recs` (full keys, count) merged.
fn table(l: &RecordLayout, recs: &[(Vec<u64>, i64)]) -> (Vec<u64>, *mut HashAgg) {
    let cap = (2 * recs.len().max(8)).next_power_of_two() as u32;
    let limit = HashAgg::bytes(l, cap);
    let mut buf = vec![0u64; limit.div_ceil(8) as usize];
    let t = unsafe { HashAgg::at(buf.as_mut_ptr() as *mut u8, l, cap, limit) };
    for (k, n) in recs {
        t.find_or_insert(k).expect("room").add_count(0, *n);
    }
    let p = t as *mut HashAgg;
    (buf, p)
}

fn owned(nowners: usize, owner_bytes: usize) -> Vec<u64> {
    let mut v = vec![0u64; (OWNED_HDR + nowners * owner_bytes).div_ceil(8)];
    let o = v.as_mut_ptr() as *mut Owned;
    unsafe { *o = Owned { nowners: nowners as u32, owner_bytes: owner_bytes as u32, limit: 0, pad0: [0; 3], kind: TABLE_OWNED, base: 0, pad1: [0; 5], cell: core::cell::UnsafeCell::new([]) } };
    v
}

/// The admitted records `(full keys, count)` of `region` under `rule`.
fn admit(region: &[u64], rule: &Rule) -> BTreeSet<(Vec<u64>, i64)> {
    let mut got = BTreeSet::new();
    let nk = rule.img_words as usize;
    let words = [rule.pack(None)];
    unsafe {
        admitted(region.as_ptr() as *const u8, &words, None, &mut |r, _| {
            let k = core::slice::from_raw_parts(r, nk).to_vec();
            assert!(got.insert((k, *r.add(rule.count_word as usize) as i64)), "a record admitted once");
        })
        .expect("an owned region");
    }
    got
}

fn check(c: &Case, nowners: usize, n: u32, desc: bool, nulls_first: bool) {
    let l = layout(c);
    let rec = l.size() as usize;
    let rule = Rule { desc, nulls_first, nullable: c.nullable, img_words: c.img as u8, rec_words: (rec / 8) as u8, count_word: (l.sentinel_off() / 8) as u8, n, ..Rule::default() };
    // the merged pass-1 records
    let (buf, t) = table(&l, &c.arrivals.iter().map(|(w, _, n)| (full_keys(w), *n)).collect::<Vec<_>>());
    let t = unsafe { &*t };
    let recs: Vec<(Vec<u64>, i64)> = (0..t.cap as usize).map(|i| t.record(i)).filter(|r| r.used()).map(|r| (r.keys().to_vec(), r.count(0))).collect();
    drop(buf);
    // before: the runs form, each record in the owner its full image routes to
    let obytes = OWNER_RUN_OFF + (recs.len() + 1) * rec;
    let mut old = owned(nowners, obytes);
    let o = unsafe { Owned::view(old.as_mut_ptr() as *const u8) };
    let mut fill = vec![0usize; nowners];
    for (k, cnt) in &recs {
        let j = (KeyHash::of(k).part(8) as usize) % nowners;
        let base = o.owner(j);
        unsafe {
            let d = base.add(OWNER_RUN_OFF + fill[j] * rec);
            let w = d as *mut u64;
            w.copy_from_nonoverlapping(k.as_ptr(), k.len());
            *(d.add(l.sentinel_off() as usize) as *mut i64) = *cnt;
            fill[j] += 1;
            let h = &mut *(base as *mut OwnerTop);
            h.worst = 1;
            let dir = base.add(OWNERTOP_HDR) as *mut u32;
            (*dir, *dir.add(1)) = (OWNER_RUN_OFF as u32, fill[j] as u32);
        }
    }
    // after: the export owners, each over its partitions (the image's route), a partition one reduce
    let spec = TopExportSpec { layout: l, count_slot: 0, img_words: c.img as u8, n, desc, nulls_first, nullable: c.nullable, ext: false, ext_nn: false, spread: false };
    let fns = owner_fns(&spec);
    let ebytes = (fns.min_bytes)(fns.params.as_ptr(), 1) as usize + 2 * (recs.len() + 1) * rec + (1 << 16);
    let mut new = owned(nowners, ebytes);
    let o = unsafe { Owned::view(new.as_mut_ptr() as *const u8) };
    for j in 0..nowners {
        let base = o.owner(j);
        unsafe { assert!((fns.open)(fns.params.as_ptr(), base, ebytes) > 0, "the owner opens") };
        for part in (0..16u32).filter(|p| *p as usize % nowners == j) {
            let mine: Vec<(Vec<u64>, i64)> = recs.iter().filter(|(k, _)| KeyHash::of(&k[..c.img]).part(4) == part).cloned().collect();
            if mine.is_empty() {
                continue;
            }
            let (_tb, t) = table(&l, &mine);
            unsafe { assert_eq!((fns.reduce)(fns.params.as_ptr(), base, t, core::ptr::null_mut(), core::ptr::null(), -1), 0, "the reduce") };
        }
        unsafe { assert_eq!((fns.end_round)(fns.params.as_ptr(), base), 0) };
    }
    let (a, b) = (admit(&old, &rule), admit(&new, &rule));
    assert_eq!(a, b, "img {} nullable {} owners {nowners} n {n} desc {desc} nulls_first {nulls_first}", c.img, c.nullable);
}

/// Keys 1..=400: every 3rd counted twice, the rest once; key 0 (the
/// nullable image's NULL, `(0, 1)`) seen once; values spread so an n-cut lands between singles.
fn case(img: usize, nullable: bool) -> Case {
    let mut a = Vec::new();
    let key = |i: u64| if img == 1 { vec![i.wrapping_mul(7919) % 100_003] } else { vec![i.wrapping_mul(7919) % 100_003, 0] };
    for i in 1..=400u64 {
        match i % 3 {
            0 => a.push((key(i), i % 4, 2)),
            _ => a.push((key(i), i % 4, 1)),
        }
    }
    // a key arriving twice: a dup only merged
    a.push((key(1000), 2, 1));
    a.push((key(1000), 2, 1));
    if nullable {
        a.push((vec![0, 1], 0, 1));
    }
    Case { img, nullable, arrivals: a }
}

#[test]
fn the_export_admits_exactly_what_every_record_admits() {
    for (img, nullable) in [(1usize, false), (2, false), (2, true)] {
        let c = case(img, nullable);
        for nowners in [1usize, 8] {
            for n in [0u32, 1, 3, 10, 150, 1000] {
                for (desc, nulls_first) in [(false, false), (true, false), (false, true), (true, true)] {
                    check(&c, nowners, n, desc, nulls_first);
                }
            }
        }
    }
}

/// An extreme top's key: `(image word, count, extreme (the witness), non-NULL rows, best)` (best: a spread rule's only).
type ExtKey = (u64, i64, i64, i64, i64);

/// Every record of `keys` in the runs form (each in the owner its image routes to), and in the export owners' form.
fn ext_regions(l: &RecordLayout, keys: &[ExtKey], nowners: usize, spec: &TopExportSpec) -> (Vec<u64>, Vec<u64>) {
    let rec = l.size() as usize;
    let put = |d: *mut u8, &(k, c, e, nn, best): &ExtKey| unsafe {
        *(d as *mut u64) = k;
        *(d.add(l.slot_off(0) as usize) as *mut i64) = c;
        *(d.add(l.slot_off(1) as usize) as *mut i64) = e;
        if spec.ext_nn {
            *(d.add(l.slot_off(2) as usize) as *mut i64) = nn;
        }
        if spec.spread {
            *(d.add(l.slot_off(2 + spec.ext_nn as usize) as usize) as *mut i64) = best;
        }
    };
    let obytes = OWNER_RUN_OFF + (keys.len() + 1) * rec;
    let mut old = owned(nowners, obytes);
    let o = unsafe { Owned::view(old.as_mut_ptr() as *const u8) };
    let mut fill = vec![0usize; nowners];
    for key in keys {
        let j = (KeyHash::of(&[key.0]).part(8) as usize) % nowners;
        let base = o.owner(j);
        unsafe {
            put(base.add(OWNER_RUN_OFF + fill[j] * rec), key);
            fill[j] += 1;
            (*(base as *mut OwnerTop)).worst = 1;
            let dir = base.add(OWNERTOP_HDR) as *mut u32;
            (*dir, *dir.add(1)) = (OWNER_RUN_OFF as u32, fill[j] as u32);
        }
    }
    let fns = owner_fns(spec);
    let ebytes = (fns.min_bytes)(fns.params.as_ptr(), 1) as usize + 2 * (keys.len() + 1) * rec + (1 << 16);
    let mut new = owned(nowners, ebytes);
    let o = unsafe { Owned::view(new.as_mut_ptr() as *const u8) };
    for j in 0..nowners {
        let base = o.owner(j);
        unsafe { assert!((fns.open)(fns.params.as_ptr(), base, ebytes) > 0, "the owner opens") };
        for part in (0..16u32).filter(|p| *p as usize % nowners == j) {
            let mine: Vec<&ExtKey> = keys.iter().filter(|k| KeyHash::of(&[k.0]).part(4) == part).collect();
            if mine.is_empty() {
                continue;
            }
            let cap = (2 * mine.len().max(8)).next_power_of_two() as u32;
            let limit = HashAgg::bytes(l, cap);
            let mut buf = vec![0u64; limit.div_ceil(8) as usize];
            let t = unsafe { HashAgg::at(buf.as_mut_ptr() as *mut u8, l, cap, limit) };
            for k in &mine {
                put(t.find_or_insert(&[k.0]).expect("room").as_ptr(), k);
            }
            let t = t as *mut HashAgg;
            unsafe { assert_eq!((fns.reduce)(fns.params.as_ptr(), base, t, core::ptr::null_mut(), core::ptr::null(), -1), 0, "the reduce") };
        }
        unsafe { assert_eq!((fns.end_round)(fns.params.as_ptr(), base), 0) };
    }
    (old, new)
}

/// An extreme top's export (each owner's keys whose best ranks at its own n-th best extreme or before; its heap's records
/// are among them) admits exactly what the extreme rule admits over every pass-1 record: one owner and eight, n past the
/// key count, both orders and NULL placements, keys whose every value is NULL and keys with some NULL rows.
#[test]
fn the_extreme_export_admits_exactly_what_every_record_admits() {
    let keys: Vec<ExtKey> = (1..=400u64)
        .map(|i| {
            let c = 1 + (i % 3) as i64;
            let nn = if i % 11 == 0 { 0 } else if i % 5 == 0 { c - 1 } else { c };
            let e = ((i * 37) % 97) as i64 - 40;
            (i.wrapping_mul(7919) % 100_003, c, e, nn, e)
        })
        .collect();
    for ext_nn in [false, true] {
        let keys: Vec<ExtKey> = keys.iter().map(|&(k, c, e, nn, b)| (k, c, e, if ext_nn { nn } else { c }, b)).collect();
        let slots: &[SlotKind] = if ext_nn { &[SlotKind::Count, SlotKind::MaxI64, SlotKind::Count] } else { &[SlotKind::Count, SlotKind::MaxI64] };
        let l = RecordLayout::with_sentinel(&[KeyKind::I64], slots, true, Some(0)).partitioned_by(1);
        for nowners in [1usize, 8] {
            for n in [0u32, 1, 3, 10, 150, 1000] {
                for (desc, nulls_first) in [(false, false), (true, false), (false, true), (true, true)] {
                    let spec = TopExportSpec { layout: l, count_slot: 0, img_words: 1, n, desc, nulls_first, nullable: false, ext: true, ext_nn, spread: false };
                    let rule = Rule { ext: true, ext_nn, desc, nulls_first, img_words: 1, rec_words: (l.size() / 8) as u8, count_word: (l.sentinel_off() / 8) as u8, n, ..Rule::default() };
                    let (old, new) = ext_regions(&l, &keys, nowners, &spec);
                    let (a, b) = (admit(&old, &rule), admit(&new, &rule));
                    assert_eq!(a, b, "ext_nn {ext_nn} owners {nowners} n {n} desc {desc} nulls_first {nulls_first}");
                    if n == 3 && !(nulls_first && ext_nn) {
                        assert!(a.len() < 40, "n 3 admits few keys: {}", a.len());
                    }
                }
            }
        }
    }
}

/// gtopn GT-1: the spread rule (`Class::two_pass_bounds`: min / max / avg in either direction, pass 1 folding the key's
/// witness and best extremes of the column) admits every key of a group that can rank in the top: over random rows of
/// `(k1, k2, v)` (some v NULL), each group's own aggregate ranked as PostgreSQL orders it, every group ranking with the
/// n-th group or before (ties included) is in an admitted key, under one owner's export and eight, both directions and
/// NULL placements; and the export admits exactly what every record admits.
#[test]
fn a_spread_rule_admits_every_key_of_a_group_that_can_rank_in_the_top() {
    use gravity_decompose::ops::shared::agg_class::{Class, Ext};
    use std::collections::BTreeMap;
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut rnd = |m: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % m
    };
    // ORDER BY agg(v) [DESC], each as PostgreSQL ranks it: NULL first or last, then the value
    #[derive(Clone, Copy, Debug)]
    enum Agg {
        Min,
        Max,
        Avg,
    }
    for round in 0..6u64 {
        let (nkeys, rows) = ([5u64, 40, 300][round as usize % 3], [30u64, 400, 2000][round as usize % 3]);
        let data: Vec<(u64, u64, Option<i64>)> = (0..rows).map(|_| (rnd(nkeys), rnd(4), (rnd(9) != 0).then(|| rnd(1000) as i64 - 300))).collect();
        let mut groups: BTreeMap<(u64, u64), Vec<Option<i64>>> = BTreeMap::new();
        let mut per_key: BTreeMap<u64, Vec<Option<i64>>> = BTreeMap::new();
        for &(k, k2, v) in &data {
            groups.entry((k, k2)).or_default().push(v);
            per_key.entry(k).or_default().push(v);
        }
        for agg in [Agg::Min, Agg::Max, Agg::Avg] {
            let class = match agg {
                Agg::Min => Class::Min(Ext::Int),
                Agg::Max => Class::Max(Ext::Int),
                Agg::Avg => Class::AvgI128,
            };
            for desc in [false, true] {
                let (witness, best) = class.two_pass_bounds(desc).expect("min, max and avg are bounded");
                let spread = best.is_some();
                let ext = |vs: &[Option<i64>], max: bool| vs.iter().flatten().copied().reduce(|a, b| if max == (b > a) { b } else { a });
                let keys: Vec<ExtKey> = per_key
                    .iter()
                    .map(|(&k, vs)| {
                        let nn = vs.iter().flatten().count() as i64;
                        (k, vs.len() as i64, ext(vs, witness).unwrap_or(0), nn, best.and_then(|m| ext(vs, m)).unwrap_or(0))
                    })
                    .collect();
                let ext_kind = |max: bool| if max { SlotKind::MaxI64 } else { SlotKind::MinI64 };
                let mut slots = vec![SlotKind::Count, ext_kind(witness), SlotKind::Count];
                slots.extend(best.map(ext_kind));
                let l = RecordLayout::with_sentinel(&[KeyKind::I64], &slots, true, Some(0)).partitioned_by(1);
                for nulls_first in [false, true] {
                    // each group's value on the top's order (smaller = first): NULL first or last, then the value
                    let rank = |vs: &[Option<i64>]| -> (u8, i128) {
                        let nn: Vec<i64> = vs.iter().flatten().copied().collect();
                        if nn.is_empty() {
                            return (if nulls_first { 0 } else { 2 }, 0);
                        }
                        // avg scaled by the count's lcm-free form: compare sum/len exactly via (sum * L / len), L = 2^40
                        let x: i128 = match agg {
                            Agg::Min => i128::from(*nn.iter().min().unwrap()) << 40,
                            Agg::Max => i128::from(*nn.iter().max().unwrap()) << 40,
                            Agg::Avg => (nn.iter().map(|&v| i128::from(v)).sum::<i128>() << 40) / nn.len() as i128,
                        };
                        (1, if desc { -x } else { x })
                    };
                    let mut ranked: Vec<((u8, i128), u64)> = groups.iter().map(|(&(k, _), vs)| (rank(vs), k)).collect();
                    ranked.sort();
                    for n in [1u32, 3, 10] {
                        let rule = Rule { ext: true, ext_nn: true, spread, desc, nulls_first, img_words: 1, rec_words: (l.size() / 8) as u8, count_word: (l.sentinel_off() / 8) as u8, n, ..Rule::default() };
                        let spec = TopExportSpec { layout: l, count_slot: 0, img_words: 1, n, desc, nulls_first, nullable: false, ext: true, ext_nn: true, spread };
                        for nowners in [1usize, 8] {
                            let (old, new) = ext_regions(&l, &keys, nowners, &spec);
                            let (a, b) = (admit(&old, &rule), admit(&new, &rule));
                            assert_eq!(a, b, "{agg:?} desc {desc} nulls_first {nulls_first} n {n} owners {nowners}");
                            let admitted: BTreeSet<u64> = a.iter().map(|(k, _)| k[0]).collect();
                            if let Some(&(cut, _)) = ranked.get(n as usize - 1) {
                                for &(r, k) in ranked.iter().take_while(|(r, _)| *r <= cut) {
                                    assert!(admitted.contains(&k), "{agg:?} desc {desc} nulls_first {nulls_first} n {n}: key {k} of a group ranked {r:?} (cut {cut:?}) not admitted");
                                }
                            } else {
                                assert_eq!(admitted.len(), per_key.len(), "fewer groups than n: every key");
                            }
                        }
                    }
                }
            }
        }
    }
}
