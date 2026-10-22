//! The broker view functions' one unsafe step (the hosts' raw-module convention).
#![allow(unsafe_code)]

/// The call's result context, detached from `fcinfo`'s borrow so the materialized SRF holds it while `fcinfo` is passed on
/// (the view functions only: the executor arms the context before the call and it outlives the call).
pub(super) fn result_mcx<'a>(fcinfo: &types_fmgr::FunctionCallInfoBaseData) -> mcx::Mcx<'a> {
    // SAFETY: the executor arms es_query_cxt before the call; it outlives the view function's frame, the only user.
    unsafe { fcinfo.result_mcx_detached() }
}
