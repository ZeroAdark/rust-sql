//! `ops::srf`'s only `unsafe` in exec (PLAN-v5 §3.2 R6): the step's end reaching its fmgr sets through the handle the
//! program reads (proto.rs `Sets::fmgr`).
#![allow(unsafe_code)]
use gravity_fn::srf_fmgr::Sets as FmgrSets;

/// The fmgr sets behind handle `h` (proto.rs `Sets::fmgr`, which points into the `Sets`' own `_host` box); None for a
/// store scan's null handle.
pub(super) fn fmgr_sets<'a>(h: *mut FmgrSets) -> Option<&'a mut FmgrSets> {
    // SAFETY: the handle points into `_host`'s box, alive with this state; no program runs at the end.
    unsafe { h.as_mut() }
}
