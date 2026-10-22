//! `pgrc` redo: a record whose directory is gone is skipped (the table was dropped later in the WAL); a record that
//! does not decode is an ERROR, one that does not apply is a PANIC.

use pgrc_format::wal::{
    decode_dir, FileRefRec, FileWriteRec, InstallRec, PublishRec, ReclaimRec, Retire, RowidExtentsRec, RowidReserveRec, XLOG_PGRC_BULK_ROWS, XLOG_PGRC_DIR_CREATE,
    XLOG_PGRC_FILE_CREATE, XLOG_PGRC_FILE_REF, XLOG_PGRC_FILE_WRITE, XLOG_PGRC_INSTALL, XLOG_PGRC_OPMASK,
    XLOG_PGRC_PUBLISH, XLOG_PGRC_RECLAIM, XLOG_PGRC_ROWID_EXTENTS, XLOG_PGRC_ROWID_RESERVE, XLOG_PGRC_ROWID_SET, XLOG_PGRC_ROW_DELETE, XLOG_PGRC_ROW_UPDATE, XLOG_PGRC_TRUNCATE,
};
use pgrc_format::dirlayout::CURRENT_FILE_NAME;
use pgrc_format::manifest::Manifest;
use pgrc_format::{wal::DirLoc, FormatError};
use pgrc_write::plan::{self, ApplyMode};
use pgrc_write::wvfs::{RealVfs, WriteVfs};
use pgrc_write::{WriteError, WriteResult};
use pgrc_am::{catalog, dirpath, footer};
use types_error::{PgError, PgResult, ERRCODE_DATA_CORRUPTED, PANIC, WARNING};
use xlogreader_seams::XLogReaderState;

fn corrupt(op: &str, e: FormatError) -> Box<PgError> {
    Box::new(PgError::error(format!("pgrc {op} record is corrupt: {e}")).with_sqlstate(ERRCODE_DATA_CORRUPTED))
}

fn failed(op: &str, e: impl std::fmt::Display) -> Box<PgError> {
    Box::new(PgError::new(PANIC, format!("pgrc redo of {op} failed: {e}")))
}

pub type DirOf<'a> = &'a dyn Fn(DirLoc) -> PgResult<Option<String>>;

#[derive(Debug)]
pub enum Applied {
    Published(DirLoc, String, Manifest),
    Truncated(DirLoc, String),
    Copied(DirLoc, String),
    Retired(DirLoc, u64, Vec<String>),
    /// On a live standby, a retirement without a conflict and its unlinks, queued until the standby's horizon passes the xid.
    Deferred(DirLoc, String, u64, u32, Vec<String>),
}

fn slot(loc: DirLoc) -> catalog::RelSlot {
    (loc.spc, loc.db, loc.relnumber as u64)
}

fn existing(loc: DirLoc) -> PgResult<Option<String>> {
    let dir = crate::dir_path(loc);
    Ok(dirpath::dir_exists(&dir)?.then_some(dir))
}

pub fn pgrc_redo(record: &mut XLogReaderState) -> PgResult<()> {
    let rec = record.record.as_ref().expect("pgrc redo with no decoded record");
    let info = rec.xl_info & XLOG_PGRC_OPMASK;
    // SAFETY: main_data points into the reader's decode buffer, valid for the redo callback's duration.
    let data = unsafe { rec.main_data_bytes() };
    if info == XLOG_PGRC_DIR_CREATE {
        let loc = decode_dir(data).map_err(|e| corrupt("DIR_CREATE", e))?;
        let dir = crate::dir_path(loc);
        dirpath::mkdir_if_absent(&dir).map_err(|e| failed("DIR_CREATE", e.message()))?;
        // a reused relfilenumber's directory starts its counter from its own rowid file
        pgrc_am::rowid_alloc::forget(&dir);
        if catalog::is_live() {
            catalog::create(slot(loc), &dir);
        }
        return Ok(());
    }
    if info == XLOG_PGRC_RECLAIM {
        // The minimum recovery point covers this record before its unlinks: a standby that crashes after them replays
        // again from its last restartpoint, and must not reach consistency before this record again.
        transam_xlog::XLogFlush(record.EndRecPtr)?;
    }
    if info == XLOG_PGRC_FILE_REF {
        let r = FileRefRec::decode(data).map_err(|e| corrupt("FILE_REF", e))?;
        let Some(dir) = existing(r.dir)? else { return Ok(()) };
        let entry = crate::fetch::materialize(&mut RealVfs, &pgrc_spool::Spool::data(), record.ReadRecPtr, &r, &dir)?;
        crate::sync::register(crate::sync::SPOOL_LOC, [entry])?;
        return crate::sync::register(r.dir, [r.name.to_string()]);
    }
    if info == XLOG_PGRC_PUBLISH {
        freeze_conflict(data)?;
    }
    if info == XLOG_PGRC_RECLAIM {
        reclaim_conflict(data)?;
    }
    if let Some(a) = redo_op(&mut RealVfs, info, data, &existing)? {
        if catalog::is_live() {
            keep_catalog(a);
        }
    }
    // deferred retirements run once this standby's readers are past them (the horizon needs a PGPROC)
    match lmgr_proc::MyProc() {
        Some(_) => catalog::drain_deferred_now(),
        None => Ok(()),
    }
}

/// A frozen generation is visible to every snapshot the moment it applies. A hot standby's snapshot that did not see
/// the publisher yet is cancelled first, as heap's freeze records do; before the apply, so the catalog never offers
/// the frozen generation to such a snapshot.
fn freeze_conflict(data: &[u8]) -> PgResult<()> {
    use pgrc_format::wal::{PublishKind, PUBLISH_KIND_FLAGS};
    use pgrc_write::publish::{clog_bound, FROZEN_PUBLISHER_FXID};
    // the kind byte follows the dir (12 bytes) and the base generation (8): only a freeze record is decoded twice
    if data.get(20).is_none_or(|k| k & !PUBLISH_KIND_FLAGS != PublishKind::Freeze as u8) {
        return Ok(());
    }
    let r = PublishRec::decode(data).map_err(|e| corrupt("PUBLISH", e))?;
    if r.header.publisher_fxid != FROZEN_PUBLISHER_FXID || r.base_gen == 0 {
        return Ok(());
    }
    let Some(dir) = existing(r.dir)? else { return Ok(()) };
    let Some(base) = plan::read_manifest(&mut RealVfs, &dir, r.base_gen) else { return Ok(()) };
    let publisher = base.header.publisher_fxid;
    if !clog_bound(publisher) {
        return Ok(());
    }
    let locator = types_storage::RelFileLocator { spcOid: r.dir.spc, dbOid: r.dir.db, relNumber: r.dir.relnumber };
    standby::ResolveRecoveryConflictWithSnapshotFullXid(types_core::xact::FullTransactionId::from_u64(publisher), false, locator)
}

/// A retirement that redo applies at once unlinks files that a hot standby's snapshot not seeing `xid` may still open
/// by path: that conflict resolves first, as heap's prune records resolve before they change the page.
fn reclaim_conflict(data: &[u8]) -> PgResult<()> {
    let r = ReclaimRec::decode(data).map_err(|e| corrupt("RECLAIM", e))?;
    if !resolves_first(r.retire_below, catalog::is_live()) || existing(r.dir)?.is_none() {
        return Ok(());
    }
    standby::ResolveRecoveryConflictWithSnapshot(r.xid, false, crate::locator(r.dir))
}

pub(crate) fn resolves_first(retire: Option<Retire>, live: bool) -> bool {
    match retire {
        Some(Retire::Conflict(_)) => true,
        Some(Retire::Deferred(_)) => !live,
        None => false,
    }
}

/// A failed load drops the entry, so the table's readers error instead of reading an older generation.
pub fn keep_catalog(a: Applied) {
    match a {
        Applied::Published(loc, dir, m) => {
            if let Err(e) = catalog::redo_publish(slot(loc), &dir, m, &footer::fold_generation, &footer::serve_generation_without_catalog) {
                let _ = elog::elog(WARNING, format!("pgrcolumnar: {dir} left the catalog: {}", e.message()));
                catalog::remove_dir(&dir);
            }
        }
        Applied::Truncated(loc, dir) => catalog::redo_truncate(slot(loc), &dir),
        Applied::Copied(loc, dir) => pgrc_am::dircopy::load_copy(slot(loc), &dir),
        Applied::Retired(loc, keep_from, names) => {
            catalog::retire_below(slot(loc), keep_from);
            catalog::forget_dead_names(slot(loc), &names);
        }
        Applied::Deferred(loc, dir, keep_from, xid, names) => catalog::defer_retire(slot(loc), keep_from, xid, dir, names),
    }
}

pub fn redo_op(vfs: &mut dyn WriteVfs, info: u8, data: &[u8], dir_of: DirOf<'_>) -> PgResult<Option<Applied>> {
    match info {
        XLOG_PGRC_ROW_DELETE | XLOG_PGRC_ROW_UPDATE | XLOG_PGRC_BULK_ROWS => {}
        XLOG_PGRC_FILE_WRITE | XLOG_PGRC_FILE_CREATE => {
            let r = FileWriteRec::decode(data).map_err(|e| corrupt("FILE_WRITE", e))?;
            let Some(dir) = dir_of(r.dir)? else { return Ok(None) };
            let create = info == XLOG_PGRC_FILE_CREATE;
            plan::apply_file_write(vfs, &dir, r.name, r.off, r.bytes, create).map_err(|e| failed("FILE_WRITE", e))?;
            crate::sync::register(r.dir, [r.name.to_string()])?;
        }
        XLOG_PGRC_PUBLISH => {
            let r = PublishRec::decode(data).map_err(|e| corrupt("PUBLISH", e))?;
            let Some(dir) = dir_of(r.dir)? else { return Ok(None) };
            let Some(m) = plan::redo_publish(vfs, &dir, &r).map_err(|e| failed("PUBLISH", e))? else { return Ok(None) };
            crate::sync::register(r.dir, plan::publish_touched(&r))?;
            return Ok(Some(Applied::Published(r.dir, dir, m)));
        }
        XLOG_PGRC_INSTALL => {
            let r = InstallRec::decode(data).map_err(|e| corrupt("INSTALL", e))?;
            let Some(dir) = dir_of(r.dir)? else { return Ok(None) };
            plan::apply_install(vfs, &dir, &r, ApplyMode::Redo).map_err(|e| failed("INSTALL", e))?;
            let copied = r.renames.iter().any(|(_, to)| to == CURRENT_FILE_NAME);
            crate::sync::register(r.dir, r.renames.into_iter().map(|(_, to)| to))?;
            if copied {
                return Ok(Some(Applied::Copied(r.dir, dir)));
            }
        }
        XLOG_PGRC_RECLAIM => {
            let r = ReclaimRec::decode(data).map_err(|e| corrupt("RECLAIM", e))?;
            let Some(dir) = dir_of(r.dir)? else { return Ok(None) };
            if let (Some(Retire::Deferred(keep_from)), true) = (r.retire_below, catalog::is_live()) {
                return Ok(Some(Applied::Deferred(r.dir, dir, keep_from, r.xid, r.names)));
            }
            plan::apply_reclaim(vfs, &dir, &r).map_err(|e| failed("RECLAIM", e))?;
            crate::sync::register(r.dir, r.names.iter().cloned())?;
            if let Some(retire) = r.retire_below {
                return Ok(Some(Applied::Retired(r.dir, retire.keep_from(), r.names)));
            }
        }
        XLOG_PGRC_ROWID_RESERVE => {
            let r = RowidReserveRec::decode(data).map_err(|e| corrupt("ROWID_RESERVE", e))?;
            let Some(dir) = dir_of(r.dir)? else { return Ok(None) };
            pgrc_am::rowid_alloc::redo_reserve(vfs, &dir, r.high).map_err(|e| failed("ROWID_RESERVE", e))?;
            crate::sync::register(r.dir, [pgrc_format::dirlayout::ROWID_FILE_NAME.to_string()])?;
        }
        XLOG_PGRC_ROWID_SET => {
            let r = RowidReserveRec::decode(data).map_err(|e| corrupt("ROWID_SET", e))?;
            let Some(dir) = dir_of(r.dir)? else { return Ok(None) };
            pgrc_am::rowid_alloc::redo_set(vfs, &dir, r.high).map_err(|e| failed("ROWID_SET", e))?;
            crate::sync::register(r.dir, [pgrc_format::dirlayout::ROWID_FILE_NAME.to_string()])?;
        }
        XLOG_PGRC_ROWID_EXTENTS => {
            let r = RowidExtentsRec::decode(data).map_err(|e| corrupt("ROWID_EXTENTS", e))?;
            let Some(dir) = dir_of(r.dir)? else { return Ok(None) };
            let image = pgrc_format::rowid::encode_segext(&r.table).map_err(|e| corrupt("ROWID_EXTENTS", e))?;
            pgrc_am::rowid_alloc::redo_extents(vfs, &dir, &image).map_err(|e| failed("ROWID_EXTENTS", e))?;
            crate::sync::register(r.dir, [pgrc_format::dirlayout::SEGEXT_FILE_NAME.to_string()])?;
        }
        XLOG_PGRC_TRUNCATE => {
            let loc = decode_dir(data).map_err(|e| corrupt("TRUNCATE", e))?;
            let Some(dir) = dir_of(loc)? else { return Ok(None) };
            truncate(vfs, &dir).map_err(|e| failed("TRUNCATE", e))?;
            return Ok(Some(Applied::Truncated(loc, dir)));
        }
        _ => return Err(Box::new(PgError::new(PANIC, format!("pgrc_redo: unknown op code {info}")))),
    }
    Ok(None)
}

fn truncate(vfs: &mut dyn WriteVfs, dir: &str) -> WriteResult<()> {
    for n in vfs.list_dir(dir)? {
        match vfs.unlink_path(&format!("{dir}/{n}")) {
            Err(WriteError::Io { errno: libc::ENOENT, .. }) => {}
            r => r?,
        }
    }
    vfs.fsync_dir(dir)
}
