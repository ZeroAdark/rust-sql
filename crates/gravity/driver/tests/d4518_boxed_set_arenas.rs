//! D4518: a Node's or a worker slot's image arena below `ARENA_MAPPED_MIN` is a boxed block, and a boxed block had no charge
//! of its own: door 1 counted it on the building thread's account and nothing named an owner, so a backend's parked Node
//! and the worker sets it parked in the process-wide depot sat in the backend's unattributed rest (sortmerge T3's merged
//! sites reserve their live windows in every image: TPC-H W8 rested ~+48 MiB of them). Now a boxed block carries its own
//! charge as a mapping does (kind `Arena`, door 1, whole), on the session that built it, and it moves with the arena: a
//! parked set's to the depot (`Cache(Parked)`), a taken set's to the taking backend's session, off the ledger at the drop.
//! One test: the depot is process-wide (c8_parked_set_arenas is the mapped twin).

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
const WIDTH: u16 = 2;
/// A source site's image reservation: each image stays a boxed block (under `ARENA_MAPPED_MIN`).
const HINT: usize = 256 << 10;
const SALT: u64 = 0xd4_5180;

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

/// Door 1's arena bytes on backend `a`: the boxed blocks' charges.
fn boxed(a: Acct) -> i64 {
    memledger::fold();
    memledger::kind_bytes(a, Kind::Arena) - memledger::kind_mapped_bytes(a, Kind::Arena)
}

/// Door 1's bytes on the depot's account (`Cache(Parked)`; a holder's account keeps no kind lanes).
fn depot() -> i64 {
    memledger::fold();
    let r = memledger::account_row(Acct::PARKED);
    r.account_bytes - r.mapped_bytes
}

/// One unkeyed parallel statement under `SALT` on backend `acct`: its worker slots from (and back to) the depot. Returns
/// what its end (`Entry::end`: the slots go back to the depot) moved off `acct`'s boxed arena bytes; the Node stays.
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
    let before = boxed(acct);
    entry.end(exec, true);
    before - boxed(acct)
}

#[test]
fn a_boxed_image_is_its_builders_then_the_depots_then_the_taking_backends() {
    std::thread::spawn(|| {
        DEPOT.clear();
        let program = Arc::new(p_count(4));
        let table = RangeTable::new(BLOCKS, 10);
        let mut derive = FakeDerive::ranges(table.clone());
        derive.source_hint = HINT;
        let flag = AtomicU8::new(0);
        let (a, b) = (Acct::backend(4070), Acct::backend(4071));
        assert!(a.is_backend() && b.is_backend());
        assert_eq!(memledger::Owner::Cache(memledger::Holder::Parked).acct(), Acct::PARKED);
        let d0 = depot();

        // backend A builds the Node and the set: their images are A's while the statement runs; at its end the set parks
        // with its images
        memledger::bind_backend(4070, 9070);
        let a0 = boxed(a);
        let mut entry = Entry::new(2);
        let left = cycle(&mut entry, &program, &mut derive, &flag, a);
        assert_eq!(DEPOT.stats().entries, 1, "the set parks");
        let parked = depot() - d0;
        assert!(parked >= WIDTH as i64 * HINT as i64, "the parked set's {WIDTH} boxed images are the depot's ({parked} bytes)");
        assert!(left >= parked, "and left backend A ({left} of {parked} bytes; the unkeyed statement's Node went with its end)");
        drop(entry);
        assert_eq!(boxed(a) - a0, 0, "backend A, its Node gone, holds no image");
        memledger::unbind_backend();

        // backend B takes the set: its images are B's while its statement runs, and leave B when it parks again
        memledger::bind_backend(4071, 9071);
        let b0 = boxed(b);
        let takes = DEPOT.stats().takes;
        let mut entry = Entry::new(2);
        let again = cycle(&mut entry, &program, &mut derive, &flag, b);
        assert_eq!(DEPOT.stats().takes, takes + 1, "backend B's statement took the parked set");
        assert!(again >= parked, "the taken set's images were backend B's: its end parks them again ({again} of {parked} bytes)");
        let on_depot = depot();
        assert!(on_depot - d0 >= parked, "on the depot again ({} of {parked} bytes)", on_depot - d0);
        drop(entry);
        assert_eq!(boxed(b) - b0, 0, "backend B, its Node gone, holds no image");

        // the depot drops the set (on B's thread): its images leave the depot, not B
        DEPOT.clear();
        assert_eq!(DEPOT.stats().entries, 0);
        let gone = on_depot - depot();
        assert!(gone >= WIDTH as i64 * HINT as i64, "the dropped set's {WIDTH} images leave the depot ({gone} of {parked} bytes)");
        assert_eq!(boxed(b) - b0, 0, "not the dropping backend");
        memledger::unbind_backend();
    })
    .join()
    .unwrap();
}
