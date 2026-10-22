use std::rc::{Rc, Weak};

use gravity_abi::ScanPos;
use types_core::Oid;
use types_error::PgResult;
use types_rel::{Relation, RelationData};
use types_storage::lock::NoLock;
use types_tuple::{ItemPointerData, ItemPointerGetBlockNumber, ItemPointerGetBlockNumberNoCheck, ItemPointerGetOffsetNumber, ItemPointerGetOffsetNumberNoCheck};

pub use super::raw::{open_heap, open_index, snapshot_of, snapshot_ptr};

/// A parked relation handle (`park`) opened again for a statement, as RelationIdGetRelation's hit: `None` when the entry is
/// gone, or no longer valid (an invalidation the relcache has not rebuilt it from yet: RelationIdGetRelation rebuilds such
/// an entry at its open, so the caller opens the relation instead of reading the stale entry).
pub fn repin(parked: &Option<Weak<RelationData<'static>>>) -> Option<Relation<'static>> {
    let data = parked.as_ref()?.upgrade()?;
    if !data.rd_isvalid.get() {
        return None;
    }
    if data.uses_local_buffers() {
        xact_seams::set_xact_accessed_temp_namespace::call();
    }
    data.pgstat_enabled.set(pgstat_seams::pgstat_init_relation::call(data.rd_id, data.rd_rel.relkind));
    Some(Relation::open_rc(data, None))
}

pub fn park(rel: Option<Relation<'static>>) -> PgResult<Option<Weak<RelationData<'static>>>> {
    match rel {
        Some(rel) => {
            let weak = Rc::downgrade(rel.data_rc());
            rel.close(NoLock)?;
            Ok(Some(weak))
        }
        None => Ok(None),
    }
}

/// A probe row's own position (`POS_ROW`): the row protocol serves it as is.
pub fn pos_of(rel: Oid, tid: &ItemPointerData) -> ScanPos {
    ScanPos::row(rel, ItemPointerGetBlockNumber(tid), ItemPointerGetOffsetNumber(tid))
}

/// D2947: `pos_of` of a row whose TID may be PostgreSQL's invalid one: a KNN walk's row from the reorder queue
/// (IndexNextWithReorder's ExecForceStoreHeapTuple clears the slot's tts_tid, so the row's ctid reads (4294967295,0)
/// there, and here). The words are read as they are, without `ItemPointerGetBlockNumber`'s validity assert.
pub fn pos_of_any(rel: Oid, tid: &ItemPointerData) -> ScanPos {
    ScanPos::row(rel, ItemPointerGetBlockNumberNoCheck(tid), ItemPointerGetOffsetNumberNoCheck(tid))
}

/// D2947: `pos` is the invalid TID a KNN walk's reordered row carries (`pos_of_any`), not a row and not the empty
/// position (block 0) a mark stores for no row.
pub fn invalid_tid(pos: ScanPos) -> bool {
    pos.off == 0 && pos.block == types_core::InvalidBlockNumber
}

pub fn tid_of(pos: ScanPos) -> ItemPointerData {
    ItemPointerData::new(pos.block, pos.off)
}
