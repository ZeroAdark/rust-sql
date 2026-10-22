use std::ffi::CString;

use types_error::{PgError, PgResult, ERROR};

pub(crate) fn read_all(path: &str) -> Option<Vec<u8>> {
    let c = CString::new(path).ok()?;
    let fd = vfs::open(&c, libc::O_RDONLY | libc::O_CLOEXEC, 0);
    if fd < 0 {
        return None;
    }
    let size = vfs::file_size(fd);
    let mut out = Vec::new();
    let ok = size >= 0 && out.try_reserve_exact(size as usize).is_ok() && {
        let mut chunk = [0u8; 8192];
        let mut off: i64 = 0;
        loop {
            let n = vfs::pread(fd, &mut chunk, off);
            if n <= 0 {
                break n == 0;
            }
            out.extend_from_slice(&chunk[..n as usize]);
            off += n as i64;
        }
    };
    vfs::close(fd);
    ok.then_some(out)
}

pub(crate) fn write_durable(path: &str, bytes: &[u8]) -> PgResult<()> {
    let tmp = format!("{path}.tmp");
    let c = CString::new(tmp.as_str()).map_err(|_| io_error(&tmp, "contains a NUL byte"))?;
    let fd = vfs::open(&c, libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_CLOEXEC, 0o600);
    if fd < 0 {
        return Err(io_error(&tmp, "could not be created"));
    }
    let mut off = 0usize;
    while off < bytes.len() {
        let n = vfs::pwrite(fd, &bytes[off..], off as i64);
        if n <= 0 {
            vfs::close(fd);
            return Err(io_error(&tmp, "could not be written"));
        }
        off += n as usize;
    }
    let synced = vfs::fsync(fd) == 0;
    if vfs::close(fd) != 0 || !synced {
        return Err(io_error(&tmp, "could not be synced"));
    }
    fd::durable_rename(&tmp, path, ERROR)?;
    Ok(())
}

fn io_error(path: &str, what: &str) -> Box<PgError> {
    let errno = vfs::get_errno();
    Box::new(
        PgError::new(ERROR, format!("file \"{path}\" {what}: errno {errno}"))
            .with_sqlstate(types_error::ERRCODE_IO_ERROR),
    )
}
