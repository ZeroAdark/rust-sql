//! The SQL-type axis as data (lowerv3 design §2.1): one row per type; a new SQL type is a new row, never a new `match` arm.
use gravity_abi::breaker::KeyKind;

use crate::catalog::Facts;
pub use crate::catalog::keysem::image::{key_of, FixedNorm, WordImage};


#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TypeFacts {
    pub typid: u32,
    pub base: u32,
    pub typlen: i16,
    pub byval: bool,
    pub align: u8,
    pub image: WordImage,
    pub kind: KeyKind,
    pub words: u8,
    /// The default btree opclass's `=` and support-1 procs, the default hash opclass's support-1 proc; 0 = none.
    pub eq: u32,
    pub cmp: u32,
    pub hash: u32,
    pub input: u32,
    pub output: u32,
    pub collatable: bool,
    /// An anonymous `record` key's canonical form id (gravity_fn `typesem::record_form` of its columns, KeyDecl `rec`),
    /// set by the lowering that resolved them; 0 for every other type and for a record whose columns are unknown.
    pub rec: u64,
    /// with#62 / #65 / #82 / #86: the user key (gravity_fn `typesem::record_user_key`) an anonymous `record` or `record[]`
    /// key over plan-known columns hashes and compares by, through PostgreSQL's support functions (no image), set by the
    /// lowering that resolved it; 0 for every other key.
    pub user: u64,
}

/// A clause's or Sort node's collation, read once (audit C8): whether equal bytes mean equal values and bytes order values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct KeyColl {
    pub oid: u32,
    pub bytes_eq: bool,
    pub bytes_order: bool,
}

pub const DEFAULT_COLLATION: u32 = 100;
pub const C_COLLATION: u32 = 950;
pub const POSIX_COLLATION: u32 = 951;
pub const FIRST_NORMAL_OID: u32 = 16384;

impl KeyColl {
    pub const NONE: KeyColl = KeyColl { oid: 0, bytes_eq: true, bytes_order: true };

    /// What the oid alone says, for a source without the catalog (`Facts::coll`'s default; D247): initdb's collations are
    /// deterministic, C / POSIX also order by bytes; the database default (100) is taken NOT to order by bytes (its locale
    /// is the database's), a user collation is the catalog's to say. `Facts::coll` answers from `pg_locale` when it can.
    pub const fn of(oid: u32) -> KeyColl {
        match oid {
            0 | C_COLLATION | POSIX_COLLATION => KeyColl { oid, bytes_eq: true, bytes_order: true },
            o if o < FIRST_NORMAL_OID => KeyColl { oid, bytes_eq: true, bytes_order: false },
            _ => KeyColl { oid, bytes_eq: false, bytes_order: false },
        }
    }
}

#[allow(clippy::too_many_arguments)]
const fn row(typid: u32, typlen: i16, byval: bool, align: u8, image: WordImage, kind: KeyKind, words: u8, procs: [u32; 5], collatable: bool) -> TypeFacts {
    TypeFacts { typid, base: typid, typlen, byval, align, image, kind, words, eq: procs[0], cmp: procs[1], hash: procs[2], input: procs[3], output: procs[4], collatable, rec: 0, user: 0 }
}

use WordImage::*;

pub static ROWS: &[TypeFacts] = &[ // seeded from PostgreSQL 18.6's catalog by gen/typefacts_procs.py; sorted by oid
    row(16, 1, true, b'c', Raw { signed: false }, KeyKind::I32, 1, [60, 1693, 6417, 1242, 1243], false), // bool
    row(17, -1, false, b'i', Bytes { text: false, bpchar_trim: false }, KeyKind::Fp128, 2, [1948, 1954, 6413, 1244, 31], false), // bytea
    row(18, 1, true, b'c', Raw { signed: false }, KeyKind::I32, 1, [61, 358, 454, 1245, 33], false), // char
    row(19, 64, false, b'c', Bytes { text: true, bpchar_trim: false }, KeyKind::Fp128, 2, [62, 359, 455, 34, 35], true), // name
    row(20, 8, true, b'd', Raw { signed: true }, KeyKind::I64, 1, [467, 842, 949, 460, 461], false), // int8
    row(21, 2, true, b's', Raw { signed: true }, KeyKind::I32, 1, [63, 350, 449, 38, 39], false), // int2
    row(22, -1, false, b'i', Canonical, KeyKind::Fp128, 2, [744, 382, 626, 40, 41], false), // int2vector
    row(23, 4, true, b'i', Raw { signed: true }, KeyKind::I32, 1, [65, 351, 450, 42, 43], false), // int4
    row(24, 4, true, b'i', Raw { signed: false }, KeyKind::I32, 1, [184, 356, 453, 44, 45], false), // regproc
    row(25, -1, false, b'i', Bytes { text: true, bpchar_trim: false }, KeyKind::Fp128, 2, [67, 360, 400, 46, 47], true), // text
    row(26, 4, true, b'i', Raw { signed: false }, KeyKind::I32, 1, [184, 356, 453, 1798, 1799], false), // oid
    row(27, 6, false, b's', Raw { signed: false }, KeyKind::I64, 1, [1292, 2794, 2233, 48, 49], false), // tid
    row(28, 4, true, b'i', Raw { signed: false }, KeyKind::I32, 1, [0, 0, 6419, 50, 51], false), // xid
    row(29, 4, true, b'i', Raw { signed: false }, KeyKind::I32, 1, [0, 0, 6423, 52, 53], false), // cid
    row(30, -1, false, b'i', Canonical, KeyKind::Fp128, 2, [679, 404, 457, 54, 55], false), // oidvector
    row(114, -1, false, b'i', Canonical, KeyKind::Fp128, 2, [0, 0, 0, 321, 322], false), // json
    row(142, -1, false, b'i', Canonical, KeyKind::Fp128, 2, [0, 0, 0, 2893, 2894], false), // xml
    row(194, -1, false, b'i', Canonical, KeyKind::Fp128, 2, [0, 0, 0, 195, 196], true), // pg_node_tree
    row(600, 16, false, b'd', Canonical, KeyKind::Fp128, 2, [0, 0, 0, 117, 118], false), // point
    row(601, 32, false, b'd', Canonical, KeyKind::Fp128, 2, [0, 0, 0, 119, 120], false), // lseg
    row(602, -1, false, b'd', Canonical, KeyKind::Fp128, 2, [0, 0, 0, 121, 122], false), // path
    row(603, 32, false, b'd', Canonical, KeyKind::Fp128, 2, [0, 0, 0, 123, 124], false), // box
    row(604, -1, false, b'd', Canonical, KeyKind::Fp128, 2, [0, 0, 0, 347, 348], false), // polygon
    row(650, -1, false, b'i', Bytes { text: false, bpchar_trim: false }, KeyKind::Fp128, 2, [920, 926, 422, 1267, 1427], false), // cidr
    row(700, 4, true, b'i', Float { f32: true }, KeyKind::F32, 1, [287, 354, 451, 200, 201], false), // float4
    row(701, 8, true, b'd', Float { f32: false }, KeyKind::F64, 1, [293, 355, 452, 214, 215], false), // float8
    row(718, 24, false, b'd', Canonical, KeyKind::Fp128, 2, [0, 0, 0, 1450, 1451], false), // circle
    row(774, 8, false, b'i', Raw { signed: false }, KeyKind::I64, 1, [4113, 4119, 328, 4110, 4111], false), // macaddr8
    row(790, 8, true, b'd', Raw { signed: true }, KeyKind::I64, 1, [888, 377, 0, 886, 887], false), // money
    row(829, 6, false, b'i', Raw { signed: false }, KeyKind::I64, 1, [830, 836, 399, 436, 437], false), // macaddr
    row(869, -1, false, b'i', Bytes { text: false, bpchar_trim: false }, KeyKind::Fp128, 2, [920, 926, 422, 910, 911], false), // inet
    row(1002, -1, false, b'i', Canonical, KeyKind::Fp128, 2, [744, 382, 626, 750, 751], false), // _char
    row(1003, -1, false, b'i', Canonical, KeyKind::Fp128, 2, [744, 382, 626, 750, 751], true), // _name
    row(1005, -1, false, b'i', Canonical, KeyKind::Fp128, 2, [744, 382, 626, 750, 751], false), // _int2
    row(1007, -1, false, b'i', Canonical, KeyKind::Fp128, 2, [744, 382, 626, 750, 751], false), // _int4
    row(1009, -1, false, b'i', Canonical, KeyKind::Fp128, 2, [744, 382, 626, 750, 751], true), // _text
    row(1014, -1, false, b'i', Canonical, KeyKind::Fp128, 2, [744, 382, 626, 750, 751], true), // _bpchar
    row(1015, -1, false, b'i', Canonical, KeyKind::Fp128, 2, [744, 382, 626, 750, 751], true), // _varchar
    row(1016, -1, false, b'd', Canonical, KeyKind::Fp128, 2, [744, 382, 626, 750, 751], false), // _int8
    row(1021, -1, false, b'i', Canonical, KeyKind::Fp128, 2, [744, 382, 626, 750, 751], false), // _float4
    row(1022, -1, false, b'd', Canonical, KeyKind::Fp128, 2, [744, 382, 626, 750, 751], false), // _float8
    row(1028, -1, false, b'i', Canonical, KeyKind::Fp128, 2, [744, 382, 626, 750, 751], false), // _oid
    row(1033, 16, false, b'd', Canonical, KeyKind::Fp128, 2, [0, 0, 329, 1031, 1032], false), // aclitem
    row(1034, -1, false, b'd', Canonical, KeyKind::Fp128, 2, [744, 382, 626, 750, 751], false), // _aclitem
    row(1042, -1, false, b'i', Bytes { text: true, bpchar_trim: true }, KeyKind::Fp128, 2, [1048, 1078, 1080, 1044, 1045], true), // bpchar
    row(1043, -1, false, b'i', Bytes { text: true, bpchar_trim: false }, KeyKind::Fp128, 2, [67, 360, 400, 1046, 1047], true), // varchar
    row(1082, 4, true, b'i', Raw { signed: true }, KeyKind::Date, 1, [1086, 1092, 6415, 1084, 1085], false), // date
    row(1083, 8, true, b'd', Raw { signed: true }, KeyKind::I64, 1, [1145, 1107, 1688, 1143, 1144], false), // time
    row(1114, 8, true, b'd', Raw { signed: true }, KeyKind::Timestamp, 1, [2052, 2045, 2039, 1312, 1313], false), // timestamp
    row(1184, 8, true, b'd', Raw { signed: true }, KeyKind::Timestamp, 1, [1152, 1314, 6425, 1150, 1151], false), // timestamptz
    row(1186, 16, false, b'd', Fixed { norm: FixedNorm::Interval }, KeyKind::I64, 2, [1162, 1315, 1697, 1160, 1161], false), // interval
    row(1266, 12, false, b'd', Fixed { norm: FixedNorm::Timetz }, KeyKind::I64, 2, [1352, 1358, 1696, 1350, 1351], false), // timetz
    row(1560, -1, false, b'i', Bits, KeyKind::Fp128, 2, [1581, 1596, 0, 1564, 1565], false), // bit
    row(1562, -1, false, b'i', Bits, KeyKind::Fp128, 2, [1666, 1672, 0, 1579, 1580], false), // varbit
    row(1700, -1, false, b'i', NumMantissa, KeyKind::I64, 1, [1718, 1769, 432, 1701, 1702], false), // numeric
    row(2202, 4, true, b'i', Raw { signed: false }, KeyKind::I32, 1, [184, 356, 453, 2212, 2213], false), // regprocedure
    row(2203, 4, true, b'i', Raw { signed: false }, KeyKind::I32, 1, [184, 356, 453, 2214, 2215], false), // regoper
    row(2204, 4, true, b'i', Raw { signed: false }, KeyKind::I32, 1, [184, 356, 453, 2216, 2217], false), // regoperator
    row(2205, 4, true, b'i', Raw { signed: false }, KeyKind::I32, 1, [184, 356, 453, 2218, 2219], false), // regclass
    row(2206, 4, true, b'i', Raw { signed: false }, KeyKind::I32, 1, [184, 356, 453, 2220, 2221], false), // regtype
    row(2249, -1, false, b'd', Canonical, KeyKind::Fp128, 2, [2981, 2987, 6192, 2290, 2291], false), // record
    row(2277, -1, false, b'd', Canonical, KeyKind::Fp128, 2, [744, 382, 626, 2296, 2297], false), // anyarray
    row(2950, 16, false, b'c', Fixed { norm: FixedNorm::Raw }, KeyKind::I64, 2, [2956, 2960, 2963, 2952, 2953], false), // uuid
    row(2970, -1, false, b'd', Canonical, KeyKind::Fp128, 2, [0, 0, 0, 2939, 2940], false), // txid_snapshot
    row(3220, 8, true, b'd', Raw { signed: false }, KeyKind::I64, 1, [3233, 3251, 3252, 3229, 3230], false), // pg_lsn
    row(3361, -1, false, b'i', Canonical, KeyKind::Fp128, 2, [0, 0, 0, 3355, 3356], true), // pg_ndistinct
    row(3402, -1, false, b'i', Canonical, KeyKind::Fp128, 2, [0, 0, 0, 3404, 3405], true), // pg_dependencies
    row(3614, -1, false, b'i', Canonical, KeyKind::Fp128, 2, [3618, 3622, 0, 3610, 3611], false), // tsvector
    row(3615, -1, false, b'i', Canonical, KeyKind::Fp128, 2, [3664, 3668, 0, 3612, 3613], false), // tsquery
    row(3734, 4, true, b'i', Raw { signed: false }, KeyKind::I32, 1, [184, 356, 453, 3736, 3737], false), // regconfig
    row(3769, 4, true, b'i', Raw { signed: false }, KeyKind::I32, 1, [184, 356, 453, 3771, 3772], false), // regdictionary
    row(3802, -1, false, b'i', Canonical, KeyKind::Fp128, 2, [4043, 4044, 4045, 3806, 3804], false), // jsonb
    row(3904, -1, false, b'i', Canonical, KeyKind::Fp128, 2, [3855, 3870, 3902, 3834, 3835], false), // int4range
    row(3906, -1, false, b'i', Canonical, KeyKind::Fp128, 2, [3855, 3870, 3902, 3834, 3835], false), // numrange
    row(3908, -1, false, b'd', Canonical, KeyKind::Fp128, 2, [3855, 3870, 3902, 3834, 3835], false), // tsrange
    row(3910, -1, false, b'd', Canonical, KeyKind::Fp128, 2, [3855, 3870, 3902, 3834, 3835], false), // tstzrange
    row(3912, -1, false, b'i', Canonical, KeyKind::Fp128, 2, [3855, 3870, 3902, 3834, 3835], false), // daterange
    row(3926, -1, false, b'd', Canonical, KeyKind::Fp128, 2, [3855, 3870, 3902, 3834, 3835], false), // int8range
    row(4089, 4, true, b'i', Raw { signed: false }, KeyKind::I32, 1, [184, 356, 453, 4084, 4085], false), // regnamespace
    row(4096, 4, true, b'i', Raw { signed: false }, KeyKind::I32, 1, [184, 356, 453, 4098, 4092], false), // regrole
    row(4191, 4, true, b'i', Raw { signed: false }, KeyKind::I32, 1, [184, 356, 453, 4193, 4194], false), // regcollation
    row(4451, -1, false, b'i', Canonical, KeyKind::Fp128, 2, [4244, 4273, 4278, 4231, 4232], false), // int4multirange
    row(4532, -1, false, b'i', Canonical, KeyKind::Fp128, 2, [4244, 4273, 4278, 4231, 4232], false), // nummultirange
    row(5017, -1, false, b'i', Canonical, KeyKind::Fp128, 2, [0, 0, 0, 5018, 5019], true), // pg_mcv_list
    row(5038, -1, false, b'd', Canonical, KeyKind::Fp128, 2, [0, 0, 0, 5055, 5056], false), // pg_snapshot
    row(5069, 8, true, b'd', Raw { signed: false }, KeyKind::I64, 1, [5084, 5096, 6421, 5070, 5081], false), // xid8
];

pub fn row_of(typid: u32) -> Option<&'static TypeFacts> {
    ROWS.binary_search_by_key(&typid, |r| r.typid).ok().map(|i| &ROWS[i])
}

/// A type the table does not list, from its storage alone (`Facts::type_row` or an attribute's facts): enums and by-value
/// types key as their word, every other type by its canonical image.
pub const fn opaque(typid: u32, base: u32, typlen: i16, byval: bool, is_enum: bool) -> TypeFacts {
    let (image, kind, words) = super::keysem::image::opaque(is_enum, byval, typlen);
    TypeFacts { typid, base, typlen, byval, align: b'i', image, kind, words, eq: 0, cmp: 0, hash: 0, input: 0, output: 0, collatable: false, rec: 0, user: 0 }
}

/// A type's row: the table's, else a domain's base row, else the catalog's storage (`Facts::type_row`); `None` when nothing
/// knows the type.
pub fn facts_of(facts: &dyn Facts, typid: u32) -> Option<TypeFacts> {
    if let Some(r) = row_of(typid) {
        return Some(*r);
    }
    let t = facts.type_row(typid)?;
    if t.base != typid && t.base != 0 {
        // a domain keys, hashes and orders as its base (the table's row, else the base's own catalog row: a domain over an
        // enum is an enum), under its own typid for output (V4R-84)
        if let Some(r) = row_of(t.base).copied().or_else(|| facts.type_row(t.base).map(|b| opaque(t.base, t.base, b.typlen, b.byval, b.typtype == b'e'))) {
            return Some(TypeFacts { typid, ..r });
        }
    }
    Some(opaque(typid, if t.base == 0 { typid } else { t.base }, t.typlen, t.byval, t.typtype == b'e'))
}


/// The kind both sides of a join key take, `None` when their words cannot be made equal (today's `unify_kind`).
pub fn unify(a: KeyKind, b: KeyKind) -> Option<KeyKind> {
    use KeyKind::*;
    match (a, b) {
        _ if a == b => Some(a),
        (I32, I64) | (I64, I32) => Some(I64),
        (F32, F64) | (F64, F32) => Some(F64),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use types_core::eq_alike::EqAlike;

    /// D2865: the planner's hashed DISTINCT and lowerv4's Merge Append admission read one type rule; every built-in row's image
    /// classes as the shared table says, and the table lists no type this one does not.
    #[test]
    fn eq_alike_is_the_shared_table() {
        for r in ROWS {
            assert_eq!(types_core::eq_alike::builtin(r.typid), Some(r.image.eq_alike()), "typid {}", r.typid);
        }
        for t in 0..20_000u32 {
            assert_eq!(types_core::eq_alike::builtin(t).is_some(), row_of(t).is_some(), "typid {t}");
        }
    }

    /// OID-19: a type's stored bytes order its values unless its equality prints apart by padding (the pad-insensitive
    /// image): the code-order premise and the shared equality table name the same rows.
    #[test]
    fn orders_as_stored_is_every_row_but_the_padded() {
        for r in ROWS {
            assert_eq!(r.image.orders_as_stored(), r.image.eq_alike() != EqAlike::Text { pad: true }, "typid {}", r.typid);
        }
        assert!(ROWS.iter().any(|r| !r.image.orders_as_stored()), "the pad-insensitive row is in the table");
    }
}
