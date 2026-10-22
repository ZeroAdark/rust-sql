use std::collections::{BTreeMap, BTreeSet};

use gravity_abi::breaker::{fits_inline_at, AggSlot, Candidates, HashAgg, IntervalSum, KeyKind, Rec, RecordLayout, SlotKind, SubSplit, HASHAGG_HDR, LAYOUT_KEY_TIES, MAX_RECORD_KEYS, MAX_RECORD_SLOTS, MOVING};
use gravity_abi::hash::KeyHash;
use gravity_kernels::ops::sort::topn::t3_dense::{Area, AREA_PAIRS, AREA_SENTINEL, AREA_SHIFT};
use gravity_kernels::table::owner_fold::{area_fold, dense_folds, generated, ident_folds, owner_fold, owner_fold_ident, Fold};

/// The candidate list room these tables carry.
const CAND_MAX: usize = 4096;

fn lcg(s: &mut u64) -> u64 {
    *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    *s >> 17
}

use SlotKind::*;

const NN_KINDS: &[SlotKind] = &[Count, SumI64, SumI128, AvgI128, SumF64, MinCode, SumF32, F64Accum, SumNum, AvgNum, SumNumWide, AvgNumWide, IntervalMin, IntervalMax];
const VALID_KINDS: &[SlotKind] = &[Count, SumI64, SumI128, AvgI128, SumF64, MinCode, MaxCode, MinI64, MaxI64, SumF32, F64Accum, BitAnd, BitOr, BitXor, SumNum, AvgNum, SumNumWide, AvgNumWide, IntervalMin, IntervalMax, IntervalSum];

fn layouts(count: usize, seed: u64) -> Vec<RecordLayout> {
    let mut rng = seed;
    let mut out = Vec::new();
    while out.len() < count {
        let nw = 1 + (lcg(&mut rng) % MAX_RECORD_KEYS as u64) as usize;
        let validity = lcg(&mut rng) % 2 == 0;
        let pool = if validity { VALID_KINDS } else { NN_KINDS };
        let ns = 1 + (lcg(&mut rng) % (MAX_RECORD_SLOTS as u64 - 1)) as usize;
        let mut slots: Vec<SlotKind> = (0..ns).map(|_| pool[(lcg(&mut rng) % pool.len() as u64) as usize]).collect();
        let sentinel = match lcg(&mut rng) % 3 {
            0 => None,
            1 => slots.iter().rposition(|&k| k == Count),
            _ => {
                let j = (lcg(&mut rng) % ns as u64) as usize;
                slots[j] = Count;
                slots.iter().position(|&k| k == Count)
            }
        };
        let keys = vec![KeyKind::I64; nw];
        if !fits_inline_at(&keys, &slots, validity, sentinel) {
            continue;
        }
        let l = RecordLayout::with_sentinel(&keys, &slots, validity, sentinel);
        let pw = (lcg(&mut rng) % 3 == 0).then(|| 1 + (lcg(&mut rng) % nw as u64) as u8).unwrap_or(0);
        out.push(l.partitioned_by(pw));
    }
    out
}

fn make_record(buf: &mut [u8; 256], layout: &RecordLayout, keys: &[u64], v: u64, null: bool) {
    buf.fill(0);
    let r = unsafe { Rec::at(buf.as_mut_ptr(), layout) };
    r.open(keys);
    for j in 0..layout.nslots() {
        if layout.has_validity() && layout.slot_off(j) != layout.sentinel_off() && (v >> (8 + j)) % 3 == 0 {
            continue;
        }
        let s = v.rotate_left(7 * j as u32);
        match layout.slot_kind(j) {
            Count => r.add_count(j, 1 + (s % 3) as i64),
            SumI64 => r.add_i64(j, (s % 1000) as i64 - 500),
            SumI128 => r.add_i128(j, (s % (1 << 40)) as i128 * 1_000_000_007),
            MinI64 => r.min_i64(j, (s % 5000) as i64 - 2500),
            MaxI64 => r.max_i64(j, (s % 5000) as i64 - 2500),
            AvgI128 => r.add_avg(j, (s % 5000) as i128, 1),
            SumF64 => r.add_f64(j, (s % 100) as f64 / 7.0).expect("finite"),
            SumF32 => r.add_f32(j, (s % 100) as f64 / 8.0).expect("finite"),
            F64Accum => r.combine_f64_accum(j, [1.0, (s % 100) as f64 / 8.0, 0.0]).expect("finite"),
            BitAnd | BitOr | BitXor => r.fold_bits(j, (s % 4096) as i64),
            MinCode => r.min_code(j, (s % 4) as u32, (s % 977) as u32),
            MaxCode => r.max_code(j, (s % 4) as u32, (s % 977) as u32),
            SumNum => r.add_i128(j, (s % (1 << 40)) as i128 * 1_000_000_007),
            AvgNum => r.add_avg(j, (s % 5000) as i128, 1),
            SumNumWide => r.add_num256(j, [s.wrapping_mul(1_000_000_007), s % 7, 0, 0]),
            AvgNumWide => r.add_avg_num256(j, [s % 5000, 0, 0, 0], 1),
            IntervalMin | IntervalMax => r.fold_interval(j, ((s % 5000) as i64 * 3_600_000_000, (s % 7) as i32 - 3, (s % 3) as i32), layout.slot_kind(j) == IntervalMin),
            IntervalSum => {
                let mut i = IntervalSum::default();
                i.accum((s % 5000) as i64 * 3_600_000_000, (s % 7) as i32 - 3, (s % 3) as i32);
                r.fold_interval_sum(j, &i).expect("in range")
            }
            k => unreachable!("{k:?}"),
        }
    }
    if null {
        r.set_null_seen();
    }
}

fn records(layout: &RecordLayout, n: usize, domain: u64, seed: u64) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rng = seed;
    let mut buf = [0u8; 256];
    for i in 0..n {
        let keys: Vec<u64> = (0..layout.nkeys()).map(|_| lcg(&mut rng) % domain).collect();
        let v = lcg(&mut rng) | lcg(&mut rng) << 40;
        make_record(&mut buf, layout, &keys, v, layout.has_validity() && i % 5 == 0);
        out.extend_from_slice(&buf[..layout.size() as usize]);
    }
    out
}

struct Table {
    region: Vec<u64>,
    cand: Vec<u32>,
}

impl Table {
    fn new(layout: &RecordLayout, cap: u32) -> Table {
        Table { region: vec![0u64; (HASHAGG_HDR + cap as usize * layout.size() as usize) / 8 + 1], cand: vec![0u32; CAND_MAX] }
    }

    fn open(&mut self, layout: &RecordLayout, cap: u32) -> *mut HashAgg {
        unsafe { HashAgg::at(self.region.as_mut_ptr() as *mut u8, layout, cap, (self.region.len() * 8) as u32) }
    }
}

fn fold_both(layout: &RecordLayout, recs: &[u8], n: usize, cap: u32, split: SubSplit, thr: i64) -> (Table, i64) {
    let mut a = Table::new(layout, cap);
    let mut b = Table::new(layout, cap);
    let (ta, tb) = (a.open(layout, cap), b.open(layout, cap));
    let off = layout.sentinel_off() as usize;
    let mut ca = unsafe { Candidates::at(a.cand.as_mut_ptr() as *mut u8, CAND_MAX, off, thr) };
    let mut cb = unsafe { Candidates::at(b.cand.as_mut_ptr() as *mut u8, CAND_MAX, off, thr) };
    let va = unsafe { owner_fold(core::ptr::null(), ta, recs.as_ptr(), n, split, &mut ca) };
    let vb = unsafe { (*tb).fold_run(recs.as_ptr(), n, split, &mut cb) };
    assert_eq!(va, vb, "the return code");
    assert_eq!((ca.len, ca.overflow), (cb.len, cb.overflow), "the candidate list");
    assert!(a.region == b.region, "the table bytes");
    assert!(a.cand[..ca.len] == b.cand[..cb.len], "the candidate indexes");
    (a, va)
}

fn what(l: &RecordLayout) -> String {
    format!("{} keys / {} partition, {:?}, validity {}, sentinel at {}", l.nkeys(), l.partition_keys(), l.slot_kinds(), l.has_validity(), l.sentinel_off())
}

#[test]
fn every_layout_takes_the_generated_fold_and_folds_as_the_generic_fold_does() {
    for layout in layouts(160, 0xd01) {
        let w = what(&layout);
        assert!(generated(&layout), "{w}");
        for (n, domain) in [(500usize, 50u64), (5000, 700), (3000, 1 << 40)] {
            let recs = records(&layout, n, domain, 0xd01 ^ n as u64 ^ layout.size() as u64);
            let (t, v) = fold_both(&layout, &recs, n, 8192, SubSplit::NONE, 2);
            assert_eq!(v, n as i64, "{w}");
            folds_as_the_model(&layout, &recs, n, &t, &w);
            let words = [u64::MAX; 64];
            let mut b = Table::new(&layout, 8192);
            let tb = b.open(&layout, 8192);
            let mut cb = unsafe { Candidates::at(b.cand.as_mut_ptr() as *mut u8, CAND_MAX, layout.sentinel_off() as usize, 2) };
            assert_eq!(unsafe { owner_fold(words.as_ptr(), tb, recs.as_ptr(), n, SubSplit::NONE, &mut cb) }, v, "{w}: the plan's words");
            assert!(b.region == t.region, "{w}: the plan's words moved the table bytes");
        }
    }
}

fn folds_as_the_model(layout: &RecordLayout, recs: &[u8], n: usize, t: &Table, what: &str) {
    if !layout.slot_kinds().iter().all(|k| matches!(k, Count | SumI64 | SumI128 | AvgI128 | SumF64 | MinI64 | MaxI64 | MinCode | MaxCode)) {
        return;
    }
    let t = unsafe { &*(t.region.as_ptr() as *const HashAgg) };
    let mut model: BTreeMap<Vec<u64>, (Vec<Option<AggSlot>>, bool)> = BTreeMap::new();
    let rec = layout.size() as usize;
    for i in 0..n {
        let r = unsafe { Rec::at(recs.as_ptr().add(i * rec) as *mut u8, layout) };
        let slots: Vec<Option<AggSlot>> = (0..layout.nslots()).map(|j| r.valid(j).then(|| r.slot(j).unwrap())).collect();
        let ns = r.null_seen();
        model
            .entry(r.keys().to_vec())
            .and_modify(|(m, seen)| {
                *seen |= ns;
                for (j, s) in m.iter_mut().enumerate() {
                    *s = match (*s, slots[j]) {
                        (a, None) => a,
                        (None, b) => b,
                        (Some(AggSlot::Count(a)), Some(AggSlot::Count(b))) => Some(AggSlot::Count(if implicit_at(layout, j) { a + (b - 1).max(0) } else { a + b })),
                        (Some(AggSlot::SumI64(a)), Some(AggSlot::SumI64(b))) => Some(AggSlot::SumI64(a.wrapping_add(b))),
                        (Some(AggSlot::SumI128(a)), Some(AggSlot::SumI128(b))) => Some(AggSlot::SumI128(a.wrapping_add(b))),
                        (Some(AggSlot::AvgI128 { sum, n }), Some(AggSlot::AvgI128 { sum: s2, n: n2 })) => Some(AggSlot::AvgI128 { sum: sum.wrapping_add(s2), n: n + n2 }),
                        (Some(AggSlot::SumF64(a)), Some(AggSlot::SumF64(b))) => Some(AggSlot::SumF64(a + b)),
                        (Some(AggSlot::MinI64(a)), Some(AggSlot::MinI64(b))) => Some(AggSlot::MinI64(a.min(b))),
                        (Some(AggSlot::MaxI64(a)), Some(AggSlot::MaxI64(b))) => Some(AggSlot::MaxI64(a.max(b))),
                        (Some(AggSlot::MinCode { part, code }), Some(AggSlot::MinCode { part: p2, code: c2 })) => Some(if (p2, c2) < (part, code) { AggSlot::MinCode { part: p2, code: c2 } } else { AggSlot::MinCode { part, code } }),
                        (Some(AggSlot::MaxCode { part, code }), Some(AggSlot::MaxCode { part: p2, code: c2 })) => Some(if (p2, c2) > (part, code) { AggSlot::MaxCode { part: p2, code: c2 } } else { AggSlot::MaxCode { part, code } }),
                        (a, b) => unreachable!("{a:?} {b:?}"),
                    };
                }
            })
            .or_insert((slots, ns));
    }
    assert_eq!(t.len as usize, model.len(), "{what}: groups");
    for r in t.records() {
        let (m, ns) = &model[&r.keys().to_vec()];
        assert_eq!(r.null_seen(), *ns, "{what} key {:?}: null_seen", r.keys());
        for (j, s) in m.iter().enumerate() {
            let got = r.valid(j).then(|| r.slot(j).unwrap());
            let got = if let Some(AggSlot::Count(c)) = got { Some(AggSlot::Count(c & !(MOVING as i64))) } else { got };
            assert_eq!(&got, s, "{what} key {:?} slot {j}", r.keys());
        }
    }
}

fn implicit_at(l: &RecordLayout, j: usize) -> bool {
    l.flags & gravity_abi::breaker::LAYOUT_IMPLICIT_COUNT != 0 && j == l.nslots() - 1
}

#[test]
fn a_layout_whose_merge_reads_across_slots_keeps_the_generic_fold() {
    let mut keyed = RecordLayout::new(&[KeyKind::I64], &[Count, MinI64, NumSideMin]);
    keyed.flags |= LAYOUT_KEY_TIES;
    assert!(!generated(&keyed), "an order-key layout");
    assert!(!generated(&RecordLayout::new(&[KeyKind::I64], &[Count, AggState])), "a hosted state");
    assert!(generated(&RecordLayout::new(&[KeyKind::I64], &[Count, MinI64])), "the same extreme without the order key");
}

#[test]
fn a_layout_whose_sentinel_is_not_its_first_count_keeps_every_group() {
    let implicit = RecordLayout::with_sentinel(&[KeyKind::I64], &[Count], false, None);
    let named = RecordLayout::with_sentinel(&[KeyKind::I64], &[Count, Count], false, Some(1));
    for layout in [implicit, named] {
        assert!(generated(&layout));
        assert_eq!(layout.sentinel_off() as usize, 16, "the sentinel is the second count");
        let (n, domain) = (6000usize, 900u64);
        let rec = layout.size() as usize;
        let mut recs = vec![0u8; n * rec];
        let mut rng = 11u64;
        let mut keys = BTreeSet::new();
        for i in 0..n {
            let k = lcg(&mut rng) % domain;
            keys.insert(k);
            let r = unsafe { Rec::at(recs.as_mut_ptr().add(i * rec), &layout) };
            r.open(&[k]);
            r.add_count(1, 1);
            if k % 3 != 0 {
                r.add_count(0, 1);
            }
        }
        let (t, v) = fold_both(&layout, &recs, n, 4096, SubSplit::NONE, 2);
        assert_eq!(v, n as i64);
        let t = unsafe { &*(t.region.as_ptr() as *const HashAgg) };
        let got: BTreeSet<u64> = t.records().map(|r| r.keys()[0]).collect();
        assert_eq!(t.len as usize, keys.len());
        assert_eq!(got, keys, "every group, the all-NULL ones included");
    }
}

#[test]
fn a_float_sum_that_overflows_refuses_as_the_generic_fold_does() {
    for layout in [RecordLayout::new_nn(&[KeyKind::I64], &[Count, SumF64]), RecordLayout::new(&[KeyKind::I64], &[Count, SumF64])] {
        let rec = layout.size() as usize;
        let mut recs = vec![0u8; 2 * rec];
        for i in 0..2 {
            let r = unsafe { Rec::at(recs.as_mut_ptr().add(i * rec), &layout) };
            r.open(&[7]);
            r.add_count(0, 1);
            r.add_f64(1, f64::MAX).expect("finite");
        }
        let (_, v) = fold_both(&layout, &recs, 2, 64, SubSplit::NONE, 2);
        assert_eq!(v, gravity_abi::breaker::FOLD_OVERFLOW, "{}", what(&layout));
    }
}

#[test]
fn a_table_under_eight_slots_keeps_one_slot_empty_and_returns_full() {
    for layout in [RecordLayout::new_nn(&[KeyKind::I64], &[Count, SumI64]), RecordLayout::new_nn(&[KeyKind::I64; 2], &[Count, MinCode])] {
        assert!(generated(&layout));
        for cap in [2u32, 4] {
            let n = cap as usize + 1;
            let recs = records(&layout, n, 1 << 40, 11);
            for generic in [false, true] {
                let (tx, rx) = std::sync::mpsc::channel();
                let recs = recs.clone();
                std::thread::spawn(move || {
                    let mut t = Table::new(&layout, cap);
                    let tp = t.open(&layout, cap);
                    let mut c = unsafe { Candidates::at(t.cand.as_mut_ptr() as *mut u8, CAND_MAX, layout.sentinel_off() as usize, 0) };
                    let v = unsafe {
                        if generic {
                            (*tp).fold_run(recs.as_ptr(), n, SubSplit::NONE, &mut c)
                        } else {
                            owner_fold(core::ptr::null(), tp, recs.as_ptr(), n, SubSplit::NONE, &mut c)
                        }
                    };
                    let _ = tx.send((v, unsafe { (*tp).len }));
                });
                let got = rx.recv_timeout(std::time::Duration::from_secs(20));
                let (v, len) = got.unwrap_or_else(|_| panic!("{} cap {cap} generic {generic}: the fold spun on a full table", what(&layout)));
                assert_eq!((v, len), (-1 - (cap as i64 - 1), cap - 1), "{} cap {cap} generic {generic}: full with one slot empty", what(&layout));
            }
        }
    }
}

#[test]
fn the_refusal_and_the_split_filter() {
    let layout = RecordLayout::new_nn(&[KeyKind::I64; 2], &[Count, MinCode]);
    let recs = records(&layout, 40, 1 << 40, 7);
    let (_, v) = fold_both(&layout, &recs, 40, 8, SubSplit::NONE, 0);
    assert_eq!(v, -1 - 7);
    let mut kept = 0;
    for sub in 0..64u64 {
        let split = SubSplit { pbits: 2, depth: 2, reread: 0, sub };
        let (_, v) = fold_both(&layout, &recs, 40, 128, split, 0);
        kept += v;
        for i in 0..40 {
            let p = unsafe { recs.as_ptr().add(i * layout.size() as usize) };
            let h = KeyHash::of(unsafe { core::slice::from_raw_parts(p as *const u64, 2) });
            assert_eq!(split.keeps(h), SubSplit::sub_of(h, 2, 2) == sub);
        }
    }
    assert_eq!(kept, 40);
}

/// exec's area as the fold sees it: the counter, the flag, the index and `span` accumulators of `acc` bytes, the fill word.
struct AreaMem {
    mem: Vec<u64>,
    span: u32,
    acc: usize,
    fill: u64,
}

impl AreaMem {
    fn new(span: u32, acc: usize) -> AreaMem {
        AreaMem { mem: vec![0u64; 1 + (span as usize).div_ceil(2) + span as usize * acc / 8], span, acc, fill: 0 }
    }

    fn area(&mut self, lo: i64, quiet: usize) -> Area {
        let base = self.mem.as_mut_ptr() as *mut u32;
        let recs = unsafe { self.mem.as_mut_ptr().add(1 + (self.span as usize).div_ceil(2)) } as *mut u8;
        unsafe { Area::over(base, base.add(1), base.add(2), recs, self.acc, lo, self.span, quiet, &mut self.fill) }
    }

    /// Per opened group: its key, the sentinel, the pairs, the riding words.
    fn groups(&self) -> BTreeMap<i64, (u64, u64, Vec<u64>)> {
        let n = self.mem[0] as u32 as usize;
        let recs = &self.mem[1 + (self.span as usize).div_ceil(2)..];
        (0..n).map(|j| &recs[j * self.acc / 8..(j + 1) * self.acc / 8]).map(|r| (r[0] as i64, (r[AREA_SENTINEL], r[AREA_PAIRS], r[AREA_PAIRS + 1..].to_vec()))).collect()
    }

    fn bad(&self) -> bool {
        self.mem[0] >> 32 != 0
    }
}

/// The identities a table holds: `(lo + word 0 - 1, word 1)` per used slot.
fn identities(t: *const HashAgg, lo: i64) -> std::collections::BTreeSet<(u64, u64)> {
    let (cap, base) = unsafe { ((*t).cap as usize, (*t).base_ptr() as *const u64) };
    (0..cap).map(|j| unsafe { (*base.add(2 * j), *base.add(2 * j + 1)) }).filter(|w| w.0 != 0).map(|w| ((w.0 - 1).wrapping_add(lo as u64), w.1)).collect()
}

/// The model of the area fold: per first key word the distinct pairs and the first `riding` slots' words summed
/// over every record (a count masked of `MOVING`, the two-word sums carried).
fn area_model(layout: &RecordLayout, recs: &[u8], n: usize, riding: usize, keep: impl Fn(i64) -> bool) -> BTreeMap<i64, (u64, u64, Vec<u64>)> {
    let rec = layout.size() as usize;
    let words = (layout.size() as usize - 16) / 8;
    let mut pairs = std::collections::BTreeSet::new();
    let mut out: BTreeMap<i64, (u64, u64, Vec<u64>)> = BTreeMap::new();
    for i in 0..n {
        let w: Vec<u64> = recs[i * rec..(i + 1) * rec].chunks(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect();
        if !keep(w[0] as i64) {
            continue;
        }
        let g = out.entry(w[0] as i64).or_insert((1, 0, vec![0; words]));
        g.1 += pairs.insert((w[0], w[1])) as u64;
        for j in 0..riding {
            let o = layout.slot_off(j) as usize / 8;
            let d = o - 2;
            match layout.slot_kind(j) {
                Count => g.2[d] = g.2[d].wrapping_add(w[o] & !MOVING),
                SumI64 => g.2[d] = g.2[d].wrapping_add(w[o]),
                k => {
                    let v = (g.2[d] as u128 | (g.2[d + 1] as u128) << 64).wrapping_add(w[o] as u128 | (w[o + 1] as u128) << 64);
                    (g.2[d], g.2[d + 1]) = (v as u64, (v >> 64) as u64);
                    if k == AvgI128 {
                        g.2[d + 2] = g.2[d + 2].wrapping_add(w[o + 2]);
                    }
                }
            }
        }
    }
    out
}

fn dense_layouts() -> Vec<RecordLayout> {
    let pool = [Count, SumI64, SumI128, AvgI128];
    let mut rng = 0xa4ea_u64;
    let mut out = Vec::new();
    while out.len() < 24 {
        let ns = 1 + (lcg(&mut rng) % 4) as usize;
        let slots: Vec<SlotKind> = (0..ns).map(|_| pool[(lcg(&mut rng) % 4) as usize]).collect();
        let sentinel = if lcg(&mut rng) % 2 == 0 { None } else { slots.iter().position(|&k| k == Count) };
        let keys = [KeyKind::I64; 2];
        if fits_inline_at(&keys, &slots, false, sentinel) {
            out.push(RecordLayout::with_sentinel(&keys, &slots, false, sentinel));
        }
    }
    out
}

#[test]
fn the_identity_fold_counts_pairs_and_folds_the_riding_slots_and_re_reads_after_a_fill() {
    assert_eq!(AREA_SHIFT, 8);
    for refused in [RecordLayout::new_nn(&[KeyKind::I64; 2], &[Count, MinCode]), RecordLayout::new_nn(&[KeyKind::I64; 2], &[Count, SumF64]), RecordLayout::new(&[KeyKind::I64; 2], &[Count, SumI64]), RecordLayout::new_nn(&[KeyKind::I64], &[Count, SumI64])] {
        assert!(!dense_folds(&refused), "{}", what(&refused));
    }
    for (row, layout) in dense_layouts().into_iter().enumerate() {
        let w = what(&layout);
        assert!(dense_folds(&layout), "{w}");
        let f = Fold::dense(&layout).expect("dense");
        let (n, rec) = (4000usize, layout.size() as usize);
        let recs = records(&layout, n, 40, 17 + row as u64);
        let implicit = layout.flags & gravity_abi::breaker::LAYOUT_IMPLICIT_COUNT != 0;
        let ridings = if implicit { vec![layout.nslots() - 1] } else { vec![layout.nslots(), layout.nslots() - 1] };
        for riding in ridings {
            let acc = rec + AREA_SHIFT;
            let cap = 8192u32;
            let mut mem = AreaMem::new(40, acc);
            let mut tb = vec![0u64; HashAgg::bytes(&layout, cap) as usize / 8];
            // a table that fills first: it counted the records before the fill, and says where it stopped
            let small = unsafe { HashAgg::at(tb.as_mut_ptr() as *mut u8, &layout, 64, HashAgg::bytes(&layout, 64)) as *mut HashAgg };
            let (mut v, mut pos) = (0, 0usize);
            for run in recs.chunks(100 * rec) {
                let area = mem.area(0, 0);
                v = unsafe { area_fold(small, run.as_ptr(), run.len() / rec, SubSplit::NONE, &area, &f, riding) };
                if v < 0 {
                    break;
                }
                pos += run.len() / rec;
            }
            assert!(v < 0, "the small table fills");
            let filled = pos + mem.fill as usize;
            assert!(filled >= 56 && filled < n, "{w} riding {riding}: the fill at {filled}");
            assert_eq!(mem.groups(), area_model(&layout, &recs, filled, riding, |_| true), "{w} riding {riding}: counted up to the fill");
            // the re-read: the same stream, the records before the fill quiet
            tb.fill(0);
            let t = unsafe { HashAgg::at(tb.as_mut_ptr() as *mut u8, &layout, cap, HashAgg::bytes(&layout, cap)) as *mut HashAgg };
            let (mut folded, mut pos) = (0, 0usize);
            for run in recs.chunks(37 * rec) {
                let area = mem.area(0, filled.saturating_sub(pos));
                folded += unsafe { area_fold(t, run.as_ptr(), run.len() / rec, SubSplit::NONE, &area, &f, riding) };
                pos += run.len() / rec;
            }
            assert_eq!(folded as usize, n);
            assert!(!mem.bad());
            let model = area_model(&layout, &recs, n, riding, |_| true);
            assert_eq!(mem.groups(), model, "{w} riding {riding}");
            assert_eq!(model.values().map(|g| g.1).sum::<u64>(), unsafe { (*t).len } as u64, "a pair per identity");
            let pairs: std::collections::BTreeSet<(u64, u64)> = (0..n).map(|i| (u64::from_le_bytes(recs[i * rec..i * rec + 8].try_into().unwrap()), u64::from_le_bytes(recs[i * rec + 8..i * rec + 16].try_into().unwrap()))).collect();
            assert_eq!(identities(t, 0), pairs, "{w}: the table holds the identities");
            // a narrower domain: the keys outside it flagged and dropped, the rest counted
            let mut narrow = AreaMem::new(20, acc);
            let area = narrow.area(5, 0);
            let mut tb2 = vec![0u64; tb.len()];
            let t2 = unsafe { HashAgg::at(tb2.as_mut_ptr() as *mut u8, &layout, cap, HashAgg::bytes(&layout, cap)) as *mut HashAgg };
            unsafe { area_fold(t2, recs.as_ptr(), n, SubSplit::NONE, &area, &f, riding) };
            assert!(narrow.bad());
            let inside = area_model(&layout, &recs, n, riding, |k| (5..25).contains(&k));
            assert_eq!(narrow.groups(), inside);
            assert_eq!(inside.values().map(|g| g.1).sum::<u64>(), unsafe { (*t2).len } as u64);
            assert!(identities(t2, 5).iter().all(|p| (5..25).contains(&(p.0 as i64))));
        }
    }
}

/// The key words of record `i` of a stream.
fn key_words(recs: &[u8], rec: usize, nw: usize, i: usize) -> Vec<u64> {
    (0..nw).map(|k| u64::from_le_bytes(recs[i * rec + 8 * k..i * rec + 8 * k + 8].try_into().unwrap())).collect()
}

#[test]
fn the_plain_identity_fold_counts_the_distinct_keys_with_the_zero_key_and_fills_as_the_generic_fold_does() {
    let mut drawn = layouts(400, 0x1de7);
    drawn.sort_by_key(|l| l.nkeys());
    drawn.dedup_by_key(|l| l.nkeys());
    assert_eq!(drawn.len(), MAX_RECORD_KEYS, "a layout of every key word count");
    for layout in drawn {
        let w = what(&layout);
        assert!(ident_folds(&layout), "{w}");
        let (n, rec, nw) = (4000usize, layout.size() as usize, layout.nkeys());
        for (domain, cap) in [(3u64, 8192u32), (3000, 8192), (3000, 64)] {
            let mut recs = records(&layout, n, domain, 5 + domain + nw as u64);
            recs[..8 * nw].fill(0);
            let distinct: BTreeSet<Vec<u64>> = (0..n).map(|i| key_words(&recs, rec, nw, i)).collect();
            assert!(distinct.contains(&vec![0; nw]), "{w}: the all-zero key is in the stream");
            let mut cand = Candidates::none();
            let mut generic = Table::new(&layout, cap);
            let tg = generic.open(&layout, cap);
            let vg = unsafe { (*tg).fold_run(recs.as_ptr(), n, SubSplit::NONE, &mut cand) };
            for chunk in [1usize, 37, n] {
                let mut a = Table::new(&layout, cap);
                let t = a.open(&layout, cap);
                let (mut folded, mut pos, mut v) = (0i64, 0usize, 0i64);
                for run in recs.chunks(chunk * rec) {
                    v = unsafe { owner_fold_ident(core::ptr::null(), t, run.as_ptr(), run.len() / rec, SubSplit::NONE, &mut cand) };
                    if v < 0 {
                        break;
                    }
                    folded += v;
                    pos += run.len() / rec;
                }
                let len = unsafe { (*t).len } as usize;
                if cap == 64 {
                    assert!(v < 0 && vg < 0, "{w} runs of {chunk}: the small table fills");
                    let at = pos + (-1 - v) as usize;
                    let seen: BTreeSet<Vec<u64>> = (0..at).map(|i| key_words(&recs, rec, nw, i)).collect();
                    assert_eq!(len, seen.len(), "{w} runs of {chunk}: the fill at {at}");
                    assert!(len >= 56, "{w}: the fill at 7/8");
                } else {
                    assert_eq!(folded as usize, n);
                    assert_eq!(len, distinct.len(), "{w} domain {domain} runs of {chunk}");
                    assert_eq!(len, unsafe { (*tg).len } as usize, "{w}: the generic fold's len");
                    let ident: BTreeSet<Vec<u64>> = (0..cap as usize).map(|j| unsafe { core::slice::from_raw_parts((*t).base_ptr().add(j * 8 * nw) as *const u64, nw).to_vec() }).filter(|w| w.iter().any(|&x| x != 0)).collect();
                    let nonzero: BTreeSet<Vec<u64>> = distinct.iter().filter(|w| w.iter().any(|&x| x != 0)).cloned().collect();
                    assert_eq!(ident, nonzero, "{w}: the table holds the identities at a stride of the key words");
                }
            }
        }
        // the split filter: the subs of a depth-2 split share the stream, the whole once
        let recs = records(&layout, 400, 1 << 40, 11 + nw as u64);
        let (mut kept, mut len) = (0, 0);
        for sub in 0..64u64 {
            let split = SubSplit { pbits: 2, depth: 2, reread: 0, sub };
            let mut a = Table::new(&layout, 1024);
            let t = a.open(&layout, 1024);
            kept += unsafe { owner_fold_ident(core::ptr::null(), t, recs.as_ptr(), 400, split, &mut Candidates::none()) };
            len += unsafe { (*t).len };
        }
        assert_eq!(kept, 400, "{w}: every record in one sub");
        assert_eq!(len as usize, (0..400).map(|i| key_words(&recs, rec, nw, i)).collect::<BTreeSet<_>>().len(), "{w}: the subs' identities");
    }
}
