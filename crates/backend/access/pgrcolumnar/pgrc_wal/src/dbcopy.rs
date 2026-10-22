//! `CREATE DATABASE … STRATEGY WAL_LOG` for a pgrcolumnar table: its directory as pgrc records, `CURRENT` last.

use pgrc_am::{dircopy, dirpath};
use pgrc_format::dirlayout::{temp_file_name, CURRENT_FILE_NAME};
use pgrc_format::wal::{DirLoc, InstallRec};
use pgrc_write::plan::{apply_install, ApplyMode};
use pgrc_write::walsink::LoggedVfs;
use pgrc_write::wvfs::RealVfs;
use pgrc_write::WriteResult;
use types_core::INVALID_PROC_NUMBER;
use types_error::PgResult;
use types_storage::storage::RelFileLocator;

use crate::insert::XlogSink;

pub fn copy_dir_logged(src: RelFileLocator, dst: RelFileLocator, permanent: bool) -> PgResult<()> {
    let from = dirpath::table_dir_path(src, INVALID_PROC_NUMBER);
    if !dirpath::dir_exists(&from)? {
        return Ok(());
    }
    let to = dirpath::table_dir_path(dst, INVALID_PROC_NUMBER);
    let loc = crate::dir_loc(dst);
    dirpath::mkdir_if_absent(&to)?;
    crate::insert::log_dir_create(loc)?;
    if permanent {
        let fxid = xact::GetCurrentFullTransactionId()?.value;
        logged(&from, &to, loc, fxid).map_err(pgrc_am::write_error)?;
    } else {
        dircopy::copy_dir(&from, &to)?;
    }
    dircopy::load_copy((dst.spcOid, dst.dbOid, dst.relNumber as u64), &to);
    Ok(())
}

fn logged(from: &str, to: &str, loc: DirLoc, fxid: u64) -> WriteResult<()> {
    let mut sink = XlogSink::new(loc);
    let mut real = RealVfs;
    let mut vfs = LoggedVfs::new(&mut real, &mut sink, to);
    for f in dircopy::carried_files(&mut vfs, from)? {
        if f != CURRENT_FILE_NAME {
            dircopy::copy_file(&mut vfs, from, &f, to, &f)?;
            continue;
        }
        let tmp = temp_file_name(fxid, 0);
        dircopy::copy_file(&mut vfs, from, &f, to, &tmp)?;
        let rec = InstallRec { dir: loc, renames: vec![(tmp, CURRENT_FILE_NAME.to_string())] };
        let (inner, sink) = vfs.into_parts();
        return sink.install(&rec, &mut || apply_install(&mut *inner, to, &rec, ApplyMode::Primary));
    }
    Ok(())
}
