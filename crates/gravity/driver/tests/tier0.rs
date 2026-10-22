//! H16 (d) (main 09-24): a tier-0 program's tier is decided at each execution's start (`Entry::start`), never inside it.
//! Its first execution interprets when every pipeline runs serial this execution (`Widths`: declared ineligible, or the
//! runner's width 0) and predicts at most the cap's rows; the second, and a
//! first that is parallel or predicts more, compiles first (synchronously); a failed compile interprets, never the statement's error.
//! The "interpreter" and the "compiled code" here are two hand kernels that count their calls on the test's thread.

use gravity_driver_rig as common;

use std::cell::Cell;
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use std::sync::Arc;

use common::*;
use gravity_abi::Ctx;
use gravity_driver::{Call, Entry, Error, Grant, Widths};
use gravity_program::{Code, End, Layout, Pipeline, Program, Reason, WritePlan};

const NULLS: [u8; 1] = [0];

unsafe extern "C-unwind" fn k_interp(ctx: *mut Ctx) -> i64 {
    TIER0_CALLS.with(|c| c.set((c.get().0 + 1, c.get().1)));
    unsafe { k_noop(ctx) }
}

unsafe extern "C-unwind" fn k_compiled(ctx: *mut Ctx) -> i64 {
    TIER0_CALLS.with(|c| c.set((c.get().0, c.get().1 + 1)));
    unsafe { k_noop(ctx) }
}

fn calls() -> (u32, u32) {
    TIER0_CALLS.with(Cell::get)
}

/// A one-pipeline program over tier-0 code: `rows` predicted, `parallel` or serial, `cap` the knob at its compile; `ok`
/// false makes the promotion fail. The count of promotions' compiles comes back beside it.
fn program(rows: u64, parallel: bool, cap: u64, ok: bool) -> (Arc<Program>, Arc<AtomicU32>) {
    program_bounded(rows, parallel, cap, ok, u64::MAX)
}

/// `program` whose tier-0 code keeps `bound`, the statement's bound on the rows its plan nodes emit (D3867).
fn program_bounded(rows: u64, parallel: bool, cap: u64, ok: bool, bound: u64) -> (Arc<Program>, Arc<AtomicU32>) {
    let ups = Arc::new(AtomicU32::new(0));
    let n = ups.clone();
    let up = move || {
        n.fetch_add(1, Ordering::Relaxed);
        if ok {
            Ok(Code::hand(k_compiled))
        } else {
            Err("gravity_tpde: compilation failed".to_string())
        }
    };
    let code = Code::tiered(Code::hand(k_interp), cap, Box::new(up)).tier0_bound(bound);
    let pipeline = if parallel { Pipeline::eligible(-1, End::Sink, rows) } else { Pipeline { predicted_rows: rows, ..Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite) } };
    let layout = Layout { nsites: 0, out_natts: 0, out_rows: 1, scratch_bytes: 0, exec_bytes: 0 };
    (Arc::new(Program::with_pipelines(layout, &[], &[], WritePlan::SELECT, [(pipeline, code)])), ups)
}

/// One execution to its end: the start's error, or the run's.
fn execute(entry: &mut Entry, program: &Arc<Program>) -> Result<(), Error> {
    execute_at(entry, program, Widths::Declared)
}

/// `execute` with the runner's widths for this execution (`Entry::start_at`).
fn execute_at(entry: &mut Entry, program: &Arc<Program>, widths: Widths<'_>) -> Result<(), Error> {
    let mut derive = FakeDerive::new([no_rows; 4]);
    let flag = AtomicU8::new(0);
    let sink = FakeSink::new();
    let mut sink_ref = &sink;
    let mut b = binding(&[], &NULLS, &mut sink_ref, &flag);
    let mut exec = entry.start_at(None, program, &mut derive, &mut b, lease(Grant::of(8 << 20)), widths)?;
    let r = entry.run(&mut exec, &mut b, Call::batch(1)).map(|_| ());
    entry.end(exec, false);
    r
}

#[test]
fn a_small_serial_programs_first_execution_interprets_and_its_second_compiles_first() {
    let (p, ups) = program(10, false, 100, true);
    let mut entry = Entry::new(4);
    let t = p.code_of(0).tier0().expect("tier-0 code");
    execute(&mut entry, &p).expect("the first execution");
    assert_eq!((calls(), ups.load(Ordering::Relaxed), t.runs(), t.promoted()), ((1, 0), 0, 1, false), "the first execution runs interpreted, nothing compiled");
    assert_eq!(p.code_of(0).kind(), "tier0");
    execute(&mut entry, &p).expect("the second execution");
    assert_eq!((calls(), ups.load(Ordering::Relaxed), t.promoted()), ((1, 1), 1, true), "the second compiles before its first call and runs compiled");
    assert_eq!(p.code_of(0).kind(), "hand", "the promoted code's kind");
    execute(&mut entry, &p).expect("the third execution");
    assert_eq!((calls(), ups.load(Ordering::Relaxed)), ((1, 2), 1), "compiled once");
}

#[test]
fn a_program_predicting_more_rows_than_the_cap_compiles_first() {
    let (p, ups) = program(101, false, 100, true);
    let mut entry = Entry::new(4);
    execute(&mut entry, &p).expect("the first execution");
    assert_eq!((calls(), ups.load(Ordering::Relaxed)), ((0, 1), 1), "over the cap: compiled before the first call");
}

#[test]
fn a_parallel_program_compiles_first() {
    let (p, ups) = program(1, true, 100, true);
    let mut entry = Entry::new(4);
    execute(&mut entry, &p).expect("the first execution");
    assert_eq!((calls(), ups.load(Ordering::Relaxed)), ((0, 1), 1), "a parallel-eligible pipeline: compiled before the first call");
}

#[test]
fn a_parallel_eligible_pipeline_this_execution_runs_serial_interprets_its_first_execution_and_compiles_at_its_second() {
    // lever T (M4-OLTP-COLD, main 09-26): the runner's width for this execution, not the declaration, says serial
    for (i, first) in [Widths::Planned(&[0]), Widths::Serial].into_iter().enumerate() {
        let (p, ups) = program(1, true, 100, true);
        let mut entry = Entry::new(4);
        let base = calls();
        execute_at(&mut entry, &p, first).expect("the first execution");
        assert_eq!((calls().0 - base.0, calls().1 - base.1, ups.load(Ordering::Relaxed)), (1, 0, 0), "case {i}: width 0 this execution: interpreted, nothing compiled");
        execute_at(&mut entry, &p, first).expect("the second execution");
        assert_eq!((calls().0 - base.0, calls().1 - base.1, ups.load(Ordering::Relaxed)), (1, 1, 1), "case {i}: the second compiles before its first call (the hot path runs compiled)");
    }
}

#[test]
fn a_parallel_eligible_pipeline_run_wide_or_over_the_cap_compiles_first() {
    for (i, (rows, w)) in [(1, Widths::Planned(&[4])), (101, Widths::Planned(&[0])), (101, Widths::Serial), (1, Widths::Planned(&[]))].into_iter().enumerate() {
        let (p, ups) = program(rows, true, 100, true);
        let mut entry = Entry::new(4);
        let base = calls();
        execute_at(&mut entry, &p, w).expect("the first execution");
        assert_eq!((calls().0 - base.0, calls().1 - base.1, ups.load(Ordering::Relaxed)), (0, 1, 1), "case {i}: compiled before the first call");
    }
}

#[test]
fn a_prediction_over_the_cap_held_to_the_statements_bound_interprets_its_first_execution() {
    // D3867: PostgreSQL's 10-block guess for a never-analyzed relation (2,550 rows) over a statement whose plan nodes the
    // relation's counted line pointers bound: the gate reads the bound; no bound (u64::MAX) or one over the cap compiles first
    for (i, (bound, want)) in [(40, (1, 0, 0)), (100, (1, 0, 0)), (101, (0, 1, 1)), (u64::MAX, (0, 1, 1))].into_iter().enumerate() {
        for w in [Widths::Planned(&[0]), Widths::Serial] {
            let (p, ups) = program_bounded(2_550, true, 100, true, bound);
            let mut entry = Entry::new(4);
            let base = calls();
            execute_at(&mut entry, &p, w).expect("the first execution");
            assert_eq!((calls().0 - base.0, calls().1 - base.1, ups.load(Ordering::Relaxed)), want, "case {i} at {w:?}");
        }
    }
    // a bound under the cap never makes a wide execution interpret
    let (p, ups) = program_bounded(2_550, true, 100, true, 40);
    let mut entry = Entry::new(4);
    let base = calls();
    execute_at(&mut entry, &p, Widths::Planned(&[4])).expect("the first execution");
    assert_eq!((calls().0 - base.0, calls().1 - base.1, ups.load(Ordering::Relaxed)), (0, 1, 1), "run wide: compiled first");
}

#[test]
fn a_failed_promotion_interprets_this_execution_and_every_later_one_and_is_counted() {
    let (p, ups) = program(10, false, 100, false);
    let mut entry = Entry::new(4);
    let t = p.code_of(0).tier0().expect("tier-0 code");
    execute(&mut entry, &p).expect("the first execution interprets");
    let failed = gravity_program::code::TIER_UP_FAILED.load(Ordering::Relaxed);
    execute(&mut entry, &p).expect("a failed compile is never the statement's error");
    assert!(gravity_program::code::TIER_UP_FAILED.load(Ordering::Relaxed) > failed, "counted");
    assert_eq!((calls(), ups.load(Ordering::Relaxed), t.promoted()), ((2, 0), 1, false), "the second execution ran interpreted after the failed compile");
    assert!(t.failure().is_some_and(|m| m.contains("compilation failed")), "{:?}", t.failure());
    assert_eq!(p.code_of(0).kind(), "tier0failed", "the served line's code= names the failed promotion");
    execute(&mut entry, &p).expect("the third execution");
    assert_eq!((calls(), ups.load(Ordering::Relaxed), t.runs()), ((3, 0), 1, 2), "interpreted again, the compile never retried (runs counts the executions before the tier settled)");
}

#[test]
fn a_first_execution_over_the_cap_whose_compile_fails_interprets() {
    let (p, ups) = program(101, false, 100, false);
    let mut entry = Entry::new(4);
    execute(&mut entry, &p).expect("a failed compile is never the statement's error");
    assert_eq!((calls(), ups.load(Ordering::Relaxed)), ((1, 0), 1), "over the cap: the compile failed, the execution interpreted");
}
