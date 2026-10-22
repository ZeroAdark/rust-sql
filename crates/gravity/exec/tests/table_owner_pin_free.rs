//! D3236 (B): a one-owner fold's region (the merged group table `section::own_all` pins) goes back to the grant when its
//! slot frees after its last consumer, not at the statement's end. TPC-DS q04 W8 held each CTE arm's 2-4 GiB owner through
//! the arms after it. Two GROUP BYs over one slot one after the other (the second once the first's emit freed it): the
//! first owner's pin is given back at that free, so G's room after the first emit is at least that pin larger than after
//! the first fold, and both emits return every group.
#[path = "../../parallel/tests/common/mod.rs"]
mod common;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use gravity_abi::breaker::ORDER_NONE;
use gravity_driver::{footprint, Entry, Exec, Grant, Reservation, OWN_CUT, OWN_PAST_GRANT, OWN_PINNED, OWN_SPLIT, POOL, SLOT_PINS_GIVEN};
use gravity_kernels::table::params as par_hashagg;
use gravity_kernels::table::params::DEFAULT_PARAMS;
use gravity_parallel::{FixturePool, HashAggPartial};
use gravity_program::{hand as programs, Code, End, OutCol, OwnerDesc, Pipeline, Program, Reason, SiteDesc, WritePlan};

static LOCK: Mutex<()> = Mutex::new(());

/// The plan, and after each pipeline G's room and the pins given back so far.
struct Watched {
    inner: FixturePlan,
    merge: HashAggPartial,
    lease: Rc<Reservation>,
    seen: RefCell<Vec<(usize, u64, u64)>>,
}

impl gravity_parallel::Plan for Watched {
    fn width_of(&self, p: usize) -> u16 {
        self.inner.width_of(p)
    }
    fn claims_for(&self, p: usize) -> &dyn gravity_parallel::MorselSource {
        self.inner.claims_for(p)
    }
    fn merge_for(&self, _p: usize) -> &dyn gravity_driver::Merge {
        &self.merge
    }
    fn after_pipeline(&self, p: usize, _exec: &mut Exec) -> gravity_driver::Result<()> {
        self.seen.borrow_mut().push((p, self.lease.left(), SLOT_PINS_GIVEN.load(Ordering::Relaxed)));
        Ok(())
    }
}

/// Two `GROUP BY k` over the table into slot 0, each emitted (the emit the slot's one consumer) before the next folds.
fn two_group_bys(rows: u64, d: OwnerDesc) -> Program {
    let st = gravity_program::Arena::new(1 << 12);
    let sites = [SiteDesc::range_source(st.handle(), T_KV, &[1, 2])];
    let out = [OutCol::new("k", programs::INT4, -1, 4, true).origin(T_KV, 1), OutCol::new("sum", programs::INT8, -1, 8, true), OutCol::new("count", programs::INT8, -1, 8, true)];
    let fold = || (Pipeline::eligible(0, End::Breaker(par_hashagg::breaker(0)), rows).owner(d), Code::hand(par_hashagg::par_hashagg));
    let emit = || (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming_one(0), Code::hand(par_hashagg::par_hashagg_emit));
    let pipes = [fold(), emit(), fold(), emit()];
    Program::with_pipelines(gravity_program::Layout { nsites: 1, out_natts: 3, out_rows: 16, scratch_bytes: 0, exec_bytes: 0 }, &sites, &out, WritePlan::SELECT, pipes)
}

fn gen_wide(r: u64) -> Option<[i64; 2]> {
    Some([r as i64, 1])
}

#[test]
fn a_one_owner_pin_goes_back_to_the_grant_when_its_slot_frees() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
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
    let once: Vec<Vec<u64>> = groups.iter().map(|(k, (s, c))| vec![*k as u64, *s as u64, *c as u64]).collect();
    let mut expect: Vec<Vec<u64>> = once.iter().chain(once.iter()).cloned().collect();
    expect.sort();
    // the DR-12 shape that pins its one owner inside the reservation (table_owner_leader.rs): 200,000 one-row groups at W4
    let d = OwnerDesc { groups: 2000, ..OwnerDesc::generic(par_hashagg::LAYOUT, ORDER_NONE, 0) };
    let program = Arc::new(two_group_bys(200_000, d));
    let params = DEFAULT_PARAMS;
    let grant = Grant::of(1 << 20);
    let need = footprint(grant, &program, 4);
    POOL.arm(need);
    let lease = Rc::new(POOL.admit_need(grant, &|g| footprint(g, &program, 4), Duration::from_millis(10)).unwrap());
    // every owner-phase pin kind (the one owner's, the owners past the grant, a split's): each fold takes one of them
    let pins = || OWN_PINNED.load(Ordering::Relaxed) + OWN_PAST_GRANT.load(Ordering::Relaxed) + OWN_SPLIT.load(Ordering::Relaxed);
    let (pinned, cut, given) = (pins(), OWN_CUT.load(Ordering::Relaxed), SLOT_PINS_GIVEN.load(Ordering::Relaxed));
    let plan = Watched {
        inner: FixturePlan::new(&program, &derive, 4, 33),
        merge: HashAggPartial::new(0, Some(gravity_exec::table::owner::bind(&d, &mut params.clone(), 3, None))),
        lease: lease.clone(),
        seen: RefCell::new(Vec::new()),
    };
    let mut sink = LegSink::new(true);
    let r = run_planned_in(&mut entry, &program, &mut derive, &pool, &plan, lease.clone(), &params, &NO_ABORT, &mut sink);
    let seen = plan.seen.borrow().clone();
    drop(plan);
    drop(lease);
    POOL.arm(0);
    assert_eq!(r.unwrap_or_else(|e| panic!("{e:?}")), expect.len() as u64);
    assert_eq!(sink.sorted(), expect, "both emits return every group");
    let census = (pins() - pinned, OWN_CUT.load(Ordering::Relaxed) - cut, OWN_PINNED.load(Ordering::Relaxed), OWN_PAST_GRANT.load(Ordering::Relaxed));
    assert!(census.0 >= 2, "each fold pins its owners past the leader region (pins, cuts, one-owner, past-grant): {census:?} {seen:?}");
    let at = |p: usize| *seen.iter().find(|s| s.0 == p).unwrap_or_else(|| panic!("pipeline {p} ran: {seen:?}"));
    let ((_, room_fold, given_fold), (_, room_emit, given_emit)) = (at(0), at(1));
    let back = given_emit - given_fold;
    assert_eq!(given_fold, given, "nothing given back before the first emit read the owner");
    assert!(back > 0, "the first owner's pin goes back as its slot frees after the emit: {seen:?}");
    assert!(room_emit >= room_fold + back, "G's room after the emit holds the owner's pin again ({room_fold} -> {room_emit}, {back} B given): {seen:?}");
    assert!(SLOT_PINS_GIVEN.load(Ordering::Relaxed) - given > back, "the second owner's pin goes back at its free too: {seen:?}");
}
