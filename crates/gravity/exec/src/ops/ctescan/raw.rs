//! `ops::ctescan`'s raw half in exec (PLAN-v5 §3.2 R6; setop_rec-0043): each CTE a rerun set re-arms, found through its
//! handle word in the caller's context params (exec rerun/mod.rs `run`, after `rearm_sets`), and the runner's frame a CTE
//! object holds; and the object itself: its head, laid at each execution's prepare, and the operation behind
//! `m8_cte_pull`.
#![allow(unsafe_code)]
use gravity_abi::status::ERROR;
use gravity_abi::Ctx;
use types_error::PgResult;

use super::proto::{bad, pull, CteRt, Head, CTE_MAGIC, OP_CTE_PULL, WORDS};
use crate::glue;
use crate::ops::Frame;

/// Each CTE whose handle word is in `ctes` re-armed (ops/ctescan/proto.rs `rearm`). `ctx` is the caller's context, bound
/// for this call (`m8_rerun`'s first argument), its params holding the handle words, each one the object proto.rs `lay`
/// laid, live until the execution's end. No CTE: no read.
pub(crate) fn rearm_ctes(ctx: *mut Ctx, ctes: &[usize]) -> PgResult<()> {
    if ctes.is_empty() {
        return Ok(());
    }
    if ctx.is_null() {
        return Err(glue::internal("gravity: m8_rerun: a CTE to re-arm with no context (setop_rec-0043)"));
    }
    for &h in ctes {
        // SAFETY: the caller's bound context (above); its params hold the handle words, a nonzero one the object's head, its
        // magic checked before any other read
        let o = unsafe { *(*(ctx as *const u8).add(gravity_abi::ctx::hdr::PARAMS).cast::<*const u64>()).add(h) } as *const Head;
        // SAFETY: as above: a nonzero handle word is a laid object's head
        if o.is_null() || unsafe { (*o).magic } != CTE_MAGIC {
            return Err(glue::internal("gravity: m8_rerun: a re-armed CTE's handle word is not its object (setop_rec-0043)"));
        }
        // SAFETY: the trailing word names the state `lay` made beside this object; both live until the execution's end
        super::proto::rearm(unsafe { &*(*o.cast::<u64>().add(WORDS - 1) as *const CteRt) })?;
    }
    Ok(())
}

/// The runner's frame `f` a CTE object holds (published by the runner on this thread while it runs the program), for one
/// call made inside one of the runner's calls, nothing else using the frame meanwhile.
pub(crate) fn on_demand(f: &mut Frame) -> &mut (dyn gravity_driver::OnDemand + 'static) {
    // SAFETY: the frame is the runner's own object, alive while it is published (the CTE object drops its copy when the
    // runner unpublishes it: `Live::frame(None)`); this thread only
    unsafe { f.as_mut() }
}

/// The object laid in `cell` (WORDS words, proto.rs `lay`): the head, whose operation is `op` below, then the address of
/// `rt`, the state `lay` made beside it; both live in the Live to the execution's end.
pub(crate) fn lay_head(cell: &mut [u64], rt: &CteRt) {
    assert_eq!(cell.len(), WORDS, "nest.cte's object is its head and its state's address");
    // SAFETY: `cell` is WORDS words: the head, then the state's address; both live in the Live to the execution's end.
    unsafe {
        let c = cell.as_mut_ptr();
        c.cast::<Head>().write(Head { magic: CTE_MAGIC, op });
        c.add(WORDS - 1).write(rt as *const CteRt as u64);
    }
}

/// The object's `Head::op`: `OP_CTE_PULL` (`a` = the rows to take) only.
unsafe fn op(h: *mut Head, _ctx: *mut Ctx, code: u64, a: u64, _b: u64) -> i64 {
    // SAFETY: the trailing word names the state `lay` made beside this object; both live until the execution's end.
    let rt = &*(*h.cast::<u64>().add(WORDS - 1) as *const CteRt);
    let r = match code {
        OP_CTE_PULL => pull(rt, a),
        _ => Err(bad(&format!("nest.cte has no operation {code}"))), // alloc: Cold
    };
    r.unwrap_or_else(|e| {
        gravity_fn::stash_error(e);
        ERROR
    })
}

/// A test's frame over `rec`, its lifetime erased as the runner's publish erases it (gravity_parallel `publish`): the
/// test keeps `rec` alive and unborrowed while the frame is set.
#[cfg(test)]
pub(crate) fn test_frame(rec: &mut dyn gravity_driver::OnDemand) -> Frame {
    // SAFETY: the same pointer with its lifetime erased; the test drops the frame before `rec` goes
    unsafe { core::mem::transmute(core::ptr::NonNull::from(rec)) }
}

/// A test program's pipeline code that does nothing.
#[cfg(test)]
pub(crate) unsafe extern "C-unwind" fn test_noop(_: *mut Ctx) -> i64 {
    0
}
