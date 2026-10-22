//! `ops::window`'s only `unsafe` (PLAN-v5 §3.2 R6): the runner frame's pull of a window's fill (the exact request,
//! PLAN-v5 §2.5; W4), as nest.cte's `m8_cte_pull`; D4237: its re-arm through the frame, and each window a rerun set
//! re-arms, found through its handle word in the caller's context params (exec rerun/mod.rs `run`), as nest.cte's
//! `rearm_ctes`.
#![allow(unsafe_code)]
use gravity_abi::Ctx;
use gravity_driver::{Frame, Pulled};
use types_error::PgResult;

use crate::glue;

/// `members` (a window's pulled fill and its exclusive producers, the fill last) run on through the runner's frame
/// (`OnDemand::pull_members`, RT-2f) until the fill's end stops resumably after the row the store wants, or ends.
pub(super) fn pull(mut frame: Frame, members: &[u16]) -> gravity_driver::Result<Pulled> {
    // SAFETY: the frame is the runner's, published on this thread while it runs the program (gravity_parallel `publish`,
    // whose guard takes it back on every exit); this call is inside one of its calls (the window object's, from the reader
    // pipeline on the leader), and nothing else uses the frame while the members run.
    unsafe { frame.as_mut() }.pull_members(members, 1)
}

/// D4237: pulled set `members` re-armed through the runner's frame (`OnDemand::rearm_pulled`, the CTE re-arm's entry): a
/// parked call dropped, the producers counted as not run, every member's kept slot reset in place.
pub(super) fn rearm(mut frame: Frame, members: &[u16]) -> gravity_driver::Result<()> {
    // SAFETY: as `pull`: the runner's frame, published on this thread while it runs the program; this call is inside one of
    // its calls (the rerun set's run, from its owner on the leader), and nothing else uses the frame meanwhile.
    unsafe { frame.as_mut() }.rearm_pulled(members)
}

/// D4237: each window whose handle word is in `windows` re-armed (hosts `WinObj::rearm`). `ctx` is the caller's context,
/// bound for this call (`m8_rerun`'s first argument), its params holding the handle words, each one the object exec's
/// prepare laid, live until the execution's end. No window: no read.
pub(crate) fn rearm_windows(ctx: *mut Ctx, windows: &[usize]) -> PgResult<()> {
    if windows.is_empty() {
        return Ok(());
    }
    if ctx.is_null() {
        return Err(glue::internal("gravity: m8_rerun: a window to re-arm with no context (D4237)"));
    }
    for &h in windows {
        // SAFETY: the caller's bound context (above); its params hold the handle words
        let w = unsafe { *(*(ctx as *const u8).add(gravity_abi::ctx::hdr::PARAMS).cast::<*const u64>()).add(h) };
        // SAFETY: a nonzero handle word is the window object's cell exec's prepare laid (`rearm_at` checks its magic first)
        unsafe { gravity_hosts::ops::window::object::rearm_at(w) }?;
    }
    Ok(())
}
