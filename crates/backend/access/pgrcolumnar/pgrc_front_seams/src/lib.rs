use std::sync::Arc;

use mcx::Mcx;
use pgrc_format::dml::Tombstone;
use tableam_vocab::{LockTupleMode, Snapshot};
use types_core::{CommandId, Oid, TransactionId};
use types_error::PgResult;
use types_rel::Relation;
use types_tuple::ItemPointerData;

seam_core::seam!(
    pub fn create_segment<'mcx>(mcx: Mcx<'mcx>, parent: Oid, slot: u16) -> PgResult<Oid>
);

seam_core::seam!(
    pub fn segment_relid(parent: Oid, slot: u16) -> PgResult<Option<Oid>>
);

seam_core::seam!(
    pub fn reset_segment<'mcx>(mcx: Mcx<'mcx>, relid: Oid) -> PgResult<()>
);

seam_core::seam!(
    /// True once the worker committed it (the caller re-reads the ring), false when no worker ran it: the caller activates in its own.
    pub fn activate_detached(parent: Oid) -> PgResult<bool>
);

seam_core::seam!(
    pub fn rotate_detached(db: Oid, parent: Oid) -> bool
);

seam_core::seam!(
    pub fn seal_pool() -> Option<Arc<dyn pgrc_write::seal::ColumnPool>>
);

seam_core::seam!(
    pub fn wake_compaction()
);

seam_core::seam!(
    pub fn create_tombstone<'mcx>(mcx: Mcx<'mcx>, parent: Oid) -> PgResult<Oid>
);

seam_core::seam!(
    pub fn tombstone_relid(parent: Oid) -> PgResult<Option<Oid>>
);

seam_core::seam!(
    /// `None`: no image, read as bearing.
    pub fn tomb_lock_only<'mcx>(tomb: &Relation<'mcx>) -> PgResult<Option<bool>>
);

seam_core::seam!(
    pub fn tomb_overwrite<'mcx>(mcx: Mcx<'mcx>, tomb: &Relation<'mcx>, tid: ItemPointerData, t: Tombstone, cid: CommandId) -> PgResult<()>
);

seam_core::seam!(
    pub fn create_tomb<'mcx>(mcx: Mcx<'mcx>, parent: Oid, slot: u8) -> PgResult<Oid>
);

seam_core::seam!(
    pub fn tomb_relid(parent: Oid, slot: u8) -> PgResult<Option<Oid>>
);

seam_core::seam!(
    pub fn tomb_relids(parent: Oid) -> PgResult<Vec<Oid>>
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TombHit {
    pub tid: ItemPointerData,
    pub succ: u64,
    pub xmin: TransactionId,
    pub cmin: CommandId,
    pub state: TombState,
    pub spec_token: u32,
    pub xmax: TransactionId,
    pub own_unfrozen: bool,
}

impl TombHit {
    pub fn is_lock(&self) -> bool {
        self.succ == pgrc_format::rowid::SUCC_LOCK
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TombState {
    Committed,
    InProgress,
    Own,
}

seam_core::seam!(
    pub fn tomb_probe<'a, 'mcx>(mcx: Mcx<'mcx>, tomb: &'a Relation<'mcx>, rowident: u64, snap: &'a Snapshot<'mcx>) -> PgResult<Option<TombHit>>
);

seam_core::seam!(
    pub fn tomb_probe_many<'a, 'mcx>(mcx: Mcx<'mcx>, tomb: &'a Relation<'mcx>, rowidents: &'a [u64], snap: &'a Snapshot<'mcx>, out: &'a mut dyn FnMut(u64, TombHit)) -> PgResult<()>
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TombInsert {
    Won(ItemPointerData),
    Lost(TransactionId),
    /// `tomb` stopped taking new tombstones before the confirm; the caller inserts again into the heap that does.
    Retry,
}

seam_core::seam!(
    pub fn tomb_insert_lock<'a, 'mcx>(mcx: Mcx<'mcx>, tomb: &'a Relation<'mcx>, rowident: u64, cid: CommandId, mode: LockTupleMode, current: &'a mut dyn FnMut() -> PgResult<bool>) -> PgResult<TombInsert>
);

seam_core::seam!(
    pub fn tomb_update<'a, 'mcx>(mcx: Mcx<'mcx>, tomb: &'a Relation<'mcx>, otid: ItemPointerData, t: Tombstone, cid: CommandId) -> PgResult<ItemPointerData>
);

seam_core::seam!(
    pub fn tomb_insert<'a, 'mcx>(mcx: Mcx<'mcx>, tomb: &'a Relation<'mcx>, t: Tombstone, cid: CommandId, current: &'a mut dyn FnMut() -> PgResult<bool>) -> PgResult<TombInsert>
);

seam_core::seam!(
    /// `None`: the provider is off, and the fetch probes the tombstone index per hit instead.
    pub fn statement_deletes<'a, 'mcx>(mcx: Mcx<'mcx>, rel: &'a Relation<'mcx>, gen: Option<&'a Arc<pgrc_am::catalog::Generation>>, segs: &'a [pgrc_format::manifest::SegmentRecord], snap: &'a types_snapshot::SnapshotData<'mcx>) -> PgResult<Option<Box<dyn pgrc_am::scan::DeletionFilter>>>
);
