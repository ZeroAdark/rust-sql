//! M8 RT-2f (main 18:18Z, conditions 1-4): a pulled member set (PG's CteScanNext over a CTE's plan) runs on demand in pulls:
//! `Entry::pull_nested` runs the set's producers once, then calls the last member (the CTE's plan) until it stops
//! resumably after its budget, parks that call between pulls (outside the readers' holds, its sites out of every L8 sweep)
//! and resumes it at the next pull; its end is sticky; an error is the pull's that reaches it and every later pull's by
//! name; a cancel drops the parked call. The store edge is a kept slot opened zeroed for every reader (`skip_pulled`).
use gravity_driver_rig as common;

use std::sync::atomic::{AtomicI64, AtomicU64, AtomicU8, Ordering::Relaxed};
use std::sync::Arc;

use common::*;
use gravity_abi::breaker::{Breaker, BreakerKind};
use gravity_abi::ctx::{cancel_pending, resume, site_ptr, RESUME_NONE};
use gravity_abi::site::{next_row, seek, site};
use gravity_abi::status::{suspend, CANCELLED, DONE, ERROR};
use gravity_abi::Ctx;
use gravity_driver::{Binding, Call, Entry, Error, Exec, Grant, Pulled, Step, DEPOT, POOL, REGIONS};
use gravity_program::{Arena, Code, End, Generation, Handle, Layout, OutCol, Pipeline, PlanKey, Program, Reason, SiteDesc, WritePlan};

const KEY: PlanKey = PlanKey { handle: Handle(32), generation: Generation(6) };
const BLOCKS: u64 = 4;
const PER_BLOCK: u64 = 10;
const ROWS: u64 = BLOCKS * PER_BLOCK;

static BUDGET: AtomicU64 = AtomicU64::new(1);
/// The row value the pulled pipeline raises at (`u64::MAX`: none).
static ERR_AT: AtomicU64 = AtomicU64::new(u64::MAX);
/// Rows the pulled pipeline appended to its store, its calls, the producer's calls and the count it saw.
static LANDED: AtomicU64 = AtomicU64::new(0);
static PULL_CALLS: AtomicU64 = AtomicU64::new(0);
static PROD_CALLS: AtomicU64 = AtomicU64::new(0);
static PROD_SEEN: AtomicI64 = AtomicI64::new(-1);
/// A reader's first call stops (an emit's suspend) before it reads: 1.
static READER_STOP: AtomicU8 = AtomicU8::new(0);
/// The store's count a reader read, -1 before.
static READ: AtomicI64 = AtomicI64::new(-1);

fn reset() {
    for a in [&BUDGET, &LANDED, &PULL_CALLS, &PROD_CALLS] {
        a.store(0, Relaxed);
    }
    BUDGET.store(1, Relaxed);
    ERR_AT.store(u64::MAX, Relaxed);
    PROD_SEEN.store(-1, Relaxed);
    READER_STOP.store(0, Relaxed);
    READ.store(-1, Relaxed);
}

/// The set's producer (pipeline 0): counts site 0's rows into slot 1.
unsafe extern "C-unwind" fn k_prod(ctx: *mut Ctx) -> i64 {
    PROD_CALLS.fetch_add(1, Relaxed);
    k_range_count(ctx)
}

/// The pulled pipeline (1, the CTE's plan): appends site 1's rows to its store (slot 0) until its budget, then stops
/// resumably; resumed, it reads on past the last row it appended.
unsafe extern "C-unwind" fn k_pull(ctx: *mut Ctx) -> i64 {
    PULL_CALLS.fetch_add(1, Relaxed);
    let s = &mut *site_ptr(ctx, 1);
    if resume::phase((*ctx).resume) != resume::EMIT {
        (*s.keys, *s.keys.add(1), *s.keys_null, *s.keys_null.add(1)) = ((*ctx).claim.lo, (*ctx).claim.hi, 0, 0);
        seek(s);
        PROD_SEEN.store(count_sum(consumed(ctx, 0)).count(0), Relaxed);
    }
    let store = count_sum(own(ctx));
    let (budget, mut n) = (BUDGET.load(Relaxed), 0u64);
    loop {
        if cancel_pending(ctx) {
            return CANCELLED;
        }
        let r = next_row(s, 2);
        if r < 0 {
            return r;
        }
        if r == 0 {
            (s.state, (*ctx).resume) = (site::STATE_EOF, RESUME_NONE);
            return DONE;
        }
        let v = *s.values;
        if v == ERR_AT.load(Relaxed) {
            return ERROR;
        }
        store.add_count(0, 1);
        store.add_i64(1, v as i64);
        LANDED.fetch_add(1, Relaxed);
        n += 1;
        if n >= budget {
            (*ctx).resume = resume::word(resume::EMIT, 0);
            return suspend(false);
        }
    }
}

/// A reader (2 and 3, CteScans of the same CTE): reads the store's count; with `READER_STOP` its first call stops first.
unsafe extern "C-unwind" fn k_read(ctx: *mut Ctx) -> i64 {
    if READER_STOP.load(Relaxed) == 1 && resume::phase((*ctx).resume) != resume::EMIT {
        (*ctx).resume = resume::word(resume::EMIT, 0);
        return suspend(false);
    }
    READ.store(count_sum(consumed(ctx, 0)).count(0), Relaxed);
    (*ctx).resume = RESUME_NONE;
    DONE
}

fn program() -> Program {
    let st = Arena::new(1 << 14);
    let sites = [SiteDesc::range_source(st.handle(), 2001, &[1, 2]), SiteDesc::range_source(st.handle(), 2002, &[1, 2])];
    let agg = |slot| End::Breaker(Breaker::sized(BreakerKind::PlainAgg, COUNT_SUM_BYTES).in_slot(slot));
    let pipes = [
        (Pipeline::serial(0, agg(1), Reason::Host), Code::hand(k_prod)),
        (Pipeline::serial(1, agg(0), Reason::Host).consuming(&[1]), Code::hand(k_pull)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(&[0]), Code::hand(k_read)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(&[0]), Code::hand(k_read)),
    ];
    let cols: [OutCol; 1] = [OutCol::new("c0", 20, -1, 8, true)];
    Program::with_pipelines(Layout { nsites: 2, out_natts: 1, out_rows: 4, scratch_bytes: 0, exec_bytes: 0 }, &sites, &cols, WritePlan::SELECT, pipes)
}

fn run_whole(e: &mut Entry, x: &mut Exec, b: &mut Binding<'_>, p: usize) -> gravity_driver::Result<()> {
    match e.run_pipeline(x, b, Call::batch(4))? {
        Step::Done => Ok(()),
        s => panic!("producer {p} stopped with {s:?}"),
    }
}

/// The tests of this binary one at a time: the kernels' counters and the pool's are process-wide.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reset();
    g
}

const THR: [u64; 1] = [1000];
const SET: [u16; 2] = [0, 1];

/// The statement over the program: the set's producer and CTE plan stepped past (deferred), both readers its owners.
fn begin(entry: &mut Entry, derive: &mut FakeDerive, program: &Arc<Program>, b: &mut Binding<'_>) -> Exec {
    let mut exec = entry.start(Some(KEY), program, derive, b, lease(Grant::of(8 << 20))).unwrap();
    entry.skip_deferred(&mut exec);
    entry.skip_pulled(&mut exec, &[2, 3]).unwrap();
    assert_eq!(exec.next_pipeline(), 2);
    exec
}

#[test]
fn budgets_1_7_and_10k_pull_one_whole_run_the_producer_once_and_the_end_is_sticky() {
    let _g = serial();
    for budget in [1u64, 7, 10_000] {
        reset();
        BUDGET.store(budget, Relaxed);
        let program = Arc::new(program());
        let mut derive = FakeDerive::ranges(RangeTable::new(BLOCKS, PER_BLOCK));
        let mut entry = Entry::new(2);
        let flag = AtomicU8::new(0);
        let mut sink = LegSink::new();
        let mut b = binding(&THR, &[0], &mut sink, &flag);
        let mut exec = begin(&mut entry, &mut derive, &program, &mut b);
        let mut pulls = 0u64;
        loop {
            pulls += 1;
            match entry.pull_nested(&mut exec, &mut b, &SET, &mut run_whole).unwrap() {
                Pulled::Stopped => assert_eq!(exec.node().parked_pulls(), 1, "a stop parks the call"),
                Pulled::Done => break,
            }
            assert_eq!(exec.next_pipeline(), 2, "the reader's place back after each pull");
        }
        assert_eq!(pulls, ROWS / budget + 1, "budget {budget}: one stop per budget, then the end");
        assert_eq!(LANDED.load(Relaxed), ROWS, "budget {budget}: one whole run's rows");
        assert_eq!(PROD_CALLS.load(Relaxed), 1, "budget {budget}: the producer ran once");
        assert_eq!(PROD_SEEN.load(Relaxed), ROWS as i64);
        assert_eq!(exec.node().parked_pulls(), 0);
        let calls = PULL_CALLS.load(Relaxed);
        assert_eq!(entry.pull_nested(&mut exec, &mut b, &SET, &mut run_whole).unwrap(), Pulled::Done, "Done is sticky");
        assert_eq!(PULL_CALLS.load(Relaxed), calls, "a pull after the end calls nothing");
        for reader in [2, 3] {
            READ.store(-1, Relaxed);
            assert_eq!(entry.run_pipeline(&mut exec, &mut b, Call::batch(4)).unwrap(), Step::Done, "reader {reader}");
            assert_eq!(READ.load(Relaxed), ROWS as i64, "reader {reader} reads the whole store");
        }
        entry.end(exec, true);
    }
}

/// A reader's own call held around the pulls (nested holds), and a reader opening while the CTE's call is parked: the
/// parked call's open site is out of the open's sweep (L8), and every reader of the set may open over its store.
#[test]
fn a_parked_pull_survives_a_readers_hold_and_a_second_readers_open() {
    let _g = serial();
    let program = Arc::new(program());
    let mut derive = FakeDerive::ranges(RangeTable::new(BLOCKS, PER_BLOCK));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&THR, &[0], &mut sink, &flag);
    let mut exec = begin(&mut entry, &mut derive, &program, &mut b);
    assert_eq!(entry.pull_nested(&mut exec, &mut b, &SET, &mut run_whole).unwrap(), Pulled::Stopped);
    READER_STOP.store(1, Relaxed);
    assert!(matches!(entry.run_pipeline(&mut exec, &mut b, Call::batch(4)).unwrap(), Step::Suspended { .. }), "reader 2 opens (site-less: its sweep skips the parked site) and stops");
    assert_eq!(entry.pull_nested(&mut exec, &mut b, &SET, &mut run_whole).unwrap(), Pulled::Stopped, "a pull inside reader 2's call");
    assert_eq!(entry.run_pipeline(&mut exec, &mut b, Call::resume(4)).unwrap(), Step::Done, "reader 2 resumes after the pull");
    assert_eq!(READ.load(Relaxed), 2, "reader 2 reads the two rows pulled so far");
    READER_STOP.store(0, Relaxed);
    while entry.pull_nested(&mut exec, &mut b, &SET, &mut run_whole).unwrap() == Pulled::Stopped {}
    assert_eq!(entry.run_pipeline(&mut exec, &mut b, Call::batch(4)).unwrap(), Step::Done, "reader 3 opens over the store");
    assert_eq!(READ.load(Relaxed), ROWS as i64);
    assert_eq!(LANDED.load(Relaxed), ROWS);
    entry.end(exec, true);
}

/// D2178 / D2179: the pulls inside reader 2's call re-park the CTE's open site (a hold that took it left it out of the
/// re-park), so reader 3's open, site-less, sweeps past it: before, debug raised L8 there and release closed the site, and
/// the next pull re-read the table from its first block.
#[test]
fn pulls_inside_a_readers_hold_keep_the_site_parked_over_a_later_readers_open() {
    let _g = serial();
    let program = Arc::new(program());
    let mut derive = FakeDerive::ranges(RangeTable::new(BLOCKS, PER_BLOCK));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&THR, &[0], &mut sink, &flag);
    let mut exec = begin(&mut entry, &mut derive, &program, &mut b);
    assert_eq!(entry.pull_nested(&mut exec, &mut b, &SET, &mut run_whole).unwrap(), Pulled::Stopped);
    READER_STOP.store(1, Relaxed);
    assert!(matches!(entry.run_pipeline(&mut exec, &mut b, Call::batch(4)).unwrap(), Step::Suspended { .. }), "reader 2 opens and stops");
    for i in 0..2 {
        assert_eq!(entry.pull_nested(&mut exec, &mut b, &SET, &mut run_whole).unwrap(), Pulled::Stopped, "pull {i} inside reader 2's call");
    }
    assert_eq!(entry.run_pipeline(&mut exec, &mut b, Call::resume(4)).unwrap(), Step::Done, "reader 2 resumes after the pulls");
    assert_eq!(READ.load(Relaxed), 3);
    assert!(matches!(entry.run_pipeline(&mut exec, &mut b, Call::batch(4)).unwrap(), Step::Suspended { .. }), "reader 3 opens (its sweep skips the parked site) and stops");
    while entry.pull_nested(&mut exec, &mut b, &SET, &mut run_whole).unwrap() == Pulled::Stopped {}
    assert_eq!(entry.run_pipeline(&mut exec, &mut b, Call::resume(4)).unwrap(), Step::Done, "reader 3 resumes over the whole store");
    assert_eq!(READ.load(Relaxed), ROWS as i64);
    assert_eq!(LANDED.load(Relaxed), ROWS, "each row landed once: no re-read from the first block");
    entry.end(exec, true);
}

/// D2179, the statement-end drain (gravity_parallel's `drain_pulled`: pulls with no reader open): after the pulls inside
/// reader 2's call, reader 3 opens and reads the store as it stands; the drain then lands the rest of the rows once.
#[test]
fn the_drain_after_a_later_readers_open_lands_each_row_once() {
    let _g = serial();
    let program = Arc::new(program());
    let mut derive = FakeDerive::ranges(RangeTable::new(BLOCKS, PER_BLOCK));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&THR, &[0], &mut sink, &flag);
    let mut exec = begin(&mut entry, &mut derive, &program, &mut b);
    assert_eq!(entry.pull_nested(&mut exec, &mut b, &SET, &mut run_whole).unwrap(), Pulled::Stopped);
    READER_STOP.store(1, Relaxed);
    assert!(matches!(entry.run_pipeline(&mut exec, &mut b, Call::batch(4)).unwrap(), Step::Suspended { .. }), "reader 2 opens and stops");
    for i in 0..2 {
        assert_eq!(entry.pull_nested(&mut exec, &mut b, &SET, &mut run_whole).unwrap(), Pulled::Stopped, "pull {i} inside reader 2's call");
    }
    assert_eq!(entry.run_pipeline(&mut exec, &mut b, Call::resume(4)).unwrap(), Step::Done, "reader 2 resumes after the pulls");
    READER_STOP.store(0, Relaxed);
    assert_eq!(entry.run_pipeline(&mut exec, &mut b, Call::batch(4)).unwrap(), Step::Done, "reader 3 opens (its sweep skips the parked site) and reads");
    assert_eq!(READ.load(Relaxed), 3, "reader 3 reads the store as it stands");
    assert_eq!(exec.node().parked_pulls(), 1);
    while entry.pull_nested(&mut exec, &mut b, &SET, &mut run_whole).unwrap() == Pulled::Stopped {}
    assert_eq!(exec.node().parked_pulls(), 0);
    assert_eq!(LANDED.load(Relaxed), ROWS, "each row landed once: no re-read from the first block");
    entry.end(exec, true);
}

/// main 18:18Z condition 3: an error row past budget 1 raises at the pull that reaches it, never earlier; every later
/// pull of the set is an error by name.
#[test]
fn an_error_past_the_first_budget_raises_at_the_pull_that_reaches_it_and_every_later_one() {
    let _g = serial();
    ERR_AT.store(3, Relaxed);
    let program = Arc::new(program());
    let mut derive = FakeDerive::ranges(RangeTable::new(BLOCKS, PER_BLOCK));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&THR, &[0], &mut sink, &flag);
    let mut exec = begin(&mut entry, &mut derive, &program, &mut b);
    for i in 0..3 {
        assert_eq!(entry.pull_nested(&mut exec, &mut b, &SET, &mut run_whole).unwrap(), Pulled::Stopped, "pull {i} before the error row");
    }
    assert_eq!(LANDED.load(Relaxed), 3, "the rows before the error landed; the error has not raised");
    assert!(entry.pull_nested(&mut exec, &mut b, &SET, &mut run_whole).is_err(), "the pull reaching the error row raises");
    assert_eq!(exec.node().parked_pulls(), 0);
    match entry.pull_nested(&mut exec, &mut b, &SET, &mut run_whole) {
        Err(Error::Refused(m)) => assert!(m.contains("read again after its error"), "{m}"),
        r => panic!("a pull after the error: {r:?}"),
    }
    entry.end(exec, false);
}

/// main 18:18Z condition 4: a cancel mid-pull drops the parked call, and no slot or region outlives the statement.
#[test]
fn a_cancel_mid_pull_drops_the_parked_call_and_nothing_outlives_the_statement() {
    let _g = serial();
    REGIONS.trim();
    DEPOT.trim();
    let held = POOL.held();
    let program = Arc::new(program());
    let mut derive = FakeDerive::ranges(RangeTable::new(BLOCKS, PER_BLOCK));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&THR, &[0], &mut sink, &flag);
    let mut exec = begin(&mut entry, &mut derive, &program, &mut b);
    assert_eq!(entry.pull_nested(&mut exec, &mut b, &SET, &mut run_whole).unwrap(), Pulled::Stopped);
    assert_eq!(exec.node().parked_pulls(), 1);
    flag.store(1, Relaxed);
    let r = entry.pull_nested(&mut exec, &mut b, &SET, &mut run_whole);
    assert!(matches!(r, Err(Error::Cancelled)), "{r:?}");
    assert_eq!(exec.node().parked_pulls(), 0, "the cancel dropped the parked call");
    entry.end(exec, false);
    // the entry's parked node (its plan cache) is the statement's no longer: gone with the entry, every region back
    drop(b);
    drop(entry);
    REGIONS.trim();
    DEPOT.trim();
    assert_eq!(POOL.held(), held, "no region or grant outlives the statement");
}

/// main 18:18Z condition 2 (the driver's half; the runners' order is gravity_parallel's test): a set no reader pulled
/// drained to its end lands its rows exactly once; a second drain runs nothing.
#[test]
fn a_never_pulled_sets_drain_lands_its_rows_exactly_once() {
    let _g = serial();
    BUDGET.store(7, Relaxed);
    let program = Arc::new(program());
    let mut derive = FakeDerive::ranges(RangeTable::new(BLOCKS, PER_BLOCK));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&THR, &[0], &mut sink, &flag);
    let mut exec = begin(&mut entry, &mut derive, &program, &mut b);
    for _ in 0..2 {
        while entry.pull_nested(&mut exec, &mut b, &SET, &mut run_whole).unwrap() == Pulled::Stopped {}
        assert_eq!(LANDED.load(Relaxed), ROWS, "the set's rows landed once");
    }
    entry.end(exec, true);
}
