//! The `pgrc` resource manager: [`insert`] logs, [`redo`] replays, [`cleanup`] ends recovery.

pub mod cleanup;
pub mod dbcopy;
pub mod fetch;
pub mod insert;
pub mod redo;
pub mod sync;

use pgrc_format::wal::DirLoc;
use types_core::INVALID_PROC_NUMBER;
use types_storage::storage::RelFileLocator;

pub use cleanup::{pgrc_cleanup, pgrc_startup};
pub use insert::{sink_for, XlogSink};
pub use redo::pgrc_redo;

pub fn locator(loc: DirLoc) -> RelFileLocator {
    RelFileLocator { spcOid: loc.spc, dbOid: loc.db, relNumber: loc.relnumber }
}

pub fn dir_loc(l: RelFileLocator) -> DirLoc {
    DirLoc { spc: l.spcOid, db: l.dbOid, relnumber: l.relNumber }
}

pub fn init_seams() {
    pgrc_wal_seams::sink_for::set(insert::sink_for);
    pgrc_wal_seams::take_insert_error::set(insert::take_insert_error);
    pgrc_wal_seams::log_row_delete::set(insert::log_row_delete);
    pgrc_wal_seams::log_row_update::set(insert::log_row_update);
    pgrc_wal_seams::log_bulk_rows::set(insert::log_bulk_rows);
    pgrc_wal_seams::log_rowid_reserve::set(insert::log_rowid_reserve);
    pgrc_wal_seams::log_rowid_set::set(insert::log_rowid_set);
    pgrc_wal_seams::log_rowid_extents::set(insert::log_rowid_extents);
    xlogrecovery_seams::pgrc_restartpoint_temps::set(cleanup::restartpoint_temps);
}

/// A logged directory's path relative to the data directory; logged tables are never temp.
pub fn dir_path(loc: DirLoc) -> String {
    pgrc_am::dirpath::table_dir_path(locator(loc), INVALID_PROC_NUMBER)
}

#[cfg(test)]
mod tests;
