//! pgrcolumnar parts by reference in a base backup (WR L4, docs WR.md "L4", OC-2). The backup's WAL may name files by
//! reference (`FILE_REF`); their bytes are not in WAL. `pg_pgrc_spool` is excluded from the directory walk (the
//! directory is sent empty), and after `do_pg_backup_stop` every spool entry of `[start, cover)` is appended to the
//! still-open main archive as `pg_pgrc_spool/<lsn>`, listed in the manifest like any file. The cover is read after
//! every WAL byte the archive holds was read (the `-X fetch` segments may run past the stop point), and a stock
//! client streaming the backup's WAL (`-X stream`) is held at the first reference at or past it (the walsender side
//! of `pgrc_spool::BackupCover`), so a restore never replays a reference its backup lacks.

use std::sync::Arc;

use elog::ereport;
use fd::PgPath;
use manifest::BackupManifestInfo;
use pgrc_format::spool::{spool_name, SPOOL_DIR};
use pgrc_spool::{BackupCover, Spool};
use sink::{Bbsink, BbsinkState};
use types_core::XLogRecPtr;
use types_error::{PgResult, ERROR, WARNING};

use crate::{loc, lstat_raw, sendFile, stat_error, INVALID_OID};

pub(crate) struct SpoolCover {
    cover: Arc<BackupCover>,
    done: bool,
}

impl Drop for SpoolCover {
    fn drop(&mut self) {
        if !self.done {
            self.cover.abort();
        }
    }
}

fn io_err<T>(e: std::io::Error, what: &str) -> PgResult<T> {
    ereport(ERROR)
        .with_saved_errno(e.raw_os_error().unwrap_or(0))
        .errcode_for_file_access()
        .errmsg(format!("{what}: %m"))
        .finish(loc("pgrc_append_spool"))?;
    unreachable!()
}

pub(crate) fn prepare() -> PgResult<()> {
    Spool::data().ensure_dir().or_else(|e| io_err(e, &format!("could not create directory \"{SPOOL_DIR}\"")))
}

pub(crate) fn register(start: XLogRecPtr) -> SpoolCover {
    let seg = transam_xlog::wal_segment_size() as u64;
    SpoolCover { cover: pgrc_spool::backup_begin(start, seg), done: false }
}

/// Whether entries of `[start, ..)` may have gone with their WAL segments: an ERROR, or while archiving (the archive
/// has the files as it has the WAL) one WARNING and `true`, so the missing entries are skipped.
fn removed(start: XLogRecPtr, tli: types_core::TimeLineID, warned: &mut bool) -> PgResult<bool> {
    let segno = start / transam_xlog::wal_segment_size() as u64;
    match transam_xlog::CheckXLogRemoved(segno, tli) {
        Ok(()) => Ok(false),
        Err(e) if transam_xlog::XLogArchivingActive() => {
            if !std::mem::replace(warned, true) {
                let _ = ereport(WARNING)
                    .errmsg(format!("{}; pgrcolumnar files its WAL references may be missing from this backup", e.message))
                    .errhint("Restore them from the WAL archive (pgrust.pgrc_archive_parts).")
                    .finish(loc("pgrc_append_spool"));
            }
            Ok(true)
        }
        Err(e) => Err(e),
    }
}

pub(crate) fn append_spool(
    sink: &mut Bbsink<'_>,
    state: &mut BbsinkState,
    manifest: &mut BackupManifestInfo,
    sc: &mut SpoolCover,
) -> PgResult<()> {
    let (start, tli) = (state.startptr, state.starttli);
    sc.cover.begin_listing();
    // On a primary the bound is the insert pointer: every reference the archive's WAL (or the stream's, up to now)
    // holds was inserted before it, so after settle it is named. A standby lands each file before the WAL naming it,
    // in LSN order, so everything landed is the bound.
    let in_recovery = transam_xlog::RecoveryInProgress();
    let bound = if in_recovery { u64::MAX } else { transam_xlog::GetXLogInsertRecPtr() };
    pgrc_spool::settle();
    let entries = Spool::data().list(start, bound).or_else(|e| io_err(e, &format!("could not list \"{SPOOL_DIR}\"")))?;
    let cover = if in_recovery { entries.last().map_or(start, |&(l, _)| l + 1) } else { bound };
    sc.cover.set_cover(cover);
    sc.done = true;
    let mut warned = false;
    removed(start, tli, &mut warned)?;

    for (lsn, _) in &entries {
        let path = PgPath::from(format!("{SPOOL_DIR}/{}", spool_name(*lsn)));
        let sent = match lstat_raw(&path) {
            Ok(statbuf) => sendFile(sink, state, &path, &path, &statbuf, true, INVALID_OID, INVALID_OID, None, manifest, None, 0)?,
            Err(libc::ENOENT) => false,
            Err(errnum) => return stat_error(&path, "file", errnum, "pgrc_append_spool"),
        };
        if !sent && !removed(start, tli, &mut warned)? {
            return stat_error(&path, "file", libc::ENOENT, "pgrc_append_spool");
        }
    }
    Ok(())
}
