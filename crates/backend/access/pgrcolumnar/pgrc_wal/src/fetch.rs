//! Where redo of a `FILE_REF` finds the referenced bytes. Every source is verified with the record's masked crc.

use pgrc_format::spool::{archive_name, spool_name};
use pgrc_format::wal::FileRefRec;
use pgrc_spool::Spool;
use pgrc_write::plan::{self, RefSource};
use pgrc_write::wvfs::WriteVfs;
use pgrc_write::WriteError;
use types_core::XLogRecPtr;
use types_error::{PgError, PgResult, FATAL, PANIC, WARNING};

fn lsn_fmt(lsn: XLogRecPtr) -> String {
    format!("{:X}/{:X}", lsn >> 32, lsn as u32)
}

fn exists(vfs: &mut dyn WriteVfs, p: &str) -> PgResult<bool> {
    vfs.exists_path(p).map_err(|e| Box::new(PgError::new(PANIC, format!("pgrc redo of FILE_REF: {e}"))))
}

pub fn not_available(lsn: XLogRecPtr, rec: &FileRefRec<'_>, dir: &str) -> Box<PgError> {
    Box::new(
        PgError::new(
            FATAL,
            format!(
                "pgrc: the file referenced at {} ({dir}/{}, {} bytes) is not available",
                lsn_fmt(lsn),
                rec.name,
                rec.len
            ),
        )
        .with_hint(
            "Its bytes are not in WAL. Restore it through restore_command with pgrust.pgrc_archive_parts=on, or run \
             the primary with pgrust.pgrc_parts_in_wal=bytes."
                .to_string(),
        ),
    )
}

/// Returns the spool entry's name for the caller's sync registration.
pub fn materialize(vfs: &mut dyn WriteVfs, spool: &Spool, lsn: XLogRecPtr, rec: &FileRefRec<'_>, dir: &str) -> PgResult<String> {
    let entry = spool.path(lsn);
    let pending = spool.pending_path(rec.dir, rec.name);
    if !exists(vfs, &entry)? && exists(vfs, &pending)? {
        spool.name_final(&pending, lsn).map_err(|e| Box::new(PgError::new(PANIC, format!("pgrc redo of FILE_REF: rename {pending}: {e}"))))?;
    }
    let src = if exists(vfs, &entry)? { RefSource::Spool(entry.clone()) } else { RefSource::InPlace };
    let in_place = src == RefSource::InPlace;
    match plan::apply_file_ref(vfs, dir, rec, src) {
        Ok(()) => {}
        Err(WriteError::Format(_)) if in_place => {
            let Some(restored) = restore_from_archive(lsn, rec.len)? else { return Err(not_available(lsn, rec, dir)) };
            let r = plan::apply_file_ref(vfs, dir, rec, RefSource::Archive(restored.clone()));
            let _ = vfs.unlink_path(&restored);
            match r {
                Ok(()) => {}
                Err(WriteError::Format(_)) => {
                    return Err(Box::new(PgError::new(
                        FATAL,
                        format!(
                            "pgrc: the file restored from the archive for the reference at {} ({dir}/{}) does not hold the referenced bytes",
                            lsn_fmt(lsn),
                            rec.name
                        ),
                    )))
                }
                Err(e) => return Err(Box::new(PgError::new(PANIC, format!("pgrc redo of FILE_REF at {}: {e}", lsn_fmt(lsn))))),
            }
        }
        Err(e) => return Err(Box::new(PgError::new(PANIC, format!("pgrc redo of FILE_REF at {}: {e}", lsn_fmt(lsn))))),
    }
    if in_place {
        spool
            .link_final(&format!("{dir}/{}", rec.name), lsn)
            .map_err(|e| Box::new(PgError::new(PANIC, format!("pgrc redo of FILE_REF: link into pg_pgrc_spool: {e}"))))?;
    }
    // A cascading walsender on this standby lists the spool only past the newest entry it knows of.
    pgrc_spool::note_named(lsn);
    Ok(spool_name(lsn))
}

/// `None`: not archiving, or no timeline's name restored.
fn restore_from_archive(lsn: XLogRecPtr, len: u64) -> PgResult<Option<String>> {
    if !xlogarchive_seams::restore_archived_file::is_installed()
        || !xlogrecovery_seams::archive_recovery_requested::is_installed()
        || !xlogrecovery_seams::archive_recovery_requested::call()
    {
        return Ok(None);
    }
    let seg_size = transam_xlog::wal_segment_size();
    let segno = transam_xlog::XLByteToSeg(lsn, seg_size);
    let history_cx = mcx::MemoryContext::new("pgrc timeline history");
    let tles = timeline_seams::read_timeline_history::call(history_cx.mcx(), xlogrecovery_seams::recovery_target_tli::call())?;
    let first = timeline_seams::tli_of_point_in_history::call(lsn, &tles)?;
    let older = tles.iter().map(|t| t.tli).filter(|&t| t < first);
    for tli in std::iter::once(first).chain(older) {
        let name = archive_name(&transam_xlog::XLogFileName(tli, segno, seg_size), lsn, seg_size as u64);
        if let Some(p) = xlogarchive_seams::restore_archived_file::call(&name, "RECOVERYPGRC", len as i64, false)? {
            return Ok(Some(p));
        }
    }
    Ok(None)
}

/// A failure is a WARNING; the entries are harmless until the next recovery retries.
pub fn remove_spool_debris() {
    let end = if xlogrecovery_seams::get_xlog_replay_rec_ptr::is_installed() {
        xlogrecovery_seams::get_xlog_replay_rec_ptr::call().0
    } else {
        0
    };
    if let Err(e) = Spool::data().remove_debris(end) {
        let _ = elog::elog(WARNING, format!("pgrcolumnar: end-of-recovery cleanup of pg_pgrc_spool failed: {e}"));
    }
}
