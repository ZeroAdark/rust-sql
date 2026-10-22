//! heap_fetch's unsafe (lint R6): a fetched varlena's whole image, and the arena's words as bytes.
#![allow(unsafe_code)]

/// A live varlena's whole image (`varsize_any` bytes: a toast pointer's or a compressed image's length is its header's).
/// The caller passes a non-null varlena datum of the slot's current tuple, before the slot moves on.
pub(super) fn varlena_bytes<'a>(p: *const u8) -> &'a [u8] {
    // SAFETY: the caller's contract above.
    unsafe { core::slice::from_raw_parts(p, types_tuple::varatt::varsize_any(p)) }
}

pub(super) fn bytes_mut(words: &mut [u64]) -> &mut [u8] {
    // SAFETY: the same allocation as bytes; u8 has no alignment and every bit pattern is a valid u8.
    unsafe { core::slice::from_raw_parts_mut(words.as_mut_ptr() as *mut u8, words.len() * 8) }
}
