//! The owner fold's rig (m3-perfdiag3 §C fix 5, lane M3-L6L): the per-run road (`fold_sub`'s: `owner_fold` per
//! block, the prefetch ring) against two roads without a probe per image — the bucket arm (`stage_fold`: the images
//! counting-sorted by an L1 window of home slots, then the per-run fold per window) and the sort arm (`sort_fold`:
//! sorted by home slot and laid in with a monotone cursor, a repeat met among its home's records). The folded record
//! set and the candidates' keys are identical over every shape and the generic layout; `FOLD_RIG=1` prints the
//! three roads' ns per image at a one-key count's shapes (the per-run road won on 2026-09-21: the arms stay here as the rig).

use gravity_abi::hash::KeyHash;
use gravity_abi::breaker::{hash_at, Candidates, HashAgg, KeyKind, Rec, RecordLayout, SlotKind, SubSplit, HASHAGG_HDR, MOVING};
use gravity_kernels::table::owner_fold::{generated, owner_fold};

/// The candidate list room these tables carry.
const CAND_MAX: usize = 4096;

fn lcg(s: &mut u64) -> u64 {
    *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    *s >> 17
}

fn records(layout: &RecordLayout, n: usize, domain: u64, seed: u64) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rng = seed;
    let mut buf = [0u8; 256];
    for _ in 0..n {
        buf.fill(0);
        let keys: Vec<u64> = (0..layout.nkeys()).map(|_| lcg(&mut rng) % domain).collect();
        let r = unsafe { Rec::at(buf.as_mut_ptr(), layout) };
        r.open(&keys);
        let v = lcg(&mut rng);
        for j in 0..layout.nslots() {
            match layout.slots[j] {
                SlotKind::Count => r.add_count(j, 1 + (v % 3) as i64),
                SlotKind::SumI64 => r.add_i64(j, (v % 1000) as i64 - 500),
                SlotKind::SumI128 => r.add_i128(j, v as i128 * 1_000_000_007),
                SlotKind::MinI64 => r.min_i64(j, (v % 5000) as i64 - 2500),
                SlotKind::MaxI64 => r.max_i64(j, (v % 5000) as i64 - 2500),
                SlotKind::AvgI128 => r.add_avg(j, (v % 5000) as i128, 1),
                SlotKind::SumF64 => r.add_f64(j, (v % 100) as f64 / 7.0).expect("finite"),
                SlotKind::MinCode => r.min_code(j, (v % 4) as u32, (v % 977) as u32),
                _ => unreachable!(),
            }
        }
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

/// The folded table as a sorted multiset of its used records (the count word's `MOVING` mark cleared), and
/// the candidates as the sorted set of their key words: the staged road lands a record in another slot.
fn folded_set(t: &HashAgg, layout: &RecordLayout, cand: &[u32]) -> (Vec<Vec<u8>>, Vec<Vec<u64>>) {
    let rec = layout.size() as usize;
    let sent = layout.sentinel_off as usize;
    let mut set = Vec::new();
    for r in t.records() {
        let mut bytes = unsafe { std::slice::from_raw_parts(r.as_ptr(), rec) }.to_vec();
        let w = u64::from_le_bytes(bytes[sent..sent + 8].try_into().unwrap()) & !MOVING;
        bytes[sent..sent + 8].copy_from_slice(&w.to_le_bytes());
        set.push(bytes);
    }
    set.sort();
    let mut keys: Vec<Vec<u64>> = cand.iter().map(|&j| t.record(j as usize).keys().to_vec()).collect();
    keys.sort();
    (set, keys)
}

/// The three roads over the same images in blocks of `block` records: the per-run road (`owner_fold` per
/// block), the staged road (`stage_fold`) and the sorted road (`sort_fold`); the folded sets equal. The walls in ns.
fn stage_both(layout: &RecordLayout, recs: &[u8], n: usize, block: usize, cap: u32, thr: i64) -> (u128, u128, u128) {
    let rec = layout.size() as usize;
    let off = layout.sentinel_off as usize;
    let blocks: Vec<(*const u8, usize)> = (0..n).step_by(block).map(|i| (unsafe { recs.as_ptr().add(i * rec) }, block.min(n - i))).collect();
    // both roads' memory touched before the clocks (a fresh mapping's page faults are not the fold)
    let mut a = Table::new(layout, cap);
    let mut b = Table::new(layout, cap);
    let mut stage = vec![0u64; (n * (rec + 4) + 4 * (cap as usize + 1) + 64) / 8 + 1];
    for v in [&mut a.region, &mut b.region, &mut stage] {
        v.fill(1);
        v.fill(0);
    }
    let ta = a.open(layout, cap);
    let mut ca = unsafe { Candidates::at(a.cand.as_mut_ptr() as *mut u8, CAND_MAX, off, thr) };
    let t0 = std::time::Instant::now();
    let mut va = 0i64;
    for &(p, k) in &blocks {
        let v = unsafe { owner_fold(core::ptr::null(), ta, p, k, SubSplit::NONE, &mut ca) };
        assert!(v >= 0, "the per-run road filled");
        va += v;
    }
    let wall_a = t0.elapsed().as_nanos();
    let tb = b.open(layout, cap);
    let mut cb = unsafe { Candidates::at(b.cand.as_mut_ptr() as *mut u8, CAND_MAX, off, thr) };
    let t1 = std::time::Instant::now();
    let mut vb = 0i64;
    let r = unsafe {
        stage_fold(tb, stage.as_mut_ptr() as *mut u8, stage.len() * 8, |f| blocks.iter().for_each(|&(p, k)| f(p, k)), |p, k| {
            let v = owner_fold(core::ptr::null(), tb, p, k, SubSplit::NONE, &mut cb);
            vb += v.max(-1 - v);
            v >= 0
        })
    };
    let wall_b = t1.elapsed().as_nanos();
    assert_eq!(r, Some(true), "the staged road");
    assert_eq!(va, vb, "the records folded");
    let mut c = Table::new(layout, cap);
    c.region.fill(1);
    c.region.fill(0);
    let tc = c.open(layout, cap);
    let mut cc = unsafe { Candidates::at(c.cand.as_mut_ptr() as *mut u8, CAND_MAX, off, thr) };
    let t2 = std::time::Instant::now();
    let vc = unsafe { sort_fold(tc, SubSplit::NONE, &mut cc, stage.as_mut_ptr() as *mut u8, stage.len() * 8, |f| blocks.iter().for_each(|&(p, k)| f(p, k))) };
    let wall_c = t2.elapsed().as_nanos();
    assert_eq!(vc, Some(va), "the sorted road");
    let (ta, tb, tc) = unsafe { (&*ta, &*tb, &*tc) };
    assert_eq!((ta.len, ta.len), (tb.len, tc.len), "the groups");
    assert_eq!(((ca.overflow, ca.len), (ca.overflow, ca.len)), ((cb.overflow, cb.len), (cc.overflow, cc.len)), "the candidate list");
    let (sa, ka) = folded_set(ta, layout, &a.cand[..ca.len]);
    let (sb, kb) = folded_set(tb, layout, &b.cand[..cb.len]);
    let (sc, kc) = folded_set(tc, layout, &c.cand[..cc.len]);
    assert!(sa == sb && sa == sc, "the folded set");
    // an overflowed list is not read (the reduce walks the table): its members follow the fold order
    assert!(ca.overflow || (ka == kb && ka == kc), "the candidates' keys");
    // the sorted road's table is a linear-probing table: every key is found from its home
    for r in tc.records() {
        let mut j = tc.home(r.hash());
        loop {
            let d = tc.record(j);
            assert!(d.used(), "a hole before key {:?}", r.keys());
            if d.keys() == r.keys() {
                break;
            }
            j = (j + 1) & (cap as usize - 1);
        }
    }
    (wall_a, wall_b, wall_c)
}

#[test]
fn the_staged_fold_folds_the_per_run_roads_record_set_over_every_shape_and_the_generic_layout() {
    use KeyKind::I64;
    use SlotKind::*;
    let layouts = [
        RecordLayout::new_nn(&[I64], &[Count]),
        RecordLayout::new_nn(&[I64], &[Count, SumI64, AvgI128]),
        RecordLayout::new_nn(&[I64], &[SumI64, Count, SumI128]),
        RecordLayout::new_nn(&[I64; 2], &[Count, MinCode]),
        RecordLayout::new_nn(&[I64; 2], &[Count, AvgI128, MinCode, SumF64]),
        RecordLayout::new_nn(&[I64; 3], &[Count, MinCode, MinCode]).partitioned_by(2),
        RecordLayout::new_nn(&[I64; 5], &[Count, MinCode]),
        RecordLayout::new_nn(&[I64; 8], &[Count]),
        RecordLayout::with_sentinel(&[I64], &[Count], false, None),
        RecordLayout::new(&[I64; 2], &[MinI64, MaxI64, MaxI64, Count]),
        RecordLayout::new(&[I64, I64, I64], &[Count, SumI64, MinI64, MaxI64, SumF64]),
    ];
    for (li, layout) in layouts.iter().enumerate() {
        assert!(layout.size() % 8 == 0);
        for (n, domain, block, cap) in [(3000usize, 2300u64, 256usize, 8192u32), (700, 40, 100, 2048), (5000, 1 << 40, 256, 16384), (1, 5, 256, 64)] {
            let recs = records(layout, n, domain, li as u64 * 7919 + n as u64);
            stage_both(layout, &recs, n, block, cap, 2);
        }
    }
}

/// `FOLD_RIG=1`: the rig of m3-perfdiag3 §C fix 5 — a one-key count's shape (a key word, a count) at the corpus partition
/// (21 K images for 16 K groups in 256-record blocks) and at a 1 M-image partition; both roads' ns per image.
#[test]
fn the_staged_fold_rig() {
    if std::env::var("FOLD_RIG").is_err() {
        return;
    }
    for (nw, kinds) in [(1usize, &[SlotKind::Count][..]), (1, &[SlotKind::Count, SlotKind::SumI64][..]), (2, &[SlotKind::Count][..])] {
        let layout = RecordLayout::new_nn(&vec![KeyKind::I64; nw], kinds);
        assert!(generated(&layout), "the generated fold");
        for (n, groups) in [(21_000usize, 16_000u64), (45_000, 34_000), (1_000_000, 770_000)] {
            let cap = (groups * 2).next_power_of_two() as u32;
            let recs = records(&layout, n, groups, n as u64);
            let mut best = (u128::MAX, u128::MAX, u128::MAX);
            for _ in 0..5 {
                let (a, b, c) = stage_both(&layout, &recs, n, 256, cap, 4);
                best = (best.0.min(a), best.1.min(b), best.2.min(c));
            }
            println!("rig nw={nw} slots={} rec={} n={n} groups={groups} cap={cap}: per-run {:.2} ns/image, staged {:.2}, sorted {:.2}", kinds.len(), layout.size(), best.0 as f64 / n as f64, best.1 as f64 / n as f64, best.2 as f64 / n as f64);
        }
    }
}

/// The table bytes one stage bucket's probes touch (an L1 window), and the most buckets a stage scatters into.
const STAGE_REGION: usize = 32 << 10;
const STAGE_BUCKETS: usize = 1024;

/// The staged owner fold (m3-perfdiag3 §C fix 5, the bucket arm): a partition's in-memory run images hashed once and counted
/// by home-slot bucket (`STAGE_REGION` bytes of table per bucket), scattered into `stage` in bucket order,
/// then `run` per bucket over its contiguous whole records — a bucket's probes all land in one L1 window of
/// the table instead of a dependent load per image across the whole table. `walk` visits the images
/// (`recs` whole records at `ptr`) and is called twice; `run` folds a run and says whether to go on (false:
/// the table filled). `None` when the stage cannot hold the images (the caller keeps the per-run road),
/// else `run`'s last verdict. The folded record set is the per-run road's; the slots the records land in
/// follow the bucket order.
/// # Safety
/// `t` is the owner's live table; `stage` is `stage_bytes` of this thread's scratch, 8-aligned; `walk`'s
/// records are whole records of the table's layout, read-only for the call.
unsafe fn stage_fold(t: *const HashAgg, stage: *mut u8, stage_bytes: usize, mut walk: impl FnMut(&mut dyn FnMut(*const u8, usize)), mut run: impl FnMut(*const u8, usize) -> bool) -> Option<bool> {
    let layout = &(*t).layout;
    let (rec, nkeys) = (layout.size() as usize, layout.nkeys());
    let cap = (*t).cap as usize;
    if rec % 8 != 0 || rec == 0 || cap < 2 || stage_bytes < 64 {
        return None;
    }
    let mut shift = 0u32;
    while (rec << shift) < STAGE_REGION {
        shift += 1;
    }
    let mut buckets = (cap >> shift).max(1);
    while buckets > STAGE_BUCKETS {
        shift += 1;
        buckets >>= 1;
    }
    let counts_bytes = (buckets + 1) * 4;
    let n_max = stage_bytes.saturating_sub(counts_bytes + 8) / (4 + rec);
    if n_max == 0 {
        return None;
    }
    let counts = stage as *mut u32;
    let homes = stage.add(counts_bytes) as *mut u32;
    let recs = stage.add((counts_bytes + 4 * n_max + 7) & !7);
    core::ptr::write_bytes(counts, 0, buckets + 1);
    let mut i = 0usize;
    walk(&mut |p: *const u8, n: usize| {
        let mut k = 0;
        while k < n && i < n_max {
            let home = (*t).home(hash_at(p.add(k * rec), nkeys)) as u32;
            *homes.add(i) = home;
            *counts.add(home as usize >> shift) += 1;
            i += 1;
            k += 1;
        }
        i += n - k;
    });
    if i > n_max {
        return None;
    }
    let mut acc = 0u32;
    for b in 0..buckets {
        let c = *counts.add(b);
        *counts.add(b) = acc;
        acc += c;
    }
    *counts.add(buckets) = acc;
    // the scatter: `counts[b]` walks from the bucket's start to its end (the end is `counts[b + 1]` after)
    let words = rec / 8;
    i = 0;
    walk(&mut |p: *const u8, n: usize| {
        for k in 0..n {
            let c = counts.add(*homes.add(i) as usize >> shift);
            let dst = recs.add(*c as usize * rec) as *mut u64;
            let src = p.add(k * rec) as *const u64;
            for w in 0..words {
                *dst.add(w) = *src.add(w);
            }
            *c += 1;
            i += 1;
        }
    });
    let mut start = 0usize;
    for b in 0..buckets {
        let end = *counts.add(b) as usize;
        if end > start && !run(recs.add(start * rec), end - start) {
            return Some(false);
        }
        start = end;
    }
    Some(true)
}

/// The sorted owner fold (m3-perfdiag3 §C fix 5, the sort arm): a partition's in-memory run images counting-sorted
/// by home slot into `stage` (hashed once), then laid into the table home by home with a monotone cursor — a
/// key's slot is `max(cursor, home)`, so every slot between its home and it is taken and the table stays a
/// linear-probing table; a repeat of a key is met among its home's few placed records, never by a probe
/// with a sentinel load; the images past the table's end wrap through the general probe. `walk` visits the
/// images (`recs` whole records at `ptr`) and is called twice. `None` when the stage cannot hold them (the
/// caller keeps the per-run road); else `FoldFn`'s verdict: the images folded, `-1 - folded` at a fill.
/// The folded record set is the per-run road's; the slots the records land in follow the home order.
/// # Safety
/// `t` is the owner's live table; `stage` is `stage_bytes` of this thread's scratch, 8-aligned; `walk`'s
/// records are whole records of the table's layout, read-only for the call; `cand` is the run's list.
/// A home slot of a record the split drops (never a slot: `cap` ≤ 2^31).
const DROPPED: u32 = u32::MAX;

unsafe fn sort_fold(t: *mut HashAgg, split: SubSplit, cand: *mut Candidates, stage: *mut u8, stage_bytes: usize, mut walk: impl FnMut(&mut dyn FnMut(*const u8, usize))) -> Option<i64> {
    let layout = (*t).layout;
    let (rec, nkeys, pkeys, sent) = (layout.size() as usize, layout.nkeys(), layout.partition_keys(), layout.sentinel_off() as usize);
    let cap = (*t).cap as usize;
    if rec % 8 != 0 || rec == 0 || cap < 2 {
        return None;
    }
    let mask = cap - 1;
    let counts_bytes = (cap + 1) * 4;
    let n_max = stage_bytes.saturating_sub(counts_bytes + 8) / (4 + rec);
    if n_max == 0 {
        return None;
    }
    let counts = stage as *mut u32;
    let homes = stage.add(counts_bytes) as *mut u32;
    let recs = stage.add((counts_bytes + 4 * n_max + 7) & !7);
    core::ptr::write_bytes(counts, 0, cap + 1);
    let keeps = |p: *const u8, h: KeyHash| split.depth == 0 || split.keeps(if pkeys == nkeys { h } else { hash_at(p, pkeys) });
    let mut i = 0usize;
    walk(&mut |p: *const u8, n: usize| {
        let mut k = 0;
        while k < n && i < n_max {
            let q = p.add(k * rec);
            let h = hash_at(q, nkeys);
            let kept = keeps(q, h);
            let home = (*t).home(h) as u32;
            *homes.add(i) = if kept { home } else { DROPPED };
            *counts.add(home as usize) += kept as u32;
            i += 1;
            k += 1;
        }
        i += n - k;
    });
    if i > n_max {
        return None;
    }
    let mut acc = 0u32;
    for b in 0..cap {
        let c = *counts.add(b);
        *counts.add(b) = acc;
        acc += c;
    }
    *counts.add(cap) = acc;
    let words = rec / 8;
    i = 0;
    walk(&mut |p: *const u8, n: usize| {
        for k in 0..n {
            let home = *homes.add(i);
            i += 1;
            let q = p.add(k * rec);
            if home == DROPPED {
                continue;
            }
            let c = counts.add(home as usize);
            let dst = recs.add(*c as usize * rec) as *mut u64;
            let src = q as *const u64;
            for w in 0..words {
                *dst.add(w) = *src.add(w);
            }
            *c += 1;
        }
    });
    let base = (*t).base_ptr();
    let full_at = (cap - cap / 8) as u32;
    let mut len = (*t).len;
    let (mut cursor, mut start, mut folded) = (0usize, 0usize, 0i64);
    for h in 0..cap {
        let end = *counts.add(h) as usize;
        let mut first = usize::MAX;
        for r in start..end {
            let src = Rec::at(recs.add(r * rec) as *mut u8, &layout);
            let mut j = first;
            let mut found = false;
            if cursor < cap {
                while j < cursor {
                    if Rec::at(base.add(j * rec), &layout).keys() == src.keys() {
                        found = true;
                        break;
                    }
                    j += 1;
                }
                if !found {
                    j = cursor.max(h);
                    if j >= cap {
                        cursor = cap;
                    }
                }
            }
            if cursor >= cap {
                j = h;
                found = loop {
                    let d = base.add(j * rec);
                    if *(d.add(sent) as *const u64) == 0 {
                        break false;
                    }
                    if Rec::at(d, &layout).keys() == src.keys() {
                        break true;
                    }
                    j = (j + 1) & mask;
                };
            }
            let d = Rec::at(base.add(j * rec), &layout);
            if found {
                d.fold(&src).expect("no float overflow");
            } else {
                if len >= full_at {
                    (*t).len = len;
                    return Some(-1 - folded);
                }
                len += 1;
                d.copy_from(&src);
                if cursor < cap {
                    if first == usize::MAX {
                        first = j;
                    }
                    cursor = j + 1;
                }
            }
            folded += 1;
            (*cand).note(d.as_ptr(), j);
        }
        start = end;
    }
    (*t).len = len;
    Some(folded)
}
