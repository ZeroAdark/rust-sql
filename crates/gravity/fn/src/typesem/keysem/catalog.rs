//! The plan-time catalog's key answers: the reads behind decompose `catalog::Facts`' key methods that the server's facts
//! (hosts/src/facts.rs) answer from the live catalog. A failed catalog read is the "no" its Facts method documents (none,
//! 0, false), so a key the catalog cannot describe refuses by its consumer's name.

type Oid = u32;

/// D2313: a btree index key column's family `opfamily` against the default btree families of its declared type
/// `opcintype` and its stored type `atttypid` (the type cache's `btree_opf`): 0 when it is either one's default (the key
/// orders as its image), else the family. A failed default read keeps the family (another family's order is never taken
/// for the default's).
pub fn index_family(opfamily: Oid, opcintype: Oid, atttypid: Option<Oid>) -> u32 {
    let default = |t: Oid| default_family(t) == Some(opfamily);
    if default(opcintype) || atttypid.is_some_and(default) { 0 } else { opfamily as u32 }
}

/// OID-07: whether operator `opno` is a btree equality of any family (`get_mergejoin_opfamilies`: strategy 3 in some btree
/// opfamily), so values it calls equal sort together under that family. A failed catalog read answers false.
pub fn op_eq(mcx: mcx::Mcx<'_>, opno: Oid) -> bool {
    lsyscache::get_mergejoin_opfamilies(mcx, opno).is_ok_and(|f| !f.is_empty())
}

/// Record-image order: type `typid`'s default btree family (the type cache's `btree_opf`, GetDefaultOpClass's family);
/// `None` when it has none or the read fails.
pub fn default_family(typid: Oid) -> Option<Oid> {
    let f = typcache::lookup_type_cache(typid, typcache::TYPECACHE_BTREE_OPFAMILY).ok()?.btree_opf();
    (f != 0).then_some(f)
}

/// Type `typid`'s storage and kind as the catalog keeps it: `(typlen, typbyval, typtype, base type)` (a domain's base is
/// its base type, any other type's itself); none when a read fails.
pub fn type_row(typid: Oid) -> Option<(i16, bool, u8, Oid)> {
    let (typlen, byval) = lsyscache::typ::get_typlenbyval(typid).ok()?;
    Some((typlen, byval, lsyscache::typ::get_typtype(typid).ok()? as u8, lsyscache::typ::getBaseType(typid).ok()?))
}
