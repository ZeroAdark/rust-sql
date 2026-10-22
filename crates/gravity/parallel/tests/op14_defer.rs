//! Op 14 (JOIN's deferred build error, main 09-24): PG's outer-first hash join never runs its build when the outer is
//! empty, so a build's error surfaces only when a probe row reaches it. A producer whose merge defers its error
//! (`Merge::defer_error` → `Ok(true)`, JOIN's FORM FAILED stamp) is held by the runner (`Entry::defer_failed`) and passed as
//! done; the statement runs on. Here: two counting producers over one relation into one emit (`p_two_slots`), the
//! relation failing a read. With merges that defer, the statement completes at every width with the read's error held
//! (the consumer would raise it through `program::raise_held`); with the fixture's merges it ends with that error, as today.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use common::*;
use gravity_abi::breaker::{Breaker, BreakerKind, HashAgg};
use gravity_abi::status::{DONE, ERROR};
use gravity_abi::Ctx;
use gravity_driver::{Binding, Entry, Error, Exec, Grant, Merge, Result, State};
use gravity_parallel::{run_program, FixturePool, MorselSource, Plan};
use gravity_kernels::util::{consumed, emit, flush};
use gravity_program::{Code, End, Generation, Handle, Layout, OutCol, Pipeline, PlanKey, Program, Reason, SiteDesc, WritePlan};

/// One plan key: every statement reuses the node the last one parked (the held error must not outlive its statement).
const KEY: PlanKey = PlanKey { handle: Handle(14), generation: Generation(1) };

/// A merge that takes its pipeline's error as deferred (it would stamp the region failed), counting the calls.
struct Defers(AtomicUsize);

impl Merge for Defers {
    fn defer_error(&self, _leader: &mut State<'_>) -> Result<bool> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(true)
    }
}

struct Deferring<'a> {
    inner: FixturePlan,
    merge: Option<&'a Defers>,
}

impl Plan for Deferring<'_> {
    fn width_of(&self, p: usize) -> u16 {
        self.inner.width_of(p)
    }
    fn claims_for(&self, p: usize) -> &dyn MorselSource {
        self.inner.claims_for(p)
    }
    fn merge_for(&self, p: usize) -> &dyn Merge {
        match self.merge {
            Some(m) if p < 2 => m,
            _ => self.inner.merge_for(p),
        }
    }
    fn after_pipeline(&self, p: usize, exec: &mut Exec) -> Result<()> {
        self.inner.after_pipeline(p, exec)
    }
}

#[test]
fn a_producer_whose_merge_defers_its_error_is_held_and_the_statement_runs_on_at_every_width() {
    let table = FixtureTable::new(T_A, 120, 50, gen_a);
    let mut derive = FixtureDerive::new(&[&table]);
    let program = Arc::new(p_two_slots(T_A, 120 * 50));
    let pool = FixturePool::new(8);
    let mut entry = Entry::new(2);
    let defers = Defers(AtomicUsize::new(0));
    for w in WIDTHS {
        for (merge, deferred) in [(None, false), (Some(&defers), true)] {
            table.fresh();
            table.fail_block.store(17, Ordering::Relaxed);
            let calls = defers.0.load(Ordering::Relaxed);
            let plan = Deferring { inner: FixturePlan::new(&program, &derive, w, 3), merge };
            let mut sink = LegSink::new(true);
            let nulls = [0u8; 1];
            let mut b = Binding::new(parts(&[700], &nulls, &NO_ABORT), &mut sink);
            let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(Grant::of(64 << 20))).expect("started");
            let r = run_program(&mut entry, &mut exec, &mut b, &mut derive, &pool, &plan);
            let held = exec.held_error();
            entry.end(exec, r.is_ok());
            if deferred {
                assert_eq!(r, Ok(1), "width {w}: both producers' errors deferred, the emit runs");
                assert_eq!(held, Some(Error::host("range read failed")), "width {w}: the first error held for the consumer's raise");
                assert_eq!(defers.0.load(Ordering::Relaxed), calls + 2, "width {w}: each failed producer's merge asked once");
            } else {
                assert_eq!(r, Err(Error::host("range read failed")), "width {w}: the statement's error, as today");
                assert_eq!(held, None, "width {w}");
            }
            assert_eq!(table.pins.load(Ordering::Relaxed), 0, "width {w}");
        }
    }
    table.fail_block.store(u64::MAX, Ordering::Relaxed);
    // the parked node that held an error starts the next statement with nothing held and runs clean
    table.fresh();
    let plan = Deferring { inner: FixturePlan::new(&program, &derive, 4, 3), merge: None };
    let mut sink = LegSink::new(true);
    let nulls = [0u8; 1];
    let mut b = Binding::new(parts(&[700], &nulls, &NO_ABORT), &mut sink);
    let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(Grant::of(64 << 20))).expect("started");
    assert_eq!(exec.held_error(), None, "nothing held from the last statement");
    let r = run_program(&mut entry, &mut exec, &mut b, &mut derive, &pool, &plan);
    entry.end(exec, r.is_ok());
    assert_eq!(r, Ok(1));
    assert_eq!(entry.stats().built, 1, "one node, parked across every statement");
}

// ---- the raise (V4R-9): `program::raise_held` from the consumer, as JOIN's probe calls it at its first outer row

/// `(build, outer)` counts from the two slots; PG's outer-first probe: the build's error surfaces only once an outer
/// row reaches the join, so the raise is asked only when the outer counted one (it answers 0 when nothing is held).
unsafe extern "C-unwind" fn k_emit_raise(ctx: *mut Ctx) -> i64 {
    use gravity_kernels::par_scan_count::{BYTES, LAYOUT};
    let a = HashAgg::at(consumed(ctx, 0), &LAYOUT, 1, BYTES).single().count(0);
    let b = HashAgg::at(consumed(ctx, 1), &LAYOUT, 1, BYTES).single().count(0);
    if b > 0 && gravity_abi::program::raise_held(ctx) < 0 {
        return ERROR;
    }
    let v = [(a + b) as u64, a as u64, b as u64];
    let n = [0u8; 3];
    if emit(ctx, 3, &[(v.as_ptr(), n.as_ptr(), 3)]) < 0 || flush(ctx) < 0 {
        return ERROR;
    }
    DONE
}

/// `p_two_slots` with the raising emit: pipeline 0 the build, pipeline 1 the outer, both `a < $1` over one relation.
fn p_raise(rel: u32, rows: u64) -> Program {
    use gravity_kernels::par_scan_count::{par_scan_count, BYTES};
    let st = gravity_program::Arena::new(1 << 12);
    let sites = [SiteDesc::range_source(st.handle(), rel, &[1])];
    let col = |name| OutCol::new(name, 20, -1, 8, true);
    let out = [col("both"), col("build"), col("outer")];
    let pipes = [
        (Pipeline::eligible(0, End::Breaker(Breaker::sized(BreakerKind::PlainAgg, BYTES).in_slot(0)), rows), Code::hand(par_scan_count)),
        (Pipeline::eligible(0, End::Breaker(Breaker::sized(BreakerKind::PlainAgg, BYTES).in_slot(1)), rows), Code::hand(par_scan_count)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(&[0, 1]), Code::hand(k_emit_raise)),
    ];
    let layout = Layout { nsites: 1, out_natts: 3, out_rows: 64, scratch_bytes: 0, exec_bytes: 0 };
    Program::with_pipelines(layout, &sites, &out, WritePlan::SELECT, pipes)
}

/// The build (pipeline 0) defers through `merge` (if any); the relation reads clean again for the outer.
struct Raising<'a> {
    inner: FixturePlan,
    table: &'a FixtureTable,
    merge: Option<&'a Defers>,
}

impl Plan for Raising<'_> {
    fn width_of(&self, p: usize) -> u16 {
        self.inner.width_of(p)
    }
    fn claims_for(&self, p: usize) -> &dyn MorselSource {
        self.inner.claims_for(p)
    }
    fn merge_for(&self, p: usize) -> &dyn Merge {
        match self.merge {
            Some(m) if p == 0 => m,
            _ => self.inner.merge_for(p),
        }
    }
    fn after_pipeline(&self, p: usize, exec: &mut Exec) -> Result<()> {
        if p == 0 {
            self.table.fail_block.store(u64::MAX, Ordering::Relaxed);
        }
        self.inner.after_pipeline(p, exec)
    }
}

#[test]
fn an_erroring_build_is_dropped_under_an_empty_outer_and_raised_as_its_own_error_under_a_matching_one() {
    let table = FixtureTable::new(T_A, 120, 50, gen_a);
    let mut derive = FixtureDerive::new(&[&table]);
    let program = Arc::new(p_raise(T_A, 120 * 50));
    let pool = FixturePool::new(8);
    let mut entry = Entry::new(2);
    let defers = Defers(AtomicUsize::new(0));
    let thr = 700u64;
    let want = table.rows().filter(|[a, _]| (*a as u64) < thr).count() as u64;
    assert!(want > 0);
    for w in WIDTHS {
        // (build fails, $1, merge, the statement's answer); `Defers` merges nothing (the default `merge_into`), so it
        // stands only where the build fails before any merge
        let cases: [(bool, u64, Option<&Defers>, std::result::Result<Vec<Vec<u64>>, Error>); 4] = [
            (false, thr, None, Ok(vec![vec![2 * want, want, want]])),
            (true, 0, Some(&defers), Ok(vec![vec![0, 0, 0]])),
            (true, thr, Some(&defers), Err(Error::host("range read failed"))),
            (true, 0, None, Err(Error::host("range read failed"))),
        ];
        for (fails, p1, merge, answer) in cases {
            table.fresh();
            table.fail_block.store(if fails { 17 } else { u64::MAX }, Ordering::Relaxed);
            let plan = Raising { inner: FixturePlan::new(&program, &derive, w, 3), table: &table, merge };
            let mut sink = LegSink::new(true);
            let (params, nulls) = ([p1], [0u8; 1]);
            let mut b = Binding::new(parts(&params, &nulls, &NO_ABORT), &mut sink);
            let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(Grant::of(64 << 20))).expect("started");
            assert_eq!(exec.held_error(), None, "width {w}: nothing held from the last statement");
            let r = run_program(&mut entry, &mut exec, &mut b, &mut derive, &pool, &plan);
            let held = exec.held_error();
            entry.end(exec, r.is_ok());
            let got = r.map(|n| {
                assert_eq!(n, 1, "width {w}");
                sink.sorted()
            });
            assert_eq!(got, answer, "width {w} fails {fails} $1 {p1} deferred {}", merge.is_some());
            let deferred = fails && merge.is_some();
            assert_eq!(held, deferred.then(|| Error::host("range read failed")), "width {w}: held only when deferred; a raise leaves it");
            assert_eq!(table.pins.load(Ordering::Relaxed), 0, "width {w}");
        }
    }
    table.fail_block.store(u64::MAX, Ordering::Relaxed);
    assert_eq!(entry.stats().built, 1, "one node, parked across every statement");
}

// ---- op 14, fed (V4R-89; D3230): the build's feeders after the failed one never run, so the build binds none of their slots

/// A deferring build fed by two feeders: pipeline 0 (fails), pipeline 1 (the later feeder, slot 1), pipeline 2 the build
/// (slot 2, reads both), pipeline 3 the outer (slot 3), pipeline 4 the probe's emit over `(build, outer)`. Every counter is
/// `a < $1` over one relation.
fn p_fed(rel: u32, rows: u64) -> Program {
    use gravity_kernels::par_scan_count::{par_scan_count, BYTES};
    let st = gravity_program::Arena::new(1 << 12);
    let sites = [SiteDesc::range_source(st.handle(), rel, &[1])];
    let col = |name| OutCol::new(name, 20, -1, 8, true);
    let out = [col("both"), col("build"), col("outer")];
    let agg = |slot| End::Breaker(Breaker::sized(BreakerKind::PlainAgg, BYTES).in_slot(slot));
    let pipes = [
        (Pipeline::eligible(0, agg(0), rows), Code::hand(par_scan_count)),
        (Pipeline::eligible(0, agg(1), rows), Code::hand(par_scan_count)),
        (Pipeline::eligible(0, agg(2), rows).consuming(&[0, 1]), Code::hand(par_scan_count)),
        (Pipeline::eligible(0, agg(3), rows), Code::hand(par_scan_count)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(&[2, 3]), Code::hand(k_emit_raise)),
    ];
    let layout = Layout { nsites: 1, out_natts: 3, out_rows: 64, scratch_bytes: 0, exec_bytes: 0 };
    Program::with_pipelines(layout, &sites, &out, WritePlan::SELECT, pipes)
}

/// JOIN's outer-first build as `defer_fed` finds it: the variant that defers (`defers`), taking the error (`defer_error`).
struct FedBuild(AtomicUsize);

impl Merge for FedBuild {
    fn defer_error(&self, _leader: &mut State<'_>) -> Result<bool> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(true)
    }
    fn defers(&self) -> bool {
        true
    }
}

/// The build (pipeline 2) defers through `merge` (if any); the relation reads clean again after the failed feeder.
struct Fed<'a> {
    inner: FixturePlan,
    table: &'a FixtureTable,
    merge: Option<&'a FedBuild>,
}

impl Plan for Fed<'_> {
    fn width_of(&self, p: usize) -> u16 {
        self.inner.width_of(p)
    }
    fn claims_for(&self, p: usize) -> &dyn MorselSource {
        self.inner.claims_for(p)
    }
    fn merge_for(&self, p: usize) -> &dyn Merge {
        match self.merge {
            Some(m) if p == 2 => m,
            _ => self.inner.merge_for(p),
        }
    }
    fn after_pipeline(&self, p: usize, exec: &mut Exec) -> Result<()> {
        if p == 0 {
            self.table.fail_block.store(u64::MAX, Ordering::Relaxed);
        }
        self.inner.after_pipeline(p, exec)
    }
}

/// D3230: the first feeder fails and its error is held for the build it feeds (`defer_fed`); the later feeder never ran,
/// so its slot never opened, and the build opened as a pipeline that runs bound it: 'pipeline 2 consumes slot 1 that is not
/// live' (a panic). The build opens its own region alone; the probe's first outer row raises the held error, an empty outer
/// drops it.
#[test]
fn a_build_fed_by_a_failed_feeder_opens_without_its_later_feeders_slots_and_its_probe_raises_the_held_error() {
    let table = FixtureTable::new(T_A, 120, 50, gen_a);
    let mut derive = FixtureDerive::new(&[&table]);
    let program = Arc::new(p_fed(T_A, 120 * 50));
    let pool = FixturePool::new(8);
    let mut entry = Entry::new(2);
    let fed = FedBuild(AtomicUsize::new(0));
    let thr = 700u64;
    let want = table.rows().filter(|[a, _]| (*a as u64) < thr).count() as u64;
    assert!(want > 0);
    for w in WIDTHS {
        let cases: [(bool, u64, Option<&FedBuild>, std::result::Result<Vec<Vec<u64>>, Error>); 4] = [
            (false, thr, None, Ok(vec![vec![2 * want, want, want]])),
            (true, 0, Some(&fed), Ok(vec![vec![0, 0, 0]])),
            (true, thr, Some(&fed), Err(Error::host("range read failed"))),
            (true, 0, None, Err(Error::host("range read failed"))),
        ];
        for (fails, p1, merge, answer) in cases {
            table.fresh();
            table.fail_block.store(if fails { 17 } else { u64::MAX }, Ordering::Relaxed);
            let calls = fed.0.load(Ordering::Relaxed);
            let plan = Fed { inner: FixturePlan::new(&program, &derive, w, 3), table: &table, merge };
            let mut sink = LegSink::new(true);
            let (params, nulls) = ([p1], [0u8; 1]);
            let mut b = Binding::new(parts(&params, &nulls, &NO_ABORT), &mut sink);
            let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(Grant::of(64 << 20))).expect("started");
            let r = run_program(&mut entry, &mut exec, &mut b, &mut derive, &pool, &plan);
            let held = exec.held_error();
            entry.end(exec, r.is_ok());
            let got = r.map(|n| {
                assert_eq!(n, 1, "width {w}");
                sink.sorted()
            });
            assert_eq!(got, answer, "width {w} fails {fails} $1 {p1} deferred {}", merge.is_some());
            let deferred = fails && merge.is_some();
            assert_eq!(held, deferred.then(|| Error::host("range read failed")), "width {w}: held only when deferred");
            assert_eq!(fed.0.load(Ordering::Relaxed), calls + deferred as usize, "width {w}: the build's merge took the error once");
            assert_eq!(table.pins.load(Ordering::Relaxed), 0, "width {w}");
        }
    }
    table.fail_block.store(u64::MAX, Ordering::Relaxed);
    assert_eq!(entry.stats().built, 1, "one node, parked across every statement");
}
