//! with#62 / #82 / #86: the array builders an anonymous record key's columns are read through (facts::record_union).

/// Whether `typid` is `record[]` (`_record`), the array of the anonymous row type: each element carries its columns in
/// its datum, as a `record` value does (`anonymous_record`).
pub fn record_array(typid: u32) -> bool {
    typid == types_core::catalog::RECORDARRAYOID
}

/// Whether function `funcid` builds its array from its arguments' elements and nothing else: array_cat (`anycompatiblearray
/// || anycompatiblearray`, pg_proc.dat 383), array_append (`anycompatiblearray || anycompatible`, 378) and array_prepend
/// (`anycompatible || anycompatiblearray`, 379). Every element of the result is an element of an array argument or the
/// element argument itself.
pub fn array_build(funcid: u32) -> bool {
    matches!(funcid, 383 | 378 | 379)
}
