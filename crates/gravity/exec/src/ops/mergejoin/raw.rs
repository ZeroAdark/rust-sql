//! `ops::mergejoin`'s only `unsafe` (PLAN-v5 §3.2 R6): the pulled inner's object laid behind its handle (kernels
//! ops/mergejoin/raw.rs `PullHead`) and its `op` / `hold`, and the runner frame's pull of the inner's set (exec pulled/'s
//! `RawPull`, as window's raw.rs).
#![allow(unsafe_code)]
use core::cell::{Cell, RefCell};

use gravity_abi::status::ERROR;
use gravity_abi::Ctx;
use gravity_driver::{Frame, Pulled};
use gravity_kernels::ops::mergejoin::raw::{PullHead, PULL_MAGIC};
use types_error::PgError;

use crate::glue;

/// The object behind the handle word: the head first (the address the IR holds), then the pull it runs and the error a
/// budgeted pull holds (MJ-AHEAD: raised past the row the merge wanted, the next pull's; `raised` once it was).
#[repr(C)]
pub(super) struct PullCell {
    head: PullHead,
    pull: Box<dyn Fn() -> types_error::PgResult<bool>>, // alloc: Exec
    held: RefCell<Option<Box<PgError>>>,
    raised: Cell<bool>,
}

impl PullCell {
    pub(super) fn new(pull: Box<dyn Fn() -> types_error::PgResult<bool>>) -> Box<PullCell> {
        Box::new(PullCell { head: PullHead { magic: PULL_MAGIC, op, hold }, pull, held: RefCell::new(None), raised: Cell::new(false) }) // alloc: Exec
    }

    /// The handle word's value: the head's address (the cell is boxed: it stays put while the box lives).
    pub(super) fn handle(&self) -> u64 {
        &self.head as *const PullHead as u64
    }
}

impl Drop for PullCell {
    fn drop(&mut self) {
        self.head.magic = 0;
    }
}

/// `PullHead::op`: one pull, 1 stored / 0 ended, ERROR with the error stashed (a held one first: MJ-AHEAD).
unsafe fn op(h: *mut PullHead, _ctx: *mut Ctx) -> i64 {
    // SAFETY: `h` is the head of a live `PullCell` (`MergePull` checked its magic), laid for this execution.
    let cell = &*h.cast::<PullCell>();
    let Ok(mut held) = cell.held.try_borrow_mut() else { return fail(glue::internal("gravity: merge.join's pulled inner entered inside its own call")) };
    if let Some(e) = held.take() {
        cell.raised.set(true);
        return fail(e);
    }
    drop(held);
    match (cell.pull)() {
        Ok(true) => 1,
        Ok(false) => 0,
        Err(e) => {
            gravity_fn::stash_error(e);
            ERROR
        }
    }
}

/// `PullHead::hold` (MJ-AHEAD): the error the last pull stashed kept for the next pull (0) when PG raises it at its row
/// (`holdable`) and none was raised before; else stashed again (ERROR). No error stashed: ERROR (the IR raises at once).
unsafe fn hold(h: *mut PullHead, _ctx: *mut Ctx) -> i64 {
    // SAFETY: `h` is the head of a live `PullCell` (`MergeHold` checked its magic), laid for this execution.
    let cell = &*h.cast::<PullCell>();
    let Ok(mut held) = cell.held.try_borrow_mut() else { return fail(glue::internal("gravity: merge.join's pulled inner held inside its own call")) };
    match gravity_fn::take_error() {
        None => ERROR,
        Some(e) if held.is_none() && !cell.raised.get() && super::proto::holdable(&e) => {
            *held = Some(e);
            0
        }
        Some(e) => fail(e),
    }
}

#[cold]
#[inline(never)]
fn fail(e: Box<PgError>) -> i64 {
    gravity_fn::stash_error(e);
    ERROR
}

/// `members` (the merge's pulled inner and its exclusive producers, the inner last) run on through the runner's frame
/// (`OnDemand::pull_members`, RT-2f) until the inner's end stops resumably after the rows the pull wants, or ends.
pub(super) fn pull(mut frame: Frame, members: &[u16]) -> gravity_driver::Result<Pulled> {
    // SAFETY: the frame is the runner's, published on this thread while it runs the program (gravity_parallel `publish`,
    // whose guard takes it back on every exit); this call is inside one of its calls (the merge's `m11_pull`, from the
    // outer pipeline on the leader), and nothing else uses the frame while the members run.
    unsafe { frame.as_mut() }.pull_members(members, 1)
}
