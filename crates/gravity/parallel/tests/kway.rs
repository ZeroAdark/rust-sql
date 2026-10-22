mod common;

use std::cell::Cell;

use common::*;
use gravity_abi::Datum;
use gravity_driver::{Merge, Partial, SinkRef, State};
use gravity_parallel::{kway_merge, RowsOrdered, SortedRun};

fn run_of(cap: u32, keys: &[u64]) -> Vec<u64> {
    let mut buf = vec![0u64; SortedRun::bytes(2, cap).div_ceil(8) as usize];
    let r = unsafe { SortedRun::at(buf.as_mut_ptr() as *mut u8) };
    r.natts = 2;
    for (i, k) in keys.iter().enumerate() {
        assert!(r.push(cap, &[*k, i as u64], &[0, 0]));
    }
    buf
}

#[test]
fn the_k_way_merge_equals_the_repeated_two_way_merge_with_far_fewer_comparisons() {
    let (k, n, cap) = (16usize, 1000u32, 1000u32);
    let runs: Vec<Vec<u64>> = (0..k as u64).map(|r| run_of(cap, &(0..n as u64).map(|i| (i * 7 + r * 3) % 5000).collect::<std::collections::BTreeSet<_>>().into_iter().collect::<Vec<_>>())).collect();
    let views: Vec<&SortedRun> = runs.iter().map(|b| unsafe { &*(b.as_ptr() as *const SortedRun) }).collect();
    let total: usize = views.iter().map(|r| r.nrows as usize).sum();
    // the repeated two-way merge (RowsOrdered) as the reference
    let m = RowsOrdered { natts: 2, cap_rows: total as u32, keys: [0, 0, 0, 0], nkeys: 1 };
    let mut state = vec![0u8; 2 * SortedRun::bytes(2, total as u32) as usize + 8];
    let mut sink = LegSink::new(true);
    for i in 0..runs.len() {
        // a run copied into a `total`-capacity layout so RowsOrdered's row() arithmetic matches
        let wide = run_of(total as u32, &(0..views[i].nrows as usize).map(|j| views[i].row(j, cap).0[0]).collect::<Vec<_>>());
        let bytes = unsafe { core::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 8) };
        m.merge_into(&mut State::new(&mut state, SinkRef::of(&mut sink)), Partial { ordinal: i as u16, bytes, rows_processed: 0 }).unwrap();
    }
    m.finish(&mut State::new(&mut state, SinkRef::of(&mut sink))).unwrap();
    let two_way: Vec<u64> = sink.rows.lock().unwrap().iter().map(|r| r[0]).collect();
    assert_eq!(two_way.len(), total);
    // the k-way merge
    let comparisons = Cell::new(0u64);
    let mut out: Vec<u64> = Vec::with_capacity(total);
    kway_merge(
        &views,
        cap,
        |a: (&[Datum], &[u8]), b: (&[Datum], &[u8])| {
            comparisons.set(comparisons.get() + 1);
            a.0[0] < b.0[0]
        },
        |row| {
            out.push(row.0[0]);
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(out, two_way, "the same ordered stream");
    assert!(out.windows(2).all(|w| w[0] <= w[1]));
    let c = comparisons.get();
    assert!(c < total as u64 * 12, "{c} comparisons for {total} rows over {k} runs: O(n log k), not O(n k)");
    assert!(c > total as u64, "every row costs at least one comparison");
}

#[test]
fn ties_come_out_in_run_order_and_empty_runs_are_skipped() {
    let a = run_of(4, &[1, 5, 5]);
    let b = run_of(4, &[]);
    let c = run_of(4, &[5, 9]);
    let views: Vec<&SortedRun> = [&a, &b, &c].iter().map(|v| unsafe { &*(v.as_ptr() as *const SortedRun) }).collect();
    let mut out = Vec::new();
    kway_merge(&views, 4, |x, y| x.0[0] < y.0[0], |row| {
        out.push((row.0[0], row.0[1]));
        Ok(())
    })
    .unwrap();
    assert_eq!(out, vec![(1, 0), (5, 1), (5, 2), (5, 0), (9, 1)], "5s from run a (rows 1, 2) before run c's (row 0)");
}
