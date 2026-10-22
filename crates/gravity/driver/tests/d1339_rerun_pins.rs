//! D1339 row C (W4b-RowC-D1339-PREP, DEV; design r2 review C3): a rerun set's member pins memory inside its owner's call
//! (an owner region past the leader's spare, a key image: the records the owner's inner reads after the run). The set runs
//! nested under `hold(owner)`, and `reattach` put the owner's (chunk_next, pinned) back, so the member's pinned chunks fell
//! into the owner's free tail and the owner's next arena grow overwrote them (row C's c1: zeroed or garbage groups, a
//! SIGSEGV). Now `reattach` at the depth `Entry::rerun_nested` holds keeps the run's pins (moved into the pinned prefix,
//! ahead of the held call's windows, filed under the set), and the set's next run unpins them: one run's pins held, never
//! one per re-open.
//! Pipelines: 0 the member (pins two chunks; slot 0), 1 the owner (site-less, reads slot 0; its call grows its arena and
//! stops resumably, so it is held across each run). The owner's calls after its first are RESUMED calls (`Call::resume`):
//! a resumed call keeps its window's cursor (no rewind to `pinned`), as the owner's call goes on after a nested run in the
//! server, so its next grow takes the first chunk at the held call's cursor: on the base that is the run's first pin (c4
//! stage 8: a fresh batch call rewound to `pinned` and reused its own windows first, so the base passed).
use gravity_driver_rig as common;

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

use common::*;
use gravity_abi::breaker::{Breaker, BreakerKind};
use gravity_abi::ctx::{resume, RESUME_NONE};
use gravity_abi::status::{suspend, DONE};
use gravity_abi::Ctx;
use gravity_driver::{Binding, Call, Entry, Exec, Grant, Step};
use gravity_program::{Arena, Code, End, Generation, Handle, Layout, OutCol, Pipeline, PlanKey, Program, Reason, SiteDesc, WritePlan};

const KEY: PlanKey = PlanKey { handle: Handle(43), generation: Generation(1) };
const THR: [u64; 1] = [7];
/// A member pin (under one chunk: the pool's first free chunk takes it) and the owner's grow.
const PIN_BYTES: usize = 40 << 10;
const GROW_BYTES: u64 = 64 << 10;
const RUNS: u64 = 40;

/// The run the member stamps into its pins; this run's pins; the owner's calls; its reads that found a pin overwritten.
static RUN: AtomicU64 = AtomicU64::new(0);
static PINS: Mutex<Vec<usize>> = Mutex::new(Vec::new());
/// Every chunk a pin ever took: the set's next run unpins the last run's, so the member's pins reuse them.
static EVER: Mutex<Vec<usize>> = Mutex::new(Vec::new());
static OWNER_CALLS: AtomicU64 = AtomicU64::new(0);
static BAD: AtomicU64 = AtomicU64::new(0);

fn stamp(run: u64) -> u8 {
    (run % 250) as u8 + 3
}

/// The member: its count into slot 0, then two pinned chunks stamped with the run (a group's records and key images).
unsafe extern "C-unwind" fn k_member(ctx: *mut Ctx) -> i64 {
    let r = k_range_count(ctx);
    if r != DONE {
        return r;
    }
    let pin = (*(*ctx).program).pin.expect("the pin host");
    for _ in 0..2 {
        let p = pin(ctx, PIN_BYTES as u64);
        assert!(!p.is_null(), "a pin under the grant");
        core::ptr::write_bytes(p, stamp(RUN.load(Relaxed)), PIN_BYTES);
        PINS.lock().unwrap().push(p as usize);
        let mut e = EVER.lock().unwrap();
        if !e.contains(&(p as usize)) {
            e.push(p as usize);
        }
    }
    DONE
}

/// The owner: grows its arena three times and overwrites what it got (its windows; a grow takes the first free chunk at
/// or past the call's cursor), then reads the last run's pins; stops resumably until the test ends it.
unsafe extern "C-unwind" fn k_owner(ctx: *mut Ctx) -> i64 {
    OWNER_CALLS.fetch_add(1, Relaxed);
    let grow = (*(*ctx).program).arena_grow.expect("the grow host");
    for _ in 0..3 {
        let n = grow(ctx, GROW_BYTES);
        assert!(n > 0, "an arena grow under the grant");
        core::ptr::write_bytes((*ctx).arena_cur, 0xEE, n as usize);
    }
    let want = stamp(RUN.load(Relaxed));
    for &p in PINS.lock().unwrap().iter() {
        let s = core::slice::from_raw_parts(p as *const u8, PIN_BYTES);
        if s.iter().any(|&b| b != want) {
            BAD.fetch_add(1, Relaxed);
        }
    }
    if RUN.load(Relaxed) < RUNS {
        (*ctx).resume = resume::word(resume::EMIT, 0);
        return suspend(false);
    }
    (*ctx).resume = RESUME_NONE;
    DONE
}

fn program() -> Program {
    let st = Arena::new(1 << 14);
    let sites = [SiteDesc::range_source(st.handle(), 2001, &[1, 2])];
    let m = Pipeline::serial(0, End::Breaker(Breaker::sized(BreakerKind::PlainAgg, COUNT_SUM_BYTES).in_slot(0)), Reason::Host);
    let owner = Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(&[0]);
    let cols: [OutCol; 1] = [OutCol::new("c0", 20, -1, 8, true)];
    rerun_set(Program::with_pipelines(Layout { nsites: 1, out_natts: 1, out_rows: 4, scratch_bytes: 0, exec_bytes: 0 }, &sites, &cols, WritePlan::SELECT, [(m, Code::hand(k_member)), (owner, Code::hand(k_owner))]), &[0])
}

fn run_whole(e: &mut Entry, x: &mut Exec, b: &mut Binding<'_>, p: usize) -> gravity_driver::Result<()> {
    match e.run_pipeline(x, b, Call::batch(4))? {
        Step::Done => Ok(()),
        s => panic!("member {p} stopped with {s:?}"),
    }
}

#[test]
fn a_rerun_members_pins_outlive_the_owners_reattach_until_the_sets_next_run() {
    let program = Arc::new(program());
    let mut derive = FakeDerive::ranges(RangeTable::new(24, 10));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&THR, &[0], &mut sink, &flag);
    let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(Grant::of(64 << 20))).unwrap();
    entry.skip_rerun(&mut exec, 1).unwrap();
    // the owner's first call: its windows laid, stopped resumably (opened: each run below holds it)
    assert!(matches!(entry.run_pipeline(&mut exec, &mut b, Call::batch(4)).unwrap(), Step::Suspended { .. }), "the owner stops resumably");
    for run in 1..=RUNS {
        RUN.store(run, Relaxed);
        PINS.lock().unwrap().clear();
        entry.rerun_nested(&mut exec, &mut b, &[0], &mut run_whole).unwrap();
        assert_eq!(PINS.lock().unwrap().len(), 2, "run {run}: the member pinned its two chunks");
        let s = entry.run_pipeline(&mut exec, &mut b, Call::resume(4)).unwrap();
        assert_eq!(BAD.load(Relaxed), 0, "run {run}: the owner's grows after the reattach overwrote none of the run's pins");
        match run {
            RUNS => assert_eq!(s, Step::Done, "the owner ends after the last run"),
            _ => assert!(matches!(s, Step::Suspended { .. }), "run {run}: the owner stops resumably again"),
        }
    }
    assert_eq!(OWNER_CALLS.load(Relaxed), RUNS + 1);
    assert!(EVER.lock().unwrap().len() <= 4, "the set's next run unpins the last run's chunks (one run's pins held): {} chunks pinned over {RUNS} runs", EVER.lock().unwrap().len());
    entry.end(exec, true);
}
