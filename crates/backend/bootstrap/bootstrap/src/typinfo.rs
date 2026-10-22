
use types_core::{Oid, C_COLLATION_OID, DEFAULT_COLLATION_OID, InvalidOid};
use types_tuple::{TYPALIGN_CHAR, TYPALIGN_INT, TYPALIGN_SHORT, TYPSTORAGE_EXTENDED, TYPSTORAGE_PLAIN};

pub struct TypInfo {
    pub name: &'static str,
    pub oid: Oid,
    pub elem: Oid,
    pub len: i16,
    pub byval: bool,
    pub align: i8,
    pub storage: i8,
    pub collation: Oid,
    pub inproc: Oid,
    pub outproc: Oid,
}

const NAMEDATALEN: i16 = types_core::fmgr::NAMEDATALEN as i16;

macro_rules! t {
    ($name:literal, $oid:expr, $elem:expr, $len:expr, $byval:expr, $align:expr, $storage:expr, $coll:expr, $in:expr, $out:expr) => {
        TypInfo { name: $name, oid: $oid, elem: $elem, len: $len, byval: $byval, align: $align, storage: $storage, collation: $coll, inproc: $in, outproc: $out }
    };
}

pub static TYP_INFO: &[TypInfo] = &[
    t!("bool", 16, 0, 1, true, TYPALIGN_CHAR, TYPSTORAGE_PLAIN, InvalidOid, 1242, 1243),
    t!("bytea", 17, 0, -1, false, TYPALIGN_INT, TYPSTORAGE_EXTENDED, InvalidOid, 1244, 31),
    t!("char", 18, 0, 1, true, TYPALIGN_CHAR, TYPSTORAGE_PLAIN, InvalidOid, 1245, 33),
    t!("int2", 21, 0, 2, true, TYPALIGN_SHORT, TYPSTORAGE_PLAIN, InvalidOid, 38, 39),
    t!("int4", 23, 0, 4, true, TYPALIGN_INT, TYPSTORAGE_PLAIN, InvalidOid, 42, 43),
    t!("float4", 700, 0, 4, true, TYPALIGN_INT, TYPSTORAGE_PLAIN, InvalidOid, 200, 201),
    t!("name", 19, 18, NAMEDATALEN, false, TYPALIGN_CHAR, TYPSTORAGE_PLAIN, C_COLLATION_OID, 34, 35),
    t!("regclass", 2205, 0, 4, true, TYPALIGN_INT, TYPSTORAGE_PLAIN, InvalidOid, 2218, 2219),
    t!("regproc", 24, 0, 4, true, TYPALIGN_INT, TYPSTORAGE_PLAIN, InvalidOid, 44, 45),
    t!("regtype", 2206, 0, 4, true, TYPALIGN_INT, TYPSTORAGE_PLAIN, InvalidOid, 2220, 2221),
    t!("regrole", 4096, 0, 4, true, TYPALIGN_INT, TYPSTORAGE_PLAIN, InvalidOid, 4098, 4092),
    t!("regnamespace", 4089, 0, 4, true, TYPALIGN_INT, TYPSTORAGE_PLAIN, InvalidOid, 4084, 4085),
    t!("text", 25, 0, -1, false, TYPALIGN_INT, TYPSTORAGE_EXTENDED, DEFAULT_COLLATION_OID, 46, 47),
    t!("oid", 26, 0, 4, true, TYPALIGN_INT, TYPSTORAGE_PLAIN, InvalidOid, 1798, 1799),
    t!("tid", 27, 0, 6, false, TYPALIGN_SHORT, TYPSTORAGE_PLAIN, InvalidOid, 48, 49),
    t!("xid", 28, 0, 4, true, TYPALIGN_INT, TYPSTORAGE_PLAIN, InvalidOid, 50, 51),
    t!("cid", 29, 0, 4, true, TYPALIGN_INT, TYPSTORAGE_PLAIN, InvalidOid, 52, 53),
    t!("pg_node_tree", 194, 0, -1, false, TYPALIGN_INT, TYPSTORAGE_EXTENDED, DEFAULT_COLLATION_OID, 195, 196),
    t!("int2vector", 22, 21, -1, false, TYPALIGN_INT, TYPSTORAGE_PLAIN, InvalidOid, 40, 41),
    t!("oidvector", 30, 26, -1, false, TYPALIGN_INT, TYPSTORAGE_PLAIN, InvalidOid, 54, 55),
    t!("_int4", 1007, 23, -1, false, TYPALIGN_INT, TYPSTORAGE_EXTENDED, InvalidOid, 750, 751),
    t!("_text", 1009, 25, -1, false, TYPALIGN_INT, TYPSTORAGE_EXTENDED, DEFAULT_COLLATION_OID, 750, 751),
    t!("_oid", 1028, 26, -1, false, TYPALIGN_INT, TYPSTORAGE_EXTENDED, InvalidOid, 750, 751),
    t!("_char", 1002, 18, -1, false, TYPALIGN_INT, TYPSTORAGE_EXTENDED, InvalidOid, 750, 751),
    t!("_aclitem", 1034, 1033, -1, false, TYPALIGN_INT, TYPSTORAGE_EXTENDED, InvalidOid, 750, 751),
];

#[derive(Clone, Debug)]
pub struct TypMap {
    pub oid: Oid,
    pub name: String,
    pub len: i16,
    pub byval: bool,
    pub align: i8,
    pub storage: i8,
    pub collation: Oid,
    pub elem: Oid,
    pub delim: i8,
    pub input: Oid,
    pub output: Oid,
}
