//! hosted_catalog's unsafe (R6 home; its child): the arena's armed catalog slices widened to its statement's life. Moved
//! from hosted_catalog.rs unchanged.
#![allow(unsafe_code)]

/// `s` as `'static`. The caller is `CatalogArm::arm`: the arena owns the slices and outlives every claim of its statement
/// (each claim's guard disarms first).
pub(super) fn statement_slice<T: 'static>(s: &[T]) -> &'static [T] {
    // SAFETY: the caller's contract above.
    unsafe { &*(s as *const [T]) }
}
