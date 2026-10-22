//! V4R-121: `Node::put_sort_run` / `Node::sort_runs`, the leader's put of a sort run exec v4 holds (MergeAppend's arm runs).
use gravity_driver_rig as common;

use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use common::*;
use gravity_abi::breaker::{SortRun, SORTRUN_HDR, TABLE_SORT};
use gravity_driver::{install_spill_factory, Entry, Grant};
use gravity_program::{Generation, Handle, PlanKey};

const KEY: PlanKey = PlanKey { handle: Handle(11), generation: Generation(1) };

/// A fixed sort run of `n` 16-byte rows in an 8-aligned buffer.
fn run_of(n: u32) -> Vec<u64> {
    let (rb, cap) = (16u32, n.max(64));
    let mut buf = vec![0u64; (SORTRUN_HDR + (cap * rb) as usize + 64) / 8]; // alloc: Exec
    let p = buf.as_mut_ptr() as *mut u8;
    unsafe {
        let r = &mut *(p as *mut SortRun);
        (r.cap, r.row_bytes, r.kind) = (cap, rb, TABLE_SORT);
        r.clear();
        for i in 0..n {
            let row = r.rows_ptr().add((i * rb) as usize) as *mut u64;
            (*row, *row.add(1)) = (1, i as u64);
            r.push();
        }
    }
    buf
}

#[test]
fn a_held_run_put_by_the_leader_is_one_more_run_of_the_slots_stream() {
    install_spill_factory(&MEM_SPILL);
    let program = Arc::new(p_group_spill(64));
    let table = RangeTable::new(4, 4);
    let mut derive = FakeDerive::ranges(table);
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let params = [7u64, 3];
    let mut b = binding(&params, &[0, 0], &mut sink, &flag);
    let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(Grant::of(64 << 20))).unwrap();
    let node = exec.node_mut();
    assert_eq!(node.sort_runs(0), 0);
    let mut a = run_of(5);
    let mut e = run_of(0);
    unsafe {
        assert_eq!(node.put_sort_run(0, a.as_mut_ptr() as *mut u8).unwrap(), 0);
        assert_eq!(node.put_sort_run(0, e.as_mut_ptr() as *mut u8).unwrap(), 1, "an empty run is a run");
        assert_eq!((*(a.as_ptr() as *const SortRun)).len, 0, "the put cleared the run");
    }
    assert_eq!(node.sort_runs(0), 2);
    assert!(node.spilled());
    entry.end(exec, false);
}

/// V4R-136 (D463): a stream counts its runs' rows at each put (every owner's, summed: W8's workers and the leader's put of
/// a MergeAppend held run alike) and an adoption moves them with the runs; a new life starts at 0.
#[test]
fn a_stream_counts_its_runs_rows_across_owners_and_an_adoption_moves_them() {
    install_spill_factory(&MEM_SPILL);
    let mut t = gravity_driver::SpillTable::with_slots(2);
    t.begin_life(0);
    t.begin_life(1);
    unsafe {
        for (owner, n) in [(1usize, 5u32), (2, 7), (1, 3), (8, 0)] {
            let mut r = run_of(n);
            t.run_put(0, owner, r.as_mut_ptr() as *mut u8).unwrap();
        }
        let mut held = run_of(11);
        t.run_put(0, 0, held.as_mut_ptr() as *mut u8).unwrap();
    }
    assert_eq!((t.stream(0).total_runs(), t.stream(0).total_rows()), (5, 26));
    assert_eq!(t.adopt_runs(0, 1).unwrap(), 5);
    assert_eq!((t.stream(0).total_rows(), t.stream(1).total_rows()), (0, 26), "the rows moved with the runs");
    t.begin_life(1);
    assert_eq!(t.stream(1).total_rows(), 0);
}

/// Main 09-25 (MERGE's part-034 SIGSEGV): two child sorts on one reused slot, each past a 4096-row window, one spilled
/// (runs put by two owners) and one in memory: the second life starts from no runs and no rows, never the first's.
#[test]
fn a_reused_slots_second_sort_counts_only_its_own_runs() {
    install_spill_factory(&MEM_SPILL);
    let mut t = gravity_driver::SpillTable::with_slots(1);
    t.begin_life(0);
    unsafe {
        for (owner, n) in [(1usize, 4097u32), (2, 5000)] {
            let mut r = run_of(n);
            t.run_put(0, owner, r.as_mut_ptr() as *mut u8).unwrap();
        }
    }
    assert_eq!((t.stream(0).total_runs(), t.stream(0).total_rows()), (2, 9097));
    t.end_life(0);
    t.begin_life(0);
    assert_eq!((t.stream(0).total_runs(), t.stream(0).total_rows()), (0, 0), "the second child: in memory, nothing put");
    unsafe {
        let mut r = run_of(6000);
        t.run_put(0, 3, r.as_mut_ptr() as *mut u8).unwrap();
    }
    assert_eq!((t.stream(0).total_runs(), t.stream(0).total_rows()), (1, 6000), "spilled again: its own run only");
}

/// D2572-d: a run's raw-or-compact choice weighs every stream's put-run bytes (`put_raw_bytes`), counted at each put
/// through an atomic: at W8 the owners put at once, each into its own runs, and the count never walks another owner's
/// files (`spilled_bytes` read the chains other owners were growing: a W8 sort's SIGSEGV). Eight owners putting at once
/// count what one thread putting the same runs counts; an adoption moves the bytes, a new life starts at 0.
#[test]
fn d2572d_put_raw_bytes_count_every_owners_runs_put_at_once() {
    install_spill_factory(&MEM_SPILL);
    let lens = |owner: usize| (0..20u32).map(move |i| owner as u32 * 10 + i);
    let mut serial = gravity_driver::SpillTable::with_slots(1);
    serial.begin_life(0);
    unsafe {
        for owner in 1..=8usize {
            for n in lens(owner) {
                let mut r = run_of(n);
                serial.run_put(0, owner, r.as_mut_ptr() as *mut u8).unwrap();
            }
        }
    }
    let mut t = gravity_driver::SpillTable::with_slots(2);
    t.begin_life(0);
    t.begin_life(1);
    unsafe {
        let mut r = run_of(0);
        t.run_put(0, 8, r.as_mut_ptr() as *mut u8).unwrap(); // every owner's entries provisioned before the section
    }
    assert_eq!(t.put_raw_bytes(), 0, "an empty run adds no bytes");
    let tp = &mut t as *mut gravity_driver::SpillTable as usize;
    std::thread::scope(|s| {
        for owner in 1..=8usize {
            s.spawn(move || {
                for n in lens(owner) {
                    let mut r = run_of(n);
                    // SAFETY: as the section's workers: each owner puts into its own runs of the shared table
                    unsafe { (*(tp as *mut gravity_driver::SpillTable)).run_put(0, owner, r.as_mut_ptr() as *mut u8).unwrap() };
                }
            });
        }
    });
    let total = serial.put_raw_bytes();
    assert!(total > 0);
    assert_eq!(t.put_raw_bytes(), total, "no put lost between owners putting at once");
    assert_eq!(t.adopt_runs(0, 1).unwrap(), 161);
    assert_eq!(t.put_raw_bytes(), total, "an adoption moves the bytes with the runs");
    t.begin_life(1);
    assert_eq!(t.put_raw_bytes(), 0, "a new life starts from none");
}
