//! Values in place (expr-compiler.md §1.4): the payload / image `View` a shim takes, formed at
//! the boundary without a copy where the header is plain; other forms expand into the region.

use gravity_fn::{Datum, NullableDatum, Region, View};
use mcx::Mcx;
use types_error::{PgError, PgResult};
use types_tuple::varatt;

#[derive(Clone, Copy, Debug)]
pub struct Val {
    pub value: NullableDatum,
    pub view: Option<View>,
}

impl Val {
    pub const NULL: Val = Val { value: NullableDatum::null(), view: None };

    #[inline(always)]
    pub fn datum(d: NullableDatum) -> Val {
        Val { value: d, view: None }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByRef {
    pub typlen: i16,
}

#[inline(always)]
pub unsafe fn varlena_is_plain(p: *const u8) -> bool {
    varatt::varatt_is_4b_u(p)
}

/// `ArgKind::P`: a varlena's data (any header form), a cstring to its NUL, a fixed type's bytes.
#[inline]
pub fn payload_view(d: Datum, typlen: i16, region: &mut Region) -> PgResult<View> {
    let p = d.as_usize() as *const u8;
    // SAFETY: a non-null by-reference datum points at a live image of its type.
    unsafe {
        match typlen {
            -1 => {
                if varlena_is_plain(p) {
                    return Ok(View::of_image(p));
                }
                if varatt::varatt_is_1b(p) && !varatt::varatt_is_1b_e(p) {
                    let n = varatt::varsize_1b(p) as usize - 1;
                    // a short-header datum off a page is unaligned: a body that reads words (numeric's digits) gets
                    // an aligned copy, as the row executor's detoast gives it
                    if (p as usize + 1) % 2 != 0 {
                        let q = region.reserve(n).ok_or_else(no_space)?;
                        core::ptr::copy_nonoverlapping(p.add(1), q, n);
                        return Ok(View { ptr: q, len: n as u64 });
                    }
                    return Ok(View { ptr: p.add(1), len: n as u64 });
                }
                Ok(View::of_image(detoast_into(region, p)?))
            }
            -2 => Ok(View { ptr: p, len: core::ffi::CStr::from_ptr(p.cast()).to_bytes().len() as u64 }),
            n => Ok(View { ptr: p, len: n as u64 }),
        }
    }
}

/// `ArgKind::I`: a varlena's 4-byte-header image (rebuilt in the region if not plain), a fixed type's bytes.
#[inline]
pub fn image_view(d: Datum, typlen: i16, region: &mut Region) -> PgResult<View> {
    let p = d.as_usize() as *const u8;
    // SAFETY: as `payload_view`.
    unsafe {
        match typlen {
            -1 => {
                if varlena_is_plain(p) {
                    return Ok(View { ptr: p, len: varatt::varsize_4b(p) as u64 });
                }
                let img = detoast_into(region, p)?;
                Ok(View { ptr: img, len: varatt::varsize_4b(img) as u64 })
            }
            -2 => Ok(View { ptr: p, len: core::ffi::CStr::from_ptr(p.cast()).to_bytes().len() as u64 + 1 }),
            n => Ok(View { ptr: p, len: n as u64 }),
        }
    }
}

/// A plain 4B image of any varlena form in the region (`detoast_attr` for the toasted forms).
pub unsafe fn detoast_into(region: &mut Region, p: *const u8) -> PgResult<*const u8> {
    if varatt::varatt_is_1b(p) && !varatt::varatt_is_1b_e(p) {
        let total = varatt::varsize_1b(p);
        let n = total - 1 + 4;
        let out = region.reserve(n).ok_or_else(no_space)?;
        out.cast::<[u8; 4]>().write_unaligned(datum::set_varsize_4b(n));
        core::ptr::copy_nonoverlapping(p.add(1), out.add(4), total - 1);
        return Ok(out);
    }
    detoast_cold(region, p)
}

/// A varlena's payload as bytes, whatever its header: a plain or short-header image read in place in full, a compressed or
/// external one expanded through the detoast seam and `copy` made of it. `None` when the detoast raises or `copy` is `None`;
/// nothing outlives the call (D348's constants; `percode::with_const_bytes`).
pub(crate) unsafe fn varlena_payload_with<R>(p: *const u8, copy: &mut dyn FnMut(&[u8]) -> Option<R>) -> Option<R> {
    if varatt::varatt_is_4b_u(p) {
        return copy(core::slice::from_raw_parts(p.add(4), varatt::varsize_4b(p) - 4));
    }
    if varatt::varatt_is_1b(p) && !varatt::varatt_is_1b_e(p) {
        // D4668: a short header leaves the payload at any address, and a reader may need its type's alignment (a
        // numeric's digits are i16): the payload (at most 126 bytes) is lent from an aligned copy, as pg_detoast_datum
        // copies a short varlena out before any type reads it
        let n = varatt::varsize_1b(p) - 1;
        let mut words = [0u64; 16];
        let aligned = core::slice::from_raw_parts_mut(words.as_mut_ptr().cast::<u8>(), n);
        aligned.copy_from_slice(core::slice::from_raw_parts(p.add(1), n));
        return copy(aligned);
    }
    let raw = core::slice::from_raw_parts(p, varatt::varsize_any(p));
    with_detoast_scratch(|cx| {
        let img = detoast_seams::detoast_attr::call(cx, raw).ok()?;
        let b: &[u8] = &img;
        let q = b.as_ptr();
        if b.len() >= 4 && varatt::varatt_is_4b_u(q) && varatt::varsize_4b(q) <= b.len() {
            return copy(&b[4..varatt::varsize_4b(q)]);
        }
        None
    })
}

#[cold]
#[inline(never)]
unsafe fn detoast_cold(region: &mut Region, p: *const u8) -> PgResult<*const u8> {
    let raw = core::slice::from_raw_parts(p, varatt::varsize_any(p));
    with_detoast_scratch(|cx| {
        let img = detoast_seams::detoast_attr::call(cx, raw)?;
        let bytes: &[u8] = &img;
        let out = region.reserve(bytes.len()).ok_or_else(no_space)?;
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), out, bytes.len());
        Ok(out as *const u8)
    })
}

thread_local! {
    // alloc: Node
    static SCRATCH: gravity_fn::scratch::ScratchSlot = gravity_fn::scratch::ScratchSlot::new("gravity_expr detoast scratch", "gravity_expr detoast scratch (nested)");
}

/// Run a detoast against the thread's retained scratch context, reset after `f` (which copies its result out);
/// a re-entered detoast (a nested statement's, under an outer view still forming) gets a context of its own, so
/// the outer's bytes stay valid until its own reset (`gravity_fn::with_scratch`'s pattern).
#[inline]
pub fn with_detoast_scratch<T>(f: impl FnOnce(Mcx<'_>) -> T) -> T {
    SCRATCH.with(|s| s.with(f))
}

#[cold]
#[inline(never)]
pub fn no_space() -> Box<PgError> {
    Box::new(PgError::error("gravity_expr: the result region is full").with_sqlstate(types_error::ERRCODE_OUT_OF_MEMORY)) // alloc: Cold
}

/// A by-reference result copied into the region as a complete image.
#[inline]
pub fn copy_into(region: &mut Region, d: Datum, typlen: i16) -> PgResult<Datum> {
    let r = gravity_fn::region_datum(region, d.as_usize() as *const u8, typlen as i32);
    if r.status == gravity_fn::OK {
        Ok(Datum::from_usize(r.val as usize))
    } else {
        Err(no_space())
    }
}

#[inline(always)]
pub fn datum_of(p: *const u8) -> Datum {
    Datum::from_usize(p as usize)
}
