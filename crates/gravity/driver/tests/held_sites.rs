//! M8 RT-1b (D824 / D1005 / M8's nested runs): an out-of-order run leaves the held call's open sites alone. The probe
//! pipeline `q` stops with a site other than its driving one OPEN (a pull-form MergeAppend's arms, an inner scan mid-way);
//! the run's pipeline without a driving site (a fed build draining a slot, an InitPlan's Result or cell end) sweeps every
//! site not held at its open (L8, `stale_site_words`). `hold(q)` kept only `q`'s driving site, so that sweep closed the
//! other one under `q` (debug: the L8 error; release: `q` re-seeked its arm and read its first row again).
use gravity_driver_rig as common;

use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU8};

use common::*;
use gravity_abi::ctx::site_ptr;
use gravity_abi::site::site::{STATE_EOF, STATE_OPEN};
use gravity_abi::status::{build_wait, DONE};
use gravity_abi::Ctx;
use gravity_abi::breaker::{Breaker, BreakerKind};
use gravity_driver::{Binding, Call, Entry, Exec, Grant, Step};
use gravity_program::{Arena, Code, End, Generation, Handle, Layout, OutCol, Pipeline, PlanKey, Program, Reason, SiteDesc, WritePlan};

const KEY: PlanKey = PlanKey { handle: Handle(29), generation: Generation(1) };

/// The probe kernel's calls, and site 1's state as its resumed call found it (this test binary's one test; no TLS, so the
/// TLS census is unchanged).
static CALLS: AtomicU32 = AtomicU32::new(0);
static RESUMED_STATE: AtomicI32 = AtomicI32::new(-1);

/// The probe `q` (pipeline 1, driving site 0, consuming the lazy build's slot 0): its first call leaves site 1 OPEN (an
/// arm or inner scan mid-way) and stops at the probe's first row; its resumed call reads site 1 on.
unsafe extern "C-unwind" fn k_probe(ctx: *mut Ctx) -> i64 {
    let n = CALLS.fetch_add(1, Relaxed) + 1;
    let s1 = site_ptr(ctx, 1);
    if n == 1 {
        (*site_ptr(ctx, 0)).state = STATE_OPEN;
        (*s1).state = STATE_OPEN;
        return build_wait(0);
    }
    RESUMED_STATE.store((*s1).state as i32, Relaxed);
    ((*site_ptr(ctx, 0)).state, (*s1).state) = (STATE_EOF, STATE_EOF);
    DONE
}

/// 0: the lazy build of `q`, no driving site (a fed build draining its producers' slot); 1: `q`.
fn program() -> Program {
    let st = Arena::new(1 << 14);
    let sites = [SiteDesc::range_source(st.handle(), 2001, &[1, 2]), SiteDesc::range_source(st.handle(), 2002, &[1, 2])];
    let mut build = Pipeline::serial(-1, End::Breaker(Breaker::sized(BreakerKind::PlainAgg, COUNT_SUM_BYTES).in_slot(0)), Reason::NoDrivingSite);
    build.lazy_for = 1;
    let pipes = [(build, Code::hand(k_noop)), (Pipeline::serial(0, End::Sink, Reason::Host).consuming(&[0]), Code::hand(k_probe))];
    let cols: [OutCol; 1] = [OutCol::new("c0", 20, -1, 8, true)];
    Program::with_pipelines(Layout { nsites: 2, out_natts: 1, out_rows: 4, scratch_bytes: 0, exec_bytes: 0 }, &sites, &cols, WritePlan::SELECT, pipes)
}

fn run_whole(e: &mut Entry, x: &mut Exec, b: &mut Binding<'_>, p: usize) -> gravity_driver::Result<()> {
    match e.run_pipeline(x, b, Call::batch(4))? {
        Step::Done => Ok(()),
        s => panic!("the build (pipeline {p}) stopped with {s:?}"),
    }
}

#[test]
fn a_build_run_out_of_order_leaves_the_held_probes_open_sites_alone() {
    CALLS.store(0, Relaxed);
    let program = std::sync::Arc::new(program());
    let mut derive = FakeDerive::ranges(RangeTable::new(24, 10));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[0], &[0], &mut sink, &flag);
    let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(Grant::of(8 << 20))).unwrap();
    assert!(entry.skip_lazy(&mut exec).unwrap(), "the lazy build steps past in order");
    assert_eq!(entry.run_pipeline(&mut exec, &mut b, Call::batch(4)).unwrap(), Step::BuildWait { pipeline: 0 });
    entry.build_wait(&mut exec, &mut b, 0, &mut run_whole).expect("the build's open sweeps no site the held probe has open (L8)");
    // SAFETY: the execution's node context, bound; site 1 is one of its program's sites.
    let state = unsafe { (*site_ptr(exec.node().ctx(), 1)).state };
    assert_eq!(state, STATE_OPEN, "the held probe's arm site stays OPEN (release: the sweep closed it)");
    assert_eq!(entry.run_pipeline(&mut exec, &mut b, Call::batch(4)).unwrap(), Step::Done);
    assert_eq!(RESUMED_STATE.load(Relaxed), STATE_OPEN as i32, "the resumed probe reads its arm on, never from its first row");
    entry.end(exec, true);
}
