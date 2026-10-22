//! C9: the host's answer for a call that failed after its host code stopped for the abort bit (`STOPPED`, exec glue
//! `stopped`): only a stop that is still the statement's to raise makes the call's error the statement's cancel; one its
//! owner already raised leaves the error as raised. The host is asked only after such a stop. A Node's kernel call
//! (node/raw.rs `settle_call`) and the leader's fold of a partial (the owner phase's `lend_cancel`).
use gravity_driver_rig as common;

use std::cell::Cell;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use common::*;
use gravity_abi::breaker::{Breaker, BreakerKind, Budget, Spill};
use gravity_abi::status::{raise, DONE, ERROR};
use gravity_abi::{Ctx, MorselRange, PtrWord};
use gravity_driver::{Call, Claimed, Entry, Error, Grant, Merge, Partial, Result, State};
use gravity_program::{Arena, Code, End, Layout, OutCol, Pipeline, Program, Reason, SiteDesc, WritePlan};

const NULLS: [u8; 1] = [0];

thread_local! {
    /// The cancel arrives during the call: the kernel (or the fold) raises its statement's abort bit, then polls.
    static CANCEL: Cell<bool> = const { Cell::new(false) };
    /// The host's answer: the owner raised the stop already (the error stands).
    static RAISED: Cell<bool> = const { Cell::new(false) };
    /// The host's answers asked on this thread.
    static ASKED: Cell<usize> = const { Cell::new(0) };
}

/// The driver's `STOPPED` as exec installs it, its answer the test's.
fn stopped(_byte: *const u8, _e: &Error) -> bool {
    ASKED.set(ASKED.get() + 1);
    !RAISED.get()
}

fn arm(cancel: bool, raised_already: bool) {
    let _ = gravity_driver::STOPPED.set(stopped);
    CANCEL.set(cancel);
    RAISED.set(raised_already);
    ASKED.set(0);
}

/// A host loop's poll, after the cancel when the test sends one.
unsafe fn poll(byte: *const u8) {
    if CANCEL.get() {
        (*(byte as *const AtomicU8)).fetch_or(cancelpoll::ABORT_BIT, Ordering::SeqCst);
    }
    let _ = cancelpoll::canceled();
}

unsafe extern "C-unwind" fn k_error(ctx: *mut Ctx) -> i64 {
    poll((*ctx).cancel.ptr());
    ERROR
}

fn node_call() -> Result<gravity_driver::Step> {
    let program = Arc::new(p_code(Code::hand(k_error), 1, 0));
    let mut entry = Entry::new(4);
    let mut derive = FakeDerive::new([no_rows; 4]);
    let flag = AtomicU8::new(0);
    let sink = FakeSink::new();
    let mut sink_ref = &sink;
    let mut b = binding(&[], &NULLS, &mut sink_ref, &flag);
    let mut exec = entry.start(None, &program, &mut derive, &mut b, lease(Grant::of(8 << 20))).unwrap();
    let r = entry.run(&mut exec, &mut b, Call::batch(1));
    entry.end(exec, false);
    r
}

#[test]
fn a_nodes_call_is_the_cancel_only_while_its_stop_is_the_statements_to_raise() {
    arm(false, false);
    assert!(matches!(node_call(), Err(Error::Host(_))), "no stop: the error stands");
    assert_eq!(ASKED.get(), 0, "no stop: the host is not asked");
    arm(true, false);
    assert_eq!(node_call(), Err(Error::Cancelled), "the stop is the statement's to raise: the cancel");
    assert_eq!(ASKED.get(), 1);
    arm(true, true);
    assert!(matches!(node_call(), Err(Error::Host(_))), "the owner raised the stop already: the error stands");
    assert_eq!(ASKED.get(), 1);
}

unsafe extern "C-unwind" fn k_done(_ctx: *mut Ctx) -> i64 {
    DONE
}

fn section_program() -> Program {
    let st = Arena::new(1 << 12);
    let sites = [SiteDesc::range_source(st.handle(), 2001, &[1, 2])];
    let out = [OutCol::new("n", 20, -1, 8, true)];
    let breaker = Breaker { kind: BreakerKind::HashBuild, slot: 0, bytes: 0, budget: Budget::Share, spill: Spill::None };
    let pipes = [(Pipeline::eligible(0, End::Breaker(breaker), 1000), Code::hand(k_done)), (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(k_done))];
    Program::with_pipelines(Layout { nsites: 1, out_natts: 1, out_rows: 8, scratch_bytes: 0, exec_bytes: 0 }, &sites, &out, WritePlan::SELECT, pipes)
}

/// The leader's fold of a partial: polls, then fails as a hosted combine does (a `HOST` raise).
struct FailingFold<'a>(&'a AtomicU8);

impl Merge for FailingFold<'_> {
    fn merge_into(&self, _leader: &mut State<'_>, _partial: Partial<'_>) -> Result<()> {
        // SAFETY: the statement's byte, live through the close
        unsafe { poll(self.0.as_ptr()) };
        Err(Error::Raised(raise::bit(raise::HOST)))
    }
}

/// Two workers' claims, then the close's folds of their partials: the close's result.
fn leader_fold() -> Result<()> {
    let table = RangeTable::new(8, 64);
    let mut derive = FakeDerive::ranges(table.clone());
    let program = Arc::new(section_program());
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[0], &NULLS, &mut sink, &flag);
    let mut exec = entry.start(None, &program, &mut derive, &mut b, lease(Grant::of(1))).unwrap();
    let open = entry.open_section(&mut exec, &b, &mut derive, 0, 2).unwrap();
    assert_eq!(open.run_claim(0, MorselRange::new(0, 2)), Ok(Claimed::Done));
    assert_eq!(open.run_claim(1, MorselRange::new(2, 4)), Ok(Claimed::Done));
    let closed = entry.close_section(open, &mut b, &FailingFold(&flag), true);
    entry.end(exec, false);
    closed
}

#[test]
fn the_leaders_fold_is_the_cancel_only_while_its_stop_is_the_statements_to_raise() {
    let host = Err(Error::Raised(raise::bit(raise::HOST)));
    arm(false, false);
    assert_eq!(leader_fold(), host, "no stop: the raise stands");
    assert_eq!(ASKED.get(), 0, "no stop: the host is not asked");
    arm(true, false);
    assert_eq!(leader_fold(), Err(Error::Cancelled), "the stop is the statement's to raise: the cancel");
    arm(true, true);
    assert_eq!(leader_fold(), host, "the owner raised the stop already: the raise stands");
}
