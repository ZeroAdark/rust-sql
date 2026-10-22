//! C9: a call whose host code stopped for its statement's abort bit and then failed settles as the statement's cancel.
//! A polled host step (`cancelpoll::Step`) behind a hosted aggregate's entry (the DISTINCT form's combine and final) stops
//! with a placeholder error, which the entry stashes and reports as `ERROR`, or as a `HOST` raise in an owner fold. Before,
//! the driver settled that as the statement's own error (node/raw.rs `settle_call`, the owner phase's `lend_cancel`), so a
//! pool worker's placeholder could reach the client in place of the cancel's 57014. Now the call learns whether such a
//! stop happened (`cancelpoll::scoped_seen`). Covered: a Node's kernel call, a pool worker's claim, and the leader's fold
//! of a partial; in each, a call that failed with no stop keeps its error. No host answer is installed here (the driver's
//! own rule: the stop is the statement's); c9_stopped_call_host.rs has the host's answer.
use gravity_driver_rig as common;

use std::cell::Cell;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use common::*;
use gravity_abi::breaker::{Breaker, BreakerKind, Budget, Spill};
use gravity_abi::status::{raise, raised, DONE, ERROR};
use gravity_abi::{Ctx, MorselRange, PtrWord};
use gravity_driver::{Call, Claimed, Entry, Error, Grant, Merge, Partial, Result, State};
use gravity_program::{Arena, Code, End, Layout, OutCol, Pipeline, Program, Reason, SiteDesc, WritePlan};

const NULLS: [u8; 1] = [0];

thread_local! {
    /// The cancel arrives during the call: the kernel (or the fold) raises its statement's abort bit, then polls.
    static CANCEL: Cell<bool> = const { Cell::new(false) };
    /// The worker's claim fails (else it is done, polling nothing).
    static CLAIM_FAILS: Cell<bool> = const { Cell::new(false) };
}

/// A host loop's poll, after the cancel when the test sends one.
unsafe fn poll(byte: *const u8) {
    if CANCEL.get() {
        (*(byte as *const AtomicU8)).fetch_or(cancelpoll::ABORT_BIT, Ordering::SeqCst);
    }
    let _ = cancelpoll::canceled();
}

/// A hosted entry's failure as `ERROR` (its error stashed host-side).
unsafe extern "C-unwind" fn k_error(ctx: *mut Ctx) -> i64 {
    poll((*ctx).cancel.ptr());
    ERROR
}

/// A hosted entry's failure as a `HOST` raise.
unsafe extern "C-unwind" fn k_raise(ctx: *mut Ctx) -> i64 {
    poll((*ctx).cancel.ptr());
    raised(raise::bit(raise::HOST))
}

/// A Node's kernel call (node/raw.rs `lent`, `settle_call`), statement started afresh.
fn node_call(k: unsafe extern "C-unwind" fn(*mut Ctx) -> i64) -> Result<gravity_driver::Step> {
    let program = Arc::new(p_code(Code::hand(k), 1, 0));
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
fn a_nodes_call_that_failed_after_its_host_code_stopped_is_the_cancel() {
    for (k, raise_form) in [(k_error as unsafe extern "C-unwind" fn(*mut Ctx) -> i64, false), (k_raise, true)] {
        CANCEL.set(false);
        let r = node_call(k);
        assert!(if raise_form { matches!(r, Err(Error::Raised(_))) } else { matches!(r, Err(Error::Host(_))) }, "no stop: the error stands, {r:?}");
        CANCEL.set(true);
        assert_eq!(node_call(k), Err(Error::Cancelled), "a stop: the statement's cancel");
    }
}

/// A worker's claim kernel: the section's pipeline 0.
unsafe extern "C-unwind" fn k_claim(ctx: *mut Ctx) -> i64 {
    if !CLAIM_FAILS.get() {
        return DONE;
    }
    k_error(ctx)
}

unsafe extern "C-unwind" fn k_none(_ctx: *mut Ctx) -> i64 {
    DONE
}

fn section_program() -> Program {
    let st = Arena::new(1 << 12);
    let sites = [SiteDesc::range_source(st.handle(), 2001, &[1, 2])];
    let out = [OutCol::new("n", 20, -1, 8, true)];
    let breaker = Breaker { kind: BreakerKind::HashBuild, slot: 0, bytes: 0, budget: Budget::Share, spill: Spill::None };
    let pipes = [(Pipeline::eligible(0, End::Breaker(breaker), 1000), Code::hand(k_claim)), (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite), Code::hand(k_none))];
    Program::with_pipelines(Layout { nsites: 1, out_natts: 1, out_rows: 8, scratch_bytes: 0, exec_bytes: 0 }, &sites, &out, WritePlan::SELECT, pipes)
}

/// The leader's fold of a partial: polls, then fails as a hosted combine does (`FOLD_HOST`'s raise).
struct FailingFold<'a>(&'a AtomicU8);

impl Merge for FailingFold<'_> {
    fn merge_into(&self, _leader: &mut State<'_>, _partial: Partial<'_>) -> Result<()> {
        // SAFETY: the statement's byte, live through the close
        unsafe { poll(self.0.as_ptr()) };
        Err(Error::Raised(raise::bit(raise::HOST)))
    }
}

/// A two-worker section: worker 0's claim (failing when `fails`), else both workers' claims and the close with the
/// leader's folds of their partials. Returns the first claim's and the close's results.
fn section(fails: bool) -> (Result<Claimed>, Result<()>) {
    let table = RangeTable::new(8, 64);
    let mut derive = FakeDerive::ranges(table.clone());
    let program = Arc::new(section_program());
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[0], &NULLS, &mut sink, &flag);
    let mut exec = entry.start(None, &program, &mut derive, &mut b, lease(Grant::of(1))).unwrap();
    let open = entry.open_section(&mut exec, &b, &mut derive, 0, 2).unwrap();
    CLAIM_FAILS.set(fails);
    let claim = open.run_claim(0, MorselRange::new(0, 2));
    if !fails {
        assert_eq!(open.run_claim(1, MorselRange::new(2, 4)), Ok(Claimed::Done));
    }
    let closed = entry.close_section(open, &mut b, &FailingFold(&flag), !fails);
    entry.end(exec, false);
    (claim, closed)
}

#[test]
fn a_workers_claim_that_failed_after_its_host_code_stopped_is_the_cancel() {
    CANCEL.set(false);
    assert!(matches!(section(true).0, Err(Error::Host(_))), "no stop: the claim's error stands");
    CANCEL.set(true);
    assert_eq!(section(true).0, Err(Error::Cancelled), "a stop: the statement's cancel");
}

#[test]
fn the_leaders_fold_that_failed_after_its_host_code_stopped_is_the_cancel() {
    CANCEL.set(false);
    let (claim, closed) = section(false);
    assert_eq!(claim, Ok(Claimed::Done));
    assert_eq!(closed, Err(Error::Raised(raise::bit(raise::HOST))), "no stop: the fold's raise stands");
    CANCEL.set(true);
    assert_eq!(section(false).1, Err(Error::Cancelled), "a stop: the statement's cancel");
}
