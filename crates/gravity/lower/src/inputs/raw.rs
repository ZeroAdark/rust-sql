//! `inputs`' raw half (rule R6): a by-reference plan constant's image, read through the plan's pointer.
#![allow(unsafe_code)]
use types_nodes::primnodes::Const;

/// A non-null by-reference constant's image as the key hashes it: a varlena by its header (1-byte short, 4-byte long, a
/// toast pointer's tag byte alone), a cstring to its NUL, a fixed length by `constlen`. `None` for a NULL or by-value
/// constant, or a length of no image.
pub(crate) fn const_image<'a>(c: &Const) -> Option<&'a [u8]> {
    if c.constisnull || c.constbyval {
        return None;
    }
    let p = c.constvalue.as_usize() as *const u8;
    // SAFETY: a non-null by-reference plan constant's value points at its image: a varlena starts with its header, a
    // cstring ends at its NUL, a fixed-length one holds `constlen` bytes.
    unsafe {
        match c.constlen {
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
