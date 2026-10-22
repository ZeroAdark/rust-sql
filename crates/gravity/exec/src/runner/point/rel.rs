//! The table tests every stage that reads or writes a table shares (`None`: the stage takes the table): a plain or
//! materialized heap table that is not a system table (a catalog or a TOAST table), a temporary one, or under row
//! security; and an index that is the table's.
use mcx::Mcx;
use types_error::PgResult;
use types_rel::{NoLock, Relation, RELKIND_MATVIEW, RELKIND_RELATION};

pub(super) fn table_refusal(heap: &Relation<'_>) -> Option<&'static str> {
    let r = &heap.rd_rel;
    if !matches!(r.relkind, RELKIND_RELATION | RELKIND_MATVIEW) || !gravity_hosts::ops::scan::index_source::heap_am(heap) {
        return Some("table kind");
    }
    // IsSystemRelation (catalog.c): a catalog or a TOAST table. Their point reads (shared catalogs, column-privileged ones
    // such as pg_authid, catalog snapshots, system columns) are a later widening with its own proof
    if catalog_seams::is_catalog_relation::call(heap) || catalog_seams::is_toast_relation::call(heap) {
        return Some("system table");
    }
    if r.relpersistence == types_core::catalog::RELPERSISTENCE_TEMP || r.relrowsecurity {
        return Some("temporary or row-security table");
    }
    None
}

/// `index` is one of `heap`'s.
pub(super) fn index_of(heap: &Relation<'_>, index: &Relation<'_>) -> bool {
    index.rd_index.as_ref().is_some_and(|f| f.indrelid == heap.rd_id)
}

/// The tests of a table a point write changes (pglpoint S3, `None`: the stage writes it): S1's (`table_refusal`), a plain
/// table (a materialized view is changed only by REFRESH), no trigger of any kind (relhastriggers: a user trigger, a
/// constraint trigger, and the RI triggers of a foreign key on either side), no rule, and neither a partition nor an
/// inheritance parent. Each is the program's to serve; a published table is the write host's (its bind runs
/// CheckCmdReplicaIdentity, as the program's does).
pub(super) fn write_refusal(heap: &Relation<'_>) -> Option<&'static str> {
    if let Some(why) = table_refusal(heap) {
        return Some(why);
    }
    let r = &heap.rd_rel;
    if r.relkind != RELKIND_RELATION {
        return Some("table kind");
    }
    if heap.rd_hastriggers {
        return Some("triggers");
    }
    if heap.rd_hasrules {
        return Some("rules");
    }
    if r.relispartition || r.relhassubclass {
        return Some("partition or inheritance");
    }
    None
}

/// The further tests of a table a point UPDATE writes (pglpoint S3b, `None`: the stage writes it), after `write_refusal`:
/// no generated column of either kind (a stored one ExecComputeStoredGenerated recomputes) and no identity column; no SET
/// column (`set`, attnos) that any index reads (RelationGetIndexAttrBitmap's hot-blocking and summarizing columns, an
/// expression's or a predicate's columns included: a key change takes LockTupleExclusive and puts new entries in every
/// index, a later widening); and no exclusion constraint (indisexclusion). The planner opened and locked every index of
/// the result relation at its own lock mode (get_relation_info), so they are read here with NoLock.
pub(super) fn update_refusal(mcx: Mcx<'_>, heap: &Relation<'_>, set: &[i16]) -> PgResult<Option<&'static str>> {
    let att = &heap.rd_att;
    let generated = att.constr.as_deref().is_some_and(|c| c.has_generated_stored || c.has_generated_virtual);
    if generated || att.attrs.iter().any(|a| !a.attisdropped && (a.attidentity != 0 || a.attgenerated != 0)) {
        return Ok(Some("generated or identity column"));
    }
    let indexed = relcache_seams::relation_get_index_attr_bitmap::call(heap.rd_id)?;
    if set.iter().any(|a| indexed.hot_blocking.binary_search(a).is_ok() || indexed.summarized.binary_search(a).is_ok()) {
        return Ok(Some("indexed column"));
    }
    for &oid in relcache_seams::relation_get_index_list::call(mcx, heap.rd_id)?.iter() {
        let index = indexam::index_open(mcx, oid, NoLock)?;
        let exclusion = index.rd_index.as_ref().is_some_and(|f| f.indisexclusion);
        indexam::index_close(index, NoLock)?;
        if exclusion {
            return Ok(Some("exclusion constraint"));
        }
    }
    Ok(None)
}
