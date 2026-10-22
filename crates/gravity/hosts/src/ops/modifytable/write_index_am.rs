//! Rung 10, member `IndexNonBtree`: plain hash / gin / gist / spgist / brin / bloom indexes through `index_insert`; the GiST constraint forms (WITHOUT OVERLAPS / EXCLUDE) go through `index_maint`'s exclusion check; a HOT update feeds the summarizing (BRIN) ones alone.

use types_rel::Relation;
use types_relscan::IndexAmKind;

/// D3279: the AMs whose insert `index_maint` serves through `indexam::index_insert` (ExecInsertIndexTuples' call): the
/// core six and contrib bloom; pgvector's hnsw stays out until a PostgreSQL 18.6 oracle with pgvector witnesses it.
pub fn insertable(kind: IndexAmKind) -> bool {
    matches!(kind, IndexAmKind::Btree | IndexAmKind::Hash | IndexAmKind::Gin | IndexAmKind::Gist | IndexAmKind::Spgist | IndexAmKind::Brin | IndexAmKind::Bloom)
}

pub fn unmaintainable_am(index: &Relation<'_>) -> Option<&'static str> {
    let form = index.rd_index.as_ref()?;
    let kind = IndexAmKind::from_relam(index.rd_rel.relam);
    if !insertable(kind) {
        Some("an index whose access method is not btree / hash / gin / gist / spgist / brin / bloom")
    } else if !matches!(kind, IndexAmKind::Btree) && form.indisunique && !form.indisexclusion {
        Some("a unique constraint on a non-btree index without exclusion operators (no core AM: amcanunique is btree's)")
    } else {
        None
    }
}

pub fn summarizing(index: &Relation<'_>) -> bool {
    IndexAmKind::from_relam(index.rd_rel.relam).amsummarizing()
}
