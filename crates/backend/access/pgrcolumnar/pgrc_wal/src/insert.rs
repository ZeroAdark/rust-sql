//! Logging a table directory: the backend's [`WalSink`] and the directory-level loggers.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::atomic::Ordering::Relaxed;

use pgrc_format::spool::spool_name;
use pgrc_format::wal::{
    encode_dir, BulkRowsRec, DirLoc, FileRefRec, FileWriteRec, InstallRec, PublishRec, ReclaimRec, RowRec, RowidReserveRec, TupleImage,
    FILE_WRITE_MAX_CHUNK, XLOG_PGRC_BULK_ROWS, XLOG_PGRC_DIR_CREATE, XLOG_PGRC_FILE_CREATE, XLOG_PGRC_FILE_REF,
    XLOG_PGRC_FILE_WRITE, XLOG_PGRC_INSTALL, XLOG_PGRC_PUBLISH, XLOG_PGRC_RECLAIM, XLOG_PGRC_ROW_DELETE,
    XLOG_PGRC_ROWID_EXTENTS, XLOG_PGRC_ROWID_RESERVE, XLOG_PGRC_ROWID_SET, XLOG_PGRC_ROW_UPDATE, XLOG_PGRC_TRUNCATE,
};
use pgrc_write::plan::publish_touched;
use pgrc_write::walsink::{Apply, FileRef, NoWal, RefSync, WalSink, WAL_INSERT_FAILED};
use pgrc_write::wvfs::WriteVfs;
use pgrc_write::{WriteError, WriteResult};
use types_core::{ProcNumber, XLogRecPtr, RM_PGRC_ID};
use types_error::{PgError, PgResult};
use types_rel::RelationData;
use types_storage::DELAY_CHKPT_START;
use xloginsert::XLR_SPECIAL_REL_UPDATE;

thread_local! {
    static INSERT_ERROR: RefCell<Option<Box<PgError>>> = const { RefCell::new(None) };
}

pub fn take_insert_error() -> Option<Box<PgError>> {
    INSERT_ERROR.with(|e| e.borrow_mut().take())
}

pub(crate) fn stash(e: Box<PgError>) -> WriteError {
    INSERT_ERROR.with(|s| *s.borrow_mut() = Some(e));
    WriteError::Contract { detail: WAL_INSERT_FAILED }
}

/// Only `FILE_REF` carries `XLR_SPECIAL_REL_UPDATE`: stock pg_rewind copies `pgrc_<n>` directories whole and replay
/// runs over them idempotently.
fn insert(info: u8, data: &[&[u8]]) -> PgResult<XLogRecPtr> {
    xloginsert::insert_record(RM_PGRC_ID, info, 0, data, &[])
}

struct DelayChkpt(ProcNumber);

impl DelayChkpt {
    fn start() -> DelayChkpt {
        let me = lmgr_proc::MyProc().expect("pgrc WAL insert without MyProc");
        let prev = lmgr_proc::GetPGProcByNumber(me).delayChkptFlags.fetch_or(DELAY_CHKPT_START, Relaxed);
        debug_assert_eq!(prev & DELAY_CHKPT_START, 0);
        DelayChkpt(me)
    }
}

impl Drop for DelayChkpt {
    fn drop(&mut self) {
        lmgr_proc::GetPGProcByNumber(self.0).delayChkptFlags.fetch_and(!DELAY_CHKPT_START, Relaxed);
    }
}

/// Namespace records run insert, then `apply` and the sync registration in a critical section, all under
/// `DELAY_CHKPT_START`: once the record is in WAL, the primary's directory must match what redo builds.
pub struct XlogSink {
    loc: DirLoc,
    first_redo: BTreeMap<String, XLogRecPtr>,
}

impl XlogSink {
    pub fn new(loc: DirLoc) -> XlogSink {
        XlogSink { loc, first_redo: BTreeMap::new() }
    }

    fn logged(&mut self, info: u8, payload: &[u8], touched: Vec<String>, apply: Apply<'_>) -> WriteResult<()> {
        let _delay = DelayChkpt::start();
        let lsn = insert(info, &[payload]).map_err(stash)?;
        if info == XLOG_PGRC_PUBLISH {
            // record in, apply not run; only a wait may be attached.
            let _ = injection_point::injection_point("pgrc-publish-after-insert");
        }
        if info == XLOG_PGRC_RECLAIM && !touched.is_empty() {
            // record in, not flushed by the sink yet, nothing unlinked; only a wait may be attached.
            let _ = injection_point::injection_point("pgrc-reclaim-after-insert");
        }
        // a PUBLISH logged by a transaction with no xid has no commit record to flush it, and its apply moves CURRENT
        // at once, so it is flushed before its apply, as a RECLAIM is.
        let no_xid_publish = info == XLOG_PGRC_PUBLISH && !xact::GetTopFullTransactionIdIfAny().is_valid();
        init_small::globals::StartCriticalSection();
        // WAL before data: a RECLAIM's unlinks cannot be undone, so its record is flushed first.
        let flushed = if info == XLOG_PGRC_RECLAIM || no_xid_publish { transam_xlog::XLogFlush(lsn) } else { Ok(()) };
        let critical = flushed.and_then(|()| apply().map_err(pgrc_am::write_error)).and_then(|()| crate::sync::register(self.loc, touched));
        if let Err(err) = &critical {
            elog::panic_on_crit_section_escape(err);
        }
        init_small::globals::EndCriticalSection();
        if critical.is_ok() && no_xid_publish {
            // record in, applied: CURRENT names it; only a wait may be attached.
            let _ = injection_point::injection_point("pgrc-publish-noxid-after-apply");
        }
        critical.map_err(stash)
    }
}

impl WalSink for XlogSink {
    fn logs(&self) -> bool {
        true
    }

    fn file_write(&mut self, name: &str, off: u64, bytes: &[u8], create: bool) -> WriteResult<()> {
        self.first_redo.entry(name.to_string()).or_insert_with(transam_xlog::GetRedoRecPtr);
        let mut head = Vec::new();
        FileWriteRec { dir: self.loc, name, off, bytes }.encode_header(&mut head)?;
        let info = if create { XLOG_PGRC_FILE_CREATE } else { XLOG_PGRC_FILE_WRITE };
        insert(info, &[&head, bytes]).map_err(stash)?;
        // a paced seal may wait here: never inside a critical section or a DELAY_CHKPT_START window
        if pgrc_write::walsink::pace::active() {
            let crit = init_small::globals::CritSectionCount() > 0;
            let delaying = lmgr_proc::MyProc().is_some_and(|me| lmgr_proc::GetPGProcByNumber(me).delayChkptFlags.load(Relaxed) & DELAY_CHKPT_START != 0);
            debug_assert!(!crit && !delaying, "pgrc seal WAL pacing inside a critical section or DELAY_CHKPT_START window");
            if !crit && !delaying {
                pgrc_write::walsink::pace::paced(bytes.len() as u64);
            }
        }
        Ok(())
    }

    fn file_done(&mut self, name: &str, fsync: &mut dyn FnMut() -> WriteResult<()>) -> WriteResult<()> {
        let first = self.first_redo.remove(name);
        {
            let _delay = DelayChkpt::start();
            if first.is_none_or(|r| r == transam_xlog::GetRedoRecPtr()) {
                return crate::sync::register(self.loc, [name.to_string()]).map_err(stash);
            }
        }
        fsync()
    }

    fn publish(&mut self, rec: &PublishRec, apply: Apply<'_>) -> WriteResult<()> {
        debug_assert_eq!(rec.dir, self.loc);
        let mut b = Vec::new();
        rec.encode(&mut b)?;
        self.logged(XLOG_PGRC_PUBLISH, &b, publish_touched(rec), apply)
    }

    fn install(&mut self, rec: &InstallRec, apply: Apply<'_>) -> WriteResult<()> {
        let mut b = Vec::new();
        rec.encode(&mut b)?;
        self.logged(XLOG_PGRC_INSTALL, &b, rec.renames.iter().map(|(_, to)| to.clone()).collect(), apply)
    }

    fn reclaim(&mut self, rec: &ReclaimRec, apply: Apply<'_>) -> WriteResult<()> {
        let mut b = Vec::new();
        rec.encode(&mut b)?;
        self.logged(XLOG_PGRC_RECLAIM, &b, rec.names.clone(), apply)
    }

    fn register(&mut self, name: &str) -> WriteResult<()> {
        crate::sync::register(self.loc, [name.to_string()]).map_err(stash)
    }

    fn ref_min(&self) -> Option<u64> {
        ref_min_for(&crate::dir_path(self.loc))
    }

    /// The file and its directory are fsynced, the file is linked into the spool as pending and the spool fsynced; then,
    /// inside one `DELAY_CHKPT_START` window, `FILE_REF` is inserted and the pending entry renamed to the record's LSN.
    /// A crash anywhere leaves either no record or a record whose bytes are durable under one of the two spool names.
    /// Under `wal_level=minimal` the fsync is all; while a low-level backup runs, the file is logged as bytes.
    fn file_ref(&mut self, f: &FileRef<'_>, vfs: &mut dyn WriteVfs, fsync: RefSync<'_>) -> WriteResult<()> {
        fsync(vfs)?;
        injection_point::injection_point("pgrc-ref-after-fsync").map_err(stash)?;
        if !transam_xlog::XLogIsNeeded() {
            return Ok(());
        }
        let Some(ticket) = pgrc_spool::begin_ref_unless_backup() else { return self.log_bytes_of(vfs, f) };
        let spool = pgrc_spool::Spool::data();
        let table_path = format!("{}/{}", f.dir, f.name);
        let pending = match spool.link_pending(&table_path, self.loc, f.name) {
            Ok(p) => p,
            Err(e) => {
                pgrc_spool::end_ref(ticket, None);
                if link_unsupported(&e) {
                    return self.log_bytes_of(vfs, f);
                }
                return Err(io_error("link into pg_pgrc_spool", &table_path, &e));
            }
        };
        if let Err(e) = injection_point::injection_point("pgrc-ref-after-link") {
            pgrc_spool::end_ref(ticket, None);
            return Err(stash(e));
        }
        let mut b = Vec::new();
        if let Err(e) = (FileRefRec { dir: self.loc, name: f.name, len: f.len, crc: f.crc, masks: f.masks }.encode(&mut b)) {
            pgrc_spool::end_ref(ticket, None);
            return Err(e.into());
        }
        let _delay = DelayChkpt::start();
        // Kept, so stock pg_rewind refuses: it deletes the target's spool entries and its O_TRUNC copies write
        // through their hard links.
        if let Err(e) = insert(XLOG_PGRC_FILE_REF | XLR_SPECIAL_REL_UPDATE, &[&b]) {
            pgrc_spool::end_ref(ticket, None);
            return Err(stash(e));
        }
        let lsn = transam_xlog::ProcLastRecPtr();
        // The record is in: only a wait may be attached here (the crash e2e's kill point).
        let _ = injection_point::injection_point("pgrc-ref-after-insert");
        init_small::globals::StartCriticalSection();
        let critical = spool
            .name_final(&pending, lsn)
            .map_err(|e| pgrc_am::write_error(io_error("rename in pg_pgrc_spool", &pending, &e)))
            .and_then(|()| crate::sync::register(crate::sync::SPOOL_LOC, [spool_name(lsn)]));
        if let Err(err) = &critical {
            elog::panic_on_crit_section_escape(err);
        }
        init_small::globals::EndCriticalSection();
        pgrc_spool::end_ref(ticket, critical.is_ok().then_some(lsn));
        critical.map_err(stash)?;
        drop(_delay);
        // A walsender sends the file only once its FILE_REF is flushed, and a reference in a partly filled page
        // otherwise waits for the page to fill or for a commit. Durability is unchanged.
        transam_xlog::XLogSetAsyncXactLSN(transam_xlog::XactLastRecEnd());
        transam_xlog::spool_grew(lsn).map_err(stash)?;
        injection_point::injection_point("pgrc-ref-after-rename").map_err(stash)
    }
}

impl XlogSink {
    fn log_bytes_of(&mut self, vfs: &mut dyn WriteVfs, f: &FileRef<'_>) -> WriteResult<()> {
        let path = format!("{}/{}", f.dir, f.name);
        let mut off = 0u64;
        loop {
            let chunk = vfs.read_at(&path, off, FILE_WRITE_MAX_CHUNK)?;
            if chunk.is_empty() && off > 0 {
                break;
            }
            self.file_write(f.name, off, &chunk, off == 0)?;
            off += chunk.len() as u64;
            if chunk.len() < FILE_WRITE_MAX_CHUNK {
                break;
            }
        }
        self.file_done(f.name, &mut || Ok(()))
    }
}

fn link_unsupported(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::EXDEV | libc::EPERM | libc::EMLINK | libc::ENOTSUP))
}

fn io_error(op: &'static str, path: &str, e: &std::io::Error) -> WriteError {
    WriteError::Io { op, path: path.to_string(), errno: e.raw_os_error().unwrap_or(libc::EIO) }
}

const PARTS_IN_WAL_GUC: &str = "pgrust.pgrc_parts_in_wal";
const REF_MIN_GUC: &str = "pgrust.pgrc_ref_min_bytes";
const ARCHIVE_PARTS_GUC: &str = "pgrust.pgrc_archive_parts";
pub const DEFAULT_REF_MIN: u64 = 1 << 20;

/// `auto` and `reference` are opt-in only: stock pg_rewind refuses a node whose divergent WAL holds a `FILE_REF`,
/// and recovery from an archive built only by stock `pg_receivewal` stops at one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartsInWal {
    Bytes,
    Reference,
    Auto,
}

impl PartsInWal {
    pub fn parse(v: &str) -> Option<PartsInWal> {
        Some(match v.trim().to_ascii_lowercase().as_str() {
            "bytes" => PartsInWal::Bytes,
            "reference" => PartsInWal::Reference,
            "auto" => PartsInWal::Auto,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefPolicy {
    pub mode: PartsInWal,
    pub ref_min: u64,
    pub archive_parts: bool,
}

impl RefPolicy {
    /// At `wal_level=minimal` a file past it is only fsynced, in every mode: only crash recovery reads WAL there.
    pub fn ref_min(&self, archiving: bool, wal_needed: bool, same_device: impl FnOnce() -> bool) -> Option<u64> {
        if !wal_needed {
            return Some(self.ref_min.max(1));
        }
        if self.mode == PartsInWal::Bytes || (archiving && !self.archive_parts) || !same_device() {
            return None;
        }
        Some(self.ref_min.max(1))
    }
}

fn raw_setting(name: &str) -> Option<String> {
    if !guc_seams::get_config_option_missing_ok::is_installed() {
        return None;
    }
    guc_seams::get_config_option_missing_ok::call(name).ok().flatten()
}

/// Read at each by-reference decision (once per file close): a session's `SET` or `PGOPTIONS` applies to its own
/// writes, and a pooled thread never carries another session's answer.
pub fn ref_policy() -> RefPolicy {
    RefPolicy {
        mode: raw_setting(PARTS_IN_WAL_GUC).and_then(|v| PartsInWal::parse(&v)).unwrap_or(PartsInWal::Bytes),
        ref_min: raw_setting(REF_MIN_GUC).and_then(|v| v.trim().parse().ok()).unwrap_or(DEFAULT_REF_MIN),
        archive_parts: raw_setting(ARCHIVE_PARTS_GUC).is_some_and(|v| matches!(v.trim(), "on" | "true" | "1" | "yes")),
    }
}

pub fn ref_min_for(table_dir: &str) -> Option<u64> {
    ref_policy().ref_min(transam_xlog::XLogArchivingActive(), transam_xlog::XLogIsNeeded(), || pgrc_spool::same_device(table_dir))
}

pub fn relation_needs_wal(rel: &RelationData<'_>) -> bool {
    rel.is_permanent()
        && (transam_xlog::XLogStandbyInfoActive()
            || (rel.rd_createSubid.get() == types_core::InvalidSubTransactionId
                && rel.rd_firstRelfilelocatorSubid.get() == types_core::InvalidSubTransactionId))
}

/// Every file and name it changes is registered for the next checkpoint. With `pgrust.pgrc_unlogged_sync=off`
/// nothing is registered or fsynced, ever; the directory's marker makes a start on another boot reset the table.
struct UnloggedSink {
    loc: DirLoc,
    sync: bool,
}

impl UnloggedSink {
    fn note(&mut self, names: Vec<String>) -> WriteResult<()> {
        if !self.sync {
            return pgrc_am::dirpath::mark_unsynced(&crate::dir_path(self.loc)).map_err(stash);
        }
        crate::sync::register(self.loc, names).map_err(stash)
    }
}

const UNLOGGED_SYNC_GUC: &str = "pgrust.pgrc_unlogged_sync";

pub fn unlogged_sync() -> bool {
    !raw_setting(UNLOGGED_SYNC_GUC).is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "off" | "false" | "0" | "no"))
}

impl WalSink for UnloggedSink {
    fn logs(&self) -> bool {
        false
    }

    fn file_write(&mut self, _: &str, _: u64, _: &[u8], _: bool) -> WriteResult<()> {
        Ok(())
    }

    fn file_done(&mut self, name: &str, _: &mut dyn FnMut() -> WriteResult<()>) -> WriteResult<()> {
        self.note(vec![name.to_string()])
    }

    fn publish(&mut self, rec: &PublishRec, apply: Apply<'_>) -> WriteResult<()> {
        apply()?;
        self.note(publish_touched(rec))
    }

    fn install(&mut self, rec: &InstallRec, apply: Apply<'_>) -> WriteResult<()> {
        apply()?;
        self.note(rec.renames.iter().map(|(_, to)| to.clone()).collect())
    }

    fn reclaim(&mut self, rec: &ReclaimRec, apply: Apply<'_>) -> WriteResult<()> {
        apply()?;
        self.note(rec.names.clone())
    }

    fn register(&mut self, name: &str) -> WriteResult<()> {
        self.note(vec![name.to_string()])
    }
}

pub fn sink_for(rel: &RelationData<'_>) -> Box<dyn WalSink + Send> {
    let loc = crate::dir_loc(rel.rd_locator.get());
    if relation_needs_wal(rel) {
        Box::new(XlogSink::new(loc))
    } else if rel.rd_rel.relpersistence == types_core::catalog::RELPERSISTENCE_UNLOGGED {
        Box::new(UnloggedSink { loc, sync: unlogged_sync() })
    } else {
        Box::new(NoWal)
    }
}

fn log_dir(info: u8, loc: DirLoc) -> PgResult<XLogRecPtr> {
    let mut b = Vec::new();
    encode_dir(loc, &mut b);
    insert(info, &[&b])
}

pub fn log_dir_create(loc: DirLoc) -> PgResult<XLogRecPtr> {
    log_dir(XLOG_PGRC_DIR_CREATE, loc)
}

pub fn log_truncate(loc: DirLoc) -> PgResult<XLogRecPtr> {
    log_dir(XLOG_PGRC_TRUNCATE, loc)
}

/// A WAL-logged table inserts `ROWID_RESERVE`, then writes its `rowid` file and queues the file's sync, all under
/// `DELAY_CHKPT_START`: a checkpoint whose redo pointer follows the record syncs the file, and one before it replays the record.
pub fn log_rowid_reserve(rel: &RelationData<'_>, dir: &str, high: u64) -> PgResult<()> {
    log_rowid_mark(rel, dir, high, XLOG_PGRC_ROWID_RESERVE)
}

fn log_rowid_mark(rel: &RelationData<'_>, dir: &str, high: u64, op: u8) -> PgResult<()> {
    let loc = crate::dir_loc(rel.rd_locator.get());
    let write = || pgrc_am::rowid_alloc::write_file(&mut pgrc_write::wvfs::RealVfs, dir, high).map_err(pgrc_am::write_error);
    let name = || [pgrc_format::dirlayout::ROWID_FILE_NAME.to_string()];
    if !relation_needs_wal(rel) {
        write()?;
        return unlogged_or_minimal_sync(rel, loc, name());
    }
    let mut b = Vec::with_capacity(pgrc_format::wal::DIR_LOC_LEN + 8);
    RowidReserveRec { dir: loc, high }.encode(&mut b);
    let _delay = DelayChkpt::start();
    insert(op, &[&b])?;
    init_small::globals::StartCriticalSection();
    let critical = write().and_then(|()| crate::sync::register(loc, name()));
    if let Err(err) = &critical {
        elog::panic_on_crit_section_escape(err);
    }
    init_small::globals::EndCriticalSection();
    critical
}

/// A temp table's is never synced; an unlogged table's follows `pgrust.pgrc_unlogged_sync` as its parts do; a
/// permanent table skipping WAL registers it, and its commit syncs the directory.
fn unlogged_or_minimal_sync(rel: &RelationData<'_>, loc: DirLoc, names: [String; 1]) -> PgResult<()> {
    match rel.rd_rel.relpersistence {
        types_core::catalog::RELPERSISTENCE_TEMP => Ok(()),
        types_core::catalog::RELPERSISTENCE_UNLOGGED if !unlogged_sync() => pgrc_am::dirpath::mark_unsynced(&crate::dir_path(loc)),
        _ => crate::sync::register(loc, names),
    }
}

pub fn log_rowid_set(rel: &RelationData<'_>, dir: &str, high: u64) -> PgResult<()> {
    log_rowid_mark(rel, dir, high, XLOG_PGRC_ROWID_SET)
}

pub fn log_rowid_extents(rel: &RelationData<'_>, dir: &str, image: &[u8]) -> PgResult<()> {
    let loc = crate::dir_loc(rel.rd_locator.get());
    let write = || pgrc_am::rowid_alloc::write_segext(&mut pgrc_write::wvfs::RealVfs, dir, image).map_err(pgrc_am::write_error);
    let name = || [pgrc_format::dirlayout::SEGEXT_FILE_NAME.to_string()];
    if !relation_needs_wal(rel) {
        write()?;
        return unlogged_or_minimal_sync(rel, loc, name());
    }
    let mut b = ::mcx::stdvec::try_vec(pgrc_format::wal::DIR_LOC_LEN + image.len(), "pgrc rowid extents WAL")?;
    loc.encode_into(&mut b);
    b.extend_from_slice(image);
    let _delay = DelayChkpt::start();
    insert(XLOG_PGRC_ROWID_EXTENTS, &[&b])?;
    init_small::globals::StartCriticalSection();
    let critical = write().and_then(|()| crate::sync::register(loc, name()));
    if let Err(err) = &critical {
        elog::panic_on_crit_section_escape(err);
    }
    init_small::globals::EndCriticalSection();
    critical
}

fn format_error(e: pgrc_format::FormatError) -> Box<PgError> {
    Box::new(PgError::error(format!("pgrc logical record: {e}")))
}

/// Under the transaction's xid (assigned here, so decode files the change under the right (sub)transaction). They
/// change no file: no `DELAY_CHKPT_START`, no `XLR_SPECIAL_REL_UPDATE`.
fn log_logical(info: u8, data: &[&[u8]]) -> PgResult<XLogRecPtr> {
    debug_assert!(transam_xlog::XLogLogicalInfoActive());
    xact::GetCurrentTransactionId()?;
    xloginsert::insert_record(RM_PGRC_ID, info, transam_xlog::XLOG_INCLUDE_ORIGIN, data, &[])
}

fn log_row(op: u8, rec: RowRec<'_>) -> PgResult<XLogRecPtr> {
    let mut b = Vec::new();
    rec.encode(op, &mut b).map_err(format_error)?;
    log_logical(op, &[&b])
}

pub fn log_row_delete(parent: DirLoc, old: Option<TupleImage<'_>>) -> PgResult<XLogRecPtr> {
    log_row(XLOG_PGRC_ROW_DELETE, RowRec { parent, old, new: None })
}

pub fn log_row_update(parent: DirLoc, old: Option<TupleImage<'_>>, new: TupleImage<'_>) -> PgResult<XLogRecPtr> {
    log_row(XLOG_PGRC_ROW_UPDATE, RowRec { parent, old, new: Some(new) })
}

pub fn log_bulk_rows(parent: DirLoc, ntuples: u32, tuples: &[u8]) -> PgResult<XLogRecPtr> {
    let mut head = Vec::new();
    BulkRowsRec { parent, ntuples, tuples }.encode_header(&mut head).map_err(format_error)?;
    log_logical(XLOG_PGRC_BULK_ROWS, &[&head, tuples])
}
