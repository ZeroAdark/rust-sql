//! D485: a one-round owner whose records ask past the one-owner wall (`OWNER_WALL`, the u32 words of `OwnerTop` / `Owned`)
//! split into k serial owners in one pinned chunk (`section.rs own_split`), never `unsupported execution (kind 2)` at the
//! wall. The wall is lowered here (`TEST_OWNER_WALL`) so a few thousand groups stand for a hundred million.
#[path = "../../parallel/tests/common/mod.rs"]
mod common;

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use common::*;
use gravity_abi::breaker::ORDER_NONE;
use gravity_abi::hash::KeyHash;
use gravity_driver::{footprint, Entry, Grant, OWN_GATED, OWN_PIN_GIVEN, OWN_SPILLED, OWN_SPLIT, POOL, TEST_OWNER_WALL};
use gravity_kernels::table::params as par_hashagg;
use gravity_kernels::table::params::{DEFAULT_PARAMS, PARAM_SPILL_BITS, PARAM_TOPK_N};
use gravity_parallel::{FixturePool, HashAggPartial};
use gravity_program::{hand as programs, Code, End, OutCol, OwnerDesc, Pipeline, Program, Reason, SiteDesc, WritePlan};

static LOCK: Mutex<()> = Mutex::new(());
/// The last `run_fault`'s resident peak (`Reservation::resident_peak`: the most its Node held at once at a section close).
static PEAK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The partition bits of the phase-1 tables (the split's most owners is `1 << PBITS`).
const PBITS: u8 = 5;
/// The lowered wall: the test's records ask several times it.
const WALL: usize = 64 << 10;

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

fn declared(rows: u64, d: OwnerDesc) -> Program {
    declared_groups(rows, d, 0)
}

/// `declared` whose breaker is partitioned for `groups` groups (`par_hashagg::breaker`): the breaker cost's held records
/// are bounded by its bits' table (`HELD_TABLES`), so a statement whose owner holds records past the format's wall needs
/// the planner's groups as well as its rows.
fn declared_groups(rows: u64, d: OwnerDesc, groups: u64) -> Program {
    let st = gravity_program::Arena::new(1 << 12);
    let sites = [SiteDesc::range_source(st.handle(), T_KV, &[1, 2])];
    let out = [OutCol::new("k", programs::INT4, -1, 4, true).origin(T_KV, 1), OutCol::new("sum", programs::INT8, -1, 8, true), OutCol::new("count", programs::INT8, -1, 8, true)];
    let pipes = [
        (Pipeline::eligible(0, End::Breaker(par_hashagg::breaker(groups)), rows).owner(d), Code::hand(par_hashagg::par_hashagg)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(par_hashagg::par_hashagg_emit)),
    ];
    Program::with_pipelines(gravity_program::Layout { nsites: 1, out_natts: 3, out_rows: 16, scratch_bytes: 0, exec_bytes: 0 }, &sites, &out, WritePlan::SELECT, pipes)
}

/// Every row its own group: 4000 groups, a one owner of several walls.
fn gen_wide(r: u64) -> Option<[i64; 2]> {
    Some([r as i64, 1])
}

/// A skewed route: `hot` groups in partition 0, `COLD` in each other partition (the first keys routing there).
const COLD: usize = 40;

fn skew_keys(hot: usize) -> Vec<i64> {
    let mut per = [0usize; 1 << PBITS];
    let mut keys = Vec::new(); // alloc: Cold
    let mut k = 0i64;
    let want = |p: usize| if p == 0 { hot } else { COLD };
    while per.iter().enumerate().any(|(p, &n)| n < want(p)) {
        let p = KeyHash::of(&[k as u64]).part(PBITS as u32) as usize;
        if per[p] < want(p) {
            per[p] += 1;
            keys.push(k); // alloc: Cold
        }
        k += 1;
    }
    keys
}

/// 600 hot groups (19 KB of runs): past owner 0's share at the first k and again at twice it; the split reruns at twice
/// the owners until partition 0 has an owner of its own.
fn gen_hot(r: u64) -> Option<[i64; 2]> {
    static KEYS: OnceLock<Vec<i64>> = OnceLock::new();
    let keys = KEYS.get_or_init(|| skew_keys(600));
    Some([keys[r as usize % keys.len()], r as i64])
}

/// 1000 hot groups (32 KB of runs): one partition past a wall-sized owner's half, a named refusal.
fn gen_too_hot(r: u64) -> Option<[i64; 2]> {
    static KEYS: OnceLock<Vec<i64>> = OnceLock::new();
    let keys = KEYS.get_or_init(|| skew_keys(1000));
    Some([keys[r as usize % keys.len()], r as i64])
}

/// One statement over `gen` at width `w` with the lowered wall; its rows against a BTreeMap's groups, and the splits it ran.
fn run(gen: fn(u64) -> Option<[i64; 2]>, w: u16) -> (gravity_driver::Result<()>, u64) {
    run_top(gen, w, 0)
}

/// `run` of a top-`topk` owner (0: the runs form, every group).
fn run_top(gen: fn(u64) -> Option<[i64; 2]>, w: u16, topk: u32) -> (gravity_driver::Result<()>, u64) {
    run_pool(gen, w, topk, 256 << 20, (40, 100), WALL)
}

/// `run_top` with the pool armed `slack` bytes past the statement's admitted need (D491: a small pool).
fn run_pool(gen: fn(u64) -> Option<[i64; 2]>, w: u16, topk: u32, slack: u64, shape: (u64, u64), wall: usize) -> (gravity_driver::Result<()>, u64) {
    run_fault(gen, w, topk, slack, shape, wall, 0, 0)
}

/// `run_pool` under the forced-spill test fault (`Grant::spill_every`: every `every`th room ask refused; 0 = off), its
/// grant carrying the one-owner wall `owner_wall` (D4078's admission snapshot, `Grant::owner_wall`; 0 = the format's).
#[allow(clippy::too_many_arguments)]
fn run_fault(gen: fn(u64) -> Option<[i64; 2]>, w: u16, topk: u32, slack: u64, (morsels, per): (u64, u64), wall: usize, every: u32, owner_wall: u64) -> (gravity_driver::Result<()>, u64) {
    let table = FixtureTable::new(T_KV, morsels, per, gen);
    let mut derive = FixtureDerive::new(&[&table]);
    let pool = FixturePool::new(w.max(4));
    let mut entry = Entry::new(2);
    let mut groups: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
    for [k, v] in table.rows() {
        let g = groups.entry(k).or_insert((0, 0));
        g.0 += v;
        g.1 += 1;
    }
    let mut expect: Vec<Vec<u64>> = groups.iter().map(|(k, (s, c))| vec![*k as u64, *s as u64, *c as u64]).collect();
    if topk > 0 {
        // the top-k form keeps the k least keys (owner_leader's `top-k` form)
        expect.truncate(topk as usize);
    }
    let d = OwnerDesc { groups: 100, ..OwnerDesc::generic(par_hashagg::LAYOUT, ORDER_NONE, topk) };
    let program = Arc::new(declared(4000, d));
    let mut params = DEFAULT_PARAMS;
    params[PARAM_SPILL_BITS] = PBITS as u64;
    params[PARAM_TOPK_N] = topk as u64;
    table.fresh();
    let mut sink = LegSink::new(true);
    let splits = OWN_SPLIT.load(Ordering::Relaxed);
    TEST_OWNER_WALL.store(wall, Ordering::Relaxed);
    let grant = Grant::of(1 << 20).with_spill_every(every).with_owner_wall(owner_wall);
    let need = footprint(grant, &program, w);
    POOL.arm(need + slack);
    let lease = Rc::new(POOL.admit_need(grant, &|g| footprint(g, &program, w), Duration::from_millis(10)).unwrap());
    let r = {
        let plan = Keyed { inner: FixturePlan::new(&program, &derive, w, 33), merge: HashAggPartial::new(0, Some(gravity_exec::table::owner::bind(&d, &mut params.clone(), PBITS, None))) };
        run_planned_in(&mut entry, &program, &mut derive, &pool, &plan, lease.clone(), &params, &NO_ABORT, &mut sink)
    };
    PEAK.store(lease.resident_peak(), Ordering::Relaxed);
    drop(lease);
    TEST_OWNER_WALL.store(0, Ordering::Relaxed);
    POOL.arm(0);
    let splits = OWN_SPLIT.load(Ordering::Relaxed) - splits;
    let r = r.map(|rows| {
        assert_eq!(rows, expect.len() as u64, "W{w}");
        assert_eq!(sink.sorted(), expect, "W{w}: every group once (a top-k: the k least keys, owner_leader's form), the owners' groups disjoint");
    });
    (r, splits)
}

#[test]
fn d485_a_one_round_owner_past_the_wall_splits_into_serial_owners_and_returns_every_group() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    install_mem_spill();
    let (r, splits) = run(gen_wide, 1);
    r.unwrap_or_else(|e| panic!("W1: {e:?}"));
    assert!(splits >= 1, "W1: the one owner split past the wall");
    // W4 folds in the parallel owners (the leader region's W regions), whatever the one-owner wall: the same rows
    run(gen_wide, 4).0.unwrap_or_else(|e| panic!("W4: {e:?}"));
}

#[test]
fn d485_a_hot_partition_overflows_its_owner_and_reruns_at_twice_the_owners() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    install_mem_spill();
    let hot = skew_keys(600).iter().filter(|&&k| KeyHash::of(&[k as u64]).part(PBITS as u32) == 0).count();
    assert_eq!(hot, 600, "the crafted keys route where they say");
    let (r, splits) = run(gen_hot, 1);
    r.unwrap_or_else(|e| panic!("W1: {e:?}"));
    assert!(splits >= 3, "W1: the hot partition's owner overflowed and the split reran at twice the owners ({splits})");
}

#[test]
fn d485_a_partition_past_a_wall_sized_owner_is_a_named_refusal_never_kind_2() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    install_mem_spill();
    match run(gen_too_hot, 1) {
        (Err(gravity_driver::Error::Refused(why)), _) => assert!(why.contains("one-owner wall"), "{why}"),
        (other, _) => panic!("W1: {other:?}"),
    }
}

/// A top-k owner keeps k records whatever the groups: its region is its run and one partition's table (`kept_run`), so the
/// records past the wall neither split it nor pin a region of them.
#[test]
fn d485_a_top_k_owner_over_records_past_the_wall_folds_in_one_small_owner() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    install_mem_spill();
    let (r, splits) = run_top(gen_wide, 1, 5);
    r.unwrap_or_else(|e| panic!("W1: {e:?}"));
    assert_eq!(splits, 0, "W1: a kept run never splits");
}

/// The runs form's table sits at the region's end (exec owner.rs `owner_open`), one partition's; a partition past it fills the
/// table mid-fold and splits on its next route bits (parallel merge.rs fold_sub), every group returned. The cap lowered to
/// 4 KiB: every partition holds more groups than a 4 KiB table's slots, so only a table that filled and split returns them.
#[test]
fn d485_a_runs_form_table_that_fills_mid_fold_splits_and_returns_every_group() {
    use gravity_exec::table::owner::OWNER_TABLE_CAP;
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    install_mem_spill();
    let slots = gravity_abi::breaker::HashAgg::cap_of(&par_hashagg::LAYOUT, 4 << 10) as usize;
    let mut per = [0usize; 1 << PBITS];
    for r in 0..4000u64 {
        per[KeyHash::of(&[gen_wide(r).unwrap()[0] as u64]).part(PBITS as u32) as usize] += 1;
    }
    assert!(per.iter().all(|&n| n > slots), "every partition past a 4 KiB table ({slots} slots): {per:?}");
    OWNER_TABLE_CAP.store(4 << 10, Ordering::Relaxed);
    let (r, _) = run(gen_wide, 1);
    OWNER_TABLE_CAP.store(64 << 20, Ordering::Relaxed);
    r.unwrap_or_else(|e| panic!("W1: {e:?}"));
}

/// D491: serial owners whose chunk the grant pool refuses. 400k groups (12.8 MB of records) past a 1 MiB wall split into
/// serial owners whose one chunk is 2.5x the records (32.5 MB); with the pool 4 or 16 MiB past the statement's need the pin
/// is refused. Before the fix the statement ended OutOfMemory; the chunk is now a temp file mapped shared (`own_split`,
/// `SpillFactory::map_temp`), and every group comes back, at W1 and W8.
#[test]
fn d491_serial_owners_past_a_small_pool_fold_into_a_mapped_temp_file_and_return_every_group() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    install_mem_spill();
    for w in [1u16, 8] {
        for slack in [4u64 << 20, 16 << 20] {
            let spilled = OWN_SPILLED.load(Ordering::Relaxed);
            let (r, splits) = run_pool(gen_wide, w, 0, slack, (400, 1000), 1 << 20);
            r.unwrap_or_else(|e| panic!("W{w} pool need + {slack}: {e:?}"));
            assert!(splits >= 1, "W{w} pool need + {slack}: the one owner split past the wall");
            assert!(OWN_SPILLED.load(Ordering::Relaxed) > spilled, "W{w} pool need + {slack}: the refused chunk was mapped");
        }
    }
}

/// The owner phase's gate (`Node::owner_refuses`): under the forced-spill test fault refusing every room ask, an owner
/// region is not pinned from the pool (ample here: 256 MiB past the statement's need) but mapped from a temp file, the
/// road a pool refusal takes (D491), and every group comes back, at W1 and W8. With the fault off the same statement
/// asks the gate nothing and maps nothing.
#[test]
fn an_owner_region_past_the_grant_under_the_forced_fault_is_mapped_and_returns_every_group() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    install_mem_spill();
    for w in [1u16, 8] {
        let census = || (OWN_GATED.load(Ordering::Relaxed), OWN_SPILLED.load(Ordering::Relaxed));
        let before = census();
        let (r, _) = run_fault(gen_wide, w, 0, 256 << 20, (400, 1000), 0, 0, 0);
        r.unwrap_or_else(|e| panic!("W{w} fault off: {e:?}"));
        assert_eq!(census(), before, "W{w} fault off: no owner draw gated, none mapped");
        let (r, _) = run_fault(gen_wide, w, 0, 256 << 20, (400, 1000), 0, 1, 0);
        r.unwrap_or_else(|e| panic!("W{w} fault on: {e:?}"));
        let after = census();
        assert!(after.0 > before.0, "W{w} fault on: an owner draw was refused");
        assert!(after.1 > before.1, "W{w} fault on: the refused owner region was mapped from a temp file");
    }
}

/// D4078: the one-owner wall is the statement's, snapshotted at its admission into its grant (`Grant::owner_wall`: half the
/// pool, `mempolicy::admit::owner_wall`), not only the format's. The 400k-group statement at the format's wall (no test
/// lever) with a 1 MiB wall in its grant splits into serial owners at it, and with the pool 4 MiB past its need the
/// owners' chunk is refused and mapped from a temp file (D491), every group returned at W1 and W8. Its footprint pins the
/// owner at that wall too: a statement whose breaker cost holds records past the format's wall is admitted at the
/// statement's wall, never seated alone past the pool at ~4 GiB (a COUNT(DISTINCT URL) under a 3 GB memory.max).
#[test]
fn d4078_the_admitted_owner_wall_splits_the_one_owner_and_bounds_its_footprint_pin() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    install_mem_spill();
    for w in [1u16, 8] {
        let spilled = OWN_SPILLED.load(Ordering::Relaxed);
        let (r, splits) = run_fault(gen_wide, w, 0, 4 << 20, (400, 1000), 0, 0, 1 << 20);
        r.unwrap_or_else(|e| panic!("W{w}: {e:?}"));
        assert!(splits >= 1, "W{w}: the one owner split past the admitted wall");
        assert!(OWN_SPILLED.load(Ordering::Relaxed) > spilled, "W{w}: the refused chunk was mapped");
    }
    let d = OwnerDesc { groups: 100, ..OwnerDesc::generic(par_hashagg::LAYOUT, ORDER_NONE, 0) };
    let program = declared_groups(1 << 32, d, 1 << 30);
    let grant = Grant::of(1 << 20);
    let walled = grant.with_owner_wall(256 << 20);
    assert_eq!(walled.trimmed(1 << 19).owner_wall, 256 << 20, "a trim keeps the wall");
    let (format, statement) = (footprint(grant, &program, 1), footprint(walled, &program, 1));
    assert!(format >= u32::MAX as u64 & !7, "the format's wall: the serial owner pin at ~4 GiB ({format})");
    assert!(statement < format - (3 << 30) && statement >= 256 << 20, "the statement's wall bounds the pin ({statement} of {format})");
}

/// D4078 (ADMITFP (A)): a one-round owner whose round 1 overflows at its statement's wall and then splits gives the region
/// round 1 failed in back before the split pins its chunk, so its owner phase holds the larger of the two, never their sum
/// (lifetime_d3437's union at W0: a 128 MiB round 1 held beside a ~200 MB chunk, a resident peak of 344 MB against its
/// 312 MB bound). The 400k-group statement, serial (W1: the serial arm's owner fold, `Entry::own_single_of`, d3437's W0
/// road; a 4 MiB statement wall, the pool ample: round 1 and the chunk are pool pins, the peak drops by the region) and
/// parallel (W8: a 1 MiB statement wall, the pool 4 MiB past the need: round 1 is a pool pin, the chunk a temp file): the
/// round-1 region is given back and every group comes back.
#[test]
fn d4078_a_round_one_past_the_statement_wall_is_given_back_before_the_split_pins_its_chunk() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    install_mem_spill();
    for (w, sw, slack) in [(1u16, 4u64 << 20, 256u64 << 20), (8, 1 << 20, 4 << 20)] {
        let given = OWN_PIN_GIVEN.load(Ordering::Relaxed);
        let (r, splits) = run_fault(gen_wide, w, 0, slack, (400, 1000), 0, 0, sw);
        r.unwrap_or_else(|e| panic!("W{w}: {e:?}"));
        let (peak, given) = (PEAK.load(Ordering::Relaxed), OWN_PIN_GIVEN.load(Ordering::Relaxed) - given);
        eprintln!("d4078 (A): W{w} statement wall {sw}: splits {splits} given {given} peak {peak}");
        assert!(splits >= 1, "W{w}: the one owner split past the statement's wall");
        assert!(given >= sw, "W{w}: the round-1 region ({sw} bytes) given back before the split's pin: {given}");
    }
}
