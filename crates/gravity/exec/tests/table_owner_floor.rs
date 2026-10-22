//! The generic owner's floor (`owner_min`) from the group estimate and the pool's spread of it (m3-perfdiag §C 1b):
//! a one-key count runs pooled at a 2 GiB grant over 16 owners, C3's floor is KiB, a short grant keeps the serial owner.
#[path = "../../parallel/tests/common/mod.rs"]
mod common;

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard};

use common::*;
use gravity_abi::breaker::{KeyKind, RecordLayout, SlotKind, ORDER_NONE, T1};
use gravity_driver::{Entry, Merge, Partial};
use gravity_exec::table::owner::owner_fns;
use gravity_kernels::dict::reps::{MAX_REPS, REP_NONE};
use gravity_kernels::table::params as par_hashagg;
use gravity_kernels::table::params::DEFAULT_PARAMS;
use gravity_parallel::{FixturePool, HashAggPartial, OWN_OVERFLOW, TABLE_RETRY};
use gravity_program::{hand as programs, Code, End, OutCol, OwnerDesc, Pipeline, Program, Reason, SiteDesc, WritePlan};

const MIB: u64 = 1 << 20;

/// `n` phase-1 tables of `layout` at `pbits`, each a zeroed region with the table laid out (the merge reads the header).
fn partials(layout: &RecordLayout, pbits: u8, n: usize) -> Vec<Vec<u64>> {
    (0..n)
        .map(|_| {
            let mut region = vec![0u64; (256 << 10) / 8];
            unsafe { T1::at(region.as_mut_ptr() as *mut u8, layout, pbits, 8192, (256 << 10) as u32) };
            region
        })
        .collect()
}

fn pooled(d: &OwnerDesc, regions: &[Vec<u64>], owner_bytes: u64) -> Option<u8> {
    let merge = HashAggPartial::new(0, Some(owner_fns(d, 10)));
    let ps: Vec<Partial<'_>> = regions.iter().enumerate().map(|(i, r)| Partial { ordinal: i as u16, bytes: unsafe { core::slice::from_raw_parts(r.as_ptr() as *const u8, r.len() * 8) }, rows_processed: 0 }).collect();
    merge.owned_partitions(&ps, owner_bytes as u32)
}

/// The grant pool (`gravity_driver::POOL`) and the owner counters (`OWN_OVERFLOW`, `OWN_PAST_GRANT`) are process-wide: the
/// tests of this binary run one at a time, so a test that arms the pool or reads a counter sees only its own runs.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|p| p.into_inner())
}

#[test]
fn q15s_generic_owner_runs_pooled_at_two_gib_over_sixteen_owners() {
    let _serial = serial();
    // UserID -> count(*): a NOT NULL int8 key, one count; ~17 M groups at bits 10
    let layout = RecordLayout::new_nn(&[KeyKind::I64], &[SlotKind::Count]);
    assert!(layout.size() <= 24, "a one-key count's record is at most 24 bytes: {}", layout.size());
    let unknown = OwnerDesc::generic(layout, ORDER_NONE, 0);
    let known = OwnerDesc { groups: 17_000_000, ..unknown };
    let fns = owner_fns(&unknown, 10);
    let floor = (fns.min_bytes)(fns.params.as_ptr(), 1);
    assert!(floor >= 512 * MIB && floor < 513 * MIB, "bits 10 without an estimate: 1024 partitions of 512 KiB ({floor})");
    let fns = owner_fns(&known, 10);
    let floor = (fns.min_bytes)(fns.params.as_ptr(), 1);
    assert!(floor >= 17_000_000 * layout.size() as u64 && floor < 17_000_000 * layout.size() as u64 + 64 * 1024, "the estimate's records ({floor})");
    let sixteen = partials(&layout, 10, 16);
    let eight = partials(&layout, 10, 8);
    for d in [&unknown, &known] {
        assert_eq!(pooled(d, &sixteen, 2048 * MIB / 16), Some(10), "a 2 GiB grant over 16 owners runs the owner phase in the pool");
        assert_eq!(pooled(d, &eight, 2048 * MIB / 8), Some(10), "and over 8");
        assert_eq!(pooled(d, &eight, 256 * MIB / 8), None, "a 256 MiB grant over 8 owners cannot hold the runs: the serial owner");
    }
    assert_eq!(pooled(&unknown, &sixteen, 1024 * MIB / 16), None, "unknown groups at 1 GiB: the rounded floor refuses");
    assert_eq!(pooled(&known, &sixteen, 1024 * MIB / 16), Some(10), "known groups at 1 GiB: 408 MiB of runs over 16 run halves of 32 MiB");
}

#[test]
fn c3s_floor_is_kib_and_a_count_ordered_floor_stays_per_owner() {
    let _serial = serial();
    // sum(b) over a nullable int8, count(*), 23 groups
    let layout = RecordLayout::new(&[KeyKind::I32], &[SlotKind::SumI64, SlotKind::Count]);
    let d = OwnerDesc { groups: 23, ..OwnerDesc::generic(layout, ORDER_NONE, 0) };
    let fns = owner_fns(&d, 3);
    let floor = (fns.min_bytes)(fns.params.as_ptr(), 1);
    assert!(floor < 64 * 1024, "23 groups need KiB, not the partition count's MiB ({floor})");
    let regions = partials(&layout, 3, 4);
    assert_eq!(pooled(&d, &regions, 64 * 1024), Some(3));
    // the count-ordered form keeps `n` per owner: its floor is not spread over the owners
    let topk = OwnerDesc::count_topk(RecordLayout::new_nn(&[KeyKind::I64], &[SlotKind::Count]), 0, 10);
    let fns = owner_fns(&topk, 10);
    let floor = (fns.min_bytes)(fns.params.as_ptr(), 1);
    let regions = partials(&topk.layout, 10, 16);
    assert_eq!(pooled(&topk, &regions, floor), Some(10));
    assert_eq!(pooled(&topk, &regions, floor / 2), None, "half the per-owner floor over 16 owners is refused");
}

/// KEYED-28: the two-round distinct form's floor for W owners is one owner's share of the planned groups (round 2 re-sinks
/// them by the group hash), so a pool whose regions hold that share runs the owner phase at a right estimate of 200,000
/// groups where one owner's floor, the planned total, refused it.
#[test]
fn keyed28_a_two_round_distinct_pool_holds_its_share_of_the_planned_groups() {
    let _serial = serial();
    let pair = RecordLayout::new_nn(&[KeyKind::I64, KeyKind::I64], &[]);
    let d = OwnerDesc::distinct(pair, OwnerDesc::accumulator_of(&pair, 1), 1, 200_000, ORDER_NONE, 0, [(REP_NONE, 0); MAX_REPS]);
    let fns = owner_fns(&d, 10);
    assert_eq!(fns.rounds, 2, "the two-round distinct form");
    let one = (fns.min_bytes)(fns.params.as_ptr(), 1);
    let eight = (fns.min_bytes)(fns.params.as_ptr(), 8);
    assert!(eight < one / 6, "eight owners' floor {eight} against one owner's {one}");
    let regions = partials(&pair, 10, 8);
    assert_eq!(pooled(&d, &regions, eight), Some(10), "a region of the share runs the pool");
    assert_eq!(pooled(&d, &regions, eight - 8), None, "under the share: the serial owner");
    assert_eq!(pooled(&d, &partials(&pair, 10, 1), eight), None, "one owner holds every group");
}

fn gen_wide(r: u64) -> Option<[i64; 2]> {
    Some([(r % 100_003) as i64, (r % 1000) as i64])
}

/// `owner.rs`'s declared program: pipeline 0 declares `d` over the kernel's layout.
fn declared(rel: u32, rows: u64, d: OwnerDesc) -> Program {
    let st = gravity_program::Arena::new(1 << 12);
    let sites = [SiteDesc::range_source(st.handle(), rel, &[1, 2])];
    let out = [OutCol::new("k", programs::INT4, -1, 4, true).origin(rel, 1), OutCol::new("sum", programs::INT8, -1, 8, true), OutCol::new("count", programs::INT8, -1, 8, true)];
    let pipes = [
        (Pipeline::eligible(0, End::Breaker(par_hashagg::breaker(0)), rows).owner(d), Code::hand(par_hashagg::par_hashagg)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(par_hashagg::par_hashagg_emit)),
    ];
    Program::with_pipelines(gravity_program::Layout { nsites: 1, out_natts: 3, out_rows: 16, scratch_bytes: 0, exec_bytes: 0 }, &sites, &out, WritePlan::SELECT, pipes)
}

/// An estimate 100x under the groups. D3004: the grant's owner cut holds less than the real records, so the owners are
/// regrown from the records before the owner phase (`owners_short`, `own_past_grant_at`: no overflow, no serial redo).
/// Where the pool has no room for the regrown owners (armed at 16 MiB), the cut's owners overflow and `own_single` redoes
/// the phase. A grant that holds the groups does neither. Every case gives the same groups: correctness never rests on
/// the estimate.
#[test]
fn an_estimate_a_hundredth_of_the_groups_regrows_its_owners_and_a_full_pool_redoes_them_serially() {
    let _serial = serial();
    install_mem_spill();
    let table = FixtureTable::new(T_KV, 2000, 100, gen_wide);
    let mut derive = FixtureDerive::new(&[&table]);
    let pool = FixturePool::new(4);
    let mut entry = Entry::new(2);
    let mut groups: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
    for [k, v] in table.rows() {
        let g = groups.entry(k).or_insert((0, 0));
        g.0 += v;
        g.1 += 1;
    }
    let mut expect: Vec<Vec<u64>> = groups.iter().map(|(k, (s, c))| vec![*k as u64, *s as u64, *c as u64]).collect();
    expect.sort();
    assert!(groups.len() > 100_000);
    let d = OwnerDesc { groups: (groups.len() / 100) as u32, ..OwnerDesc::generic(par_hashagg::LAYOUT, ORDER_NONE, 0) };
    let program = Arc::new(declared(T_KV, 200_000, d));
    let params = DEFAULT_PARAMS;
    // (grant, pool capacity (0 = as armed), regrown, overflowed)
    for (grant, cap, regrown, overflows) in [(1u64 << 20, 0u64, true, false), (1 << 20, 16 << 20, false, true), (1 << 20, 8 << 20, false, true), (1 << 20, 4 << 20, false, true), (64 << 20, 0, false, false)] {
        table.fresh();
        gravity_driver::REGIONS.trim();
        let prev = gravity_driver::POOL.capacity();
        if cap != 0 {
            gravity_driver::POOL.arm(cap);
        }
        let (o0, p0) = (OWN_OVERFLOW.load(Ordering::Relaxed), gravity_driver::OWN_PAST_GRANT.load(Ordering::Relaxed));
        let plan = FixturePlan::new(&program, &mut derive, 4, 33);
        let merge = HashAggPartial::new(0, Some(gravity_exec::table::owner::bind(&d, &mut params.clone(), 3, None)));
        let plan = Keyed { inner: plan, merge };
        let mut sink = LegSink::new(true);
        let rows = run_planned(&mut entry, &program, &mut derive, &pool, &plan, gravity_driver::Grant::of(grant), &params, &NO_ABORT, &mut sink);
        gravity_driver::POOL.arm(prev);
        let rows = rows.unwrap();
        assert_eq!(rows, expect.len() as u64, "grant {grant} cap {cap}: rows");
        assert_eq!(sink.sorted(), expect, "grant {grant} cap {cap}: groups");
        assert_eq!(gravity_driver::OWN_PAST_GRANT.load(Ordering::Relaxed) > p0, regrown, "grant {grant} cap {cap}: the owners regrown from the records");
        assert_eq!(OWN_OVERFLOW.load(Ordering::Relaxed) > o0, overflows, "grant {grant} cap {cap}: the overflow redone serially");
    }
}

struct Keyed {
    inner: FixturePlan,
    merge: HashAggPartial,
}

impl gravity_parallel::Plan for Keyed {
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

fn gen_many(r: u64) -> Option<[i64; 2]> {
    Some([(r % 1_200_007) as i64, (r % 1000) as i64])
}

/// Partitions holding more groups than the partition count's 512 KiB says (the plan's estimate far under): the
/// estimate's table fills and the partition is refolded at the records' size — the same groups.
#[test]
fn a_partition_past_the_estimates_table_is_refolded_at_the_records_size() {
    let _serial = serial();
    install_mem_spill();
    let table = FixtureTable::new(T_KV, 13_000, 100, gen_many);
    let mut derive = FixtureDerive::new(&[&table]);
    let pool = FixturePool::new(4);
    let mut entry = Entry::new(2);
    let mut groups: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
    for [k, v] in table.rows() {
        let g = groups.entry(k).or_insert((0, 0));
        g.0 += v;
        g.1 += 1;
    }
    let mut expect: Vec<Vec<u64>> = groups.iter().map(|(k, (s, c))| vec![*k as u64, *s as u64, *c as u64]).collect();
    expect.sort();
    let d = OwnerDesc::generic(par_hashagg::LAYOUT, ORDER_NONE, 0);
    let program = Arc::new(declared(T_KV, 1_300_000, d));
    let params = DEFAULT_PARAMS;
    let before = TABLE_RETRY.load(Ordering::Relaxed);
    let plan = FixturePlan::new(&program, &mut derive, 4, 33);
    let merge = HashAggPartial::new(0, Some(gravity_exec::table::owner::bind(&d, &mut params.clone(), 4, None)));
    let plan = Keyed { inner: plan, merge };
    let mut sink = LegSink::new(true);
    let rows = run_planned(&mut entry, &program, &mut derive, &pool, &plan, gravity_driver::Grant::of(256 << 20), &params, &NO_ABORT, &mut sink).unwrap();
    assert_eq!(rows, expect.len() as u64);
    assert_eq!(sink.sorted(), expect);
    assert!(TABLE_RETRY.load(Ordering::Relaxed) > before, "a partition's table filled at the estimate and was refolded");
}
