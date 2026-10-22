//! The parent's pgstat counts.
//! A seal, compile, compaction or VACUUM FULL moves rows and does not count them. Counts are transactional, as heap's
//! are: an aborted insert counts as a dead tuple, which is the rolled-back residue a later VACUUM or flush reclaims.

use types_rel::Relation;

pub fn inserted(rel: &Relation<'_>, n: u64) {
    if n > 0 && rel.pgstat_enabled.get() {
        pgstat::relation::pgstat_count_heap_insert(rel.rd_id, rel.rd_rel.relisshared, n as i64);
    }
}

/// One row of `rel` updated: a new version in the front and a tombstone for the old one. Never HOT; the new version
/// always goes to a segment, and pgstat's newpage count is heap's page-movement fact, so it is not claimed.
pub fn updated(rel: &Relation<'_>) {
    if rel.pgstat_enabled.get() {
        pgstat::relation::pgstat_count_heap_update(rel.rd_id, rel.rd_rel.relisshared, false, false);
    }
}

pub fn deleted(rel: &Relation<'_>) {
    if rel.pgstat_enabled.get() {
        pgstat::relation::pgstat_count_heap_delete(rel.rd_id, rel.rd_rel.relisshared);
    }
}
