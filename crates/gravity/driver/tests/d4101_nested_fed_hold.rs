//! D4101 (k2; op 14 in a nested run): a nest's rerun set holds a build fed by an earlier member (PG's outer-first hash join
//! in the nest's inner, its Hash over a HashAggregate the set runs again at every outer row). The feeder's error is held
//! for the build (`Entry::defer_fed`) so that the probe's first row raises it and a probe side with no row never does.
//! Inside the set's run the owner (the nest's pipeline, whose call the run holds) lists its members' slots among those
//! it consumes (kept for the statement, D3515), so the walk by last consumer reached the owner, not the build: the error
//! left the call unheld, ahead of the probe. And the run went on to the build the hold had passed. Now the walk passes
//! the owner by, and the run steps past what the hold passed, as the top level's runner does.
//! Pipelines: 0 the feeder (a range count, slot 0), 1 the build (reads 0, slot 1; the deferring merge), 2 the set's owner
//! (keeps 0 and 1).
use gravity_driver_rig as common;

use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicU32, AtomicU8, AtomicUsize};
use std::sync::Arc;

use common::*;
use gravity_abi::breaker::{Breaker, BreakerKind};
use gravity_abi::status::DONE;
use gravity_abi::Ctx;
use gravity_driver::{Binding, Call, Entry, Error, Exec, Grant, Merge, Result, State, Step};
use gravity_program::{Arena, Code, End, Generation, Handle, Layout, OutCol, Pipeline, PlanKey, Program, Reason, SiteDesc, WritePlan};

const KEY: PlanKey = PlanKey { handle: Handle(41), generation: Generation(1) };
const THR: [u64; 1] = [7];

/// The build's runs.
static BUILDS: AtomicU32 = AtomicU32::new(0);

unsafe extern "C-unwind" fn k_build(_ctx: *mut Ctx) -> i64 {
    BUILDS.fetch_add(1, Relaxed);
    DONE
}

/// Every merge but the build's.
struct Plain;

impl Merge for Plain {}

/// JOIN's outer-first build as `defer_fed` finds it: the variant that defers, taking the error (counted).
struct FedBuild(AtomicUsize);

impl Merge for FedBuild {
    fn defer_error(&self, _leader: &mut State<'_>) -> Result<bool> {
        self.0.fetch_add(1, Relaxed);
        Ok(true)
    }
    fn defers(&self) -> bool {
        true
    }
}

static PLAIN: Plain = Plain;
static FED: FedBuild = FedBuild(AtomicUsize::new(0));

fn merge_for(p: usize) -> &'static dyn Merge {
    if p == 1 {
        &FED
    } else {
        &PLAIN
    }
}

fn program() -> Program {
    let st = Arena::new(1 << 14);
    let sites = [SiteDesc::range_source(st.handle(), 2001, &[1, 2])];
    let feeder = Pipeline::serial(0, End::Breaker(Breaker::sized(BreakerKind::PlainAgg, COUNT_SUM_BYTES).in_slot(0)), Reason::Host);
    let build = Pipeline::serial(-1, End::Breaker(Breaker::sized(BreakerKind::PlainAgg, COUNT_SUM_BYTES).in_slot(1)), Reason::NoDrivingSite).consuming(&[0]);
    let owner = Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(&[0, 1]);
    let pipes = [(feeder, Code::hand(k_range_count)), (build, Code::hand(k_build)), (owner, Code::hand(k_noop))];
    let cols: [OutCol; 1] = [OutCol::new("c0", 20, -1, 8, true)];
    rerun_set(Program::with_pipelines(Layout { nsites: 1, out_natts: 1, out_rows: 4, scratch_bytes: 0, exec_bytes: 0 }, &sites, &cols, WritePlan::SELECT, pipes), &[0, 1])
}

/// A member's run, as the runner's width-1 producer arm: its error held when a merge defers it (`defer_failed`), or
/// the deferring build it feeds takes it (`defer_fed`), else the run's error.
fn run_member(e: &mut Entry, x: &mut Exec, b: &mut Binding<'_>, p: usize) -> Result<()> {
    match e.run_pipeline(x, b, Call::batch(4)) {
        Ok(Step::Done) => Ok(()),
        Ok(s) => panic!("member {p} stopped with {s:?}"),
        Err(err) => e.defer_failed(x, b, merge_for(p), p, err).or_else(|err| e.defer_fed(x, b, &merge_for, p, err)),
    }
}

#[test]
fn a_nested_sets_fed_build_holds_its_feeders_error_and_the_run_steps_past_it() {
    let program = Arc::new(program());
    let table = RangeTable::new(24, 10);
    let mut derive = FakeDerive::ranges(table.clone());
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&THR, &[0], &mut sink, &flag);
    let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(Grant::of(8 << 20))).unwrap();
    entry.skip_rerun(&mut exec, 2).unwrap();
    entry.skip_rerun(&mut exec, 2).unwrap();
    for (i, fails) in [true, false, true, true, false].into_iter().enumerate() {
        table.fail_block.store(if fails { 5 } else { u64::MAX }, Relaxed);
        let (builds, taken) = (BUILDS.load(Relaxed), FED.0.load(Relaxed));
        let r = entry.rerun_nested(&mut exec, &mut b, &[0, 1], &mut run_member);
        assert_eq!(r, Ok(()), "run {i}: the feeder's error is held for the build, the run goes on");
        let held = exec.held_error();
        if fails {
            assert_eq!(held, Some(Error::host("range read failed")), "run {i}: held for the probe's first row");
            assert_eq!(FED.0.load(Relaxed) - taken, 1, "run {i}: the build's merge took it once");
            assert_eq!(BUILDS.load(Relaxed) - builds, 0, "run {i}: the build the hold passed does not run");
        } else {
            assert_eq!(held, None, "run {i}: a run that held nothing drops the error an earlier run held for its member");
            assert_eq!(FED.0.load(Relaxed) - taken, 0, "run {i}");
            assert_eq!(BUILDS.load(Relaxed) - builds, 1, "run {i}: the build runs");
        }
    }
    table.fail_block.store(u64::MAX, Relaxed);
    assert_eq!(entry.run_pipeline(&mut exec, &mut b, Call::batch(4)).unwrap(), Step::Done);
    entry.end(exec, true);
    assert_eq!(table.pins.load(Relaxed), 0);
}
