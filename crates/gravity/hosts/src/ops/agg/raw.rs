//! The agg family's raw reads (T-unsafe): an `unsafe` the family's host files add, a named function with its safety
//! contract (the pgrc family's raw.rs pattern).
#![allow(unsafe_code)]

use datum::Datum;

/// C MakeExpandedObjectReadOnly: a state `advance` kept as a read-write expanded object (CAND-M10) as a final's or a
/// combine's partial argument, which may read it but not grow it.
/// # Safety — `v` is a live non-NULL state of length `translen`.
pub(crate) unsafe fn read_only(v: Datum, translen: i16) -> Datum {
    datum::expandeddatum::make_expanded_object_read_only(v, false, translen)
}
