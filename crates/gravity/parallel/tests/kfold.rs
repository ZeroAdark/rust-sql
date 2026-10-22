//! The owner fold generated per layout against the generic fold: the same partials (buffer chains, a table
//! residue, a spilled epoch) folded through `HashAgg::fold_run` and through the kernels'
//! `owner_fold`, the owner region byte-identical after every partition (records, candidates, the
//! split's children hashed by the reduce), at region sizes that split to depth 0, 1 and 2.

mod common;

use gravity_abi::breaker::{Candidates, FoldFn, HashAgg, KeyKind, OwnerFns, OwnerTop, Rec, RecordLayout, SlotKind, SubSplit, T1, MOVING, ORDER_NONE, OWNERTOP_HDR, OWNER_PARAMS};
use gravity_driver::{Merge, Partial, Region, SpillFile, SpillSet};
use gravity_kernels::table::owner_fold as fold;
use gravity_parallel::HashAggPartial;

const PBITS: u8 = 3;
const WORKERS: usize = 3;
const WORKER_BYTES: usize = 96 << 10;

fn lcg(s: &mut u64) -> u64 {
    *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    *s >> 17
}

fn make_record(buf: &mut [u8; 256], layout: &RecordLayout, keys: &[u64], seed: u64) {
    buf.fill(0);
    let r = unsafe { Rec::at(buf.as_mut_ptr(), layout) };
    r.open(keys);
    for j in 0..layout.nslots() {
        match layout.slots[j] {
            SlotKind::Count => r.add_count(j, 1 + (seed % 3) as i64),
            SlotKind::SumI64 => r.add_i64(j, (seed % 1000) as i64 - 500),
            SlotKind::SumI128 => r.add_i128(j, seed as i128 * 1_000_000_007),
            SlotKind::MinI64 => r.min_i64(j, (seed % 5000) as i64),
            SlotKind::MaxI64 => r.max_i64(j, (seed % 5000) as i64),
            SlotKind::AvgI128 => r.add_avg(j, (seed % 5000) as i128, 1),
            SlotKind::SumF64 => r.add_f64(j, (seed % 100) as f64 / 7.0).expect("finite"),
            SlotKind::SumF32 => r.add_f32(j, (seed % 100) as f64 / 8.0).expect("finite"),
            SlotKind::F64Accum => r.combine_f64_accum(j, [1.0, (seed % 100) as f64 / 8.0, 0.0]).expect("finite"),
            SlotKind::BitAnd | SlotKind::BitOr | SlotKind::BitXor => r.fold_bits(j, (seed % 4096) as i64),
            SlotKind::MinCode => r.min_code(j, (seed % 4) as u32, (seed % 977) as u32),
            SlotKind::MaxCode => r.max_code(j, (seed % 4) as u32, (seed % 977) as u32),
            SlotKind::SumNum => r.add_i128(j, seed as i128 * 1_000_000_007),
            SlotKind::AvgNum => r.add_avg(j, (seed % 5000) as i128, 1),
            SlotKind::SumNumWide => r.add_num256(j, [seed.wrapping_mul(1_000_000_007), seed % 7, 0, 0]),
            SlotKind::AvgNumWide => r.add_avg_num256(j, [seed % 5000, 0, 0, 0], 1),
            SlotKind::IntervalMin | SlotKind::IntervalMax => {
                let v = ((seed % 5000) as i64 * 3_600_000_000, (seed % 7) as i32 - 3, (seed % 3) as i32);
                r.fold_interval(j, v, layout.slots[j] == SlotKind::IntervalMin)
            }
            SlotKind::IntervalSum => {
                let mut v = gravity_abi::breaker::IntervalSum::default();
                v.accum((seed % 5000) as i64 * 3_600_000_000, (seed % 7) as i32 - 3, (seed % 3) as i32);
                r.fold_interval_sum(j, &v).expect("unlicensed")
            }
            SlotKind::MinVar | SlotKind::MaxVar | SlotKind::AggState | SlotKind::NumSideMin | SlotKind::NumSideMax => {}
        }
    }
}

unsafe fn t_open(params: *const u64, owner: *mut u8, _bytes: usize) -> i64 {
    core::ptr::write_bytes(owner, 0, OWNERTOP_HDR);
    let top = owner as *mut OwnerTop;
    (*top).table_off = 64;
    (*top).thr = *params as i64;
    0
}

/// FNV over the used records (index, bytes) and the candidate list into `OwnerTop.total`; `groups` += the table's length.
unsafe fn t_reduce(_params: *const u64, owner: *mut u8, table: *const HashAgg, _site: *mut gravity_abi::Site, cand: *const u32, ncand: i64) -> i64 {
    let t = &*table;
    let top = owner as *mut OwnerTop;
    let mut h = (*top).total ^ 0xcbf29ce484222325;
    let mut mix = |b: u64| h = (h ^ b).wrapping_mul(0x100000001b3);
    for i in 0..t.cap as usize {
        let r = t.record(i);
        if !r.used() {
            continue;
        }
        mix(i as u64);
        for w in r.bytes().chunks(8) {
            mix(w.iter().fold(0u64, |a, &b| a << 8 | b as u64));
        }
    }
    mix(ncand as u64);
    for i in 0..ncand.max(0) as usize {
        mix(*cand.add(i) as u64);
    }
    (*top).total = h;
    (*top).groups += t.len as u64;
    0
}

unsafe fn t_noop(_params: *const u64, _owner: *mut u8) -> i64 {
    0
}

fn t_min(_params: *const u64, _owners: u32) -> u64 {
    0
}

fn fns(fold: Option<FoldFn>, thr: i64, order: bool) -> OwnerFns {
    let mut params = [0u64; OWNER_PARAMS];
    params[0] = thr as u64;
    let order_slot = if order { 0 } else { ORDER_NONE };
    OwnerFns { params, rounds: 1, order_slot, order_slot2: order_slot, min_bytes: t_min, open: t_open, reduce: t_reduce, end_round: t_noop, reduce2: t_reduce, fold }
}

struct Sources {
    regions: Vec<Vec<u64>>,
    files: Vec<Box<dyn SpillFile>>,
    records: u64,
}

/// `WORKERS` phase-1 partials of `layout`: `n` records each over `domain` keys, a fifth into the table
/// (the residue), a third into a spilled epoch, the rest appended to the chains (the pool's overflow spills too).
fn sources(layout: &RecordLayout, n: usize, domain: u64, seed: u64) -> Sources {
    let mut s = Sources { regions: Vec::new(), files: Vec::new(), records: 0 };
    let mut rng = seed;
    let mut buf = [0u8; 256];
    let mut keys = [0u64; 8];
    for w in 0..WORKERS {
        let mut region = vec![0u64; WORKER_BYTES / 8];
        let mut file = common::MemSpillSet.open("kfold", 0, w, 1 << PBITS).unwrap();
        let mut epoch: Vec<Vec<u8>> = (0..1 << PBITS).map(|_| Vec::new()).collect();
        let t = unsafe { T1::at(region.as_mut_ptr() as *mut u8, layout, PBITS, 8192, WORKER_BYTES as u32) as *mut T1 };
        for i in 0..n {
            for k in keys.iter_mut().take(layout.nkeys()) {
                *k = lcg(&mut rng) % domain;
            }
            let v = lcg(&mut rng);
            make_record(&mut buf, layout, &keys[..layout.nkeys()], v);
            let r = unsafe { Rec::at(buf.as_mut_ptr(), layout) };
            let part = r.route().part(PBITS as u32);
            s.records += 1;
            if i % 5 == 0 {
                if let Some(p) = unsafe { T1::probe(t, r.keys(), r.hash()) } {
                    unsafe { Rec::at(p, layout) }.fold(&r).expect("no float overflow");
                    continue;
                }
            }
            if i % 3 == 1 || !unsafe { T1::append_bytes(t, part, buf.as_ptr()) } {
                epoch[part as usize].extend_from_slice(r.bytes());
            }
        }
        let parts: Vec<(u32, &[u8])> = epoch.iter().enumerate().filter(|(_, b)| !b.is_empty()).map(|(p, b)| (p as u32, b.as_slice())).collect();
        file.write_epoch(&parts).unwrap();
        s.regions.push(region);
        s.files.push(file);
    }
    s
}

/// The owner region after every partition under `fold`, concatenated.
fn own(src: &Sources, layout: &RecordLayout, fold: Option<FoldFn>, region_bytes: usize, thr: i64, order: bool) -> (Vec<u8>, u64) {
    let merge = HashAggPartial::new(0, Some(fns(fold, thr, order)));
    let partials: Vec<Partial<'_>> = src.regions.iter().enumerate().map(|(o, r)| Partial { ordinal: o as u16, bytes: unsafe { core::slice::from_raw_parts(r.as_ptr() as *const u8, WORKER_BYTES) }, rows_processed: 0 }).collect();
    let files: Vec<&dyn SpillFile> = src.files.iter().map(|f| f.as_ref()).collect();
    let mut region = vec![0u64; region_bytes / 8];
    let owner = Region { ptr: region.as_mut_ptr() as *mut u8, len: region_bytes };
    merge.open_owner(owner, 0).unwrap();
    let mut out = Vec::new();
    for part in 0..1u32 << PBITS {
        merge.own_partition(owner, part, PBITS, &partials, &files, core::ptr::null_mut(), core::ptr::null(), 1, &mut || Ok(())).unwrap();
        out.extend_from_slice(unsafe { core::slice::from_raw_parts(owner.ptr, region_bytes) });
    }
    let _ = layout;
    (out, unsafe { OwnerTop::view(owner.ptr) }.groups)
}

fn check(layout: &RecordLayout, n: usize, domain: u64, region_bytes: usize, order: bool) {
    let src = sources(layout, n, domain, 0x5eed ^ n as u64 ^ domain);
    let (generic, g1) = own(&src, layout, None, region_bytes, 2, order);
    let (kernel, g2) = own(&src, layout, Some(fold::owner_fold), region_bytes, 2, order);
    assert_eq!(g1, g2, "groups folded");
    assert!(g1 >= domain.min(n as u64 / 4), "the sources were folded");
    assert!(generic == kernel, "owner region bytes differ (records {}, domain {domain}, region {region_bytes})", src.records);
}

fn layouts() -> Vec<RecordLayout> {
    use KeyKind::I64;
    use SlotKind::*;
    vec![
        RecordLayout::new_nn(&[I64], &[Count]),
        RecordLayout::new_nn(&[I64], &[Count, SumI64, AvgI128]),
        RecordLayout::new_nn(&[I64], &[SumI64, Count, AvgI128]),
        RecordLayout::new_nn(&[I64], &[Count, SumI64, SumI64, AvgI128]),
        RecordLayout::new_nn(&[I64], &[Count, SumI128]),
        RecordLayout::new_nn(&[I64], &[Count, SumF64]),
        RecordLayout::new_nn(&[I64; 2], &[Count, MinCode]),
        RecordLayout::new_nn(&[I64; 2], &[Count, AvgI128, MinCode]),
        RecordLayout::new_nn(&[I64; 2], &[SumF32, Count, SumNum]),
        RecordLayout::new_nn(&[I64; 3], &[Count, MinCode, MinCode]),
        RecordLayout::new_nn(&[I64; 3], &[Count, MinCode, MinCode]).partitioned_by(2),
        RecordLayout::new_nn(&[I64; 4], &[Count, F64Accum]),
        RecordLayout::new_nn(&[I64; 5], &[Count, MinCode]),
        RecordLayout::new_nn(&[I64; 7], &[MinCode, Count, AvgNum]),
        RecordLayout::new_nn(&[I64; 8], &[Count]),
        RecordLayout::with_sentinel(&[I64], &[Count], false, None),
        RecordLayout::with_sentinel(&[I64; 2], &[Count, Count], false, Some(1)),
        RecordLayout::new(&[I64], &[SumI64, Count]),
        RecordLayout::new(&[I64; 2], &[MinI64, MaxI64, MaxI64, Count]),
        RecordLayout::new(&[I64; 2], &[Count, SumF64, MaxCode]),
        RecordLayout::new(&[I64; 3], &[Count, BitAnd, BitOr, BitXor]),
        RecordLayout::new(&[I64; 4], &[Count, IntervalMin, IntervalMax]),
        RecordLayout::new(&[I64; 2], &[Count, AvgNum, SumNumWide]),
        RecordLayout::with_sentinel(&[I64; 2], &[SumI64, AvgI128], true, None),
        RecordLayout::new(&[I64; 6], &[Count, SumI64, MinI64, MaxCode]).partitioned_by(3),
    ]
}

/// Perf: one test per chunk (one over every layout was the gate's longest); the chunks cover `0..layouts().len()`.
const SHAPE_CHUNKS: usize = 16;

fn every_shape_folds_chunk(c: usize) {
    let all = layouts();
    let n = all.len();
    for layout in &all[c * n / SHAPE_CHUNKS..(c + 1) * n / SHAPE_CHUNKS] {
        let layout = *layout;
        assert!(fold::generated(&layout), "{:?} folds through the generated fold", layout.slot_kinds());
        // depth 0 (a small domain), depth 1, depth 2 (an 8 KiB region: 56–224 records per table)
        check(&layout, 3000, 400, 8 << 10, true);
        check(&layout, 4000, 20_000, 8 << 10, true);
        check(&layout, 9000, 1 << 40, 8 << 10, false);
    }
}

macro_rules! every_shape_folds_tests {
    ($($name:ident = $c:literal),* $(,)?) => {
        $(#[test]
        fn $name() {
            every_shape_folds_chunk($c);
        })*
        const SHAPE_CHUNK_TESTS: &[usize] = &[$($c),*];
    };
}

every_shape_folds_tests! {
    every_shape_folds_byte_identical_to_the_generic_fold_unsplit_and_split_00 = 0,
    every_shape_folds_byte_identical_to_the_generic_fold_unsplit_and_split_01 = 1,
    every_shape_folds_byte_identical_to_the_generic_fold_unsplit_and_split_02 = 2,
    every_shape_folds_byte_identical_to_the_generic_fold_unsplit_and_split_03 = 3,
    every_shape_folds_byte_identical_to_the_generic_fold_unsplit_and_split_04 = 4,
    every_shape_folds_byte_identical_to_the_generic_fold_unsplit_and_split_05 = 5,
    every_shape_folds_byte_identical_to_the_generic_fold_unsplit_and_split_06 = 6,
    every_shape_folds_byte_identical_to_the_generic_fold_unsplit_and_split_07 = 7,
    every_shape_folds_byte_identical_to_the_generic_fold_unsplit_and_split_08 = 8,
    every_shape_folds_byte_identical_to_the_generic_fold_unsplit_and_split_09 = 9,
    every_shape_folds_byte_identical_to_the_generic_fold_unsplit_and_split_10 = 10,
    every_shape_folds_byte_identical_to_the_generic_fold_unsplit_and_split_11 = 11,
    every_shape_folds_byte_identical_to_the_generic_fold_unsplit_and_split_12 = 12,
    every_shape_folds_byte_identical_to_the_generic_fold_unsplit_and_split_13 = 13,
    every_shape_folds_byte_identical_to_the_generic_fold_unsplit_and_split_14 = 14,
    every_shape_folds_byte_identical_to_the_generic_fold_unsplit_and_split_15 = 15,
}

#[test]
fn every_shape_folds_chunks_cover_the_shape_set() {
    assert_eq!(SHAPE_CHUNK_TESTS, (0..SHAPE_CHUNKS).collect::<Vec<_>>().as_slice(), "one test per chunk");
    let n = layouts().len();
    let covered: usize = (0..SHAPE_CHUNKS).map(|c| (c + 1) * n / SHAPE_CHUNKS - c * n / SHAPE_CHUNKS).sum();
    assert_eq!(covered, n, "the chunks cover every layout once");
}

#[test]
fn a_validity_layout_folds_through_the_kernel() {
    let layout = RecordLayout::new(&[KeyKind::I64, KeyKind::I32], &[SlotKind::Count, SlotKind::MinI64, SlotKind::SumF64, SlotKind::MaxCode]);
    assert!(fold::generated(&layout));
    check(&layout, 4000, 3000, 8 << 10, true);
}

#[test]
fn the_fold_reports_the_refusal_and_the_records_folded_before_it() {
    let layout = RecordLayout::new_nn(&[KeyKind::I64], &[SlotKind::Count]);
    let mut region = vec![0u64; 512];
    let t = unsafe { HashAgg::at(region.as_mut_ptr() as *mut u8, &layout, 8, 4096) as *mut HashAgg };
    let mut recs = Vec::new();
    let mut buf = [0u8; 256];
    for k in 0..20u64 {
        make_record(&mut buf, &layout, &[k], k);
        recs.extend_from_slice(&buf[..layout.size() as usize]);
    }
    let mut cand = Candidates::none();
    let v = unsafe { fold::owner_fold(core::ptr::null(), t, recs.as_ptr(), 20, SubSplit::NONE, &mut cand) };
    assert_eq!(v, -1 - 7, "7 of 8 slots fill, the eighth key is refused");
    assert_eq!(unsafe { (*t).len }, 7);
    let mut sum = 0;
    for s in 0..8u32 {
        let t2 = unsafe { HashAgg::at((region.as_mut_ptr() as *mut u8).add(2048), &layout, 64, 2048) as *mut HashAgg };
        unsafe { (*t2).clear() };
        sum += unsafe { fold::owner_fold(core::ptr::null(), t2, recs.as_ptr(), 20, SubSplit { pbits: 0, depth: 1, reread: 0, sub: s as u64 }, &mut cand) };
    }
    assert_eq!(sum, 20, "the split's children partition the run");
    let _ = MOVING;
}

/// A fold slowed to ~1 ms a run of records: the owner phase over `sources` takes seconds (D283's forced slow fold).
unsafe fn slow_fold(params: *const u64, table: *mut HashAgg, recs: *const u8, nrecs: usize, split: SubSplit, cand: *mut Candidates) -> i64 {
    let t = std::time::Instant::now();
    while t.elapsed() < std::time::Duration::from_millis(1) {
        std::hint::spin_loop();
    }
    fold::owner_fold(params, table, recs, nrecs, split, cand)
}

/// D283: the owner phase checks the statement's abort byte (`Ctx::cancel`) per run of records: a cancel or a
/// statement_timeout that sets it mid-fold ends `own_partition` with `Error::Cancelled` (raised as PG's 57014) within a
/// run, not after every partition (the parent folded on for seconds: its loops never looked at the byte).
#[test]
fn a_pending_cancel_stops_the_owner_fold_within_a_run() {
    let layout = RecordLayout::new_nn(&[KeyKind::I64], &[SlotKind::Count]);
    let src = sources(&layout, 9000, 1 << 40, 0xd283);
    let merge = HashAggPartial::new(0, Some(fns(Some(slow_fold), 2, false)));
    let partials: Vec<Partial<'_>> = src.regions.iter().enumerate().map(|(o, r)| Partial { ordinal: o as u16, bytes: unsafe { core::slice::from_raw_parts(r.as_ptr() as *const u8, WORKER_BYTES) }, rows_processed: 0 }).collect();
    let files: Vec<&dyn SpillFile> = src.files.iter().map(|f| f.as_ref()).collect();
    let mut region = vec![0u64; (64 << 10) / 8];
    let owner = Region { ptr: region.as_mut_ptr() as *mut u8, len: 64 << 10 };
    merge.open_owner(owner, 0).unwrap();
    let abort = std::sync::atomic::AtomicU8::new(0);
    let set_at = std::sync::OnceLock::new();
    let (r, ended) = std::thread::scope(|s| {
        s.spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(50));
            set_at.set(std::time::Instant::now()).unwrap();
            abort.store(1, std::sync::atomic::Ordering::Release);
        });
        let mut r = Ok(());
        for part in 0..1u32 << PBITS {
            r = merge.own_partition(owner, part, PBITS, &partials, &files, core::ptr::null_mut(), abort.as_ptr(), 1, &mut || Ok(()));
            if r.is_err() {
                break;
            }
        }
        (r, std::time::Instant::now())
    });
    assert_eq!(r, Err(gravity_driver::Error::Cancelled), "the fold ran on past the abort byte");
    let late = ended.duration_since(*set_at.get().expect("the byte was set before the fold ended"));
    assert!(late < std::time::Duration::from_millis(100), "the fold stopped {late:?} after the byte was set");
}

/// V4R-375's order test: the worker regions' bases and each run's source ('F' a file, 'C' a chain, 'R' a table record).
static AREAS: std::sync::Mutex<(Vec<usize>, Vec<u8>)> = std::sync::Mutex::new((Vec::new(), Vec::new()));

/// A fold that notes where each run comes from and folds nothing (the table stays empty).
unsafe fn noting_fold(_params: *const u64, _t: *mut HashAgg, recs: *const u8, nrecs: usize, _split: SubSplit, _cand: *mut Candidates) -> i64 {
    let mut a = AREAS.lock().unwrap();
    let p = recs as usize;
    let from = a.0.iter().find(|&&b| p >= b && p < b + WORKER_BYTES).map(|&b| {
        let t1 = unsafe { T1::view(b as *const u8) };
        if p >= b + t1.pool_off as usize { b'C' } else { b'R' }
    });
    a.1.push(from.unwrap_or(b'F'));
    nrecs as i64
}

/// The runs' sources in the order `fold_sub` reads them, per partition, over `layout`'s partials.
fn source_order(layout: &RecordLayout) -> Vec<Vec<u8>> {
    let src = sources(layout, 3000, 1 << 20, 0x0de5);
    AREAS.lock().unwrap().0 = src.regions.iter().map(|r| r.as_ptr() as usize).collect();
    let merge = HashAggPartial::new(0, Some(fns(Some(noting_fold), 2, false)));
    let partials: Vec<Partial<'_>> = src.regions.iter().enumerate().map(|(o, r)| Partial { ordinal: o as u16, bytes: unsafe { core::slice::from_raw_parts(r.as_ptr() as *const u8, WORKER_BYTES) }, rows_processed: 0 }).collect();
    let files: Vec<&dyn SpillFile> = src.files.iter().map(|f| f.as_ref()).collect();
    let mut region = vec![0u64; (256 << 10) / 8];
    let owner = Region { ptr: region.as_mut_ptr() as *mut u8, len: 256 << 10 };
    merge.open_owner(owner, 0).unwrap();
    (0..1u32 << PBITS)
        .map(|part| {
            AREAS.lock().unwrap().1.clear();
            merge.own_partition(owner, part, PBITS, &partials, &files, core::ptr::null_mut(), core::ptr::null(), 1, &mut || Ok(())).unwrap();
            AREAS.lock().unwrap().1.clone()
        })
        .collect()
}

/// V4R-375: a table holding hosted states (`AggState` slots and the handle's `MaxI64` after them) reads a partition oldest
/// first, the files' epochs, then the chains, then the tables' records; every other table keeps its order (the chains and
/// the tables' records worker by worker, then the files).
#[test]
fn a_hosted_table_reads_its_partition_oldest_first_and_every_other_keeps_its_order() {
    let plain = RecordLayout::new(&[KeyKind::I64], &[SlotKind::Count, SlotKind::SumI64]);
    let hosted = RecordLayout::new(&[KeyKind::I64], &[SlotKind::Count, SlotKind::AggState, SlotKind::MaxI64]);
    for (layout, files_first) in [(plain, false), (hosted, true)] {
        let orders = source_order(&layout);
        assert!(orders.iter().any(|o| o.contains(&b'F') && o.contains(&b'C') && o.contains(&b'R')), "the sources have all three kinds");
        for o in orders {
            let (first_f, last_f) = (o.iter().position(|&c| c == b'F'), o.iter().rposition(|&c| c == b'F'));
            let (first_m, last_m) = (o.iter().position(|&c| c != b'F'), o.iter().rposition(|&c| c != b'F'));
            if let (Some(ff), Some(lf), Some(fm), Some(lm)) = (first_f, last_f, first_m, last_m) {
                match files_first {
                    true => assert!(lf < fm, "hosted: the files before the chains and records: {:?}", String::from_utf8_lossy(&o)),
                    false => assert!(lm < ff, "plain: the chains and records before the files: {:?}", String::from_utf8_lossy(&o)),
                }
            }
        }
    }
}
