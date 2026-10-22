//! bitmap_array's unsafe (lint R6; construct bundle scan-05; its raw.rs child): `array_image`, moved as is from
//! bitmap_array.rs with its safety contract.
#![allow(unsafe_code)]

/// An array datum's image with a 4-byte header (a short-header image is re-headed, as nbtree's key image is); `None` for an
/// external (TOAST pointer, header byte 0x01) or compressed image, which the seek's bind expands first (D4528:
/// `bitmap_window::bind_words`; read as a short header, 0x01 made a zero length and `total - 1` wrapped).
pub(super) fn array_image(p: *const u8) -> Option<Vec<u8>> {
    // SAFETY: a search-array key's argument addresses a live array image (the recipe's Const image, or the seek's word,
    // plain once bind_words has expanded it); its first byte is read before any length, and a TOASTed header is refused.
    unsafe {
        let b0 = *p;
        if b0 == 0x01 || b0 & 0x03 == 0x02 {
            return None;
        }
        Some(if b0 & 0x01 == 0x01 {
            let total = ((b0 >> 1) & 0x7F) as usize;
            let mut v = ((total - 1 + 4) as u32).wrapping_shl(2).to_le_bytes().to_vec(); // alloc: Exec
            v.extend_from_slice(core::slice::from_raw_parts(p.add(1), total - 1));
            v
        } else {
            core::slice::from_raw_parts(p, arrayfuncs::arr_size(core::slice::from_raw_parts(p, 4))).to_vec() // alloc: Exec
        })
    }
}
