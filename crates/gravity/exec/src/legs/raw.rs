//! `legs`' unsafe steps (rule R6, the module's raw home): the length and bytes of a pushed batch's by-reference image.
#![allow(unsafe_code)]

use gravity_abi::Datum;

/// The bytes of the by-reference image at `p` of a column of `typlen` (> 0 fixed, -1 varlena, -2 cstring): `VARSIZE_ANY`
/// as `gravity_expr::byref::image_len` reads it (1-byte header, the 18-byte on-disk toast pointer, 4-byte header).
/// # Safety
/// `p` is a live image of that type (the push's contract: the batch's datums are readable during the push).
pub(super) unsafe fn image_len(p: *const u8, typlen: i16) -> usize {
    if typlen > 0 {
        return typlen as usize;
    }
    if typlen == -2 {
        return core::ffi::CStr::from_ptr(p as *const core::ffi::c_char).to_bytes().len() + 1;
    }
    let b0 = *p;
    if b0 & 1 != 0 {
        // the 4-byte header is read only behind the 1-byte test: a short datum may end its page or mapping
        return if b0 == 1 { 18 } else { (b0 >> 1) as usize };
    }
    ((core::ptr::read_unaligned(p as *const u32) >> 2) & 0x3fff_ffff) as usize
}

/// The by-reference image the datum `d` of a column of `typlen` points at, `image_len` bytes long.
pub(super) fn image<'a>(d: Datum, typlen: i16) -> &'a [u8] {
    // SAFETY: the push's datums are live through the push (`BorrowedBatch`), this one an image of its column's type
    unsafe { core::slice::from_raw_parts(d as *const u8, image_len(d as *const u8, typlen)) }
}
