//! M8 RT-2e (cards/M8.md L2 RT-2; main 17:25Z): a rerun set's members run again on each call of `m8_rerun`, as PostgreSQL
//! re-executes a subplan on chgParam. `Entry::skip_rerun` steps a member past in the producer order, keeps its slot for the
//! statement and opens it zeroed for the owner (the pipeline calling `m8_rerun`); `Entry::rerun_nested` resets each member's
//! slot in place (no region or grant drawn after the first run) and runs the members nested. A reader other than the owner
//! of a set that never ran is a Bug by name, never an empty table.
use gravity_driver_rig as common;

use std::sync::atomic::{AtomicI64, AtomicU8, Ordering::Relaxed};
use std::sync::Arc;

use common::*;
use gravity_abi::breaker::{Breaker, BreakerKind};
use gravity_abi::status::DONE;
use gravity_abi::Ctx;
use gravity_driver::{Binding, Call, Entry, Error, Exec, Grant, Step, REGIONS};
use gravity_program::lifetime::{Lifetimes, RunLife, RunMember, Runner, NO_PROBE};
use gravity_program::protos::{Proto, ProtoKind, V4Descs};
use gravity_program::{Arena, Code, End, Generation, Handle, Layout, OutCol, Pipeline, PlanKey, Program, Reason, SiteDesc, WritePlan};

const KEY: PlanKey = PlanKey { handle: Handle(31), generation: Generation(2) };

/// The owner's read of the set's table (count, sum), -1 before it reads.
static OWNER_COUNT: AtomicI64 = AtomicI64::new(-1);

/// The owner: reads the set's one record (slot 0).
unsafe extern "C-unwind" fn k_owner(ctx: *mut Ctx) -> i64 {
    OWNER_COUNT.store(count_sum(consumed(ctx, 0)).count(0), Relaxed);
    DONE
}

fn member() -> Pipeline {
    Pipeline::serial(0, End::Breaker(Breaker::sized(BreakerKind::PlainAgg, COUNT_SUM_BYTES).in_slot(0)), Reason::Host)
}

/// `members` pipelines producing slot 0 (with `arms`: one append.arms run), then `readers`, each consuming slot 0; the
/// last pipeline is the rerun owner.
fn program(members: usize, readers: usize, arms: bool) -> Program {
    let st = Arena::new(1 << 14);
    let sites = [SiteDesc::range_source(st.handle(), 2001, &[1, 2])];
    let mut pipes = Vec::new(); // alloc: Cold
    for _ in 0..members {
        pipes.push((member(), Code::hand(k_range_count)));
    }
    for _ in 0..readers {
        pipes.push((Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(&[0]), Code::hand(k_owner)));
    }
    let cols: [OutCol; 1] = [OutCol::new("c0", 20, -1, 8, true)];
    let mut p = Program::with_pipelines(Layout { nsites: 1, out_natts: 1, out_rows: 4, scratch_bytes: 0, exec_bytes: 0 }, &sites, &cols, WritePlan::SELECT, pipes);
    if arms {
        p.set_v4(&V4Descs::new(vec![Proto::new(ProtoKind::Arms, 0).slot(0).arg(members as u64)])); // alloc: Cold
    }
    rerun_set(p, &(0..members as u16).collect::<Vec<_>>())
}

fn run_whole(e: &mut Entry, x: &mut Exec, b: &mut Binding<'_>, p: usize) -> gravity_driver::Result<()> {
    match e.run_pipeline(x, b, Call::batch(4))? {
        Step::Done => Ok(()),
        s => panic!("member {p} stopped with {s:?}"),
    }
}

/// The count in the set's slot now.
fn slot_count(exec: &Exec) -> i64 {
    // SAFETY: slot 0 is live (the owner reads it), `COUNT_SUM_BYTES` of it laid by the members' breaker.
    unsafe { count_sum(exec.node().slot_region(0).as_mut_ptr()).count(0) }
}

struct Run {
    entry: Entry,
    derive: FakeDerive,
    program: Arc<Program>,
}

impl Run {
    fn new(program: Program) -> Run {
        Run { entry: Entry::new(2), derive: FakeDerive::ranges(RangeTable::new(24, 10)), program: Arc::new(program) }
    }
}

const THR: [u64; 1] = [7];

/// The tests of this binary one at a time: REGIONS' counters are process-wide.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[test]
fn a_rerun_resets_the_sets_slot_in_place_and_draws_nothing_after_the_first_run() {
    let _g = serial();
    let mut r = Run::new(program(1, 1, false));
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&THR, &[0], &mut sink, &flag);
    let l = lease(Grant::of(8 << 20));
    let mut exec = r.entry.start(Some(KEY), &r.program, &mut r.derive, &mut b, l.clone()).unwrap();
    r.entry.skip_rerun(&mut exec, 1).unwrap();
    assert!(exec.node().is_rerun_unrun(0) && exec.node().slot_live(0), "the owner's slot opens zeroed before any run");
    assert_eq!(slot_count(&exec), 0, "a never-run set's slot is empty");
    r.entry.rerun_nested(&mut exec, &mut b, &[0], &mut run_whole).unwrap();
    let once = slot_count(&exec);
    assert!(once > 0, "the first run fills the set");
    assert!(!exec.node().is_rerun_unrun(0));
    let (takes, drawn) = (REGIONS.stats().takes, l.drawn());
    for i in 0..10_000 {
        r.entry.rerun_nested(&mut exec, &mut b, &[0], &mut run_whole).unwrap();
        assert_eq!(slot_count(&exec), once, "rerun {i} holds one run's rows, never the runs before it added");
    }
    assert_eq!(REGIONS.stats().takes, takes, "no region taken after the set's first run");
    assert_eq!(l.drawn(), drawn, "no grant drawn after the set's first run");
    OWNER_COUNT.store(-1, Relaxed);
    assert_eq!(r.entry.run_pipeline(&mut exec, &mut b, Call::batch(4)).unwrap(), Step::Done);
    assert_eq!(OWNER_COUNT.load(Relaxed), once, "the owner reads the last run's table");
    r.entry.end(exec, true);
}

#[test]
fn an_owner_no_row_reaches_reads_the_never_run_sets_empty_slot() {
    let _g = serial();
    let mut r = Run::new(program(1, 1, false));
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&THR, &[0], &mut sink, &flag);
    let mut exec = r.entry.start(Some(KEY), &r.program, &mut r.derive, &mut b, lease(Grant::of(8 << 20))).unwrap();
    r.entry.skip_rerun(&mut exec, 1).unwrap();
    OWNER_COUNT.store(-1, Relaxed);
    assert_eq!(r.entry.run_pipeline(&mut exec, &mut b, Call::batch(4)).unwrap(), Step::Done, "the owner opens over the set's zeroed slot");
    assert_eq!(OWNER_COUNT.load(Relaxed), 0);
    r.entry.end(exec, true);
}

/// main 17:25Z: a never-run set's slot is read before its first run ONLY by its owner; pipeline 1 reads it too.
#[test]
#[cfg_attr(debug_assertions, should_panic(expected = "of a rerun set that never ran"))]
fn another_reader_of_a_never_run_set_is_a_bug_by_name_never_an_empty_table() {
    let _g = serial();
    let mut r = Run::new(program(1, 2, false));
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&THR, &[0], &mut sink, &flag);
    let mut exec = r.entry.start(Some(KEY), &r.program, &mut r.derive, &mut b, lease(Grant::of(8 << 20))).unwrap();
    r.entry.skip_rerun(&mut exec, 2).unwrap();
    OWNER_COUNT.store(-1, Relaxed);
    match r.entry.run_pipeline(&mut exec, &mut b, Call::batch(4)) {
        Err(Error::Refused(m)) => assert!(m.contains("gravity: Bug: pipeline 1 reads slot 0 of a rerun set that never ran"), "{m}"),
        r => panic!("pipeline 1 over a never-run set: {r:?}"),
    }
    assert_eq!(OWNER_COUNT.load(Relaxed), -1, "no reader read the empty table");
    r.entry.end(exec, false);
}

/// D3914: a pass of the owner (`Pipeline::pass_of`: a spilled join's pair pipeline, a HAVING split's second pass) consumes
/// the owner's slots and runs its ops again: it opens over a set that never ran where the owner may (c-random rnd-0483 /
/// rnd-2406 refused it as another reader).
#[test]
fn a_pass_of_the_owner_opens_over_a_never_run_set_as_the_owner_does() {
    let _g = serial();
    let mut p = program(1, 2, false);
    p.pipelines[2].pass_of = 1;
    let mut r = Run::new(p);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&THR, &[0], &mut sink, &flag);
    let mut exec = r.entry.start(Some(KEY), &r.program, &mut r.derive, &mut b, lease(Grant::of(8 << 20))).unwrap();
    r.entry.skip_rerun(&mut exec, 1).unwrap();
    for who in ["the owner", "its pass"] {
        OWNER_COUNT.store(-1, Relaxed);
        assert_eq!(r.entry.run_pipeline(&mut exec, &mut b, Call::batch(4)).unwrap(), Step::Done, "{who} opens over the set's zeroed slot");
        assert_eq!(OWNER_COUNT.load(Relaxed), 0, "{who} reads the never-run set's empty table");
    }
    r.entry.end(exec, true);
}

/// CAND-M03: a member one run masked empty (D276 `run_empty`, as a member's empty hash build masks its probe side for that
/// run: PG never fetches the outer of an empty build) runs with its claims at the next re-run. Before the fix the mask
/// stayed for the statement, so every later run's probe side ran empty (counts 0 after a NULL parameter's run).
#[test]
fn a_member_one_run_masked_empty_runs_with_its_claims_at_the_next_run() {
    let _g = serial();
    let mut r = Run::new(program(1, 1, false));
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&THR, &[0], &mut sink, &flag);
    let mut exec = r.entry.start(Some(KEY), &r.program, &mut r.derive, &mut b, lease(Grant::of(8 << 20))).unwrap();
    r.entry.skip_rerun(&mut exec, 1).unwrap();
    r.entry.rerun_nested(&mut exec, &mut b, &[0], &mut run_whole).unwrap();
    let once = slot_count(&exec);
    assert!(once > 0, "the first run fills the set");
    // an earlier member's empty build masks member 0 before it runs (BuildSideLive::after_pipeline)
    r.entry.rerun_nested(&mut exec, &mut b, &[0], &mut |e, x, bd, p| {
        x.run_empty(1 << p);
        run_whole(e, x, bd, p)
    }).unwrap();
    assert!(exec.runs_empty(0), "the mask holds for the rest of the run that set it");
    r.entry.rerun_nested(&mut exec, &mut b, &[0], &mut run_whole).unwrap();
    assert!(!exec.runs_empty(0), "the next run re-arms the member: its mask was the earlier run's");
    assert_eq!(slot_count(&exec), once, "the next run holds its own rows");
    r.entry.end(exec, true);
}

/// `run_nested` alone would run an already-open member again with no reopen, folding the second run into the first's rows.
#[test]
fn a_second_run_without_the_reset_is_a_bug_by_name_never_folded_rows() {
    let _g = serial();
    let mut r = Run::new(program(1, 1, false));
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&THR, &[0], &mut sink, &flag);
    let mut exec = r.entry.start(Some(KEY), &r.program, &mut r.derive, &mut b, lease(Grant::of(8 << 20))).unwrap();
    r.entry.skip_rerun(&mut exec, 1).unwrap();
    r.entry.run_nested(&mut exec, &mut b, &[0], &mut run_whole).unwrap();
    let once = slot_count(&exec);
    match r.entry.run_nested(&mut exec, &mut b, &[0], &mut run_whole) {
        Err(Error::Refused(m)) => assert!(m.contains("gravity: Bug: rerun set member 0 run again without its reset"), "{m}"),
        r => panic!("a second run with no reset: {r:?}"),
    }
    assert_eq!(slot_count(&exec), once, "the refused run folded nothing");
    r.entry.end(exec, false);
}

/// A run again of a member the program's lifetimes do not declare (here: no lifetimes at all) is a Bug by name before
/// anything is reset or run: the runtime never works out a reset for itself.
#[test]
fn an_undeclared_member_is_refused_before_anything_is_reset_or_run() {
    let _g = serial();
    let mut p = program(1, 1, false);
    p.v4 = None;
    let mut r = Run::new(p);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&THR, &[0], &mut sink, &flag);
    let mut exec = r.entry.start(Some(KEY), &r.program, &mut r.derive, &mut b, lease(Grant::of(8 << 20))).unwrap();
    r.entry.skip_rerun(&mut exec, 1).unwrap();
    match r.entry.rerun_nested(&mut exec, &mut b, &[0], &mut run_whole) {
        Err(Error::Refused(m)) => assert!(m.contains("gravity: Bug: a run again of pipelines [0] the program's lifetimes do not declare"), "{m}"),
        r => panic!("an undeclared member: {r:?}"),
    }
    assert!(exec.node().is_rerun_unrun(0), "nothing ran");
    assert_eq!(slot_count(&exec), 0, "nothing filled the slot");
    r.entry.end(exec, false);
}

/// A member PG keeps (`RunLife::FirstRun`, D1018 item 9 (b)) is reset only before a run produced it: the first run adopts
/// its slot; a later run naming it is refused by name before the member runs, its kept rows untouched.
#[test]
fn a_kept_member_is_never_reset_after_its_fill() {
    let _g = serial();
    let mut p = program(1, 1, false);
    let kept = RunMember { pipeline: 0, runner: Runner::Set(0), life: RunLife::FirstRun, lazy_probe: NO_PROBE };
    p.set_v4(&V4Descs { lifetimes: Lifetimes::new(vec![kept]).unwrap(), ..V4Descs::new(Vec::new()) }); // alloc: Cold
    let mut r = Run::new(p);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&THR, &[0], &mut sink, &flag);
    let mut exec = r.entry.start(Some(KEY), &r.program, &mut r.derive, &mut b, lease(Grant::of(8 << 20))).unwrap();
    r.entry.skip_rerun(&mut exec, 1).unwrap();
    r.entry.rerun_nested(&mut exec, &mut b, &[0], &mut run_whole).unwrap();
    let once = slot_count(&exec);
    assert!(once > 0, "the first run fills the kept member");
    match r.entry.rerun_nested(&mut exec, &mut b, &[0], &mut run_whole) {
        Err(Error::Refused(m)) => assert!(m.contains("gravity: Bug: rerun set member 0 run again without its reset"), "{m}"),
        r => panic!("a kept member run again after its fill: {r:?}"),
    }
    assert_eq!(slot_count(&exec), once, "the kept rows are untouched");
    r.entry.end(exec, false);
}

/// D4120: a run that refills its kept members (an enclosing owner changed a param their fill reads, PG's ExecReScanAgg
/// rebuilding a kept table) resets the kept slot in place as every member's: the fill runs again into an empty slot,
/// never folded into the first fill's rows. A plain run after it still keeps the member.
#[test]
fn a_refill_resets_a_kept_member_in_place_and_fills_it_again() {
    let _g = serial();
    let mut p = program(1, 1, false);
    let kept = RunMember { pipeline: 0, runner: Runner::Set(0), life: RunLife::FirstRun, lazy_probe: NO_PROBE };
    p.set_v4(&V4Descs { lifetimes: Lifetimes::new(vec![kept]).unwrap(), ..V4Descs::new(Vec::new()) }); // alloc: Cold
    let mut r = Run::new(p);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&THR, &[0], &mut sink, &flag);
    let mut exec = r.entry.start(Some(KEY), &r.program, &mut r.derive, &mut b, lease(Grant::of(8 << 20))).unwrap();
    r.entry.skip_rerun(&mut exec, 1).unwrap();
    r.entry.rerun_nested(&mut exec, &mut b, &[0], &mut run_whole).unwrap();
    let once = slot_count(&exec);
    assert!(once > 0, "the first run fills the kept member");
    for _ in 0..2 {
        r.entry.refill_nested(&mut exec, &mut b, &[0], &mut run_whole).unwrap();
        assert_eq!(slot_count(&exec), once, "a refill fills the slot again from empty");
    }
    match r.entry.rerun_nested(&mut exec, &mut b, &[0], &mut run_whole) {
        Err(Error::Refused(m)) => assert!(m.contains("gravity: Bug: rerun set member 0 run again without its reset"), "{m}"),
        r => panic!("a kept member run again after its refill without one: {r:?}"),
    }
    assert_eq!(slot_count(&exec), once, "the kept rows are untouched");
    r.entry.end(exec, false);
}

#[test]
fn a_cancel_inside_a_rerun_fails_it_and_the_statement_ends_clean() {
    let _g = serial();
    let mut r = Run::new(program(1, 1, false));
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&THR, &[0], &mut sink, &flag);
    let mut exec = r.entry.start(Some(KEY), &r.program, &mut r.derive, &mut b, lease(Grant::of(8 << 20))).unwrap();
    r.entry.skip_rerun(&mut exec, 1).unwrap();
    r.entry.rerun_nested(&mut exec, &mut b, &[0], &mut run_whole).unwrap();
    flag.store(1, Relaxed);
    let e = r.entry.rerun_nested(&mut exec, &mut b, &[0], &mut |e, x, bd, p| match e.run_pipeline(x, bd, Call::batch(4)) {
        Ok(Step::Done) => Ok(()),
        Ok(s) => panic!("member {p} stopped with {s:?}"),
        Err(err) => Err(err),
    });
    assert!(matches!(e, Err(Error::Cancelled)), "{e:?}");
    r.entry.end(exec, false);
}

/// An Append-armed member range (append.arms, one production across the arms): a rerun resets the slot once, at its first
/// arm; the second arm folds on into it (RT-1b's arms_of); each rerun holds both arms' rows of that run only.
#[test]
fn a_rerun_over_an_append_armed_member_range_holds_each_runs_arms_once() {
    let _g = serial();
    let mut r = Run::new(program(2, 1, true));
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&THR, &[0], &mut sink, &flag);
    let mut exec = r.entry.start(Some(KEY), &r.program, &mut r.derive, &mut b, lease(Grant::of(8 << 20))).unwrap();
    r.entry.skip_rerun(&mut exec, 2).unwrap();
    r.entry.skip_rerun(&mut exec, 2).unwrap();
    r.entry.rerun_nested(&mut exec, &mut b, &[0, 1], &mut run_whole).unwrap();
    let both = slot_count(&exec);
    assert!(both > 0 && both % 2 == 0, "two arms over the same rows: {both}");
    for _ in 0..100 {
        r.entry.rerun_nested(&mut exec, &mut b, &[0, 1], &mut run_whole).unwrap();
        assert_eq!(slot_count(&exec), both);
    }
    OWNER_COUNT.store(-1, Relaxed);
    assert_eq!(r.entry.run_pipeline(&mut exec, &mut b, Call::batch(4)).unwrap(), Step::Done);
    assert_eq!(OWNER_COUNT.load(Relaxed), both);
    r.entry.end(exec, true);
}

/// A member producing `slot` over site 0's range (its count and sum).
fn member_in(slot: u8) -> Pipeline {
    Pipeline::serial(0, End::Breaker(Breaker::sized(BreakerKind::PlainAgg, COUNT_SUM_BYTES).in_slot(slot)), Reason::Host)
}

/// The count in slot `slot` now.
fn count_in(exec: &Exec, slot: u8) -> i64 {
    // SAFETY: `slot` is live (a member filled it), `COUNT_SUM_BYTES` of it laid by the member's breaker.
    unsafe { count_sum(exec.node().slot_region(slot).as_mut_ptr()).count(0) }
}

/// D3374 (rerun_nested_recursive N13): pipeline 1, a member of the outer set (members 1 and 2, owner 3), runs an inner
/// set (member 0) nested inside its own run, as a merge's inner Sort pulls a re-armed recursive CTE whose term set runs
/// per generation, before the outer set's later member 2 opens. The inner run's end drops only its own members' resets,
/// so member 2 adopts the slot its set's run reset, at every run: never 'produced again while a consumer of its live
/// life is pending' (L3).
#[test]
fn a_set_run_nested_inside_a_member_leaves_the_outer_sets_resets() {
    let _g = serial();
    let st = Arena::new(1 << 14);
    let sites = [SiteDesc::range_source(st.handle(), 2001, &[1, 2])];
    let cols: [OutCol; 1] = [OutCol::new("c0", 20, -1, 8, true)];
    let pipes = vec![
        (member_in(0), Code::hand(k_range_count)),
        (member_in(1).consuming(&[0]), Code::hand(k_range_count)),
        (member_in(2), Code::hand(k_range_count)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(&[1, 2]), Code::hand(k_owner)),
    ];
    let mut p = Program::with_pipelines(Layout { nsites: 1, out_natts: 1, out_rows: 4, scratch_bytes: 0, exec_bytes: 0 }, &sites, &cols, WritePlan::SELECT, pipes);
    let m = |pipeline: u16, set: u16| RunMember { pipeline, runner: Runner::Set(set), life: RunLife::EveryRun, lazy_probe: NO_PROBE };
    p.set_v4(&V4Descs { lifetimes: Lifetimes::new(vec![m(0, 1), m(1, 0), m(2, 0)]).unwrap(), ..V4Descs::new(Vec::new()) }); // alloc: Cold
    let mut r = Run::new(p);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&THR, &[0], &mut sink, &flag);
    let mut exec = r.entry.start(Some(KEY), &r.program, &mut r.derive, &mut b, lease(Grant::of(8 << 20))).unwrap();
    r.entry.skip_rerun(&mut exec, 1).unwrap();
    r.entry.skip_rerun(&mut exec, 3).unwrap();
    r.entry.skip_rerun(&mut exec, 3).unwrap();
    let mut outer = |e: &mut Entry, x: &mut Exec, b: &mut Binding<'_>, q: usize| {
        run_whole(e, x, b, q)?;
        if q == 1 {
            e.rerun_nested(x, b, &[0], &mut run_whole)?;
        }
        Ok(())
    };
    r.entry.rerun_nested(&mut exec, &mut b, &[1, 2], &mut outer).unwrap();
    let once = count_in(&exec, 2);
    assert!(once > 0, "the first run fills member 2");
    for i in 0..3 {
        if let Err(e) = r.entry.rerun_nested(&mut exec, &mut b, &[1, 2], &mut outer) {
            panic!("outer run {i}: {e:?}");
        }
        assert_eq!((count_in(&exec, 1), count_in(&exec, 2)), (once, once), "run {i} holds one run's rows in each member's slot");
    }
    OWNER_COUNT.store(-1, Relaxed);
    assert_eq!(r.entry.run_pipeline(&mut exec, &mut b, Call::batch(4)).unwrap(), Step::Done);
    assert_eq!(OWNER_COUNT.load(Relaxed), once, "the owner reads the last run's table");
    r.entry.end(exec, true);
}
