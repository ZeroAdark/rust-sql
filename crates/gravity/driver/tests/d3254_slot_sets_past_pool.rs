//! D3254: an admission whose whole ask is past the pool drops the parked worker-slot sets and keeps the parked regions.
//! A set's sources and cursors hold memory outside the pool's account (resident; the memory governor counts it), and
//! trunk's whole-depot trim at such an ask dropped them as a side effect: keeping them across a run of wide statements
//! left ~8 GiB more resident and the governor refused each halving of the next wide grouping down to a sixteenth of its
//! grant. The parked regions stay: dropping them at every admission made each statement map its regions afresh. One
//! test: the pool and both depots are process-wide.

use gravity_driver_rig as common;

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use gravity_abi::MorselRange;
use gravity_driver::{try_region, Claimed, Entry, Error, Grant, Open, Step, DEPOT, POOL, REGIONS};
use gravity_program::Program;

const NULLS: [u8; 1] = [0];
const BLOCKS: u64 = 24;
const PER_BLOCK: u64 = 10;
const WIDTH: u16 = 2;
const MIB: u64 = 1 << 20;

fn drain(open: &Open<'_>, cursor: &AtomicU64, ordinal: u16) -> Result<u64, Error> {
    let mut n = 0;
    loop {
        let lo = cursor.fetch_add(5, Ordering::AcqRel);
        if lo >= BLOCKS {
            return Ok(n);
        }
        match open.run_claim(ordinal, MorselRange::new(lo, (lo + 5).min(BLOCKS)))? {
            Claimed::Done => n += 1,
            Claimed::Stopped => return Ok(n),
        }
    }
}

/// One parallel statement on `entry`: its worker slots go back to the depot at its end (one parked set).
fn statement(entry: &mut Entry, program: &Arc<Program>, derive: &mut FakeDerive, flag: &AtomicU8) {
    let mut sink = LegSink::new();
    let mut b = binding(&[100], &NULLS, &mut sink, flag);
    let mut exec = entry.start(None, program, derive, &mut b, lease(Grant::of(8 << 20))).unwrap();
    exec.set_slot_salt(Some(0x3254));
    let open = entry.open_section(&mut exec, &mut b, derive, 0, WIDTH).unwrap();
    let cursor = AtomicU64::new(0);
    for o in 0..WIDTH {
        drain(&open, &cursor, o).unwrap();
    }
    entry.close_section(open, &mut b, &CountMerge, true).unwrap();
    assert_eq!(entry.run(&mut exec, &mut b, gravity_driver::Call::batch(4)).unwrap(), Step::Done);
    entry.end(exec, true);
}

#[test]
fn an_ask_past_the_pool_drops_the_parked_slot_sets_not_the_regions() {
    POOL.gate(0);
    REGIONS.trim();
    DEPOT.clear();
    assert_eq!(POOL.held(), 0, "a previous test left bytes held");
    let program = Arc::new(p_count(4));
    let mut derive = FakeDerive::ranges(RangeTable::new(BLOCKS, PER_BLOCK));
    let flag = AtomicU8::new(0);
    let mut entry = Entry::new(2);
    statement(&mut entry, &program, &mut derive, &flag);
    assert_eq!(DEPOT.stats().entries, 1, "the statement's slot set parked");

    POOL.arm(3072 * MIB); // the region depot keeps a sixth of the pool: 512 MiB
    REGIONS.put(try_region((200 * MIB) as usize).expect("room in the pool"));
    let (regions, ev) = (REGIONS.stats(), DEPOT.stats().evictions);

    let a = POOL.admit_need(Grant::of(4000 * MIB), &|g| g.bytes, Duration::from_secs(5)).expect("admitted");
    assert_eq!(a.grant.bytes, 2000 * MIB, "halved once, beside the parked region");
    let s = DEPOT.stats();
    assert_eq!((s.entries, s.evictions - ev), (0, 1), "the ask past the pool dropped the parked slot set");
    let r = REGIONS.stats();
    assert_eq!((r.bytes, r.evictions), (regions.bytes, regions.evictions), "and kept the parked region");
    drop(a);

    REGIONS.trim();
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}
