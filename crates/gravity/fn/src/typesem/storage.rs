//! typesem::storage: a type's storage, pg_type's `typlen`, `typbyval` and `typalign`, read in the type-semantics home
//! (rule C4, the by-value width) for a host that sizes a state word or copies a datum as PostgreSQL's datumCopy does, or
//! deconstructs an array of it.

use types_core::Oid;
use types_error::PgResult;

/// `get_typlenbyval`: the type's `typlen` (bytes; -1 a varlena, -2 a C string) and whether a datum of it is passed by
/// value. A failed catalog read is the error.
pub fn len_byval(typid: Oid) -> PgResult<(i16, bool)> {
    lsyscache::get_typlenbyval(typid)
}

/// `get_typlenbyvalalign`: `len_byval` and the type's `typalign` (`'c'`, `'s'`, `'i'` or `'d'`), as an array builder
/// reads an element type's storage (arrayfuncs.c `initArrayResultAny`) or an index scan deconstructs a search array
/// (nodeIndexscan.c ExecIndexEvalArrayKeys). A failed catalog read is the error.
pub fn len_byval_align(typid: Oid) -> PgResult<(i16, bool, i8)> {
    lsyscache::get_typlenbyvalalign(typid)
}
