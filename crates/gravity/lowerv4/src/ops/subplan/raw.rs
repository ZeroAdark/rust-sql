//! nest.subplan's raw half: the lent entry a SubPlan step hands its readers through `Env::subplans` as an address
//! (site.rs). Owner subplan.
#![allow(unsafe_code)]
use crate::ops::subplan::site::Lent;

/// The entry at `at`: `site::lend`'s `lent`, on its frame while `next` emits the readers, `site::relend`'s, on its frame
/// while `rest` emits the redo's readers (D3451), or an `Early`'s cell, set by a step the function's lowering holds.
pub(crate) fn lent_at<'a>(at: usize) -> Option<Lent<'a>> {
    // SAFETY: `at` is only ever one of the three addresses above (site.rs pushes no other), alive while its readers lower
    unsafe { *(at as *const Option<Lent<'a>>) }
}

/// An `Early` cell's entry: only the lifetime changes; it is read (`site::eval_at`) only while the function lowers, the
/// step alive.
pub(crate) fn erase(l: Lent<'_>) -> Lent<'static> {
    // SAFETY: as above; `Lent` holds only references, so its layout does not depend on the lifetime
    unsafe { core::mem::transmute::<Lent<'_>, Lent<'static>>(l) }
}

/// D3506 EXISTS SORT (rerun/epq/sub_lock.rs `redo_own`): the SubPlan step lent at `at`, a `&dyn StepMech` on
/// `SortClaim::lower`'s frame while the claim's lock it wraps lowers (its recheck's redo included).
pub(crate) fn step_at<'a>(at: usize) -> &'a dyn crate::face::step::StepMech {
    // SAFETY: `at` is only ever a `SortClaim::lower` entry (sub_lock.rs pushes no other), alive while that lock lowers
    unsafe { *(at as *const &'a dyn crate::face::step::StepMech) }
}
