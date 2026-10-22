//! fn srf_fmgr's catalog gates (its family's sibling file in the typesem home, rule R3): which C functions run their own
//! statement, so a function-manager set refuses them by name (`srf_fmgr::Refuse::Nested`). Moved unchanged from
//! srf_fmgr.rs; the caller still makes the same catalog reads in the same order (the OID test, then pg_proc's name).
use types_core::Oid;

/// `ts_rewrite(tsquery, text)`: its text is a query it runs (the NESTED rule by OID: the other `ts_rewrite` runs none).
const TS_REWRITE_QUERY: Oid = 3685;

/// The bodies that reach the executor again (SPI): named, never run from a program.
const NESTED: [&[u8]; 16] = [
    b"ts_stat", b"query_to_xml", b"query_to_xmlschema", b"query_to_xml_and_xmlschema", b"cursor_to_xml", b"cursor_to_xmlschema", b"table_to_xml", b"table_to_xmlschema",
    b"table_to_xml_and_xmlschema", b"schema_to_xml", b"schema_to_xmlschema", b"schema_to_xml_and_xmlschema", b"database_to_xml", b"database_to_xmlschema",
    b"database_to_xml_and_xmlschema", b"pg_get_serial_sequence",
];

/// Function `funcid` runs its own statement by its OID alone (`ts_rewrite(tsquery, text)`).
pub fn nested_by_oid(funcid: Oid) -> bool {
    funcid == TS_REWRITE_QUERY
}

/// A function named `name` (pg_proc.proname) runs its own statement.
pub fn nested_by_name(name: &[u8]) -> bool {
    NESTED.contains(&name)
}
