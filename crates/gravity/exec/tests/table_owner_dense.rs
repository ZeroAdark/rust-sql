//! The distinct form's dense round 1 (exec `owner.rs::distinct_fns_dense`; ladder/rung5-distinct.md need 1) against
//! the pair path over the same declared layout and against an oracle: partitions of deduped pair records folded and
//! reduced per owner by the driver's protocol at several widths and partition counts, the round closed, round 2
//! over the owners' round-2 buffers — the count form and the run form, the int-pair key shapes, a negative domain,
//! a region too small for the area (the pair path), a key outside the domain (refused by name), the bind's words.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use gravity_abi::breaker::{Candidates, HashAgg, SubSplit, HASHAGG_HDR, KeyKind, OwnerFns, OwnerTop, Rec, RecordLayout, SlotKind, ORDER_NONE, OWNERTOP_HDR, REDUCE_FLUSH, T1, T1_MAX_PARTS};
use gravity_abi::hash::KeyHash;
use gravity_exec::table::owner::{bind, bind_dense, bind_dense_plain_at, owner_fns, owner_fns_dense};
use gravity_kernels::dict::reps::{MAX_REPS, REP_NONE};
use gravity_program::{OwnerDesc, WordsDesc, ACC_PAIRS, ACC_SLOT_BASE};

/// The rig's fill → refold switch (`fold_part`): the groups a partition's first table is sized for (0: the estimate), and the refolds made.
static RETRY_EST: AtomicUsize = AtomicUsize::new(0);
static RETRIES: AtomicUsize = AtomicUsize::new(0);

struct Row {
    k: i64,
    x: i64,
    v: i64,
}

fn lcg(s: &mut u64) -> u64 {
    *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    *s >> 17
}

/// `n` rows over `span` group keys from `lo` (skewed: a third of the rows in the first eight keys), `dom` arguments.
fn rows(n: usize, lo: i64, span: u64, dom: u64, seed: u64) -> Vec<Row> {
    let mut s = seed;
    (0..n)
        .map(|_| {
            let r = lcg(&mut s);
            let k = lo + if r % 3 == 0 { (r % 8) % span } else { r % span } as i64;
            Row { k, x: (lcg(&mut s) % dom) as i64, v: (lcg(&mut s) % 1000) as i64 - 500 }
        })
        .collect()
}

/// A group as the accumulator holds it: the key, the pairs, then the riding slots as `i128`s (an average as its sum and its count).
type Group = (i64, i64, Vec<i128>);

fn oracle(rows: &[Row], slots: bool) -> Vec<Group> {
    let mut seen = std::collections::BTreeSet::new();
    let mut m: BTreeMap<i64, (i64, i64, i64, i128, i64)> = BTreeMap::new();
    for r in rows {
        let g = m.entry(r.k).or_insert((0, 0, 0, 0, 0));
        if seen.insert((r.k, r.x)) {
            g.0 += 1;
        }
        g.1 += 1;
        g.2 += r.v;
        g.3 += r.v as i128;
        g.4 += 1;
    }
    m.into_iter().map(|(k, (p, c, s, a, n))| (k, p, if slots { vec![c as i128, s as i128, a, n as i128] } else { vec![] })).collect()
}

fn read(r: Rec<'_>) -> Group {
    let mut slots = Vec::new();
    for j in ACC_SLOT_BASE as usize..r.layout.nslots() {
        match r.layout.slot_kind(j) {
            SlotKind::Count => slots.push(r.count(j) as i128),
            SlotKind::SumI64 => slots.push(r.i64(j) as i128),
            SlotKind::AvgI128 => {
                let (s, n) = r.avg(j);
                slots.push(s);
                slots.push(n as i128);
            }
            SlotKind::SumI128 => slots.push(r.i128(j)),
            SlotKind::SumF64 => slots.push(r.f64(j).to_bits() as i128),
            SlotKind::BitAnd | SlotKind::BitOr | SlotKind::BitXor => slots.push(r.i64(j) as i128),
            SlotKind::MinI64 | SlotKind::MaxI64 => slots.push(r.i64(j) as i128),
            SlotKind::MinCode | SlotKind::MaxCode => {
                let (part, code) = r.code(j);
                slots.push(((part as i128) << 32) | code as i128)
            }
            k => panic!("a slot kind the test does not read: {k:?}"),
        }
    }
    (r.key(0) as i64, r.count(ACC_PAIRS as usize), slots)
}

fn pair_layout(slots: bool) -> RecordLayout {
    if slots { RecordLayout::new_nn(&[KeyKind::I32, KeyKind::I32], &[SlotKind::Count, SlotKind::SumI64, SlotKind::AvgI128]) } else { RecordLayout::new_nn(&[KeyKind::I32, KeyKind::I32], &[]) }
}

/// The pair's riding slots filled from a row.
type Fill = fn(&Rec<'_>, &Row);

fn fill_none(_: &Rec<'_>, _: &Row) {}

fn fill_count_sum_avg(rec: &Rec<'_>, r: &Row) {
    rec.add_count(0, 1);
    rec.add_i64(1, r.v);
    rec.add_avg(2, r.v as i128, 1);
}

fn fill_of(slots: bool) -> Fill {
    if slots { fill_count_sum_avg } else { fill_none }
}

/// Every NOT NULL slot kind the fold table folds, one pair layout: `[Count, SumI64, SumI128, AvgI128, MinCode, BitOr]`
/// (six: the accumulator adds two counts to eight; a float rider leaves the dense form, D244: its merge can raise;
/// `BitAnd` rides a validity layout and `BitXor` is `pair_layout_bits`', D252).
fn pair_layout_every_kind() -> RecordLayout {
    RecordLayout::new_nn(&[KeyKind::I64, KeyKind::I64], &[SlotKind::Count, SlotKind::SumI64, SlotKind::SumI128, SlotKind::AvgI128, SlotKind::MinCode, SlotKind::BitOr])
}

/// D252's other NOT NULL bit slot: `[Count, BitXor, BitOr]`.
fn pair_layout_bits() -> RecordLayout {
    RecordLayout::new_nn(&[KeyKind::I64, KeyKind::I64], &[SlotKind::Count, SlotKind::BitXor, SlotKind::BitOr])
}

fn fill_bits(rec: &Rec<'_>, r: &Row) {
    rec.add_count(0, 1);
    rec.fold_bits(1, r.v);
    rec.fold_bits(2, r.x as i64);
}

fn fill_every_kind(rec: &Rec<'_>, r: &Row) {
    rec.add_count(0, 1);
    rec.add_i64(1, r.v);
    rec.add_i128(2, (r.v as i128) << 70);
    rec.add_avg(3, r.v as i128, 1);
    rec.min_code(4, (r.x % 7) as u32, (r.x as u32).wrapping_mul(2_654_435_761));
    rec.fold_bits(5, r.v);
}

fn desc_of(pair: RecordLayout, count: bool) -> OwnerDesc {
    let acc = OwnerDesc::accumulator_of(&pair, 1);
    if count { OwnerDesc::distinct(pair, acc, 1, 2_000, ACC_PAIRS, 10, [(REP_NONE, 0); MAX_REPS]) } else { OwnerDesc::distinct(pair, acc, 1, 2_000, ORDER_NONE, 0, [(REP_NONE, 0); MAX_REPS]) }
}

fn desc(slots: bool, count: bool) -> OwnerDesc {
    let pair = pair_layout(slots);
    let acc = OwnerDesc::accumulator_of(&pair, 1);
    if count { OwnerDesc::distinct(pair, acc, 1, 2_000, ACC_PAIRS, 10, [(REP_NONE, 0); MAX_REPS]) } else { OwnerDesc::distinct(pair, acc, 1, 2_000, ORDER_NONE, 0, [(REP_NONE, 0); MAX_REPS]) }
}

/// The two rounds over `w` owners as the driver runs them: round-1 partition `p` (the pair hash) folded into a pair
/// table and reduced by owner `p % w`, every owner's round closed, round-2 partition `q` (the group hash) folded
/// from every owner's round-2 buffers and reduced by owner `q % w`; per owner its kept records or its runs.
fn own(d: &OwnerDesc, fns: &OwnerFns, rows: &[Row], pbits: u8, w: usize, bytes: usize, fill: Fill) -> (Vec<Vec<Group>>, Vec<i64>) {
    own_by(d, fns, rows, pbits, w, bytes, fill, None)
}

/// One round-1 partition as the driver's `fold_sub` runs it: the rows' records (one per row, repeats and all)
/// through `fns.fold` in runs into a table laid at the region's table space, then the reduce; a table that fills
/// (`shrink`: the capacity estimate divided, so the first tables do) is read again as eight sub-partitions.
#[allow(clippy::too_many_arguments)]
fn fold_part(fns: &OwnerFns, pair: &RecordLayout, region: *mut u8, table_off: usize, bytes: usize, recs: &[u8], split: SubSplit, est: usize, splits: &mut usize) {
    let rec = pair.size() as usize;
    // `merge.rs`'s fill → refold: a partition's first table sized for `RETRY_EST` groups when set, read again ONCE at the same depth at the records' size
    let low = RETRY_EST.load(Ordering::Relaxed);
    let est_used = if low != 0 && split.reread == 0 && split.depth == 0 { low.min(est) } else { est };
    let cap = ((est_used * 2).next_power_of_two().max(64) as u32).min(HashAgg::cap_of(pair, (bytes - table_off) as u32));
    let t = unsafe {
        core::ptr::write_bytes(region.add(table_off), 0, HASHAGG_HDR);
        HashAgg::at(region.add(table_off), pair, cap, (bytes - table_off) as u32) as *mut HashAgg
    };
    // the table's homes read below the split's route bits, as `merge.rs fold_sub` lays it
    unsafe { (*t).set_route_bits(split.table_depth()) };
    let mut cand = Candidates::none();
    let (mut n, mut full) = (0usize, false);
    // runs of 61 records and, every third, of one (the driver's hot-table records arrive singly)
    let runs = recs.chunks(61 * rec).enumerate().flat_map(|(k, run)| if k % 3 == 2 { vec![&run[..rec], &run[rec..]] } else { vec![run] }).filter(|r| !r.is_empty());
    for run in runs {
        let v = unsafe { (fns.fold.expect("the form's fold"))(fns.params.as_ptr(), t, run.as_ptr(), run.len() / rec, split, &mut cand) };
        n += if v >= 0 { v } else { -1 - v } as usize;
        if v < 0 {
            full = true;
            break;
        }
    }
    if full {
        if est_used < est {
            RETRIES.fetch_add(1, Ordering::Relaxed);
            return fold_part(fns, pair, region, table_off, bytes, recs, SubSplit { reread: 1, ..split }, est, splits);
        }
        assert!(gravity_abi::hash::Depth::new(split.pbits as u32, split.depth as u32 + 1).is_some(), "the split ends");
        *splits += 1;
        for s in 0..8u32 {
            fold_part(fns, pair, region, table_off, bytes, recs, SubSplit { pbits: split.pbits, depth: split.depth + 1, reread: 0, sub: split.sub << 3 | s as u64 }, n / 8 + 1, splits);
        }
        return;
    }
    let rc = unsafe { (fns.reduce)(fns.params.as_ptr(), region, t as *const HashAgg, core::ptr::null_mut(), core::ptr::null(), -1) };
    assert_eq!(rc, 0, "the reduce of a folded table");
}

/// `own`; with `through_fold: Some(shrink)` round 1 goes through `fns.fold` (`fold_part`), the capacity estimate
/// divided by `shrink`.
#[allow(clippy::too_many_arguments)]
fn own_by(d: &OwnerDesc, fns: &OwnerFns, rows: &[Row], pbits: u8, w: usize, bytes: usize, fill: Fill, through_fold: Option<usize>) -> (Vec<Vec<Group>>, Vec<i64>) {
    let (pair, acc) = (d.layout, d.acc);
    let p = fns.params.as_ptr();
    assert!(bytes as u64 >= (fns.min_bytes)(p, w as u32), "the region holds one of the w owners' floor");
    let mut flushed: BTreeMap<u32, Vec<Vec<u8>>> = BTreeMap::new();
    let mut regions: Vec<Vec<u64>> = (0..w).map(|_| vec![0u64; bytes / 8]).collect();
    let mut opened = Vec::new();
    for region in regions.iter_mut() {
        let rc = unsafe { (fns.open)(p, region.as_mut_ptr() as *mut u8, bytes) };
        assert!(rc >= 0, "the owner region opens");
        opened.push(rc);
    }
    for part in 0..1u32 << pbits {
        let mine: Vec<&Row> = rows.iter().filter(|r| (KeyHash::of(&[r.k as u64, r.x as u64])).part(pbits as u32) == part).collect();
        if mine.is_empty() {
            continue;
        }
        if let Some(shrink) = through_fold {
            let rec = pair.size() as usize;
            let mut recs = vec![0u8; mine.len() * rec];
            for (i, r) in mine.iter().enumerate() {
                let q = unsafe { Rec::at(recs.as_mut_ptr().add(i * rec), &pair) };
                q.open(&[r.k as u64, r.x as u64]);
                fill(&q, r);
            }
            let o = part as usize % w;
            let mut splits = 0;
            fold_part(fns, &pair, regions[o].as_mut_ptr() as *mut u8, opened[o] as usize, bytes, &recs, SubSplit { pbits, depth: 0, reread: 0, sub: 0 }, mine.len() / shrink, &mut splits);
            assert_eq!(splits > 0, shrink > 8, "partition {part}: a shrunk estimate fills the first table");
            continue;
        }
        let cap = (mine.len() * 2).max(64).next_power_of_two() as u32;
        let mut tb = vec![0u64; HashAgg::bytes(&pair, cap) as usize / 8];
        let t = unsafe { HashAgg::at(tb.as_mut_ptr() as *mut u8, &pair, cap, HashAgg::bytes(&pair, cap)) };
        for r in mine {
            let rec = t.find_or_insert(&[r.k as u64, r.x as u64]).expect("room");
            fill(&rec, r);
        }
        let region = regions[part as usize % w].as_mut_ptr() as *mut u8;
        let mut rc = unsafe { (fns.reduce)(p, region, t as *const HashAgg, core::ptr::null_mut(), core::ptr::null(), -1) };
        // a full round-2 pool: the driver writes the buffers out and the reduce resumes (D584)
        while rc == REDUCE_FLUSH {
            flush_round2(region, &acc, &mut flushed);
            rc = unsafe { (fns.reduce)(p, region, t as *const HashAgg, core::ptr::null_mut(), core::ptr::null(), -1) };
        }
        assert_eq!(rc, 0, "partition {part}");
    }
    for region in regions.iter_mut() {
        let mut rc = unsafe { (fns.end_round)(p, region.as_mut_ptr() as *mut u8) };
        while rc == REDUCE_FLUSH {
            flush_round2(region.as_mut_ptr() as *mut u8, &acc, &mut flushed);
            rc = unsafe { (fns.end_round)(p, region.as_mut_ptr() as *mut u8) };
        }
        assert_eq!(rc, 0, "the round closes");
    }
    // the owners' round-2 buffers by partition (the flushed ones first)
    let mut buffered: BTreeMap<u32, Vec<Vec<u8>>> = flushed;
    let mut pbits2 = 0;
    for region in regions.iter() {
        let h = unsafe { OwnerTop::view(region.as_ptr() as *const u8) };
        let r2 = unsafe { T1::view((region.as_ptr() as *const u8).add(h.round2_off as usize)) };
        pbits2 = r2.pbits;
        for r in r2.records() {
            buffered.entry(r.route().part(r2.pbits as u32)).or_default().push(r.bytes().to_vec());
        }
        for q in 0..r2.parts() as u32 {
            for b in r2.chain(q) {
                for i in 0..b.recs {
                    buffered.entry(q).or_default().push(unsafe { Rec::at(b.ptr.add(i * acc.size() as usize) as *mut u8, &acc) }.bytes().to_vec());
                }
            }
        }
    }
    for q in 0..1u32 << pbits2 {
        let Some(recs) = buffered.get(&q) else { continue };
        let cap = (recs.len() * 2).max(64).next_power_of_two() as u32;
        let mut tb = vec![0u64; HashAgg::bytes(&acc, cap) as usize / 8];
        let t = unsafe { HashAgg::at(tb.as_mut_ptr() as *mut u8, &acc, cap, HashAgg::bytes(&acc, cap)) };
        for r in recs {
            let src = unsafe { Rec::at(r.as_ptr() as *mut u8, &acc) };
            t.find_or_insert(src.keys()).expect("room").fold(&src).expect("no float overflow");
        }
        let region = regions[q as usize % w].as_mut_ptr() as *mut u8;
        assert!(unsafe { (fns.reduce2)(p, region, t as *const HashAgg, core::ptr::null_mut(), core::ptr::null(), -1) } >= 0, "the round-2 reduce");
    }
    let mut out = Vec::new();
    for region in regions.iter() {
        let base = region.as_ptr() as *const u8;
        let h = unsafe { OwnerTop::view(base) };
        let mut groups = Vec::new();
        if d.topk > 0 {
            for i in 0..h.len as usize {
                groups.push(read(unsafe { Rec::at(base.add(OWNERTOP_HDR + i * acc.size() as usize) as *mut u8, &acc) }));
            }
        } else {
            let dir = unsafe { base.add(OWNERTOP_HDR) as *const u32 };
            for run in 0..h.worst as usize {
                let (off, n) = unsafe { (*dir.add(2 * run) as usize, *dir.add(2 * run + 1) as usize) };
                for i in 0..n {
                    groups.push(read(unsafe { Rec::at(base.add(off + i * acc.size() as usize) as *mut u8, &acc) }));
                }
            }
            assert_eq!(groups.len(), h.len as usize);
        }
        groups.sort();
        out.push(groups);
    }
    let _ = T1_MAX_PARTS;
    (out, opened)
}

static FLUSHES: AtomicUsize = AtomicUsize::new(0);

/// The driver's flush of an owner's round-2 pool: every chained record of it to `out` by partition, the pool emptied.
fn flush_round2(region: *mut u8, acc: &RecordLayout, out: &mut BTreeMap<u32, Vec<Vec<u8>>>) {
    FLUSHES.fetch_add(1, Ordering::Relaxed);
    let h = unsafe { OwnerTop::view(region as *const u8) };
    let r2 = unsafe { region.add(h.round2_off as usize) as *mut T1 };
    let t = unsafe { &mut *r2 };
    for q in 0..t.parts() as u32 {
        for b in t.chain(q) {
            for i in 0..b.recs {
                out.entry(q).or_default().push(unsafe { Rec::at(b.ptr.add(i * acc.size() as usize) as *mut u8, acc) }.bytes().to_vec());
            }
        }
    }
    t.reset_pool();
}

/// D584: a region with a domain the area does not fit takes the pair path, whose reduce flushes a full round-2 pool
/// and resumes by `OwnerTop.resume`; the resumed reduce stays the pair path (the area was found at the resume index,
/// its pairs noted into the table space: counts past the oracle's).
#[test]
fn d584_a_flushed_pair_path_reduce_resumes_as_the_pair_path_under_a_domain() {
    let (lo, span) = (0i64, 60_000u64);
    let data = rows(60_000, lo, span, 40_000, 23);
    let d = desc(false, true);
    // a domain past D586's floor cap: the floor region lays no area
    let dense = owner_fns_dense(&d, 3, Some((lo, lo + (1 << 22) - 1)));
    let pairs = owner_fns(&d, 3);
    let _ = span;
    let bytes = (1usize << 20).max((dense.min_bytes)(dense.params.as_ptr(), 1) as usize);
    FLUSHES.store(0, Ordering::Relaxed);
    let (a, opened_a) = own(&d, &dense, &data, 3, 1, bytes, fill_none);
    assert!(FLUSHES.load(Ordering::Relaxed) > 0, "the round-2 pool filled");
    let (b, opened_b) = own(&d, &pairs, &data, 3, 1, bytes, fill_none);
    assert_eq!(opened_a, opened_b, "no area: the pair path's split");
    assert_eq!(a, b);
    let union: Vec<Group> = a.into_iter().flatten().collect();
    assert_eq!(best(&union, 10), best(&oracle(&data, false), 10));
}

/// D586: a region of the dense binding's floor (`min_bytes`, which own_all's one owner takes) lays the area, in both forms,
/// and the groups there are the pair path's; a domain past the floor's cap keeps the plain floor.
#[test]
fn d586_the_floor_region_lays_the_dense_area() {
    let (lo, span) = (-50i64, 131_071u64);
    let data = rows(30_000, lo, 2_000, 5_000, 29);
    for count in [false, true] {
        let d = desc(false, count);
        let dense = owner_fns_dense(&d, 3, Some((lo, lo + span as i64 - 1)));
        let pairs = owner_fns(&d, 3);
        let floor = (dense.min_bytes)(dense.params.as_ptr(), 1) as usize;
        assert_ne!(fold_shape(&dense, floor), 0, "count {count}: the floor region lays the area");
        let (a, _) = own(&d, &dense, &data, 3, 1, floor, fill_none);
        let (b, _) = own(&d, &pairs, &data, 3, 1, floor.max((pairs.min_bytes)(pairs.params.as_ptr(), 1) as usize), fill_none);
        assert_eq!(a, b, "count {count}: the floor region's groups");
        let wide = owner_fns_dense(&d, 3, Some((0, (1 << 22) - 1)));
        assert_eq!((wide.min_bytes)(wide.params.as_ptr(), 1), (pairs.min_bytes)(pairs.params.as_ptr(), 1), "count {count}: an area past the cap keeps the floor");
    }
}

/// KEYED-28: one region of the floor for `w` owners (`min_bytes(params, w)`: its share of the planned groups, which round 2
/// re-sinks to it by the group hash) holds a right estimate's groups at every width, under one owner's floor.
#[test]
fn keyed28_the_floor_for_w_owners_holds_their_share_of_a_right_estimate() {
    let data = rows(200_000, 0, 20_000, 50, 41);
    let all = oracle(&data, false);
    let pair = pair_layout(false);
    let d = OwnerDesc::distinct(pair, OwnerDesc::accumulator_of(&pair, 1), 1, all.len() as u32, ORDER_NONE, 0, [(REP_NONE, 0); MAX_REPS]);
    let fns = owner_fns(&d, 3);
    let one = (fns.min_bytes)(fns.params.as_ptr(), 1);
    for w in [2usize, 4, 8] {
        let floor = (fns.min_bytes)(fns.params.as_ptr(), w as u32);
        assert!(floor < one, "width {w}: {floor} under one owner's {one}");
        let (a, _) = own(&d, &fns, &data, 3, w, floor as usize, fill_none);
        let mut u: Vec<Group> = a.into_iter().flatten().collect();
        u.sort();
        assert_eq!(u, all, "width {w}: every group once");
    }
}

fn best(groups: &[Group], n: usize) -> Vec<Group> {
    let mut v = groups.to_vec();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v.truncate(n);
    v
}

#[test]
fn the_dense_round_gives_the_pair_paths_groups_in_every_form_shape_and_width() {
    let (lo, span) = (-50i64, 2_000u64);
    let data = rows(60_000, lo, span, 5_000, 11);
    for slots in [false, true] {
        let all = oracle(&data, slots);
        for count in [false, true] {
            let d = desc(slots, count);
            let dense = owner_fns_dense(&d, 3, Some((lo, lo + span as i64 - 1)));
            let pairs = owner_fns(&d, 3);
            for w in [1usize, 2, 3, 8] {
                for pbits in [3u8, 5] {
                    let (a, opened_a) = own(&d, &dense, &data, pbits, w, 32 << 20, fill_of(slots));
                    let (b, opened_b) = own(&d, &pairs, &data, pbits, w, 32 << 20, fill_of(slots));
                    assert!(opened_a[0] < opened_b[0], "slots {slots} count {count}: the dense area is laid before the table space");
                    assert_eq!(a, b, "slots {slots} count {count} width {w} pbits {pbits}: per owner");
                    let union: Vec<Group> = a.iter().flatten().cloned().collect();
                    if count {
                        assert!(union.len() >= 10 && a.iter().all(|o| o.len() <= 10), "slots {slots} width {w}: the kept sets");
                        assert_eq!(best(&union, 10), best(&all, 10), "slots {slots} width {w} pbits {pbits}: the best ten");
                    } else {
                        let mut u = union;
                        u.sort();
                        assert_eq!(u, all, "slots {slots} width {w} pbits {pbits}: every group once");
                    }
                }
            }
        }
    }
}

/// A date or a timestamp group key (a signed word of days / microseconds) takes the dense round 1 as an int key
/// does, below zero too, and its groups are the pair path's and the oracle's at every width.
#[test]
fn a_date_or_timestamp_group_key_takes_the_dense_round_as_an_int_key() {
    let (lo, span) = (-50i64, 2_000u64);
    let data = rows(20_000, lo, span, 5_000, 13);
    let all = oracle(&data, false);
    for kind in [KeyKind::Date, KeyKind::Timestamp] {
        let d = desc_of(RecordLayout::new_nn(&[kind, KeyKind::I32], &[]), false);
        let dense = owner_fns_dense(&d, 3, Some((lo, lo + span as i64 - 1)));
        let pairs = owner_fns(&d, 3);
        for w in [1usize, 3] {
            let (a, opened_a) = own(&d, &dense, &data, 3, w, 32 << 20, fill_none);
            let (b, opened_b) = own(&d, &pairs, &data, 3, w, 32 << 20, fill_none);
            assert!(opened_a[0] < opened_b[0], "{kind:?} width {w}: the dense area is laid before the table space");
            assert_eq!(a, b, "{kind:?} width {w}: per owner");
            let mut u: Vec<Group> = a.iter().flatten().cloned().collect();
            u.sort();
            assert_eq!(u, all, "{kind:?} width {w}: every group once");
        }
    }
}

/// The fold's shape word of a region opened by `fns` (the marker before the table space names the area; the
/// area header's fourth word): 0 when the reduce counts.
fn fold_shape(fns: &OwnerFns, bytes: usize) -> u32 {
    let mut region = vec![0u64; bytes / 8];
    let table_off = unsafe { (fns.open)(fns.params.as_ptr(), region.as_mut_ptr() as *mut u8, bytes) } as usize;
    let words = unsafe { core::slice::from_raw_parts(region.as_ptr() as *const u32, bytes / 4) };
    match words[table_off / 4 - 1] as usize {
        0 => 0,
        back => words[(table_off - back) / 4 + 3] & 0x7fff_ffff,
    }
}

/// The count inside the fold (lower.md §6.12.7): round 1 through `fns.fold` as the driver runs it — the dense
/// binding's fold counts the int-pair key shapes (both slot orders) into the area and its reduce walks nothing;
/// the groups are the pair path's and the oracle's at every width, both forms, with tables that fill and split
/// (the re-read with the quiet prefix), and a layout without a dense shape keeps the reduce's count behind the same fold.
#[test]
fn the_fold_counts_into_the_area_as_the_pair_path_and_the_oracle_do() {
    let (lo, span) = (-50i64, 2_000u64);
    let data = rows(20_000, lo, span, 5_000, 29);
    let sum_count_avg_lowered = RecordLayout::new_nn(&[KeyKind::I32, KeyKind::I32], &[SlotKind::SumI64, SlotKind::Count, SlotKind::AvgI128]);
    let fill_lowered: Fill = |rec, r| {
        rec.add_i64(0, r.v);
        rec.add_count(1, 1);
        rec.add_avg(2, r.v as i128, 1);
    };
    let shapes: [(RecordLayout, Fill, bool); 5] = [(pair_layout(false), fill_none, true), (pair_layout(true), fill_count_sum_avg, true), (sum_count_avg_lowered, fill_lowered, true), (pair_layout_every_kind(), fill_every_kind, false), (pair_layout_bits(), fill_bits, false)];
    for (si, (pair, fill, shaped)) in shapes.into_iter().enumerate() {
        assert_eq!(gravity_kernels::table::owner_fold::dense_folds(&pair), shaped, "shape {si}");
        for count in [false, true] {
            let d = desc_of(pair, count);
            let dense = owner_fns_dense(&d, 3, Some((lo, lo + span as i64 - 1)));
            let pairs = owner_fns(&d, 3);
            assert_eq!(fold_shape(&dense, 32 << 20) != 0, shaped, "shape {si}: the fold counts a shaped layout");
            for w in [1usize, 2, 3, 8] {
                let (b, _) = own(&d, &pairs, &data, 3, w, 32 << 20, fill);
                for shrink in [1usize, 64] {
                    let (a, _) = own_by(&d, &dense, &data, 3, w, 32 << 20, fill, Some(shrink));
                    assert_eq!(a, b, "shape {si} count {count} width {w} shrink {shrink}: per owner");
                }
                if si == 1 && !count {
                    let mut u: Vec<Group> = b.into_iter().flatten().collect();
                    u.sort();
                    assert_eq!(u, oracle(&data, true), "width {w}: every group once");
                }
            }
        }
    }
}

/// A key outside the domain met by the fold is refused by the reduce, by name.
#[test]
fn a_key_outside_the_domain_met_by_the_fold_is_refused() {
    let data = rows(1_000, 0, 100, 50, 3);
    let d = desc(false, false);
    let fns = owner_fns_dense(&d, 3, Some((0, 49)));
    let bytes = 8 << 20;
    let mut region = vec![0u64; bytes / 8];
    let p = fns.params.as_ptr();
    let table_off = unsafe { (fns.open)(p, region.as_mut_ptr() as *mut u8, bytes) };
    assert!(table_off >= 0);
    let pair = d.layout;
    let rec = pair.size() as usize;
    let mut recs = vec![0u8; data.len() * rec];
    for (i, r) in data.iter().enumerate() {
        unsafe { Rec::at(recs.as_mut_ptr().add(i * rec), &pair) }.open(&[r.k as u64, r.x as u64]);
    }
    let base = region.as_mut_ptr() as *mut u8;
    let t = unsafe { HashAgg::at(base.add(table_off as usize), &pair, 4096, (bytes - table_off as usize) as u32) as *mut HashAgg };
    let mut cand = Candidates::none();
    assert_eq!(unsafe { (fns.fold.unwrap())(p, t, recs.as_ptr(), data.len(), SubSplit::NONE, &mut cand) }, data.len() as i64);
    assert!(unsafe { (fns.reduce)(p, base, t as *const HashAgg, core::ptr::null_mut(), core::ptr::null(), -1) } < 0);
}

/// The fold table (lower.md §6.12.5 need 1) folds every NOT NULL slot kind as the pair path's generic fold does,
/// at every width and both forms; the sums are checked against the rows for one group.
#[test]
fn the_fold_table_folds_every_slot_kind_as_the_pair_path_does_at_every_width() {
    let (lo, span) = (100i64, 700u64);
    let data = rows(40_000, lo, span, 900, 23);
    for count in [false, true] {
        let d = desc_of(pair_layout_every_kind(), count);
        let dense = owner_fns_dense(&d, 3, Some((lo, lo + span as i64 - 1)));
        let pairs = owner_fns(&d, 3);
        for w in [1usize, 2, 3, 8] {
            let (a, _) = own(&d, &dense, &data, 4, w, 32 << 20, fill_every_kind);
            let (b, _) = own(&d, &pairs, &data, 4, w, 32 << 20, fill_every_kind);
            assert_eq!(a, b, "count {count} width {w}: per owner");
            let union: Vec<Group> = a.iter().flatten().cloned().collect();
            let g = union.iter().find(|g| g.0 == lo).expect("the first key is a group");
            let mine: Vec<&Row> = data.iter().filter(|r| r.k == lo).collect();
            let mut seen = std::collections::BTreeSet::new();
            let pairs_of = mine.iter().filter(|r| seen.insert(r.x)).count() as i64;
            assert_eq!(g.1, pairs_of, "count {count} width {w}: the pairs");
            let sum: i64 = mine.iter().map(|r| r.v).sum();
            assert_eq!(g.2[1], sum as i128, "the i64 sum");
            assert_eq!(g.2[2], (sum as i128) << 70, "the i128 sum");
            assert_eq!((g.2[3], g.2[4]), (sum as i128, mine.len() as i128), "the average");
            let code = mine.iter().map(|r| ((r.x % 7) as u64) << 32 | (r.x as u32).wrapping_mul(2_654_435_761) as u64).min().unwrap();
            assert_eq!(g.2[5], code as i128, "the least code");
            assert_eq!(g.2[6], mine.iter().fold(0, |a, r| a | r.v) as i128, "bit_or");
            if !count {
                assert_eq!(union.len(), span as usize, "every group once");
            }
        }
    }
}

/// A region whose table space could not keep its floor beside the area opens on the pair path — the same table
/// space as the pair binding's and the same groups.
#[test]
fn a_region_too_small_for_the_area_takes_the_pair_path() {
    let (lo, span) = (0i64, 60_000u64);
    let data = rows(20_000, lo, span, 3_000, 5);
    let d = desc(true, false);
    // D586: a domain whose area the floor grows for is always laid (the floor region holds it); a domain past the floor's
    // cap is what a small region still meets
    let wide = owner_fns_dense(&d, 3, Some((lo, lo + (1 << 22) - 1)));
    let dense = owner_fns_dense(&d, 3, Some((lo, lo + span as i64 - 1)));
    let pairs = owner_fns(&d, 3);
    let (a, opened_a) = own(&d, &wide, &data, 3, 2, 2 << 20, fill_count_sum_avg);
    let (b, opened_b) = own(&d, &pairs, &data, 3, 2, 2 << 20, fill_count_sum_avg);
    assert_eq!(opened_a, opened_b, "the pair path's split");
    assert_eq!(a, b);
    let mut u: Vec<Group> = a.into_iter().flatten().collect();
    u.sort();
    assert_eq!(u, oracle(&data, true));
    // the same data with room for the area
    let (c, opened_c) = own(&d, &dense, &data, 3, 2, 32 << 20, fill_count_sum_avg);
    let (e, opened_e) = own(&d, &pairs, &data, 3, 2, 32 << 20, fill_count_sum_avg);
    assert!(opened_c[0] < opened_e[0], "the area is laid before the table space");
    assert_eq!(c, e);
}

/// A group key outside the declared domain is an error by name, not a wrong count.
#[test]
fn a_key_outside_the_domain_is_refused() {
    let data = rows(1_000, 0, 100, 50, 3);
    let d = desc(false, false);
    let fns = owner_fns_dense(&d, 3, Some((0, 49)));
    let bytes = 8 << 20;
    let mut region = vec![0u64; bytes / 8];
    let p = fns.params.as_ptr();
    assert!(unsafe { (fns.open)(p, region.as_mut_ptr() as *mut u8, bytes) } >= 0);
    let pair = d.layout;
    let mut tb = vec![0u64; HashAgg::bytes(&pair, 4096) as usize / 8];
    let t = unsafe { HashAgg::at(tb.as_mut_ptr() as *mut u8, &pair, 4096, HashAgg::bytes(&pair, 4096)) };
    for r in &data {
        t.find_or_insert(&[r.k as u64, r.x as u64]).expect("room");
    }
    assert!(unsafe { (fns.reduce)(p, region.as_mut_ptr() as *mut u8, t as *const HashAgg, core::ptr::null_mut(), core::ptr::null(), -1) } < 0);
}

/// D244: a float rider (`sum(float8)` beside a `count(DISTINCT)`) leaves the dense form for the pair path, whose merge
/// raises PG's `float8_pl` overflow: two pairs of one group whose riders are finite but sum past f64 reduce to the
/// 22003 status, never to an infinite sum.
#[test]
fn a_float_rider_takes_the_pair_path_and_its_merge_overflow_is_raised() {
    let pair = RecordLayout::new_nn(&[KeyKind::I64, KeyKind::I64], &[SlotKind::Count, SlotKind::SumF64]);
    let d = desc_of(pair, false);
    let table_off = |fns: &OwnerFns| {
        let bytes = 16 << 20;
        let mut region = vec![0u64; bytes / 8];
        unsafe { (fns.open)(fns.params.as_ptr(), region.as_mut_ptr() as *mut u8, bytes) }
    };
    assert_eq!(table_off(&owner_fns_dense(&d, 3, Some((0, 99)))), table_off(&owner_fns(&d, 3)), "no dense area under a float rider");
    let fns = owner_fns_dense(&d, 3, Some((0, 99)));
    let bytes = 8 << 20;
    let mut region = vec![0u64; bytes / 8];
    let p = fns.params.as_ptr();
    assert!(unsafe { (fns.open)(p, region.as_mut_ptr() as *mut u8, bytes) } >= 0);
    let mut tb = vec![0u64; HashAgg::bytes(&pair, 64) as usize / 8];
    let t = unsafe { HashAgg::at(tb.as_mut_ptr() as *mut u8, &pair, 64, HashAgg::bytes(&pair, 64)) };
    for x in [1u64, 2] {
        let r = t.find_or_insert(&[7, x]).expect("room");
        r.add_count(0, 1);
        r.add_f64(1, 1.5e308).expect("one finite value");
    }
    let v = unsafe { (fns.reduce)(p, region.as_mut_ptr() as *mut u8, t as *const HashAgg, core::ptr::null_mut(), core::ptr::null(), -1) };
    assert_eq!(v, gravity_abi::breaker::FloatOverflow::STATUS, "the merge's overflow is the reduce's status");
}

/// The forms the dense round does not take keep the pair path: a text-keyed or nullable layout, a domain past the dense
/// word's `SPAN_MAX`, no domain (`lo > hi`); the bind reads the domain from the declared words.
#[test]
fn the_bind_reads_the_domain_from_the_declared_words() {
    let d = desc(true, true).dense_key(1);
    let words_desc = WordsDesc { topk: WordsDesc::NONE, dense_lo: 1, dense_hi: 2, bits: WordsDesc::NONE, order: WordsDesc::NONE, nparams: 5 };
    let table_off = |fns: &OwnerFns| {
        let bytes = 16 << 20;
        let mut region = vec![0u64; bytes / 8];
        unsafe { (fns.open)(fns.params.as_ptr(), region.as_mut_ptr() as *mut u8, bytes) }
    };
    let plain = table_off(&bind(&d, &mut [0u64; 5], 3, None));
    let mut words = [0u64, 10, 1_009, 0, 0];
    assert!(table_off(&bind_dense(&d, &mut words, 3, None, Some(words_desc))) < plain, "a domain at the declared words");
    let mut words = [0u64, 1, 0, 0, 0];
    assert_eq!(table_off(&bind_dense(&d, &mut words, 3, None, Some(words_desc))), plain, "lo > hi: no domain");
    let mut words = [0u64, 10, 1_009, 0, 0];
    assert_eq!(table_off(&bind_dense(&d, &mut words, 3, None, Some(WordsDesc::bits_only(3, 5)))), plain, "no dense words declared");
    let mut words = [0u64, 10, 1_009, 0, 0];
    assert_eq!(table_off(&bind_dense(&desc(true, true), &mut words, 3, None, Some(words_desc))), plain, "no key named: the runner's default extrema are not this form's domain");
    assert_eq!(table_off(&owner_fns_dense(&d, 3, Some((0, gravity_kernels::ops::sort::topn::t3_dense::SPAN_MAX as i64)))), plain, "a span past the word's SPAN_MAX");
    // B04: the census's rows bound the area's folds (program cost `dense_pays`): a domain the rows do not fill is not bound,
    // so the owner takes the pair path as with no domain; a row a key binds it
    let mut words = [0u64, 10, 1_009, 0, 0];
    let (f, g) = bind_dense_plain_at(&d, &mut words, 3, None, Some(words_desc), true, Some(0));
    assert!(table_off(&f) == plain && g.is_none(), "no rows: no domain bound");
    let mut words = [0u64, 10, 1_009, 0, 0];
    let (f, g) = bind_dense_plain_at(&d, &mut words, 3, None, Some(words_desc), true, Some(1_000));
    assert!(table_off(&f) < plain && g.is_some(), "a row a key: the domain bound");
    // a nullable pair layout is not the form
    let pair = RecordLayout::new(&[KeyKind::I32, KeyKind::I32], &[SlotKind::Count, SlotKind::SumI64]);
    let nullable = OwnerDesc::distinct(pair, OwnerDesc::accumulator_of(&pair, 1), 1, 100, ORDER_NONE, 0, [(REP_NONE, 0); MAX_REPS]);
    assert_eq!(table_off(&owner_fns_dense(&nullable, 3, Some((0, 99)))), table_off(&owner_fns(&nullable, 3)));
}

/// A pair as the phase-1 loop leaves it for M5 §4.5's forms: the key words (the flag word last) and the riding count.
struct Flagged {
    keys: Vec<u64>,
    rows: i64,
}

/// `own_by` over explicit pairs: round 1 by the pair hash over every key word, round 2 by the group hash; per owner its kept records or runs.
fn own_flagged(d: &OwnerDesc, fns: &OwnerFns, pairs: &[Flagged], pbits: u8, w: usize, bytes: usize) -> Vec<Vec<Group>> {
    let (pair, acc) = (d.layout, d.acc);
    let p = fns.params.as_ptr();
    assert!(bytes as u64 >= (fns.min_bytes)(p, 1), "the region holds the floor");
    let mut regions: Vec<Vec<u64>> = (0..w).map(|_| vec![0u64; bytes / 8]).collect();
    for region in regions.iter_mut() {
        assert!(unsafe { (fns.open)(p, region.as_mut_ptr() as *mut u8, bytes) } >= 0, "the owner region opens");
    }
    for part in 0..1u32 << pbits {
        let mine: Vec<&Flagged> = pairs.iter().filter(|r| KeyHash::of(&r.keys).part(pbits as u32) == part).collect();
        if mine.is_empty() {
            continue;
        }
        let cap = (mine.len() * 2).max(64).next_power_of_two() as u32;
        let mut tb = vec![0u64; HashAgg::bytes(&pair, cap) as usize / 8];
        let t = unsafe { HashAgg::at(tb.as_mut_ptr() as *mut u8, &pair, cap, HashAgg::bytes(&pair, cap)) };
        for r in mine {
            t.find_or_insert(&r.keys).expect("room").add_count(0, r.rows);
        }
        let region = regions[part as usize % w].as_mut_ptr() as *mut u8;
        assert_eq!(unsafe { (fns.reduce)(p, region, t as *const HashAgg, core::ptr::null_mut(), core::ptr::null(), -1) }, 0, "partition {part}");
    }
    for region in regions.iter_mut() {
        assert_eq!(unsafe { (fns.end_round)(p, region.as_mut_ptr() as *mut u8) }, 0, "the round closes");
    }
    let mut buffered: BTreeMap<u32, Vec<Vec<u8>>> = BTreeMap::new();
    let mut pbits2 = 0;
    for region in regions.iter() {
        let h = unsafe { OwnerTop::view(region.as_ptr() as *const u8) };
        let r2 = unsafe { T1::view((region.as_ptr() as *const u8).add(h.round2_off as usize)) };
        pbits2 = r2.pbits;
        for r in r2.records() {
            buffered.entry(r.route().part(r2.pbits as u32)).or_default().push(r.bytes().to_vec());
        }
        for q in 0..r2.parts() as u32 {
            for b in r2.chain(q) {
                for i in 0..b.recs {
                    buffered.entry(q).or_default().push(unsafe { Rec::at(b.ptr.add(i * acc.size() as usize) as *mut u8, &acc) }.bytes().to_vec());
                }
            }
        }
    }
    for q in 0..1u32 << pbits2 {
        let Some(recs) = buffered.get(&q) else { continue };
        let cap = (recs.len() * 2).max(64).next_power_of_two() as u32;
        let mut tb = vec![0u64; HashAgg::bytes(&acc, cap) as usize / 8];
        let t = unsafe { HashAgg::at(tb.as_mut_ptr() as *mut u8, &acc, cap, HashAgg::bytes(&acc, cap)) };
        for r in recs {
            let src = unsafe { Rec::at(r.as_ptr() as *mut u8, &acc) };
            t.find_or_insert(src.keys()).expect("room").fold(&src).expect("no float overflow");
        }
        let region = regions[q as usize % w].as_mut_ptr() as *mut u8;
        assert!(unsafe { (fns.reduce2)(p, region, t as *const HashAgg, core::ptr::null_mut(), core::ptr::null(), -1) } >= 0, "the round-2 reduce");
    }
    let mut out = Vec::new();
    for region in regions.iter() {
        let base = region.as_ptr() as *const u8;
        let h = unsafe { OwnerTop::view(base) };
        let mut groups = Vec::new();
        if d.topk > 0 {
            for i in 0..h.len as usize {
                groups.push(read(unsafe { Rec::at(base.add(OWNERTOP_HDR + i * acc.size() as usize) as *mut u8, &acc) }));
            }
        } else {
            let dir = unsafe { base.add(OWNERTOP_HDR) as *const u32 };
            for run in 0..h.worst as usize {
                let (off, n) = unsafe { (*dir.add(2 * run) as usize, *dir.add(2 * run + 1) as usize) };
                for i in 0..n {
                    groups.push(read(unsafe { Rec::at(base.add(off + i * acc.size() as usize) as *mut u8, &acc) }));
                }
            }
        }
        groups.sort();
        out.push(groups);
    }
    out
}

/// M5 §4.5 (`OwnerDesc::null_arg` / `arg_fold`) on the two-round owner, the pair path and the dense round 1 alike: a flagged
/// (NULL-argument) pair folds its riders and adds nothing to the pair count; `sum(DISTINCT x)` and `max(DISTINCT x)` fold the
/// argument word into the widened accumulator once per new pair — the count and the run forms at several widths, against the oracle.
#[test]
fn a_flagged_pair_counts_nothing_and_the_argument_folds_once_on_both_round_1_paths() {
    let (lo, span) = (-20i64, 400u64);
    let pair = RecordLayout::new_nn(&[KeyKind::I64, KeyKind::I64, KeyKind::I64], &[SlotKind::Count]);
    // the NOT NULL accumulator takes the dense round 1; a max needs the validity word, so its accumulator is the pair path's
    let accs = [RecordLayout::new_nn(&[KeyKind::I64], &[SlotKind::Count, SlotKind::Count, SlotKind::Count, SlotKind::SumI64]), RecordLayout::new(&[KeyKind::I64], &[SlotKind::Count, SlotKind::Count, SlotKind::Count, SlotKind::SumI64, SlotKind::MaxI64])];
    let data = rows(30_000, lo, span, 900, 17);
    let pairs: Vec<Flagged> = data.iter().map(|r| if r.x % 6 == 0 { Flagged { keys: vec![r.k as u64, 0, 1 << 1], rows: 1 } } else { Flagged { keys: vec![r.k as u64, r.x as u64, 0], rows: 1 } }).collect();
    let mut want: BTreeMap<i64, (i64, i64, i64, i64)> = BTreeMap::new();
    let mut seen = std::collections::BTreeSet::new();
    for r in data.iter() {
        let e = want.entry(r.k).or_insert((0, 0, 0, i64::MIN));
        e.1 += 1;
        if r.x % 6 != 0 && seen.insert((r.k, r.x)) {
            e.0 += 1;
            e.2 += r.x;
            e.3 = e.3.max(r.x);
        }
    }
    for (acc, with_max) in accs.into_iter().zip([false, true]) {
        let all: Vec<Group> = want.iter().map(|(k, (p, c, s, m))| (*k, *p, if with_max { vec![*c as i128, *s as i128, *m as i128] } else { vec![*c as i128, *s as i128] })).collect();
        for count in [false, true] {
            let mut d = if count { OwnerDesc::distinct(pair, acc, 2, span as u32, ACC_PAIRS, 7, [(REP_NONE, 0); MAX_REPS]) } else { OwnerDesc::distinct(pair, acc, 2, span as u32, ORDER_NONE, 0, [(REP_NONE, 0); MAX_REPS]) }.null_arg(2, 1).arg_fold(3);
            if with_max {
                d = d.arg_fold(4);
            }
            let dense = owner_fns_dense(&d, 3, Some((lo, lo + span as i64 - 1)));
            let plain = owner_fns(&d, 3);
            assert_eq!(fold_shape(&dense, 8 << 20), 0, "a flagged or folded argument leaves the count to the reduce");
            for w in [1usize, 3] {
                let a = own_flagged(&d, &dense, &pairs, 4, w, 8 << 20);
                let b = own_flagged(&d, &plain, &pairs, 4, w, 8 << 20);
                assert_eq!(a, b, "max {with_max} count {count} width {w}: the dense round and the pair path agree");
                let union: Vec<Group> = a.into_iter().flatten().collect();
                if count {
                    assert_eq!(best(&union, 7), best(&all, 7), "max {with_max} width {w}: the best seven");
                } else {
                    let mut u = union;
                    u.sort();
                    assert_eq!(u, all, "max {with_max} width {w}: every group once, the NULL pair uncounted, the argument folded once");
                }
            }
        }
    }
}

/// D500 (`OwnerDesc::group_flags`): a nullable group key beside count(DISTINCT). The pair image is `[k, x, flags]` (bit 0: k
/// NULL, its image 0; bit 1: x NULL); the accumulator keys are k and the flags word with x's bit cleared, so the NULL group and
/// the 0 group stay two groups, a NULL argument counts no pair in either, the riders count every row and `sum(DISTINCT x)`
/// folds once per pair — the plain and the dense-domain bindings alike (the form never takes the dense round), at widths 1 and 3.
#[test]
fn d500_group_flags_keep_the_null_group_apart_from_the_zero_group() {
    let pair = RecordLayout::new_nn(&[KeyKind::I64, KeyKind::I64, KeyKind::I64], &[SlotKind::Count]);
    let acc = RecordLayout::new(&[KeyKind::I64, KeyKind::I64], &[SlotKind::Count, SlotKind::Count, SlotKind::Count, SlotKind::SumI64]);
    let (mut pairs, mut want, mut seen) = (Vec::new(), BTreeMap::new(), std::collections::BTreeSet::new());
    for i in 0..20_000i64 {
        let (kn, xn) = (i % 9 == 0, i % 5 == 0);
        let (k, x) = (if kn { 0 } else { i % 7 - 3 }, if xn { 0 } else { (i * 13) % 40 });
        pairs.push(Flagged { keys: vec![k as u64, x as u64, kn as u64 | (xn as u64) << 1], rows: 1 });
        let e: &mut (i64, i64, i64) = want.entry((k, kn)).or_insert((0, 0, 0));
        e.1 += 1;
        if !xn && seen.insert((k, kn, x)) {
            e.0 += 1;
            e.2 += x;
        }
    }
    let mut all: Vec<Group> = want.iter().map(|(&(k, _), &(p, c, s))| (k, p, vec![c as i128, s as i128])).collect();
    all.sort();
    assert_eq!(all.iter().filter(|g| g.0 == 0).count(), 2, "the oracle holds the NULL group and the 0 group");
    let d = OwnerDesc::distinct(pair, acc, 1, 16, ORDER_NONE, 0, [(REP_NONE, 0); MAX_REPS]).null_arg(2, 1).arg_fold(3).group_flags();
    for fns in [owner_fns(&d, 3), owner_fns_dense(&d, 3, Some((-3, 3)))] {
        for w in [1usize, 3] {
            let mut u: Vec<Group> = own_flagged(&d, &fns, &pairs, 4, w, 8 << 20).into_iter().flatten().collect();
            u.sort();
            assert_eq!(u, all, "width {w}: the NULL group apart from the 0 group, NULL arguments uncounted, the argument folded once");
        }
    }
}

/// CAP-28 (`OwnerDesc::group_flag_words`): the pair image's NULL flags take one word per 64 keys, so past 64 keys the
/// group-flags accumulator carries the group keys' flags words (one per 64 group keys) and clears the argument's bit only
/// where it shares one. Two images: `[k, x, f0, f1]` as if x were key 64 (k's bit 0 of f0, x's bit 0 of f1: the accumulator
/// keys `[k, f0]`, x's word dropped), and `[k, j, x, f0, f1]` as if j were key 64 and x key 65 (k's bit 0 of f0, j's bit 0
/// and x's bit 1 of f1: the accumulator keys `[k, j, f0, f1 with x's bit cleared]`). A NULL group key stays its own group,
/// a NULL argument counts no pair, `sum(DISTINCT x)` folds once per pair, at widths 1 and 3.
#[test]
fn cap28_group_flags_carry_every_group_flags_word() {
    for two in [false, true] {
        let g = 1 + two as usize;
        let pair = RecordLayout::new_nn(&vec![KeyKind::I64; g + 3], &[SlotKind::Count]);
        let acc = RecordLayout::new(&vec![KeyKind::I64; g + 2 - (!two) as usize], &[SlotKind::Count, SlotKind::Count, SlotKind::Count, SlotKind::SumI64]);
        let (mut pairs, mut want, mut seen) = (Vec::new(), BTreeMap::new(), std::collections::BTreeSet::new());
        for i in 0..20_000i64 {
            let (kn, jn, xn) = (i % 9 == 0, two && i % 4 == 0, i % 5 == 0);
            let (k, x) = (if kn { 0 } else { i % 7 - 3 }, if xn { 0 } else { (i * 13) % 40 });
            let j = if jn { 0 } else { k * 2 };
            let mut keys = vec![k as u64];
            if two {
                keys.push(j as u64);
            }
            keys.push(x as u64);
            keys.push(kn as u64);
            keys.push(if two { jn as u64 | (xn as u64) << 1 } else { xn as u64 });
            pairs.push(Flagged { keys, rows: 1 });
            let e: &mut (i64, i64, i64) = want.entry((k, kn, jn)).or_insert((0, 0, 0));
            e.1 += 1;
            if !xn && seen.insert((k, kn, jn, x)) {
                e.0 += 1;
                e.2 += x;
            }
        }
        let mut all: Vec<Group> = want.iter().map(|(&(k, _, _), &(p, c, s))| (k, p, vec![c as i128, s as i128])).collect();
        all.sort();
        let (first, w) = (g as u8 + 1, g as u8 + 2);
        let d = OwnerDesc::distinct(pair, acc, (pair.nkeys() - acc.nkeys()) as u8, 16, ORDER_NONE, 0, [(REP_NONE, 0); MAX_REPS]).null_arg(w, two as u8).arg_fold(3).group_flag_words(first, 1 + two as u8);
        let fns = owner_fns(&d, 3);
        for width in [1usize, 3] {
            let mut u: Vec<Group> = own_flagged(&d, &fns, &pairs, 4, width, 8 << 20).into_iter().flatten().collect();
            u.sort();
            assert_eq!(u, all, "two {two} width {width}: NULL group keys apart, NULL arguments uncounted, the argument folded once");
        }
    }
}

/// `merge.rs`'s fill → refold (`SubSplit::reread`): a partition whose first table was sized for eight groups fills and is read again
/// at the records' size at the SAME depth. The dense round noted the records before the fill at the insert, so the re-read must keep
/// them quiet like the finer split does — before 2026-09-21 it did not, and every group re-counted its prefix (a
/// `count(DISTINCT b) GROUP BY a` read 5586 in every group on the dropped tip 76535584473, PD-HEAPW8 §2b).
#[test]
fn a_table_that_fills_at_the_estimate_is_refolded_once_without_renoting_the_prefix() {
    let (lo, span) = (-50i64, 2_000u64);
    let data = rows(60_000, lo, span, 5_000, 11);
    RETRY_EST.store(8, Ordering::Relaxed);
    for slots in [false, true] {
        let all = oracle(&data, slots);
        for count in [false, true] {
            let d = desc(slots, count);
            let dense = owner_fns_dense(&d, 3, Some((lo, lo + span as i64 - 1)));
            let pairs = owner_fns(&d, 3);
            for w in [1usize, 3] {
                for pbits in [3u8, 5] {
                    let before = RETRIES.load(Ordering::Relaxed);
                    let (a, _) = own_by(&d, &dense, &data, pbits, w, 32 << 20, fill_of(slots), Some(1));
                    assert!(RETRIES.load(Ordering::Relaxed) > before, "slots {slots} count {count} width {w} pbits {pbits}: the first tables filled and were refolded");
                    let (b, _) = own_by(&d, &pairs, &data, pbits, w, 32 << 20, fill_of(slots), Some(1));
                    assert_eq!(a, b, "slots {slots} count {count} width {w} pbits {pbits}: the dense round after a refold is the pair path");
                    let union: Vec<Group> = a.iter().flatten().cloned().collect();
                    if count {
                        assert_eq!(best(&union, 10), best(&all, 10), "slots {slots} width {w} pbits {pbits}: the best ten");
                    } else {
                        let mut u = union;
                        u.sort();
                        assert_eq!(u, all, "slots {slots} width {w} pbits {pbits}: every group once, its distinct count the oracle's");
                    }
                }
            }
        }
    }
    RETRY_EST.store(0, Ordering::Relaxed);
}

/// `PHI64`'s inverse mod 2^64: a two-word key image's hash is a bijection in its last word, so a crafted argument
/// routes the pair where the test says.
const PHI_INV: u64 = 0xf1de_83e1_9937_733d;

/// The argument `x` for which `KeyHash::of(&[k, x])` is `h` (`KeyHash::of` run backwards over its last word).
fn craft_x(k: i64, h: u64) -> i64 {
    let phi = gravity_abi::hash::PHI64;
    let s = gravity_abi::hash::IMAGE_SHIFT;
    let mut h1 = (phi ^ 2 ^ k as u64).wrapping_mul(phi);
    h1 ^= h1 >> s;
    let a = h.wrapping_mul(PHI_INV);
    let mut u = a;
    let mut sh = s;
    while sh < 64 {
        u ^= a >> sh;
        sh += s;
    }
    (u.wrapping_mul(PHI_INV) ^ h1) as i64
}

/// D471: `t3_dense::Fills` kept one fill slot per split depth for depths 0..3 only; since D445 the split goes past
/// depth 3 and a depth-4 table's fill overwrote the depth-3 slot, so the depth-4 sibling after it kept its sibling's
/// fill quiet: the records between the depth-3 fill and the sibling's were never noted and their pairs went uncounted.
/// One partition whose pairs agree in the route through depth 3 (every table to depth 3 fills at the same position),
/// then split at depth 4: group A (sub 0, 240 pairs) fills again at depth 4 well after group B (sub 1, 48 pairs)
/// was interleaved into the stream past the depth-3 fill.
#[test]
fn d471_a_depth_4_sibling_after_a_depth_4_fill_counts_every_pair() {
    assert_eq!(gravity_abi::hash::PHI64.wrapping_mul(PHI_INV), 1);
    let (lo, span) = (-7i64, 4u64);
    let pbits = 3u8;
    let pair = |k: i64, sub4: u64, i: u64| {
        let low = (i.wrapping_add(1).wrapping_mul(0xd6e8_feb8_6659_fd93)) >> 18;
        let h = sub4 << 49 | (i % 8) << 46 | low;
        let x = craft_x(k, h);
        let kh = KeyHash::of(&[k as u64, x as u64]);
        assert_eq!((kh.part(pbits as u32), kh.sub(pbits as u32, 3), kh.sub(pbits as u32, 4) & 7), (0, 0, sub4), "the crafted pair routes where it says");
        Row { k, x, v: 1 }
    };
    let a: Vec<Row> = (0..240).map(|i| pair(lo, 0, i)).collect();
    let b: Vec<Row> = (0..48).map(|i| pair(lo + 1, 1, 1000 + i)).collect();
    let mut data = Vec::new();
    for i in 0..48 {
        data.push(Row { ..a[i] });
        data.push(Row { ..b[i] });
    }
    data.extend(a[48..].iter().map(|r| Row { ..*r }));
    let all = oracle(&data, false);
    assert_eq!(all.iter().map(|g| (g.0, g.1)).collect::<Vec<_>>(), vec![(lo, 240), (lo + 1, 48)]);
    let d = desc(false, false);
    let dense = owner_fns_dense(&d, 3, Some((lo, lo + span as i64 - 1)));
    let pairs = owner_fns(&d, 3);
    for (name, fns) in [("dense", &dense), ("pairs", &pairs)] {
        let (got, _) = own_by(&d, fns, &data, pbits, 1, 32 << 20, fill_of(false), Some(1 << 20));
        let mut u: Vec<Group> = got.into_iter().flatten().collect();
        u.sort();
        assert_eq!(u, all, "{name}: every pair counted once past the depth-4 fill");
    }
}
