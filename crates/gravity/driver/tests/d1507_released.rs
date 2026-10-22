//! D1507: a source keeps what it read across statements (a granule source its decode cursors, each holding its part file
//! open) in the worker slots the depot parks and in the leader Node a backend parks. A dropped relation's kept handles
//! stayed until the slot was evicted: pgrc's DROP left the unlinked part files open. The relcache callback records the
//! relation (`slots::note_released`: no lock, no allocation); the next statement boundary releases it from the parked
//! slots (`slots::drain_released`, the depot's take / put) and from this backend's park (the entry's acquire / release),
//! capacity kept. The fake range source keeps a handle on its table across unbind (`RangeTable::keep`), so the table's
//! `Arc` count is what the parked slots still hold. One test: the depot is process-wide.

use gravity_driver_rig as common;

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use common::*;
use gravity_abi::MorselRange;
use gravity_driver::slots::{drain_released, note_released};
use gravity_driver::{Claimed, Entry, Error, Open, Step, DEPOT};
use gravity_program::{Generation, Handle, PlanKey, Program};

const NULLS: [u8; 1] = [0];
const BLOCKS: u64 = 24;
const PER_BLOCK: u64 = 10;
const WIDTH: u16 = 3;
/// `p_count`'s relation (its range site).
const REL: u32 = 2001;
const SALT: u64 = 0x0d15_0700;
const KEY: PlanKey = PlanKey { handle: Handle(0x1507), generation: Generation(1) };

/// The handles the sources keep across unbind (parked or not).
fn kept(table: &RangeTable) -> usize {
    Arc::strong_count(&table.token) - 1
}

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

/// One parallel statement: its worker slots from (and back to) the depot under `SALT`; keyed, the leader Node parks.
fn cycle(entry: &mut Entry, program: &Arc<Program>, derive: &mut FakeDerive, key: Option<PlanKey>, flag: &AtomicU8) {
    let mut sink = LegSink::new();
    let mut b = binding(&[100], &NULLS, &mut sink, flag);
    let mut exec = entry.start(key, program, derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    exec.set_slot_salt(Some(SALT));
    let open = entry.open_section(&mut exec, &b, derive, 0, WIDTH).unwrap();
    let cursor = AtomicU64::new(0);
    for o in 0..WIDTH {
        drain(&open, &cursor, o).unwrap();
    }
    entry.close_section(open, &mut b, &CountMerge, true).unwrap();
    assert_eq!(entry.run(&mut exec, &mut b, gravity_driver::Call::batch(4)).unwrap(), Step::Done);
    entry.end(exec, true);
}

#[test]
fn a_recorded_relation_is_released_from_the_parked_slots_at_the_next_boundary_and_the_capacity_stays() {
    let program = Arc::new(p_count(4));
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    table.keep.store(true, Ordering::Relaxed);
    let mut derive = FakeDerive::ranges(table.clone());
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    DEPOT.trim();
    drain_released();
    let base = kept(&table);
    assert_eq!(base, 0);

    // unkeyed: the Node goes at end, its worker slots park in the depot, each source keeping its handle
    cycle(&mut entry, &program, &mut derive, None, &flag);
    let parked = DEPOT.stats();
    let held = kept(&table);
    assert!(held > base, "the parked worker sources keep their handles ({held} over {base})");

    // recorded only: nothing is released until a boundary drains it
    note_released(REL + 1);
    assert_eq!(kept(&table), held, "the callback's record releases nothing by itself");
    drain_released();
    assert_eq!(kept(&table), held, "another relation's record leaves the parked handles");

    note_released(REL);
    drain_released();
    assert_eq!(kept(&table), base, "the relation's record releases every parked worker's handle");
    let after = DEPOT.stats();
    assert_eq!((after.entries, after.bytes, after.evictions), (parked.entries, parked.bytes, parked.evictions), "capacity stays: the slots remain parked");

    // the slots are taken again (a reopen is lazy) and park again; a reset (relid 0) releases every relation's
    cycle(&mut entry, &program, &mut derive, None, &flag);
    assert_eq!(DEPOT.stats().takes, parked.takes + 1, "the released slots were taken, not rebuilt");
    assert_eq!(kept(&table), held);
    note_released(0);
    drain_released();
    assert_eq!(kept(&table), base, "relid 0 (an invalidation reset) releases every relation's");

    // keyed: the leader Node parks in this backend's park, released at the entry's next acquire (never from the callback)
    cycle(&mut entry, &program, &mut derive, Some(KEY), &flag);
    let both = kept(&table);
    assert!(both > held, "the workers in the depot and the leader in the park ({both} over {held})");
    note_released(REL);
    drain_released();
    let leader = kept(&table);
    assert!(leader > base && leader < both, "the drain reaches the depot, not a backend's park ({leader})");
    let mut sink = LegSink::new();
    let mut b = binding(&[100], &NULLS, &mut sink, &flag);
    let exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    assert_eq!(kept(&table), base, "the next acquire released the parked leader's handle before the bind");
    entry.end(exec, false);

    drop(entry);
    DEPOT.trim();
    assert_eq!(kept(&table), 0, "nothing kept once the depot is trimmed");
}
