//! The pool's unsafe steps (rule R6, its raw home): the Send / Sync promises of a section's shared state and of the
//! spill files, the section's borrowed job and claims (their lifetimes erased for the runtime's task set, valid until
//! `run_led` returns), and a spill run's reader over the context it owns.
#![allow(unsafe_code)]

use gravity_driver::mem::root::Root;
use gravity_hosts::stash;
use gravity_parallel::{Job, MorselSource};

use crate::{File, Reader, Units, Work};

// SAFETY: `claims` names the section's cursor, a `MorselSource` (Sync), valid until `run_led` returns after the last claim.
unsafe impl Send for Units {}
unsafe impl Sync for Units {}

// SAFETY: `job` and `claims` name the section's Sync job and cursor and `order` its ordered window, each valid until
// `run_led` returns after the last claim.
unsafe impl Send for Work {}
unsafe impl Sync for Work {}

// SAFETY: a file is written by its owning thread alone (`SpillTable`'s law) and read through
// `spillset`'s shared reader with a context per read; the writer's context is the owner's only.
unsafe impl Send for File {}
unsafe impl Sync for File {}

// SAFETY: a reader is driven by the one thread that opened it; its context is touched by that thread only.
unsafe impl Send for Reader {}

/// The section's job with its lifetime erased: `run_led` returns only after the section's last claim, so the job it
/// borrowed outlives every call through the pointer.
pub(crate) fn erase_job(job: &Job<'_>) -> *const Job<'static> {
    unsafe { core::mem::transmute::<*const Job<'_>, *const Job<'static>>(job as *const Job<'_>) }
}

/// The section's claims with their lifetime erased, under the same bound as `erase_job`.
pub(crate) fn erase_claims(claims: &dyn MorselSource) -> *const (dyn MorselSource + 'static) {
    unsafe { core::mem::transmute::<*const (dyn MorselSource + '_), *const (dyn MorselSource + 'static)>(claims as *const dyn MorselSource) }
}

impl Units {
    /// The cursor `run_led` borrowed (`erase_claims`); the runtime asks for it only while the section runs.
    pub(crate) fn claims(&self) -> &dyn MorselSource {
        unsafe { &*self.claims }
    }
}

impl Work {
    /// The section's job (`erase_job`); a claim runs it only inside the section.
    pub(crate) fn job(&self) -> &Job<'static> {
        unsafe { &*self.job }
    }

    /// The section's claims (`erase_claims`).
    pub(crate) fn claims(&self) -> &dyn MorselSource {
        unsafe { &*self.claims }
    }

    pub(crate) fn order(&self) -> Option<&gravity_driver::ClaimOrder> {
        // SAFETY: null or the `run_leading` caller's order, which outlives the section (`run_led` returns after the last claim).
        unsafe { self.order.as_ref() }
    }
}

impl Reader {
    /// A pull reader over `part` of `f`, with a context of its own for the reader's buffers; `None` when the part is empty.
    pub(crate) fn open(f: &spillset::SpillFile, part: u32) -> gravity_driver::Result<Option<Reader>> {
        // a Root is boxed, so its handle stays put when the root moves into the reader (a context moved after the
        // borrow left the reader's buffers owned by a dead stack slot: the leader's SIGSEGV at 0x60 closing a run's BufFile)
        let ctx = Root::new("GravitySpillRun");
        // SAFETY: the reader lives beside the root that owns its buffers and is dropped first.
        let mcx: mcx::Mcx<'static> = unsafe { ctx.held() };
        let Some(r) = f.read_part(mcx, part).map_err(stash)? else { return Ok(None) };
        Ok(Some(Reader { r: Some(r), _ctx: ctx }))
    }

    /// `open` from byte `offset` of `part` on (D3190: a put sort run's fence); `None` when no byte lies past it.
    pub(crate) fn open_at(f: &spillset::SpillFile, part: u32, offset: u64) -> gravity_driver::Result<Option<Reader>> {
        // the root boxed and held as in `open`
        let ctx = Root::new("GravitySpillRun");
        // SAFETY: the reader lives beside the root that owns its buffers and is dropped first.
        let mcx: mcx::Mcx<'static> = unsafe { ctx.held() };
        let Some(r) = f.read_part_at(mcx, part, offset).map_err(stash)? else { return Ok(None) };
        Ok(Some(Reader { r: Some(r), _ctx: ctx }))
    }
}
