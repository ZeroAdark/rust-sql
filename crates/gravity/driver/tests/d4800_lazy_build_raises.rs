//! D4800 (op 14 over D824's lazy build): a lazy build runs in its reader's `build_wait`, once a row reaches that reader,
//! which is where PG's outer-first join builds and raises. Its reader takes no deferral over a lazy build (lowerv4 hashjoin
//! probe.rs `defer && !lazy`), but the build's merge kept its DEFER bit (a keyed nest's raising inner under road
//! nest.key.eq), so the build's error, or its feeder's (`defer_fed`), was held for a first row that had already passed:
//! nothing raised it, the build read as failed-empty, and the statement returned (0, 0) where PG raises. Now a lazy
//! build's error is not held: the wait returns it. An eager deferred build still holds its error for the first row, and
//! a lazy held run's fill (a merge that holds its own error and defers no feeder, as exec ops/sort/held.rs `HeldRows`:
//! D1108's plan Material run whole at the nest's first outer row) still holds its error for its reader's record.
//! Programs: own (0 the lazy build of 1, slot 0, deferring; 1 the probe); fed (0 the feeder, slot 0, 1 the lazy build
//! of 2 that reads it, slot 1, deferring; 2 the probe); eager (0 the build, deferring, not lazy; 1 the probe).
use gravity_driver_rig as common;

use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicUsize};
use std::sync::Arc;

use common::*;
use gravity_abi::breaker::{Breaker, BreakerKind};
use gravity_abi::ctx::FLAG_PENDING_SHIFT;
use gravity_abi::status::{build_wait, DONE};
use gravity_abi::Ctx;
use gravity_driver::{Binding, Call, Entry, Error, Exec, Grant, Merge, Result, State, Step};
use gravity_program::{Arena, Code, End, Generation, Handle, Layout, OutCol, Pipeline, PlanKey, Program, Reason, SiteDesc, WritePlan};

const THR: [u64; 1] = [7];

/// Whether the probe's outer has a row (else it ends without waiting, as PG's outer-first join over an empty outer).
static PROBE_ROW: AtomicBool = AtomicBool::new(true);
/// The build's count the probe read, -1 before.
static PROBE_READ: AtomicI64 = AtomicI64::new(-1);

/// The probe: waits for its lazy build at its first row while its consumed slot is pending, then reads the build.
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

/// The fed build: its feeder's rows reach it, nothing more.
unsafe extern "C-unwind" fn k_build(_ctx: *mut Ctx) -> i64 {
    DONE
}

struct Plain;

impl Merge for Plain {}

/// JOIN's outer-first build with its DEFER bit: it takes the error (counted).
struct Deferring(AtomicUsize);

impl Merge for Deferring {
    fn defer_error(&self, _leader: &mut State<'_>) -> Result<bool> {
        self.0.fetch_add(1, Relaxed);
        Ok(true)
    }
    fn defers(&self) -> bool {
        true
    }
}

/// A held run's fill (exec ops/sort/held.rs `HeldRows`, D1108's plan Material): it takes its own error (counted), and is
/// not op 14's deferring build variant (`defers` false): its reader raises the error at the record the fill stopped at.
struct HeldRun(AtomicUsize);

impl Merge for HeldRun {
    fn defer_error(&self, _leader: &mut State<'_>) -> Result<bool> {
        self.0.fetch_add(1, Relaxed);
        Ok(true)
    }
}

static PLAIN: Plain = Plain;
static DEFER: Deferring = Deferring(AtomicUsize::new(0));
static HELD_RUN: HeldRun = HeldRun(AtomicUsize::new(0));
/// Whether the build's merge is the held run's (`HELD_RUN`) rather than the deferring build's (`DEFER`).
static RUN_MERGE: AtomicBool = AtomicBool::new(false);

/// The build's merge's takes so far, either merge's.
fn takes() -> usize {
    DEFER.0.load(Relaxed) + HELD_RUN.0.load(Relaxed)
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Shape {
    Own,
    Fed,
    Eager,
}

impl Shape {
    /// The build's pipeline (the deferring merge).
    fn build(self) -> usize {
        if self == Shape::Fed {
            1
        } else {
            0
        }
    }

    fn probe(self) -> usize {
        self.build() + 1
    }

    fn program(self) -> Program {
        let st = Arena::new(1 << 14);
        let sites = [SiteDesc::range_source(st.handle(), 2001, &[1, 2])];
        let cols: [OutCol; 1] = [OutCol::new("c0", 20, -1, 8, true)];
        let layout = Layout { nsites: 1, out_natts: 1, out_rows: 4, scratch_bytes: 0, exec_bytes: 0 };
        let agg = |slot: u8| End::Breaker(Breaker::sized(BreakerKind::PlainAgg, COUNT_SUM_BYTES).in_slot(slot));
        let probe = || Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite);
        match self {
            Shape::Own | Shape::Eager => {
                let mut build = Pipeline::serial(0, agg(0), Reason::Host);
                if self == Shape::Own {
                    build.lazy_for = 1;
                }
                Program::with_pipelines(layout, &sites, &cols, WritePlan::SELECT, [(build, Code::hand(k_range_count)), (probe().consuming(&[0]), Code::hand(k_probe))])
            }
            Shape::Fed => {
                let mut feeder = Pipeline::serial(0, agg(0), Reason::Host);
                feeder.lazy_for = 2;
                let mut build = Pipeline::serial(-1, agg(1), Reason::NoDrivingSite).consuming(&[0]);
                build.lazy_for = 2;
                let pipes = [(feeder, Code::hand(k_range_count)), (build, Code::hand(k_build)), (probe().consuming(&[1]), Code::hand(k_probe))];
                Program::with_pipelines(layout, &sites, &cols, WritePlan::SELECT, pipes)
            }
        }
    }

    fn merge_for(self) -> impl Fn(usize) -> &'static dyn Merge {
        move |p: usize| -> &'static dyn Merge {
            if p == self.build() && RUN_MERGE.load(Relaxed) {
                &HELD_RUN
            } else if p == self.build() {
                &DEFER
            } else {
                &PLAIN
            }
        }
    }
}

/// A producer's run, as the runner's width-1 producer arm: its error held when its merge defers it (`defer_failed`) or
/// the deferring build it feeds takes it (`defer_fed`), else the run's error.
fn run_producer(shape: Shape, e: &mut Entry, x: &mut Exec, b: &mut Binding<'_>, p: usize) -> Result<()> {
    let merge_for = shape.merge_for();
    match e.run_pipeline(x, b, Call::batch(4)) {
        Ok(Step::Done) => Ok(()),
        Ok(s) => panic!("producer {p} stopped with {s:?}"),
        Err(err) => e.defer_failed(x, b, merge_for(p), p, err).or_else(|err| e.defer_fed(x, b, &merge_for, p, err)),
    }
}

/// The probe's run: a `BuildWait` runs the lazy build (`run_producer` for each pipeline of its wait), then the call
/// resumes at its row.
fn run_probe(shape: Shape, e: &mut Entry, x: &mut Exec, b: &mut Binding<'_>) -> Result<()> {
    let mut call = Call::batch(4);
    loop {
        match e.run_pipeline(x, b, call)? {
            Step::Done => return Ok(()),
            Step::BuildWait { pipeline } => {
                e.build_wait(x, b, pipeline, &mut |e, x, b, p| run_producer(shape, e, x, b, p))?;
                call = Call::resume(4);
            }
            s => panic!("the probe stopped with {s:?}"),
        }
    }
}

/// One statement of `shape`: the build's range failing (`fails`) or not, the probe's outer with a row or not. The
/// statement's result, the error held at its end, the build's merge's takes and the probe's read.
fn statement(shape: Shape, handle: u64, fails: bool, row: bool) -> (Result<()>, Option<Error>, usize, i64) {
    let program = Arc::new(shape.program());
    let table = RangeTable::new(24, 10);
    table.fail_block.store(if fails { 5 } else { u64::MAX }, Relaxed);
    let mut derive = FakeDerive::ranges(table.clone());
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&THR, &[0], &mut sink, &flag);
    let key = PlanKey { handle: Handle(handle), generation: Generation(1) };
    let mut exec = entry.start(Some(key), &program, &mut derive, &mut b, lease(Grant::of(8 << 20))).unwrap();
    PROBE_ROW.store(row, Relaxed);
    PROBE_READ.store(-1, Relaxed);
    let taken = takes();
    let r = (|| {
        for p in 0..shape.probe() {
            if !entry.skip_lazy(&mut exec)? {
                assert_eq!(shape, Shape::Eager, "pipeline {p} steps past only when lazy");
                run_producer(shape, &mut entry, &mut exec, &mut b, p)?;
            }
        }
        run_probe(shape, &mut entry, &mut exec, &mut b)
    })();
    let held = exec.held_error();
    let ok = r.is_ok();
    entry.end(exec, ok);
    assert_eq!(table.pins.load(Relaxed), 0, "{shape:?}: no pin outlives the statement");
    (r, held, takes() - taken, PROBE_READ.load(Relaxed))
}

/// The tests of this binary one at a time: the probe's and the merges' counters are statics (the build's merge the
/// deferring build's unless a test picks the held run's).
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    RUN_MERGE.store(false, Relaxed);
    g
}

#[test]
fn a_lazy_builds_error_raises_at_its_wait_and_is_never_held() {
    let _g = serial();
    for (shape, handle) in [(Shape::Own, 4800), (Shape::Fed, 4801)] {
        let (r, held, taken, read) = statement(shape, handle, true, true);
        assert_eq!(r, Err(Error::host("range read failed")), "{shape:?}: the wait raises the build's error, as PG's join raises it there");
        assert_eq!(held, None, "{shape:?}: nothing held for a first row that has passed");
        assert_eq!(taken, 0, "{shape:?}: the build's merge never takes the error");
        assert_eq!(read, -1, "{shape:?}: the probe never reads a failed build");
    }
}

#[test]
fn a_lazy_build_with_no_row_at_its_probe_never_runs_and_one_that_succeeds_is_read() {
    let _g = serial();
    for (shape, handle) in [(Shape::Own, 4802), (Shape::Fed, 4803)] {
        let (r, held, taken, read) = statement(shape, handle, true, false);
        assert_eq!((r, held, taken, read), (Ok(()), None, 0, -1), "{shape:?}: an empty outer never builds, never raises");
        let (r, held, taken, read) = statement(shape, handle + 10, false, true);
        assert_eq!((r, held, taken), (Ok(()), None, 0), "{shape:?}: a quiet build");
        assert!(read > 0 || shape == Shape::Fed, "{shape:?}: the probe reads the build's count ({read})");
    }
}

#[test]
fn an_eager_deferred_builds_error_is_still_held_for_the_first_row() {
    let _g = serial();
    let (r, held, taken, _) = statement(Shape::Eager, 4804, true, false);
    assert_eq!(r, Ok(()), "the eager build's error is held: an empty outer never raises it");
    assert_eq!(held, Some(Error::host("range read failed")), "held for the probe's first row");
    assert_eq!(taken, 1, "the build's merge took it once");
}

#[test]
fn a_lazy_held_runs_fill_still_holds_its_error_for_its_readers_record() {
    let _g = serial();
    RUN_MERGE.store(true, Relaxed);
    let (r, held, taken, _) = statement(Shape::Own, 4805, true, true);
    assert_eq!(r, Ok(()), "D1108: the fill's error is held over its lazy run too, not raised at the wait");
    assert_eq!(held, Some(Error::host("range read failed")), "held for the reader, which raises it at its record");
    assert_eq!(taken, 1, "the held run's merge took it once");
}
