//! KN-02 (B): a parallel owner phase whose grant's owner cut misses the merge's floor takes its owners from the partials' real
//! records, pinned past the grant within the pool's room as it is now (`Open::own_past_grant`), before one owner on the leader
//! thread does (`own_single`). Answers are the same either way, and a pool with no room costs no wait and keeps no refusal.
#[path = "../../parallel/tests/common/mod.rs"]
mod common;

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use gravity_abi::breaker::{ORDER_KEY, ORDER_NONE};
use gravity_driver::{footprint, Entry, Grant, Reservation, OWN_PAST_GRANT, POOL};
use gravity_kernels::table::params as par_hashagg;
use gravity_kernels::table::params::{DEFAULT_PARAMS, PARAM_ORDER, PARAM_TOPK_N};
use gravity_parallel::{FixturePool, HashAggPartial};
use gravity_program::{hand as programs, Code, End, OutCol, OwnerDesc, Pipeline, Program, Reason, SiteDesc, WritePlan};

static LOCK: Mutex<()> = Mutex::new(());

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
    let st = gravity_program::Arena::new(1 << 12);
    let sites = [SiteDesc::range_source(st.handle(), T_KV, &[1, 2])];
    let out = [OutCol::new("k", programs::INT4, -1, 4, true).origin(T_KV, 1), OutCol::new("sum", programs::INT8, -1, 8, true), OutCol::new("count", programs::INT8, -1, 8, true)];
    let pipes = [
        (Pipeline::eligible(0, End::Breaker(par_hashagg::breaker(0)), rows).owner(d), Code::hand(par_hashagg::par_hashagg)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(par_hashagg::par_hashagg_emit)),
    ];
    Program::with_pipelines(gravity_program::Layout { nsites: 1, out_natts: 3, out_rows: 16, scratch_bytes: 0, exec_bytes: 0 }, &sites, &out, WritePlan::SELECT, pipes)
}

fn gen_100k(r: u64) -> Option<[i64; 2]> {
    Some([(r * 7919 % 100_000) as i64, (r % 5) as i64])
}

/// One statement admitted at the program's footprint over a pool of that many bytes and `room` more.
fn admitted(program: &Arc<Program>, grant: Grant, w: u16, room: u64, run: impl FnOnce(Rc<Reservation>) -> gravity_driver::Result<u64>) -> gravity_driver::Result<u64> {
    let need = footprint(grant, program, w);
    POOL.arm(need + room);
    let a = Rc::new(POOL.admit_need(grant, &|g| footprint(g, program, w), Duration::from_millis(10)).unwrap());
    // the pool's room alone bounds the owners here (D2990's per-statement part would refuse them in a pool this small first)
    a.set_pool_fraction(gravity_driver::POOL_FRACTION_WHOLE);
    // D4043: a want at or under the floor that waited takes the floor road's same-size trim (`Grant::trimmed`), which
    // drops no byte and carries the trim mark: the grant is the asked one but for that mark
    assert_eq!((Grant { trimmed: false, ..a.grant }, a.reserved), (grant, need), "admitted whole");
    let r = run(a);
    POOL.arm(0);
    r
}

/// 100,000 groups at W4 under a 1 MiB grant: the four owners' cut of the leader region misses the floor of the unordered and
/// key-ordered forms (a top-k form's owners fit the cut), and four owners sized from the real records are pinned past the
/// grant in the pool's room. The owners run on the pool, and every form answers as one owner does.
#[test]
fn a_hundred_thousand_groups_at_w4_take_four_owners_past_the_grant() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    install_mem_spill();
    let table = FixtureTable::new(T_KV, 400, 500, gen_100k);
    let mut derive = FixtureDerive::new(&[&table]);
    let pool = FixturePool::new(4);
    let mut entry = Entry::new(2);
    let mut groups: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
    for [k, v] in table.rows() {
        let g = groups.entry(k).or_insert((0, 0));
        g.0 += v;
        g.1 += 1;
    }
    let all: Vec<Vec<u64>> = groups.iter().map(|(k, (s, c))| vec![*k as u64, *s as u64, *c as u64]).collect();
    for (form, order_slot, topk) in [("unordered", ORDER_NONE, 0u32), ("key-ordered", ORDER_KEY, 0), ("top-k", ORDER_NONE, 5)] {
        let d = OwnerDesc { groups: groups.len() as u32, ..OwnerDesc::generic(par_hashagg::LAYOUT, order_slot, topk) };
        let program = Arc::new(declared(200_000, d));
        let mut params = DEFAULT_PARAMS;
        params[PARAM_ORDER] = (order_slot == ORDER_KEY) as u64;
        params[PARAM_TOPK_N] = topk as u64;
        let expect: Vec<Vec<u64>> = if topk > 0 { all.iter().take(topk as usize).cloned().collect() } else { all.clone() };
        table.fresh();
        let before = OWN_PAST_GRANT.load(Ordering::Relaxed);
        let mut sink = LegSink::new(true);
        let rows = admitted(&program, Grant::of(1 << 20), 4, 512 << 20, |lease| {
            let plan = Keyed { inner: FixturePlan::new(&program, &derive, 4, 33), merge: HashAggPartial::new(0, Some(gravity_exec::table::owner::bind(&d, &mut params.clone(), 3, None))) };
            run_planned_in(&mut entry, &program, &mut derive, &pool, &plan, lease, &params, &NO_ABORT, &mut sink)
        })
        .unwrap_or_else(|e| panic!("{form}: {e:?}"));
        assert_eq!(rows, expect.len() as u64, "{form}");
        assert_eq!(sink.sorted(), { let mut v = expect.clone(); v.sort(); v }, "{form}");
        if order_slot == ORDER_KEY || topk > 0 {
            assert_eq!(sink.rows(), expect, "{form}: the emitted order");
        }
        assert!(topk > 0 || OWN_PAST_GRANT.load(Ordering::Relaxed) > before, "{form}: four owners pinned past the grant");
    }
}

/// The same statement with the planner's rows at a tenth of the real ones, over a pool with 14 MiB past its footprint and no
/// parked region to take: room for the one owner's region (12.2 MiB), not for four owners' chunk (15.3 MiB). The four are
/// not pinned, with no wait for bytes and no refusal counted, and one owner folds as before, with the same answer.
#[test]
fn four_owners_the_pools_room_cannot_hold_leave_the_one_owner_fold_with_no_wait() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    install_mem_spill();
    let table = FixtureTable::new(T_KV, 400, 500, gen_100k);
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
    let d = OwnerDesc { groups: groups.len() as u32, ..OwnerDesc::generic(par_hashagg::LAYOUT, ORDER_NONE, 0) };
    let program = Arc::new(declared(20_000, d));
    let params = DEFAULT_PARAMS;
    table.fresh();
    gravity_driver::park::REGIONS.trim();
    let before = (OWN_PAST_GRANT.load(Ordering::Relaxed), POOL.stats().waits, POOL.stats().refusals);
    let mut sink = LegSink::new(true);
    let rows = admitted(&program, Grant::of(1 << 20), 4, 14 << 20, |lease| {
        let plan = Keyed { inner: FixturePlan::new(&program, &derive, 4, 33), merge: HashAggPartial::new(0, Some(gravity_exec::table::owner::bind(&d, &mut params.clone(), 3, None))) };
        run_planned_in(&mut entry, &program, &mut derive, &pool, &plan, lease, &params, &NO_ABORT, &mut sink)
    })
    .unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(rows, expect.len() as u64);
    assert_eq!(sink.sorted(), expect);
    assert_eq!((OWN_PAST_GRANT.load(Ordering::Relaxed), POOL.stats().waits, POOL.stats().refusals), before, "no pool room for four owners: none pinned, no wait, no refusal");
}
