mod common;

use std::sync::atomic::Ordering;
use std::sync::Arc;

use common::*;
use gravity_driver::Entry;
use gravity_program::hand as programs;
use gravity_parallel::FixturePool;

#[test]
fn hashjoin_rows_are_identical_at_every_width_and_both_pipelines_section() {
    let inner = FixtureTable::new(T_INNER, 100, 40, gen_inner);
    let outer = FixtureTable::new(T_OUTER, 120, 25, gen_outer);
    let mut derive = FixtureDerive::new(&[&inner, &outer]);
    let program = Arc::new(programs::par_hashjoin_with(T_INNER, T_OUTER, 32, 800, 3000));
    let pool = FixturePool::new(8);
    let mut entry = Entry::new(2);
    let inner_rows: Vec<[i64; 2]> = inner.rows().collect();
    let mut expect: Vec<Vec<u64>> = outer.rows().flat_map(|[k, b]| inner_rows.iter().filter(move |r| r[0] == k).map(move |r| vec![k as u64, r[1] as u64, b as u64])).collect();
    expect.sort();
    for w in WIDTHS {
        for claim in [1u64, 16] {
            inner.fresh();
            outer.fresh();
            let mut sink = LegSink::new(true);
            let before = pool.claims_run();
            let rows = run_once(&mut entry, &program, &mut derive, &pool, w, claim, &gravity_kernels::par_hashjoin::DEFAULT_PARAMS, &NO_ABORT, &mut sink).unwrap();
            assert_eq!(rows, expect.len() as u64, "width {w}");
            assert_eq!(sink.sorted(), expect, "width {w} claim {claim}");
            assert!(inner.ledger.consumed_once() && outer.ledger.consumed_once(), "width {w} claim {claim}");
            assert_eq!((inner.pins.load(Ordering::Relaxed), outer.pins.load(Ordering::Relaxed)), (0, 0));
            let claims = pool.claims_run() - before;
            if w >= 2 {
                assert_eq!(claims, 100u64.div_ceil(claim) + 120u64.div_ceil(claim), "width {w} claim {claim}: both pipelines ran as sections");
            } else {
                assert_eq!(claims, 0, "width 1 is serial on the leader");
            }
        }
    }
}

/// `HashBuildPartial` with phase 2 recorded: partials prepared, partitions merged, serial folds.
struct LedgeredBuild {
    inner: gravity_parallel::HashBuildPartial,
    prepared: std::sync::atomic::AtomicU64,
    parts: gravity_parallel::ClaimLedger,
    serial: std::sync::atomic::AtomicU64,
}

impl gravity_driver::Merge for LedgeredBuild {
    fn merge_into(&self, leader: &mut gravity_driver::State<'_>, partial: gravity_driver::Partial<'_>) -> gravity_driver::Result<()> {
        self.serial.fetch_add(1, Ordering::Relaxed);
        self.inner.merge_into(leader, partial)
    }
    fn finish(&self, leader: &mut gravity_driver::State<'_>) -> gravity_driver::Result<()> {
        self.inner.finish(leader)
    }
    fn room_for(&self, leader: &[u8], partial: gravity_driver::Partial<'_>) -> bool {
        self.inner.room_for(leader, partial)
    }
    fn partition_plan(&self, partials: &[gravity_driver::Partial<'_>]) -> Option<gravity_driver::PartitionPlan> {
        self.inner.partition_plan(partials)
    }
    fn prepare_partitions(&self, leader: &mut [u8], plan: gravity_driver::PartitionPlan, partials: &[gravity_driver::Partial<'_>]) -> gravity_driver::Result<()> {
        self.inner.prepare_partitions(leader, plan, partials)
    }
    fn prepares_partials(&self) -> bool {
        true
    }
    fn prepare_partial(&self, partial: gravity_driver::Region, plan: gravity_driver::PartitionPlan) -> gravity_driver::Result<()> {
        self.prepared.fetch_add(1, Ordering::Relaxed);
        self.inner.prepare_partial(partial, plan)
    }
    fn merge_partition(&self, leader: gravity_driver::Region, part: u32, plan: gravity_driver::PartitionPlan, partials: &[gravity_driver::Partial<'_>]) -> gravity_driver::Result<()> {
        self.parts.record(gravity_abi::MorselRange::new(part as u64, part as u64 + 1));
        self.inner.merge_partition(leader, part, plan, partials)
    }
}

struct JoinPlan {
    inner: FixturePlan,
    build: LedgeredBuild,
}

impl gravity_parallel::Plan for JoinPlan {
    fn width_of(&self, p: usize) -> u16 {
        self.inner.width_of(p)
    }
    fn claims_for(&self, p: usize) -> &dyn gravity_parallel::MorselSource {
        self.inner.claims_for(p)
    }
    fn merge_for(&self, p: usize) -> &dyn gravity_driver::Merge {
        if p == 0 {
            &self.build
        } else {
            self.inner.merge_for(p)
        }
    }
    fn after_pipeline(&self, p: usize, exec: &mut gravity_driver::Exec) -> gravity_driver::Result<()> {
        self.inner.after_pipeline(p, exec)
    }
}

fn gen_big_inner(r: u64) -> Option<[i64; 2]> {
    Some([r as i64, (r * 3) as i64])
}

fn gen_big_outer(r: u64) -> Option<[i64; 2]> {
    Some([((r * 7919) % 100_000) as i64, r as i64])
}

#[test]
fn a_build_past_the_gate_merges_from_the_workers_sorted_partitions_at_every_width() {
    use gravity_driver::Grant;
    // 100,000 build entries × 16 B = 1.6 MB: the gate says P = 2; each worker's partial is sorted by
    // partition on its own thread (phase 2a), each partition job reads its slice of every partial
    let inner = FixtureTable::new(T_INNER, 2500, 40, gen_big_inner);
    let outer = FixtureTable::new(T_OUTER, 40, 25, gen_big_outer);
    let mut derive = FixtureDerive::new(&[&inner, &outer]);
    let program = Arc::new(programs::par_hashjoin_with(T_INNER, T_OUTER, 32, 100_000, 1000));
    let pool = FixturePool::new(8);
    let mut entry = Entry::new(2);
    let expect = join_expect(&inner, &outer);
    assert_eq!(expect.len(), 1000);
    let plan_expected = gravity_driver::partition_plan(100_000, 16).unwrap();
    assert_eq!(plan_expected.pbits, 1);
    for w in WIDTHS {
        inner.fresh();
        outer.fresh();
        let plan = JoinPlan { inner: FixturePlan::new(&program, &derive, w, 100), build: LedgeredBuild { inner: gravity_parallel::HashBuildPartial, prepared: std::sync::atomic::AtomicU64::new(0), parts: gravity_parallel::ClaimLedger::new(2), serial: std::sync::atomic::AtomicU64::new(0) } };
        let mut sink = LegSink::new(true);
        let rows = run_planned(&mut entry, &program, &mut derive, &pool, &plan, Grant::of(64 << 20), &gravity_kernels::par_hashjoin::DEFAULT_PARAMS, &NO_ABORT, &mut sink).unwrap();
        assert_eq!(rows, 1000, "width {w}");
        assert_eq!(sink.sorted(), expect, "width {w}: identical rows");
        assert!(inner.ledger.consumed_once() && outer.ledger.consumed_once(), "width {w}");
        if w >= 2 {
            let prepared = plan.build.prepared.load(Ordering::Relaxed);
            assert!(prepared >= 1 && prepared <= w as u64, "width {w}: every claiming partial prepared once ({prepared})");
            assert!(plan.build.parts.consumed_once(), "width {w}: both partitions merged exactly once");
            assert_eq!(plan.build.serial.load(Ordering::Relaxed), 0, "width {w}: no serial fold after phase 2");
        }
        assert_eq!((inner.pins.load(Ordering::Relaxed), outer.pins.load(Ordering::Relaxed)), (0, 0));
    }
}

/// D3192: pipeline 0's claims, where every thread but the first to claim waits `delay` before each claim (a loaded box's
/// late workers), so one worker's partial takes most of the build.
struct Skewed<'a> {
    inner: &'a dyn gravity_parallel::MorselSource,
    first: std::sync::Mutex<Option<std::thread::ThreadId>>,
    delay: std::time::Duration,
}

impl gravity_parallel::MorselSource for Skewed<'_> {
    fn total(&self) -> u64 {
        self.inner.total()
    }
    fn unit(&self) -> gravity_parallel::Unit {
        self.inner.unit()
    }
    fn claim(&self) -> Option<gravity_abi::MorselRange> {
        let me = std::thread::current().id();
        let first = *self.first.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert(me) == me;
        if !first {
            std::thread::sleep(self.delay);
        }
        self.inner.claim()
    }
    fn close(&self) {
        self.inner.close()
    }
    fn remaining(&self) -> u64 {
        self.inner.remaining()
    }
    fn reset(&self) {
        *self.first.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.inner.reset()
    }
    fn rescan(&self) {
        *self.first.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.inner.rescan()
    }
    fn startup_c0(&self) -> u64 {
        self.inner.startup_c0()
    }
    fn next_boundary_after(&self, unit: u64) -> u64 {
        self.inner.next_boundary_after(unit)
    }
    fn first_live_at(&self, unit: u64) -> u64 {
        self.inner.first_live_at(unit)
    }
    fn whole_boundary_claims(&self) -> bool {
        self.inner.whole_boundary_claims()
    }
    fn observe_bound(&self, bound: i64) {
        self.inner.observe_bound(bound)
    }
    fn beyond_bound(&self, unit: u64) -> bool {
        self.inner.beyond_bound(unit)
    }
}

struct SkewedPlan<'a> {
    join: &'a JoinPlan,
    build: Skewed<'a>,
}

impl gravity_parallel::Plan for SkewedPlan<'_> {
    fn width_of(&self, p: usize) -> u16 {
        gravity_parallel::Plan::width_of(self.join, p)
    }
    fn claims_for(&self, p: usize) -> &dyn gravity_parallel::MorselSource {
        if p == 0 { &self.build } else { gravity_parallel::Plan::claims_for(self.join, p) }
    }
    fn merge_for(&self, p: usize) -> &dyn gravity_driver::Merge {
        gravity_parallel::Plan::merge_for(self.join, p)
    }
    fn after_pipeline(&self, p: usize, exec: &mut gravity_driver::Exec) -> gravity_driver::Result<()> {
        gravity_parallel::Plan::after_pipeline(self.join, p, exec)
    }
}

/// The served join's shape at W = 16: 1,000,000 unique keys, P = 16, phase 2 partition-parallel. D3192: the grant is 1 GiB,
/// so one worker's share (a sixteenth) holds the whole build's table; at 512 MiB a worker that took most claims (a loaded
/// box) passed its share and refused (no spill store in the rig) or spilled to the serial fold.
fn million_entry_build(skew: Option<std::time::Duration>) {
    use gravity_driver::Grant;
    let inner = FixtureTable::new(T_INNER, 25_000, 40, gen_big_inner);
    let outer = FixtureTable::new(T_OUTER, 40, 25, gen_big_outer);
    let mut derive = FixtureDerive::new(&[&inner, &outer]);
    let program = Arc::new(programs::par_hashjoin_with(T_INNER, T_OUTER, 32, 1_000_000, 1000));
    let pool = FixturePool::new(16);
    let mut entry = Entry::new(2);
    let expect = join_expect(&inner, &outer);
    let join = JoinPlan { inner: FixturePlan::new(&program, &derive, 16, 100), build: LedgeredBuild { inner: gravity_parallel::HashBuildPartial, prepared: std::sync::atomic::AtomicU64::new(0), parts: gravity_parallel::ClaimLedger::new(16), serial: std::sync::atomic::AtomicU64::new(0) } };
    let skewed = SkewedPlan { join: &join, build: Skewed { inner: gravity_parallel::Plan::claims_for(&join.inner, 0), first: std::sync::Mutex::new(None), delay: skew.unwrap_or_default() } };
    let plan: &dyn gravity_parallel::Plan = if skew.is_some() { &skewed } else { &join };
    let mut sink = LegSink::new(true);
    let rows = run_planned(&mut entry, &program, &mut derive, &pool, plan, Grant::of(1 << 30), &gravity_kernels::par_hashjoin::DEFAULT_PARAMS, &NO_ABORT, &mut sink).unwrap();
    assert_eq!(rows, 1000);
    assert_eq!(sink.sorted(), expect);
    assert_eq!(join.build.serial.load(Ordering::Relaxed), 0, "no serial fold after phase 2");
    assert!(join.build.parts.consumed_once(), "every one of the 16 partitions merged exactly once");
}

#[test]
fn a_million_entry_build_merges_partition_parallel_at_width_16() {
    million_entry_build(None);
}

#[test]
fn a_million_entry_build_one_worker_mostly_claimed_still_merges_partition_parallel() {
    million_entry_build(Some(std::time::Duration::from_millis(5)));
}

fn join_expect(inner: &FixtureTable, outer: &FixtureTable) -> Vec<Vec<u64>> {
    let inner_rows: Vec<[i64; 2]> = inner.rows().collect();
    let mut expect: Vec<Vec<u64>> = outer.rows().flat_map(|[k, b]| inner_rows.iter().filter(move |r| r[0] == k).map(move |r| vec![k as u64, r[1] as u64, b as u64])).collect();
    expect.sort();
    expect
}

#[test]
fn a_spilled_build_joins_identically_at_every_width_through_its_partitions() {
    use gravity_kernels::par_hashjoin::params;
    install_mem_spill();
    // 8,000 build entries (128 kB) against a 1-byte grant's 64 kB shares: every worker's partition
    // overflows, the outer rows are kept and scattered by the same bits, the third phase joins per partition
    let inner = FixtureTable::new(T_INNER, 1000, 40, gen_inner);
    let outer = FixtureTable::new(T_OUTER, 40, 25, gen_outer);
    let mut derive = FixtureDerive::new(&[&inner, &outer]);
    let program = Arc::new(programs::par_hashjoin_with(T_INNER, T_OUTER, 32, 8000, 1000));
    let words = params(8000);
    let pool = FixturePool::new(8);
    let mut entry = Entry::new(2);
    let expect = join_expect(&inner, &outer);
    assert!(expect.len() > 100_000, "{} rows: a join worth checking", expect.len());
    // in memory under a 64 MB grant: the partition-parallel build, no spill
    for w in WIDTHS {
        inner.fresh();
        outer.fresh();
        let plan = FixturePlan::new(&program, &derive, w, 16);
        let mut sink = LegSink::new(true);
        let (rows, spilled, parts, _) = run_spilling(&mut entry, &program, &mut derive, &pool, &plan, gravity_driver::Grant::of(64 << 20), &words, &NO_ABORT, &mut sink).unwrap();
        assert_eq!((rows, spilled, parts), (expect.len() as u64, false, 0), "width {w}: in memory");
        assert_eq!(sink.sorted(), expect, "width {w}: in memory");
    }
    // spilled: 16 partitions each refilled whole; then 2 partitions too big for the third phase's
    // table, split into a second generation on refill (the program and the words share the bits)
    for (bits, want_gens) in [(4u8, 1u8), (1, 2)] {
        let program = Arc::new(programs::par_hashjoin_bits(T_INNER, T_OUTER, 32, 8000, 1000, bits));
        let params = [bits as u64];
        for w in WIDTHS {
            inner.fresh();
            outer.fresh();
            let plan = FixturePlan::new(&program, &derive, w, 16);
            let mut sink = LegSink::new(true);
            let (rows, spilled, parts, gens) = run_spilling(&mut entry, &program, &mut derive, &pool, &plan, gravity_driver::Grant::of(1), &params, &NO_ABORT, &mut sink).unwrap();
            assert!(spilled && parts > 0, "width {w} bits {bits}: the build spilled ({parts} partitions)");
            assert_eq!(gens, want_gens, "width {w} bits {bits}: generations");
            assert_eq!(rows, expect.len() as u64, "width {w} bits {bits}");
            assert_eq!(sink.sorted(), expect, "width {w} bits {bits}: identical to the in-memory join");
            assert!(inner.ledger.consumed_once() && outer.ledger.consumed_once(), "width {w} bits {bits}");
        }
    }
}
