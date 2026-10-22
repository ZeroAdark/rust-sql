//! The runner's unsafe steps (rule R6, the module's raw home): the plan lifetimes a bind and the simple protocol keep, the
//! sinks' scratch contexts, a run's borrows of its boxed slot and wire, the reads of a node's context block, of a value
//! datum and of a merged TopK run, the late key expressions, and the producers' frame (the runner's registry and the
//! execution a nested run re-enters, D1702 / D1708). Each keeps its contract here.
#![allow(unsafe_code)]

use std::sync::Arc;

use gravity_abi::breaker::TopK;
use gravity_driver::mem::root::Root;
use gravity_driver::{Binding, Exec};
use gravity_hosts::HostRegistry;
use gravity_program::Program;
use mcx::Mcx;
use types_nodes::plannodes::PlannedStmt;

use super::ExecSlot;

/// admission.rs `bind`: the cached plan's first statement, held for the bind.
pub(super) fn pinned_stmt(pstmt: &PlannedStmt<'_>) -> &'static PlannedStmt<'static> {
    // SAFETY: the cplan refcount pins the plan and its stmts past this bind.
    unsafe { &*(pstmt as *const PlannedStmt<'_> as *const PlannedStmt<'static>) }
}

/// execute.rs's simple protocol: the current statement's plan, held for the call.
pub(super) fn current_stmt(pstmt: &PlannedStmt<'_>) -> &'static PlannedStmt<'static> {
    // SAFETY: pstmt outlives this call (it is the current statement's plan).
    unsafe { &*(pstmt as *const PlannedStmt<'_> as *const PlannedStmt<'static>) }
}

/// The runner plan scratch's context (plan.rs `PlanScratch::take`), for the vectors its `Bufs` place.
pub(super) fn scratch_mcx(root: &Root) -> Mcx<'static> {
    // SAFETY: address-stable: `Root` boxes its context, so moving the `PlanScratch` (or the Backend holding it) moves the
    // box, never the context the handle names. The vectors placed through it live in a `Bufs`, which
    // `PlanScratch::give` keeps in the scratch's `bufs` (dropped before its `root`) or drops before the root goes; a
    // plan holding one runs inside its level's borrow (`run_parallel`), and `PlanScratch::drain` reaches idle levels
    // only (`for_each_idle`, `Backend::empty_deep`, the level's own drop).
    unsafe { root.held() }
}

/// A DestSink's context (drive.rs, execute.rs `bounded_step`), `dctx` the root the caller made for it.
pub(super) fn sink_mcx(dctx: &Root) -> Mcx<'static> {
    // SAFETY: `dctx` outlives the sink (same call frame, dropped after end).
    unsafe { dctx.held() }
}

/// execute.rs's simple protocol: the kept statement context, for the objects the call admits into it.
pub(super) fn kept_mcx(m: Mcx<'_>) -> Mcx<'static> {
    // SAFETY: `ctx` outlives every object admitted into it: they live in the call's closure; the guard resets it after.
    unsafe { core::mem::transmute::<Mcx<'_>, Mcx<'static>>(m) }
}

/// execute.rs: a run's borrow of its slot, `sp` the boxed slot.
pub(super) fn run_slot(sp: *mut ExecSlot) -> &'static mut ExecSlot {
    // SAFETY: the slot is boxed and outlives the run; its arrays and snapshot do not move while the adapter is open (the
    // next Append arm reopens the adapter).
    unsafe { &mut *sp }
}

/// execute.rs `bounded_step`: the slot's wire node, `wire` taken from the slot before the run.
pub(super) fn wire<'a>(wire: *mut gravity_wire::WireNode) -> &'a mut gravity_wire::WireNode {
    // SAFETY: `wire` is the slot's node, alive for the run; the adapter never aliases it.
    unsafe { &mut *wire }
}

/// The node context block's resume word (`Exec::node().ctx()`).
pub(super) fn resume(ctx: *const gravity_abi::Ctx) -> i64 {
    // SAFETY: the node's context block lives as long as the node.
    unsafe { (*ctx).resume }
}

/// drive.rs `reevaluate_fault`: a value datum's payload, `len` bytes past its 4-byte header.
pub(super) fn varlena_payload<'a>(datum: u64, len: u32) -> &'a [u8] {
    // SAFETY: a value datum points at a 4B-U varlena image resident with its part; `len` is its payload length.
    unsafe { core::slice::from_raw_parts((datum as *const u8).add(4), len as usize) }
}

/// plan.rs `late_words`: the admission's key expressions.
pub(super) fn late_exprs<'a>(exprs: *mut crate::shapes::KeyExprs) -> &'a mut crate::shapes::KeyExprs {
    // SAFETY: `late_exprs` points at the admission's key expressions (`prepare`'s `a`), which outlive the plan (the run
    // holds the admission); the leader evaluates them as a pipeline starts, when no other borrow of them is live (bind
    // evaluated its own words before the run, an EPQ recheck runs between calls)
    unsafe { &mut *exprs }
}

/// plan.rs: the merged TopK run in the slot region `region`.
pub(super) fn topk_run<'a>(region: *mut u8) -> &'a mut TopK {
    // SAFETY: the slot region is the merged TopK run the kernel laid out (its header says its shape: a ties run has
    // cap > n, its columns need not be the output's).
    unsafe {
        let h = TopK::view(region);
        TopK::at_cap(region, h.n, h.cap, h.natts, h.flags)
    }
}

/// producers.rs (D1708 (a)): the runner's registry, raw below the producers' frame, for one borrow that ends before any
/// nested run can reach it (a section's claims, an EPQ recheck, a fault's reevaluation).
pub(super) fn registry<'a>(derive: *mut HostRegistry) -> &'a mut HostRegistry {
    // SAFETY: `derive` is the runner's own registry (`Producers::run` / `build_wait`'s argument, the frame's pointer),
    // alive across the producers and reached on this thread only; no `&mut` of it is live across the borrow but the ones
    // derived from this pointer.
    unsafe { &mut *derive }
}

/// producers.rs `with_frame`: the program of the adapter's open execution.
pub(super) fn exec_program(exec: *mut Exec) -> Arc<Program> {
    // SAFETY: as `Leader` (the caller's pointers, alive across `f`, used on this thread only).
    unsafe { (*exec).node().program().clone() }
}

/// producers.rs `Leader::at` (D1702): the execution a nested run re-enters: the innermost runner call's (the entry's
/// current, published for that call only), else the frame's own.
pub(super) fn current<'a, 'b>(cell: *const gravity_driver::CurrentCell, entry: *mut gravity_driver::Entry, exec: *mut Exec, binding: *mut Binding<'b>) -> (&'a mut gravity_driver::Entry, &'a mut Exec, &'a mut Binding<'b>) {
    // SAFETY: `cell` is the runner's installed slot: `with_current_slot` takes it off and the cell drops after `_frame`
    // unpublishes the frame, so the cell outlives every call through the frame. The pointers are the runner's own objects,
    // alive while the frame is published (`_frame` drops first); the current is the innermost call's own, live across that
    // call, inside which the nested run runs. This thread only.
    unsafe {
        let (e, x, b) = gravity_driver::current_or(cell, entry, exec, binding);
        (&mut *e, &mut *x, &mut *b)
    }
}

/// producers.rs `emit_image`: the binding's words pointed at the image's buffers `p` / `n`.
pub(super) fn lay_image(binding: &mut Binding<'_>, p: &[u64], n: &[u8]) {
    // SAFETY: the image's buffers live in the returned EmitImage, which the caller keeps until the adapter (the binding and
    // the leader's context that point at them) has closed; nothing writes them after this point.
    unsafe {
        binding.parts.params = &*(p as *const [u64]);
        binding.parts.param_nulls = &*(n as *const [u8]);
    }
}
