//! The decomposer's raw half: a constant's fixed-length by-reference image, read through the bake log (expr.rs
//! `const_fixed_bytes` is its safe face).
#![allow(unsafe_code)]

/// P5-13: the `len`-byte image of a non-NULL fixed-length by-reference constant `c` (`constlen` == `len` > 0), read
/// through `bake::read`, so it is logged in the key as `const_bytes`' payload is; `None` for a NULL, a by-value or a
/// varlena constant, or another length.
pub(crate) fn const_fixed_image<'p>(c: &types_nodes::primnodes::Const, len: u16) -> Option<&'p [u8]> {
    if c.constisnull || c.constbyval || c.constlen != len as i32 || len == 0 {
        return None;
    }
    // SAFETY: a non-NULL by-reference constant of `constlen` > 0 points at its live `constlen`-byte image
    Some(unsafe { core::slice::from_raw_parts(crate::bake::read(c).as_usize() as *const u8, len as usize) })
}
