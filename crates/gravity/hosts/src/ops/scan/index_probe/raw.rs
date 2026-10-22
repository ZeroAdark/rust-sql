//! `index_probe`'s unsafe step (rule R6, the module's raw home): the filed image widened to `'static` for the view that
//! holds it.
#![allow(unsafe_code)]

use super::Filed;

/// `filed`'s image for the life of the `View` that also holds `filed` (an `Arc`, so the image never moves) and drops its
/// `ImageRel` and scan first (`View`'s field order).
pub(super) fn image(filed: &std::sync::Arc<Filed>) -> &'static nbtree::IndexImage {
    // SAFETY: the caller's contract above: the Arc in the same View outlives every borrow made through this reference.
    unsafe { &*std::sync::Arc::as_ptr(&filed.img) }
}
