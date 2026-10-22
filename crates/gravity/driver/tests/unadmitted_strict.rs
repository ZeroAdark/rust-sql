//! H16 (a) (main 09-24): a serial, breakerless, single-pipeline program runs outside the grant pool on either protocol, under a
//! strict reservation (`Reservation::unadmitted_strict`). Its premise, no region draw, is guarded: a program that opens a
//! breaker's slot region under it fails by name, XX000 `gravity: unadmitted program drew grant memory` (a debug build asserts
//! first), never bypassing the pool; an arena chunk (the bump window's grow) comes from the capacity as an unadmitted pull's
//! does, counted as the statement's draw, and the statement completes.

use gravity_driver_rig as common;

use std::rc::Rc;
use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use common::*;
use gravity_driver::{Call, Entry, Error, Grant, Reservation, Step, UNADMITTED_DREW};

const MIB: u64 = 1 << 20;
const NULLS: [u8; 1] = [0];

/// A program with a breaker (a share-budgeted hash aggregate) run under a strict reservation: its slot's region is the named error.
#[test]
#[cfg_attr(debug_assertions, should_panic(expected = "unadmitted program drew grant memory"))]
fn a_program_that_opens_a_region_under_a_strict_reservation_fails_by_name() {
    let program = Arc::new(p_group_share(4));
    let mut derive = FakeDerive::ranges(RangeTable::new(24, 10));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[8], &NULLS, &mut sink, &flag);
    let res = Rc::new(Reservation::unadmitted_strict(Grant::of(8 * MIB)));
    let got = match entry.start(None, &program, &mut derive, &mut b, res) {
        Ok(mut exec) => {
            let r = entry.run(&mut exec, &mut b, Call::batch(64));
            entry.end(exec, false);
            r.map(|_| ())
        }
        Err(e) => Err(e),
    };
    match got {
        Err(Error::Host(m)) => assert_eq!(m, UNADMITTED_DREW, "the premise's error, by name"),
        other => panic!("a region under a strict reservation must fail by name, got {other:?}"),
    }
}

/// A breakerless program under a strict reservation grows its arena: the chunk comes from the capacity, is counted as the
/// statement's draw, and the statement completes with every row.
#[test]
fn a_breakerless_programs_arena_chunk_under_a_strict_reservation_is_drawn_and_completes() {
    let program = Arc::new(p_rows(64));
    let table = RangeTable::new(24, 10);
    let mut derive = FakeDerive::ranges(table.clone());
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[0], &NULLS, &mut sink, &flag);
    let res = Rc::new(Reservation::unadmitted_strict(Grant::of(8 * MIB)));
    assert!(res.is_strict());
    let mut exec = entry.start(None, &program, &mut derive, &mut b, res.clone()).expect("a breakerless program starts under a strict reservation");
    let ctx = exec.node().ctx();
    let want = 256 << 10;
    let got = unsafe { gravity_abi::program::arena_grow(ctx, want) };
    assert!(got >= want as i64, "the arena chunk is drawn, not refused ({got})");
    assert!(res.drawn() >= want, "counted as the statement's draw ({} B)", res.drawn());
    assert_eq!(entry.run(&mut exec, &mut b, Call::batch(64)).expect("no error"), Step::Done);
    entry.end(exec, true);
    assert_eq!(sink.sorted().len() as u64, table.rows(), "every row");
}
