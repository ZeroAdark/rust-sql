//! The hand distinct plan's dense owner under `merge.rs`'s fill → refold: a partition table sized for a few groups
//! fills, is read again ONCE at the same depth at the records' size (`SubSplit::reread`), and when that fills too,
//! eight times under the finer split. The dense hook noted every pair before the fill at the insert; the re-reads
//! must keep that prefix quiet (lane KDENSE-REFOLD, 2026-09-21; before it the kernels' hook had no protocol and
//! every group re-counted its prefix, or a records'-size table laid over the tail area lost it).
use gravity_abi::breaker::{Candidates, HashAgg, OwnerFns, OwnerTop, Rec, RecordLayout, SubSplit, T1, HASHAGG_HDR, OWNERTOP_HDR};
use gravity_abi::hash::KeyHash;
use gravity_kernels::ops::sort::topn::t3::{misc_word, owner_fns_distinct, packed, shape_word_nn, Plan, K_INT, MODE_HASH, NPARAMS, ORDER_COUNT_DESC, OUT_COUNT, OUT_DISTINCT, OUT_KEY, P_KEYS, P_LIMIT, P_MISC, P_OUT, P_OUTVAL, P_SHAPE, P_SLOTS, S_AVG, S_COUNT, S_SUM};
use gravity_kernels::ops::sort::topn::t3_dense::word;
use std::collections::BTreeMap;

fn lcg(s: &mut u64) -> u64 {
    *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    *s >> 17
}

/// Q9's plan words: `(region, user)` with `count(*)`, `sum`, `avg` when `slots`, NOT NULL, the domain word; every group kept.
fn plan(lo: i64, hi: i64, slots: bool, dense: bool) -> [u64; NPARAMS] {
    let mut p = [0u64; NPARAMS];
    p[P_LIMIT] = (hi - lo + 1) as u64;
    let ns = if slots { 3 } else { 0 };
    p[P_SHAPE] = shape_word_nn(2, ns, ORDER_COUNT_DESC, MODE_HASH, 0, None, 0, 2 + ns, true);
    p[P_KEYS] = packed(&[(0, K_INT), (1, K_INT)], 4);
    p[P_SLOTS] = if slots { packed(&[(0, S_COUNT), (2, S_SUM), (3, S_AVG)], 4) } else { 0 };
    p[P_OUT] = packed(&[(0, OUT_KEY), (0, OUT_DISTINCT), (0, OUT_COUNT)], 4);
    p[P_OUTVAL] = if dense { word(lo, hi) } else { 0 };
    p[P_MISC] = misc_word(4, 0, (hi - lo + 1) as u64);
    p
}

#[derive(Clone, Copy)]
struct Row {
    r: i64,
    u: u64,
    c: i64,
    s: i64,
    a: i64,
}

/// `n` rows over `span` regions from `lo`, users over `users` values: many repeats of a pair, skewed to the low regions.
fn rows(n: usize, lo: i64, span: u64, users: u64, seed: u64) -> Vec<Row> {
    let mut rng = seed;
    (0..n)
        .map(|_| {
            let x = lcg(&mut rng);
            let r = lo + if x % 3 == 0 { (x / 3 % 16) as i64 } else { (x / 3 % span) as i64 };
            let u = lcg(&mut rng) % users;
            Row { r, u, c: (u % 7) as i64 + 1, s: (u % 1000) as i64 - 500, a: (u % 91) as i64 }
        })
        .collect()
}

type Group = (i64, i64, i64, i64, i128, i64);

/// Per region: distinct pairs, count(*), sum, avg (sum, n) over the rows (every row folds; a pair counts once).
fn oracle(rows: &[Row], slots: bool) -> Vec<Group> {
    let mut pairs = std::collections::BTreeSet::new();
    let mut g: BTreeMap<i64, Group> = BTreeMap::new();
    for r in rows {
        let e = g.entry(r.r).or_insert((r.r, 0, 0, 0, 0, 0));
        if pairs.insert((r.r, r.u)) {
            e.1 += 1;
        }
        if slots {
            e.2 += r.c;
            e.3 += r.s;
            e.4 += r.a as i128;
            e.5 += r.c;
        }
    }
    g.into_values().collect()
}

fn fill(q: &Rec<'_>, r: &Row, slots: bool) {
    q.open(&[r.r as u64, r.u]);
    if slots {
        q.add_count(0, r.c);
        q.add_i64(1, r.s);
        q.add_avg(2, r.a as i128, r.c);
    }
}

fn read(r: Rec<'_>, slots: bool) -> Group {
    if slots {
        let (s, n) = r.avg(3);
        (r.key(0) as i64, r.count(0), r.count(1), r.i64(2), s, n)
    } else {
        (r.key(0) as i64, r.count(0), 0, 0, 0, 0)
    }
}

/// One round-1 partition as `merge.rs fold_sub` runs it: the records in runs through `fns.fold` into a table laid at
/// the owner's table space, then the reduce; the first table sized for `first` groups when set — a fill is read again
/// once at the same depth at the records' size (`est`), a fill of that as eight sub-partitions of `n / 8 + 1`.
#[allow(clippy::too_many_arguments)]
fn fold_part(fns: &OwnerFns, pair: &RecordLayout, region: *mut u8, table_off: usize, bytes: usize, recs: &[u8], split: SubSplit, est: usize, first: usize, stats: &mut (usize, usize)) {
    let rec = pair.size() as usize;
    let est_used = if first != 0 && split.reread == 0 && split.depth == 0 { first.min(est) } else { est };
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
            stats.0 += 1;
            return fold_part(fns, pair, region, table_off, bytes, recs, SubSplit { reread: 1, ..split }, est, first, stats);
        }
        assert!(split.depth < 3, "the split ends");
        stats.1 += 1;
        for s in 0..8u32 {
            fold_part(fns, pair, region, table_off, bytes, recs, SubSplit { pbits: split.pbits, depth: split.depth + 1, reread: 0, sub: split.sub << 3 | s as u64 }, n / 8 + 1, first, stats);
        }
        return;
    }
    let rc = unsafe { (fns.reduce)(fns.params.as_ptr(), region, t as *const HashAgg, core::ptr::null_mut(), core::ptr::null(), -1) };
    assert_eq!(rc, 0, "the reduce of a folded table");
}

/// The owner phase over `w` owners: round 1 through `fold_part` (the estimate of a partition's records over `shrink`,
/// the first table `first` groups), the round's end, round 2 over the owners' buffers; every owner's groups, sorted,
/// and (refolds, splits).
fn own(p: &[u64; NPARAMS], rows: &[Row], pbits: u8, w: usize, bytes: usize, shrink: usize, first: usize) -> (Vec<Group>, (usize, usize)) {
    let pl = Plan::from_words(p).expect("the plan");
    let slots = pl.nslots > 0;
    let (pair, acc) = (pl.layout(), pl.layout_of(1, 1));
    let fns = owner_fns_distinct(p);
    let fp = fns.params.as_ptr();
    assert!(bytes as u64 >= (fns.min_bytes)(fp, 1), "the region holds the floor");
    let mut regions: Vec<Vec<u64>> = (0..w).map(|_| vec![0u64; bytes / 8]).collect();
    let mut opened = Vec::new();
    for region in regions.iter_mut() {
        let rc = unsafe { (fns.open)(fp, region.as_mut_ptr() as *mut u8, bytes) };
        assert!(rc >= 0, "the owner region opens");
        opened.push(rc as usize);
    }
    let mut stats = (0, 0);
    for part in 0..1u32 << pbits {
        let mine: Vec<&Row> = rows.iter().filter(|r| (KeyHash::of(&[r.r as u64, r.u])).part(pbits as u32) == part).collect();
        if mine.is_empty() {
            continue;
        }
        let rec = pair.size() as usize;
        let mut recs = vec![0u8; mine.len() * rec];
        for (i, r) in mine.iter().enumerate() {
            fill(&unsafe { Rec::at(recs.as_mut_ptr().add(i * rec), &pair) }, r, slots);
        }
        let o = part as usize % w;
        fold_part(&fns, &pair, regions[o].as_mut_ptr() as *mut u8, opened[o], bytes, &recs, SubSplit { pbits, depth: 0, reread: 0, sub: 0 }, mine.len() / shrink, first, &mut stats);
    }
    for region in regions.iter_mut() {
        assert_eq!(unsafe { (fns.end_round)(fp, region.as_mut_ptr() as *mut u8) }, 0, "the round closes");
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
        assert!(unsafe { (fns.reduce2)(fp, region, t as *const HashAgg, core::ptr::null_mut(), core::ptr::null(), -1) } >= 0, "the round-2 reduce");
    }
    let mut out = Vec::new();
    for region in regions.iter() {
        let base = region.as_ptr() as *const u8;
        let h = unsafe { OwnerTop::view(base) };
        for i in 0..h.len as usize {
            out.push(read(unsafe { Rec::at(base.add(OWNERTOP_HDR + i * acc.size() as usize) as *mut u8, &acc) }, slots));
        }
    }
    out.sort();
    (out, stats)
}

#[test]
fn a_table_that_fills_at_the_estimate_is_refolded_once_without_renoting_the_prefix() {
    let (lo, span) = (-50i64, 1_000u64);
    let data = rows(60_000, lo, span, 5_000, 11);
    for slots in [false, true] {
        let all = oracle(&data, slots);
        let dense = plan(lo, lo + span as i64 - 1, slots, true);
        let pairs = plan(lo, lo + span as i64 - 1, slots, false);
        for w in [1usize, 3] {
            for pbits in [3u8, 5] {
                // the first table of every partition sized for eight groups: it fills, the re-read at the records' size does not
                let (a, (refolds, splits)) = own(&dense, &data, pbits, w, 32 << 20, 1, 8);
                assert!(refolds > 0 && splits == 0, "slots {slots} width {w} pbits {pbits}: every first table filled and was refolded once ({refolds} refolds, {splits} splits)");
                let (b, _) = own(&pairs, &data, pbits, w, 32 << 20, 1, 8);
                assert_eq!(a, b, "slots {slots} width {w} pbits {pbits}: the dense round after a refold is the pair path");
                assert_eq!(a, all, "slots {slots} width {w} pbits {pbits}: every group once, its distinct count the oracle's");
            }
        }
    }
}

#[test]
fn a_refolded_table_that_fills_again_splits_with_the_prefix_quiet() {
    let (lo, span) = (-50i64, 1_000u64);
    let data = rows(60_000, lo, span, 5_000, 23);
    for slots in [false, true] {
        let all = oracle(&data, slots);
        let dense = plan(lo, lo + span as i64 - 1, slots, true);
        for pbits in [0u8, 3] {
            // the estimate an eighth of the records: the first table fills, the re-read at the estimate fills, the eight children finish
            let (a, (refolds, splits)) = own(&dense, &data, pbits, 1, 32 << 20, 8, 8);
            assert!(refolds > 0 && splits > 0, "slots {slots} pbits {pbits}: refolded then split ({refolds} refolds, {splits} splits)");
            assert_eq!(a, all, "slots {slots} pbits {pbits}: every group once through the re-read and the split");
        }
    }
}

#[test]
fn the_area_lies_before_the_table_and_the_marker_names_it() {
    let (lo, span) = (0i64, 100u64);
    let p = plan(lo, lo + span as i64 - 1, true, true);
    let fns = owner_fns_distinct(&p);
    let bytes = 8 << 20;
    let mut region = vec![0u64; bytes / 8];
    let off = unsafe { (fns.open)(fns.params.as_ptr(), region.as_mut_ptr() as *mut u8, bytes) } as usize;
    let h = unsafe { OwnerTop::view(region.as_ptr() as *const u8) };
    assert_eq!(h.table_off as usize, off);
    let marker = region[off / 8 - 1] as usize;
    assert!(marker > 0 && off > h.round2_off as usize + marker + 8, "the area's bytes sit right before the table, after the round-2 buffers");
    // without the domain word the table starts where the round-2 buffers end
    let q = plan(lo, lo + span as i64 - 1, true, false);
    let fns = owner_fns_distinct(&q);
    let mut region = vec![0u64; bytes / 8];
    let off2 = unsafe { (fns.open)(fns.params.as_ptr(), region.as_mut_ptr() as *mut u8, bytes) } as usize;
    assert_eq!(off, off2 + marker + 8);
}

/// D597: a pair whose group key is outside the domain word (a stale domain) is kept out of the identity table and flagged;
/// the reduce refuses by name (`UNSUPPORTED_DOMAIN`, the driver's rerun through the plain form), never a count short.
#[test]
fn a_key_outside_the_domain_is_flagged_and_refused_by_name_not_dropped() {
    use gravity_abi::status::{unsupported, UNSUPPORTED_DOMAIN};
    let (lo, span) = (0i64, 200u64);
    let data = rows(5_000, lo, span, 300, 31);
    for slots in [false, true] {
        for (hi, missed) in [(lo + span as i64 - 1, false), (lo + span as i64 / 2 - 1, true)] {
            let p = plan(lo, hi, slots, true);
            let pl = Plan::from_words(&p).expect("the plan");
            let pair = pl.layout();
            let fns = owner_fns_distinct(&p);
            let fp = fns.params.as_ptr();
            let bytes = 32 << 20;
            let mut region = vec![0u64; bytes / 8];
            let base = region.as_mut_ptr() as *mut u8;
            let table_off = unsafe { (fns.open)(fp, base, bytes) } as usize;
            let rec = pair.size() as usize;
            let mut recs = vec![0u8; data.len() * rec];
            for (i, r) in data.iter().enumerate() {
                fill(&unsafe { Rec::at(recs.as_mut_ptr().add(i * rec), &pair) }, r, slots);
            }
            let t = unsafe {
                core::ptr::write_bytes(base.add(table_off), 0, HASHAGG_HDR);
                HashAgg::at(base.add(table_off), &pair, 16_384, (bytes - table_off) as u32) as *mut HashAgg
            };
            let mut cand = Candidates::none();
            let v = unsafe { (fns.fold.expect("the form's fold"))(fp, t, recs.as_ptr(), data.len(), SubSplit::NONE, &mut cand) };
            assert_eq!(v, data.len() as i64, "slots {slots}: every record met");
            let rc = unsafe { (fns.reduce)(fp, base, t as *const HashAgg, core::ptr::null_mut(), core::ptr::null(), -1) };
            assert_eq!(rc, if missed { unsupported(UNSUPPORTED_DOMAIN) } else { 0 }, "slots {slots} missed {missed}");
        }
    }
}
