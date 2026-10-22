//! `entry`'s unsafe steps (rule R6, the module's raw home): the outer call's out block set aside around a nested run,
//! the current execution published across a call (D1702) with the node's binding attached for it (D1706), and the
//! leader ctx's sites.
#![allow(unsafe_code)]

use gravity_abi::PtrWord;
use super::*;

impl OutRows {
    pub(super) fn hold(exec: &mut Exec) -> OutRows {
        // SAFETY: the node's own context and out block (`Node::build` points `out` at its Inner's), alive while the node is
        // bound; the leader's thread is the only one writing them here, between the outer call's rows.
        unsafe {
            let o = (*exec.node.ctx()).out.ptr();
            let held = OutRows((*o).count, (*o).region_len);
            ((*o).count, (*o).region_len) = (0, 0);
            held
        }
    }

    pub(super) fn put_back(self, exec: &mut Exec) -> Result<()> {
        // SAFETY: as `hold`.
        let left = unsafe {
            let o = (*exec.node.ctx()).out.ptr();
            let left = (*o).count;
            ((*o).count, (*o).region_len) = (self.0, self.1);
            left
        };
        match left {
            0 => Ok(()),
            n => Err(Error::Refused(format!("gravity: Bug: a nested run left {n} rows in the outer call's out block (D1876)").into())), // alloc: Cold
        }
    }
}

/// D1702: the execution a frame's nested run re-enters: `cell`'s current, else the frame's own pointers (no call on the
/// stack: a statement-end drain).
/// # Safety
/// `cell` is null or a live installed cell.
pub unsafe fn current_or<'b>(cell: *const CurrentCell, entry: *mut Entry, exec: *mut Exec, binding: *mut Binding<'b>) -> (*mut Entry, *mut Exec, *mut Binding<'b>) {
    match cell.as_ref().and_then(|c| c.get()) {
        Some(c) => (c.entry, c.exec, c.binding.cast()),
        None => (entry, exec, binding),
    }
}

impl Drop for PutBack {
    fn drop(&mut self) {
        // SAFETY: the installed cell `publish_current` read, alive while installed
        unsafe { (*self.0).set(self.1) }
    }
}

impl Entry {
    /// D1702: `f` (a call a host may re-enter the runner from: a late step's `Plan::late_words`) with this entry, `exec` and
    /// `binding` the current execution, the one before back after it.
    pub fn with_current<R>(&mut self, exec: &mut Exec, binding: &mut Binding<'_>, f: impl FnOnce() -> R) -> R {
        // SAFETY: this call's own arguments, alive across `f`, which reaches them only through the cell
        unsafe { Self::publish_current(self, exec, binding, f) }
    }

    /// # Safety
    /// `this`, `exec` and `binding` are valid, and no borrow of them is live across `f` but the caller's.
    pub(super) unsafe fn publish_current<R>(this: *mut Entry, exec: *mut Exec, binding: *mut Binding<'_>, f: impl FnOnce() -> R) -> R {
        let cell = (*this).current;
        if cell.is_null() {
            return f();
        }
        let _back = PutBack(cell, (*cell).replace(Some(Current { entry: this, exec, binding: binding.cast() })));
        f()
    }
}

/// `run_pipeline`'s call of pipeline `p` over `xp`'s node, `bp` attached and the three published current, both put back after.
pub(super) fn call_pipeline_published(this: *mut Entry, xp: *mut Exec, bp: *mut Binding<'_>, p: usize, call: Call) -> Result<Ret> {
    // SAFETY: D1702: the node's call over this call's own execution, published current: a nested run re-enters
    // through it, and no borrow of the entry, the execution or its node is live across the call. D1706: the node's
    // attached binding is this call's own pointer (a nested run's writes are below it), the one before back after it
    unsafe {
        let _back = Node::attach_raw(core::ptr::addr_of_mut!(*(*xp).node), bp);
        Entry::publish_current(this, xp, bp, || Node::call_pipeline_raw(core::ptr::addr_of_mut!(*(*xp).node), p, call))
    }
}

/// A claim's call of pipeline `p` over `xp`'s node, attached and published as `call_pipeline_published`.
pub(super) fn call_claim_published(this: *mut Entry, xp: *mut Exec, bp: *mut Binding<'_>, p: usize, claim: MorselRange, resume: bool) -> Result<Ret> {
    // SAFETY: as `run_pipeline`'s call (D1702, D1706)
    unsafe {
        let _back = Node::attach_raw(core::ptr::addr_of_mut!(*(*xp).node), bp);
        Entry::publish_current(this, xp, bp, || Node::call_claim_at_raw(core::ptr::addr_of_mut!(*(*xp).node), p, claim, resume))
    }
}

/// A rerun's stopped pipeline called over `xp`'s node: published current first, then `bp` attached inside the call (D1706).
/// `call`: a batch, or the resume after a lazy build's wait (D2033).
pub(super) fn call_stopped_published(ep: *mut Entry, xp: *mut Exec, bp: *mut Binding<'_>, stopped: usize, call: Call) -> Result<Ret> {
    // SAFETY: this call's own entry, execution and binding, alive across the call; no borrow of them is live across it
    // but the published pointers a nested run derives from
    unsafe {
        Entry::publish_current(ep, xp, bp, || {
            let _back = Node::attach_raw(core::ptr::addr_of_mut!(*(*xp).node), bp);
            (*xp).node.call_pipeline(stopped, call)
        })
    }
}

/// The node ctx's resume word (`resume::resumable`).
pub(super) fn resume_of(ctx: *mut gravity_abi::Ctx) -> i64 {
    // SAFETY: the node's own ctx, bound while the node is.
    unsafe { (*ctx).resume }
}

/// The produced slot's region read as the one partial, apart from the node's borrow (the one-owner fold).
pub(super) fn slot_bytes<'a>(src: *const u8, len: usize) -> &'a [u8] {
    // SAFETY: the slot's region, `len` bytes, idle while the owner folds it on the leader thread.
    unsafe { core::slice::from_raw_parts(src, len) }
}

/// C10 2b (D3582): the produced slot's region apart from the node's borrow, for a batched slot's header laid beside its
/// spill table (`Entry::own_batched_of`).
pub(super) fn slot_bytes_mut<'a>(r: &mut [u8]) -> &'a mut [u8] {
    // SAFETY: the slot's region, idle past its producer on the leader thread; the node outlives the lay that writes it.
    unsafe { core::slice::from_raw_parts_mut(r.as_mut_ptr(), r.len()) }
}

/// Site `i` of the leader's ctx.
pub(super) fn site_of(ctx: *mut gravity_abi::Ctx, i: usize) -> *mut gravity_abi::Site {
    // SAFETY: the leader's ctx, bound; `i` is one of the program's sites.
    unsafe { gravity_abi::ctx::site_ptr(ctx, i) }
}

/// Site `i` of the leader's ctx, to write its state (the leader thread, between pipelines).
pub(super) fn site_mut<'a>(ctx: *mut gravity_abi::Ctx, i: usize) -> &'a mut gravity_abi::Site {
    // SAFETY: `i` is one of the program's sites, laid out in the Node's ctx; the leader thread, between pipelines.
    unsafe { &mut *gravity_abi::ctx::site_ptr(ctx, i) }
}

/// The source at site `i` of the leader's ctx.
pub(super) fn source_of<'a>(ctx: *mut gravity_abi::Ctx, i: usize) -> Option<&'a (dyn crate::source::Source + 'static)> {
    // SAFETY: the leader's ctx, bound; its claims are over, so nothing else holds the site's source.
    unsafe { crate::source::site_source(ctx, i) }
}
