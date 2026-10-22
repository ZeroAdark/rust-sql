//! `index_ranges`' unsafe steps (rule R6, the module's raw home): the filed image widened to `'static` for the view that
//! holds it, and a separator's first key column.
#![allow(unsafe_code)]

use super::Filed;

/// `filed`'s image for the life of the `View` that also holds `filed` (an `Arc`, so the image never moves) and drops its
/// `ImageRel` and scan first (`View`'s field order).
pub(super) fn image(filed: &std::sync::Arc<Filed>) -> &'static nbtree::IndexImage {
    // SAFETY: the caller's contract above: the Arc in the same View outlives every borrow made through this reference.
    unsafe { &*std::sync::Arc::as_ptr(&filed.img) }
}

/// A copy of separator `key` (a pivot tuple's bytes, `nbtree::bt_pivots`) at a word-aligned address, as a page holds it:
/// its attributes read in place (`first_column`), a by-reference bound pointing into it.
pub(super) fn aligned(key: &[u8]) -> Vec<u64> {
    let mut buf = vec![0u64; key.len().div_ceil(8)]; // alloc: Exec
    // SAFETY: `buf` holds at least `key.len()` bytes and does not overlap `key`
    unsafe { core::ptr::copy_nonoverlapping(key.as_ptr(), buf.as_mut_ptr().cast::<u8>(), key.len()) };
    buf
}

/// Separator `key`'s (`aligned`) first key column under the index's descriptor `desc` (`indnkeyatts` key columns): `None`
/// when it is NULL or the separator keeps no key column (a minus-infinity pivot). A by-reference value points into `key`.
pub(super) fn first_column(key: &[u64], desc: &types_tuple::TupleDescData<'_>, indnkeyatts: i32) -> Option<datum::Datum> {
    let itup = key.as_ptr().cast::<u8>();
    // SAFETY: `key` holds a whole index tuple (a pivot's copy, its size its own header's), read while `key` lives
    if key.is_empty() || unsafe { nbtree::itup::bt_tuple_get_natts(itup, indnkeyatts) } < 1 {
        return None;
    }
    let mut null = false;
    // SAFETY: as above; column 1 is within the tuple's kept key columns
    let d = unsafe { nbtree::itup::index_getattr(itup, 1, desc, &mut null) };
    (!null).then_some(d)
}
