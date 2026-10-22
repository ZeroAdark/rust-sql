//! hosted_bind's unsafe (R6 home; its child): the carrier an FmgrInfo reads, widened to its box's life, and the
//! by-reference initcond / direct-argument copies into the arena's init context. Moved from hosted_bind.rs unchanged.
#![allow(unsafe_code)]
use datum::Datum;
use types_core::fmgr::{AggFnArgTypes, FnExprErased};
use types_core::Oid;
use types_error::PgResult;

/// `types` as `'static`. memsig E-pub t3: the slice is in the arena's init context (`AggArena.keep`), which drops after
/// the arena's slots (declared after `slots`; in `AggArena::new`, a local declared before them), so it outlives every
/// FmgrInfo of the slots' calls reading it.
pub(super) fn static_oids(types: &[Oid]) -> &'static [Oid] {
    // SAFETY: the caller's contract above: the slice lives in the arena's init context as long as the FmgrInfo reading it.
    unsafe { &*(types as *const [Oid]) }
}

/// `carrier` as an FmgrInfo's `fn_expr`. The caller keeps the carrier's box alive as long as the FmgrInfo.
pub(super) fn fn_expr(carrier: &AggFnArgTypes) -> FnExprErased {
    // SAFETY: the caller's contract above: the carrier's box outlives the FmgrInfo.
    unsafe { FnExprErased::from_node_ref(carrier) }
}

/// execexpr's datumCopy (GetAggInitVal's) of the non-NULL by-reference datum `v` of length `len` into `mcx`. The caller
/// passes a live datum: an input function's result while its FmgrInfo lives (D1280), or a constant of the live plan.
pub(super) fn datum_copy(mcx: mcx::Mcx<'_>, v: Datum, len: i16) -> PgResult<Datum> {
    // SAFETY: the caller's contract above.
    unsafe { execexpr::agg_datum_copy(mcx, v, len) }
}
