//! The writer's file-system seam.
//!
//! Every byte this crate persists goes through [`WriteVfs`]:
//!
//! - [`RealVfs`] calls the sanctioned `vfs::` free-function shims (the
//!   determinism-lint choke point — this crate owes zero ledger rows). Under
//!   `--cfg pgrust_sim` those shims dispatch to the tree's SimVfs, so M3-K's
//!   crash batteries (sector tearing, seeded dirent subsets) compose under
//!   this writer with no writer changes.
//! - [`MemVfs`] is the in-crate deterministic model used by this crate's own
//!   kill-9-shaped crash matrix in DEFAULT CI (no cfg): per-file
//!   volatile/durable content, per-directory volatile/durable dirents
//!   (namespace ops are volatile until the parent dir is fsync'd — the
//!   dirent-loss model), an op log for ordering witnesses, and a
//!   crash-at-op-N injector. [`crate::testkit::crash_and_revive`] drops everything
//!   not durable, exactly a `kill -9` at an op boundary.
//!
//! Method names deliberately avoid the determinism lint's name-based
//! patterns (`sync_all`/`sync_data`/`metadata`/`write_all_at`): the lint
//! flags those on ANY receiver.

use crate::{WriteError, WriteResult};
use pgrc_format::wal::SmallMasks;
use std::collections::BTreeMap;
use std::ffi::CString;

/// Opaque file handle: `raw` is the vfs-minted descriptor (RealVfs), `key`
/// the MemVfs open-table key; `path` rides for error reporting + op logs.
#[derive(Debug)]
pub struct WvFd {
    raw: i32,
    key: u64,
    path: String,
}

impl WvFd {
    pub fn path(&self) -> &str {
        &self.path
    }
}

/// The writer-side Vfs contract. `&mut self` throughout: the writer is
/// single-threaded per table (R1–R6 worker confinement), so no interior
/// mutability and no sync types are needed.
pub trait WriteVfs {
    /// Create-or-truncate `path` for read/write.
    fn create_rw(&mut self, path: &str) -> WriteResult<WvFd>;
    /// Open an EXISTING file for read/write (publish-time part_no patch).
    fn open_rw(&mut self, path: &str) -> WriteResult<WvFd>;
    fn pwrite_at(&mut self, fd: &WvFd, off: u64, bytes: &[u8]) -> WriteResult<()>;
    fn fsync_file(&mut self, fd: &WvFd) -> WriteResult<()>;
    /// colload (k): fsync every descriptor of `fds`, the publish's per-part and per-sidecar barriers. When it returns
    /// `Ok`, every one is durable, as after [`Self::fsync_file`] on each in order (the default does exactly that).
    /// [`RealVfs`] issues them concurrently through the installed [`FsyncFanout`], so the filesystem can commit them
    /// together. On failure: the error of the
    /// first failing descriptor in `fds` order.
    fn fsync_files(&mut self, fds: &[WvFd]) -> WriteResult<()> {
        for fd in fds {
            self.fsync_file(fd)?;
        }
        Ok(())
    }
    fn close_file(&mut self, fd: WvFd) -> WriteResult<()>;
    fn read_full(&mut self, path: &str) -> WriteResult<Vec<u8>>;
    /// The first `max` bytes of the file (fewer at its end): the manifest's
    /// section-0 readers.
    fn read_prefix(&mut self, path: &str, max: usize) -> WriteResult<Vec<u8>> {
        let mut v = self.read_full(path)?;
        v.truncate(max);
        Ok(v)
    }
    /// `len` bytes at `off` (fewer at the file's end): redo's footer identity check.
    fn read_at(&mut self, path: &str, off: u64, len: usize) -> WriteResult<Vec<u8>> {
        let mut v = self.read_prefix(path, off as usize + len)?;
        v.drain(..(off as usize).min(v.len()));
        Ok(v)
    }
    fn rename_path(&mut self, from: &str, to: &str) -> WriteResult<()>;
    fn unlink_path(&mut self, path: &str) -> WriteResult<()>;
    fn fsync_dir(&mut self, dir: &str) -> WriteResult<()>;
    /// Deterministic (sorted) names, no "." / "..".
    fn list_dir(&mut self, dir: &str) -> WriteResult<Vec<String>>;
    fn mkdir_path(&mut self, dir: &str) -> WriteResult<()>;
    fn exists_path(&mut self, path: &str) -> WriteResult<bool>;
    /// Advisory: `[off, off + len)` of `path` is read soon. A missing file is ignored.
    fn willneed(&mut self, _path: &str, _off: u64, _len: u64) {}
    /// Parts by reference (WR): the byte ranges of the file behind `fd` that are patched after it is complete (a
    /// part's publish-time header and footer), so a reference's crc skips them. Called before the first write; only
    /// the logging vfs records them.
    fn note_ref_masks(&mut self, _fd: &WvFd, _masks: SmallMasks) -> WriteResult<()> {
        Ok(())
    }
    /// A second name for the file at `from` (a hard link). The default refuses; callers that can copy instead check
    /// [`WriteError::is_link_unsupported`].
    fn link_path(&mut self, _from: &str, _to: &str) -> WriteResult<()> {
        Err(WriteError::Contract { detail: LINK_UNSUPPORTED })
    }
    /// Whether `a` and `b` are on one file system (a hard link between them can work).
    fn same_device(&mut self, _a: &str, _b: &str) -> WriteResult<bool> {
        Ok(true)
    }
    /// How many names the file at `path` has (WR: a spool entry with another name is already linked elsewhere).
    fn link_count(&mut self, _path: &str) -> WriteResult<u64> {
        Ok(1)
    }
    /// A read handle on a part file, for the footer reads of a stats load or refold (`stats.rs`). The default reads
    /// the whole file into memory (the model vfs's); `RealVfs` opens it for positioned reads.
    fn part_io(&mut self, path: &str) -> WriteResult<Box<dyn pgrc_read::PartIo>> {
        Ok(Box::new(pgrc_read::MemPartIo::new(self.read_full(path)?, 0, 0)))
    }
}

/// The `Contract` detail of a vfs without hard links.
pub const LINK_UNSUPPORTED: &str = "this vfs makes no hard links";

impl WriteError {
    /// A `link_path` failure that a copy can stand in for: no hard links here, another file system, or the link
    /// count limit.
    pub fn is_link_unsupported(&self) -> bool {
        match self {
            WriteError::Contract { detail } => *detail == LINK_UNSUPPORTED,
            WriteError::Io { errno, .. } => [libc::EXDEV, libc::EPERM, libc::EMLINK, libc::ENOTSUP].contains(errno),
            _ => false,
        }
    }
}

// ---------------------------------------------------------------------------
// RealVfs — the production impl over the vfs:: shims
// ---------------------------------------------------------------------------

/// Production impl. ZST; all state is the kernel's (or SimVfs's under
/// `--cfg pgrust_sim`).
#[derive(Debug, Default, Clone, Copy)]
pub struct RealVfs;

/// The fsync(2) calls [`pg_fsync`] made in this process (D4225's witness: a publish under fsync=off makes none).
static REAL_FSYNCS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Every pgrcolumnar fsync of a real descriptor ([`RealVfs`]'s file and directory fsyncs, pgrc_am's table-dir
/// fsyncs) goes through here, under PostgreSQL's pg_fsync policy (D4225): with fsync=off (`enableFsync` false) it
/// returns 0 and syncs nothing, as fd.c's pg_fsync_no_writethrough returns before its fsync(2); else one plain fsync(2)
/// through the vfs choke, -1 / errno as `vfs::fsync` gives them. A directory is still opened and closed by its caller,
/// as fsync_fname_ext opens it whatever fsync says. [`MemVfs`] and the crash matrices model their own fsyncs and never
/// come here; under `--cfg pgrust_sim` the sim's durability model keeps every fsync (the crash batteries run with
/// fsync=off in the test boot and model fsync on).
pub fn pg_fsync(raw: i32) -> i32 {
    #[cfg(not(pgrust_sim))]
    {
        if !init_small::globals::enableFsync() {
            return 0;
        }
    }
    REAL_FSYNCS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    vfs::fsync(raw)
}

/// The fsync(2) calls [`pg_fsync`] has made in this process.
pub fn real_fsyncs() -> u64 {
    REAL_FSYNCS.load(std::sync::atomic::Ordering::Relaxed)
}

fn cpath(path: &str, op: &'static str) -> WriteResult<CString> {
    CString::new(path).map_err(|_| WriteError::Io {
        op,
        path: path.to_string(), // alloc: Cold
        errno: 0,
    })
}

fn io_err(op: &'static str, path: &str) -> WriteError {
    WriteError::Io {
        op,
        path: path.to_string(), // alloc: Cold
        errno: vfs::get_errno(),
    }
}

/// colload (k): the fan-out face of [`RealVfs`]'s `fsync_files`. `fsync_fanout(raws, status)` fsyncs each raw
/// descriptor through [`fsync_counted`] (EINTR retried) and writes its outcome to the same index of `status`: 0
/// synced, an errno > 0 failed, [`FSYNC_NOT_RUN`] (as given) not reached. It returns only once no fsync of `raws` is
/// still running. The writer cannot reach the runtime (pgrc_am, which depends on this crate, is below it), so the
/// runtime binding (`pgrc_ingest_par`) installs it: the fsyncs run as morsels on the runtime pool, inside
/// blocking-I/O sections, with the calling thread taking its share. Not installed: serial `fsync_file` calls.
pub type FsyncFanout = fn(raws: &[i32], status: &mut [i32]);

/// colload (k): the status of a descriptor the fan-out did not reach.
pub const FSYNC_NOT_RUN: i32 = -1;

static FSYNC_FANOUT: pgsync::OnceLock<FsyncFanout> = pgsync::OnceLock::new();

/// colload (k): install the [`FsyncFanout`] (first install wins; later calls are no-ops).
pub fn install_fsync_fanout(f: FsyncFanout) {
    let _ = FSYNC_FANOUT.set(f);
}

/// colload (k): one fsync(2) of `raw`, counted in [`real_fsyncs`], WITHOUT the enableFsync check: for the
/// [`FsyncFanout`] on a pool worker, after the calling backend's `fsync_files` checked its policy. EINTR retried;
/// 0 or the errno.
pub fn fsync_counted(raw: i32) -> i32 {
    loop {
        REAL_FSYNCS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if vfs::fsync(raw) == 0 {
            return 0;
        }
        let errno = vfs::get_errno();
        if errno != libc::EINTR {
            return errno.max(1);
        }
    }
}

/// colload (k): the most descriptors a publish holds open at once for one [`WriteVfs::fsync_files`] batch.
pub const FSYNC_BATCH: usize = 64;

impl RealVfs {
    fn open_flags(&mut self, path: &str, flags: libc::c_int, op: &'static str) -> WriteResult<WvFd> {
        let c = cpath(path, op)?;
        let raw = vfs::open(&c, flags, 0o600 as libc::mode_t);
        if raw < 0 {
            return Err(io_err(op, path));
        }
        Ok(WvFd {
            raw,
            key: 0,
            path: path.to_string(),
        })
    }
}

impl WriteVfs for RealVfs {
    fn create_rw(&mut self, path: &str) -> WriteResult<WvFd> {
        self.open_flags(
            path,
            libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
            "create_rw",
        )
    }

    fn open_rw(&mut self, path: &str) -> WriteResult<WvFd> {
        self.open_flags(path, libc::O_RDWR, "open_rw")
    }

    /// vfs contract v2 `link` (link(2)); EXDEV across file systems.
    fn link_path(&mut self, from: &str, to: &str) -> WriteResult<()> {
        let cf = cpath(from, "link")?;
        let ct = cpath(to, "link")?;
        if vfs::link(&cf, &ct) < 0 {
            return Err(io_err("link", from));
        }
        Ok(())
    }

    fn link_count(&mut self, path: &str) -> WriteResult<u64> {
        let c = cpath(path, "stat")?;
        let mut info = vfs::FileInfo::zeroed();
        if vfs::stat(&c, &mut info) < 0 {
            return Err(io_err("stat", path));
        }
        Ok(info.nlink)
    }

    fn same_device(&mut self, a: &str, b: &str) -> WriteResult<bool> {
        let mut dev = [0u64; 2];
        for (i, p) in [a, b].into_iter().enumerate() {
            let c = cpath(p, "stat")?;
            let mut info = vfs::FileInfo::zeroed();
            if vfs::stat(&c, &mut info) < 0 {
                return Err(io_err("stat", p));
            }
            dev[i] = info.dev;
        }
        Ok(dev[0] == dev[1])
    }

    fn willneed(&mut self, path: &str, off: u64, len: u64) {
        if let Ok(fd) = self.open_flags(path, libc::O_RDONLY, "open_ro") {
            let _ = vfs::fadvise_willneed(fd.raw, off as libc::off_t, len as libc::off_t);
            let _ = self.close_file(fd);
        }
    }

    fn pwrite_at(&mut self, fd: &WvFd, off: u64, bytes: &[u8]) -> WriteResult<()> {
        let mut done = 0usize;
        while done < bytes.len() {
            let n = vfs::pwrite(fd.raw, &bytes[done..], (off + done as u64) as libc::off_t);
            if n < 0 {
                if vfs::get_errno() == libc::EINTR {
                    continue;
                }
                return Err(io_err("pwrite", &fd.path));
            }
            if n == 0 {
                return Err(io_err("pwrite(short)", &fd.path));
            }
            done += n as usize;
        }
        // GL-BULKWRITE-1 (#686): a sealed part is up to 256 MiB of dirty
        // page cache in one call; account it against the dirty-page
        // governor (no-op unless armed). WHAT is written is untouched —
        // parts stay byte-identical; only writeback timing changes.
        write_governor::note_write(fd.raw, off, bytes.len() as u64);
        // SEALSIZE fix 3: a paced seal starts each piece's writeback now (sync_file_range WRITE, no wait), so its
        // part does not reach the disk as one burst of page-cache writeback later
        if crate::walsink::pace::active() {
            let _ = vfs::flush_range(fd.raw, off as libc::off_t, bytes.len() as libc::off_t);
        }
        Ok(())
    }

    fn fsync_file(&mut self, fd: &WvFd) -> WriteResult<()> {
        // EINTR retry (as the v1 segment files did).
        while pg_fsync(fd.raw) < 0 {
            if vfs::get_errno() != libc::EINTR {
                return Err(io_err("fsync", &fd.path));
            }
        }
        Ok(())
    }

    #[cfg(not(pgrust_sim))]
    fn fsync_files(&mut self, fds: &[WvFd]) -> WriteResult<()> {
        // Fan out only when the policy syncs at all: with fsync=off the serial loop is a run of no-op pg_fsync calls
        // (and enableFsync is the backend's setting, read here on its thread, not on a pool worker).
        let fanout = match FSYNC_FANOUT.get() {
            Some(f) if fds.len() > 1 && init_small::globals::enableFsync() => *f,
            _ => return fds.iter().try_for_each(|fd| self.fsync_file(fd)),
        };
        for batch in fds.chunks(FSYNC_BATCH) {
            let mut raws = [0i32; FSYNC_BATCH];
            for (r, fd) in raws.iter_mut().zip(batch) {
                *r = fd.raw;
            }
            let mut status = [FSYNC_NOT_RUN; FSYNC_BATCH];
            fanout(&raws[..batch.len()], &mut status[..batch.len()]);
            // In `fds` order: the first failure is the one reported; a descriptor the fan-out did not reach is
            // synced here, so `Ok` always means every one is durable.
            for (fd, st) in batch.iter().zip(&status) {
                match *st {
                    0 => {}
                    FSYNC_NOT_RUN => self.fsync_file(fd)?,
                    errno => return Err(WriteError::Io { op: "fsync", path: fd.path.to_string(), errno }), // alloc: Cold
                }
            }
        }
        Ok(())
    }

    fn close_file(&mut self, fd: WvFd) -> WriteResult<()> {
        // GL-BULKWRITE-1 (#686): kick + forget any pending governor window
        // before the descriptor number can be reused.
        write_governor::note_close(fd.raw);
        if vfs::close(fd.raw) < 0 {
            return Err(io_err("close", &fd.path));
        }
        Ok(())
    }

    fn read_full(&mut self, path: &str) -> WriteResult<Vec<u8>> {
        self.read_prefix(path, usize::MAX)
    }

    fn part_io(&mut self, path: &str) -> WriteResult<Box<dyn pgrc_read::PartIo>> {
        let c = cpath(path, "part open")?;
        pgrc_read::VfsPartIo::open(&c).map(|io| Box::new(io) as Box<dyn pgrc_read::PartIo>).map_err(|e| match e {
            pgrc_read::ReadError::Io { errno, .. } => WriteError::Io { op: "part open", path: path.to_string(), errno },
            pgrc_read::ReadError::Format(f) => WriteError::Format(f),
            _ => WriteError::Io { op: "part open", path: path.to_string(), errno: 0 },
        })
    }

    fn read_prefix(&mut self, path: &str, max: usize) -> WriteResult<Vec<u8>> {
        let fd = self.open_flags(path, libc::O_RDONLY, "open_ro")?;
        let mut out = Vec::new();
        let mut buf = [0u8; 64 * 1024];
        let mut off: u64 = 0;
        loop {
            let want = (max - out.len()).min(buf.len());
            if want == 0 {
                break;
            }
            let n = vfs::pread(fd.raw, &mut buf[..want], off as libc::off_t);
            if n < 0 {
                if vfs::get_errno() == libc::EINTR {
                    continue;
                }
                let e = io_err("pread", path);
                let _ = self.close_file(fd);
                return Err(e);
            }
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n as usize]);
            off += n as u64;
        }
        self.close_file(fd)?;
        Ok(out)
    }

    fn read_at(&mut self, path: &str, off: u64, len: usize) -> WriteResult<Vec<u8>> {
        let fd = self.open_flags(path, libc::O_RDONLY, "open_ro")?;
        let mut out = match pgrc_format::try_filled(len, 0u8, "wvfs") {
            Ok(v) => v,
            Err(e) => {
                let _ = self.close_file(fd);
                return Err(e.into());
            }
        };
        let mut done = 0usize;
        while done < len {
            let n = vfs::pread(fd.raw, &mut out[done..], (off + done as u64) as libc::off_t);
            if n < 0 {
                if vfs::get_errno() == libc::EINTR {
                    continue;
                }
                let e = io_err("pread", path);
                let _ = self.close_file(fd);
                return Err(e);
            }
            if n == 0 {
                break;
            }
            done += n as usize;
        }
        out.truncate(done);
        self.close_file(fd)?;
        Ok(out)
    }

    fn rename_path(&mut self, from: &str, to: &str) -> WriteResult<()> {
        let cf = cpath(from, "rename")?;
        let ct = cpath(to, "rename")?;
        if vfs::rename(&cf, &ct) < 0 {
            return Err(io_err("rename", from));
        }
        Ok(())
    }

    fn unlink_path(&mut self, path: &str) -> WriteResult<()> {
        let c = cpath(path, "unlink")?;
        if vfs::unlink(&c) < 0 {
            return Err(io_err("unlink", path));
        }
        Ok(())
    }

    fn fsync_dir(&mut self, dir: &str) -> WriteResult<()> {
        let c = cpath(dir, "fsync_dir")?;
        let raw = vfs::open(&c, libc::O_RDONLY, 0);
        if raw < 0 {
            return Err(io_err("fsync_dir(open)", dir));
        }
        while pg_fsync(raw) < 0 {
            let e = vfs::get_errno();
            if e == libc::EINTR {
                continue;
            }
            // Some filesystems refuse dir fsync; tolerate exactly the
            // fd.c/fsync_fname_ext posture (EBADF/EINVAL non-fatal).
            if e == libc::EBADF || e == libc::EINVAL {
                break;
            }
            vfs::close(raw);
            return Err(WriteError::Io {
                op: "fsync_dir",
                path: dir.to_string(), // alloc: Cold
                errno: e,
            });
        }
        if vfs::close(raw) < 0 {
            return Err(io_err("fsync_dir(close)", dir));
        }
        Ok(())
    }

    fn list_dir(&mut self, dir: &str) -> WriteResult<Vec<String>> {
        let c = cpath(dir, "list_dir")?;
        let iter = vfs::read_dir(&c).map_err(|errno| WriteError::Io {
            op: "list_dir",
            path: dir.to_string(), // alloc: Cold
            errno,
        })?;
        let mut names: Vec<String> = Vec::new();
        for entry in iter {
            let entry = entry.map_err(|errno| WriteError::Io {
                op: "list_dir(entry)",
                path: dir.to_string(), // alloc: Cold
                errno,
            })?;
            names.push(String::from_utf8_lossy(entry.as_bytes()).into_owned());
        }
        // PosixVfs order is directory order; sort for determinism (SimVfs is
        // already BTree-ordered — sorting is idempotent there).
        names.sort();
        Ok(names)
    }

    fn mkdir_path(&mut self, dir: &str) -> WriteResult<()> {
        let c = cpath(dir, "mkdir")?;
        if vfs::mkdir(&c, 0o700 as libc::mode_t) < 0 {
            return Err(io_err("mkdir", dir));
        }
        Ok(())
    }

    fn exists_path(&mut self, path: &str) -> WriteResult<bool> {
        let c = cpath(path, "stat")?;
        let mut info = vfs::FileInfo::default();
        if vfs::stat(&c, &mut info) == 0 {
            return Ok(true);
        }
        if vfs::get_errno() == libc::ENOENT {
            return Ok(false);
        }
        Err(io_err("stat", path))
    }
}

// ---------------------------------------------------------------------------
// MemVfs — the deterministic crash-model impl (default-CI test vehicle)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub(crate) struct MemFile {
    cur: Vec<u8>,
    durable: Vec<u8>,
}

impl MemFile {
    /// (volatile, durable) content: the test kit's crash reverts one to the other.
    pub(crate) fn sides(&mut self) -> (&mut Vec<u8>, &Vec<u8>) {
        (&mut self.cur, &self.durable)
    }
}

#[derive(Debug, Default, Clone)]
pub(crate) struct MemDir {
    /// name → inode.
    cur: BTreeMap<String, u64>,
    durable: BTreeMap<String, u64>,
}

impl MemDir {
    /// (volatile, durable) dirents: the test kit's crash reverts one to the other.
    pub(crate) fn sides(&mut self) -> (&mut BTreeMap<String, u64>, &BTreeMap<String, u64>) {
        (&mut self.cur, &self.durable)
    }

    /// The volatile dirents.
    pub(crate) fn cur(&self) -> &BTreeMap<String, u64> {
        &self.cur
    }
}

/// Deterministic in-memory file system with a kill-9-shaped durability
/// model:
///
/// - file WRITES are volatile until `fsync_file` (content law);
/// - namespace ops (create/rename/unlink dirents) are volatile until the
///   parent directory's `fsync_dir` (dirent law — the SimVfs rule-3 model);
/// - [`crate::testkit::crash_and_revive`] discards every volatile state: dirents
///   revert to the durable set, surviving files revert to durable content,
///   files with no durable dirent vanish, open fds die;
/// - `crash_at_op(n)` kills the universe BEFORE the nth subsequent op
///   applies — the crash-matrix injector (every op boundary is a kill -9
///   point);
/// - every op appends a readable line to [`MemVfs::ops`] — the ordering
///   witness the publish-ordering tests string-match (the sim_dirsync
///   op-trace pattern).
#[derive(Debug, Default)]
pub struct MemVfs {
    pub(crate) files: BTreeMap<u64, MemFile>,
    pub(crate) dirs: BTreeMap<String, MemDir>,
    pub(crate) open: BTreeMap<u64, u64>,
    next_ino: u64,
    next_fd: u64,
    pub(crate) ops: Vec<String>,
    op_count: u64,
    pub(crate) crash_at: Option<u64>,
    pub(crate) killed: bool,
}

fn split_path(path: &str) -> WriteResult<(&str, &str)> {
    match path.rsplit_once('/') {
        Some((dir, name)) if !dir.is_empty() && !name.is_empty() => Ok((dir, name)),
        _ => Err(WriteError::Contract {
            detail: "MemVfs paths are dir/name shaped",
        }),
    }
}

impl MemVfs {
    pub fn new() -> MemVfs {
        MemVfs::default()
    }

    /// The op log (readable lines, in op order).
    pub fn ops(&self) -> &[String] {
        &self.ops
    }

    pub fn op_count(&self) -> u64 {
        self.op_count
    }

    /// Arm a crash BEFORE the `n`th subsequent op applies (1-based: `1`
    /// kills the very next op).
    pub fn crash_at_op(&mut self, n: u64) {
        self.crash_at = Some(self.op_count + n);
    }

    pub fn killed(&self) -> bool {
        self.killed
    }

    fn tick(&mut self, line: String) -> WriteResult<()> {
        if self.killed {
            return Err(WriteError::Io {
                op: "killed",
                path: line,
                errno: 0,
            });
        }
        self.op_count += 1;
        if self.crash_at == Some(self.op_count) {
            self.killed = true;
            return Err(WriteError::Io {
                op: "crash",
                path: line,
                errno: 0,
            });
        }
        self.ops.push(line);
        Ok(())
    }

    fn ino_of(&self, path: &str) -> WriteResult<Option<u64>> {
        let (dir, name) = split_path(path)?;
        Ok(self.dirs.get(dir).and_then(|d| d.cur.get(name)).copied())
    }
}

impl WriteVfs for MemVfs {
    fn create_rw(&mut self, path: &str) -> WriteResult<WvFd> {
        self.tick(format!("create {path}"))?;
        let (dir, name) = split_path(path)?;
        let d = self.dirs.get_mut(dir).ok_or(WriteError::Io {
            op: "create_rw",
            path: path.to_string(),
            errno: libc::ENOENT,
        })?;
        let ino = match d.cur.get(name) {
            Some(&ino) => {
                // O_TRUNC semantics.
                self.files.get_mut(&ino).expect("live ino").cur.clear();
                ino
            }
            None => {
                self.next_ino += 1;
                let ino = self.next_ino;
                d.cur.insert(name.to_string(), ino);
                self.files.insert(ino, MemFile::default());
                ino
            }
        };
        self.next_fd += 1;
        self.open.insert(self.next_fd, ino);
        Ok(WvFd {
            raw: -1,
            key: self.next_fd,
            path: path.to_string(),
        })
    }

    fn open_rw(&mut self, path: &str) -> WriteResult<WvFd> {
        self.tick(format!("open {path}"))?;
        let ino = self.ino_of(path)?.ok_or(WriteError::Io {
            op: "open_rw",
            path: path.to_string(),
            errno: libc::ENOENT,
        })?;
        self.next_fd += 1;
        self.open.insert(self.next_fd, ino);
        Ok(WvFd {
            raw: -1,
            key: self.next_fd,
            path: path.to_string(),
        })
    }

    fn pwrite_at(&mut self, fd: &WvFd, off: u64, bytes: &[u8]) -> WriteResult<()> {
        self.tick(format!("pwrite {} off={} len={}", fd.path, off, bytes.len()))?;
        let ino = *self.open.get(&fd.key).ok_or(WriteError::Io {
            op: "pwrite",
            path: fd.path.clone(),
            errno: libc::EBADF,
        })?;
        let f = self.files.get_mut(&ino).expect("open ino live");
        let end = off as usize + bytes.len();
        if f.cur.len() < end {
            f.cur.resize(end, 0);
        }
        f.cur[off as usize..end].copy_from_slice(bytes);
        Ok(())
    }

    fn fsync_file(&mut self, fd: &WvFd) -> WriteResult<()> {
        self.tick(format!("fsync {}", fd.path))?;
        let ino = *self.open.get(&fd.key).ok_or(WriteError::Io {
            op: "fsync",
            path: fd.path.clone(),
            errno: libc::EBADF,
        })?;
        let f = self.files.get_mut(&ino).expect("open ino live");
        f.durable = f.cur.clone();
        Ok(())
    }

    fn close_file(&mut self, fd: WvFd) -> WriteResult<()> {
        self.tick(format!("close {}", fd.path))?;
        self.open.remove(&fd.key);
        Ok(())
    }

    fn read_full(&mut self, path: &str) -> WriteResult<Vec<u8>> {
        // Reads are not durability ops; still logged for completeness.
        self.tick(format!("read {path}"))?;
        let ino = self.ino_of(path)?.ok_or(WriteError::Io {
            op: "read_full",
            path: path.to_string(),
            errno: libc::ENOENT,
        })?;
        Ok(self.files.get(&ino).expect("live ino").cur.clone())
    }

    fn rename_path(&mut self, from: &str, to: &str) -> WriteResult<()> {
        self.tick(format!("rename {from} -> {to}"))?;
        let (fdir, fname) = split_path(from)?;
        let (tdir, tname) = split_path(to)?;
        // rename(2): two names of one inode stay as they are.
        if let (Some(a), Some(b)) = (self.ino_of(from)?, self.ino_of(to)?) {
            if a == b {
                return Ok(());
            }
        }
        let ino = self
            .dirs
            .get_mut(fdir)
            .and_then(|d| d.cur.remove(fname))
            .ok_or(WriteError::Io {
                op: "rename",
                path: from.to_string(),
                errno: libc::ENOENT,
            })?;
        let td = self.dirs.get_mut(tdir).ok_or(WriteError::Io {
            op: "rename",
            path: to.to_string(),
            errno: libc::ENOENT,
        })?;
        td.cur.insert(tname.to_string(), ino);
        Ok(())
    }

    fn unlink_path(&mut self, path: &str) -> WriteResult<()> {
        self.tick(format!("unlink {path}"))?;
        let (dir, name) = split_path(path)?;
        let removed = self.dirs.get_mut(dir).and_then(|d| d.cur.remove(name));
        if removed.is_none() {
            return Err(WriteError::Io {
                op: "unlink",
                path: path.to_string(), // alloc: Cold
                errno: libc::ENOENT,
            });
        }
        Ok(())
    }

    fn fsync_dir(&mut self, dir: &str) -> WriteResult<()> {
        self.tick(format!("fsyncdir {dir}"))?;
        let d = self.dirs.get_mut(dir).ok_or(WriteError::Io {
            op: "fsync_dir",
            path: dir.to_string(),
            errno: libc::ENOENT,
        })?;
        d.durable = d.cur.clone();
        Ok(())
    }

    fn list_dir(&mut self, dir: &str) -> WriteResult<Vec<String>> {
        self.tick(format!("listdir {dir}"))?;
        let d = self.dirs.get(dir).ok_or(WriteError::Io {
            op: "list_dir",
            path: dir.to_string(),
            errno: libc::ENOENT,
        })?;
        Ok(d.cur.keys().cloned().collect())
    }

    fn mkdir_path(&mut self, dir: &str) -> WriteResult<()> {
        self.tick(format!("mkdir {dir}"))?;
        self.dirs.entry(dir.to_string()).or_default();
        Ok(())
    }

    fn exists_path(&mut self, path: &str) -> WriteResult<bool> {
        self.tick(format!("stat {path}"))?;
        Ok(self.ino_of(path)?.is_some())
    }

    fn link_count(&mut self, path: &str) -> WriteResult<u64> {
        let ino = self.ino_of(path)?.ok_or(WriteError::Io { op: "stat", path: path.to_string(), errno: libc::ENOENT })?;
        Ok(self.dirs.values().flat_map(|d| d.cur.values()).filter(|&&i| i == ino).count() as u64)
    }

    /// A second dirent for the same inode: a write through either name shows through both. Volatile until the new
    /// name's directory is fsynced, like any dirent.
    fn link_path(&mut self, from: &str, to: &str) -> WriteResult<()> {
        self.tick(format!("link {from} -> {to}"))?;
        let ino = self.ino_of(from)?.ok_or(WriteError::Io { op: "link", path: from.to_string(), errno: libc::ENOENT })?;
        let (tdir, tname) = split_path(to)?;
        let td = self.dirs.get_mut(tdir).ok_or(WriteError::Io { op: "link", path: to.to_string(), errno: libc::ENOENT })?;
        if td.cur.contains_key(tname) {
            return Err(WriteError::Io { op: "link", path: to.to_string(), errno: libc::EEXIST });
        }
        td.cur.insert(tname.to_string(), ino);
        Ok(())
    }
}
