//! C8 area move 7k: a worker slot set's arena pages are owned. A set parks in the process-wide depot at its statement's
//! end (driver slots.rs `release_workers`) and the next statement under its key takes it, on any backend. Each arena
//! step was charged to the thread's current account and left it at the unmap from whichever account was current then,
//! so the backend that built a set kept its pages after the set parked (and after another backend took it), and the
//! account that dropped it went below what it held. Now the release moves each arena's charge to the depot
//! (`Cache(Parked)`) and the take moves it to the taking backend's session. One test: the depot is process-wide.

use gravity_driver_rig as common;

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use common::*;
use gravity_abi::MorselRange;
use gravity_driver::{Claimed, Entry, Error, Open, Step, DEPOT};
use gravity_program::Program;
use memledger::{Acct, Kind};

const NULLS: [u8; 1] = [0];
const BLOCKS: u64 = 24;
const PER_BLOCK: u64 = 10;
const WIDTH: u16 = 2;
/// A source site's image reservation: each worker slot's arena maps past `ARENA_MAPPED_MIN` (lazy pages, never touched).
const HINT: usize = 8 << 20;
const SALT: u64 = 0xc8_7c00;

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

/// Door 2's arena bytes on account `a`; pagepool P3 (the forced-on arm): the arenas are the pool's (`Door::Pool`, on the
/// kind's lane with no mapped bytes), so both doors' arena bytes.
fn arena(a: Acct) -> i64 {
    memledger::fold();
    if pool_armed() { memledger::kind_bytes(a, Kind::Arena) } else { memledger::kind_mapped_bytes(a, Kind::Arena) }
}

/// One unkeyed parallel statement under `SALT` on backend `acct`: its worker slots from (and back to) the depot. Returns
/// what its end (`Entry::end`: the slots go back to the depot) moved off `acct`'s mapped arena bytes; the Node stays.
fn cycle(entry: &mut Entry, program: &Arc<Program>, derive: &mut FakeDerive, flag: &AtomicU8, acct: Acct) -> i64 {
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
    let before = arena(acct);
    entry.end(exec, true);
    before - arena(acct)
}

#[test]
fn a_parked_sets_arena_pages_are_the_depots_then_the_taking_backends() {
    std::thread::spawn(|| {
        DEPOT.clear();
        let program = Arc::new(p_count(4));
        let table = RangeTable::new(BLOCKS, PER_BLOCK);
        let mut derive = FakeDerive::ranges(table.clone());
        derive.source_hint = HINT;
        let flag = AtomicU8::new(0);
        let (a, b) = (Acct::backend(4060), Acct::backend(4061));
        assert!(a.is_backend() && b.is_backend());

        // backend A builds the set; at its statement's end the set parks, and its arenas' pages leave A for the depot
        memledger::bind_backend(4060, 9060);
        let a0 = arena(a);
        let mut entry = Entry::new(2);
        let parked = cycle(&mut entry, &program, &mut derive, &flag, a);
        assert_eq!(DEPOT.stats().entries, 1, "the set parks");
        assert!(parked >= WIDTH as i64 * memledger::DOOR_FOLD_BYTES, "the parked set's {WIDTH} arenas' pages leave backend A ({parked} bytes)");
        drop(entry);
        assert_eq!(arena(a) - a0, 0, "backend A, its Node gone, holds none of the parked set's pages");
        memledger::unbind_backend();

        // backend B takes the set: its pages are B's while its statement runs, and leave B when it parks again
        memledger::bind_backend(4061, 9061);
        let b0 = arena(b);
        let takes = DEPOT.stats().takes;
        let mut entry = Entry::new(2);
        let again = cycle(&mut entry, &program, &mut derive, &flag, b);
        assert_eq!(DEPOT.stats().takes, takes + 1, "backend B's statement took the parked set");
        assert!(again >= parked, "the taken set's pages were backend B's: its end parks {again} bytes, A parked {parked}");
        drop(entry);
        assert_eq!(arena(b) - b0, 0, "backend B, its Node gone, holds none of the parked set's pages");

        // the depot drops the set (on B's thread): its pages leave the depot, not B
        DEPOT.clear();
        assert_eq!(DEPOT.stats().entries, 0);
        assert_eq!(arena(b) - b0, 0, "the dropped set's pages leave the depot, not the dropping backend");
        memledger::unbind_backend();
    })
    .join()
    .unwrap();
}
