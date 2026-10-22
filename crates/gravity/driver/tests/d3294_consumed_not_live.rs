//! D3294 (D3262's site): a pipeline opened over a consumed slot that is not live (its producer never ran, or a re-attached
//! call over a slot released under it) is the statement's error by name. `consumed_ptrs` asserted it, and a panic there
//! (on a host's frame or past it) aborts the server (D2933).
use gravity_driver_rig as common;

use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use common::*;
use gravity_abi::breaker::{Breaker, BreakerKind};
use gravity_driver::{Call, Entry, Error, Exec, Grant, Step};
use gravity_program::{Arena, Code, End, Generation, Handle, Layout, OutCol, Pipeline, PlanKey, Program, Reason, SiteDesc, WritePlan};

const KEY: PlanKey = PlanKey { handle: Handle(34), generation: Generation(1) };

/// Pipeline 0 counts site 0 into slot 1; pipeline 1 consumes slot 1 and counts site 0 into slot 0; pipeline 2 sinks.
fn program() -> Program {
    let st = Arena::new(1 << 14);
    let sites = [SiteDesc::range_source(st.handle(), 2001, &[1, 2])];
    let agg = |slot| End::Breaker(Breaker::sized(BreakerKind::PlainAgg, COUNT_SUM_BYTES).in_slot(slot));
    let pipes = [
        (Pipeline::serial(0, agg(1), Reason::Host), Code::hand(k_range_count)),
        (Pipeline::serial(0, agg(0), Reason::Host).consuming_one(1), Code::hand(k_range_count)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming_one(0), Code::hand(k_range_count)),
    ];
    let cols: [OutCol; 1] = [OutCol::new("c0", 20, -1, 8, true)];
    Program::with_pipelines(Layout { nsites: 1, out_natts: 1, out_rows: 4, scratch_bytes: 0, exec_bytes: 0 }, &sites, &cols, WritePlan::SELECT, pipes)
}

fn run_whole(e: &mut Entry, x: &mut Exec, b: &mut gravity_driver::Binding<'_>, p: usize) -> gravity_driver::Result<()> {
    match e.run_pipeline(x, b, Call::batch(4))? {
        Step::Done => Ok(()),
        s => panic!("pipeline {p} stopped with {s:?}"),
    }
}

#[test]
fn a_pipeline_over_a_consumed_slot_that_is_not_live_is_the_statements_error_not_a_panic() {
    let program = Arc::new(program());
    let mut derive = FakeDerive::ranges(RangeTable::new(4, 10));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[1000], &[0], &mut sink, &flag);
    let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(Grant::of(8 << 20))).unwrap();
    entry.skip_deferred(&mut exec);
    assert!(!exec.node().slot_live(1), "slot 1's producer has not run");
    let got = entry.run_nested(&mut exec, &mut b, &[1], &mut run_whole);
    match got {
        Err(Error::Host(m)) => assert!(m.contains("pipeline 1 consumes slot 1 that is not live"), "{m}"),
        other => panic!("expected the statement's error by name, got {other:?}"),
    }
    entry.end(exec, false);
}
