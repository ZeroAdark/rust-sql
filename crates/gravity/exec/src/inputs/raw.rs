//! The front walk's unsafe reads (rule R6, the module's raw home): a by-reference constant's image and a constant list's
//! element count, read through the plan's pointer. Each takes the Const and answers `None` for a NULL or by-value one, so
//! front.rs reads them safely.
#![allow(unsafe_code)]

use types_nodes::primnodes::Const;

/// A by-reference constant's image as key.rs hashes it: a varlena by its header (1-byte short, 4-byte long, a toast
/// pointer's tag byte alone), a cstring to its NUL, a fixed length by `constlen`; any other length by its datum. `None`
/// for a NULL or by-value constant, or a length of no image.
pub(super) fn image<'a>(k: &Const) -> Option<&'a [u8]> {
    if k.constisnull || k.constbyval {
        return None;
    }
    let p = k.constvalue.as_usize() as *const u8;
    // SAFETY: a non-null by-reference plan constant's value points at its image (key.rs hashes the same bytes): a varlena
    // starts with its header, a cstring ends at its NUL, a fixed-length one holds `constlen` bytes.
    unsafe {
        match k.constlen {
            -1 => {
                let b0 = *p;
                let len = if b0 == 0x01 {
                    1
                } else if b0 & 0x01 == 0x01 {
                    (b0 >> 1) as usize
                } else {
                    (u32::from_le_bytes([*p, *p.add(1), *p.add(2), *p.add(3)]) >> 2) as usize
                };
                Some(core::slice::from_raw_parts(p, len.max(1)))
            }
            -2 => Some(core::ffi::CStr::from_ptr(p as *const core::ffi::c_char).to_bytes()),
            n if n > 0 => Some(core::slice::from_raw_parts(p, n as usize)),
            _ => None,
        }
    }
}

/// A constant array list's element count (gravity_expr konst.rs `list_len`, V4R-110): `None` for a NULL or by-value
/// constant, or an image whose count it cannot read (compressed, external).
pub(super) fn list_len(k: &Const) -> Option<usize> {
    if k.constisnull || k.constbyval {
        return None;
    }
    // SAFETY: a non-null by-reference constant's value is its image; the caller hands a ScalarArrayOpExpr's array
    // argument, whose image is an array.
    unsafe { gravity_expr::nodes::konst::list_len(k.constvalue.as_usize()) }
}
