//! D1699: the process-wide slot depot (driver slots.rs `DEPOT`) evicts parked worker slot sets past `DEPOT_BYTES` when a
//! statement parks its own (`SlotDepot::put`). The evicted sets dropped inside `put`, under the depot's lock: each slot's
//! drop unmaps its arena, frees its chunks and drops its sources (a granule source's kept cursors close their part files),
//! while every backend's statement start (`take`), end (`put`) and the grant pool's trim waited on that lock. They drop
//! after it now, as `trim_for`'s and D1507's released cursors do: the lock is a leaf.
//! The fake range source's drop probes the lock (`RangeTable::drop_probe`: `DEPOT.stats()` from another thread, answered
//! or not within `PROBE_WAIT`). Each worker slot's arena maps a third of `DEPOT_BYTES` on lazy pages (`source_hint`, never
//! touched), so the second statement's set, parked under another salt, takes the depot past its cap and evicts the first.
//! One test: the depot is process-wide.

use gravity_driver_rig as common;

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use common::*;
use gravity_abi::MorselRange;
use gravity_driver::slots::DEPOT_BYTES;
use gravity_driver::{Claimed, Entry, Error, Open, Step, DEPOT};
use gravity_program::Program;

const NULLS: [u8; 1] = [0];
const BLOCKS: u64 = 24;
const PER_BLOCK: u64 = 10;
const WIDTH: u16 = 2;
/// A source site's image reservation: a third of the depot's cap per slot, so one set of `WIDTH` slots parks past half of it.
const HINT: usize = DEPOT_BYTES / 3;
const SALT_A: u64 = 0x0d16_9900;
const SALT_B: u64 = 0x0d16_9901;

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

/// One unkeyed parallel statement: its worker slots from (and back to) the depot under `salt`; the leader Node drops at end.
fn cycle(entry: &mut Entry, program: &Arc<Program>, derive: &mut FakeDerive, salt: u64, flag: &AtomicU8) {
    let mut sink = LegSink::new();
    let mut b = binding(&[100], &NULLS, &mut sink, flag);
    let mut exec = entry.start(None, program, derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    exec.set_slot_salt(Some(salt));
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
fn a_slot_set_evicted_past_the_depot_cap_drops_after_the_depot_lock() {
    let program = Arc::new(p_count(4));
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    let mut derive = FakeDerive::ranges(table.clone());
    derive.source_hint = HINT;
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    DEPOT.trim();
    let s0 = DEPOT.stats();
    assert_eq!(s0.entries, 0);

    cycle(&mut entry, &program, &mut derive, SALT_A, &flag);
    let s1 = DEPOT.stats();
    assert_eq!(s1.entries, 1, "the first statement's set parks");
    assert!(s1.bytes > DEPOT_BYTES / 2 && s1.bytes <= DEPOT_BYTES, "one set parks past half the cap ({} of {DEPOT_BYTES})", s1.bytes);

    table.drop_probe.store(true, Ordering::Relaxed);
    cycle(&mut entry, &program, &mut derive, SALT_B, &flag);
    table.drop_probe.store(false, Ordering::Relaxed);
    let s2 = DEPOT.stats();
    assert_eq!((s2.entries, s2.evictions), (1, s1.evictions + 1), "the second set took the depot past its cap and evicted the first");
    let (free, locked) = (table.drops_free.load(Ordering::Relaxed), table.drops_locked.load(Ordering::Relaxed));
    assert_eq!(locked, 0, "{locked} evicted slot source(s) dropped while the depot's lock was held (inside put); {free} after it");
    assert!(free >= WIDTH as u64, "the evicted set's {WIDTH} worker sources dropped ({free} probed drops)");

    DEPOT.trim();
    assert_eq!(DEPOT.stats().entries, 0);
}
