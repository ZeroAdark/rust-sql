// storage_xlog.h + storage.c smgr_redo.
use types_core::{BlockNumber, ForkNumber, InvalidBlockNumber, INVALID_PROC_NUMBER, MAX_FORKNUM};
use elog::{ereport, PCT_M};
use types_error::{ErrorLocation, PgError, PgResult, ERRCODE_DATA_CORRUPTED, ERROR, PANIC};
use types_storage::{RelFileLocator, RelFileLocatorBackend};
use xlogreader_seams::XLogReaderState;

pub const XLOG_SMGR_CREATE: u8 = 0x10;
pub const XLOG_SMGR_TRUNCATE: u8 = 0x20;
/// pgrust-only: create a pgrcolumnar relation's table directory. Its payload
/// is the relation's RelFileLocator. Columnar data itself is never logged, so
/// only directories copied empty by CREATE DATABASE are logged this way.
pub const XLOG_SMGR_CREATE_TABLE_DIR: u8 = 0x70;
pub const SIZEOF_XL_SMGR_CREATE_TABLE_DIR: usize = 12;
/// pgrust-only (D3150): a pgrcolumnar publish's generation stamp. Payload: the relation's RelFileLocator, then
/// `pgrc_am::walgen::WalGen::payload` (the generation, its publisher, the base it chained on, the base's
/// publisher). Redo writes it into the table directory as the WALGEN file, so a node built from a base backup and WAL
/// knows the generations it replayed but has no files for (columnar data itself is never logged).
pub const XLOG_SMGR_TABLE_DIR_GEN: u8 = pgrc_am::walgen::SMGR_INFO;
pub const SIZEOF_XL_SMGR_TABLE_DIR_GEN: usize = 12 + pgrc_am::walgen::WalGen::PAYLOAD_LEN;

/// sizeof(xl_smgr_create): RelFileLocator (3 * Oid) + ForkNumber (int).
const SIZEOF_XL_SMGR_CREATE: usize = 16;
/// sizeof(xl_smgr_truncate): BlockNumber + RelFileLocator (3 * Oid) + int flags.
const SIZEOF_XL_SMGR_TRUNCATE: usize = 20;

pub const SMGR_TRUNCATE_HEAP: u32 = 0x0001;
pub const SMGR_TRUNCATE_VM: u32 = 0x0002;
pub const SMGR_TRUNCATE_FSM: u32 = 0x0004;
pub const SMGR_TRUNCATE_ALL: u32 = SMGR_TRUNCATE_HEAP | SMGR_TRUNCATE_VM | SMGR_TRUNCATE_FSM;

/// Validate that an SMGR redo record carries at least `need` bytes of main data
/// before the fixed-offset field reads below index into it. C's smgr_redo casts
/// XLogRecGetData() to the struct type and reads fields regardless of the
/// declared length; here (startup redo thread) a crafted short record must be a
/// catchable ERRCODE_DATA_CORRUPTED rather than a slice-index panic.
fn smgr_check_len(data: &[u8], need: usize, op: &str) -> PgResult<()> {
    if data.len() < need {
        ereport(ERROR)
            .errcode(ERRCODE_DATA_CORRUPTED)
            .errmsg(format!(
                "{op} smgr record has {} bytes of main data, expected at least {need}",
                data.len()
            ))
            .finish(ErrorLocation::new(file!(), line!() as i32, "smgr_redo"))?;
        unreachable!("ERROR finish returned");
    }
    Ok(())
}

/// Validate an on-WAL fork number before decoding. `ForkNumber::from_i32`
/// accepts only -1..=MAX_FORKNUM; a crafted out-of-range value must surface as
/// a typed error instead of an `.expect()` panic in the startup redo thread.
fn smgr_fork_num(raw: i32, op: &str) -> PgResult<ForkNumber> {
    match ForkNumber::from_i32(raw) {
        Some(fork) => Ok(fork),
        None => {
            ereport(ERROR)
                .errcode(ERRCODE_DATA_CORRUPTED)
                .errmsg(format!("{op} smgr record has invalid fork number {raw}"))
                .finish(ErrorLocation::new(file!(), line!() as i32, "smgr_redo"))?;
            unreachable!("ERROR finish returned");
        }
    }
}

pub fn smgr_redo(record: &mut XLogReaderState) -> PgResult<()> {
    const XLR_INFO_MASK: u8 = 0x0F;
    let lsn = record.EndRecPtr;
    let rec = record.record.as_ref().expect("smgr redo with no decoded record");
    let info = rec.xl_info & !XLR_INFO_MASK;
    // SAFETY: points into the reader's decode buffer, valid for this callback.
    let xlrec = unsafe { rec.main_data_bytes() };
    if info == XLOG_SMGR_CREATE {
        smgr_check_len(xlrec, SIZEOF_XL_SMGR_CREATE, "XLOG_SMGR_CREATE")?;
        let locator = RelFileLocator::new(
            u32::from_ne_bytes(xlrec[0..4].try_into().unwrap()),
            u32::from_ne_bytes(xlrec[4..8].try_into().unwrap()),
            u32::from_ne_bytes(xlrec[8..12].try_into().unwrap()),
        );
        let fork_num =
            smgr_fork_num(i32::from_ne_bytes(xlrec[12..16].try_into().unwrap()), "XLOG_SMGR_CREATE")?;
        let key = RelFileLocatorBackend { locator, backend: INVALID_PROC_NUMBER };
        smgr::smgropen(locator, INVALID_PROC_NUMBER)?;
        smgr::smgrcreate(key, fork_num, true)?;
        Ok(())
    } else if info == XLOG_SMGR_TRUNCATE {
        smgr_check_len(xlrec, SIZEOF_XL_SMGR_TRUNCATE, "XLOG_SMGR_TRUNCATE")?;
        let blkno = BlockNumber::from_ne_bytes(xlrec[0..4].try_into().unwrap());
        let locator = RelFileLocator::new(
            u32::from_ne_bytes(xlrec[4..8].try_into().unwrap()),
            u32::from_ne_bytes(xlrec[8..12].try_into().unwrap()),
            u32::from_ne_bytes(xlrec[12..16].try_into().unwrap()),
        );
        let flags = u32::from_ne_bytes(xlrec[16..20].try_into().unwrap());
        let key = RelFileLocatorBackend { locator, backend: INVALID_PROC_NUMBER };
        smgr::smgropen(locator, INVALID_PROC_NUMBER)?;

        // Recreate if dropped later in the WAL sequence; flush so the minimum
        // recovery point covers this record before the irreversible truncate.
        smgr::smgrcreate(key, ForkNumber::MAIN_FORKNUM, true)?;
        transam_xlog::XLogFlush(lsn)?;

        const NF: usize = MAX_FORKNUM as usize;
        let mut forks = [ForkNumber::MAIN_FORKNUM; NF];
        let mut old_blocks = [InvalidBlockNumber; NF];
        let mut blocks = [InvalidBlockNumber; NF];
        let mut nforks = 0;

        if flags & SMGR_TRUNCATE_HEAP != 0 {
            old_blocks[nforks] = smgr::smgrnblocks(key, ForkNumber::MAIN_FORKNUM)?;
            blocks[nforks] = blkno;
            nforks += 1;
            xlogutils::XLogTruncateRelation(locator, ForkNumber::MAIN_FORKNUM, blkno)?;
        }

        let fakerel = xlogutils::CreateFakeRelcacheEntry(locator);
        let mut need_fsm_vacuum = false;

        if flags & SMGR_TRUNCATE_FSM != 0 && smgr::smgrexists(key, ForkNumber::FSM_FORKNUM)? {
            let b = freespace::FreeSpaceMapPrepareTruncateRel(&fakerel, blkno)?;
            if b != InvalidBlockNumber {
                forks[nforks] = ForkNumber::FSM_FORKNUM;
                old_blocks[nforks] = smgr::smgrnblocks(key, ForkNumber::FSM_FORKNUM)?;
                blocks[nforks] = b;
                nforks += 1;
                need_fsm_vacuum = true;
            }
        }
        if flags & SMGR_TRUNCATE_VM != 0
            && smgr::smgrexists(key, ForkNumber::VISIBILITYMAP_FORKNUM)?
        {
            let b = visibilitymap::visibilitymap_prepare_truncate(&fakerel, blkno)?;
            if b != InvalidBlockNumber {
                forks[nforks] = ForkNumber::VISIBILITYMAP_FORKNUM;
                old_blocks[nforks] = smgr::smgrnblocks(key, ForkNumber::VISIBILITYMAP_FORKNUM)?;
                blocks[nforks] = b;
                nforks += 1;
            }
        }

        if nforks > 0 {
            init_small::globals::StartCriticalSection();
            smgr::smgrtruncate(key, &forks[..nforks], &old_blocks[..nforks], &blocks[..nforks])?;
            init_small::globals::EndCriticalSection();
        }

        // Truncated-away pages were likely marked all-free in the upper FSM
        // levels and would be preferentially handed out (storage.c).
        if need_fsm_vacuum {
            freespace::FreeSpaceMapVacuumRange(&fakerel, blkno, InvalidBlockNumber)?;
        }
        xlogutils::FreeFakeRelcacheEntry(fakerel);
        Ok(())
    } else if info == XLOG_SMGR_CREATE_TABLE_DIR {
        smgr_check_len(xlrec, SIZEOF_XL_SMGR_CREATE_TABLE_DIR, "XLOG_SMGR_CREATE_TABLE_DIR")?;
        let locator = RelFileLocator::new(
            u32::from_ne_bytes(xlrec[0..4].try_into().unwrap()),
            u32::from_ne_bytes(xlrec[4..8].try_into().unwrap()),
            u32::from_ne_bytes(xlrec[8..12].try_into().unwrap()),
        );
        redo_create_table_dir(locator)
    } else if info == XLOG_SMGR_TABLE_DIR_GEN {
        smgr_check_len(xlrec, SIZEOF_XL_SMGR_TABLE_DIR_GEN, "XLOG_SMGR_TABLE_DIR_GEN")?;
        let locator = RelFileLocator::new(
            u32::from_ne_bytes(xlrec[0..4].try_into().unwrap()),
            u32::from_ne_bytes(xlrec[4..8].try_into().unwrap()),
            u32::from_ne_bytes(xlrec[8..12].try_into().unwrap()),
        );
        let stamp = pgrc_am::walgen::WalGen::from_payload(&xlrec[12..]).expect("length checked above");
        redo_table_dir_gen(locator, stamp)
    } else {
        // storage.c:1094: elog(PANIC, "smgr_redo: unknown op code %u", info) —
        // a PANIC-level XX000 report the recovery error path owns, never a
        // Rust panic!() unwinding the startup redo thread.
        Err(panic_err(format!("smgr_redo: unknown op code {info}")))
    }
}

/// The pgrcolumnar table directory of `locator`: beside its main fork (pgrc_am dirpath.rs's rule).
pub fn table_dir_of(locator: RelFileLocator) -> String {
    let main = relpath_seams::relpathperm::call(locator, ForkNumber::MAIN_FORKNUM);
    let name = pgrc_format::dirlayout::table_dir_name(locator.relNumber as u64);
    match main.rfind('/') {
        Some(i) => format!("{}/{}", &main[..i], name),
        None => name,
    }
}

fn redo_create_table_dir(locator: RelFileLocator) -> PgResult<()> {
    let dir = table_dir_of(locator);
    if fd::MakePGDirectory(&dir) != 0 {
        let errno = fd::get_errno();
        if errno != libc::EEXIST {
            return Err(ereport(ERROR)
                .with_saved_errno(errno)
                .errcode_for_file_access()
                .errmsg(format!("could not create directory \"{dir}\": {PCT_M}"))
                .into_error()
                .into());
        }
    }
    let parent = match dir.rfind('/') {
        Some(i) => &dir[..i],
        None => ".",
    };
    fd::fsync_fname(parent, true)
}

/// D3150: the publish's stamp into the table directory, made when the base backup brought none (a table first loaded
/// after it): `WALGEN` replaced through `WALGEN.tmp` and a durable rename. The catalog judges it against the files when
/// it loads the directory (pgrc_am catalog::load_dir).
fn redo_table_dir_gen(locator: RelFileLocator, stamp: pgrc_am::walgen::WalGen) -> PgResult<()> {
    use pgrc_am::walgen::{WALGEN_FILE_NAME, WALGEN_TMP_FILE_NAME};
    redo_create_table_dir(locator)?;
    let dir = table_dir_of(locator);
    let tmp = format!("{dir}/{WALGEN_TMP_FILE_NAME}");
    let path = format!("{dir}/{WALGEN_FILE_NAME}");
    let text = stamp.encode();
    if let Err(errno) = fd::write_whole_file(&tmp, text.as_bytes(), true) {
        return Err(file_err(errno, format!("could not write file \"{tmp}\": {PCT_M}")));
    }
    fd::durable_rename(&tmp, &path, ERROR)?;
    Ok(())
}

/// D3150: replay of a dropped relation's storage (commit / abort redo, DropRelationFiles) also removes its pgrcolumnar
/// table directory, as the dropping transaction's own commit did on the primary (pgrc_am session.rs): a stamp replayed
/// before the drop recreated it, and a directory a backup copied would outlive its relation.
pub fn redo_remove_table_dir(locator: RelFileLocator) -> PgResult<()> {
    let dir = table_dir_of(locator);
    let mut md = fd::FileInfo::zeroed();
    if fd::pg_lstat(&dir, &mut md) != 0 {
        return Ok(()); // never a table directory here: nothing to remove
    }
    fd::rmtree(&dir, true)?;
    Ok(())
}

#[cold]
#[inline(never)]
fn file_err(errno: i32, msg: String) -> Box<PgError> {
    ereport(ERROR).with_saved_errno(errno).errcode_for_file_access().errmsg(msg).into_error().into()
}

// elog(PANIC) — an unrecoverable redo error (smgr_redo's unknown op code).
#[cold]
#[inline(never)]
fn panic_err(msg: String) -> Box<PgError> {
    Box::new(PgError::new(PANIC, msg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use types_error::ERRCODE_INTERNAL_ERROR;

    // A crafted CRC-valid RM_SMGR record can carry fewer main-data bytes than
    // the redo arm reads at fixed offsets, or an out-of-range fork number. C's
    // smgr_redo over-reads the decode buffer; the Rust startup redo thread must
    // reject both as a typed ERRCODE_DATA_CORRUPTED, never a slice-index or
    // .expect() panic.
    #[test]
    fn smgr_check_len_rejects_short_records() {
        // Full-length payloads pass.
        assert!(smgr_check_len(&[0u8; SIZEOF_XL_SMGR_CREATE], SIZEOF_XL_SMGR_CREATE, "CREATE").is_ok());
        assert!(
            smgr_check_len(&[0u8; SIZEOF_XL_SMGR_TRUNCATE], SIZEOF_XL_SMGR_TRUNCATE, "TRUNCATE")
                .is_ok()
        );

        // Every short length for either op yields a typed error, not a panic.
        for &need in &[SIZEOF_XL_SMGR_CREATE, SIZEOF_XL_SMGR_TRUNCATE] {
            for bad_len in 0..need {
                let err = smgr_check_len(&vec![0u8; bad_len], need, "op").err().unwrap();
                assert_eq!(err.sqlstate(), ERRCODE_DATA_CORRUPTED);
            }
        }
    }

    // storage.c:1094: elog(PANIC, "smgr_redo: unknown op code %u", info) — a
    // PANIC-level XX000 report printing the whole info byte (xl_info &
    // ~XLR_INFO_MASK). An unknown opcode must surface as a PANIC-level
    // PgError routed through the recovery error path, not an unhandled Rust
    // panic!() unwinding the startup redo thread (audit-18.6 b130
    // c960f8b9ff634465c446).
    #[test]
    fn unknown_op_code_is_a_panic_error_not_a_rust_panic() {
        let mut rec = xlogreader_seams::DecodedXLogRecord::default();
        rec.xl_info = 0x30; // & !XLR_INFO_MASK == 0x30, neither CREATE nor TRUNCATE
        let mut record = XLogReaderState { record: Some(rec), ..Default::default() };
        let err = smgr_redo(&mut record).expect_err("unknown smgr opcode must not redo silently");
        assert_eq!(err.message(), "smgr_redo: unknown op code 48");
        assert_eq!(err.level(), PANIC);
        assert_eq!(err.sqlstate(), ERRCODE_INTERNAL_ERROR);
    }

    #[test]
    fn smgr_fork_num_rejects_out_of_range() {
        // In-range values (-1..=MAX_FORKNUM) decode.
        for raw in -1..=(MAX_FORKNUM as i32) {
            assert!(smgr_fork_num(raw, "CREATE").is_ok());
        }
        // Out-of-range values are rejected as typed corruption errors.
        for &raw in &[-2i32, MAX_FORKNUM as i32 + 1, i32::MIN, i32::MAX] {
            let err = smgr_fork_num(raw, "CREATE").err().unwrap();
            assert_eq!(err.sqlstate(), ERRCODE_DATA_CORRUPTED);
        }
    }
}
