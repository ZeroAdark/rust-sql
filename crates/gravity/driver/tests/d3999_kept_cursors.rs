//! D3999: a backend's parked Nodes and the depot's parked worker sets keep their sources' cursors across statements, each
//! a use of its part, and a backend's park released them only at its session's next statement: an idle session pinned
//! its parts in the part cache (5.2 GB in use with no statement running), past every trim. The idle release's shed and
//! the backstop's reclaim release them.

use gravity_driver_rig as common;

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use common::*;
use gravity_abi::MorselRange;
use gravity_driver::slots::drain_released;
use gravity_driver::{Claimed, Entry, Error, Open, Step, DEPOT};
use gravity_program::{Generation, Handle, PlanKey, Program};

const NULLS: [u8; 1] = [0];
const BLOCKS: u64 = 24;
const PER_BLOCK: u64 = 10;
const WIDTH: u16 = 3;
const SALT: u64 = 0x0d39_9900;
const KEY: PlanKey = PlanKey { handle: Handle(0x3999), generation: Generation(1) };

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
fn the_backstop_and_the_idle_shed_release_every_kept_cursor() {
    let program = Arc::new(p_count(4));
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    table.keep.store(true, Ordering::Relaxed);
    let mut derive = FakeDerive::ranges(table.clone());
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    DEPOT.trim();
    drain_released();
    let base = kept(&table);
    cycle(&mut entry, &program, &mut derive, Some(KEY), &flag);
    let both = kept(&table);
    assert!(both > base, "the workers in the depot and the leader in the park keep their handles ({both} over {base})");
    gravity_driver::mem::reclaim::reclaim_backstop(0);
    let leader = kept(&table);
    assert!(leader > base && leader < both, "the backstop releases the depot's sets at once ({leader} of {both})");
    entry.park().shed_all();
    assert_eq!(kept(&table), base, "the idle shed releases this backend's park");
    drop(entry);
    DEPOT.trim();
}
