//! index_source's unsafe (lint R6), each with its safety contract, moved as is from index_source.rs: `plain_image` and
//! `varlena_image` whole, and one-call wrappers for a search array's image reads whose callers keep their reasons beside
//! the call.
#![allow(unsafe_code)]
use super::*;

/// A live varlena's whole image (`varsize_any` bytes: a toast pointer's or a compressed image's length is its header's).
/// The caller passes the address of a live varlena header (a non-null array word: the bind's parameter or the program's
/// value).
pub(super) fn varlena_bytes<'a>(p: *const u8) -> &'a [u8] {
    // SAFETY: the caller's contract above.
    unsafe { core::slice::from_raw_parts(p, types_tuple::varatt::varsize_any(p)) }
}

/// An array image with a 4-byte header (a short-header image is re-headed, as nbtree's key image is). The caller passes a
/// live array image (the recipe's Const image or the seek's plain image).
pub(super) fn array_image(p: *const u8) -> Vec<u8> {
    // SAFETY: the caller's contract above.
    unsafe {
        let b0 = *p;
        if b0 & 0x01 == 0x01 {
            let total = ((b0 >> 1) & 0x7F) as usize;
            let mut v = ((total - 1 + 4) as u32).wrapping_shl(2).to_le_bytes().to_vec(); // alloc: Exec
            v.extend_from_slice(core::slice::from_raw_parts(p.add(1), total - 1));
            v
        } else {
            core::slice::from_raw_parts(p, arrayfuncs::arr_size(core::slice::from_raw_parts(p, 4))).to_vec() // alloc: Exec
        }
    }
}

/// A search array's image nbtree reads in place: a plain 4-byte or a short header (not compressed, not external).
pub(super) fn plain_image(d: datum::Datum) -> bool {
    let p = d.as_u64() as *const u8;
    // SAFETY: a non-null array word addresses a live varlena header (the bind's parameter or the program's value).
    let b0 = unsafe { *p };
    (b0 & 0x01 == 0x01 && b0 != 0x01) || b0 & 0x03 == 0
}


/// A varlena datum's whole image (4-byte or 1-byte header), copied; `None` on a toast pointer.
pub(super) fn varlena_image(p: *const u8) -> Option<Arc<[u8]>> {
    // SAFETY: a non-null varlena datum of the plan addresses a live header and its bytes.
    unsafe {
        let b0 = *p;
        let len = if b0 & 0x01 == 0x01 {
            if b0 == 0x01 {
                return None;
            }
            ((b0 >> 1) & 0x7F) as usize
        } else {
            (u32::from_le_bytes([*p, *p.add(1), *p.add(2), *p.add(3)]) >> 2) as usize
        };
        Some(Arc::from(core::slice::from_raw_parts(p, len))) // alloc: Program
    }
}


