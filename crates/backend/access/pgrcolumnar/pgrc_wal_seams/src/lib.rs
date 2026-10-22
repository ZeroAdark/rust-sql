use pgrc_format::wal::{DirLoc, TupleImage};
use pgrc_write::walsink::WalSink;
use types_core::XLogRecPtr;
use types_error::{PgError, PgResult};
use types_rel::RelationData;

seam_core::seam!(
    pub fn sink_for(rel: &RelationData<'_>) -> Box<dyn WalSink + Send>
);

seam_core::seam!(
    pub fn take_insert_error() -> Option<Box<PgError>>
);

seam_core::seam!(
    pub fn log_row_delete(parent: DirLoc, old: Option<TupleImage<'_>>) -> PgResult<XLogRecPtr>
);

seam_core::seam!(
    pub fn log_row_update(parent: DirLoc, old: Option<TupleImage<'_>>, new: TupleImage<'_>) -> PgResult<XLogRecPtr>
);

// A table's row IDs below `high` are reserved durably, under DELAY_CHKPT_START.
seam_core::seam!(
    pub fn log_rowid_reserve(rel: &RelationData<'_>, dir: &str, high: u64) -> PgResult<()>
);

seam_core::seam!(
    pub fn log_rowid_set(rel: &RelationData<'_>, dir: &str, high: u64) -> PgResult<()>
);

seam_core::seam!(
    pub fn log_rowid_extents(rel: &RelationData<'_>, dir: &str, image: &[u8]) -> PgResult<()>
);

seam_core::seam!(
    pub fn log_bulk_rows(parent: DirLoc, ntuples: u32, tuples: &[u8]) -> PgResult<XLogRecPtr>
);
