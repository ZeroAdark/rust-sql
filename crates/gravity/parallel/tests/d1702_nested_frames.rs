//! D1702 (the rest of D1744's class, D1358b's aliasing): an M8 nested run re-enters the runner from inside a kernel call
//! (a host re-runs a rerun set, `OnDemand::rerun_members`, as it runs an InitPlan's, `run_members`; pulls a CTE,
//! `pull_members`) through the frame published to the plan. Before the fix the frame held `run_body`'s own pointers, and the nested run wrote the execution (its next
//! pipeline, state, resume word, the node's nest count and lanes) through them while every frame between `run_body` and
//! the kernel held a `&mut Entry` / `&mut Exec` / `&mut Binding` (`run_arm`, `run_arm_now`, `Entry::run_pipeline`) or a
//! `&mut Node` (`Node::call_pipeline`) across the call: a write no frame's reference had derived. Plain `cargo test` passes
//! either way (the nested run puts back every field the outer frames read after the call); the defect shows under
//! `MIRIFLAGS=-Zmiri-tree-borrows cargo +nightly miri test -p gravity_parallel --test d1702_nested_frames` as a protector
//! error. Width 1, a few rows: Miri's speed.
mod common;

use core::cell::Cell;
use std::sync::atomic::{AtomicI64, AtomicPtr, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

use common::*;
use gravity_abi::breaker::{Breaker, BreakerKind, HashAgg};
use gravity_abi::ctx::{resume, site_ptr, RESUME_NONE};
use gravity_abi::status::{suspend, DONE, ERROR};
use gravity_abi::Ctx;
use gravity_driver::{Frame, Merge, Pulled, Result};
use gravity_kernels::par_scan_count::{BYTES, LAYOUT};
use gravity_kernels::util::{consumed, emit, flush, partial, range_seek, window};
use gravity_parallel::{FixturePool, MorselSource, Plan};
use gravity_program::lifetime::{Lifetimes, RunLife, RunMember, Runner, NO_PROBE};
use gravity_program::protos::V4Descs;
use gravity_program::{Code, End, Layout, OutCol, Pipeline, Program, Reason, SiteDesc, WritePlan};

/// Rows a member call counts before it stops resumably (a pull's budget); the whole set is `ROWS`.
const BUDGET: u64 = 7;
const ROWS: u64 = 2 * 10;
/// The rows the member appended; how the emit reaches the set: 0 a rerun (`rerun_members`), 1 a pull (`pull_members`).
static LANDED: AtomicU64 = AtomicU64::new(0);
static EMIT_SAW: AtomicI64 = AtomicI64::new(-1);
static PULL: AtomicU64 = AtomicU64::new(0);
/// The statement's plan, for the emit's host-side run (as `m8_init_run` / `m8_cte_pull` reach the frame published to it).
static PLAN: AtomicPtr<Nf> = AtomicPtr::new(core::ptr::null_mut());
static ERR: Mutex<String> = Mutex::new(String::new());

/// The member (pipeline 0): counts site 0's rows into its table (slot 0). Pulled, it stops resumably after `BUDGET` rows;
/// re-run as a rerun set's member, it runs to its end.
unsafe extern "C-unwind" fn k_member(ctx: *mut Ctx) -> i64 {
    if resume::phase((*ctx).resume) != resume::EMIT && range_seek(ctx, 0) < 0 {
        return ERROR;
    }
    let s = site_ptr(ctx, 0);
    let pulled = PULL.load(Relaxed) == 1;
    let mut n = 0u64;
    loop {
        let r = left(ctx);
        if r <= 0 {
            (*ctx).resume = RESUME_NONE;
            return if r == 0 { DONE } else { r };
        }
        let take = if pulled { (r as u64).min(BUDGET - n) } else { r as u64 };
        HashAgg::at(partial(ctx), &LAYOUT, 1, BYTES).single().add_count(0, take as i64);
        LANDED.fetch_add(take, Relaxed);
        (*s).cursor += take as u32;
        n += take;
        if pulled && n >= BUDGET {
            (*ctx).resume = resume::word(resume::EMIT, 0);
            return suspend(false);
        }
    }
}

/// Site 0's rows left in its window, the next window when it is spent: `util::advance`'s protocol over the site's raw
/// pointer (> 0 rows, 0 at the claim's end, a status below zero). D1705: `util::advance` holds `site`'s `&mut Site` across
/// the host's `window`, which rewrites the block in place (a Tree Borrows violation of the kernel helpers', filed apart);
/// this witness judges the driver's frames, so its kernels reach the block only through `site_ptr`.
unsafe fn left(ctx: *mut Ctx) -> i64 {
    let s = site_ptr(ctx, 0);
    loop {
        let (cursor, end) = ((*s).cursor, (*s).end());
        if cursor < end {
            return (end - cursor) as i64;
        }
        let n = window(ctx, 0);
        if n <= 0 {
            if n == 0 {
                (*s).state = gravity_abi::site::site::STATE_EOF;
            }
            return n;
        }
    }
}

/// The emit (1): runs the set nested from inside its own call through the published frame, then emits the set's count.
unsafe extern "C-unwind" fn k_emit(ctx: *mut Ctx) -> i64 {
    let plan = &*PLAN.load(Relaxed);
    let f = plan.frame(None);
    plan.frame(f);
    let Some(mut f) = f else { return ERROR };
    let ran = match PULL.load(Relaxed) {
        1 => loop {
            match f.as_mut().pull_members(&[0], BUDGET) {
                Ok(Pulled::Stopped) => {}
                Ok(Pulled::Done) => break Ok(()),
                Err(e) => break Err(e),
            }
        },
        _ => f.as_mut().rerun_members(&[0]),
    };
    if let Err(e) = ran {
        *ERR.lock().unwrap() = format!("{e:?}");
        return ERROR;
    }
    let a = HashAgg::at(consumed(ctx, 0), &LAYOUT, 1, BYTES).single().count(0);
    EMIT_SAW.store(a, Relaxed);
    let v = [a as u64];
    let n = [0u8; 1];
    if emit(ctx, 1, &[(v.as_ptr(), n.as_ptr(), 1)]) < 0 || flush(ctx) < 0 {
        return ERROR;
    }
    DONE
}

fn program() -> Program {
    let st = gravity_program::Arena::new(1 << 12);
    let sites = [SiteDesc::range_source(st.handle(), T_A, &[1])];
    let out = [OutCol::new("set", 20, -1, 8, true)];
    let pipes = [
        (Pipeline::serial(0, End::Breaker(Breaker::sized(BreakerKind::PlainAgg, BYTES).in_slot(0)), Reason::Host), Code::hand(k_member)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(&[0]), Code::hand(k_emit)),
    ];
    let mut p = Program::with_pipelines(Layout { nsites: 1, out_natts: 1, out_rows: 8, scratch_bytes: 0, exec_bytes: 0 }, &sites, &out, WritePlan::SELECT, pipes);
    // pipeline 0 is the emit's rerun set's one member, as lowerv4's lifetime home declares it
    let member = RunMember { pipeline: 0, runner: Runner::Set(0), life: RunLife::EveryRun, lazy_probe: NO_PROBE };
    p.set_v4(&V4Descs { lifetimes: Lifetimes::new(vec![member]).unwrap(), ..V4Descs::new(Vec::new()) });
    p
}

/// The fixture plan at width 1, pipeline 0 deferred: a rerun set's member owned by the emit, or a pulled CTE plan it reads.
struct Nf {
    inner: FixturePlan,
    pull: bool,
    frame: Cell<Option<Frame>>,
}

impl Plan for Nf {
    fn width_of(&self, _p: usize) -> u16 {
        1
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
    fn rerun_owner(&self, p: usize) -> Option<usize> {
        (p == 0 && !self.pull).then_some(1)
    }
    fn pull_readers(&self, p: usize) -> &[usize] {
        if p == 0 && self.pull { &[1] } else { &[] }
    }
    fn frame(&self, f: Option<Frame>) -> Option<Frame> {
        self.frame.replace(f)
    }
}

/// One statement: the emit's rows, or the error.
fn run(pull: bool) -> Result<Vec<Vec<u64>>> {
    let table = FixtureTable::new(T_A, 2, 10, gen_a);
    let mut derive = FixtureDerive::new(&[&table]);
    let program = Arc::new(program());
    let pool = FixturePool::new(1);
    let mut entry = gravity_driver::Entry::new(2);
    let mut sink = LegSink::new(true);
    let plan = Nf { inner: FixturePlan::new(&program, &derive, 1, 3), pull, frame: Cell::new(None) };
    PLAN.store(&plan as *const Nf as *mut Nf, Relaxed);
    (LANDED.store(0, Relaxed), EMIT_SAW.store(-1, Relaxed), PULL.store(pull as u64, Relaxed));
    ERR.lock().unwrap().clear();
    let r = run_planned(&mut entry, &program, &mut derive, &pool, &plan, gravity_driver::Grant::of(16 << 20), &[1_000_000], &NO_ABORT, &mut sink);
    PLAN.store(core::ptr::null_mut(), Relaxed);
    r.map(|_| sink.sorted())
}

/// The tests of this binary one at a time (the statics above are process-wide).
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[test]
fn a_rerun_inside_a_call_runs_its_members_through_the_frame_below_every_outer_borrow() {
    let _g = serial();
    let rows = run(false).unwrap_or_else(|e| panic!("{e:?} ({})", ERR.lock().unwrap()));
    assert_eq!(rows, vec![vec![ROWS]], "the emit reads the member's whole table, run once from inside its call");
    assert_eq!(LANDED.load(Relaxed), ROWS);
}

#[test]
fn a_pull_inside_a_call_runs_the_set_through_the_frame_below_every_outer_borrow() {
    let _g = serial();
    let rows = run(true).unwrap_or_else(|e| panic!("{e:?} ({})", ERR.lock().unwrap()));
    assert_eq!(rows, vec![vec![ROWS]], "the emit pulls the set to its end, then reads it");
    assert_eq!(LANDED.load(Relaxed), ROWS);
}
