//! D4768: a parked worker set that read a relation whose storage went (DROP TABLE / DATABASE, TRUNCATE, a rewrite: the
//! columnar catalog's removed hook) is evicted at the removal (`SlotDepot::evict_reading`), on the removing thread. Before,
//! the removal released only its kept cursors and the set stayed until a later put's eviction or a trim: up to DEPOT_SETS
//! dead sets that no take could find. A set its Node took before the removal and puts after it drops at the put; a set
//! taken after the removal parks and is taken again (a live relation's next runs keep the depot's reuse). A set a running
//! statement holds is not in the depot: the removal leaves it running. One test: the depot is process-wide.

use gravity_driver_rig as common;

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use common::*;
use gravity_abi::MorselRange;
use gravity_driver::slots::{drain_released, note_removed};
use gravity_driver::{Claimed, Entry, Error, Open, Step, DEPOT};
use gravity_program::Program;

const NULLS: [u8; 1] = [0];
const BLOCKS: u64 = 24;
const PER_BLOCK: u64 = 10;
const WIDTH: u16 = 3;
/// `p_count`'s relation (its range site).
const REL: u32 = 2001;
const SALT: u64 = 0x0d47_6800;

/// Relation `rel`'s storage goes, as the columnar catalog's removed hook does it: noted, then the parked sets that read
/// it evicted. The sets evicted.
fn remove(rel: u32) -> usize {
    note_removed(rel);
    DEPOT.evict_reading(&|r| r == rel)
}

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

/// One parallel statement, unkeyed: its worker slots from (and back to) the depot under `SALT`; `during` runs while the
/// statement holds them (after its claims, before its section closes). The claims it ran.
fn statement(entry: &mut Entry, program: &Arc<Program>, derive: &mut FakeDerive, flag: &AtomicU8, during: impl FnOnce()) -> u64 {
    let mut sink = LegSink::new();
    let mut b = binding(&[100], &NULLS, &mut sink, flag);
    let mut exec = entry.start(None, program, derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    exec.set_slot_salt(Some(SALT));
    let open = entry.open_section(&mut exec, &b, derive, 0, WIDTH).unwrap();
    let cursor = AtomicU64::new(0);
    let mut claims = 0;
    for o in 0..WIDTH {
        claims += drain(&open, &cursor, o).unwrap();
    }
    during();
    entry.close_section(open, &mut b, &CountMerge, true).unwrap();
    assert_eq!(entry.run(&mut exec, &mut b, gravity_driver::Call::batch(4)).unwrap(), Step::Done);
    entry.end(exec, true);
    claims
}

#[test]
fn a_removed_relations_parked_sets_go_at_the_removal_and_a_live_relations_stay_reused() {
    let program = Arc::new(p_count(4));
    let table = RangeTable::new(BLOCKS, PER_BLOCK);
    table.keep.store(true, Ordering::Relaxed);
    let mut derive = FakeDerive::ranges(table.clone());
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    DEPOT.trim();
    drain_released();
    let base = DEPOT.stats();
    assert_eq!((base.entries, kept(&table)), (0, 0));

    // a statement parks its set in the depot, each source keeping its handle
    statement(&mut entry, &program, &mut derive, &flag, || {});
    let parked = DEPOT.stats();
    assert_eq!(parked.entries, 1, "one set parked");
    let held = kept(&table);
    assert!(held > 0, "the parked worker sources keep their handles");

    // another relation's removal leaves it; its own relation's removal evicts it now, here
    assert_eq!(remove(REL + 1), 0, "another relation's removal evicts nothing");
    assert_eq!(DEPOT.stats().entries, 1);
    assert_eq!(remove(REL), 1, "its relation's removal evicts it");
    let after = DEPOT.stats();
    assert_eq!((after.entries, after.held, after.evictions), (0, 0, parked.evictions + 1), "gone from the depot, its charge with it");
    assert_eq!(kept(&table), 0, "and every handle its sources kept");

    // a statement holds its set across a removal of its relation: it runs on, and its put drops the set
    let claims = statement(&mut entry, &program, &mut derive, &flag, || {
        assert_eq!(remove(REL), 0, "a set a statement holds is not in the depot");
    });
    assert!(claims > 0, "the statement ran its claims");
    let late = DEPOT.stats();
    assert_eq!((late.entries, late.puts, late.evictions), (0, after.puts, after.evictions + 1), "taken before the removal, put after it: dropped, not parked");

    // the relation's next runs (taken after the removal) park and take again: the depot's reuse stays
    statement(&mut entry, &program, &mut derive, &flag, || {});
    let again = DEPOT.stats();
    assert_eq!((again.entries, again.puts), (1, late.puts + 1), "a set taken after the removal parks");
    statement(&mut entry, &program, &mut derive, &flag, || {});
    let reused = DEPOT.stats();
    assert_eq!((reused.entries, reused.takes, reused.evictions), (1, again.takes + 1, again.evictions), "and is taken again, nothing evicted");

    drop(entry);
    DEPOT.trim();
    assert_eq!(kept(&table), 0, "nothing kept once the depot is trimmed");
}
