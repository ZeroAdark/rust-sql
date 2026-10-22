//! D1700 (D1654's runtime): a set run nested (an Init's members, `Entry::run_nested`; a rerun set's, `rerun_nested`) that
//! holds a lazy hash build (D824: PG's outer-first hash join, built when its probe's first row arrives) and its probe ran
//! the build in program order, before its probe, and again at the probe's `BuildWait` over the table it had laid out (the
//! slot still pending: `built_pending` is `build_wait`'s): a doubled table, or (lowerv4's Rerun) a region read as laid out
//! that never was (D1654: "out of memory" over 4 GiB at W0, "region_grow ... past the region's 64 bytes" at W8). The set's
//! run steps the build past now, as the top level's `skip_lazy`, and as PG's rescan of a LEFT / ANTI hash join (every
//! rescan fetches the outer's first row before it builds): the probe's first row builds it once, a run whose probe has no
//! row builds nothing, and the probe reads that one build's table.
//! Pipelines: 0 the lazy build of 1 (slot 0), 1 its probe (slot 1; with 0 the set's members), 2 the set's owner (reads 1).
use gravity_driver_rig as common;

use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU8};
use std::sync::Arc;

use common::*;
use gravity_abi::breaker::{Breaker, BreakerKind};
use gravity_abi::ctx::FLAG_PENDING_SHIFT;
use gravity_abi::status::{build_wait, DONE};
use gravity_abi::Ctx;
use gravity_driver::{Binding, Call, Entry, Exec, Grant, Step, REGIONS};
use gravity_program::{Arena, Code, End, Generation, Handle, Layout, OutCol, Pipeline, PlanKey, Program, Reason, SiteDesc, WritePlan};

const KEY: PlanKey = PlanKey { handle: Handle(37), generation: Generation(1) };
const THR: [u64; 1] = [7];

/// The build's calls (one per run: it reads its whole range in one call).
static BUILDS: AtomicU32 = AtomicU32::new(0);
/// Whether the probe's outer has a row this run (else it ends without waiting, as PG's outer-first join on an empty outer).
static PROBE_ROW: AtomicBool = AtomicBool::new(true);
/// The build's count the probe read after its wait, -1 before.
static PROBE_READ: AtomicI64 = AtomicI64::new(-1);

unsafe extern "C-unwind" fn k_build(ctx: *mut Ctx) -> i64 {
    BUILDS.fetch_add(1, Relaxed);
    k_range_count(ctx)
}

/// The probe: waits for its lazy build at its first row while slot 0 is pending, then reads the build's table.
unsafe extern "C-unwind" fn k_probe(ctx: *mut Ctx) -> i64 {
    if !PROBE_ROW.load(Relaxed) {
        return DONE;
    }
    if (*ctx).flags & 1 << FLAG_PENDING_SHIFT != 0 {
        return build_wait(0);
    }
    PROBE_READ.store(count_sum(consumed(ctx, 0)).count(0), Relaxed);
    DONE
}

fn program() -> Program {
    let st = Arena::new(1 << 14);
    let sites = [SiteDesc::range_source(st.handle(), 2001, &[1, 2])];
    let mut build = Pipeline::serial(0, End::Breaker(Breaker::sized(BreakerKind::PlainAgg, COUNT_SUM_BYTES).in_slot(0)), Reason::Host);
    build.lazy_for = 1;
    let probe = Pipeline::serial(-1, End::Breaker(Breaker::sized(BreakerKind::PlainAgg, COUNT_SUM_BYTES).in_slot(1)), Reason::NoDrivingSite).consuming(&[0]);
    let owner = Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(&[1]);
    let pipes = [(build, Code::hand(k_build)), (probe, Code::hand(k_probe)), (owner, Code::hand(k_noop))];
    let cols: [OutCol; 1] = [OutCol::new("c0", 20, -1, 8, true)];
    rerun_set(Program::with_pipelines(Layout { nsites: 1, out_natts: 1, out_rows: 4, scratch_bytes: 0, exec_bytes: 0 }, &sites, &cols, WritePlan::SELECT, pipes), &[0, 1])
}

fn run_whole(e: &mut Entry, x: &mut Exec, b: &mut Binding<'_>, p: usize) -> gravity_driver::Result<()> {
    match e.run_pipeline(x, b, Call::batch(4))? {
        Step::Done => Ok(()),
        s => panic!("the build (pipeline {p}) stopped with {s:?}"),
    }
}

/// A member's run, as the runner's width-1 producer arm: a `BuildWait` runs the build, then the call resumes at its row.
fn run_member(e: &mut Entry, x: &mut Exec, b: &mut Binding<'_>, p: usize) -> gravity_driver::Result<()> {
    let mut call = Call::batch(4);
    loop {
        match e.run_pipeline(x, b, call)? {
            Step::Done => return Ok(()),
            Step::BuildWait { pipeline } => {
                e.build_wait(x, b, pipeline, &mut run_whole)?;
                call = Call::resume(4);
            }
            s => panic!("member {p} stopped with {s:?}"),
        }
    }
}

/// The tests of this binary one at a time: the kernels' counters and REGIONS' are process-wide.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[test]
fn a_rerun_sets_lazy_build_runs_once_at_its_probes_first_row_and_never_without_one() {
    let _g = serial();
    let program = Arc::new(program());
    let mut derive = FakeDerive::ranges(RangeTable::new(24, 10));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&THR, &[0], &mut sink, &flag);
    let l = lease(Grant::of(8 << 20));
    let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, l.clone()).unwrap();
    assert!(entry.skip_lazy(&mut exec).unwrap(), "the lazy build steps past in order");
    entry.skip_rerun(&mut exec, 2).unwrap();
    let mut once = -1;
    let mut after_first = (0, 0);
    for (i, row) in [true, true, false, true, false, false, true, true, false].into_iter().enumerate() {
        PROBE_ROW.store(row, Relaxed);
        PROBE_READ.store(-1, Relaxed);
        let builds = BUILDS.load(Relaxed);
        entry.rerun_nested(&mut exec, &mut b, &[0, 1], &mut run_member).unwrap();
        assert_eq!(BUILDS.load(Relaxed) - builds, row as u32, "rerun {i}: the build runs once at its probe's first row, never without one");
        if !row {
            assert_eq!(PROBE_READ.load(Relaxed), -1);
            continue;
        }
        let read = PROBE_READ.load(Relaxed);
        assert!(read > 0, "rerun {i}: the probe reads the build's table ({read})");
        if once < 0 {
            (once, after_first) = (read, (REGIONS.stats().takes, l.drawn()));
        }
        assert_eq!(read, once, "rerun {i}: the probe reads one build's rows, never a table built over the one before");
    }
    assert_eq!((REGIONS.stats().takes, l.drawn()), after_first, "the build's kept slot: no region or grant drawn after its first run");
    assert_eq!(entry.run_pipeline(&mut exec, &mut b, Call::batch(4)).unwrap(), Step::Done);
    entry.end(exec, true);
}

#[test]
fn an_init_sets_lazy_build_runs_once_at_its_probes_first_row() {
    let _g = serial();
    let program = Arc::new(program());
    let mut derive = FakeDerive::ranges(RangeTable::new(24, 10));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&THR, &[0], &mut sink, &flag);
    let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(Grant::of(8 << 20))).unwrap();
    assert!(entry.skip_lazy(&mut exec).unwrap(), "the lazy build steps past in order");
    entry.skip_deferred(&mut exec);
    PROBE_ROW.store(true, Relaxed);
    PROBE_READ.store(-1, Relaxed);
    let builds = BUILDS.load(Relaxed);
    entry.run_nested(&mut exec, &mut b, &[0, 1], &mut run_member).unwrap();
    assert_eq!(BUILDS.load(Relaxed) - builds, 1, "the build runs once, at its probe's first row");
    assert!(PROBE_READ.load(Relaxed) > 0, "the probe reads the build's table");
    assert_eq!(entry.run_pipeline(&mut exec, &mut b, Call::batch(4)).unwrap(), Step::Done);
    entry.end(exec, true);
}
