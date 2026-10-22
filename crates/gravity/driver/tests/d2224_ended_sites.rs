//! D2224: a pipeline that ends early (DONE at its first row: a LIMIT or a top satisfied, an EXISTS tail's inner scan) leaves
//! its scan's site OPEN. The next pipeline without a driving site (a RIGHT JOIN's unmatched emit, a fold's emit over the
//! LIMIT, an InitPlan's Result) sweeps every site at its open (L8, `stale_site_words`): a debug build raised "opens with
//! site 0's per-open word state = 1 left by an earlier open", a release build logged the ledger line and reset it there.
//! The ended pipeline's run resets the words it left, so the open finds them clean.
use gravity_driver_rig as common;

use std::sync::atomic::AtomicU8;

use common::*;
use gravity_abi::ctx::site_ptr;
use gravity_abi::site::site::STATE_OPEN;
use gravity_driver::{Call, Entry, Grant, Step};
use gravity_program::{Arena, Code, End, Generation, Handle, Layout, OutCol, Pipeline, PlanKey, Program, Reason, SiteDesc, WritePlan};

const KEY: PlanKey = PlanKey { handle: Handle(31), generation: Generation(1) };

/// 0: a range scan that emits its first row and ends (DONE), its site left OPEN; 1: an emit with no driving site.
fn program() -> Program {
    let st = Arena::new(1 << 14);
    let sites = [SiteDesc::range_source(st.handle(), 2001, &[1, 2])];
    let cols: [OutCol; 2] = [OutCol::new("c0", 20, -1, 8, true), OutCol::new("c1", 20, -1, 8, true)];
    let pipes = [
        (Pipeline::serial(0, End::Sink, Reason::Host), Code::hand(k_range_first_row)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(k_noop)),
    ];
    Program::with_pipelines(Layout { nsites: 1, out_natts: 2, out_rows: 4, scratch_bytes: 0, exec_bytes: 0 }, &sites, &cols, WritePlan::SELECT, pipes)
}

#[test]
fn d2224_a_site_less_open_after_an_early_end_finds_no_stale_site() {
    let program = std::sync::Arc::new(program());
    let mut derive = FakeDerive::ranges(RangeTable::new(24, 10));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[0], &[0], &mut sink, &flag);
    let mut exec = entry.start(Some(KEY), &program, &mut derive, &mut b, lease(Grant::of(8 << 20))).unwrap();
    assert_eq!(entry.run_pipeline(&mut exec, &mut b, Call::batch(4)).unwrap(), Step::Done, "the scan ends at its first row");
    // SAFETY: the execution's node context, bound; site 0 is its program's site.
    let state = unsafe { (*site_ptr(exec.node().ctx(), 0)).state };
    assert_ne!(state, STATE_OPEN, "the ended run's site is not left OPEN");
    assert_eq!(entry.run_pipeline(&mut exec, &mut b, Call::batch(4)).expect("the site-less emit opens clean (L8)"), Step::Done);
    entry.end(exec, true);
}
