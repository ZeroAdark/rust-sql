//! D1702 (main's condition 1): the slot a run installs on its entry and the execution each runner call publishes in it
//! are put back on every exit, an unwind included. A member's panic inside its node call, in the nested run the emit's
//! own call started (a rerun set, `OnDemand::rerun_members`), unwinds through the inner call's publication, the emit
//! call's, and `run_body`'s installed slot: the entry holds no pointer into the dead run after it, and runs its next
//! statement. Width 1, as d1702_nested_frames.rs (the defect's witness) runs.
mod common;

use core::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering::Relaxed};
use std::sync::Arc;

use common::*;
use gravity_abi::breaker::{Breaker, BreakerKind, HashAgg};
use gravity_abi::ctx::{resume, site_ptr, RESUME_NONE};
use gravity_abi::status::{DONE, ERROR};
use gravity_abi::Ctx;
use gravity_driver::{Frame, Merge, Result};
use gravity_kernels::par_scan_count::{BYTES, LAYOUT};
use gravity_kernels::util::{consumed, emit, flush, partial, range_seek, window};
use gravity_parallel::{FixturePool, MorselSource, Plan};
use gravity_program::lifetime::{Lifetimes, RunLife, RunMember, Runner, NO_PROBE};
use gravity_program::protos::V4Descs;
use gravity_program::{Code, End, Layout, OutCol, Pipeline, Program, Reason, SiteDesc, WritePlan};

const ROWS: u64 = 2 * 10;
const PAYLOAD: &str = "the member's panic inside its nested call";
static PANIC: AtomicBool = AtomicBool::new(false);
static PLAN: AtomicPtr<Nf> = AtomicPtr::new(core::ptr::null_mut());

/// The member (pipeline 0, the emit's rerun set): panics inside its call when `PANIC`, else counts site 0 into slot 0.
unsafe extern "C-unwind" fn k_member(ctx: *mut Ctx) -> i64 {
    if PANIC.load(Relaxed) {
        std::panic::resume_unwind(Box::new(PAYLOAD));
    }
    if resume::phase((*ctx).resume) != resume::EMIT && range_seek(ctx, 0) < 0 {
        return ERROR;
    }
    let s = site_ptr(ctx, 0);
    loop {
        let r = left(ctx);
        if r <= 0 {
            (*ctx).resume = RESUME_NONE;
            return if r == 0 { DONE } else { r };
        }
        HashAgg::at(partial(ctx), &LAYOUT, 1, BYTES).single().add_count(0, r);
        (*s).cursor += r as u32;
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

/// The emit (1): reruns the set from inside its own call through the published frame, then emits its count.
unsafe extern "C-unwind" fn k_emit(ctx: *mut Ctx) -> i64 {
    let plan = &*PLAN.load(Relaxed);
    let f = plan.frame(None);
    plan.frame(f);
    let Some(mut f) = f else { return ERROR };
    if f.as_mut().rerun_members(&[0]).is_err() {
        return ERROR;
    }
    let v = [HashAgg::at(consumed(ctx, 0), &LAYOUT, 1, BYTES).single().count(0) as u64];
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

/// The fixture plan at width 1, pipeline 0 deferred: the emit's rerun set.
struct Nf {
    inner: FixturePlan,
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
        (p == 0).then_some(1)
    }
    fn frame(&self, f: Option<Frame>) -> Option<Frame> {
        self.frame.replace(f)
    }
}

/// One statement on `entry`: the emit's rows, or the error.
fn run_on(entry: &mut gravity_driver::Entry) -> Result<Vec<Vec<u64>>> {
    let table = FixtureTable::new(T_A, 2, 10, gen_a);
    let mut derive = FixtureDerive::new(&[&table]);
    let program = Arc::new(program());
    let pool = FixturePool::new(1);
    let mut sink = LegSink::new(true);
    let plan = Nf { inner: FixturePlan::new(&program, &derive, 1, 3), frame: Cell::new(None) };
    PLAN.store(&plan as *const Nf as *mut Nf, Relaxed);
    let r = run_planned(entry, &program, &mut derive, &pool, &plan, gravity_driver::Grant::of(16 << 20), &[1_000_000], &NO_ABORT, &mut sink);
    PLAN.store(core::ptr::null_mut(), Relaxed);
    r.map(|_| sink.sorted())
}

#[test]
fn a_panic_inside_a_nested_call_puts_every_current_back_on_its_way_out() {
    let mut entry = gravity_driver::Entry::new(2);
    PANIC.store(true, Relaxed);
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_on(&mut entry)));
    PANIC.store(false, Relaxed);
    PLAN.store(core::ptr::null_mut(), Relaxed);
    assert_eq!(r.err().and_then(|p| p.downcast_ref::<&str>().copied()), Some(PAYLOAD), "the member's panic, out of the run");
    assert!(!entry.has_current_slot(), "the run's slot is off the entry after the unwind");
    assert_eq!(run_on(&mut entry).expect("the entry's next statement"), vec![vec![ROWS]]);
}
