//! The pgrc family's raw reads (T-unsafe): every `unsafe` the family's lowering files need, each a named function with
//! its SAFETY text, so those files compile under the crate's `#![deny(unsafe_code)]`.
#![allow(unsafe_code)]
use gravity_expr::nodes::array::{ElemFacts, Walk};
use gravity_expr::nodes::konst::ConstNode;

use super::strategies::jsonb_inplace::TEXT_OID;

/// A varlena's payload read in place (PostgreSQL varatt.h, little-endian headers): the 1-byte short and the plain 4-byte
/// forms; `None` for an external (TOAST) pointer or a compressed image.
/// # Safety — `p` points at a varlena datum that outlives `'a`.
unsafe fn varlena_inline<'a>(p: *const u8) -> Option<&'a [u8]> {
    match *p {
        h if h & 1 == 1 && h != 1 => Some(core::slice::from_raw_parts(p.add(1), (h >> 1) as usize - 1)),
        h if h & 3 == 0 => Some(core::slice::from_raw_parts(p.add(4), (u32::from_le_bytes(*(p as *const [u8; 4])) >> 2) as usize - 4)),
        _ => None,
    }
}

/// A non-NULL by-reference constant's payload, read (`ConstNode::read`: logged as baked); `None` for NULL, a by-value
/// constant, or an image not inline.
pub(crate) fn const_inline<'a>(k: &'a ConstNode<'_>) -> Option<&'a [u8]> {
    if k.is_null() || k.byval {
        return None;
    }
    let img = k.read().value.as_u64() as usize as *const u8;
    // SAFETY: a non-NULL by-reference constant points at its image, which the program owns for its life
    unsafe { varlena_inline(img) }
}

/// A non-NULL text array constant's elements in storage order (`deconstruct_array`'s), read (`ConstNode::read`: logged as
/// baked); `None` for NULL, a by-value constant, an image not plain, a NULL element, or an element not inline.
pub(crate) fn const_text_elems<'a>(k: &'a ConstNode<'_>) -> Option<Vec<&'a [u8]>> {
    if k.is_null() || k.byval {
        return None;
    }
    let img = k.read().value;
    // SAFETY: a non-NULL by-reference constant points at its image, which the program owns for its life
    if unsafe { *(img.as_usize() as *const u8) } & 3 != 0 {
        return None;
    }
    let elem = ElemFacts { typ: TEXT_OID, len: -1, byval: false, align: b'i' as i8 };
    let mut w = Walk::over(img, elem, &mut gravity_fn::Region::over(&mut [])).ok()?;
    let mut out = Vec::with_capacity(w.len()); // alloc: Program
    while let Some(e) = w.next() {
        if e.isnull {
            return None;
        }
        // SAFETY: a present element of a plain array image is a varlena inside the image, which the program owns for its life
        out.push(unsafe { varlena_inline(e.value.as_usize() as *const u8) }?);
    }
    Some(out)
}

// The two host entries the lowered field = ANY test calls by address. contract::host writes their `extern "C"`; names,
// signatures and bodies stay as they were, so the IR's host names do not move.
gravity_abi::host! {
    /// D564: the text array `array` (`align`) of `n` elements decoded into `out` (`1 + 2n` words): the status (1, | 2 when an
    /// element is NULL), each element's payload address and length (-1: NULL). Status 0 (the host walk) for an image not
    /// plain, another count, or an element not inline. # Safety — `array` is a live varlena, `out` holds `1 + 2n` words.
    pub(crate) unsafe fn field_any_decode(array: u64, align: u64, n: u64, out: u64) {
        let out = core::slice::from_raw_parts_mut(out as usize as *mut i64, 1 + 2 * n as usize);
        out[0] = 0;
        if *(array as usize as *const u8) & 3 != 0 {
            return;
        }
        let elem = ElemFacts { typ: TEXT_OID, len: -1, byval: false, align: align as i8 };
        let Ok(mut w) = Walk::over(gravity_fn::Datum::from_usize(array as usize), elem, &mut gravity_fn::Region::over(&mut [])) else { return };
        if w.len() != n as usize {
            return;
        }
        let mut st = 1;
        for i in 0..n as usize {
            let e = w.next().expect("n elements");
            let (p, l) = match e.isnull {
                true => (0, -1),
                false => match varlena_inline(e.value.as_usize() as *const u8) {
                    Some(d) => (d.as_ptr() as i64, d.len() as i64),
                    None => return,
                },
            };
            st |= 2 * e.isnull as i64;
            (out[1 + 2 * i], out[2 + 2 * i]) = (p, l);
        }
        out[0] = st;
    }

    /// `[p, p + len)` (`len` -1: a NULL operand) `= ANY` of the text array `array`, its elements aligned `align`: 1 matched,
    /// 0 none, 2 none over a NULL element or a NULL operand, 3 a NULL operand over an empty array, -1 an image it does not
    /// read (not a plain header, a compressed element): the call runs cold. # Safety — `p` holds `len` bytes; `array` is a
    /// live varlena.
    pub(crate) unsafe fn field_any(p: u64, len: u64, array: u64, align: u64) -> i64 {
        if *(array as usize as *const u8) & 3 != 0 {
            return -1;
        }
        let elem = ElemFacts { typ: TEXT_OID, len: -1, byval: false, align: align as i8 };
        let Ok(mut w) = Walk::over(gravity_fn::Datum::from_usize(array as usize), elem, &mut gravity_fn::Region::over(&mut [])) else { return -1 };
        if len as i64 == -1 {
            return if w.is_empty() { 3 } else { 2 };
        }
        let (a, mut null) = (core::slice::from_raw_parts(p as usize as *const u8, len as usize), false);
        while let Some(e) = w.next() {
            match e.isnull {
                true => null = true,
                false => match gravity_fn::with_payload(e.value.as_usize() as *const u8, |n| n == a) {
                    Ok(true) => return 1,
                    Ok(false) => {}
                    Err(_) => return -1,
                },
            }
        }
        if null { 2 } else { 0 }
    }
}
