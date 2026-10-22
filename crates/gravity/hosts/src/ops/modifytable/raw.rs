//! The modifytable family's raw home (R6): an `unsafe` the family's host files need, behind a safe function whose body
//! keeps its contract (the agg / window families' raw.rs pattern).
#![allow(unsafe_code)]

use core::ptr::NonNull;

use datum::Datum;
use execexpr::{AggBind, ExprState};
use mcx::{Mcx, MemoryContext};
use types_core::Oid;
use types_error::{PgError, PgResult};
use types_nodes::primnodes::SubPlan;
use types_nodes::Node;
use types_tuple::{HeapTupleData, ItemPointerData};

/// W6-D1112: `f` over `st` with the by-reference results of its calls allocated in `row` (the row's context: a check
/// option SubPlan's left side laid into the parked call outlives this call but not the row), its frames armed again with
/// `bind` (the context the state was compiled in, which outlives it) before returning, as write_check.rs's qual is.
pub(crate) fn eval_in_row<'m, R>(st: &mut ExprState<'m>, row: Mcx<'_>, bind: Mcx<'m>, f: impl FnOnce(&mut ExprState<'m>) -> R) -> R {
    // SAFETY: the state evaluates only inside `f`, while `row` is borrowed (its context alive and address-stable for the
    // call); the frames point at `bind` again before any later evaluation
    unsafe { st.arm_result_mcx_raw(row) };
    let r = f(st);
    st.arm_result_mcx(bind);
    r
}

/// The result frames of `st` armed with `row` (the by-reference results of a check
/// qual allocated in the row's context); the caller arms them with the state's own context again before the state
/// evaluates outside the row (write_check.rs `run`: only the qual's Boolean escapes, write_constraints' discipline).
pub(crate) fn arm_row(st: &mut ExprState<'_>, row: Mcx<'_>) {
    // SAFETY: the caller's contract above: the frames point at `row` only while the row's context is alive
    unsafe { st.arm_result_mcx_raw(row) };
}

/// Word `i` of a binding's words `ptr .. len` (write_check.rs `ParamWords`: alive while the host is bound, and every check
/// runs between bind and unbind), none past `len`.
pub(crate) fn param_word(ptr: *const u64, len: usize, i: usize) -> Option<u64> {
    // SAFETY: the caller's contract above: `ptr .. len` is the bound binding's words
    (i < len).then(|| unsafe { *ptr.add(i) })
}

/// write_check.rs `InitParams::fetch`: ExecSetParamPlan's run of the InitPlan behind Init cell `cell` for `paramid`
/// (nest.initplan's `m8_init_run`): the param's (value, null) words, none when it failed (its error is pending). `cell` is
/// the Init cell exec's prepare laid for this execution (`m8_init_run` checks its magic first); the cell's operation reads
/// its own state, never the context, and runs the members through the leader's frame (the write's pipeline is serial:
/// decompose `serial_after(.., Reason::Write)`).
pub(crate) fn init_pair(cell: u64, paramid: i32) -> Option<(u64, u64)> {
    // SAFETY: the caller's contract above
    let pair = unsafe { gravity_kernels::ops::shared::object::entries::m8_init_run(core::ptr::null_mut(), cell, paramid as u32 as u64) };
    // SAFETY: a nonnegative return is the address of the param's pair: the value word, then the null word
    (pair >= 0).then(|| unsafe { (*(pair as *const u64), *(pair as *const u64).add(1)) })
}

/// D1112 (write_check.rs): a check SubPlan's cookie is its own node (`CheckSpec::node` keeps the plan alive while the
/// states live); `SuspendKind::SubPlan` hands it back at the read (`cookie_sub_plan`).
pub(crate) unsafe fn sub_cookie(_: NonNull<()>, n: Node<'_>, _: Option<AggBind>, _: Option<execexpr::MergeActionCell>) -> PgResult<NonNull<()>> {
    match n.as_sub_plan() {
        Some(sp) => Ok(NonNull::from(sp).cast()),
        None => Err(Box::new(PgError::error("gravity: a check option SubPlan cookie over another node"))), // alloc: Cold
    }
}

/// D1112: the SubPlan node behind a cookie `sub_cookie` made: a SubPlan node of a spec's qual, alive while the specs are.
pub(crate) fn cookie_sub_plan(cookie: NonNull<()>) -> &'static SubPlan<'static> {
    // SAFETY: the caller's contract above
    unsafe { cookie.cast::<SubPlan<'static>>().as_ref() }
}

/// index_maint.rs `cache_mcx`: the index AM cache context with its lifetime extended. The AMs' states (`am_cache`) drop
/// at `unbind` before the reset; the context is the last field of its IndexMaint to drop, and it is boxed, so a move of
/// the IndexMaint (its host's) leaves the context where the handed-out `Mcx`s point.
pub(crate) fn cache_mcx(cache: &MemoryContext) -> Mcx<'static> {
    // SAFETY: the caller's contract above
    unsafe { core::mem::transmute::<Mcx<'_>, Mcx<'static>>(cache.mcx()) }
}

/// write_view_rows.rs `write`: the wholerow cell `d`'s flat composite image (detoasted into `mcx` when the cell is not an
/// aligned flat image) and its datum length. `d` is a non-null wholerow cell of the view row op's site: a live composite
/// image for the call.
pub(crate) fn wholerow_image(mcx: Mcx<'_>, d: Datum) -> PgResult<(*const u8, u32)> {
    // SAFETY: the caller's contract above
    let hdr = unsafe { exectuples::datum_get_heap_tuple_header(mcx, d)? };
    // SAFETY: an aligned flat composite image readable for its datum length (datum_get_heap_tuple_header's)
    Ok((hdr.cast::<u8>(), unsafe { (*hdr).datum_length() }))
}

/// write_view_rows.rs: a heap tuple over `img`, `len` bytes, at `tid` of table `toid`. The callers' images: a wholerow
/// cell's flat image (`wholerow_image`, live for the row op's call), a slot's materialized image or a row-context copy,
/// each MAXALIGNed, readable for `len` bytes and written by no one while the tuple is read (the trigger runs).
pub(crate) fn tuple_at<'t>(img: *const u8, len: u32, tid: ItemPointerData, toid: Oid) -> HeapTupleData<'t> {
    // SAFETY: the caller's contract above (HeapTupleData::from_raw_parts's)
    unsafe { HeapTupleData::from_raw_parts(img, len, tid, toid) }
}

/// write_view_rows.rs `update`: the tuple an INSTEAD OF trigger returned (ExecCallTriggerFunc's answer, a tuple live in
/// the per-call row context while the caller copies it).
pub(crate) fn returned_tuple<'r, 't>(p: NonNull<HeapTupleData<'t>>) -> &'r HeapTupleData<'t> {
    // SAFETY: the caller's contract above
    unsafe { p.as_ref() }
}

/// A heap tuple's image bytes: `t_len` bytes at its header (HeapTupleData::from_raw_parts's contract: readable for `t_len`).
pub(crate) fn tuple_bytes<'a>(t: &'a HeapTupleData<'_>) -> &'a [u8] {
    // SAFETY: the tuple's own contract above
    unsafe { core::slice::from_raw_parts(t.header_ptr(), t.t_len as usize) }
}
