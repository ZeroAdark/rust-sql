//! D3957: a Node's image arena that the allocator or the kernel refuses is the statement's `OutOfMemory` (53200 at the exec
//! glue), never the backend's abort: at W0 from the entry's start (the leader's image) and at W8 from the section's provision
//! (each worker slot's image, built on the leader before any claim runs, so no worker waits on a slot that never came). The
//! entry serves the next execution. The refusal: a source size hint past every address space, a mapping no kernel makes.

use gravity_driver_rig as common;

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use common::*;
use gravity_abi::MorselRange;
use gravity_driver::{Claimed, Entry, Error};
use gravity_program::{Generation, Handle, PlanKey, Program};

const KEY: PlanKey = PlanKey { handle: Handle(57), generation: Generation(1) };
const NULLS: [u8; 1] = [0];
const WIDTH: u16 = 8;
const REFUSED: usize = 1 << 60;

fn refused(e: &Error) -> bool {
    matches!(e, Error::OutOfMemory { bytes, .. } if *bytes >= REFUSED as u64)
}

fn serves_at_w8(entry: &mut Entry, program: &Arc<Program>, derive: &mut FakeDerive) {
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[0], &NULLS, &mut sink, &flag);
    let mut exec = entry.start(Some(KEY), program, derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    let open = entry.open_section(&mut exec, &b, derive, 0, WIDTH).expect("the next execution opens");
    let claims: Vec<_> = std::thread::scope(|s| {
        let open = &open;
        let hs: Vec<_> = (0..WIDTH).map(|o| s.spawn(move || open.run_claim(o, MorselRange::new(o as u64 * 3, o as u64 * 3 + 3)))).collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert!(claims.iter().all(|c| *c == Ok(Claimed::Done)), "{claims:?}");
    entry.close_section(open, &mut b, &NoMerge, true).unwrap();
    entry.end(exec, true);
    assert_eq!(sink.rows.lock().unwrap().len(), WIDTH as usize, "one row per worker");
}

#[test]
fn a_refused_leader_image_fails_the_start_with_out_of_memory_and_the_entry_serves_again() {
    let mut derive = FakeDerive::ranges(RangeTable::new(24, 10));
    let program = Arc::new(p_first_row(4));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[0], &NULLS, &mut sink, &flag);
    derive.source_hint = REFUSED;
    let started = catch_unwind(AssertUnwindSafe(|| entry.start(Some(KEY), &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).err()));
    let e = started.expect("an error, not a plain panic (the backend's abort since D2933)").expect("the start fails");
    assert!(refused(&e), "{e:?}");
    derive.source_hint = 0;
    serves_at_w8(&mut entry, &program, &mut derive);
}

#[test]
fn a_refused_worker_image_fails_the_open_with_out_of_memory_and_the_entry_serves_again() {
    let mut derive = FakeDerive::ranges(RangeTable::new(24, 10));
    let program = Arc::new(p_first_row(4));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[0], &NULLS, &mut sink, &flag);
    let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    derive.source_hint = REFUSED;
    let opened = catch_unwind(AssertUnwindSafe(|| entry.open_section(&mut exec, &b, &mut derive, 0, WIDTH).err()));
    let e = opened.expect("an error, not a plain panic (the backend's abort since D2933)").expect("the open fails");
    assert!(refused(&e), "{e:?}");
    entry.end(exec, false);
    derive.source_hint = 0;
    serves_at_w8(&mut entry, &program, &mut derive);
}
