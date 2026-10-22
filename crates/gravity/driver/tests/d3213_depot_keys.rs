//! D3213 (sev-1, found by pgrc's SCANSTATS): the process-wide slot depot (driver slots.rs `DEPOT`) parked one worker-slot
//! set per (program key, salt) and handed it back only under that key, with no count cap, and its byte cap counted no
//! source buffer: a workload of many distinct keys grew it without bound (pgrc's line: a new key per execution, 0.88 MB of
//! PSS a parked set, 450 MB/min, evictions 0). Now it keeps at most `DEPOT_SETS` sets (least recently put dropped first)
//! and `DEPOT_SETS_PER_KEY` of one key, and a set's bytes count its sources' kept buffers (`Source::kept_bytes`).
//! One test: the depot is process-wide.

use gravity_driver_rig as common;

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use common::*;
use gravity_abi::MorselRange;
use gravity_driver::{Claimed, Entry, Error, Open, Step, DEPOT, DEPOT_SETS, DEPOT_SETS_PER_KEY};
use gravity_program::Program;

const NULLS: [u8; 1] = [0];
const BLOCKS: u64 = 24;
const PER_BLOCK: u64 = 10;
const WIDTH: u16 = 2;
/// What each fake source reports kept (a granule source's bind-sized lanes are this order a column).
const KEPT: usize = 1 << 20;

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

/// One unkeyed parallel statement under `salt` on each of `entries`, all open before any ends: the worker slots come
/// from the depot (or are built) at each open and go back to it at each end.
fn statements(entries: &mut [Entry], program: &Arc<Program>, derive: &mut FakeDerive, salt: u64, flag: &AtomicU8) {
    let mut sinks: Vec<LegSink> = entries.iter().map(|_| LegSink::new()).collect();
    let mut bindings: Vec<_> = sinks.iter_mut().map(|sink| binding(&[100], &NULLS, sink, flag)).collect();
    let mut execs = Vec::new();
    for (entry, b) in entries.iter_mut().zip(bindings.iter_mut()) {
        let mut exec = entry.start(None, program, derive, b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
        exec.set_slot_salt(Some(salt));
        let open = entry.open_section(&mut exec, b, derive, 0, WIDTH).unwrap();
        let cursor = AtomicU64::new(0);
        for o in 0..WIDTH {
            drain(&open, &cursor, o).unwrap();
        }
        entry.close_section(open, b, &CountMerge, true).unwrap();
        assert_eq!(entry.run(&mut exec, b, gravity_driver::Call::batch(4)).unwrap(), Step::Done);
        execs.push(exec);
    }
    for (entry, exec) in entries.iter_mut().zip(execs) {
        entry.end(exec, true);
    }
}

#[test]
fn the_depot_keeps_a_bounded_number_of_sets_and_counts_their_sources_buffers() {
    DEPOT.clear();
    let program = Arc::new(p_count(4));
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    table.kept_bytes.store(KEPT, Ordering::Relaxed);
    let mut derive = FakeDerive::ranges(Arc::clone(&table));
    let flag = AtomicU8::new(0);
    let mut one = [Entry::new(2)];

    // a set's bytes count its sources' kept buffers, which its arena and regions do not hold
    statements(&mut one, &program, &mut derive, 0, &flag);
    let s = DEPOT.stats();
    assert_eq!(s.entries, 1);
    assert!(s.bytes >= WIDTH as usize * KEPT, "{} bytes parked for {WIDTH} slots keeping {KEPT} each", s.bytes);

    // one key's statements in flight together park one set each, down to the per-key cap (the oldest go)
    DEPOT.clear();
    let mut many: Vec<Entry> = (0..DEPOT_SETS_PER_KEY + 3).map(|_| Entry::new(2)).collect();
    let ev = DEPOT.stats().evictions;
    statements(&mut many, &program, &mut derive, 1, &flag);
    let s = DEPOT.stats();
    assert_eq!((s.entries, s.evictions - ev), (DEPOT_SETS_PER_KEY, 3), "one key keeps DEPOT_SETS_PER_KEY sets");

    // keys no statement takes again: the count cap drops the least recently put, so the depot stops growing
    DEPOT.clear();
    let ev = DEPOT.stats().evictions;
    let keys = DEPOT_SETS as u64 + 40;
    for salt in 0..keys {
        statements(&mut one, &program, &mut derive, 0x3213_0000 + salt, &flag);
    }
    let s = DEPOT.stats();
    assert_eq!((s.entries, s.evictions - ev), (DEPOT_SETS, 40), "past DEPOT_SETS distinct keys the oldest sets drop");
    assert!(s.bytes >= DEPOT_SETS * WIDTH as usize * KEPT, "the parked sets' sources' buffers counted");
    let takes = s.takes;
    statements(&mut one, &program, &mut derive, 0x3213_0000 + keys - 1, &flag);
    assert_eq!(DEPOT.stats().takes, takes + 1, "the newest key's set is taken again");
    statements(&mut one, &program, &mut derive, 0x3213_0000, &flag);
    assert_eq!(DEPOT.stats().takes, takes + 1, "the oldest key's set was dropped: its statement built its slots");
    assert_eq!(DEPOT.stats().entries, DEPOT_SETS);
}
