//! KeySem's type-keyed entry: a key type's support as PostgreSQL's type cache names it (typcache.c `lookup_type_cache`),
//! for a consumer that holds a type and no operator (a MinMaxExpr's comparator, a type's hash function, a range's
//! subtype comparator). Each answer asks the type cache for its own flag only, as PostgreSQL's executor asks it at the same
//! point, so no read here fills a cache entry PostgreSQL would not have filled; an opclass change reaches the next answer
//! through the cache's own invalidation, exactly as PostgreSQL's next plan sees it.
use types_core::Oid;
use types_error::PgResult;

/// `TYPECACHE_EQ_OPR`: the type's default btree class's `=`, else its default hash class's (a composite's `record_eq`
/// `=`); 0 when it has none.
pub fn eq_opr(typid: Oid) -> PgResult<Oid> {
    Ok(typcache::lookup_type_cache(typid, typcache::TYPECACHE_EQ_OPR)?.eq_opr())
}

/// `TYPECACHE_LT_OPR`: the type's default btree class's `<` (the parser's ORDER BY operator for the type); 0 when it
/// has none.
pub fn lt_opr(typid: Oid) -> PgResult<Oid> {
    Ok(typcache::lookup_type_cache(typid, typcache::TYPECACHE_LT_OPR)?.lt_opr())
}

/// KeySem's answer for a key that has a type and no operator (core-rep R1 (a): a rep site that holds only the key's
/// type): the type's default `=` (`eq_opr`, `KeyUse::Hash`) or `<` (`lt_opr`, `KeyUse::Sort`) under collation `coll`,
/// read for the type itself (`KeyReq::typed`: an array's, range's, enum's or composite's polymorphic `=` keys by the
/// type's own word, form and collatability). `Ok(Err)`: the type has no such operator (`NoHash(0)` / `NoOrder(0)`), or
/// the coordinate `resolve` names; `Err`: a catalog read failed.
pub fn key(mcx: mcx::Mcx<'_>, typid: Oid, coll: Oid, use_: super::KeyUse) -> PgResult<Result<super::KeySem, super::TypeClass>> {
    use super::{KeyReq, KeyUse, TypeClass};
    let req = match use_ {
        KeyUse::Hash => match eq_opr(typid)? {
            0 => return Ok(Err(TypeClass::NoHash(0))),
            op => KeyReq::hash(op, coll),
        },
        KeyUse::Sort => match lt_opr(typid)? {
            0 => return Ok(Err(TypeClass::NoOrder(0))),
            op => KeyReq::sort(op).under(coll),
        },
        KeyUse::Eq | KeyUse::Merge => unreachable!("of_type::key answers KeyUse::Hash and KeyUse::Sort"),
    };
    super::resolve(mcx, &req.typed(typid, typid))
}

/// `TYPECACHE_CMP_PROC`: the type's default btree class's comparison function (support 1); 0 when it has none.
pub fn cmp_proc(typid: Oid) -> PgResult<Oid> {
    Ok(typcache::lookup_type_cache(typid, typcache::TYPECACHE_CMP_PROC)?.cmp_proc())
}

/// `TYPECACHE_HASH_PROC` (`ext`: `TYPECACHE_HASH_EXTENDED_PROC`): the type's default hash class's hash function, or its
/// extended (seeded) one; 0 when it has none.
pub fn hash_proc(typid: Oid, ext: bool) -> PgResult<Oid> {
    Ok(match ext {
        false => typcache::lookup_type_cache(typid, typcache::TYPECACHE_HASH_PROC)?.hash_proc(),
        true => typcache::lookup_type_cache(typid, typcache::TYPECACHE_HASH_EXTENDED_PROC)?.hash_extended_proc(),
    })
}

/// `TYPECACHE_RANGE_INFO`: range type `typid`'s subtype comparator (`rng_cmp_proc_finfo`, the range's subtype opclass's
/// support 1, which orders and compares its bounds).
pub fn range_cmp_proc(typid: Oid) -> PgResult<Oid> {
    Ok(typcache::lookup_type_cache(typid, typcache::TYPECACHE_RANGE_INFO)?.rng_cmp_proc_finfo().fn_oid)
}
