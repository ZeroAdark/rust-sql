//! prewarm's unsafe steps (rule R6, its raw home): the fixed NOT NULL columns of a pg_database or pg_class row, read
//! under the descriptor of the catalog the row was scanned from.
#![allow(unsafe_code)]

use types_core::Oid;
use types_tuple::{HeapTupleData, TupleDescData};

use crate::{ANUM_PG_CLASS_OID, ANUM_PG_CLASS_RELKIND, ANUM_PG_CLASS_RELNAMESPACE, ANUM_PG_CLASS_RELTOASTRELID};

/// A pg_database row's (oid, datallowconn, datistemplate, datconnlimit); `tup` comes from a scan of pg_database and
/// `desc` is that relation's descriptor.
pub(crate) fn database_row(tup: &HeapTupleData<'_>, desc: &TupleDescData<'_>) -> (Oid, bool, bool, i32) {
    let mut isnull = false;
    // SAFETY: fixed NOT NULL pg_database columns under its descriptor.
    unsafe {
        (
            types_tuple::heap_getattr(tup, pg_database::Anum_pg_database_oid, desc, &mut isnull).as_oid(),
            types_tuple::heap_getattr(tup, pg_database::Anum_pg_database_datallowconn, desc, &mut isnull).as_bool(),
            types_tuple::heap_getattr(tup, pg_database::Anum_pg_database_datistemplate, desc, &mut isnull).as_bool(),
            types_tuple::heap_getattr(tup, pg_database::Anum_pg_database_datconnlimit, desc, &mut isnull).as_i32(),
        )
    }
}

/// A pg_class row's (relnamespace, relkind, oid, reltoastrelid); `tup` comes from a scan of pg_class and `desc` is that
/// relation's descriptor.
pub(crate) fn class_row(tup: &HeapTupleData<'_>, desc: &TupleDescData<'_>) -> (Oid, u8, Oid, Oid) {
    let mut isnull = false;
    // SAFETY: fixed NOT NULL pg_class columns under its descriptor.
    unsafe {
        (
            types_tuple::heap_getattr(tup, ANUM_PG_CLASS_RELNAMESPACE, desc, &mut isnull).as_oid(),
            types_tuple::heap_getattr(tup, ANUM_PG_CLASS_RELKIND, desc, &mut isnull).as_i8() as u8,
            types_tuple::heap_getattr(tup, ANUM_PG_CLASS_OID, desc, &mut isnull).as_oid(),
            types_tuple::heap_getattr(tup, ANUM_PG_CLASS_RELTOASTRELID, desc, &mut isnull).as_oid(),
        )
    }
}
