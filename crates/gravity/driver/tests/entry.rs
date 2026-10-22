use gravity_driver_rig as common;

use std::rc::Rc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use common::*;
use gravity_abi::ScanPos;
use gravity_driver::{Call, Entry, Error, ExecState, Outcome, Step};
use gravity_program::{Code, Generation, Handle, PlanKey};

fn key(h: u64, g: u32) -> PlanKey {
    PlanKey { handle: Handle(h), generation: Generation(g) }
}

const NULLS: [u8; 4] = [0; 4];

#[test]
fn start_run_end_then_reuse_from_park() {
    let mut entry = Entry::new(4);
    let program = Arc::new(p_lookup3(64));
    let mut derive = FakeDerive::new([customer, no_rows, no_rows, no_rows]);
    let flag = AtomicU8::new(0);
    for cycle in 0..3 {
        let sink = FakeSink::new();
        let mut sink_ref = &sink;
        let mut b = binding(&[1, 2, 3], &NULLS, &mut sink_ref, &flag);
        let mut exec = entry.start(Some(key(1, 1)), &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
        assert_eq!(exec.state(), ExecState::Ready);
        assert_eq!(entry.run(&mut exec, &mut b, Call::batch(64)).unwrap(), Step::Done);
        assert_eq!(exec.state(), ExecState::Done);
        assert_eq!(exec.calls(), 1);
        assert_eq!(b.rows_processed, 1);
        assert_eq!(exec.node().positions()[0], ScanPos::new(1001, 1, 1));
        assert_eq!(exec.node().site_pos(0), exec.node().positions()[0]);
        entry.end(exec, true);
        assert_eq!((sink.rows(), sink.batches()), (1, 1));
        assert_eq!(sink.last()[..3], [0xF1, 0x1A57, 500]);
        assert_eq!(entry.park().len(), 1);
        let s = entry.stats();
        assert_eq!((s.built, s.reused, s.ended_ok), (1, cycle, cycle + 1));
    }
}

/// D776: a source site whose own scan a mark names settles to the row its marked-row site holds (taken once), not to its
/// window's base; without one (`p_lookup3`, `start_run_end_then_reuse_from_park`) the base stays.
#[test]
fn a_marked_row_settles_as_its_scan_sites_position() {
    let mut entry = Entry::new(4);
    let program = Arc::new(p_lookup3_marked(64));
    let mut derive = FakeDerive::new([customer, no_rows, no_rows, no_rows]);
    let flag = AtomicU8::new(0);
    let sink = FakeSink::new();
    let mut sink_ref = &sink;
    let mut b = binding(&[1, 2, 3], &NULLS, &mut sink_ref, &flag);
    let mut exec = entry.start(Some(key(21, 1)), &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    assert_eq!(entry.run(&mut exec, &mut b, Call::batch(64)).unwrap(), Step::Done);
    assert_eq!(b.rows_processed, 1);
    let settled = exec.node().positions()[0];
    assert_eq!((settled, settled.flags), (MARKED_ROW, MARKED_ROW.flags));
    assert_eq!(exec.node().site_pos(0), ScanPos::new(1001, 1, 1), "the scan's own position is the window's");
    assert!(!exec.node().site_pos(1).is_valid(), "taken once");
    entry.end(exec, true);
}

#[test]
fn suspend_at_stop_then_resume_from_the_source_position() {
    let mut entry = Entry::new(4);
    let program = Arc::new(p_lookup4(2));
    let (mut derive, stats) = FakeDerive::new([by_last_name, no_rows, no_rows, no_rows]).with_stats();
    let flag = AtomicU8::new(0);
    let sink = FakeSink::new();
    let mut sink_ref = &sink;
    sink.stop_after.set(Some(2));
    let mut b = binding(&[1, 2, 3, 4], &NULLS, &mut sink_ref, &flag);
    let mut exec = entry.start(Some(key(3, 1)), &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    assert_eq!(entry.run(&mut exec, &mut b, Call::batch(2)).unwrap(), Step::Suspended { out_full: true });
    assert_eq!(exec.state(), ExecState::Running);
    assert_eq!(sink.rows(), 2);
    assert_eq!(hdr_cell(exec.node(), RESUME_CELL), 1);
    assert_eq!(entry.run(&mut exec, &mut b, Call::resume(2)).unwrap(), Step::Suspended { out_full: true });
    assert_eq!(entry.run(&mut exec, &mut b, Call::resume(2)).unwrap(), Step::Done);
    assert_eq!((sink.rows(), sink.batches(), stats.seeks.get()), (5, 3, 1));
    assert_eq!(b.rows_processed, 5);
    entry.end(exec, true);
    let sink = FakeSink::new();
    let mut sink_ref = &sink;
    sink.stop_after.set(Some(1));
    let mut b = binding(&[1, 2, 3, 4], &NULLS, &mut sink_ref, &flag);
    let mut exec = entry.start(Some(key(3, 1)), &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    assert_eq!(entry.run(&mut exec, &mut b, Call::batch(2)).unwrap(), Step::Suspended { out_full: true });
    assert_eq!(entry.run(&mut exec, &mut b, Call::batch(2)).unwrap(), Step::Suspended { out_full: true }, "a fresh call restarts the probe");
    assert_eq!(stats.seeks.get(), 3);
    entry.end(exec, true);
}

#[test]
fn join_pushes_full_batches_and_the_tail() {
    let mut entry = Entry::new(4);
    let program = Arc::new(p_join(4));
    let mut derive = FakeDerive::new([orders, order_lines, no_rows, no_rows]);
    let flag = AtomicU8::new(0);
    let sink = FakeSink::new();
    let mut sink_ref = &sink;
    let mut b = binding(&[1, 2, 30], &NULLS, &mut sink_ref, &flag);
    let mut exec = entry.start(Some(key(8, 1)), &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    assert_eq!(entry.run(&mut exec, &mut b, Call::batch(4)).unwrap(), Step::Done);
    assert_eq!((sink.rows(), sink.batches()), (6, 2));
    assert_eq!(sink.last()[..6], [31, 20260913, 8, 3, 3103, 15]);
    assert_eq!(exec.node().positions()[1], ScanPos::new(1009, 3, 1));
    entry.end(exec, true);
}

#[test]
fn write_path_counts_rows_and_carries_the_position() {
    let mut entry = Entry::new(4);
    let program = Arc::new(p_update());
    let (mut derive, stats) = FakeDerive::new([stock, no_rows, no_rows, no_rows]).with_stats();
    let flag = AtomicU8::new(0);
    let sink = FakeSink::new();
    let mut sink_ref = &sink;
    let mut b = binding(&[1, 77], &NULLS, &mut sink_ref, &flag);
    let mut exec = entry.start(Some(key(5, 1)), &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    assert_eq!(entry.run(&mut exec, &mut b, Call::batch(1)).unwrap(), Step::Done);
    assert_eq!((b.rows_processed, sink.rows(), stats.writes.get()), (1, 0, 1));
    let (op, pos, row) = stats.last_write.get();
    assert_eq!((op, pos, &row[..2]), (1, ScanPos::new(1005, 1, 1), &[0x71D, 118][..]));
    entry.end(exec, true);
    derive.write_outcome = Outcome::Skipped;
    let mut b = binding(&[1, 77], &NULLS, &mut sink_ref, &flag);
    let mut exec = entry.start(None, &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    assert_eq!(entry.run(&mut exec, &mut b, Call::batch(1)).unwrap(), Step::Done);
    assert_eq!(b.rows_processed, 0, "a skipped write is not counted");
    entry.end(exec, true);
    derive.write_outcome = Outcome::Recheck;
    let mut b = binding(&[1, 77], &NULLS, &mut sink_ref, &flag);
    let mut exec = entry.start(None, &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    assert_eq!(entry.run(&mut exec, &mut b, Call::batch(1)).unwrap(), Step::Recheck { site: 0 });
    assert_eq!(exec.state(), ExecState::Running);
    assert_eq!(entry.run_to_end(&mut exec, &mut b), Err(Error::Recheck(0)));
    entry.end(exec, false);
}

#[test]
fn lock_outcomes_emit_skip_or_recheck() {
    let mut entry = Entry::new(4);
    let program = Arc::new(p_lock());
    let (mut derive, stats) = FakeDerive::new([five_stock, no_rows, no_rows, no_rows]).with_stats();
    derive.lock_outcomes = [Outcome::Done, Outcome::Skipped, Outcome::Done, Outcome::Recheck];
    let flag = AtomicU8::new(0);
    let sink = FakeSink::new();
    let mut sink_ref = &sink;
    let mut b = binding(&[1, 2], &NULLS, &mut sink_ref, &flag);
    let mut exec = entry.start(Some(key(11, 1)), &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    assert_eq!(entry.run(&mut exec, &mut b, Call::batch(64)).unwrap(), Step::Recheck { site: 0 });
    assert_eq!((sink.rows(), stats.locks.get()), (2, 4), "the rows before the recheck are sent before the stop (D699)");
    assert_eq!(stats.last_lock.get(), ScanPos::new(1005, 4, 1));
    entry.end(exec, false);
    derive.lock_outcomes = [Outcome::Done; 4];
    let mut b = binding(&[1, 2], &NULLS, &mut sink_ref, &flag);
    let mut exec = entry.start(None, &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    assert_eq!(entry.run_to_end(&mut exec, &mut b).unwrap(), 1);
    assert_eq!((sink.rows(), b.rows_processed), (2 + 5, 5));
    entry.end(exec, true);
}

#[test]
fn error_mid_run_fails_the_exec_and_the_node_is_parked_clean() {
    let mut entry = Entry::new(4);
    let program = Arc::new(p_lookup4(2));
    let mut derive = FakeDerive::new([by_last_name, no_rows, no_rows, no_rows]);
    derive.fail_next = Some((0, 3));
    let flag = AtomicU8::new(0);
    for reused in 0..2 {
        let sink = FakeSink::new();
        let mut sink_ref = &sink;
        let mut b = binding(&[1, 2, 3, 4], &NULLS, &mut sink_ref, &flag);
        let mut exec = entry.start(Some(key(3, 1)), &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
        assert_eq!(entry.run(&mut exec, &mut b, Call::batch(2)), Err(Error::host("index read failed")));
        assert_eq!(exec.state(), ExecState::Failed);
        entry.end(exec, false);
        // the out_full push's 2 rows, then the row completed before the failing read: pushed ahead of the error (ENTRY E3)
        assert_eq!(sink.rows(), 3);
        assert_eq!(entry.park().len(), 1);
        assert_eq!((entry.stats().ended_failed, entry.stats().reused), (reused + 1, reused));
    }
}

#[test]
fn cancel_through_the_host_and_at_the_entry() {
    let mut entry = Entry::new(4);
    let program = Arc::new(p_lookup4(64));
    let mut derive = FakeDerive::new([by_last_name, no_rows, no_rows, no_rows]);
    let flag = AtomicU8::new(1);
    let sink = FakeSink::new();
    let mut sink_ref = &sink;
    let mut b = binding(&[1, 2, 3, 4], &NULLS, &mut sink_ref, &flag);
    let mut exec = entry.start(Some(key(3, 1)), &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    assert_eq!(entry.run(&mut exec, &mut b, Call::batch(64)), Err(Error::Cancelled));
    assert_eq!(exec.state(), ExecState::Failed);
    assert_eq!(sink.rows(), 0);
    entry.end(exec, false);
    assert_eq!(entry.park().len(), 1);
    flag.store(0, Ordering::Release);
    let sink = FakeSink::new();
    let mut sink_ref = &sink;
    let mut b = binding(&[1, 2, 3, 4], &NULLS, &mut sink_ref, &flag);
    let mut exec = entry.start(Some(key(3, 1)), &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    assert_eq!(entry.run_to_end(&mut exec, &mut b).unwrap(), 1);
    entry.end(exec, true);
    assert_eq!(sink.rows(), 5);
    assert_eq!(entry.stats().reused, 1);
}

/// D3253: one execution of `p_lookup3` under `k` through a registry of identity `identity`, its one row sent.
fn run_identity(entry: &mut Entry, program: &Arc<gravity_program::Program>, k: PlanKey, identity: u64) {
    let mut derive = FakeDerive::new([customer, no_rows, no_rows, no_rows]);
    derive.identity = identity;
    let flag = AtomicU8::new(0);
    let sink = FakeSink::new();
    let mut sink_ref = &sink;
    let mut b = binding(&[1, 2, 3], &NULLS, &mut sink_ref, &flag);
    let mut exec = entry.start(Some(k), program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    assert_eq!(entry.run(&mut exec, &mut b, Call::batch(64)).unwrap(), Step::Done);
    entry.end(exec, true);
    assert_eq!(sink.rows(), 1);
}

/// D3253: a Node parked under a plan key serves only the registry it was built from (its sources may hold closures over
/// that admission); a later registry under the same key drops it and builds its own. A content key's Node serves any
/// registry, and a registry without an identity (0) any Node, as before.
#[test]
fn a_plan_keyed_node_serves_only_the_registry_it_was_built_from() {
    let mut entry = Entry::new(4);
    let program = Arc::new(p_lookup3(64));
    let counts = |e: &Entry| (e.stats().built, e.stats().reused, e.stats().other_admission);
    let plan = key(5, 1);
    assert!(!plan.is_content());
    run_identity(&mut entry, &program, plan, 1);
    run_identity(&mut entry, &program, plan, 1);
    assert_eq!(counts(&entry), (1, 1, 0), "its own registry takes the parked Node");
    run_identity(&mut entry, &program, plan, 2);
    assert_eq!(counts(&entry), (2, 1, 1), "another registry drops it and builds");
    run_identity(&mut entry, &program, plan, 2);
    assert_eq!(counts(&entry), (2, 2, 1), "the new builder takes its own");
    run_identity(&mut entry, &program, plan, 1);
    assert_eq!(counts(&entry), (3, 2, 2));
    let content = key(9 | PlanKey::CONTENT, 1);
    assert!(content.is_content());
    run_identity(&mut entry, &program, content, 3);
    run_identity(&mut entry, &program, content, 4);
    assert_eq!(counts(&entry), (4, 3, 2), "a content key's Node serves another registry");
    let unknown = key(11, 1);
    run_identity(&mut entry, &program, unknown, 0);
    run_identity(&mut entry, &program, unknown, 0);
    assert_eq!(counts(&entry), (5, 4, 2), "no identity: reused as before");
}

/// C3 S4c: a parked Node serves only the program it was built over (the same `Arc`). An equal program lowered again (its
/// recorded catalog reads moved, so its filed entry was retired) under the same key builds its own: the key no longer carries
/// a relcache epoch to retire the Node.
#[test]
fn a_parked_node_serves_only_the_program_it_was_built_over() {
    let mut entry = Entry::new(4);
    let program = Arc::new(p_lookup3(64));
    let again = Arc::new(p_lookup3(64));
    assert_eq!(program.key, again.key, "the same content");
    let counts = |e: &Entry| (e.stats().built, e.stats().reused, e.stats().other_admission);
    for k in [key(5, 1), key(9 | PlanKey::CONTENT, 1)] {
        let mut entry = Entry::new(4);
        run_identity(&mut entry, &program, k, 1);
        run_identity(&mut entry, &program, k, 1);
        assert_eq!(counts(&entry), (1, 1, 0), "its own program takes the parked Node");
        run_identity(&mut entry, &again, k, 1);
        assert_eq!(counts(&entry), (2, 1, 1), "another program of the same content drops it and builds");
        run_identity(&mut entry, &again, k, 1);
        assert_eq!(counts(&entry), (2, 2, 1), "the new program takes its own");
    }
    run_identity(&mut entry, &program, key(5, 1), 1);
    assert_eq!(counts(&entry), (1, 0, 0));
}

#[test]
fn sink_error_fails_the_exec() {
    let mut entry = Entry::new(4);
    let program = Arc::new(p_lookup4(2));
    let mut derive = FakeDerive::new([by_last_name, no_rows, no_rows, no_rows]);
    let flag = AtomicU8::new(0);
    let sink = FakeSink::new();
    let mut sink_ref = &sink;
    sink.fail_after.set(Some(3));
    let mut b = binding(&[1, 2, 3, 4], &NULLS, &mut sink_ref, &flag);
    let mut exec = entry.start(Some(key(3, 1)), &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    assert_eq!(entry.run(&mut exec, &mut b, Call::batch(2)), Err(Error::host("sink full")));
    entry.end(exec, false);
    assert_eq!(sink.rows(), 3);
}

#[test]
fn a_retained_batch_pointer_reads_poison_after_push() {
    let mut entry = Entry::new(4);
    let program = Arc::new(p_lookup3(64));
    let mut derive = FakeDerive::new([customer, no_rows, no_rows, no_rows]);
    let flag = AtomicU8::new(0);
    let mut sink = RetainingSink { retained: core::ptr::null(), first_seen: 0 };
    let mut b = binding(&[1, 2, 3], &NULLS, &mut sink, &flag);
    let mut exec = entry.start(None, &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    assert_eq!(entry.run(&mut exec, &mut b, Call::batch(64)).unwrap(), Step::Done);
    assert_eq!(sink.first_seen, 0xF1);
    // D469: `hostimpl/sink.rs settle` poisons the pushed block only under `cfg!(debug_assertions)` (a release build does not
    // pay for the fill), so the retained pointer is read, and must read POISON, in a debug build only; every profile runs
    // the rest (the batch reached the sink during push, the run and the end)
    #[cfg(debug_assertions)]
    {
        let after = unsafe { *sink.retained };
        assert_eq!(after, u64::from_ne_bytes([gravity_driver::POISON; 8]), "the batch outlived push");
    }
    assert!(!sink.retained.is_null(), "the sink saw the pushed batch");
    entry.end(exec, true);
}

#[test]
fn bind_failure_returns_the_node_to_the_park() {
    let mut entry = Entry::new(4);
    let program = Arc::new(p_join(64));
    let mut derive = FakeDerive::new([orders, order_lines, no_rows, no_rows]);
    derive.bind_fail_site = Some(1);
    let flag = AtomicU8::new(0);
    let sink = FakeSink::new();
    let mut sink_ref = &sink;
    let mut b = binding(&[1, 2, 30], &NULLS, &mut sink_ref, &flag);
    let r = entry.start(Some(key(8, 1)), &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20)));
    assert!(matches!(r, Err(Error::Host(_))));
    assert_eq!(entry.park().len(), 1);
}

/// D4602: a failed bind parks its node with no host holding what a bind took: the host whose bind failed (it took its state
/// before the step that failed, as D4599's bitmap source took its recheck qual) is unbound with the hosts bound before it.
/// BORN RED: `Node::apply` unbound only the hosts bound before the failing one, and the parked node kept the rest.
#[test]
fn a_failed_bind_parks_no_host_holding_its_state() {
    let mut entry = Entry::new(4);
    let program = Arc::new(p_join(64));
    let mut derive = FakeDerive::new([orders, order_lines, no_rows, no_rows]);
    derive.bind_fail_site = Some(1);
    let stats = Rc::new(Stats::default());
    derive.stats = Some(stats.clone());
    let flag = AtomicU8::new(0);
    let sink = FakeSink::new();
    let mut sink_ref = &sink;
    let mut b = binding(&[1, 2, 30], &NULLS, &mut sink_ref, &flag);
    let r = entry.start(Some(key(8, 1)), &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20)));
    assert!(matches!(r, Err(Error::Host(_))));
    assert_eq!(entry.park().len(), 1);
    assert_eq!(stats.binds.get(), 1, "the site before the failing one bound");
    assert_eq!(stats.held.get(), 0, "a parked host still holds what its failed bind took");
}

#[test]
fn bad_return_code_is_an_error_and_unkeyed_nodes_are_torn_down() {
    let program = Arc::new(p_code(Code::hand(k_bad), 1, 0));
    let mut entry = Entry::new(4);
    let mut derive = FakeDerive::new([no_rows; 4]);
    let flag = AtomicU8::new(0);
    let sink = FakeSink::new();
    let mut sink_ref = &sink;
    let mut b = binding(&[], &NULLS, &mut sink_ref, &flag);
    let mut exec = entry.start(None, &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    assert_eq!(entry.run(&mut exec, &mut b, Call::batch(1)), Err(Error::BadReturn(7)));
    entry.end(exec, false);
    assert!(entry.park().is_empty());
}

#[test]
fn park_purges_on_content_key_change_and_drop_for() {
    let mut entry = Entry::new(4);
    let a = Arc::new(p_lookup3(64));
    let b_prog = Arc::new(p_lookup3(8));
    let mut derive = FakeDerive::new([customer; 4]);
    let flag = AtomicU8::new(0);
    let sink = FakeSink::new();
    let mut sink_ref = &sink;
    let mut b = binding(&[1, 2, 3], &NULLS, &mut sink_ref, &flag);
    let exec = entry.start(Some(key(1, 1)), &a, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    entry.end(exec, true);
    let exec = entry.start(Some(key(1, 1)), &b_prog, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    assert_eq!(entry.stats().built, 2);
    entry.end(exec, true);
    assert_eq!(entry.park().len(), 1);
    let exec = entry.start(Some(key(2, 1)), &a, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    entry.end(exec, true);
    assert_eq!(entry.park().len(), 2);
    assert_eq!(entry.park().drop_for(Handle(1)), 1);
    assert_eq!(entry.park().len(), 1);
    let exec = entry.start(Some(key(2, 1)), &a, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    assert_eq!(entry.stats().reused, 1);
    entry.end(exec, true);
}

#[test]
fn park_is_fixed_capacity_and_evicts_the_oldest() {
    let mut entry = Entry::new(2);
    let program = Arc::new(p_lookup3(64));
    let mut derive = FakeDerive::new([customer; 4]);
    let flag = AtomicU8::new(0);
    let sink = FakeSink::new();
    let mut sink_ref = &sink;
    let mut b = binding(&[1, 2, 3], &NULLS, &mut sink_ref, &flag);
    for h in 1..=3 {
        let exec = entry.start(Some(key(h, 1)), &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
        entry.end(exec, true);
    }
    assert_eq!((entry.park().len(), entry.park().capacity(), entry.park().evictions()), (2, 2, 1));
    let exec = entry.start(Some(key(1, 1)), &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    assert_eq!(entry.stats().built, 4);
    entry.end(exec, true);
}

#[test]
#[should_panic(expected = "run on a Done exec")]
fn run_after_done_is_a_protocol_violation() {
    let mut entry = Entry::new(4);
    let program = Arc::new(p_lookup3(64));
    let mut derive = FakeDerive::new([customer; 4]);
    let flag = AtomicU8::new(0);
    let sink = FakeSink::new();
    let mut sink_ref = &sink;
    let mut b = binding(&[1, 2, 3], &NULLS, &mut sink_ref, &flag);
    let mut exec = entry.start(None, &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    entry.run_to_end(&mut exec, &mut b).unwrap();
    let _ = entry.run(&mut exec, &mut b, Call::batch(1));
}

#[test]
fn a_program_without_breakers_has_no_spill_streams_and_its_census_reads_zero() {
    let mut entry = Entry::new(4);
    let program = Arc::new(p_lookup3(64));
    let mut derive = FakeDerive::new([customer, no_rows, no_rows, no_rows]);
    let flag = AtomicU8::new(0);
    let sink = FakeSink::new();
    let mut sink_ref = &sink;
    let mut b = binding(&[1, 2, 3], &NULLS, &mut sink_ref, &flag);
    let mut exec = entry.start(Some(key(21, 1)), &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    assert_eq!(entry.run(&mut exec, &mut b, Call::batch(64)).unwrap(), Step::Done);
    let node = exec.node();
    assert_eq!((node.nslots(), node.spill_parts(), node.spill_parts_of(0), node.spill_generations(), node.spilled()), (0, 0, 0, 0, false));
    entry.end(exec, true);
}
