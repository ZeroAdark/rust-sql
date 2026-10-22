//! The function and type identities the pgrc source claims, each asked by its meaning: the jsonb roads' compares
//! (lowerv4 ops/scan/pgrc/strategies: jsonb_codes, jsonb_inplace, jsonb_lanes), the zone verdicts' constant and key
//! types (ops/scan/pgrc/verdict_zone.rs) and the base types a code lane renders (hosts ops/scan/pgrc/granule_source.rs).
//! A road claims a compare only for the functions named here, so a user function of the same signature never reaches its
//! text-by-bytes compare. The OIDs are pg_proc.dat's and pg_type.dat's.

use types_core::Oid;

/// `texteq` (pg_proc 67, `text = text`, utils/adt/varlena.c).
const F_TEXTEQ: Oid = types_core::fmgr::F_TEXTEQ;
/// `textne` (pg_proc 157, `text <> text`, utils/adt/varlena.c).
const F_TEXTNE: Oid = 157;
/// `jsonb_object_field_text` (pg_proc 3214, `jsonb ->> text`, utils/adt/jsonfuncs.c).
const F_JSONB_OBJECT_FIELD_TEXT: Oid = 3214;
/// `jsonb_object_field` (pg_proc 3478, `jsonb -> text`, utils/adt/jsonfuncs.c).
const F_JSONB_OBJECT_FIELD: Oid = 3478;
/// `jsonb_extract_path_text` (pg_proc 3940, `jsonb #>> text[]` and the VARIADIC call, utils/adt/jsonfuncs.c).
const F_JSONB_EXTRACT_PATH_TEXT: Oid = 3940;

/// The key tests (jsonb_op.c): `jsonb_exists` (pg_proc 4047, `jsonb ? text`), `jsonb_exists_any` (4048, `?|`) and
/// `jsonb_exists_all` (4049, `?&`, both over a text[]).
const F_JSONB_EXISTS: Oid = 4047;
const F_JSONB_EXISTS_ANY: Oid = 4048;
const F_JSONB_EXISTS_ALL: Oid = 4049;

/// Function `funcid` as a top-level key test: `Some(None)` for `?` (one text key), `Some(Some(true))` for `?|` (any key
/// of a text[]), `Some(Some(false))` for `?&` (every key). Over an object root each asks whether the root holds the key
/// (whatever its value, a JSON null too); over an array root, whether a string element equals it; over a scalar root,
/// whether the scalar is that string (jsonb_op.c `jsonb_exists` .. through `findJsonbValueFromContainer`).
pub fn jsonb_key_test(funcid: Oid) -> Option<Option<bool>> {
    match funcid {
        F_JSONB_EXISTS => Some(None),
        F_JSONB_EXISTS_ANY => Some(Some(true)),
        F_JSONB_EXISTS_ALL => Some(Some(false)),
        _ => None,
    }
}

/// `jsonb_contains` (pg_proc 4046, `jsonb @> jsonb`, utils/adt/jsonb_op.c).
const F_JSONB_CONTAINS: Oid = 4046;

/// Whether function `funcid` is `jsonb_contains`, the `@>` whose right argument is the template the left must contain.
pub fn jsonb_contains(funcid: Oid) -> bool {
    funcid == F_JSONB_CONTAINS
}

/// A containment template's string leaves (jsonb_util.c `JsonbDeepContains` over an object template): `Some` when the
/// template (a jsonb payload, the varlena header stripped) is a non-empty object whose every value is a string or a
/// non-empty object of the same kind, at most `max_depth` keys deep: each leaf's key chain (outermost first) and its
/// string's bytes. Then `doc @> template` holds exactly when every leaf's chain reaches, through objects only, a string
/// equal to the leaf's: an object pair matches by key and its values must be the same jsonb type, a scalar compared by
/// value, a nested object recursively. `None` for anything else (an array or scalar template, an empty object, a number,
/// boolean, null or array value anywhere): those keep PostgreSQL's call whole.
pub fn contains_string_leaves(payload: &[u8], max_depth: usize) -> Option<Vec<(Vec<&[u8]>, &[u8])>> {
    use adt_jsonb::container::{container_is_object, container_is_scalar, container_size, fill_item, get_jsonb_length, get_jsonb_offset, JsonbItem};
    fn walk<'a>(c: &'a [u8], max_depth: usize, prefix: &mut Vec<&'a [u8]>, out: &mut Vec<(Vec<&'a [u8]>, &'a [u8])>) -> Option<()> {
        if c.len() < 4 || !container_is_object(c) || container_is_scalar(c) || container_size(c) == 0 || prefix.len() >= max_depth {
            return None;
        }
        let n = container_size(c);
        let base = 4 + 8 * n;
        if c.len() < base as usize {
            return None;
        }
        for i in 0..n {
            let (start, len) = ((base + get_jsonb_offset(c, i)) as usize, get_jsonb_length(c, i) as usize);
            prefix.push(c.get(start..start + len)?); // alloc: Cold
            match fill_item(c, i + n, base, get_jsonb_offset(c, i + n)) {
                JsonbItem::String(v) => out.push((prefix.clone(), v)), // alloc: Cold
                JsonbItem::Binary(child) => walk(child, max_depth, prefix, out)?,
                _ => return None,
            }
            prefix.pop();
        }
        Some(())
    }
    let mut out = Vec::new(); // alloc: Cold
    walk(payload, max_depth, &mut Vec::new(), &mut out)?; // alloc: Cold
    Some(out)
}

/// Whether function `funcid` is text equality or inequality, and which: `Some(false)` for `texteq` (`=`), `Some(true)` for
/// `textne` (`<>`), `None` for any other function. Under a collation that compares by bytes both answer by the bytes of
/// their arguments (varlena.c `texteq` / `textne`: `memcmp` when the collation is deterministic).
pub fn text_eq_ne(funcid: Oid) -> Option<bool> {
    match funcid {
        F_TEXTEQ => Some(false),
        F_TEXTNE => Some(true),
        _ => None,
    }
}

/// Whether function `funcid` is `jsonb_object_field_text`, the `->>` that reads an object's field by a text key and
/// renders it as text (NULL for an absent key, a JSON null, or a non-object).
pub fn jsonb_field_text(funcid: Oid) -> bool {
    funcid == F_JSONB_OBJECT_FIELD_TEXT
}

/// Whether function `funcid` is `jsonb_object_field`, the `->` that reads an object's field by a text key as jsonb (NULL
/// for an absent key or a non-object; a JSON null is the jsonb `null`, a scalar, whose fields are NULL).
pub fn jsonb_field(funcid: Oid) -> bool {
    funcid == F_JSONB_OBJECT_FIELD
}

/// `int8in` (pg_proc 460, the int8 input function, utils/adt/int8.c).
const F_INT8IN: Oid = 460;

/// Whether function `funcid` is `int8in`, the input function a `text::bigint` cast (a CoerceViaIO) calls: over the text a
/// jsonb number of scale 0 renders (`->>`: its digits, an optional leading minus) it is that number, and a number that
/// does not fit 64 bits never sits in an int8 lane (the writer's fixed-scale fit).
pub fn int8_in(funcid: Oid) -> bool {
    funcid == F_INT8IN
}

/// Whether function `funcid` is `jsonb_extract_path_text`, the `#>>` (and the VARIADIC call) that follows a text[] path
/// and renders the value it reaches as text (jsonfuncs.c `jsonb_get_element`: an object level takes the element as a key,
/// an array level as an integer index, a scalar level or an absent key is NULL, an empty path the whole document).
pub fn jsonb_path_text(funcid: Oid) -> bool {
    funcid == F_JSONB_EXTRACT_PATH_TEXT
}

/// Whether type `typid` is `numeric` (pg_type.dat `NUMERICOID`): the one constant type a packed-numeric lane's zone
/// predicate compares, by its mantissa at the lane's scale (lowerv4 ops/scan/pgrc/verdict_zone.rs `mantissa_bound`).
pub fn numeric(typid: Oid) -> bool {
    typid == types_core::catalog::NUMERICOID
}

/// Whether type `typid` is a signed by-value word type whose datum word, sign-extended, is its exact zone key and image:
/// int2, int4, int8, date, timestamp, timestamptz (signed C integers: `int16` / `int32` / `int64`, `DateADT`
/// (utils/date.h), `Timestamp` / `TimestampTz` (datatype/timestamp.h)).
pub fn signed_word(typid: Oid) -> bool {
    use types_core::catalog::{DATEOID, INT2OID, INT4OID, INT8OID, TIMESTAMPOID, TIMESTAMPTZOID};
    matches!(typid, INT8OID | INT2OID | INT4OID | DATEOID | TIMESTAMPOID | TIMESTAMPTZOID)
}

/// How a by-value lane of type `typid` compares on its packed image (lowerv4 frame/mask_vec.rs `packed_cond`: a pgrc
/// ByteFor frame read undecoded, `reference + delta` the datum word as the stream extends it, exact in i64): `Some(true)`
/// for a type whose order is its signed datum word's (`order_image` Signed: sign-extended, the image's i64 order),
/// `Some(false)` for oid, whose order is its unsigned 4-byte word's (an unsigned stream zero-extends it, so the image is
/// never negative and its i64 order is the unsigned order), `None` for every other type (an 8-byte unsigned word,
/// whose image passes i64's sign, bool and "char" (1 byte), the floats, numeric, text).
pub fn packed_order(typid: Oid) -> Option<bool> {
    match super::order_image(typid)? {
        super::OrderBy::Signed => Some(true),
        super::OrderBy::Unsigned if typid == types_core::catalog::OIDOID => Some(false),
        _ => None,
    }
}

/// Whether base type `typid` is text, varchar or bpchar (pg_type.dat `TEXTOID`, `VARCHAROID`, `BPCHAROID`): the types whose
/// stored bytes are the value's text, so a code lane may render a dictionary code as those bytes (a domain over one of
/// them is asked by its base type).
pub fn code_text(typid: Oid) -> bool {
    use types_core::catalog::{BPCHAROID, TEXTOID, VARCHAROID};
    matches!(typid, TEXTOID | VARCHAROID | BPCHAROID)
}

/// P5-13: the image length of base type `typid` when its btree order and equality are those of `memcmp` over its fixed-length
/// by-reference image (utils/adt/uuid.c `uuid_internal_cmp`; mac.c `macaddr_cmp` and mac8.c `macaddr8_cmp`, the bytes high
/// to low), as pgrc's schema types them `MemcmpOrdered`: uuid 16, macaddr 6, macaddr8 8. `None` for any other type (interval
/// and timetz are fixed images whose order is not their bytes').
pub fn memcmp_fixed(typid: Oid) -> Option<u16> {
    use types_core::catalog::{MACADDR8OID, MACADDROID, UUIDOID};
    match typid {
        UUIDOID => Some(16),
        MACADDROID => Some(6),
        MACADDR8OID => Some(8),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_text_compares_and_the_text_field_read_are_named_by_identity() {
        assert_eq!((text_eq_ne(67), text_eq_ne(157), text_eq_ne(3214)), (Some(false), Some(true), None));
        assert!(jsonb_field_text(3214) && !jsonb_field_text(3478) && !jsonb_field_text(67));
        assert!(jsonb_field(3478) && !jsonb_field(3214) && !jsonb_field(3215), "->, not ->> nor jsonb_array_element (3215)");
        assert!(jsonb_path_text(3940) && !jsonb_path_text(3217) && !jsonb_path_text(3214), "#>>, not #> (jsonb_extract_path 3217)");
    }

    #[test]
    fn the_zone_types_are_named_by_identity() {
        assert!(numeric(1700) && !numeric(701) && !numeric(20));
        assert!([20, 21, 23, 1082, 1114, 1184].iter().all(|&t| signed_word(t)));
        assert!(![16, 26, 700, 701, 1083, 1700, 25].iter().any(|&t| signed_word(t)), "bool, oid, floats, time, numeric, text");
    }

    #[test]
    fn the_packed_orders_follow_the_word_order_of_the_type() {
        assert!([21, 23, 20, 1082, 1083, 1114, 1184, 790].iter().all(|&t| packed_order(t) == Some(true)), "ints, date, time, timestamps, money");
        assert_eq!(packed_order(26), Some(false), "oid");
        assert!([16, 18, 5069, 3220, 700, 701, 1700, 25, 0].iter().all(|&t| packed_order(t).is_none()), "bool, char, xid8, pg_lsn, floats, numeric, text");
    }

    #[test]
    fn the_code_lane_text_types_are_named_by_identity() {
        assert!([25, 1043, 1042].iter().all(|&t| code_text(t)), "text, varchar, bpchar");
        assert!(![18, 19, 17, 114, 3802, 1700, 0].iter().any(|&t| code_text(t)), "char, name, bytea, json, jsonb, numeric, none");
    }

    #[test]
    fn the_memcmp_fixed_types_are_named_by_identity() {
        assert_eq!((memcmp_fixed(2950), memcmp_fixed(829), memcmp_fixed(774)), (Some(16), Some(6), Some(8)), "uuid, macaddr, macaddr8");
        assert!([1186, 1266, 19, 27, 17, 25, 0].iter().all(|&t| memcmp_fixed(t).is_none()), "interval, timetz, name, tid, bytea, text, none");
    }
}
