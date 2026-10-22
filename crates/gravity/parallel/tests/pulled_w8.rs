//! M8 RT-2f through the runner (main 18:18Z conditions 1 and 2): a pulled set (a CTE's plan) runs leader-serial while the
//! rest of the statement runs at its width (W8 here), pulled from the emit's call through the runner's published frame;
//! a pulled member planned past width 1 is a Bug by name; a data-modifying set no reader pulled drains to its end after
//! the emit finished (PG's ExecPostprocessPlan), its rows landing exactly once.
mod common;

use core::cell::Cell;
use std::sync::atomic::{AtomicI64, AtomicPtr, AtomicU64, AtomicU8, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

use common::*;
use gravity_abi::breaker::{Breaker, BreakerKind, HashAgg};
use gravity_abi::ctx::{resume, site_ptr, RESUME_NONE};
use gravity_abi::status::{suspend, DONE, ERROR};
use gravity_abi::Ctx;
use gravity_driver::{Entry, Frame, Merge, Pulled, Result};
use gravity_kernels::par_scan_count::{par_scan_count, BYTES, LAYOUT};
use gravity_kernels::util::{advance, consumed, emit, end, flush, partial, range_seek};
use gravity_parallel::{FixturePool, MorselSource, Plan};
use gravity_program::{Code, End, Layout, OutCol, Pipeline, Program, Reason, SiteDesc, WritePlan};

const BUDGET: u64 = 37;
/// Rows the pulled pipeline appended; the count the emit saw in the store; 1: the emit pulls the set to its end first.
static LANDED: AtomicU64 = AtomicU64::new(0);
static EMIT_SAW: AtomicI64 = AtomicI64::new(-1);
static EMIT_PULLS: AtomicU8 = AtomicU8::new(1);
/// The statement's plan, for the emit's host-side pull (as `m8_cte_pull` reaches the frame published to the plan).
static PLAN: AtomicPtr<Pw> = AtomicPtr::new(core::ptr::null_mut());
static PULL_ERR: Mutex<String> = Mutex::new(String::new());

/// The pulled pipeline (0, a CTE's plan): counts site 0's rows into its store (slot 0), `BUDGET` rows per call, then stops
/// resumably; resumed, it reads on from its window's cursor.
unsafe extern "C-unwind" fn k_pull(ctx: *mut Ctx) -> i64 {
    if resume::phase((*ctx).resume) != resume::EMIT && range_seek(ctx, 0) < 0 {
        return ERROR;
    }
    let store = HashAgg::at(partial(ctx), &LAYOUT, 1, BYTES).single();
    let mut n = 0u64;
    loop {
        let r = advance(ctx, 0);
        if r <= 0 {
            (*ctx).resume = RESUME_NONE;
            return if r == 0 { DONE } else { r };
        }
        let s = &mut *site_ptr(ctx, 0);
        let take = ((end(s) - s.cursor) as u64).min(BUDGET - n);
        store.add_count(0, take as i64);
        LANDED.fetch_add(take, Relaxed);
        s.cursor += take as u32;
        n += take;
        if n >= BUDGET {
            (*ctx).resume = resume::word(resume::EMIT, 0);
            return suspend(false);
        }
    }
}

/// The emit (2): pulls the set to its end (with `EMIT_PULLS`), then emits (the store's count, the width-8 scan's count).
unsafe extern "C-unwind" fn k_emit(ctx: *mut Ctx) -> i64 {
    if EMIT_PULLS.load(Relaxed) == 1 {
        let plan = &*PLAN.load(Relaxed);
        let f = plan.frame(None);
        plan.frame(f);
        let Some(mut f) = f else { return ERROR };
        loop {
            match f.as_mut().pull_members(&[0], BUDGET) {
                Ok(Pulled::Stopped) => {}
                Ok(Pulled::Done) => break,
                Err(e) => {
                    *PULL_ERR.lock().unwrap() = format!("{e:?}");
                    return ERROR;
                }
            }
        }
    }
    let a = HashAgg::at(consumed(ctx, 0), &LAYOUT, 1, BYTES).single().count(0);
    let b = HashAgg::at(consumed(ctx, 1), &LAYOUT, 1, BYTES).single().count(0);
    EMIT_SAW.store(a, Relaxed);
    let v = [a as u64, b as u64];
    let n = [0u8; 2];
    if emit(ctx, 2, &[(v.as_ptr(), n.as_ptr(), 2)]) < 0 || flush(ctx) < 0 {
        return ERROR;
    }
    DONE
}

fn program(rows: u64) -> Program {
    let st = gravity_program::Arena::new(1 << 12);
    let sites = [SiteDesc::range_source(st.handle(), T_A, &[1])];
    let out = [OutCol::new("store", 20, -1, 8, true), OutCol::new("scan", 20, -1, 8, true)];
    let agg = |slot| End::Breaker(Breaker::sized(BreakerKind::PlainAgg, BYTES).in_slot(slot));
    let pipes = [
        (Pipeline::serial(0, agg(0), Reason::Host), Code::hand(k_pull)),
        (Pipeline::eligible(0, agg(1), rows), Code::hand(par_scan_count)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(&[0, 1]), Code::hand(k_emit)),
    ];
    Program::with_pipelines(Layout { nsites: 1, out_natts: 2, out_rows: 64, scratch_bytes: 0, exec_bytes: 0 }, &sites, &out, WritePlan::SELECT, pipes)
}

/// The fixture plan at width `w`, pipeline 0 a pulled set's CTE plan (deferred, read by the emit) planned at `member_w`,
/// drained at the statement's end with `drain`.
struct Pw {
    inner: FixturePlan,
    member_w: u16,
    drain: bool,
    frame: Cell<Option<Frame>>,
}

impl Plan for Pw {
    fn width_of(&self, p: usize) -> u16 {
        if p == 0 { self.member_w } else { self.inner.width_of(p) }
    }
    fn claims_for(&self, p: usize) -> &dyn MorselSource {
        self.inner.claims_for(p)
    }
    fn merge_for(&self, p: usize) -> &dyn Merge {
        self.inner.merge_for(p)
    }
    fn after_pipeline(&self, p: usize, exec: &mut gravity_driver::Exec) -> Result<()> {
        self.inner.after_pipeline(p, exec)
    }
    fn deferred(&self, p: usize) -> bool {
        p == 0
    }
    fn pull_readers(&self, p: usize) -> &[usize] {
        if p == 0 { &[2] } else { &[] }
    }
    fn drain_at_end(&self) -> gravity_parallel::DrainSets<'_> {
        let mut sets = gravity_parallel::DrainSets::none();
        if self.drain {
            sets.with_vec(|v| v.push(&[0u16][..]));
        }
        sets
    }
    fn frame(&self, f: Option<Frame>) -> Option<Frame> {
        self.frame.replace(f)
    }
}

/// One statement at width `w`; the emit's rows, or the error.
fn run(w: u16, member_w: u16, drain: bool, pulls: bool) -> Result<Vec<Vec<u64>>> {
    let table = FixtureTable::new(T_A, 20, 50, gen_a);
    let mut derive = FixtureDerive::new(&[&table]);
    let program = Arc::new(program(ROWS));
    let pool = FixturePool::new(8);
    let mut entry = Entry::new(2);
    let mut sink = LegSink::new(true);
    let plan = Pw { inner: FixturePlan::new(&program, &derive, w, 3), member_w, drain, frame: Cell::new(None) };
    PLAN.store(&plan as *const Pw as *mut Pw, Relaxed);
    (LANDED.store(0, Relaxed), EMIT_SAW.store(-1, Relaxed), EMIT_PULLS.store(pulls as u8, Relaxed));
    let r = run_planned(&mut entry, &program, &mut derive, &pool, &plan, gravity_driver::Grant::of(64 << 20), &[1_000_000], &NO_ABORT, &mut sink);
    PLAN.store(core::ptr::null_mut(), Relaxed);
    r.map(|_| sink.sorted())
}

/// The tests of this binary one at a time (the kernels' counters are process-wide).
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

const ROWS: u64 = 20 * 50;

#[test]
fn a_pulled_set_runs_leader_serial_while_the_statement_runs_at_w8() {
    let _g = serial();
    for w in [1u16, 8] {
        let rows = run(w, 1, false, true).unwrap();
        assert_eq!(rows, vec![vec![ROWS, ROWS]], "width {w}: the store holds one whole run, the W{w} scan all rows");
        assert_eq!(LANDED.load(Relaxed), ROWS, "width {w}");
    }
}

/// main 18:18Z condition 1: a pulled member planned past width 1 (a parked member would be a pool section stopped
/// mid-run with workers holding claims) is a Bug by name, never run.
#[test]
fn a_pulled_member_planned_at_w8_is_a_bug_by_name() {
    let _g = serial();
    PULL_ERR.lock().unwrap().clear();
    assert!(run(8, 8, false, true).is_err(), "the emit's pull raises");
    let m = PULL_ERR.lock().unwrap().clone();
    assert!(m.contains("gravity: Bug: pulled member 0 planned at width 8"), "{m}");
    assert_eq!(LANDED.load(Relaxed), 0, "nothing ran");
}

/// main 18:18Z condition 2: a data-modifying set no reader pulled drains after the emit finished (the emit saw an empty
/// store), its rows landing exactly once per statement.
#[test]
fn a_never_read_data_modifying_set_drains_after_the_emit_its_rows_landing_once() {
    let _g = serial();
    for w in [1u16, 8] {
        for _ in 0..2 {
            let rows = run(w, 1, true, false).unwrap();
            assert_eq!(rows, vec![vec![0, ROWS]], "width {w}: the emit ran before the drain");
            assert_eq!(EMIT_SAW.load(Relaxed), 0);
            assert_eq!(LANDED.load(Relaxed), ROWS, "width {w}: the drain landed the set's rows once");
        }
        let rows = run(w, 1, true, true).unwrap();
        assert_eq!(rows, vec![vec![ROWS, ROWS]], "width {w}: pulled whole by the emit");
        assert_eq!(LANDED.load(Relaxed), ROWS, "width {w}: the drain of a set pulled to its end runs nothing again");
    }
}
