//! C6 (d): Gravity's one interrupt facade. A Gravity check for a pending cancel, statement_timeout or terminate used to
//! be spelled five ways: PostgreSQL's CHECK_FOR_INTERRUPTS seam called directly (with or without its own
//! `InterruptPending` test first), the hosts' `AbortByte` check, the compile's stop points and the probe they read, the
//! compile wait's 10 ms slices, and the kernels' back-edge poll. Each interrupt fix added a site of its own (D2909,
//! D3552, D3575, D3610, D3764, D3811, D3837, D3887). Every one now asks here:
//! - [`check`]: C's CHECK_FOR_INTERRUPTS (postgres.c), at a point that can raise. The pending interrupt is processed as
//!   PostgreSQL processes it, and its own error comes back (a cancel's or a statement_timeout's 57014, a terminate's
//!   FATAL). Ok where no backend is installed (a unit test).
//! - [`check_abort`]: the same restricted to the abort class (ProcessAbortInterrupts): a run-ahead reader's start.
//! - [`pending`]: an interrupt the backend can take now is pending (INTERRUPT_PENDING and INTERRUPTS_CAN_BE_PROCESSED),
//!   read only. The caller stops and then raises through [`check`]: a lowering at its next cancel point refuses
//!   (lowerv4 `refusal::check_interrupts`), a compile at its next stop point unwinds (gravity_tpde `stop_point`), a
//!   compile wait ends its slice (gravity_backend `Flight::wait`). Each calls the facade itself: nothing is installed
//!   into the compiler (the probe word and the host check it used to take are gone).
//! - [`pending_abort`]: an abort-class interrupt the backend can take now is pending (INTERRUPTS_CAN_BE_PROCESSED and
//!   AbortPending), read only; [`flagged`]: INTERRUPT_PENDING alone, read only. The grant pool's wait (exec knobs
//!   `admit_waiting`) wakes on them.
//!
//! Generated code polls the statement's abort byte at each loop back-edge (lowerv4 `frame/cancel.rs`). A set byte calls
//! the kernel poll (gravity exec glue `poll_kernel`), which runs [`check`]. A long host loop inside a kernel call asks
//! [`crate::canceled`].
//!
//! RI (crates/gravity/lint/rules/ri_interrupts.py) fails Gravity code that calls PostgreSQL's interrupt seams, or reads
//! its interrupt globals, itself. lint/INTERRUPT-DEBT lists the sites not moved yet.
use types_error::PgResult;

/// C's CHECK_FOR_INTERRUPTS: process a pending interrupt, which raises its error. Ok when none is pending, or when no
/// backend is installed (a unit test). It is a poll: the seam's body (tcop postgres `check_for_interrupts`) marks the
/// poll-gap probe (`crate::polled`) at every CHECK_FOR_INTERRUPTS that could service a cancel (no interrupt or
/// query-cancel holdoff, no critical section), this one and PostgreSQL's own (D3962).
/// K11 (D4214 / D4229): #[inline(always)] #[track_caller], and the seam is tracked (seam_core::seam_tracked!), so the
/// probe's poll site is this call's own line (before: tcop postgres lib.rs:312, the seam body's definition, for every one).
#[inline(always)]
#[track_caller]
pub fn check() -> PgResult<()> {
    if postgres_seams::check_for_interrupts::is_installed() {
        return postgres_seams::check_for_interrupts::call();
    }
    Ok(())
}

/// [`check`] restricted to the abort class (ProcessAbortInterrupts): a cancel, a timeout or a terminate raises.
/// Deferrable interrupts stay pending. A poll, as [`check`] (the seam's body marks the probe, D3962). K11: the caller's
/// line, as [`check`].
#[inline(always)]
#[track_caller]
pub fn check_abort() -> PgResult<()> {
    if postgres_seams::check_for_abort_interrupts::is_installed() {
        return postgres_seams::check_for_abort_interrupts::call();
    }
    Ok(())
}

/// A backend's CHECK_FOR_INTERRUPTS is installed: false in a test without the host, where [`check`] raises nothing and
/// a caller that stopped for an interrupt keeps its own answer (gravity_backend cache `take_interrupt`).
#[inline]
pub fn installed() -> bool {
    postgres_seams::check_for_interrupts::is_installed()
}

/// An interrupt the backend can take now is pending; read only (nothing is processed or cleared). false when no backend
/// is installed.
#[inline]
pub fn pending() -> bool {
    postgres_seams::interrupts_pending::is_installed() && postgres_seams::interrupts_pending::call()
}

/// An abort-class interrupt (a cancel, a statement_timeout, a terminate) the backend can take now is pending
/// (INTERRUPTS_CAN_BE_PROCESSED and AbortPending); read only. false when no backend is installed.
#[inline]
pub fn pending_abort() -> bool {
    postgres_seams::abort_pending::is_installed() && postgres_seams::abort_pending::call()
}

/// INTERRUPT_PENDING is set, whether or not the backend can take it now; read only. false when no backend is installed.
/// A wait that checked and found the flag still pending reads it to wait on for the abort class only (exec knobs
/// `admit_waiting`, D355).
#[inline]
pub fn flagged() -> bool {
    postgres_seams::interrupt_flagged::is_installed() && postgres_seams::interrupt_flagged::call()
}
