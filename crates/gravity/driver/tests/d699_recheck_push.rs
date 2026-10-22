//! D699: a pipeline that stops on an EvalPlanQual recheck has sent the rows it emitted before the rechecked one when the
//! stop returns, so the row the recheck pushes follows them (PostgreSQL's scan order); a sink's failure at that send is
//! the statement's error, not a recheck.
use gravity_driver_rig as common;

use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use common::*;
use gravity_driver::{Call, Entry, Outcome, Step};
use gravity_program::{Arena, Code, End, Generation, Handle, Layout, LockKind, OutCol, Pipeline, PlanKey, Program, Reason, SiteDesc, WaitPolicy, WritePlan};

const KEY: PlanKey = PlanKey { handle: Handle(5), generation: Generation(1) };
const NULLS: [u8; 4] = [0; 4];

/// One locking pipeline over five index rows (2..6): locked, skipped, locked, then the recheck on row 5.
fn run_to_recheck(sink: &FakeSink) -> Result<Step, gravity_driver::Error> {
    let st = Arena::new(1 << 14);
    let sites = [SiteDesc::index_source(st.handle(), 1005, 1105, &[1, 2], &[0, 1], &[3])];
    let layout = Layout { nsites: 1, out_natts: 1, out_rows: 64, scratch_bytes: 0, exec_bytes: 0 };
    let pipes = [(Pipeline::serial(0, End::Sink, Reason::Lock), Code::hand(k_lock))];
    let program = Arc::new(Program::with_pipelines(layout, &sites, &[OutCol::new("c0", 20, -1, 8, true)], WritePlan::locking(1005, LockKind::Exclusive, WaitPolicy::Block), pipes));
    let mut entry = Entry::new(4);
    let mut derive = FakeDerive::new([five_stock, no_rows, no_rows, no_rows]);
    derive.lock_outcomes = [Outcome::Done, Outcome::Skipped, Outcome::Done, Outcome::Recheck];
    let flag = AtomicU8::new(0);
    let mut sink_ref = sink;
    let mut b = binding(&[1, 2], &NULLS, &mut sink_ref, &flag);
    let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    let r = entry.run(&mut exec, &mut b, Call::batch(64));
    entry.end(exec, false);
    r
}

#[test]
fn the_rows_before_a_recheck_reach_the_sink_before_the_stop() {
    let sink = FakeSink::new();
    assert_eq!(run_to_recheck(&sink).unwrap(), Step::Recheck { site: 0 });
    assert_eq!((sink.rows(), sink.last()[0]), (2, 4), "rows 2 and 4 were sent before the recheck of row 5");
}

/// D1744: in release too. The settle's `&mut Inner` receiver was live across the send while the sink's host wrote the
/// error through the node's `env`, and release kept the error read before the send: `Ok(Recheck { site: 0 })`.
#[test]
fn a_sink_failure_at_the_recheck_send_is_the_error() {
    let sink = FakeSink::new();
    sink.fail_after.set(Some(0));
    assert_eq!(run_to_recheck(&sink), Err(gravity_driver::Error::host("sink full")), "the send before the recheck failed");
}
