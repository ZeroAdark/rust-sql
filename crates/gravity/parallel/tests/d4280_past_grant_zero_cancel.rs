//! D4280: a cancel that lands while the leader zeroes the owners' chunk it pinned past the grant (`Open::own_past_grant`,
//! a two-round shape's whole chunk zeroed, a tick a piece) ends the statement at the fill's next piece. The leader runs
//! that pin between kernel calls, where no abort byte was lent: the fill's step ticked none, and near a cgroup's limit
//! (each fresh page reclaimed first) a W8 cb09's leader zeroed for 1.4 s with its cancel unanswered while the cgroup
//! grew. The witness lever (`cancelpoll::set_cancel_at`) sets the statement's abort bit as the fill's step opens on a
//! lent byte. One test per process: the grant pool and the lever are process-wide.
mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use gravity_driver::{footprint, Entry, Error, Grant, Reservation, OWN_PAST_GRANT, POOL};
use gravity_kernels::par_distinct;
use gravity_parallel::FixturePool;
use gravity_program::{hand as programs, Program};

/// The label of the arena's zero fill of a pinned chunk (`zero_stepped`).
const FILL: &str = "arena pin: a pinned chunk zeroed";

/// Fills opened on a lent byte, and whether the lever sets that byte's abort bit.
static OPENED: AtomicU64 = AtomicU64::new(0);
static ARMED: AtomicBool = AtomicBool::new(false);

fn fire(byte: *const AtomicU8) {
    OPENED.fetch_add(1, Relaxed);
    if ARMED.load(Relaxed) {
        // SAFETY: the lever passes the byte the open step's kernel call lent: the statement's abort byte, live for it
        unsafe { (*byte).fetch_or(cancelpoll::ABORT_BIT, Relaxed) };
    }
}

/// `(k, x)` with 600 groups and a pseudo-random `x` from 400 values: about 140,000 distinct pairs.
fn gen_pairs(r: u64) -> Option<[i64; 2]> {
    Some([(r % 600) as i64, ((r.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 40) % 400) as i64])
}

/// One statement admitted at the program's footprint over a pool of that many bytes and `room` more, its pool part whole.
fn admitted(program: &Arc<Program>, grant: Grant, w: u16, room: u64, run: impl FnOnce(Rc<Reservation>) -> gravity_driver::Result<u64>) -> gravity_driver::Result<u64> {
    let need = footprint(grant, program, w);
    POOL.arm(need + room);
    let a = Rc::new(POOL.admit_need(grant, &|g| footprint(g, program, w), Duration::from_millis(10)).unwrap());
    a.set_pool_fraction(gravity_driver::POOL_FRACTION_WHOLE);
    let r = run(a);
    POOL.arm(0);
    r
}

#[test]
fn a_cancel_inside_the_past_grant_owners_zero_fill_ends_the_statement() {
    install_mem_spill();
    cancelpoll::set_cancel_at(FILL, fire);
    let table = FixtureTable::new(T_KV, 2000, 100, gen_pairs);
    let mut derive = FixtureDerive::new(&[&table]);
    let program = Arc::new(programs::par_distinct_with(T_KV, 16, 200_000, 120_000));
    let words = par_distinct::params(120_000);
    let pool = FixturePool::new(4);
    let mut entry = Entry::new(2);
    let mut sets: BTreeMap<i64, BTreeSet<i64>> = BTreeMap::new();
    for [k, x] in table.rows() {
        sets.entry(k).or_default().insert(x);
    }
    let expect: Vec<Vec<u64>> = sets.iter().map(|(k, s)| vec![*k as u64, s.len() as u64]).collect();
    let abort = AtomicU8::new(0);
    let mut run = |armed: bool| {
        ARMED.store(armed, Relaxed);
        abort.store(0, Relaxed);
        table.fresh();
        let (opened, past) = (OPENED.load(Relaxed), OWN_PAST_GRANT.load(Relaxed));
        let mut sink = LegSink::new(true);
        let r = admitted(&program, Grant::of(1 << 20), 4, 512 << 20, |lease| {
            let plan = FixturePlan::new(&program, &derive, 4, 16);
            run_planned_in(&mut entry, &program, &mut derive, &pool, &plan, lease, &words, &abort, &mut sink)
        });
        (r, sink, OPENED.load(Relaxed) - opened, OWN_PAST_GRANT.load(Relaxed) - past)
    };

    // a 1 MiB grant's owner cut misses the two-round floor: four owners sized from the records are pinned past it in one
    // chunk, zeroed whole, the fill a step on the statement's lent byte (trunk: on none, so the lever never fired)
    let (r, sink, opened, past) = run(false);
    assert_eq!(r.unwrap_or_else(|e| panic!("{e:?}")), expect.len() as u64);
    assert_eq!(sink.rows(), expect, "every group's distinct count, in key order");
    assert_eq!((past, opened), (1, 1), "one past-grant chunk, its zero fill the statement's one fill on a lent byte");

    // the abort bit set as that fill opens: the statement ends as its cancel, the chunk not taken
    let (r, _, opened, past) = run(true);
    assert!(matches!(r, Err(Error::Cancelled)), "the cancel inside the fill: {r:?}");
    assert_eq!((past, opened), (0, 1), "the cancel landed in the past-grant chunk's fill");
}
