//! exec::rerun::epq's raw half (rule R6: the module's unsafe code lives here): the node context block's read after a
//! recheck (`recheck`). Owner lockrows.
#![allow(unsafe_code)]

/// Whether the node context block `ctx` asks the statement to resume (`resume != RESUME_NONE`) once the recheck ran.
pub(super) fn resumes(ctx: *mut gravity_abi::Ctx) -> bool {
    // SAFETY: the node's context block lives as long as the node (`Exec::node().ctx()`, read while the node runs).
    unsafe { (*ctx).resume != gravity_abi::ctx::RESUME_NONE }
}
