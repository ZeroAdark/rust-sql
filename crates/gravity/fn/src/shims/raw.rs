//! The hand adapters' raw reads (lint R6's home under shims/): an argument a shim takes as its Datum (`ArgKind::D`), read
//! as the stored image PostgreSQL's function manager hands the function before any detoast.

/// The stored image of the non-NULL varlena whose Datum bits are `d`, in any header form: a short (1-byte) or plain (4-byte)
/// header, an inline compressed image, or a TOAST pointer's own bytes (its extent from its own header, `varsize_any`). Its
/// callers are strict shims whose text argument passed the call's null test, so `d` is a live varlena for the call.
pub(crate) fn varlena_image<'a>(d: i64) -> &'a [u8] {
    let p = d as usize as *const u8;
    // SAFETY: as above: a live varlena, read for the call only, no further than its header's extent.
    unsafe { core::slice::from_raw_parts(p, types_tuple::varatt::varsize_any(p)) }
}
