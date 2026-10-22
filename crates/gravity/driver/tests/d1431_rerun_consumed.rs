//! D1431 (W8-RECURSE R1a): a rerun member's consumed slot whose producer has not run when the member is stepped past (a
//! CTE's pulled producer: a recursive term's once-run Hash build over a base table, run by the CTE's first pull) is kept
//! for the statement like one live at that step (D1430): `free_after` freed it after the member's first run, and the next
//! rerun's open failed with "consumes slot N that is not live". Does not apply to a pulled set's last member
//! (`skip_pulled`): a slot it consumes that is not live at its step still frees at its last consumer.
use gravity_driver_rig as common;

use std::sync::atomic::{AtomicI64, AtomicU8, Ordering::Relaxed};
use std::sync::Arc;

use common::*;
use gravity_abi::breaker::{Breaker, BreakerKind};
use gravity_abi::status::DONE;
use gravity_abi::Ctx;
use gravity_driver::{Binding, Call, Entry, Exec, Grant, Pulled, Step};
use gravity_program::{Arena, Code, End, Generation, Handle, Layout, OutCol, Pipeline, PlanKey, Program, Reason, SiteDesc, WritePlan};

const KEY: PlanKey = PlanKey { handle: Handle(33), generation: Generation(4) };

/// The reader's count of slot 0, -1 before it reads.
static READ: AtomicI64 = AtomicI64::new(-1);

unsafe extern "C-unwind" fn k_read(ctx: *mut Ctx) -> i64 {
    READ.store(count_sum(consumed(ctx, 0)).count(0), Relaxed);
    DONE
}

/// Pipeline 0 (the producer outside the set: the term's once-run build) counts site 0 into slot 1; pipeline 1 (the set's
/// member, or the pulled set's last member) consumes slot 1 and counts site 0 into slot 0; pipeline 2 reads slot 0.
fn program() -> Program {
    program_on(0, 1)
}

/// `program` with the member's slot `out` and its consumed slot `kept` (CAP-35: slots past 63).
fn program_on(out: u8, kept: u8) -> Program {
    let st = Arena::new(1 << 14);
    let sites = [SiteDesc::range_source(st.handle(), 2001, &[1, 2])];
    let agg = |slot| End::Breaker(Breaker::sized(BreakerKind::PlainAgg, COUNT_SUM_BYTES).in_slot(slot));
    let pipes = [
        (Pipeline::serial(0, agg(kept), Reason::Host), Code::hand(k_range_count)),
        (Pipeline::serial(0, agg(out), Reason::Host).consuming_one(kept), Code::hand(k_range_count)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming_one(out), Code::hand(k_read)),
    ];
    let cols: [OutCol; 1] = [OutCol::new("c0", 20, -1, 8, true)];
    rerun_set(Program::with_pipelines(Layout { nsites: 1, out_natts: 1, out_rows: 4, scratch_bytes: 0, exec_bytes: 0 }, &sites, &cols, WritePlan::SELECT, pipes), &[1])
}

fn run_whole(e: &mut Entry, x: &mut Exec, b: &mut Binding<'_>, p: usize) -> gravity_driver::Result<()> {
    match e.run_pipeline(x, b, Call::batch(4))? {
        Step::Done => Ok(()),
        s => panic!("pipeline {p} stopped with {s:?}"),
    }
}

/// The tests of this binary one at a time: REGIONS' counters are process-wide.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

const THR: [u64; 1] = [1000];

#[test]
fn a_rerun_members_consumed_slot_produced_after_its_step_is_kept_across_every_rerun() {
    reruns_keep(program(), 1);
}

/// CAP-35: a rerun set's kept slots past 63 (slot sets cover every u8 slot; the u64 masks refused them by name).
#[test]
fn a_rerun_set_over_slots_past_63_is_kept_across_every_rerun() {
    reruns_keep(program_on(70, 65), 65);
}

fn reruns_keep(program: Program, kept: u8) {
    let _g = serial();
    let program = Arc::new(program);
    let mut derive = FakeDerive::ranges(RangeTable::new(4, 10));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&THR, &[0], &mut sink, &flag);
    let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(Grant::of(8 << 20))).unwrap();
    entry.skip_deferred(&mut exec);
    entry.skip_rerun(&mut exec, 2).unwrap();
    assert!(!exec.node().slot_live(kept), "the producer outside the set has not run at the member's step");
    entry.run_nested(&mut exec, &mut b, &[0], &mut run_whole).unwrap();
    assert!(exec.node().slot_live(kept));
    for i in 0..5 {
        entry.rerun_nested(&mut exec, &mut b, &[1], &mut run_whole).unwrap_or_else(|e| panic!("rerun {i}: {e:?}"));
        assert!(exec.node().slot_live(kept), "rerun {i}: the member's consumed slot outlives the member's run");
    }
    READ.store(-1, Relaxed);
    assert_eq!(entry.run_pipeline(&mut exec, &mut b, Call::batch(4)).unwrap(), Step::Done);
    assert!(READ.load(Relaxed) > 0, "the owner reads the last rerun's table");
    entry.end(exec, true);
}

#[test]
fn does_not_apply_a_pulled_sets_consumed_slot_not_live_at_its_step_frees_at_its_last_consumer() {
    let _g = serial();
    let program = Arc::new(program());
    let mut derive = FakeDerive::ranges(RangeTable::new(4, 10));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&THR, &[0], &mut sink, &flag);
    let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(Grant::of(8 << 20))).unwrap();
    entry.skip_deferred(&mut exec);
    entry.skip_pulled(&mut exec, &[2]).unwrap();
    while entry.pull_nested(&mut exec, &mut b, &[0, 1], &mut run_whole).unwrap() == Pulled::Stopped {}
    assert!(!exec.node().slot_live(1), "the pulled member's consumed slot freed at its last consumer");
    assert!(exec.node().slot_live(0), "the store edge stays for the readers");
    assert_eq!(entry.run_pipeline(&mut exec, &mut b, Call::batch(4)).unwrap(), Step::Done);
    entry.end(exec, true);
}
