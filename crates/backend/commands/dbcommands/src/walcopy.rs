use elog::{ereport, PCT_M};
use mcx::Mcx;
use types_core::primitive::RmgrIds;
use types_core::{
    BlockNumber, ForkNumber, InvalidOid, Oid, INVALID_PROC_NUMBER, RELPERSISTENCE_PERMANENT,
    RELPERSISTENCE_TEMP,
};
use types_error::{PgResult, ERROR};
use types_rel::{LockRelId, RELKIND_HAS_STORAGE};
use types_storage::buf::BufferAccessStrategyType;
use types_storage::bufpage::PageRef;
use types_storage::lock::AccessShareLock;
use types_storage::{ReadBufferMode, RelFileLocator, RelFileLocatorBackend};
use types_tuple::{HeapTupleData, ItemPointerData};

use crate::XLOG_DBASE_CREATE_WAL_LOG;

// The PG_VERSION fsync failure of CreateDirAndVersionFile (dbcommands.c:513):
// raised at data_sync_elevel(ERROR) — PANIC unless data_sync_retry is on,
// since un-synced data must not be retried at a catchable level (fd.c:4001).
fn version_file_fsync_error(versionfile: &str, errno: i32) -> Box<types_error::PgError> {
    ereport(fd::data_sync_elevel(ERROR))
        .with_saved_errno(errno)
        .errcode_for_file_access()
        .errmsg(format!("could not fsync file \"{versionfile}\": {PCT_M}"))
        .into_error()
        .into()
}

const RelationRelationId: Oid = 1259;
const PG_MAJORVERSION: &str = "18";
// wait_event_names.txt WaitEventIO order (PG_WAIT_IO | eventId).
const WAIT_EVENT_VERSION_FILE_SYNC: u32 = 0x0A00_0000 | 65;
const WAIT_EVENT_VERSION_FILE_WRITE: u32 = 0x0A00_0000 | 66;

pub(crate) struct CreateDBRelInfo {
    pub(crate) rlocator: RelFileLocator,
    pub(crate) reloid: Oid,
    pub(crate) relam: Oid,
    pub(crate) permanent: bool,
}

pub(crate) fn CreateDatabaseUsingWalLog(
    mcx: Mcx<'_>,
    src_dboid: Oid,
    dst_dboid: Oid,
    src_tsid: Oid,
    dst_tsid: Oid,
) -> PgResult<()> {
    let srcpath = relpath::GetDatabasePath(mcx, src_dboid, src_tsid)?;
    let dstpath = relpath::GetDatabasePath(mcx, dst_dboid, dst_tsid)?;

    CreateDirAndVersionFile(dstpath.as_str(), dst_dboid, dst_tsid, false)?;

    relmapper::RelationMapCopy(dst_dboid, dst_tsid, srcpath.as_str(), dstpath.as_str())?;

    let rlocatorlist = ScanSourceDatabasePgClass(src_tsid, src_dboid, srcpath.as_str())?;
    debug_assert!(!rlocatorlist.is_empty());

    for relinfo in &rlocatorlist {
        let srcrlocator = relinfo.rlocator;
        let dstrlocator = RelFileLocator {
            spcOid: if srcrlocator.spcOid == src_tsid { dst_tsid } else { srcrlocator.spcOid },
            dbOid: dst_dboid,
            relNumber: srcrlocator.relNumber,
        };

        let srcrelid = LockRelId { relId: relinfo.reloid, dbId: src_dboid };
        let dstrelid = LockRelId { relId: relinfo.reloid, dbId: dst_dboid };
        lmgr::LockRelationId(&srcrelid, AccessShareLock)?;
        lmgr::LockRelationId(&dstrelid, AccessShareLock)?;

        bufmgr::CreateAndCopyRelationData(srcrlocator, dstrlocator, relinfo.permanent)?;
        pgrc_wal::dbcopy::copy_dir_logged(srcrlocator, dstrlocator, relinfo.permanent)?;

        lmgr::UnlockRelationId(&srcrelid, AccessShareLock)?;
        lmgr::UnlockRelationId(&dstrelid, AccessShareLock)?;
    }
    Ok(())
}

/// pgrust-only additive probe (test-views.md mint-strategy addendum): the
/// number of relations a wal_log CREATE DATABASE of `src_dboid` would sweep
/// — `ScanSourceDatabasePgClass`'s list length, nothing else. The janitor's
/// per-template strategy pick prices wal_log with it. Caller must hold an
/// open transaction; cost is one read of the source database's pg_class
/// through shared buffers (bulk-read strategy), same as the wal_log scan
/// itself.
pub fn count_swept_relations(mcx: Mcx<'_>, src_dboid: Oid, src_tsid: Oid) -> PgResult<usize> {
    let srcpath = relpath::GetDatabasePath(mcx, src_dboid, src_tsid)?;
    Ok(ScanSourceDatabasePgClass(src_tsid, src_dboid, srcpath.as_str())?.len())
}

pub(crate) fn ScanSourceDatabasePgClass(
    tbid: Oid,
    dbid: Oid,
    srcpath: &str,
) -> PgResult<Vec<CreateDBRelInfo>> {
    let relfilenumber =
        relmapper::RelationMapOidToFilenumberForDatabase(srcpath, RelationRelationId)?;
    let mut rlocatorlist = Vec::new();
    scan_source_catalog(tbid, dbid, RelationRelationId, relfilenumber, &mut |tuple| {
        if let Some(relinfo) = scan_tuple(tuple, tbid, dbid, srcpath)? {
            rlocatorlist.push(relinfo);
        }
        Ok(())
    })?;
    Ok(rlocatorlist)
}

/// Visit every tuple of a source-database catalog visible to the latest
/// snapshot, reading its pages without a relcache entry (the pg_class scan of
/// CreateDatabaseUsingWalLog, dbcommands.c:271).
pub(crate) fn scan_source_catalog(
    tbid: Oid,
    dbid: Oid,
    catalog_relid: Oid,
    relfilenumber: Oid,
    visit: &mut dyn FnMut(&HeapTupleData<'_>) -> PgResult<()>,
) -> PgResult<()> {
    let relid = LockRelId { relId: catalog_relid, dbId: dbid };
    lmgr::LockRelationId(&relid, AccessShareLock)?;

    let rlocator = RelFileLocator { spcOid: tbid, dbOid: dbid, relNumber: relfilenumber };
    let key = RelFileLocatorBackend { locator: rlocator, backend: INVALID_PROC_NUMBER };
    smgr::smgropen(rlocator, INVALID_PROC_NUMBER)?;
    let nblocks = smgr::smgrnblocks(key, ForkNumber::MAIN_FORKNUM)?;
    smgr::smgrclose(key)?;

    let bstrategy = bufmgr::GetAccessStrategy(BufferAccessStrategyType::BasBulkread);

    let latest = snapmgr::GetLatestSnapshot()?;
    let snapshot = snapmgr::RegisterSnapshot(Some(&latest))?.expect("registered snapshot");

    for blkno in 0..nblocks {
        postgres_seams::check_for_interrupts::call()?;

        let buf = bufmgr::ReadBufferWithoutRelcache(
            rlocator,
            ForkNumber::MAIN_FORKNUM,
            blkno,
            ReadBufferMode::Normal,
            bstrategy.clone(),
            true,
        )?;

        bufmgr::LockBuffer(buf, bufmgr::BUFFER_LOCK_SHARE)?;
        // SAFETY: pinned, share-locked buffer page for the scan body.
        let page = unsafe { PageRef::from_raw(bufmgr::BufferGetPagePtr(buf)) };
        if page.is_new() || page.max_offset_number() == 0 {
            bufmgr::UnlockReleaseBuffer(buf)?;
            continue;
        }

        scan_page(page, buf, blkno, catalog_relid, &snapshot, visit)?;

        bufmgr::UnlockReleaseBuffer(buf)?;
    }
    snapmgr::UnregisterSnapshot(Some(&snapshot));

    lmgr::UnlockRelationId(&relid, AccessShareLock)?;
    Ok(())
}

fn scan_page(
    page: PageRef<'_>,
    buf: types_core::Buffer,
    blkno: BlockNumber,
    catalog_relid: Oid,
    snapshot: &snapmgr::Snapshot,
    visit: &mut dyn FnMut(&HeapTupleData<'_>) -> PgResult<()>,
) -> PgResult<()> {
    let maxoff = page.max_offset_number();
    for offnum in 1..=maxoff {
        let itemid = page.item_id(offnum);
        if !itemid.is_used() || itemid.is_dead() || itemid.is_redirected() {
            continue;
        }
        debug_assert!(itemid.is_normal());
        let mut tid = ItemPointerData::default();
        types_tuple::ItemPointerSet(&mut tid, blkno, offnum);
        // SAFETY: normal item id on a share-locked page; the item image is a
        // live heap tuple readable for its recorded length.
        let (item_ptr, item_len) = unsafe { page.item_raw_unchecked(itemid) };
        let mut tuple = unsafe {
            HeapTupleData::from_raw_parts(item_ptr, item_len, tid, catalog_relid)
        };

        if heapam_visibility_seams::heap_tuple_satisfies_visibility::call(
            &mut tuple,
            snapshot.as_ref(),
            buf,
        )? {
            visit(&tuple)?;
        }
    }
    Ok(())
}

// ScanSourceDatabasePgClassTuple: fixed-offset GETSTRUCT reads of the
// pg_class prefix (oid, relam, relfilenode, reltablespace, relpersistence, relkind);
// prefix columns precede any nullable/varlena attribute.
fn scan_tuple(
    tuple: &HeapTupleData<'_>,
    tbid: Oid,
    dbid: Oid,
    srcpath: &str,
) -> PgResult<Option<CreateDBRelInfo>> {
    let base = tuple.getstruct();
    // SAFETY: pg_class rows always carry the full fixed prefix (120 bytes);
    // offsets mirror FormData_pg_class field order in pg_class.h.
    let (oid, relam, relfilenode, reltablespace, relpersistence, relkind) = unsafe {
        let u32_at = |off: usize| -> u32 {
            u32::from_ne_bytes(core::slice::from_raw_parts(base.add(off), 4).try_into().unwrap())
        };
        (u32_at(0), u32_at(84), u32_at(88), u32_at(92), *base.add(118), *base.add(119))
    };

    if reltablespace == crate::GLOBALTABLESPACE_OID
        || !RELKIND_HAS_STORAGE(relkind)
        || relpersistence == RELPERSISTENCE_TEMP
    {
        return Ok(None);
    }

    let relfilenumber = if relfilenode != InvalidOid {
        relfilenode
    } else {
        relmapper::RelationMapOidToFilenumberForDatabase(srcpath, oid)?
    };

    if relfilenumber == InvalidOid {
        return Err(ereport(ERROR)
            .errmsg(format!("relation with OID {oid} does not have a valid relfilenumber"))
            .into_error()
            .into());
    }

    Ok(Some(CreateDBRelInfo {
        rlocator: RelFileLocator {
            spcOid: if reltablespace != InvalidOid { reltablespace } else { tbid },
            dbOid: dbid,
            relNumber: relfilenumber,
        },
        reloid: oid,
        relam,
        permanent: relpersistence == RELPERSISTENCE_PERMANENT,
    }))
}

pub(crate) fn CreateDirAndVersionFile(
    dbpath: &str,
    dbid: Oid,
    tsid: Oid,
    is_redo: bool,
) -> PgResult<()> {
    let buf = format!("{PG_MAJORVERSION}\n");

    if fd::MakePGDirectory(dbpath) < 0 {
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        if errno != libc::EEXIST || !is_redo {
            return Err(ereport(ERROR)
                .errcode_for_file_access()
                .with_saved_errno(errno)
                .errmsg(format!("could not create directory \"{dbpath}\": {PCT_M}"))
                .into_error()
                .into());
        }
    }

    let versionfile = format!("{dbpath}/PG_VERSION");

    let mut fdnum =
        fd::OpenTransientFile(&versionfile, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)?;
    if fdnum < 0 {
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        if errno == libc::EEXIST && is_redo {
            fdnum = fd::OpenTransientFile(&versionfile, libc::O_WRONLY | libc::O_TRUNC)?;
        }
    }
    if fdnum < 0 {
        return Err(ereport(ERROR)
            .errcode_for_file_access()
            .errmsg(format!("could not create file \"{versionfile}\": {PCT_M}"))
            .into_error()
            .into());
    }

    waitevent_seams::pgstat_report_wait_start::call(WAIT_EVENT_VERSION_FILE_WRITE);
    fd::set_errno(0);
    // SAFETY: fdnum is an open fd owned by the transient-file table.
    let written = unsafe { libc::write(fdnum, buf.as_ptr().cast(), buf.len()) };
    if written != buf.len() as isize {
        return Err(version_file_write_error(&versionfile, fd::get_errno()));
    }
    waitevent_seams::pgstat_report_wait_end::call();

    waitevent_seams::pgstat_report_wait_start::call(WAIT_EVENT_VERSION_FILE_SYNC);
    if fd::pg_fsync(fdnum) != 0 {
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        return Err(version_file_fsync_error(&versionfile, errno));
    }
    fd::fsync_fname(dbpath, true)?;
    waitevent_seams::pgstat_report_wait_end::call();

    fd::CloseTransientFile(fdnum);

    if !is_redo {
        init_small::globals::StartCriticalSection();
        let mut xlrec = [0u8; 8];
        xlrec[0..4].copy_from_slice(&dbid.to_ne_bytes());
        xlrec[4..8].copy_from_slice(&tsid.to_ne_bytes());
        let res = xloginsert::insert_record(
            RmgrIds::RM_DBASE_ID as u8,
            XLOG_DBASE_CREATE_WAL_LOG,
            0,
            &[&xlrec],
            &[],
        );
        init_small::globals::EndCriticalSection();
        res?;
    }
    Ok(())
}

// dbcommands.c:499-507: a short write that left errno unset is ENOSPC.
fn version_file_write_error(versionfile: &str, errno: i32) -> Box<types_error::PgError> {
    let errno = if errno == 0 { libc::ENOSPC } else { errno };
    ereport(ERROR)
        .with_saved_errno(errno)
        .errcode_for_file_access()
        .errmsg(format!("could not write to file \"{versionfile}\": {PCT_M}"))
        .into_error()
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_write_without_errno_is_disk_full() {
        let err = version_file_write_error("/base/16384/PG_VERSION", 0);
        assert_eq!(err.sqlstate(), types_error::ERRCODE_DISK_FULL);
        assert_eq!(
            err.message(),
            "could not write to file \"/base/16384/PG_VERSION\": No space left on device"
        );
        let err = version_file_write_error("/base/16384/PG_VERSION", libc::EIO);
        assert_eq!(err.sqlstate(), types_error::ERRCODE_IO_ERROR);
    }

    /// dbcommands.c:513: a failed fsync of the new database's PG_VERSION is
    /// raised at data_sync_elevel(ERROR) — PANIC at the data_sync_retry=off
    /// default (fd.c:4001), never a catchable ERROR that lets the transaction
    /// roll back over un-synced data.
    #[test]
    fn version_file_fsync_failure_is_data_sync_elevel() {
        let err = version_file_fsync_error("/base/16384/PG_VERSION", libc::EIO);
        assert_eq!(err.level(), fd::data_sync_elevel(ERROR));
        assert_eq!(err.level(), types_error::PANIC, "data_sync_retry defaults to off");
        assert_eq!(err.sqlstate(), types_error::ERRCODE_IO_ERROR);
        assert!(
            err.message().starts_with("could not fsync file \"/base/16384/PG_VERSION\": "),
            "{}",
            err.message()
        );
    }
}
