use std::rc::Rc;

use mcx::Mcx;
use tableam_vocab::VacuumParams;
use types_error::PgResult;
use types_rel::{Relation, RelationData};
use types_snapshot::SnapshotData;
use types_storage::buf::BufferAccessStrategy;
use types_tuple::ItemPointerData;

seam_core::seam!(
    pub fn table_relation_vacuum<'a, 'mcx>(
        mcx: Mcx<'mcx>,
        rel: &'a RelationData<'mcx>,
        params: &'a VacuumParams,
        bstrategy: BufferAccessStrategy,
    ) -> PgResult<()>
);

seam_core::seam!(
    pub fn pgrc_relation_vacuum<'a, 'mcx>(
        mcx: Mcx<'mcx>,
        rel: &'a RelationData<'mcx>,
        params: &'a VacuumParams,
        bstrategy: BufferAccessStrategy,
    ) -> PgResult<()>
);

seam_core::seam!(
    // `scan` runs every index's TID collection into the sink it is given; the answer is the TIDs of the rows dead to
    // everyone, ascending, for ambulkdelete, their standby conflict record already in WAL.
    pub fn pgrc_index_dead<'a, 'mcx>(
        mcx: Mcx<'mcx>,
        rel: &'a RelationData<'mcx>,
        scan: &'a mut dyn FnMut(&mut dyn FnMut(&ItemPointerData) -> PgResult<()>) -> PgResult<()>,
    ) -> PgResult<Vec<ItemPointerData>>
);

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct PgrcPassFacts {
    /// The cap on the relfrozenxid the pass may set (None: none; InvalidTransactionId: keep the current one).
    pub relfrozenxid_cap: Option<types_core::TransactionId>,
    /// None: never published (the pass's zeros stand).
    pub relstats: Option<(types_core::BlockNumber, f64)>,
}

seam_core::seam!(
    pub fn pgrc_freeze<'a, 'mcx>(
        mcx: Mcx<'mcx>,
        rel: &'a RelationData<'mcx>,
        oldest_xmin: types_core::TransactionId,
    ) -> PgResult<PgrcPassFacts>
);

pub const PGRC_VACFULL_RENUMBER: u32 = 3;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PgrcVacFull {
    pub phase: u32,
    pub steps: u32,
    pub sealed: u64,
    pub folded: u64,
    pub jobs: u64,
    pub removed: u64,
    pub parts_before: u64,
    pub parts_after: u64,
    pub rows_after: u64,
    pub held_bytes: u64,
    pub renumber: bool,
    pub renumbered: bool,
    pub visible: u64,
    pub cursor: u64,
    pub rewritten: u64,
    pub next_id: u64,
}

seam_core::seam!(
    // One step of the synchronous compaction per call, each in its own transaction under vacuum_rel's session
    // AccessExclusiveLock; false once done.
    pub fn pgrc_vacuum_full<'a, 'mcx>(
        mcx: Mcx<'mcx>,
        rel: &'a RelationData<'mcx>,
        params: &'a VacuumParams,
        bstrategy: BufferAccessStrategy,
        st: &'a mut PgrcVacFull,
    ) -> PgResult<bool>
);

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PgrcVacFlush {
    pub phase: u32,
    pub targets: Vec<(types_core::Oid, u32)>,
    pub sealed: u32,
}

seam_core::seam!(
    // One step of a manual lazy VACUUM's flush per call, each in its own ordinary (not PROC_IN_VACUUM) transaction
    // under vacuum_rel's session ShareUpdateExclusiveLock, before the heap pass; false once done.
    pub fn pgrc_vacuum_flush<'a, 'mcx>(
        mcx: Mcx<'mcx>,
        rel: &'a RelationData<'mcx>,
        st: &'a mut PgrcVacFlush,
    ) -> PgResult<bool>
);

seam_core::seam!(
    // currtid_internal (tid.c): scan-open + fetch + scan-close, bundled.
    pub fn table_tid_get_latest<'mcx>(
        mcx: Mcx<'mcx>,
        rel: Relation<'mcx>,
        snapshot: Rc<SnapshotData<'static>>,
        tid: ItemPointerData,
    ) -> PgResult<ItemPointerData>
);

seam_core::seam!(
    // get_table_am_oid (amcmds.c); tableamapi.c's GUC check hook reaches up
    // into commands (guc <- tableam <- amcmds is not a crate edge).
    pub fn get_table_am_oid(amname: &str, missing_ok: bool) -> PgResult<types_core::Oid>
);

seam_core::seam!(
    pub fn table_relation_dir_size<'a, 'mcx>(rel: &'a Relation<'mcx>) -> PgResult<u64>
);

seam_core::seam!(
    pub fn database_dir_am_size(db_path: &str) -> PgResult<u64>
);
