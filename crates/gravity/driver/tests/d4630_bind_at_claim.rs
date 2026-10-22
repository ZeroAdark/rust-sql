//! D4630 (b): under `slots::bind_at_claim` the section's open binds none of the width's worker slots on the leader: a
//! slot binds at its first claim, on the claiming thread, and a slot no claim reached is let go unbound (no bind, no
//! unbind of a source never bound). With the switch off the open binds every slot of the width on the leader, as before.
//! The switch is on by default (run 0 sets nothing). D4695: an owner renders through its own worker slot's site, so a slot
//! no claim reached binds before its owner's fold (`Open::own_partitions`, `Open::own_single`).

use gravity_driver_rig as common;

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use common::*;
use gravity_abi::MorselRange;
use gravity_driver::{Claimed, Entry, Flush, Merge, Partial, Region, SpillFile};
use gravity_program::{Generation, Handle, PlanKey};

/// The switch is the process's: the tests take it one at a time.
static SWITCH: Mutex<()> = Mutex::new(());

const KEY: PlanKey = PlanKey { handle: Handle(4630), generation: Generation(1) };
const NULLS: [u8; 1] = [0];
const WIDTH: u16 = 4;
const CLAIMED: u16 = 2;

#[test]
fn a_slot_binds_at_its_first_claim_on_the_claiming_thread_and_a_slot_no_claim_reached_never_binds() {
    let _switch = SWITCH.lock().unwrap_or_else(|e| e.into_inner());
    let table = RangeTable::new(24, 10);
    table.at_claim.store(true, Ordering::Relaxed);
    let mut derive = FakeDerive::ranges(table.clone());
    let program = Arc::new(p_first_row(4));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let leader = std::thread::current().id();
    // the default (on), off, then on again over the slots the depot kept (each execution takes the last one's set)
    for (run, set) in [None, Some(false), Some(true)].into_iter().enumerate() {
        if let Some(on) = set {
            gravity_driver::slots::set_bind_at_claim(on);
        }
        let on = set.unwrap_or(true);
        table.bind_threads.lock().unwrap().clear();
        let rebinds = table.rebinds.load(Ordering::Relaxed);
        let mut sink = LegSink::new();
        let mut b = binding(&[0], &NULLS, &mut sink, &flag);
        let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
        let bound0 = table.bound_now.load(Ordering::Relaxed);
        let open = entry.open_section(&mut exec, &b, &mut derive, 0, WIDTH).unwrap();
        let opened = table.bound_now.load(Ordering::Relaxed) - bound0;
        assert_eq!(opened, if on { 0 } else { WIDTH as i64 }, "run {run}: the open binds {} worker slots", if on { "no" } else { "every" });
        // two of the four ordinals claim, each on its own thread
        let claims: Vec<_> = std::thread::scope(|s| {
            let open = &open;
            let hs: Vec<_> = (0..CLAIMED).map(|o| s.spawn(move || open.run_claim(o, MorselRange::new(o as u64 * 6, o as u64 * 6 + 6)))).collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert!(claims.iter().all(|c| *c == Ok(Claimed::Done)), "run {run}: every claim serves: {claims:?}");
        let threads = table.bind_threads.lock().unwrap().clone();
        if on {
            assert_eq!(threads.len(), CLAIMED as usize, "run {run}: one bind per claimed slot, none for the slots no claim reached");
            assert!(threads.iter().all(|t| *t != leader), "run {run}: each claimed slot bound on its claiming thread");
            assert_eq!(table.bound_now.load(Ordering::Relaxed) - bound0, CLAIMED as i64, "run {run}: only the claimed slots are bound");
        } else {
            assert_eq!(threads.len(), WIDTH as usize, "run {run}: the open bound every slot");
            assert!(threads.iter().all(|t| *t == leader), "run {run}: on the leader");
        }
        entry.close_section(open, &mut b, &NoMerge, true).unwrap();
        assert_eq!(table.bound_now.load(Ordering::Relaxed), bound0, "run {run}: the close unbinds every bound slot");
        assert_eq!(table.rebinds.load(Ordering::Relaxed), rebinds, "run {run}: no source bound twice");
        assert_eq!(sink.rows.lock().unwrap().len(), CLAIMED as usize, "run {run}: one row per claim");
        entry.end(exec, true);
    }
    // D4714: the default (on since b9f0cd23311) back for the next test: the other test's run 0 reads it unset
    gravity_driver::slots::set_bind_at_claim(true);
}

/// Each owner fold's count of the bound sources when it reads its site (where a text key's fold renders it).
struct BoundAtOwn<'a> {
    table: &'a RangeTable,
    seen: Mutex<Vec<i64>>,
}

impl Merge for BoundAtOwn<'_> {
    #[allow(clippy::too_many_arguments)]
    fn own_partition(&self, _owner: Region, _part: u32, _pbits: u8, _partials: &[Partial<'_>], _files: &[&dyn SpillFile], _site: *mut gravity_abi::Site, _cancel: *const u8, _round: u8, _flush: Flush<'_>) -> gravity_driver::Result<()> {
        self.seen.lock().unwrap().push(self.table.bound_now.load(Ordering::Relaxed));
        Ok(())
    }
}

/// D4695: ordinals 2 and 3 claim, 0 and 1 never do. Owner 1's fold (on its own thread) and the one owner's (slot 0's site,
/// on the leader) each read their site with that slot bound; the close unbinds all four, none bound twice. Unbound, the
/// owners rendered a text key through a source no bind had reached, and a count top's tiebreak kept other tie members.
#[test]
fn an_owner_of_a_slot_no_claim_reached_reads_its_site_bound() {
    let _switch = SWITCH.lock().unwrap_or_else(|e| e.into_inner());
    gravity_driver::slots::set_bind_at_claim(true);
    let table = RangeTable::new(24, 10);
    table.at_claim.store(true, Ordering::Relaxed);
    let mut derive = FakeDerive::ranges(table.clone());
    let program = Arc::new(p_group_share(4));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[8], &NULLS, &mut sink, &flag);
    let mut exec = entry.start(None, &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(64 << 20))).unwrap();
    let (bound0, rebinds) = (table.bound_now.load(Ordering::Relaxed), table.rebinds.load(Ordering::Relaxed));
    let open = entry.open_section(&mut exec, &b, &mut derive, 0, WIDTH).unwrap();
    for o in 2..WIDTH {
        let lo = (o as u64 - 2) * 12;
        assert_eq!(open.run_claim(o, MorselRange::new(lo, lo + 12)), Ok(Claimed::Done), "ordinal {o} claims");
    }
    assert_eq!(table.bound_now.load(Ordering::Relaxed) - bound0, 2, "only the claimed slots are bound");
    let probe = BoundAtOwn { table: &table, seen: Mutex::new(Vec::new()) };
    let opened = AtomicBool::new(false);
    std::thread::scope(|s| s.spawn(|| open.own_partitions(&probe, 1, 0, 0, 1, &opened).unwrap()).join().unwrap());
    assert_eq!(probe.seen.lock().unwrap().iter().map(|n| n - bound0).collect::<Vec<_>>(), vec![3], "owner 1 folds with its slot bound beside the two claimed");
    probe.seen.lock().unwrap().clear();
    let _ = open.own_single(&probe, 0);
    let seen: Vec<i64> = probe.seen.lock().unwrap().iter().map(|n| n - bound0).collect();
    assert!(!seen.is_empty() && seen.iter().all(|&n| n == 4), "the one owner folds through slot 0's site bound: {seen:?}");
    let _ = entry.close_section(open, &mut b, &GroupMerge, false);
    assert_eq!(table.bound_now.load(Ordering::Relaxed), bound0, "the close unbinds every slot, the owners' too");
    assert_eq!(table.rebinds.load(Ordering::Relaxed), rebinds, "no source bound twice");
    entry.end(exec, false);
    // D4714: the default (on), not the old off: run 0 of the test above reads the default when this one ran first
    gravity_driver::slots::set_bind_at_claim(true);
}
