//! RESIDUE: a parked worker-slot set's sources keep their heap between statements (D3213), and
//! that heap stayed on the account of the backend that last ran the set: over a TPC-H session the backend's unattributed
//! bytes climbed by every new program's parked set (0 -> 534 MiB), and the depot's idle release then freed them on a thread
//! with no backend (UNBOUND -453 MiB, the backend still +537 MiB). Now the slot depot is a holder (`Holder::SlotDepot`, its
//! own account): the park moves each source's held charge to it (`Source::transfer_held`), the take moves it to the taking
//! statement, and a dropped set's charges leave the depot on the dropping thread, where its buffers free. One test: the
//! depot is process-wide.

use gravity_driver_rig as common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use common::*;
use gravity_abi::MorselRange;
use gravity_driver::{Claimed, Entry, Error, Open, Step, DEPOT};
use gravity_program::Program;
use memledger::Acct;

/// Door 1, as the server's allocator wrapper counts it (init_small oom.rs): a heap charge moves bytes the allocator counted
/// on the allocating thread's account, so this binary counts them too.
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
static ALLOC: Door1 = Door1;

const NULLS: [u8; 1] = [0];
const BLOCKS: u64 = 24;
const PER_BLOCK: u64 = 10;
const WIDTH: u16 = 2;
const SALT: u64 = 0x5e_51d0;
/// Each source's kept heap: well past a thread's fold slack, so the moves read exactly.
const HEAP: usize = 4 << 20;
/// What a statement's own machinery leaves on the account around it (its Node, the rig's buffers): far below `HEAP`.
const SLACK: i64 = 1 << 20;

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

fn bytes(a: Acct) -> i64 {
    memledger::fold();
    memledger::account_row(a).account_bytes
}

/// The slot depot holder's bytes less the parked sets' own heap (`DepotStats::held`, D4631: each set's slot list and
/// per-statement lists): what the sets' sources hold there.
fn held_for_sources() -> i64 {
    bytes(Acct::SLOT_DEPOT) - DEPOT.stats().held as i64
}

/// One unkeyed parallel statement under `SALT`: its worker slots from (and back to) the depot.
fn cycle(entry: &mut Entry, program: &Arc<Program>, derive: &mut FakeDerive, flag: &AtomicU8) {
    let mut sink = LegSink::new();
    let mut b = binding(&[100], &NULLS, &mut sink, flag);
    let mut exec = entry.start(None, program, derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
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
fn a_parked_sets_source_heap_is_the_slot_depots_then_the_taking_backends() {
    std::thread::spawn(|| {
        DEPOT.clear();
        let program = Arc::new(p_count(4));
        let table = RangeTable::new(BLOCKS, PER_BLOCK);
        table.held_heap.store(HEAP, Ordering::Relaxed);
        let mut derive = FakeDerive::ranges(table.clone());
        let flag = AtomicU8::new(0);
        let (a, b) = (Acct::backend(4062), Acct::backend(4063));
        let parked = WIDTH as i64 * HEAP as i64;

        // backend A builds the set; at its statement's end its sources' heap leaves A for the slot depot's holder
        memledger::bind_backend(4062, 9062);
        let (a0, d0) = (bytes(a), held_for_sources());
        let mut entry = Entry::new(2);
        cycle(&mut entry, &program, &mut derive, &flag);
        assert_eq!(DEPOT.stats().entries, 1, "the set parks");
        assert_eq!(held_for_sources() - d0, parked, "the parked set's {WIDTH} sources' heap is the slot depot's");
        drop(entry);
        assert!((bytes(a) - a0).abs() < SLACK, "backend A holds none of the parked set's heap: {}", bytes(a) - a0);
        memledger::unbind_backend();

        // backend B takes the set: its heap is B's statement's while it runs, the depot's again when it parks
        memledger::bind_backend(4063, 9063);
        let b0 = bytes(b);
        let takes = DEPOT.stats().takes;
        let mut entry = Entry::new(2);
        cycle(&mut entry, &program, &mut derive, &flag);
        assert_eq!(DEPOT.stats().takes, takes + 1, "backend B's statement took the parked set");
        assert_eq!(held_for_sources() - d0, parked, "parked again: the depot's");
        drop(entry);
        assert!((bytes(b) - b0).abs() < SLACK, "backend B holds none of it: {}", bytes(b) - b0);

        // the depot drops the set on B's thread: the charges leave the depot first, then the buffers free there
        DEPOT.clear();
        assert_eq!(DEPOT.stats().entries, 0);
        assert_eq!(held_for_sources(), d0, "the dropped set's heap leaves the depot");
        assert!((bytes(b) - b0).abs() < SLACK, "and nets to 0 on the dropping backend: {}", bytes(b) - b0);
        memledger::unbind_backend();
    })
    .join()
    .unwrap();
}
