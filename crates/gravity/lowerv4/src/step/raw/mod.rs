//! The step module's raw half (lint R6): expr.saop's host half and hashed set (`saop_host`, `saop_set`: the program's
//! owned descriptor read through its owner pointer, the elements' images walked in place), and the safe wrappers the
//! step files need over an image read in place, each with its safety contract.
#![allow(unsafe_code)]
use gravity_expr::nodes::konst::list_len;
use gravity_expr::nodes::ConstNode;
use mcx::{CollectIn, Mcx, PgVec};
use types_nodes::primnodes::Const;

pub(crate) mod saop_bytes;
pub(crate) mod saop_extreme;
pub(crate) mod saop_host;
pub(crate) mod saop_set;

/// The element count of non-null by-reference array constant `c`'s image (gravity_expr konst.rs `list_len`, the keys'
/// reader), `None` when its image does not give one (step/consts.rs `needs_of`).
pub(crate) fn const_list_len(c: &Const) -> Option<usize> {
    // SAFETY: a non-null by-reference constant points at its image.
    unsafe { list_len(c.constvalue.as_usize()) }
}

/// A non-NULL varlena constant's bytes when its image is inline (a 1-byte or plain 4-byte header); `None` for any other
/// constant (step/text_like.rs `const_bytes`).
pub(crate) fn const_inline_bytes<'a>(k: &ConstNode<'a>) -> Option<&'a [u8]> {
    let (v, p) = (k.read(), k.read().value.as_u64() as usize as *const u8);
    // SAFETY: a non-NULL by-reference constant points at its image, which the program owns for its life
    (!v.isnull && !k.byval && k.typlen == -1).then(|| unsafe {
        match *p {
            h if h & 1 == 1 && h != 1 => Some(core::slice::from_raw_parts(p.add(1), (h >> 1) as usize - 1)),
            h if h & 3 == 0 => Some(core::slice::from_raw_parts(p.add(4), (u32::from_le_bytes(*(p as *const [u8; 4])) >> 2) as usize - 4)),
            _ => None,
        }
    })?
}

/// Whether PostgreSQL's `upper(text)` under collation `coll` maps every ASCII byte as `to_ascii_uppercase` does, probed
/// once through PG's own function over the bytes 1..0x7f (step/text_case.rs `mode`); `None` when the call fails.
pub(crate) fn upper_keeps_ascii(coll: u32, mcx: Mcx<'_>) -> Option<bool> {
    keeps_ascii(coll, false, mcx)
}

/// `upper_keeps_ascii` for `lower(text)` and `to_ascii_lowercase` (step/text_case.rs `fold_mode`).
pub(crate) fn lower_keeps_ascii(coll: u32, mcx: Mcx<'_>) -> Option<bool> {
    keeps_ascii(coll, true, mcx)
}

fn keeps_ascii(coll: u32, lower: bool, mcx: Mcx<'_>) -> Option<bool> {
    let probe: PgVec<'_, u8> = (1u8..0x80).collect_in(mcx);
    let mut buf = [0u8; 512];
    let mut region = gravity_fn::Region::over(&mut buf);
    let mut err = gravity_fn::ErrSlot::NONE;
    let view = gravity_fn::View { ptr: probe.as_ptr(), len: probe.len() as u64 };
    let r = if lower {
        gravity_fn::shims::byref_ref::lower(&mut err, &mut region, coll, view)
    } else {
        gravity_fn::shims::byref_ref::upper(&mut err, &mut region, coll, view)
    };
    let want = if lower { probe.to_ascii_lowercase() } else { probe.to_ascii_uppercase() };
    // SAFETY: an OK result is a plain 4-byte-header image in `buf`
    let got = (r.status == gravity_fn::OK && gravity_abi::unword(err.err).is_null()).then(|| unsafe {
        let p = r.val as usize as *const u8;
        core::slice::from_raw_parts(p.add(4), (u32::from_le_bytes(*(p as *const [u8; 4])) >> 2) as usize - 4)
    })?;
    Some(got == want)
}
