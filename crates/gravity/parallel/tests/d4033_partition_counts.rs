//! D4033: the owner phase reads every phase-1 partial at one partition count. A partial at another count (a worker share
//! the pool's room opened smaller, cut by its own limit before the fix) is refused by name and never read: its words past
//! its own partitions are no chains (the over-read that ended in a SIGSEGV), and a chain below them holds other
//! partitions' records (a group folded into two owner partitions: two rows for one group). Partials at one count fold
//! every record once.

use gravity_abi::breaker::{HashAgg, KeyKind, OwnerFns, OwnerTop, Rec, RecordLayout, SlotKind, T1, ORDER_NONE, OWNERTOP_HDR, OWNER_PARAMS};
use gravity_driver::{Merge, Partial, Region, SpillFile};
use gravity_parallel::HashAggPartial;

const WORKER_BYTES: usize = 96 << 10;
const OWNER_BYTES: usize = 64 << 10;
const KEYS: u64 = 300;

unsafe fn t_open(_params: *const u64, owner: *mut u8, _bytes: usize) -> i64 {
    core::ptr::write_bytes(owner, 0, OWNERTOP_HDR);
    (*(owner as *mut OwnerTop)).table_off = 64;
    0
}

/// Each group once: `groups` += the table's groups, `records` += their counts.
unsafe fn t_reduce(_params: *const u64, owner: *mut u8, table: *const HashAgg, _site: *mut gravity_abi::Site, _cand: *const u32, _ncand: i64) -> i64 {
    let t = &*table;
    let top = owner as *mut OwnerTop;
    for i in 0..t.cap as usize {
        let r = t.record(i);
        if r.used() {
            (*top).groups += 1;
            (*top).records += r.count(0) as u64;
        }
    }
    0
}

unsafe fn t_noop(_params: *const u64, _owner: *mut u8) -> i64 {
    0
}

fn t_min(_params: *const u64, _owners: u32) -> u64 {
    0
}

fn fns() -> OwnerFns {
    OwnerFns { params: [0; OWNER_PARAMS], rounds: 1, order_slot: ORDER_NONE, order_slot2: ORDER_NONE, min_bytes: t_min, open: t_open, reduce: t_reduce, end_round: t_noop, reduce2: t_reduce, fold: None }
}

fn layout() -> RecordLayout {
    RecordLayout::new(&[KeyKind::I64], &[SlotKind::Count])
}

/// A worker's phase-1 table at `pbits` partitions: every key of `0..KEYS` appended once to its partition's chain.
fn partial(layout: &RecordLayout, pbits: u8) -> Vec<u64> {
    let mut region = vec![0u64; WORKER_BYTES / 8];
    let t = unsafe { T1::at(region.as_mut_ptr() as *mut u8, layout, pbits, 8192, WORKER_BYTES as u32) as *mut T1 };
    let mut buf = [0u8; 64];
    for k in 0..KEYS {
        buf.fill(0);
        let r = unsafe { Rec::at(buf.as_mut_ptr(), layout) };
        r.open(&[k]);
        r.add_count(0, 1);
        let part = r.route().part(pbits as u32);
        assert!(unsafe { T1::append_bytes(t, part, buf.as_ptr()) }, "the pool holds {KEYS} records");
    }
    region
}

/// The owner phase over `regions` at the partials' partition count: each partition's result, then the groups and records
/// folded.
fn own(regions: &[Vec<u64>]) -> (Vec<Result<(), String>>, u64, u64) {
    let merge = HashAggPartial::new(0, Some(fns()));
    let partials: Vec<Partial<'_>> = regions.iter().enumerate().map(|(o, r)| Partial { ordinal: o as u16, bytes: unsafe { core::slice::from_raw_parts(r.as_ptr() as *const u8, WORKER_BYTES) }, rows_processed: 0 }).collect();
    let pbits = merge.partition_bits(&partials).expect("phase-1 partials");
    let files: Vec<&dyn SpillFile> = Vec::new();
    let mut region = vec![0u64; OWNER_BYTES / 8];
    let owner = Region { ptr: region.as_mut_ptr() as *mut u8, len: OWNER_BYTES };
    merge.open_owner(owner, 0).unwrap();
    let got = (0..1u32 << pbits).map(|part| merge.own_partition(owner, part, pbits, &partials, &files, core::ptr::null_mut(), core::ptr::null(), 1, &mut || Ok(())).map_err(|e| format!("{e:?}"))).collect();
    let top = unsafe { OwnerTop::view(owner.ptr) };
    (got, top.groups, top.records)
}

#[test]
fn partials_at_one_partition_count_fold_every_record_once() {
    let l = layout();
    let (got, groups, records) = own(&[partial(&l, 3), partial(&l, 3)]);
    assert!(got.iter().all(Result::is_ok), "{got:?}");
    assert_eq!((groups, records), (KEYS, 2 * KEYS), "each key one group of both partials' records");
}

#[test]
fn a_partial_at_fewer_partitions_is_refused_by_name_and_never_read() {
    let l = layout();
    let (got, groups, records) = own(&[partial(&l, 3), partial(&l, 2)]);
    for (part, r) in got.iter().enumerate() {
        let e = r.as_ref().expect_err("a partial at another partition count is refused");
        assert!(e.contains("a phase-1 partial at 4 partitions where its owner phase reads 8"), "partition {part}: {e}");
    }
    assert_eq!((groups, records), (0, 0), "no partition folded");
}
