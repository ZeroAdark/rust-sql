//! The key decisions of a hashed SubPlan's set (lowerv4 ops/subplan/hashed.rs `test_of`, `user_key`), answered in the
//! key-semantics home: which form keys a column whose test operator PostgreSQL runs through its support functions
//! (nodeSubplan.c `ExecInitSubPlan` binds the operator's equality and hash procs; `buildSubPlanHash` hashes with them).
//! The lowering keeps the IR it emits and its refusals; a catalog read that fails is the outer error, which it refuses by
//! the unread-operator name.
use types_core::Oid;

use super::super::R;
use super::Procs;

/// A built-in string equality across text and name (`texteqname` / `nameeqtext`) under a deterministic collation: each
/// side keyed by its own type's canonical form, the two forms comparing as the operator does. `None` when either side has
/// no form.
pub fn cross_forms(lt: Oid, rt: Oid) -> R<Option<(u64, u64)>> {
    let (l, r) = (super::super::form(lt)?, super::super::form(rt)?);
    Ok(match (l, r) {
        (Ok((lform, _)), Ok((rform, _))) => Some((lform, rform)),
        _ => None,
    })
}

/// D3682: a built-in cross-type float equality, the float_ops family's `float48eq` (float4, float8) or `float84eq` (float8,
/// float4) hashed by each side's own `hashfloat4` / `hashfloat8`: hashfloat4 hashes a float4 as its float8 and the operator
/// compares the float4 widened, so both sides key as the float8 value (`-0 = 0`, every NaN equal). Each side's width
/// (`true` for float4); `None` for any other operator or procs.
pub fn float_cross(lt: Oid, rt: Oid, procs: &Procs) -> Option<(bool, bool)> {
    use types_core::catalog::{FLOAT4OID, FLOAT8OID};
    const FLOAT48EQ: Oid = 299;
    const FLOAT84EQ: Oid = 305;
    let hash = |t: Oid| if t == FLOAT4OID { 451 } else { 452 }; // hashfloat4 / hashfloat8
    match (lt, rt, procs.eq) {
        (FLOAT4OID, FLOAT8OID, FLOAT48EQ) | (FLOAT8OID, FLOAT4OID, FLOAT84EQ) if procs.lhs_hash == hash(lt) && procs.rhs_hash == hash(rt) => {
            Some((lt == FLOAT4OID, rt == FLOAT4OID))
        }
        _ => None,
    }
}

/// USEROP: the user key of the by-reference key type `t` when PostgreSQL's hashed SubPlan runs exactly its support
/// functions: its user key (typesem `user_key`: the type cache's `=` and hash proc, refused where the hash disagrees with
/// the `=` at any level) under collation `coll` names the procs the operator's own hash support names (`procs`: the test's
/// equality on both sides and its left and right hash functions). `None` for any other.
pub fn user_set_key(t: Oid, coll: Oid, procs: &Procs) -> R<Option<u64>> {
    let spec = match super::user::user_key(t)? {
        Ok(spec) => spec,
        Err(_) => return Ok(None),
    };
    let uk = super::user::spec(spec)?;
    let id = |p: Oid| -> R<Option<u32>> { Ok(crate::resolved::resolve(p)?.ok()) };
    if uk.byval || uk.coll != coll || procs.eq == 0 || procs.rhs_eq != procs.eq || id(procs.eq)? != Some(uk.eq) || uk.hash == 0
        || id(procs.lhs_hash)? != Some(uk.hash) || id(procs.rhs_hash)? != Some(uk.hash)
    {
        return Ok(None);
    }
    Ok(Some(spec))
}

/// D2849: the canonical form of array key type `t` when PostgreSQL's hashed SubPlan hashes it by `hash_array` over an
/// element type whose hash proc reads its collation (`HASH_NEEDS_COLLATION`: hashtext, hashbpchar) under none (`coll` 0):
/// `hash_array` raises 42P22 at the first non-NULL ELEMENT it hashes, so a NULL array, an empty one or one of only NULL
/// elements hashes quietly. `procs` are the test's: `array_eq` on both sides and `hash_array` for each. The lowering keys
/// the set by the form and raises where PG's build or probe hashes such an element (lowerv4 ops/subplan/hashed.rs
/// `KeyCol::Canon`'s `elems`). `None` for any other key.
pub fn element_raise(t: Oid, coll: Oid, procs: &Procs) -> R<Option<u64>> {
    const F_ARRAY_EQ: Oid = 744;
    const F_HASH_ARRAY: Oid = 626;
    if coll != 0 || procs.eq != F_ARRAY_EQ || procs.rhs_eq != F_ARRAY_EQ || procs.lhs_hash != F_HASH_ARRAY || procs.rhs_hash != F_HASH_ARRAY {
        return Ok(None);
    }
    let elem = lsyscache::get_element_type(t)?;
    if elem == 0 || !types_core::quiet::hash_needs_collation(typcache::lookup_type_cache(elem, typcache::TYPECACHE_HASH_PROC)?.hash_proc()) {
        return Ok(None);
    }
    Ok(super::super::form(t)?.ok().map(|(id, _)| id))
}

#[cfg(test)]
mod tests {
    use super::super::Procs;
    use types_core::catalog::{FLOAT4OID, FLOAT8OID, INT4OID};

    fn procs(eq: u32, lhs_hash: u32, rhs_hash: u32) -> Procs {
        Procs { eq, lhs_hash, rhs_hash, rhs_eq: 0, cmp: 0 }
    }

    /// D3682: float48eq / float84eq over their own hash procs key each side by its width; anything else is no answer
    #[test]
    fn float_cross_takes_the_float_ops_cross_members_only() {
        assert_eq!(super::float_cross(FLOAT4OID, FLOAT8OID, &procs(299, 451, 452)), Some((true, false)));
        assert_eq!(super::float_cross(FLOAT8OID, FLOAT4OID, &procs(305, 452, 451)), Some((false, true)));
        assert_eq!(super::float_cross(FLOAT4OID, FLOAT8OID, &procs(305, 451, 452)), None, "the operator's own procedure");
        assert_eq!(super::float_cross(FLOAT4OID, FLOAT8OID, &procs(299, 452, 452)), None, "each side's own hash");
        assert_eq!(super::float_cross(FLOAT8OID, FLOAT8OID, &procs(293, 452, 452)), None, "one type is not a cross key");
        assert_eq!(super::float_cross(INT4OID, FLOAT8OID, &procs(299, 451, 452)), None);
    }
}
