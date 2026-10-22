//! index_window's unsafe (lint R6; construct bundle scan-05; its raw.rs child): `copy_image` and its test moved as is from
//! index_window.rs, and the window fill's one-call wrapper over it, whose caller keeps its reason beside the call.
#![allow(unsafe_code)]

/// `copy_image` of a non-null by-reference cell of the batch. The caller passes the cell's pointer at its image inside the
/// index tuple (or the name buffer; D2988: a bitmap window's heap page its slot still pins), live while the row is
/// copied, and the column's `attlen`.
pub(crate) fn copy_cell(bytes: &mut Vec<u8>, p: *const u8, attlen: i16) -> u64 {
    // SAFETY: the caller's contract above.
    unsafe { copy_image(bytes, p, attlen) }
}

/// The by-reference image at `p` (`attlen` -1 varlena, -2 cstring, > 0 fixed) appended to `bytes`, 8-aligned; its offset.
/// # Safety
/// `p` points at a live image of that form.
unsafe fn copy_image(bytes: &mut Vec<u8>, p: *const u8, attlen: i16) -> u64 {
    let len = match attlen {
        -1 => types_tuple::varatt::varsize_any(p),
        -2 => core::ffi::CStr::from_ptr(p as *const core::ffi::c_char).to_bytes_with_nul().len(),
        n => n as usize,
    };
    let off = bytes.len().next_multiple_of(8);
    bytes.resize(off, 0); // alloc: Exec
    bytes.extend_from_slice(core::slice::from_raw_parts(p, len)); // alloc: Exec
    off as u64
}

#[cfg(test)]
mod tests {
    use super::copy_image;

    #[test]
    fn a_by_reference_cell_is_copied_whole_and_aligned() {
        let mut bytes = vec![0xaa]; // alloc: Exec
        // a 1-byte-header varlena of 4 bytes ("abc"), a cstring, an 8-byte fixed image
        let short = [(4u8 << 1) | 1, b'a', b'b', b'c'];
        let off = unsafe { copy_image(&mut bytes, short.as_ptr(), -1) } as usize;
        assert_eq!((off, &bytes[off..]), (8, &short[..]));
        let off = unsafe { copy_image(&mut bytes, c"name".as_ptr() as *const u8, -2) } as usize;
        assert_eq!((off, &bytes[off..]), (16, &b"name\0"[..]));
        let fixed = 0x0102_0304_0506_0708u64.to_ne_bytes();
        let off = unsafe { copy_image(&mut bytes, fixed.as_ptr(), 8) } as usize;
        assert_eq!((off, &bytes[off..]), (24, &fixed[..]));
    }
}
