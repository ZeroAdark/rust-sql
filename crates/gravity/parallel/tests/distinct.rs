mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use common::*;
use gravity_driver::Entry;
use gravity_kernels::par_distinct;
use gravity_program::hand as programs;
use gravity_parallel::FixturePool;

/// `(k, x)` with 300 groups and a pseudo-random `x` from 400 values: ~210 distinct per group, many repeats.
fn gen_pairs(r: u64) -> Option<[i64; 2]> {
    Some([(r % 300) as i64, ((r.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 40) % 400) as i64])
}

#[test]
fn count_distinct_per_group_is_identical_at_every_width_in_memory_and_through_partitions() {
    install_mem_spill();
    let table = FixtureTable::new(T_KV, 900, 100, gen_pairs);
    let mut derive = FixtureDerive::new(&[&table]);
    let program = Arc::new(programs::par_distinct_with(T_KV, 16, 90_000, 120_000));
    let words = par_distinct::params(120_000);
    let pool = FixturePool::new(8);
    let mut entry = Entry::new(2);
    let mut sets: BTreeMap<i64, BTreeSet<i64>> = BTreeMap::new();
    for [k, x] in table.rows() {
        sets.entry(k).or_default().insert(x);
    }
    let expect: Vec<Vec<u64>> = sets.iter().map(|(k, s)| vec![*k as u64, s.len() as u64]).collect();
    assert!(expect.iter().map(|r| r[1]).sum::<u64>() > 50_000, "a set worth checking");
    for (grant, want_spill) in [(64u64 << 20, false), (1, true)] {
        for w in WIDTHS {
            table.fresh();
            let plan = FixturePlan::new(&program, &derive, w, 16);
            let mut sink = LegSink::new(true);
            let (n, spilled, parts, _) = run_spilling(&mut entry, &program, &mut derive, &pool, &plan, gravity_driver::Grant::of(grant), &words, &NO_ABORT, &mut sink).unwrap();
            assert_eq!((n as usize, spilled, parts > 0), (expect.len(), want_spill, want_spill), "grant {grant} width {w}");
            assert_eq!(sink.rows(), expect, "grant {grant} width {w}: every group's distinct count, in key order");
        }
    }
}
