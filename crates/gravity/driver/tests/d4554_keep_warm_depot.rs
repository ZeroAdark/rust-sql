//! D4554 (hot repeats): a worker set parked in the slot depot freed its sources' bind-sized buffers at the park (D4464)
//! and its next statement's bind built them again, on every hot repeat. Under the keep-warm switch (mempolicy's
//! `keep_warm` knob) the park leaves them with the set, on the depot's holder; the taking statement binds them as they are,
//! and the depot gives them back with the set, or the reclaim parks them under pressure (`SlotDepot::trim_but_newest`).
//! Off, the park frees them as before. One test: the depot and the knob are process-wide.

use gravity_driver_rig as common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use common::*;
use gravity_abi::MorselRange;
use gravity_driver::{Claimed, Entry, Error, Open, Step, DEPOT};
use gravity_program::Program;
use memledger::Acct;
use mempolicy::consts::{self, K};

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
const SALT: u64 = 0x4554;
/// Each source's bind-sized buffer: well past a thread's fold slack, so the moves read exactly.
const HEAP: usize = 4 << 20;
/// The trimming thread's backend (a slot of its own: a procno past the slots would be UNBOUND, the set's own account here).
const TRIMMER: u32 = 554;

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
fn a_warm_parked_set_keeps_its_bind_sized_buffers_on_the_depot_and_off_frees_them() {
    std::thread::spawn(|| {
        let program = Arc::new(p_count(4));
        let table = RangeTable::new(BLOCKS, PER_BLOCK);
        table.held_heap.store(HEAP, Ordering::Relaxed);
        table.shed_on_park.store(true, Ordering::Relaxed);
        let mut derive = FakeDerive::ranges(table.clone());
        let flag = AtomicU8::new(0);
        let parked = WIDTH as i64 * HEAP as i64;
        memledger::bind_backend(4554, 9554);

        // off (the default): each park frees the set's buffers, each take's bind builds them again
        consts::reset();
        assert!(!mempolicy::keep::keep_warm(), "off by default");
        DEPOT.clear();
        let d0 = held_for_sources();
        let mut entry = Entry::new(2);
        for run in 1..=3u64 {
            cycle(&mut entry, &program, &mut derive, &flag);
            assert_eq!(DEPOT.stats().entries, 1, "the set parks");
            assert_eq!(table.parks.load(Ordering::Relaxed), run * WIDTH as u64, "each park parks every source");
            assert_eq!(table.heaps.load(Ordering::Relaxed), run * WIDTH as u64, "each run's bind builds the buffers again");
            assert_eq!(held_for_sources(), d0, "the parked set holds no buffer");
        }
        drop(entry);
        DEPOT.clear();

        // on: the first run builds them, the park leaves them on the depot's holder, the next runs take them as they are
        consts::set(K::KeepWarm, 1);
        assert!(mempolicy::keep::keep_warm());
        let (parks0, heaps0) = (table.parks.load(Ordering::Relaxed), table.heaps.load(Ordering::Relaxed));
        let mut entry = Entry::new(2);
        for _ in 0..3 {
            cycle(&mut entry, &program, &mut derive, &flag);
            assert_eq!(DEPOT.stats().entries, 1, "the set parks");
            assert_eq!(table.parks.load(Ordering::Relaxed), parks0, "the depot's park leaves the sources as they are");
            assert_eq!(table.heaps.load(Ordering::Relaxed), heaps0 + WIDTH as u64, "built by the first run only");
            assert_eq!(held_for_sources() - d0, parked, "the parked set's {WIDTH} buffers are the depot's");
        }

        // under pressure the reclaim's slot rung parks the kept set's sources: their buffers leave the depot, the set stays.
        // The shed runs on the TRIMMING thread (the reclaimer's or a make_room backend's, another account than the set's),
        // under the depot's lock: the buffers' frees land on its door 1, and its account goes no lower than it started.
        assert_eq!(DEPOT.trimmable_bytes(), parked as usize, "the newest set's warm buffers are the trim's");
        let (freed, start, end) = std::thread::spawn(move || {
            memledger::bind_backend(TRIMMER, 9000 + TRIMMER);
            let me = Acct::backend(TRIMMER);
            let start = bytes(me);
            let freed = DEPOT.trim_but_newest(parked as usize);
            let end = bytes(me);
            memledger::unbind_backend();
            (freed, start, end)
        })
        .join()
        .unwrap();
        assert_eq!(freed, parked as usize, "the reclaim frees them");
        assert!(end >= start, "the trimming thread's account goes no lower than it started: {start} -> {end}");
        assert_eq!(DEPOT.stats().entries, 1, "the set stays");
        assert_eq!(table.parks.load(Ordering::Relaxed), parks0 + WIDTH as u64, "its sources parked once");
        assert_eq!(held_for_sources(), d0, "the depot holds none of them");
        assert_eq!((DEPOT.trimmable_bytes(), DEPOT.trim_but_newest(parked as usize)), (0, 0), "nothing more to shed");
        // and the next run builds them again and leaves them warm
        cycle(&mut entry, &program, &mut derive, &flag);
        assert_eq!(table.heaps.load(Ordering::Relaxed), heaps0 + 2 * WIDTH as u64, "built again once");
        assert_eq!(held_for_sources() - d0, parked, "warm again on the depot");
        drop(entry);

        // the depot gives them back with the set: its holder's row returns to where it was
        DEPOT.clear();
        assert_eq!(DEPOT.stats().entries, 0);
        assert_eq!(held_for_sources(), d0, "the dropped set's buffers leave the depot");
        consts::reset();
        memledger::unbind_backend();
    })
    .join()
    .unwrap();
}
