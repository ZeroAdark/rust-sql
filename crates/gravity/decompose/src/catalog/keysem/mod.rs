//! Decompose's plan-time key answers over `Facts`, kept in the plan-time catalog beside the methods they read: the one
//! implementation of each (their callers in facts/ and effects/ ask here). KeySem's equality request replaces the
//! `Facts::key_eq` reads below when grouping moves onto it; until then these are its only readers outside the catalog.
use super::{Facts, KeyEq};

pub mod image;

/// An equality's (left, right) types when its equal values are one image of each side's type (`Facts::op_image_eq`: the
/// key home's `keysem::image` list on the server, a fixture's `-- imageeq` lines): the own `=` of int2 / int4 / int8 / oid /
/// date / bool / char, or the integer `=` across widths (D434, D345). Read by facts pinned / passenger and ops/agg keys.
pub fn image_eq(f: &dyn Facts, op: u32) -> Option<(u32, u32)> {
    f.op_image_eq(op)
}

/// An image equality's one type when both its sides are that type (never a cross-width integer `=`: int4 = int8 has two).
pub fn image_eq_same(f: &dyn Facts, op: u32) -> Option<u32> {
    f.op_image_eq(op).filter(|(l, r)| l == r).map(|(l, _)| l)
}

/// Equal values of type `ty` under collation `coll` are one image (the typefacts row's key image): a raw word, an enum,
/// a byte string compared by its bytes (a text under a deterministic collation). Never a float (-0 / 0), a numeric
/// (dscale), an interval or timetz (normalized), a bpchar (trailing blanks), a bit string or a canonical image. Read by
/// ops/agg keys (a pinned GROUP BY column) and ops/group absorb (a pinned column the plain fold keeps).
pub fn one_image(f: &dyn Facts, ty: u32, coll: u32) -> bool {
    use crate::catalog::typefacts::{row_of, WordImage};
    match row_of(ty).map(|t| t.image) {
        Some(WordImage::Raw { .. } | WordImage::Enum) => true,
        Some(WordImage::Bytes { text, bpchar_trim: false }) => !text || f.coll(coll).bytes_eq,
        _ => false,
    }
}

/// D3375 (facts/demand/readers.rs `uniq`): operator `op` is type `ty`'s own built-in `=` (`Facts::key_eq` Own), an
/// equivalence under one collation: two values equal to a third are equal to each other and to the same values, whatever
/// their images (numeric 1.0 = 1.00; a nondeterministic collation's case-equal strings). A user operator or type never.
pub fn own_equivalence(f: &dyn Facts, op: u32, ty: u32) -> bool {
    f.key_eq(op, ty) == KeyEq::Own && !crate::effects::user_code(op) && !crate::effects::user_code(ty)
}

/// `facts::pinned::pinned_bytes`' equality: operator `op` is type `ty`'s own `=` and the type's image is its untrimmed
/// bytes.
pub fn own_bytes_eq(f: &dyn Facts, op: u32, ty: u32) -> bool {
    f.key_eq(op, ty) == KeyEq::Own && crate::catalog::typefacts::facts_of(f, ty).is_some_and(|t| matches!(t.image, crate::catalog::typefacts::WordImage::Bytes { bpchar_trim: false, .. }))
}

/// D2719 / D2640: whether a key of type `typid` under equality operator `op` compares by no image of its type: by user
/// code or by a built-in other than the type's own `=` (`Facts::key_eq` User or Other). The key half of effects
/// `user_group_eq`, which adds the operator's being a user's (a user-code reach test, the effects home's).
pub fn eq_not_image(f: &dyn Facts, op: u32, typid: u32) -> bool {
    matches!(f.key_eq(op, typid), KeyEq::User | KeyEq::Other)
}

/// D2640: whether a key of type `typid` under equality operator `op` compares by a procedure the plan-time catalog does
/// not see into (`Facts::key_eq` User: user code, or no catalog to tell): a hashed group keys such a by-value key by its
/// class representative's word (facts/user_hash.rs `key`), which only an equality whose classes are one value each makes
/// exact; a built-in other than the type's own `=` (int4ne, `KeyEq::Other`) is not one.
pub fn eq_opaque(f: &dyn Facts, op: u32, typid: u32) -> bool {
    f.key_eq(op, typid) == KeyEq::User
}

/// A group key of type `ty` under equality operator `op` (decompose agg `map` / `map_set`, D2719 / D2640, KeySem slice 4):
/// whether the group refuses the key. The type's own `=` never refuses; a byte-equal operator is taken only by an Agg
/// (`agg`) keying a type that holds a multirange, and refuses everywhere else; another built-in refuses unless the group
/// streams or runs its keys through the operator (`streams_or_runs`); a user operator refuses unless the group streams or
/// runs, or a hashed Agg hashes the key through the operator class (`hashes`, asked only then), and always over a type
/// holding a multirange.
pub fn group_key_refused(f: &dyn Facts, op: u32, ty: u32, agg: bool, streams_or_runs: bool, hashes: &dyn Fn() -> bool) -> bool {
    match f.key_eq(op, ty) {
        KeyEq::Own => false,
        KeyEq::Bytes => !(agg && f.key_holds_multirange(ty)),
        KeyEq::Other => !streams_or_runs,
        KeyEq::User => !(streams_or_runs || hashes()) || f.key_holds_multirange(ty),
    }
}

/// Whether a group key of type `ty` holds a multirange (`Facts::key_holds_multirange`): a refused user operator's key then
/// names the operator refusal (`join::USER_GROUP_OP`).
pub fn key_holds_multirange(f: &dyn Facts, ty: u32) -> bool {
    f.key_holds_multirange(ty)
}

/// A by-reference group key of type `ty` holding a multirange: whether operator `op` compares it by its bytes (`Some`);
/// `None` for a type holding none (decompose agg `eq_op`).
pub fn multirange_bytes_eq(f: &dyn Facts, op: u32, ty: u32) -> Option<bool> {
    f.key_holds_multirange(ty).then(|| f.key_eq(op, ty) == KeyEq::Bytes)
}

/// K4 (decision 24, the hash join's keys; D2036): the hash functions PostgreSQL's hash join calls on each input of clause
/// operator `op` under input collation `coll` (`Facts::op_hash_functions`, which the host answers from KeySem's
/// `hash_procs`), both set; none = unknown, no hash support or an indeterminate collation (join.rs `user_keys` refuses).
pub fn hash_join_procs(f: &dyn Facts, op: u32, coll: u32) -> Option<(u32, u32)> {
    f.op_hash_functions(op, coll).filter(|&(l, r)| l != 0 && r != 0)
}

/// K4 (D1901, V4R-508's in-row refusal at a hash join): whether a key whose sides have types `types` (outer, inner, where
/// known) holds a multirange (`Facts::key_holds_multirange`) under clause operator `op` that is no built-in hashable
/// equality (`hash_eq::builtin_hash_eq`): such a key would key by multirange_eq's canonical form, PostgreSQL by the
/// clause's operator (join.rs `key_decls` refuses it by `join::USER_HASH_OP`).
pub fn hash_join_multirange(f: &dyn Facts, op: u32, types: [Option<u32>; 2]) -> bool {
    !super::hash_eq::builtin_hash_eq(f, op) && types.into_iter().flatten().any(|t| f.key_holds_multirange(t))
}
