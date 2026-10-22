//! `glue`'s unsafe step (rule R6, the module's raw home): the abort flag read as the byte it is, and the kernels' poll
//! reading a ctx's byte.
#![allow(unsafe_code)]

use core::sync::atomic::AtomicU8;

use gravity_abi::PtrWord;

pub fn abort_u8() -> &'static AtomicU8 {
    init_small::globals::abort_pending_flag()
}

/// D3406: the kernels' poll (gravity_abi `ctx::set_poll`), on a kernel's set byte. The byte this thread owns (the
/// statement's own run): the abort bit stops at once (the runner's abort-class check raises it); the service bit is C's
/// CHECK_FOR_INTERRUPTS here, inside the kernel, as a busy C backend runs it (a ProcSignalBarrier absorbed, the memory
/// contexts logged, a recovery conflict raised), and the kernel goes on unless it raised (the error is the runner's to
/// report, `super::take_polled`). A byte another thread owns (a pool worker reading its leader's): the abort bit alone;
/// the leader services its own. C5: the revoke bit, on any participant (no abort pending): the region this participant
/// fills is cut once per ask (gravity_driver `mem::grow::revoke_poll`), and the kernel goes on; the bit stays set until
/// the statement's end, so every participant's next poll sees it.
/// # Safety
/// `ctx` is a bound ctx whose `cancel` is a live abort byte (`set_poll`'s contract).
pub unsafe fn poll_kernel(ctx: *mut gravity_abi::Ctx) -> bool {
    let own = abort_u8();
    // SAFETY: the caller's contract
    let byte = unsafe { &*((*ctx).cancel.ptr() as *const AtomicU8) };
    // D3887: the poll-gap probe's mark (off: one load); the probe's bit alone is no interrupt
    cancelpoll::polled();
    let v = byte.load(core::sync::atomic::Ordering::Acquire);
    // C5: the revoke bit, on any participant (no abort pending): this participant's region cut once per ask (its
    // generation), the kernel goes on. The bit stays set until the statement ends (mcx::revoke::clear_own_bit): the
    // participants share the byte, and a first poller's clear left every other worker uncut until its next claim
    if v & gravity_abi::ctx::REVOKE_BIT != 0 && v & gravity_abi::ctx::ABORT_BIT == 0 {
        // SAFETY: the caller's contract: `ctx` is bound, on the thread running its call
        unsafe { gravity_driver::mem::grow::revoke_poll(ctx) };
    }
    if v & (gravity_abi::ctx::ABORT_BIT | gravity_abi::ctx::SERVICE_BIT) == 0 {
        return false;
    }
    if !core::ptr::eq(byte, own) || gravity_abi::ctx::abort_set(own) {
        return gravity_abi::ctx::abort_set(byte);
    }
    super::service_interrupts(own)
}
