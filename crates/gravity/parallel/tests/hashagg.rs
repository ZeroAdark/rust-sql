mod common;

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use common::*;
use gravity_driver::Entry;
use gravity_program::hand as programs;
use gravity_parallel::FixturePool;

#[test]
fn hashagg_groups_are_identical_at_every_width() {
    let table = FixtureTable::new(T_KV, 160, 60, gen_kv);
    let mut derive = FixtureDerive::new(&[&table]);
    let program = Arc::new(programs::par_hashagg_with(T_KV, 16, 9600));
    let pool = FixturePool::new(8);
    let mut entry = Entry::new(2);
    let mut groups: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
    for [k, v] in table.rows() {
        let g = groups.entry(k).or_insert((0, 0));
        g.0 += v;
        g.1 += 1;
    }
    let mut expect: Vec<Vec<u64>> = groups.iter().map(|(k, (s, c))| vec![*k as u64, *s as u64, *c as u64]).collect();
    expect.sort();
    for w in WIDTHS {
        for claim in [2u64, 33] {
            table.fresh();
            let mut sink = LegSink::new(true);
            let rows = run_once(&mut entry, &program, &mut derive, &pool, w, claim, &gravity_kernels::table::params::DEFAULT_PARAMS, &NO_ABORT, &mut sink).unwrap();
            assert_eq!(rows, 97, "width {w}");
            assert_eq!(sink.sorted(), expect, "width {w} claim {claim}");
            assert!(table.ledger.consumed_once(), "width {w} claim {claim}");
            assert_eq!(table.pins.load(Ordering::Relaxed), 0);
        }
    }
}

/// A serial source (decompose.md decision 81: a sorted pgrc scan, like an index scan) runs its whole relation
/// in one call on the leader at every granted width, then the owner fold and the finish every producer gets:
/// the consumer finds the owned result, never the raw partial.
#[test]
fn a_serial_source_into_a_hashagg_is_folded_on_the_leader_at_every_width() {
    let table = FixtureTable::new(T_KV, 160, 60, gen_kv);
    let mut derive = FixtureDerive::new(&[&table]);
    let mut program = programs::par_hashagg_with(T_KV, 16, 9600);
    program.pipelines[0].parallel = gravity_program::Parallel::Ineligible { site: 0, reason: gravity_program::Reason::SortedSource };
    let program = Arc::new(program);
    let pool = FixturePool::new(8);
    let mut entry = Entry::new(2);
    let mut groups: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
    for [k, v] in table.rows() {
        let g = groups.entry(k).or_insert((0, 0));
        g.0 += v;
        g.1 += 1;
    }
    let mut expect: Vec<Vec<u64>> = groups.iter().map(|(k, (s, c))| vec![*k as u64, *s as u64, *c as u64]).collect();
    expect.sort();
    for w in WIDTHS {
        table.fresh();
        let mut sink = LegSink::new(true);
        let rows = run_once(&mut entry, &program, &mut derive, &pool, w, 33, &gravity_kernels::table::params::DEFAULT_PARAMS, &NO_ABORT, &mut sink).unwrap();
        assert_eq!(rows, 97, "width {w}");
        assert_eq!(sink.sorted(), expect, "width {w}");
        assert!(table.ledger.consumed_once(), "width {w}: one whole-relation read");
        assert_eq!(table.pins.load(Ordering::Relaxed), 0);
    }
}

/// `FixturePlan` with the kernel's own merge words: the zone top-k (the merge proves the walk's
/// bound, the owners keep the `n` smallest keys, the emit cuts) or the ordered form.
struct KeyedPlan {
    inner: FixturePlan,
    merge: gravity_parallel::HashAggPartial,
}

impl KeyedPlan {
    fn new(inner: FixturePlan, params: &[u64], n: u32) -> KeyedPlan {
        KeyedPlan { inner, merge: gravity_parallel::HashAggPartial::new(n, Some(gravity_kernels::table::params::owner_fns(params))) }
    }
}

impl gravity_parallel::Plan for KeyedPlan {
    fn width_of(&self, p: usize) -> u16 {
        self.inner.width_of(p)
    }
    fn claims_for(&self, p: usize) -> &dyn gravity_parallel::MorselSource {
        self.inner.claims_for(p)
    }
    fn merge_for(&self, _p: usize) -> &dyn gravity_driver::Merge {
        &self.merge
    }
}

fn gen_unique(r: u64) -> Option<[i64; 2]> {
    Some([((r * 7919) % 100_003) as i64, (r % 1000) as i64])
}

#[test]
fn the_bounded_export_keeps_the_n_smallest_groups_complete_without_spilling_at_every_width() {
    use gravity_kernels::table::params::{DEFAULT_PARAMS, PARAM_TOPK_N};
    // 96,000 near-unique keys: far past a 64 kB share's table, so an unbounded fold would spill
    let table = FixtureTable::new(T_KV, 1600, 60, gen_unique);
    let mut derive = FixtureDerive::new(&[&table]);
    let program = Arc::new(programs::par_hashagg_with(T_KV, 16, 96_000));
    let pool = FixturePool::new(8);
    let mut entry = Entry::new(2);
    let n = 3u32;
    let mut groups: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
    for [k, v] in table.rows() {
        let g = groups.entry(k).or_insert((0, 0));
        g.0 += v;
        g.1 += 1;
    }
    let expect: Vec<Vec<u64>> = groups.iter().take(n as usize).map(|(k, (s, c))| vec![*k as u64, *s as u64, *c as u64]).collect();
    let mut params = DEFAULT_PARAMS;
    params[PARAM_TOPK_N] = n as u64;
    for w in WIDTHS {
        table.fresh();
        let plan = KeyedPlan::new(FixturePlan::new(&program, &derive, w, 16), &params, n);
        let mut sink = LegSink::new(true);
        let nulls = [0u8; 16];
        let mut b = gravity_driver::Binding::new(parts(&params, &nulls, &NO_ABORT), &mut sink);
        let mut exec = entry.start(None, &program, &mut derive, &mut b, std::rc::Rc::new(gravity_driver::Reservation::unadmitted(gravity_driver::Grant::of(1)))).unwrap();
        let r = gravity_parallel::run_program(&mut entry, &mut exec, &mut b, &mut derive, &pool, &plan);
        let spilled = exec.node().spilled();
        entry.end(exec, r.is_ok());
        assert_eq!(r.unwrap(), n as u64, "width {w}");
        assert!(!spilled, "width {w}: the bounded export never spills");
        assert_eq!(sink.sorted(), expect, "width {w}: the {n} smallest groups, complete");
    }
}

#[test]
fn the_dense_front_gives_the_same_groups_at_every_width() {
    use gravity_kernels::table::params::{DEFAULT_PARAMS, PARAM_DENSE_HI, PARAM_DENSE_LO};
    let table = FixtureTable::new(T_KV, 160, 60, gen_kv);
    let mut derive = FixtureDerive::new(&[&table]);
    let program = Arc::new(programs::par_hashagg_with(T_KV, 16, 9600));
    let pool = FixturePool::new(8);
    let mut entry = Entry::new(2);
    let mut groups: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
    for [k, v] in table.rows() {
        let g = groups.entry(k).or_insert((0, 0));
        g.0 += v;
        g.1 += 1;
    }
    let mut expect: Vec<Vec<u64>> = groups.iter().map(|(k, (s, c))| vec![*k as u64, *s as u64, *c as u64]).collect();
    expect.sort();
    // gen_kv's keys span [-3, 93]: the exact domain and a wider one
    for (lo, hi) in [(-3i64, 93i64), (-1000, 1000)] {
        let mut params = DEFAULT_PARAMS;
        params[PARAM_DENSE_LO] = lo as u64;
        params[PARAM_DENSE_HI] = hi as u64;
        for w in WIDTHS {
            table.fresh();
            let mut sink = LegSink::new(true);
            let rows = run_once(&mut entry, &program, &mut derive, &pool, w, 33, &params, &NO_ABORT, &mut sink).unwrap();
            assert_eq!(rows, 97, "width {w} domain [{lo}, {hi}]");
            assert_eq!(sink.sorted(), expect, "width {w} domain [{lo}, {hi}]");
        }
    }
}

#[test]
fn a_spilled_aggregate_emits_in_key_order_through_sorted_runs_at_every_width() {
    use gravity_kernels::table::params::{DEFAULT_PARAMS, PARAM_ORDER};
    install_mem_spill();
    // 96,000 near-unique groups against 64 kB shares: every partition refills, sorts, and goes to a
    // run; the emit merges the runs in key order (a stop mid-merge resumes in EMIT: out_rows 16)
    let table = FixtureTable::new(T_KV, 1600, 60, gen_unique);
    let mut derive = FixtureDerive::new(&[&table]);
    let program = Arc::new(programs::par_hashagg_with(T_KV, 16, 96_000));
    let pool = FixturePool::new(8);
    let mut entry = Entry::new(2);
    let mut groups: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
    for [k, v] in table.rows() {
        let g = groups.entry(k).or_insert((0, 0));
        g.0 += v;
        g.1 += 1;
    }
    let expect: Vec<Vec<u64>> = groups.iter().map(|(k, (s, c))| vec![*k as u64, *s as u64, *c as u64]).collect();
    let mut params = DEFAULT_PARAMS;
    params[PARAM_ORDER] = 1;
    for (grant, want_spill) in [(64u64 << 20, false), (1, true)] {
        for w in WIDTHS {
            table.fresh();
            let plan = KeyedPlan::new(FixturePlan::new(&program, &derive, w, 16), &params, 0);
            let mut sink = LegSink::new(true);
            let sets = common::SPILL_SETS_CREATED.load(std::sync::atomic::Ordering::Relaxed);
            let (rows, spilled, parts, _) = run_spilling(&mut entry, &program, &mut derive, &pool, &plan, gravity_driver::Grant::of(grant), &params, &NO_ABORT, &mut sink).unwrap();
            assert_eq!((rows as usize, spilled, parts > 0), (expect.len(), want_spill, want_spill), "grant {grant} width {w}");
            // every worker's first claim overflows at once under grant 1: one set is made, under its lock
            assert!(common::SPILL_SETS_CREATED.load(std::sync::atomic::Ordering::Relaxed) - sets <= 1, "grant {grant} width {w}: one spill set per Node");
            assert_eq!(sink.rows(), expect, "grant {grant} width {w}: the groups in key order as pushed");
        }
    }
}
