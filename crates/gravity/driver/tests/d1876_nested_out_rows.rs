//! D1876: a set run nested inside the outer call (a rerun set's members, `Entry::rerun_nested`; an Init's, `run_nested`)
//! whose member ends as every lowered pipeline does, through the kernels' `done` (the out block flushed when it holds
//! rows), pushed the rows the outer call had stored in the out block, mid-row, and dropped the receiver's stop: a
//! cursor's FETCH n ran on past its cap, re-running the set (its VOLATILE calls) per row, until the pull buffer
//! overflowed. The outer call's rows now stay in its out block across the run, and the receiver sees nothing from it.
//! Pipelines: 0 the member (slot 0), 1 the owner (the sink).
use gravity_driver_rig as common;

use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use common::*;
use gravity_abi::breaker::{Breaker, BreakerKind};
use gravity_abi::status::{DONE, ERROR};
use gravity_abi::Ctx;
use gravity_driver::{Binding, Call, Entry, Error, Exec, Grant, Step};
use gravity_program::{Arena, Code, End, Generation, Handle, Layout, OutCol, Pipeline, PlanKey, Program, Reason, SiteDesc, WritePlan};

const KEY: PlanKey = PlanKey { handle: Handle(41), generation: Generation(1) };
const THR: [u64; 1] = [7];

/// The member: its count, then the kernels' `done` (kernels util.rs): the out block pushed when it holds rows.
unsafe extern "C-unwind" fn k_member(ctx: *mut Ctx) -> i64 {
    let r = k_range_count(ctx);
    if r != DONE {
        return r;
    }
    if (*(*ctx).out).count > 0 && gravity_abi::program::push(ctx) < 0 {
        return ERROR;
    }
    DONE
}

/// The member that ends in the out block: one row stored there (no lowered member does).
unsafe extern "C-unwind" fn k_member_row(ctx: *mut Ctx) -> i64 {
    let o = (*ctx).out;
    *(*o).values.add((*o).count as usize) = 99;
    *(*o).isnull.add((*o).count as usize) = 0;
    (*o).count += 1;
    DONE
}

fn program(member: unsafe extern "C-unwind" fn(*mut Ctx) -> i64) -> Program {
    let st = Arena::new(1 << 14);
    let sites = [SiteDesc::range_source(st.handle(), 2001, &[1, 2])];
    let m = Pipeline::serial(0, End::Breaker(Breaker::sized(BreakerKind::PlainAgg, COUNT_SUM_BYTES).in_slot(0)), Reason::Host);
    let owner = Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(&[0]);
    let cols: [OutCol; 1] = [OutCol::new("c0", 20, -1, 8, true)];
    rerun_set(Program::with_pipelines(Layout { nsites: 1, out_natts: 1, out_rows: 4, scratch_bytes: 0, exec_bytes: 0 }, &sites, &cols, WritePlan::SELECT, [(m, Code::hand(member)), (owner, Code::hand(k_noop))]), &[0])
}

fn run_whole(e: &mut Entry, x: &mut Exec, b: &mut Binding<'_>, p: usize) -> gravity_driver::Result<()> {
    match e.run_pipeline(x, b, Call::batch(4))? {
        Step::Done => Ok(()),
        s => panic!("member {p} stopped with {s:?}"),
    }
}

/// The outer call's rows in the out block: `rows` stored, as its sink leaves them below the cap.
fn store(exec: &Exec, rows: &[u64]) {
    // SAFETY: the bound node's out block, `out_rows` (4) rows of one column.
    unsafe {
        let o = (*exec.node().ctx()).out;
        for (i, &v) in rows.iter().enumerate() {
            *(*o).values.add(i) = v;
            *(*o).isnull.add(i) = 0;
        }
        (*o).count = rows.len() as i64;
    }
}

fn out_rows(exec: &Exec) -> Vec<u64> {
    // SAFETY: as `store`.
    unsafe {
        let o = (*exec.node().ctx()).out;
        (0..(*o).count as usize).map(|i| *(*o).values.add(i)).collect()
    }
}

#[test]
fn a_rerun_sets_members_leave_the_outer_calls_rows_in_its_out_block() {
    let program = Arc::new(program(k_member));
    let mut derive = FakeDerive::ranges(RangeTable::new(24, 10));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    sink.stop_after = Some(1);
    let mut b = binding(&THR, &[0], &mut sink, &flag);
    let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(Grant::of(8 << 20))).unwrap();
    entry.skip_rerun(&mut exec, 1).unwrap();
    for (i, rows) in [&[][..], &[11], &[11, 12], &[11, 12, 13]].into_iter().enumerate() {
        store(&exec, rows);
        entry.rerun_nested(&mut exec, &mut b, &[0], &mut run_whole).unwrap();
        assert_eq!(out_rows(&exec), rows, "rerun {i}: the outer call's rows stay in its out block");
    }
    store(&exec, &[]);
    entry.end(exec, true);
    drop(b);
    assert!(sink.rows.lock().unwrap().is_empty(), "the receiver saw nothing from a member's end");
}

#[test]
fn an_init_sets_members_leave_the_outer_calls_rows_in_its_out_block() {
    let program = Arc::new(program(k_member));
    let mut derive = FakeDerive::ranges(RangeTable::new(24, 10));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&THR, &[0], &mut sink, &flag);
    let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(Grant::of(8 << 20))).unwrap();
    entry.skip_deferred(&mut exec);
    store(&exec, &[21, 22, 23]);
    entry.run_nested(&mut exec, &mut b, &[0], &mut run_whole).unwrap();
    assert_eq!(out_rows(&exec), [21, 22, 23], "the outer call's rows stay in its out block");
    store(&exec, &[]);
    entry.end(exec, true);
    drop(b);
    assert!(sink.rows.lock().unwrap().is_empty(), "the receiver saw nothing from a member's end");
}

#[test]
fn a_member_that_leaves_rows_in_the_out_block_is_a_bug_by_name() {
    let program = Arc::new(program(k_member_row));
    let mut derive = FakeDerive::ranges(RangeTable::new(24, 10));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&THR, &[0], &mut sink, &flag);
    let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(Grant::of(8 << 20))).unwrap();
    entry.skip_rerun(&mut exec, 1).unwrap();
    store(&exec, &[31]);
    match entry.rerun_nested(&mut exec, &mut b, &[0], &mut run_whole) {
        Err(Error::Refused(m)) => assert!(m.contains("gravity: Bug: a nested run left 1 rows in the outer call's out block (D1876)"), "{m}"),
        r => panic!("a member's row in the out block: {r:?}"),
    }
    assert_eq!(out_rows(&exec).len(), 1, "the outer call's count is put back (the statement fails with the Bug)");
    store(&exec, &[]);
    entry.end(exec, false);
}
