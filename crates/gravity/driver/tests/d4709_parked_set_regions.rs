//! D4709 (Q24, the depot-fetch switch): a parked worker set carries no region of the statement that parked it, so the
//! backend that takes it never settles another backend's regions. The statement's end settles every slot region (the
//! share and fixed buffers, the grow chunks) to the region depot BEFORE the set parks (`Entry::release`: `settle_regions`,
//! then `release_workers`), and the take charges each region it draws to the taker as it draws it. Backend A runs a
//! share-budgeted hash aggregate at width 2 under a slot salt (its workers draw share regions) and ends with the set
//! parked; backend B on the same thread (a reused thread) takes the set, runs the same statement and parks it again; the
//! depot then drops it on B's thread. Checks: each backend's whole account (every kind, heap and mapped) is back at its
//! start once its statement ended; the depot's holders hold the same after B's park as after A's; a cleared depot gives
//! their rows back to where they started and charges B nothing. A warm-up statement on a third backend first, so the
//! process-wide tables' first growth is no backend's under test. Its own binary: the global allocator here is door 1
//! (every heap block counted on the thread's account, as the server's wrapper counts it), so a heap charge's commit and
//! discharge balance; one test: the depots are process-wide.

use gravity_driver_rig as common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use common::*;
use gravity_abi::MorselRange;
use gravity_driver::{Claimed, Entry, Error, Open, Step, DEPOT};
use gravity_program::Program;
use memledger::charge::owner_row;
use memledger::{Acct, Holder, Owner};

const NULLS: [u8; 1] = [0];
const BLOCKS: u64 = 24;
const PER_BLOCK: u64 = 10;
const WIDTH: u16 = 2;
const SALT: u64 = 0x4709_05e7;

struct Door1;

// SAFETY: forwards to the system allocator; `door_heap` is a const thread-local add (no allocation, no destructor).
unsafe impl GlobalAlloc for Door1 {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        // SAFETY: the caller's layout, forwarded.
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            memledger::door_heap(l.size() as isize);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: `p` came from `alloc` with `l`.
        unsafe { System.dealloc(p, l) };
        memledger::door_heap(-(l.size() as isize));
    }
}

#[global_allocator]
static DOOR1: Door1 = Door1;

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

/// Everything on account `a` (heap and mapped, every kind), the thread's counts folded first.
fn whole(a: Acct) -> i64 {
    memledger::fold();
    memledger::account_row(a).account_bytes
}

/// The slot depot's holder (the parked sets' heap) and the parked holder (their arenas' pages), committed bytes.
fn depot_rows() -> (i64, i64) {
    (owner_row(Owner::Cache(Holder::SlotDepot)).committed, owner_row(Owner::Cache(Holder::Parked)).committed)
}

/// One parallel `p_group_share` statement on the bound backend under `SALT`: its worker slots from (and back to) the
/// depot, its workers' share regions drawn and settled within it. The Entry (and so its Node) is dropped after it.
fn statement(program: &Arc<Program>, derive: &mut FakeDerive, flag: &AtomicU8) {
    let mut entry = Entry::new(2);
    {
        let mut sink = LegSink::new();
        let mut b = binding(&[8], &NULLS, &mut sink, flag);
        let mut exec = entry.start(None, program, derive, &mut b, lease(gravity_driver::Grant::of(64 << 20))).unwrap();
        exec.set_slot_salt(Some(SALT));
        let open = entry.open_section(&mut exec, &b, derive, 0, WIDTH).unwrap();
        let cursor = AtomicU64::new(0);
        for o in 0..WIDTH {
            drain(&open, &cursor, o).unwrap();
        }
        entry.close_section(open, &mut b, &GroupMerge, true).unwrap();
        assert_eq!(entry.run(&mut exec, &mut b, gravity_driver::Call::batch(4)).unwrap(), Step::Done);
        entry.end(exec, true);
    }
    drop(entry);
}

#[test]
fn a_taken_set_settles_no_region_of_the_backend_that_parked_it() {
    std::thread::spawn(|| {
        DEPOT.clear();
        let program = Arc::new(p_group_share(4));
        let table = RangeTable::new(BLOCKS, PER_BLOCK);
        let mut derive = FakeDerive::ranges(table.clone());
        let flag = AtomicU8::new(0);
        let (w, a, b) = (Acct::backend(4707), Acct::backend(4708), Acct::backend(4709));

        // warm-up on a third backend: the process-wide tables grow here, then its set is dropped
        memledger::bind_backend(4707, 94_707);
        statement(&program, &mut derive, &flag);
        DEPOT.clear();
        let _ = whole(w);
        memledger::unbind_backend();
        let (d0, p0) = depot_rows();

        // backend A: its workers draw share regions, its end settles them and parks the set; A holds nothing after
        memledger::bind_backend(4708, 94_708);
        let a0 = whole(a);
        statement(&program, &mut derive, &flag);
        assert_eq!(DEPOT.stats().entries, 1, "the set parks");
        assert_eq!(whole(a) - a0, 0, "backend A holds nothing once its statement ended and its set parked");
        let (d1, p1) = depot_rows();
        assert!(p1 > p0, "the parked set's arenas are the parked holder's ({p0} -> {p1} B)");
        memledger::unbind_backend();

        // backend B on the same thread takes the set, runs, and parks it again: B holds nothing after, the depot the same
        memledger::bind_backend(4709, 94_709);
        let b0 = whole(b);
        let takes = DEPOT.stats().takes;
        statement(&program, &mut derive, &flag);
        assert_eq!(DEPOT.stats().takes, takes + 1, "backend B's statement took the parked set");
        assert_eq!(whole(b) - b0, 0, "backend B settled no region it did not draw: its account is back at its start");
        assert_eq!(depot_rows(), (d1, p1), "the set parked again holds what A's park left the depot's holders");

        // the depot drops the set on B's thread: the holders' rows back to their start, B charged nothing
        DEPOT.clear();
        assert_eq!(depot_rows(), (d0, p0), "the dropped set leaves the depot's holders where they started");
        assert_eq!(whole(b) - b0, 0, "and charges the dropping backend nothing");
        memledger::unbind_backend();
    })
    .join()
    .unwrap();
}
