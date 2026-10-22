//! parthaving's floor over any count final: the HAVING's lower bound may sit on a count other than the record's row count
//! (`count(x)` over a nullable x, a filtered count). The owner sums that slot over a group's parts and keeps the groups
//! at the floor; every other slot of a kept group folds as the parts merge. A group whose row count passes the floor
//! while its `count(x)` does not leaves the owner, and the reverse never happens.
use std::collections::BTreeMap;

use gravity_abi::breaker::{HashAgg, KeyKind, OwnerTop, Rec, RecordLayout, SlotKind, OWNERTOP_HDR};
use gravity_kernels::dict::reps::{MAX_REPS, REP_NONE};
use gravity_kernels::ops::sort::topn::parthaving::{owner_fns, PartHavingSpec};

/// Arrivals `(group, part, rows, xs)` folded into one table, reduced under `floor` on slot 1: the kept groups ->
/// (summed rows, summed xs).
fn kept(floor: i64, arrivals: &[(u64, u64, i64, i64)]) -> BTreeMap<u64, (i64, i64)> {
    let layout = RecordLayout::new(&[KeyKind::I64, KeyKind::I64], &[SlotKind::Count, SlotKind::Count]).partitioned_by(1);
    let cap = 1u32 << 10;
    let limit = HashAgg::bytes(&layout, cap);
    let mut tbuf = vec![0u64; limit.div_ceil(8) as usize];
    let t = unsafe { HashAgg::at(tbuf.as_mut_ptr() as *mut u8, &layout, cap, limit) };
    for &(g, part, rows, xs) in arrivals {
        let r = t.find_or_insert(&[g, part]).expect("room");
        r.add_count(0, rows);
        r.add_count(1, xs);
    }
    let fns = owner_fns(&PartHavingSpec { layout, mins: [(REP_NONE, 0); MAX_REPS], count_slot: 1, bits: 0, floor });
    let bytes = (fns.min_bytes)(fns.params.as_ptr(), 1) as usize + 2 * limit as usize;
    let mut obuf = vec![0u64; bytes.div_ceil(8)];
    let owner = obuf.as_mut_ptr() as *mut u8;
    unsafe {
        assert!((fns.open)(fns.params.as_ptr(), owner, bytes) > 0, "the owner opens");
        assert_eq!((fns.reduce)(fns.params.as_ptr(), owner, t as *const HashAgg, core::ptr::null_mut(), core::ptr::null(), -1), 0, "the reduce");
    }
    let h = unsafe { &*(owner as *const OwnerTop) };
    let rec = layout.size() as usize;
    let mut out = BTreeMap::new();
    for run in 0..h.worst as usize {
        let (off, m) = unsafe { (*(owner.add(OWNERTOP_HDR) as *const u32).add(2 * run), *(owner.add(OWNERTOP_HDR) as *const u32).add(2 * run + 1)) };
        for i in 0..m as usize {
            let r = unsafe { Rec::at(owner.add(off as usize + i * rec), &layout) };
            assert!(out.insert(r.key(0), (r.count(0), r.count(1))).is_none(), "a group kept once");
        }
    }
    out
}

#[test]
fn the_floor_reads_the_count_it_names_not_the_row_count() {
    // group 1: 30 rows, 9 non-NULL x over three parts; group 2: 12 rows, 12 x; group 3: 10 rows, 10 x in one part
    let arrivals = [(1, 0, 10, 3), (1, 1, 10, 3), (1, 2, 10, 3), (2, 0, 6, 6), (2, 3, 6, 6), (3, 1, 10, 10)];
    let got = kept(10, &arrivals);
    let want: BTreeMap<u64, (i64, i64)> = [(2, (12, 12)), (3, (10, 10))].into_iter().collect();
    assert_eq!(got, want, "count(x) >= 10 keeps groups 2 and 3, never group 1 (30 rows, 9 x)");
    assert_eq!(kept(13, &arrivals), BTreeMap::new(), "no group has 13 x");
    assert_eq!(kept(9, &arrivals).len(), 3, "at 9 every group's x reaches the floor");
}
