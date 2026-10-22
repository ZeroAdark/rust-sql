//! BootStrapXLOG / InitControlFile / WriteControlFile (xlog.c).

use std::sync::atomic::Ordering::Relaxed;

use controldata_utils::{
    CheckPoint, ControlFileData, CATALOG_VERSION_NO, DB_SHUTDOWNED, MOCK_AUTH_NONCE_LEN,
    PG_CONTROL_FILE_SIZE, PG_CONTROL_VERSION, SIZEOF_CHECKPOINT, XLOG_CONTROL_FILE,
};
use elog::{ereport, PCT_M};
use types_core::{FullTransactionId, Oid, FirstNormalTransactionId, InvalidTransactionId};
use types_error::{PgResult, PANIC, ERRCODE_INTERNAL_ERROR};

use crate::control_file::{control_file_update, ReadControlFile, FLOATFORMAT_VALUE, FirstNormalUnloggedLSN, TOAST_MAX_CHUNK_SIZE};
use crate::write::XLogFileInit;
use crate::*;

/// pg_multixact FirstMultiXactId (multixact.h); duplicated to avoid a
const FIRST_MULTI_XACT_ID: u32 = 1;
const TEMPLATE1_DB_OID: Oid = 1;
const BOOTSTRAP_TIMELINE_ID: TimeLineID = 1;
const XLR_BLOCK_ID_DATA_SHORT: u8 = 255;
const SIZE_OF_XLOG_RECORD_DATA_HEADER_SHORT: usize = 2;
pub const PG_DATA_CHECKSUM_VERSION: u32 = 1;

const PG_WAIT_IO: u32 = 0x0A00_0000;
const WAIT_EVENT_WAL_BOOTSTRAP_WRITE: u32 = PG_WAIT_IO | 70;
const WAIT_EVENT_WAL_BOOTSTRAP_SYNC: u32 = PG_WAIT_IO | 69;
const WAIT_EVENT_CONTROL_FILE_WRITE: u32 = PG_WAIT_IO | 9;
const WAIT_EVENT_CONTROL_FILE_SYNC: u32 = PG_WAIT_IO | 6;

#[track_caller]
fn loc(func: &'static str) -> types_error::ErrorLocation {
    let site = core::panic::Location::caller();
    types_error::ErrorLocation::new(site.file(), site.line() as i32, func)
}

fn put_u16(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_ne_bytes());
}
fn put_u32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_ne_bytes());
}
fn put_u64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_ne_bytes());
}

// The bootstrap WAL segment messages name no file (xlog.c:5194-5208); the
// control-file ones do.
fn file_errmsg(what: &str, path: Option<&str>) -> String {
    match path {
        Some(path) => format!("could not {what} \"{path}\": {PCT_M}"),
        None => format!("could not {what}: {PCT_M}"),
    }
}

fn file_panic(en: i32, what: &str, path: Option<&str>) -> Box<types_error::PgError> {
    let en = if en != 0 { en } else { libc::ENOSPC };
    match ereport(PANIC)
        .with_saved_errno(en)
        .errcode_for_file_access()
        .errmsg(file_errmsg(what, path))
        .finish(loc("BootStrapXLOG"))
    {
        Err(e) => e,
        Ok(()) => unreachable!("PANIC report returned"),
    }
}

/// `BootStrapXLOG(data_checksum_version)`.
pub fn BootStrapXLOG(data_checksum_version: u32) -> PgResult<()> {
    SetInstallXLogFileSegmentActive()?;

    // sysidentifier = seconds << 32 | usecs << 12 | pid & 0xFFF, from the
    let now_us = pg_clock::wall_us();
    let secs = (now_us / 1_000_000) as u64;
    let usecs = (now_us % 1_000_000) as u64;
    let sysidentifier = (secs << 32) | (usecs << 12) | (init_small::globals::MyProcPid() as u64 & 0xFFF);

    let wal_segsz = wal_segment_size();
    let check_point = CheckPoint {
        redo: wal_segsz as u64 + SizeOfXLogLongPHD as u64,
        ThisTimeLineID: BOOTSTRAP_TIMELINE_ID,
        PrevTimeLineID: BOOTSTRAP_TIMELINE_ID,
        fullPageWrites: guc_tables::vars::fullPageWrites.read(),
        wal_level: wal_level(),
        nextXid: FullTransactionId { value: FirstNormalTransactionId as u64 },
        nextOid: types_core::FirstGenbkiObjectId,
        nextMulti: FIRST_MULTI_XACT_ID,
        nextMultiOffset: 0,
        oldestXid: FirstNormalTransactionId,
        oldestXidDB: TEMPLATE1_DB_OID,
        oldestMulti: FIRST_MULTI_XACT_ID,
        oldestMultiDB: TEMPLATE1_DB_OID,
        time: secs as i64,
        oldestCommitTsXid: InvalidTransactionId,
        newestCommitTsXid: InvalidTransactionId,
        oldestActiveXid: InvalidTransactionId,
    };

    procarray::TransamVariables().nextXid.store(check_point.nextXid.value, Relaxed);
    varsup::TransamVariables().nextOid.store(check_point.nextOid, Relaxed);
    varsup::TransamVariables().oidCount.store(0, Relaxed);
    // MultiXactSetNextMXact / SetMultiXactIdLimit / SetCommitTsLimit are the
    varsup::AdvanceOldestClogXid(check_point.oldestXid)?;
    varsup::SetTransactionIdLimit(check_point.oldestXid, check_point.oldestXidDB)?;

    // The first WAL page: long header + the shutdown checkpoint record.
    let mut page = vec![0u8; XLOG_BLCKSZ];
    put_u16(&mut page, 0, XLOG_PAGE_MAGIC);
    put_u16(&mut page, 2, XLP_LONG_HEADER);
    put_u32(&mut page, 4, BOOTSTRAP_TIMELINE_ID);
    put_u64(&mut page, 8, wal_segsz as u64);
    put_u32(&mut page, 16, 0);
    put_u64(&mut page, 24, sysidentifier);
    put_u32(&mut page, 32, wal_segsz as u32);
    put_u32(&mut page, 36, XLOG_BLCKSZ as u32);

    let rec = SizeOfXLogLongPHD;
    let tot_len = (SizeOfXLogRecord + SIZE_OF_XLOG_RECORD_DATA_HEADER_SHORT + SIZEOF_CHECKPOINT) as u32;
    put_u32(&mut page, rec, tot_len);
    put_u32(&mut page, rec + 4, InvalidTransactionId);
    put_u64(&mut page, rec + 8, 0);
    page[rec + 16] = XLOG_CHECKPOINT_SHUTDOWN;
    page[rec + 17] = RM_XLOG_ID;
    let mut p = rec + SizeOfXLogRecord;
    page[p] = XLR_BLOCK_ID_DATA_SHORT;
    page[p + 1] = SIZEOF_CHECKPOINT as u8;
    p += SIZE_OF_XLOG_RECORD_DATA_HEADER_SHORT;
    page[p..p + SIZEOF_CHECKPOINT].copy_from_slice(&check_point.to_bytes());
    let mut crc = crc32c::CRC32C_INIT;
    crc = crc32c::pg_comp_crc32c(crc, &page[rec + SizeOfXLogRecord..rec + tot_len as usize]);
    crc = crc32c::pg_comp_crc32c(crc, &page[rec..rec + 20]);
    put_u32(&mut page, rec + 20, crc32c::fin_crc32c(crc));

    let f = XLogFileInit(1, BOOTSTRAP_TIMELINE_ID)?;
    write::report_wait_start(WAIT_EVENT_WAL_BOOTSTRAP_WRITE);
    let n = fd::pg_pwrite(f, &page, 0);
    write::report_wait_end();
    if n != XLOG_BLCKSZ as isize {
        return Err(file_panic(fd::get_errno(), "write bootstrap write-ahead log file", None));
    }
    write::report_wait_start(WAIT_EVENT_WAL_BOOTSTRAP_SYNC);
    let rc = fd::pg_fsync(f);
    write::report_wait_end();
    if rc != 0 {
        return Err(file_panic(fd::get_errno(), "fsync bootstrap write-ahead log file", None));
    }
    if fd::pg_close(f) != 0 {
        return Err(file_panic(fd::get_errno(), "close bootstrap write-ahead log file", None));
    }

    let mut cf = init_control_file(sysidentifier, data_checksum_version)?;
    cf.time = check_point.time;
    cf.checkPoint = check_point.redo;
    cf.checkPointCopy = check_point;
    write_control_file(&mut cf)?;
    control_file_update(|dst| *dst = cf);

    // Re-read so every derived field (data_checksums GUC etc.) is set the
    ReadControlFile()
}

/// `InitControlFile`: the status fields and replay-relevant parameters.
fn init_control_file(sysidentifier: u64, data_checksum_version: u32) -> PgResult<ControlFileData> {
    let mut nonce = [0u8; MOCK_AUTH_NONCE_LEN];
    if !pg_strong_random::pg_strong_random(&mut nonce) {
        return ereport(PANIC)
            .errcode(ERRCODE_INTERNAL_ERROR)
            .errmsg("could not generate secret authorization token")
            .finish(loc("InitControlFile"))
            .map(|_| unreachable!());
    }
    // SAFETY: ControlFileData is a plain repr(C) aggregate of integers,
    // bools and byte arrays; all-zero is a valid value (C memsets it).
    let mut cf: ControlFileData = unsafe { core::mem::zeroed() };
    cf.system_identifier = sysidentifier;
    cf.mock_authentication_nonce = nonce;
    cf.state = DB_SHUTDOWNED;
    cf.unloggedLSN = FirstNormalUnloggedLSN;
    cf.MaxConnections = guc_tables::vars::MaxConnections.read();
    cf.max_worker_processes = guc_tables::vars::max_worker_processes.read();
    cf.max_wal_senders = guc_tables::vars::max_wal_senders.read();
    cf.max_prepared_xacts = guc_tables::vars::max_prepared_xacts.read();
    cf.max_locks_per_xact = guc_tables::vars::max_locks_per_xact.read();
    cf.wal_level = wal_level();
    cf.wal_log_hints = guc_tables::vars::wal_log_hints.read();
    cf.track_commit_timestamp = guc_tables::vars::track_commit_timestamp.read();
    cf.data_checksum_version = data_checksum_version;
    Ok(cf)
}

/// `WriteControlFile`: fill the build-constant fields, CRC, create
/// global/pg_control (O_EXCL) as one PG_CONTROL_FILE_SIZE block.
fn write_control_file(cf: &mut ControlFileData) -> PgResult<()> {
    cf.pg_control_version = PG_CONTROL_VERSION;
    cf.catalog_version_no = CATALOG_VERSION_NO;
    cf.maxAlign = types_tuple::MAXIMUM_ALIGNOF as u32;
    cf.floatFormat = FLOATFORMAT_VALUE;
    cf.blcksz = types_core::BLCKSZ as u32;
    cf.relseg_size = types_storage::smgr::RELSEG_SIZE;
    cf.xlog_blcksz = XLOG_BLCKSZ as u32;
    cf.xlog_seg_size = wal_segment_size() as u32;
    cf.nameDataLen = types_core::fmgr::NAMEDATALEN as u32;
    cf.indexMaxKeys = types_core::fmgr::INDEX_MAX_KEYS as u32;
    cf.toast_max_chunk_size = TOAST_MAX_CHUNK_SIZE;
    cf.loblksize = types_storage::large_object::LOBLKSIZE as u32;
    cf.float8ByVal = true;
    cf.default_char_signedness = true;

    let image = cf.to_disk_bytes();
    cf.crc = controldata_utils::crc_of_image(&image);
    let image = cf.to_disk_bytes();
    let mut buffer = [0u8; PG_CONTROL_FILE_SIZE];
    buffer[..image.len()].copy_from_slice(&image);

    let f = fd::BasicOpenFile(XLOG_CONTROL_FILE, libc::O_RDWR | libc::O_CREAT | libc::O_EXCL)?;
    if f < 0 {
        return Err(file_panic(fd::get_errno(), "create file", Some(XLOG_CONTROL_FILE)));
    }
    write::report_wait_start(WAIT_EVENT_CONTROL_FILE_WRITE);
    let n = fd::pg_pwrite(f, &buffer, 0);
    write::report_wait_end();
    if n != PG_CONTROL_FILE_SIZE as isize {
        return Err(file_panic(fd::get_errno(), "write to file", Some(XLOG_CONTROL_FILE)));
    }
    write::report_wait_start(WAIT_EVENT_CONTROL_FILE_SYNC);
    let rc = fd::pg_fsync(f);
    write::report_wait_end();
    if rc != 0 {
        return Err(file_panic(fd::get_errno(), "fsync file", Some(XLOG_CONTROL_FILE)));
    }
    if fd::pg_close(f) != 0 {
        return Err(file_panic(fd::get_errno(), "close file", Some(XLOG_CONTROL_FILE)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use elog::PCT_M;

    #[test]
    fn bootstrap_wal_file_errors_name_no_path() {
        assert_eq!(
            super::file_errmsg("write bootstrap write-ahead log file", None),
            format!("could not write bootstrap write-ahead log file: {PCT_M}")
        );
        assert_eq!(
            super::file_errmsg("fsync file", Some("global/pg_control")),
            format!("could not fsync file \"global/pg_control\": {PCT_M}")
        );
    }
}
