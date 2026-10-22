//! O-7 directory path derivation + the vfs-only directory operations
//! (spec §12: the table directory `pgrc_<relfilenumber>` is a SIBLING of
//! where the relation's main fork lives — tablespace and temp-namespace
//! semantics ride `relpath::GetRelationPath` unchanged).
//!
//! All I/O goes through the sanctioned `vfs::` choke (determinism-ledger
//! law: zero rows owed). Directory contents are FLAT by construction
//! (`pgrc_format::dirlayout` names only); recursive trees never exist, so
//! removal is unlink-all-entries + rmdir.

use std::ffi::CString;

use pgrc_format::dirlayout::{table_dir_name, temp_table_dir_name};
use types_core::{ForkNumber, ProcNumber, INVALID_PROC_NUMBER};
use types_error::{PgError, PgResult};
use types_storage::storage::RelFileLocator;

/// The per-table pgrcolumnar directory path for a relation's locator: `pgrc_t<procno>_<n>` for a temp relation.
pub fn table_dir_path(locator: RelFileLocator, backend: ProcNumber) -> String {
    let main = relpath::GetRelationPath(locator, backend, ForkNumber::MAIN_FORKNUM);
    let dir_name = if backend == INVALID_PROC_NUMBER {
        table_dir_name(locator.relNumber as u64)
    } else {
        temp_table_dir_name(backend as u32, locator.relNumber as u64)
    };
    match main.rfind('/') {
        Some(i) => format!("{}/{}", &main[..i], dir_name),
        // A bare relative main path (global tablespace shapes still carry
        // "global/<rel>", so this arm is defensive only).
        None => dir_name,
    }
}

fn cpath(path: &str) -> PgResult<CString> {
    CString::new(path).map_err(|_| io_err("path contains NUL", path, 0))
}

fn io_err(what: &str, path: &str, errno: i32) -> Box<PgError> {
    Box::<PgError>::new( // alloc: Cold
        PgError::error(format!("pgrcolumnar: {what}: {path} (errno {errno})")) // alloc: Cold
            .with_sqlstate(types_error::ERRCODE_IO_ERROR),
    )
}

/// The bytes of a table directory's files (flat by construction: parts, manifests, sections, CURRENT), the apparent
/// sizes `du --apparent-size` sums; 0 when the directory does not exist (a table never written, or a standby). An entry
/// removed while the directory is read (a concurrent cleanup) counts 0.
pub fn dir_bytes(dir: &str) -> PgResult<u64> {
    let c = cpath(dir)?;
    let iter = match vfs::read_dir(&c) {
        Ok(iter) => iter,
        Err(errno) if errno == libc::ENOENT => return Ok(0),
        Err(errno) => return Err(io_err("dir scan failed", dir, errno)),
    };
    let mut total = 0u64;
    for entry in iter {
        let name = entry.map_err(|errno| io_err("dir scan entry failed", dir, errno))?;
        if name == "." || name == ".." {
            continue;
        }
        let path = format!("{dir}/{name}");
        let mut info = vfs::FileInfo::zeroed();
        if vfs::stat(&cpath(&path)?, &mut info) != 0 {
            let errno = vfs::get_errno();
            if errno == libc::ENOENT {
                continue;
            }
            return Err(io_err("stat failed", &path, errno));
        }
        total += info.size.max(0) as u64;
    }
    Ok(total)
}

/// Does the directory exist? (stat; ENOENT → false).
pub fn dir_exists(dir: &str) -> PgResult<bool> {
    let c = cpath(dir)?;
    let mut info = vfs::FileInfo::zeroed();
    if vfs::stat(&c, &mut info) != 0 {
        let errno = vfs::get_errno();
        if errno == libc::ENOENT {
            return Ok(false);
        }
        return Err(io_err("stat failed", dir, errno));
    }
    Ok(info.is_dir())
}

/// Create the table directory if absent; returns true iff THIS call created
/// it. Durability: the parent directory is fsynced after a create, so the
/// dirent survives a crash (the publish path re-fsyncs the table dir for
/// its own contents).
pub fn mkdir_if_absent(dir: &str) -> PgResult<bool> {
    let c = cpath(dir)?;
    if vfs::mkdir(&c, 0o700) != 0 {
        let errno = vfs::get_errno();
        if errno == libc::EEXIST {
            return Ok(false);
        }
        return Err(io_err("mkdir failed", dir, errno));
    }
    fsync_parent(dir)?;
    Ok(true)
}

fn fsync_parent(dir: &str) -> PgResult<()> {
    let parent = match dir.rfind('/') {
        Some(i) if i > 0 => &dir[..i],
        _ => ".",
    };
    fsync_path(parent)
}

fn fsync_path(path: &str) -> PgResult<()> {
    let c = cpath(path)?;
    let fd = vfs::open(&c, libc::O_RDONLY | libc::O_CLOEXEC, 0);
    if fd < 0 {
        return Err(io_err("open for fsync failed", path, vfs::get_errno()));
    }
    let rc = pgrc_write::wvfs::pg_fsync(fd);
    let errno = vfs::get_errno();
    vfs::close(fd);
    if rc != 0 {
        return Err(io_err("fsync failed", path, errno));
    }
    Ok(())
}

/// Starts the writeback of a whole file (`sync_file_range(WRITE)`, PG's `pre_sync_fname`): a hint, every error ignored.
pub fn flush_path(path: &str) {
    let Ok(c) = CString::new(path) else { return };
    let fd = vfs::open(&c, libc::O_RDONLY | libc::O_CLOEXEC, 0);
    if fd >= 0 {
        let _ = vfs::flush_range(fd, 0, 0);
        vfs::close(fd);
    }
}

/// Flushes every path, then fsyncs each in order, stopping at the first fsync error: with every file's writeback
/// already in flight the fsyncs wait for one batch of device writes, not for one write each in turn.
pub fn flush_then_fsync<E>(
    paths: &[String],
    mut flush: impl FnMut(&str),
    mut fsync: impl FnMut(&str) -> Result<(), E>,
) -> Result<(), E> {
    for p in paths {
        flush(p);
    }
    for p in paths {
        fsync(p)?;
    }
    Ok(())
}

/// A new relfilenumber's directory (CREATE, TRUNCATE, a rewrite's new locator) and its empty catalog entry.
pub fn create_table_dir(locator: RelFileLocator, backend: ProcNumber) -> PgResult<()> {
    let dir = table_dir_path(locator, backend);
    mkdir_if_absent(&dir)?;
    // IX0: a reused relfilenumber must not inherit an old counter its new rowid file and WAL never reserved
    crate::rowid_alloc::forget(&dir);
    crate::catalog::create((locator.spcOid, locator.dbOid, locator.relNumber as u64), &dir);
    Ok(())
}

/// The smgr drop of a relation's storage: its directory, if it has one.
pub fn remove_dir_by_locator(locator: RelFileLocator, backend: ProcNumber) -> PgResult<()> {
    remove_table_dir(&table_dir_path(locator, backend))
}

/// Whether the relation's (empty) main fork exists: smgr creates it with every relfilenumber, beside the table directory
/// (`create_table_dir`), so a main fork without its directory is a removed directory, never a relation not yet written.
pub fn main_fork_exists(locator: RelFileLocator, backend: ProcNumber) -> bool {
    let Ok(c) = cpath(&relpath::GetRelationPath(locator, backend, ForkNumber::MAIN_FORKNUM)) else { return false };
    let mut info = vfs::FileInfo::zeroed();
    vfs::stat(&c, &mut info) == 0
}

/// D4853: whether the relation's main fork starts as a v0.3.x pgrcolumnar (v1) segment file does (the v1 format's
/// 8-byte magic, `V1_MAGIC`); this engine never writes the main fork.
pub fn v1_segment(locator: RelFileLocator, backend: ProcNumber) -> bool {
    const V1_MAGIC: [u8; 8] = 0x6362_7374_6f72_6531u64.to_le_bytes();
    let Ok(c) = cpath(&relpath::GetRelationPath(locator, backend, ForkNumber::MAIN_FORKNUM)) else { return false };
    let fd = vfs::open(&c, libc::O_RDONLY | libc::O_CLOEXEC, 0);
    if fd < 0 {
        return false;
    }
    let mut head = [0u8; 8];
    let n = vfs::pread(fd, &mut head, 0);
    vfs::close(fd);
    n == 8 && head == V1_MAGIC
}

/// A small file's bytes through the vfs (`None`: absent or unreadable).
pub fn read_small(path: &str) -> Option<Vec<u8>> {
    let c = cpath(path).ok()?;
    let fd = vfs::open(&c, libc::O_RDONLY | libc::O_CLOEXEC, 0);
    if fd < 0 {
        return None;
    }
    let mut buf = vec![0u8; 4096];
    let n = vfs::pread(fd, &mut buf, 0);
    vfs::close(fd);
    (n >= 0).then(|| {
        buf.truncate(n as usize);
        buf
    })
}

/// WALMODE: this boot's id (Linux `boot_id`; `None` elsewhere, where every start counts as a new boot).
pub fn boot_id() -> Option<String> {
    let b = read_small("/proc/sys/kernel/random/boot_id")?;
    Some(String::from_utf8_lossy(&b).trim().to_string()).filter(|s| !s.is_empty())
}

fn unsynced_path(dir: &str) -> String {
    format!("{}/{}", pgrc_format::spool::UNSYNCED_DIR, pgrc_format::spool::unsynced_name(dir))
}

fn exists(path: &str) -> PgResult<bool> {
    let mut st = vfs::FileInfo::default();
    Ok(vfs::stat(&cpath(path)?, &mut st) == 0)
}

/// WALMODE: the unlogged table directory `dir` is written without any fsync (`pgrust.pgrc_unlogged_sync=off`). A
/// durable marker holding this boot's id makes the next start on another boot reset the table (fd `reinit`): its
/// files may have been lost with the page cache, after a clean shutdown too. Written once (an existing marker is kept:
/// it is this boot's, since startup removes the others), before the shutdown checkpoint can complete.
pub fn mark_unsynced(dir: &str) -> PgResult<()> {
    let path = unsynced_path(dir);
    if exists(&path)? {
        return Ok(());
    }
    let d = pgrc_format::spool::UNSYNCED_DIR;
    if vfs::mkdir(&cpath(d)?, 0o700) == 0 {
        fsync_path(".")?;
    } else if vfs::get_errno() != libc::EEXIST {
        return Err(io_err("mkdir failed", d, vfs::get_errno()));
    }
    let tmp = format!("{path}.tmp");
    let body = format!("{}\n", boot_id().unwrap_or_else(|| "unknown".to_string()));
    let fd = vfs::open(&cpath(&tmp)?, libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_CLOEXEC, 0o600);
    if fd < 0 {
        return Err(io_err("open failed", &tmp, vfs::get_errno()));
    }
    let failed = if vfs::pwrite(fd, body.as_bytes(), 0) != body.len() as isize {
        Some(("write failed", vfs::get_errno()))
    } else if vfs::fsync(fd) != 0 {
        Some(("fsync failed", vfs::get_errno()))
    } else {
        None
    };
    vfs::close(fd);
    if let Some((what, errno)) = failed {
        return Err(io_err(what, &tmp, errno));
    }
    if vfs::rename(&cpath(&tmp)?, &cpath(&path)?) != 0 {
        return Err(io_err("rename failed", &tmp, vfs::get_errno()));
    }
    fsync_path(d)
}

/// The directory is gone: its marker goes too (not fsynced; a marker left by a crash names no directory, and the
/// next start removes it).
fn forget_unsynced(dir: &str) {
    if let Ok(c) = cpath(&unsynced_path(dir)) {
        let _ = vfs::unlink(&c);
    }
}

/// Remove the table directory and everything in it (flat by construction).
/// ENOENT anywhere is fine: deletion is idempotent (commit-time replay
/// after a partial removal must succeed — the pendingDeletes posture).
/// The entry goes after the directory, so a concurrent load cannot put it back.
pub fn remove_table_dir(dir: &str) -> PgResult<()> {
    let r = remove_tree(dir);
    forget_unsynced(dir);
    crate::catalog::remove_dir(dir);
    // GENID: a directory emptied and kept (TRUNCATE, an unlogged reset) keeps its used numbers; one removed forgets them
    crate::catalog::forget_dir(dir);
    crate::rowid_alloc::forget(dir);
    crate::rowid_alloc::forget_renumbered(|d| d == dir);
    // D4581: gone for good: its publish mutex goes (a failed removal keeps it)
    if r.is_ok() {
        crate::inval::forget_dir(dir);
    }
    r
}

fn remove_tree(dir: &str) -> PgResult<()> {
    let c = cpath(dir)?;
    if !unlink_entries(dir, &c)? {
        return Ok(());
    }
    if vfs::rmdir(&c) != 0 {
        let errno = vfs::get_errno();
        if errno != libc::ENOENT {
            return Err(io_err("rmdir failed", dir, errno));
        }
    }
    fsync_parent(dir)
}

/// Remove every file INSIDE the directory, keeping the directory itself (TRUNCATE's redo,
/// an unlogged table's reset).
pub fn remove_dir_contents(dir: &str) -> PgResult<()> {
    crate::catalog::remove_dir(dir);
    let c = cpath(dir)?;
    let r = unlink_entries(dir, &c);
    // IX0: the rowid file went with the rest, so the counter restarts at 291 (main 2026-10-04, L1-c); after the
    // unlinks, so no reservation reloads the old file into a fresh entry
    crate::rowid_alloc::forget(dir);
    if !r? {
        return Ok(());
    }
    fsync_path(dir)
}

/// The nontransactional truncate: the directory emptied, its catalog entry empty, the numbers it used kept (GENID:
/// the next publish numbers above them, never from 1 again).
pub fn truncate_table_dir(locator: RelFileLocator, backend: ProcNumber) -> PgResult<()> {
    let dir = table_dir_path(locator, backend);
    remove_dir_contents(&dir)?;
    crate::catalog::create((locator.spcOid, locator.dbOid, locator.relNumber as u64), &dir);
    Ok(())
}

/// Fsync every file of the directory, then the directory: the commit of a relation whose writes
/// skipped WAL (`wal_level=minimal`). An absent directory has nothing to sync.
pub fn sync_table_dir(dir: &str) -> PgResult<()> {
    let c = cpath(dir)?;
    let iter = match vfs::read_dir(&c) {
        Ok(iter) => iter,
        Err(libc::ENOENT) => return Ok(()),
        Err(errno) => return Err(io_err("dir scan failed", dir, errno)),
    };
    let mut paths = Vec::new();
    for entry in iter {
        let name = entry.map_err(|errno| io_err("dir scan entry failed", dir, errno))?;
        if name != "." && name != ".." {
            paths.push(format!("{dir}/{name}"));
        }
    }
    flush_then_fsync(&paths, flush_path, fsync_path)?;
    fsync_path(dir)
}

/// Unlink every entry of a flat directory. `Ok(false)` = the directory does
/// not exist (nothing to do); ENOENT on individual entries is tolerated.
fn unlink_entries(dir: &str, c: &CString) -> PgResult<bool> {
    let iter = match vfs::read_dir(c) {
        Ok(iter) => iter,
        Err(errno) => {
            if errno == libc::ENOENT {
                return Ok(false);
            }
            return Err(io_err("dir scan failed", dir, errno));
        }
    };
    for entry in iter {
        let name = match entry {
            Ok(n) => n,
            Err(errno) => return Err(io_err("dir scan entry failed", dir, errno)),
        };
        if name == "." || name == ".." {
            continue;
        }
        let p = cpath(&format!("{dir}/{name}"))?;
        if vfs::unlink(&p) != 0 {
            let errno = vfs::get_errno();
            if errno != libc::ENOENT {
                return Err(io_err("unlink failed", &format!("{dir}/{name}"), errno)); // alloc: Cold
            }
        }
    }
    Ok(true)
}
