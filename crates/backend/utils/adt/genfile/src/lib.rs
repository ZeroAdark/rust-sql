mod builtins;
pub use builtins::GENFILE_BUILTINS;

use std::io::{Read, Seek, SeekFrom};

use adt_datetime::{POSTGRES_EPOCH_JDATE, SECS_PER_DAY, UNIX_EPOCH_JDATE, USECS_PER_SEC};
use datum::Datum;
use elog::{ereport, PCT_M};
use types_core::Oid;
use types_error::{
    PgError, PgResult, ERRCODE_INSUFFICIENT_PRIVILEGE, ERRCODE_INVALID_PARAMETER_VALUE,
    ERRCODE_PROGRAM_LIMIT_EXCEEDED, ERRCODE_UNDEFINED_OBJECT, ERROR,
};

const ROLE_PG_READ_SERVER_FILES: Oid = 4569;
// MaxAllocSize (memutils.h): 1 GB - 1.
const MAX_ALLOC_SIZE: usize = 0x3fff_ffff;
const VARHDRSZ: usize = 4;
pub(crate) const XLOGDIR: &str = "pg_wal";
pub(crate) const PG_LOGICAL_SNAPSHOTS_DIR: &str = "pg_logical/snapshots";
pub(crate) const PG_LOGICAL_MAPPINGS_DIR: &str = "pg_logical/mappings";

// Filenames are raw server-encoding bytes (C's char *): a SQL_ASCII database
// hands these builtins text that need not be UTF-8, and a directory may hold
// entries whose names are not UTF-8 either; C passes both through untouched.
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(target_os = "wasi")]
use std::os::wasi::ffi::OsStrExt;

pub(crate) fn os_path(bytes: &[u8]) -> &std::path::Path {
    std::path::Path::new(std::ffi::OsStr::from_bytes(bytes))
}

pub(crate) fn os_name_bytes(name: &std::ffi::OsStr) -> Vec<u8> {
    let b = name.as_bytes();
    let mut v = Vec::new();
    let _ = v.try_reserve(b.len());
    v.extend_from_slice(b);
    v
}

// ereport(errmsg("<prefix>\"%s\"<suffix>", filename)) (genfile.c): with a
// non-UTF-8 filename the message bytes reach the client verbatim, as C's %s
// does. `%m` in `suffix` is expanded against the io error's errno.
pub(crate) fn io_error_path(
    e: &std::io::Error,
    prefix: &str,
    path: &[u8],
    suffix: &str,
) -> Box<PgError> {
    // %m expands in the format only: a filename containing "%m" is a %s
    // argument in C and stays verbatim.
    let errno = e.raw_os_error().unwrap_or(0);
    let suffix = elog::errno::replace_percent_m(suffix, errno);
    let mut err = PgError::error_bytes(&format!("{prefix}\""), path, &format!("\"{suffix}"))
        .with_sqlstate(elog::errno::sqlstate_for_file_access(errno));
    err.saved_errno = Some(errno);
    Box::new(err)
}

// IS_DIR_SEP / is_absolute_path (path.c), non-Windows.
fn is_absolute_path(path: &[u8]) -> bool {
    path.first() == Some(&b'/')
}

fn path_is_prefix_of_path(path1: &[u8], path2: &[u8]) -> bool {
    match path2.strip_prefix(path1) {
        Some(rest) => rest.is_empty() || rest.starts_with(b"/"),
        None => false,
    }
}

// path must be canonicalized already (path.c path_contains_parent_reference).
fn path_contains_parent_reference(path: &[u8]) -> bool {
    path == b".."
        || path.starts_with(b"../")
        || path.windows(4).any(|w| w == b"/../")
        || path.ends_with(b"/..")
}

fn path_is_relative_and_below_cwd(path: &[u8]) -> bool {
    !is_absolute_path(path) && !path_contains_parent_reference(path)
}

// C reads the Log_directory global directly; the slot's owner (syslogger) may
// not be linked, in which case the GUC boot value "log" applies.
pub(crate) fn log_directory() -> String {
    if guc_tables::vars::Log_directory.installed() {
        if let Some(dir) = guc_tables::vars::Log_directory.read() {
            return dir;
        }
    }
    "log".to_string()
}

pub(crate) fn convert_and_check_filename(arg: &[u8]) -> PgResult<Vec<u8>> {
    let filename = pg_path::canonicalize_path_bytes(arg);

    if acl_seams::has_privs_of_role::call(miscinit::GetUserId(), ROLE_PG_READ_SERVER_FILES)? {
        return Ok(filename);
    }

    if is_absolute_path(&filename) {
        let data_dir =
            init_small::globals::DataDir().expect("DataDir must be set (C Assert in path checks)");
        let log_dir = log_directory();
        if !path_is_prefix_of_path(data_dir.as_bytes(), &filename)
            && (!is_absolute_path(log_dir.as_bytes())
                || !path_is_prefix_of_path(log_dir.as_bytes(), &filename))
        {
            return Err(ereport(ERROR)
                .errcode(ERRCODE_INSUFFICIENT_PRIVILEGE)
                .errmsg("absolute path not allowed")
                .into_error()
                .into());
        }
    } else if !path_is_relative_and_below_cwd(&filename) {
        return Err(ereport(ERROR)
            .errcode(ERRCODE_INSUFFICIENT_PRIVILEGE)
            .errmsg("path must be in or below the data directory")
            .into_error()
            .into());
    }

    Ok(filename)
}

fn read_binary_file(
    filename: &[u8],
    seek_offset: i64,
    bytes_to_read: i64,
    missing_ok: bool,
) -> PgResult<Option<Vec<u8>>> {
    if bytes_to_read > (MAX_ALLOC_SIZE - VARHDRSZ) as i64 {
        return Err(ereport(ERROR)
            .errcode(ERRCODE_INVALID_PARAMETER_VALUE)
            .errmsg("requested length too large")
            .into_error()
            .into());
    }

    let index = fd::AllocateFilePath(&fd::PgPath::from_bytes(filename), "rb")?;
    if index < 0 {
        let e = std::io::Error::from_raw_os_error(fd::get_errno());
        if missing_ok && e.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(io_error_path(&e, "could not open file ", filename, &format!(" for reading: {PCT_M}")));
    }

    let r = fd::with_allocated_stdio(index, |file| {
        read_allocated_file(file, filename, seek_offset, bytes_to_read)
    });
    fd::FreeFile(index)?;
    match r {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

fn read_allocated_file(
    file: &mut std::fs::File,
    filename: &[u8],
    seek_offset: i64,
    bytes_to_read: i64,
) -> PgResult<Vec<u8>> {
    let whence = if seek_offset >= 0 {
        SeekFrom::Start(seek_offset as u64)
    } else {
        SeekFrom::End(seek_offset)
    };
    if let Err(e) = file.seek(whence) {
        return Err(io_error_path(&e, "could not seek in file ", filename, &format!(": {PCT_M}")));
    }

    if bytes_to_read >= 0 {
        let mut buf = ::mcx::std_vec_from_elem("adt", 0u8, bytes_to_read as usize)?;
        let mut nbytes = 0;
        while nbytes < buf.len() {
            match file.read(&mut buf[nbytes..]) {
                Ok(0) => break,
                Ok(n) => nbytes += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    return Err(io_error_path(&e, "could not read file ", filename, &format!(": {PCT_M}")))
                }
            }
        }
        buf.truncate(nbytes);
        Ok(buf)
    } else {
        // C caps the whole-file StringInfo at MaxAllocSize - 1 bytes including
        // the reserved varlena header.
        let limit = MAX_ALLOC_SIZE - 1 - VARHDRSZ;
        let mut buf = Vec::new();
        if let Err(e) = file.by_ref().take(limit as u64 + 1).read_to_end(&mut buf) {
            return Err(io_error_path(&e, "could not read file ", filename, &format!(": {PCT_M}")));
        }
        if buf.len() > limit {
            return Err(ereport(ERROR)
                .errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED)
                .errmsg("file length too large")
                .into_error()
                .into());
        }
        Ok(buf)
    }
}

fn read_text_file(
    filename: &[u8],
    seek_offset: i64,
    bytes_to_read: i64,
    missing_ok: bool,
) -> PgResult<Option<Vec<u8>>> {
    let buf = read_binary_file(filename, seek_offset, bytes_to_read, missing_ok)?;
    if let Some(buf) = &buf {
        mbutils::pg_verifymbstr(buf, false)?;
    }
    Ok(buf)
}

fn negative_length() -> Box<PgError> {
    Box::new(
        ereport(ERROR)
            .errcode(ERRCODE_INVALID_PARAMETER_VALUE)
            .errmsg("requested length cannot be negative")
            .into_error(),
    )
}

pub(crate) fn pg_read_file_common(
    filename: &[u8],
    seek_offset: i64,
    bytes_to_read: i64,
    read_to_eof: bool,
    missing_ok: bool,
) -> PgResult<Option<Vec<u8>>> {
    if read_to_eof {
        debug_assert_eq!(bytes_to_read, -1);
    } else if bytes_to_read < 0 {
        return Err(negative_length());
    }
    read_text_file(&convert_and_check_filename(filename)?, seek_offset, bytes_to_read, missing_ok)
}

pub(crate) fn pg_read_binary_file_common(
    filename: &[u8],
    seek_offset: i64,
    bytes_to_read: i64,
    read_to_eof: bool,
    missing_ok: bool,
) -> PgResult<Option<Vec<u8>>> {
    if read_to_eof {
        debug_assert_eq!(bytes_to_read, -1);
    } else if bytes_to_read < 0 {
        return Err(negative_length());
    }
    read_binary_file(&convert_and_check_filename(filename)?, seek_offset, bytes_to_read, missing_ok)
}

// pg_ls_tmpdir (genfile.c:649): TABLESPACEOID existence check + TempTablespacePath.
pub(crate) fn tmpdir_path(tblspc: Oid) -> PgResult<String> {
    use cache_syscache::{ReleaseSysCache, SearchSysCache1, SysCacheKey, TABLESPACEOID};
    match SearchSysCache1(TABLESPACEOID, SysCacheKey::Value(Datum::from_oid(tblspc)))? {
        Some(ht) => ReleaseSysCache(ht),
        None => {
            return Err(ereport(ERROR)
                .errcode(ERRCODE_UNDEFINED_OBJECT)
                .errmsg(format!("tablespace with OID {tblspc} does not exist"))
                .into_error()
                .into())
        }
    }
    Ok(fd::TempTablespacePath(tblspc))
}

pub(crate) fn time_t_to_timestamptz(tm: i64) -> i64 {
    (tm - (POSTGRES_EPOCH_JDATE - UNIX_EPOCH_JDATE) as i64 * SECS_PER_DAY as i64) * USECS_PER_SEC
}
