//! `backend-storage-file-vfs` — the DST P1 VFS contract surface (contract v1,
//! frozen at tag `vfs-trait-v1`; revised to v2 at tag `vfs-trait-v2`, see the
//! [`Vfs`] trait's revision note).
//!
//! The [`Vfs`] trait exists for **shape enforcement and SimVfs conformance**,
//! not dynamic dispatch. Product builds monomorphize [`ActiveVfs`] =
//! [`posix::PosixVfs`]; the sim harness selects `SimVfs` with the non-default
//! `--cfg pgrust_sim` (set exclusively by the sim harness RUSTFLAGS — never in
//! `.cargo/config`, product profiles, or CI cluster submit envs). Product codegen
//! is byte-identical to raw libc calls by construction: every op is an
//! `#[inline]` single-syscall wrapper.
//!
//! Binding rules (contract §1.1):
//! - **Errno contract (C-shaped):** every op returns `-1`/negative on failure
//!   and sets errno through the shared TLS cell ([`set_errno`]) that `fd` uses.
//!   No `io::Error` wrapping at this layer; the raw errno stays visible above.
//! - **Ops are single-shot.** All retry policy (EINTR loops, fd.c's
//!   EMFILE-retry `LruOpen` → `ReleaseLruFiles` dance) lives ABOVE the trait,
//!   in `fd`, exactly where it lives today. SimVfs never emits EINTR.
//! - **PG error-code mapping stays ABOVE the trait** (`errcode_for_file_access`
//!   in fd). SimVfs fault injection (P4) may only emit errnos from that
//!   vocabulary.
//! - **Composites stay above:** `durable_rename`, `durable_unlink`,
//!   `MakePGDirectory`, the VFD table/LRU/`max_safe_fds` machinery, temp-file
//!   accounting, resowner hooks, and waitevent brackets are fd's, untouched.
//!   The trait never owns, recomputes, or intercepts the fd budget.
//! - **Platform splits live INSIDE PosixVfs:** macOS F_FULLFSYNC, the
//!   synthetic `PG_O_DIRECT` bit → `fcntl(F_NOCACHE)` mapping, F_RDADVISE vs
//!   posix_fadvise.
//! - **Out of Vfs scope, permanently:** io_uring (stays behind `aio_seams`),
//!   anonymous mmap, stdio streams/pipes (`AllocateFile` fopen /
//!   `OpenPipeStream` popen), wait-event instrumentation.

#![allow(clippy::missing_safety_doc)]

use core::ffi::CStr;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};

pub use libc::{c_int, mode_t, off_t};

pub mod posix;

// ---------------------------------------------------------------------------
// Errno plumbing — the shared TLS cell (the C `errno` itself). `fd` re-exports
// these; PosixVfs sets errno implicitly via the kernel, SimVfs sets it here.
// ---------------------------------------------------------------------------

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
fn errno_location() -> *mut libc::c_int {
    // SAFETY: returns the thread-local errno lvalue.
    unsafe { libc::__error() }
}
#[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "freebsd")))]
fn errno_location() -> *mut libc::c_int {
    // SAFETY: returns the thread-local errno lvalue.
    unsafe { libc::__errno_location() }
}

#[inline]
pub fn get_errno() -> i32 {
    // SAFETY: reading the thread-local errno.
    unsafe { *errno_location() }
}

#[inline]
pub fn set_errno(value: i32) {
    // SAFETY: writing the thread-local errno.
    unsafe {
        *errno_location() = value;
    }
}

// ---------------------------------------------------------------------------
// PG_O_DIRECT — the synthetic direct-IO open flag (storage/fd.h). Lives here
// because the PG_O_DIRECT → F_NOCACHE mapping is PosixVfs's platform split;
// `fd` re-exports it.
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
pub const PG_O_DIRECT: i32 = libc::O_DIRECT;
// storage/fd.h: the F_NOCACHE stand-in bit; asserted disjoint from open(2) flags.
#[cfg(target_os = "macos")]
pub const PG_O_DIRECT: i32 = 0x8000_0000u32 as i32;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub const PG_O_DIRECT: i32 = 0;

#[cfg(target_os = "macos")]
const _: () = assert!(
    PG_O_DIRECT
        & (libc::O_APPEND
            | libc::O_CLOEXEC
            | libc::O_CREAT
            | libc::O_DSYNC
            | libc::O_EXCL
            | libc::O_RDWR
            | libc::O_RDONLY
            | libc::O_SYNC
            | libc::O_TRUNC
            | libc::O_WRONLY)
        == 0
);

// ---------------------------------------------------------------------------
// Plain types
// ---------------------------------------------------------------------------

/// Stat result, plain-typed. `mode` is `st_mode` widened to u32 (mode_t is u16
/// on macOS).
///
/// Contract v1.1 (additive revision, WS-B request, tag `vfs-trait-v1.1`):
/// `dev`/`ino` carry the (st_dev, st_ino) identity vocabulary the part-cache
/// staleness rule and the SegMap::open_shared registry key speak.
///
/// Contract v1.2 (additive revision, WS-A inc-3, tag `vfs-trait-v1.2`):
/// `uid`/`gid` carry the ownership words basebackup's tar headers stamp.
///
/// Construct via [`FileInfo::zeroed`] (or struct-update from it) so additive
/// revisions stay non-breaking. SimVfs fills synthetic stable values.
#[derive(Clone, Copy, Debug, Default)]
pub struct FileInfo {
    pub size: i64,
    pub mode: u32,
    pub nlink: u64,
    pub mtime_sec: i64,
    pub mtime_nsec: i64,
    pub dev: u64,
    pub ino: u64,
    pub uid: u32,
    pub gid: u32,
    pub blksize: i64,
}

impl FileInfo {
    pub const fn zeroed() -> Self {
        FileInfo {
            size: 0,
            mode: 0,
            nlink: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            uid: 0,
            gid: 0,
            blksize: 0,
        }
    }

    #[inline]
    pub fn is_dir(&self) -> bool {
        self.mode & (libc::S_IFMT as u32) == libc::S_IFDIR as u32
    }

    #[inline]
    pub fn is_file(&self) -> bool {
        self.mode & (libc::S_IFMT as u32) == libc::S_IFREG as u32
    }

    #[inline]
    pub fn is_symlink(&self) -> bool {
        self.mode & (libc::S_IFMT as u32) == libc::S_IFLNK as u32
    }
}

/// Errno-carrying result for the ops that can't express failure in-band.
pub type VfsResult<T> = Result<T, c_int>;

// ---------------------------------------------------------------------------
// PgPath — an OS path as the kernel sees it: raw bytes, no encoding. C's
// `char path[MAXPGPATH]` carries dirent names, readlink targets and WAL
// record payloads untouched; a `String` at this layer would force a lossy
// UTF-8 decode (U+FFFD) that the kernel then fails to resolve. Convert to
// `str` only where C itself renders the bytes as text (an ereport message,
// a manifest "Encoded-Path" decision), never before a syscall or a tar
// header.
// ---------------------------------------------------------------------------

/// Owned byte path. `Deref<Target = [u8]>` for slicing/comparison;
/// [`PgPath::to_c_string`] is the syscall boundary.
#[derive(Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PgPath(Vec<u8>);

impl PgPath {
    pub const fn new() -> Self {
        PgPath(Vec::new())
    }

    pub fn from_bytes(bytes: &[u8]) -> Self {
        PgPath(bytes.to_owned())
    }

    pub fn from_vec(bytes: Vec<u8>) -> Self {
        PgPath(bytes)
    }

    #[cfg(any(unix, target_os = "wasi"))]
    pub fn from_os_str(s: &std::ffi::OsStr) -> Self {
        #[cfg(unix)]
        use std::os::unix::ffi::OsStrExt;
        #[cfg(target_os = "wasi")]
        use std::os::wasi::ffi::OsStrExt;
        PgPath(s.as_bytes().to_owned())
    }

    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    /// The bytes as an `OsStr` (unix/wasi: the same bytes, no re-encoding),
    /// for the few std::fs composites fd does not front.
    #[cfg(any(unix, target_os = "wasi"))]
    pub fn as_os_str(&self) -> &std::ffi::OsStr {
        #[cfg(unix)]
        use std::os::unix::ffi::OsStrExt;
        #[cfg(target_os = "wasi")]
        use std::os::wasi::ffi::OsStrExt;
        std::ffi::OsStr::from_bytes(&self.0)
    }

    #[cfg(any(unix, target_os = "wasi"))]
    pub fn as_std_path(&self) -> &std::path::Path {
        std::path::Path::new(self.as_os_str())
    }

    /// The syscall form. An interior NUL (impossible for a kernel-sourced
    /// name) degrades to the empty path, which every op rejects with ENOENT
    /// — fd's `cpath` contract.
    pub fn to_c_string(&self) -> std::ffi::CString {
        std::ffi::CString::new(self.0.clone()).unwrap_or_default()
    }

    /// Valid UTF-8 view, if the bytes are; the honest gate for callers whose
    /// C counterpart parses the name as text (relation file names, WAL
    /// segment names).
    pub fn as_str(&self) -> Option<&str> {
        core::str::from_utf8(&self.0).ok()
    }

    /// Message rendering (U+FFFD for undecodable bytes) — the point at which
    /// C's `%s` in an ereport meets pgrust's `String` messages.
    pub fn to_string_lossy(&self) -> String {
        String::from_utf8_lossy(&self.0).into_owned()
    }

    /// `self + "/" + name` (C: `snprintf(pathbuf, "%s/%s", path, de->d_name)`).
    pub fn join(&self, name: &[u8]) -> PgPath {
        // MAXPGPATH-bounded; grown in place like every other path buffer.
        let mut v = Vec::new();
        v.extend_from_slice(&self.0);
        v.push(b'/');
        v.extend_from_slice(name);
        PgPath(v)
    }

    pub fn push_bytes(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }

    /// The last component (bytes after the final '/'; the whole path when
    /// there is none).
    pub fn file_name(&self) -> &[u8] {
        match self.0.iter().rposition(|&b| b == b'/') {
            Some(i) => &self.0[i + 1..],
            None => &self.0,
        }
    }

    /// The bytes before the final '/', if any.
    pub fn parent(&self) -> Option<&[u8]> {
        self.0.iter().rposition(|&b| b == b'/').map(|i| &self.0[..i])
    }
}

impl core::ops::Deref for PgPath {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl AsRef<[u8]> for PgPath {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl From<&str> for PgPath {
    fn from(s: &str) -> Self {
        PgPath(s.as_bytes().to_owned())
    }
}

impl From<&String> for PgPath {
    fn from(s: &String) -> Self {
        PgPath(s.as_bytes().to_owned())
    }
}

impl From<String> for PgPath {
    fn from(s: String) -> Self {
        PgPath(s.into_bytes())
    }
}

impl From<Vec<u8>> for PgPath {
    fn from(v: Vec<u8>) -> Self {
        PgPath(v)
    }
}

impl From<&[u8]> for PgPath {
    fn from(b: &[u8]) -> Self {
        PgPath(b.to_owned())
    }
}

impl PartialEq<str> for PgPath {
    fn eq(&self, other: &str) -> bool {
        self.0 == other.as_bytes()
    }
}

impl PartialEq<&str> for PgPath {
    fn eq(&self, other: &&str) -> bool {
        self.0 == other.as_bytes()
    }
}

impl PartialEq<[u8]> for PgPath {
    fn eq(&self, other: &[u8]) -> bool {
        self.0 == other
    }
}

impl core::fmt::Display for PgPath {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&String::from_utf8_lossy(&self.0))
    }
}

impl core::fmt::Debug for PgPath {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?}", String::from_utf8_lossy(&self.0))
    }
}

enum DirIterInner {
    /// Kernel readdir order, "." and ".." elided (std::fs::read_dir semantics —
    /// identical to what fd's AllocateDir has always exposed to its callers).
    Posix(std::fs::ReadDir),
    /// SimVfs constructor: a pre-collected name list (BTree order — the
    /// deterministic-readdir rule).
    Named(std::vec::IntoIter<PgPath>),
}

/// Directory iterator. Yields entry names as the kernel returned them
/// ([`PgPath`] — raw `d_name` bytes, exactly C's `struct dirent`), never "."
/// or "..". `Err(errno)` mirrors a readdir failure mid-stream. fd's
/// `ReadDirExtended` is where the historical lossy-UTF-8 `DirEnt` view is
/// derived; byte-faithful walkers use `with_allocated_dir_path`.
pub struct VfsDirIter(DirIterInner);

/// C `struct dirent` d_type as readdir(3) reported it: `Unknown` is
/// DT_UNKNOWN (and every type get_dirent_type does not classify), which the
/// SimVfs arm always reports.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VfsDType {
    Unknown,
    Reg,
    Dir,
    Lnk,
}

/// One readdir(3) entry: raw `d_name` bytes plus `d_type`.
pub struct VfsDirEnt {
    pub d_name: PgPath,
    pub d_type: VfsDType,
}

impl VfsDirIter {
    /// SimVfs constructor: names must already be in deterministic (BTree)
    /// order and exclude "." / "..".
    pub fn from_names(names: Vec<String>) -> Self {
        let mut paths = Vec::new();
        for name in names {
            paths.push(PgPath::from(name));
        }
        VfsDirIter(DirIterInner::Named(paths.into_iter()))
    }

    pub(crate) fn from_read_dir(rd: std::fs::ReadDir) -> Self {
        VfsDirIter(DirIterInner::Posix(rd))
    }
}

impl VfsDirIter {
    /// The next entry with its `d_type` (the Posix arm reads the dirent's
    /// type; std stats a DT_UNKNOWN entry itself, which is C's fallback).
    pub fn next_entry(&mut self) -> Option<Result<VfsDirEnt, c_int>> {
        match &mut self.0 {
            DirIterInner::Posix(rd) => match rd.next() {
                Some(Ok(entry)) => {
                    let d_type = match entry.file_type() {
                        Ok(ft) if ft.is_dir() => VfsDType::Dir,
                        Ok(ft) if ft.is_file() => VfsDType::Reg,
                        Ok(ft) if ft.is_symlink() => VfsDType::Lnk,
                        _ => VfsDType::Unknown,
                    };
                    #[cfg(any(unix, target_os = "wasi"))]
                    let d_name = PgPath::from_os_str(&entry.file_name());
                    #[cfg(not(any(unix, target_os = "wasi")))]
                    let d_name = PgPath::from(entry.file_name().to_string_lossy().into_owned());
                    Some(Ok(VfsDirEnt { d_name, d_type }))
                }
                Some(Err(e)) => {
                    let en = e.raw_os_error().unwrap_or(libc::EIO);
                    set_errno(en);
                    Some(Err(en))
                }
                None => None,
            },
            DirIterInner::Named(names) => {
                names.next().map(|d_name| Ok(VfsDirEnt { d_name, d_type: VfsDType::Unknown }))
            }
        }
    }
}

impl Iterator for VfsDirIter {
    type Item = Result<PgPath, c_int>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_entry().map(|r| r.map(|e| e.d_name))
    }
}

/// The full posix fd-budget probe outcome (`count_usable_fds` shape). The
/// trait op [`Vfs::fd_budget_probe`] returns only the budget; fd's diagnostics
/// (`already_open`, getrlimit/dup warnings) consume this report through
/// [`fd_budget_probe_report`].
#[derive(Clone, Copy, Debug)]
pub struct FdBudgetProbe {
    /// Successfully probed descriptors (the usable budget).
    pub used: i32,
    /// Highest fd number observed during the probe.
    pub highest_fd: i32,
    /// getrlimit(RLIMIT_NOFILE) failed; `getrlimit_errno` holds why.
    pub getrlimit_failed: bool,
    pub getrlimit_errno: i32,
    /// Errno that stopped the dup loop (0 = stopped by limit/max_to_probe;
    /// EMFILE/ENFILE are the expected stops and not warned about).
    pub stop_errno: i32,
}

/// Outcome of the boot-time RLIMIT_NOFILE raise ([`raise_fd_soft_limit_to_hard`]).
/// `soft_after` is the descriptor ceiling the whole server process has to
/// live inside — in the one-process/thread-per-session model that is the
/// budget every session shares, which is why fd's budget arithmetic needs it.
#[derive(Clone, Copy, Debug)]
pub struct FdSoftLimitRaise {
    pub soft_before: u64,
    /// Soft limit in force after the raise (== `soft_before` when no raise
    /// was needed or none was accepted).
    pub soft_after: u64,
    /// Hard limit; `u64::MAX` when the platform reports RLIM_INFINITY.
    pub hard: u64,
    /// getrlimit(RLIMIT_NOFILE) failed — the process ceiling is unknown and
    /// callers must fall back to the probed budget.
    pub getrlimit_failed: bool,
    pub getrlimit_errno: i32,
    /// Errno of the last rejected setrlimit (0 = no raise attempted, or the
    /// raise succeeded).
    pub setrlimit_errno: i32,
    /// Per-process cap the KERNEL enforces regardless of RLIMIT_NOFILE
    /// (`kern.maxfilesperproc`, `fs.nr_open`); `None` if unreadable. macOS
    /// accepts a soft limit far above it and then fails `open()` at it, so
    /// budget arithmetic must take the smaller of this and `soft_after`.
    pub kernel_per_process_cap: Option<u64>,
}

// ---------------------------------------------------------------------------
// The Vfs trait — the contract surface (signatures binding, names normative).
// FROZEN at tag vfs-trait-v1; any op-set or signature change after that tag is
// a contract revision, not a lane decision.
//
// Contract v2 (op-set revision, tag `vfs-trait-v2`, ruled 2026-09-25 for
// pgrcolumnar parts by reference, WR): `link` — link(2), a second name for a
// file. Both PosixVfs and SimVfs implement it; SimVfs gives the new dirent
// the same durability law as every namespace op (volatile until its parent
// directory is fsynced) and a crash keeps each durable name (nlink = the
// number of durable names). Additive: no existing op changed.
//
// v1 op-set note (folded in at freeze time): `rmdir` was added to the contract
// draft's list — fd's fenced clusters remove directories (RemovePgTempFiles,
// rmtree, unlink_if_exists_fname) and the namespace plane must be closed under
// the ops fd actually performs.
// ---------------------------------------------------------------------------

pub trait Vfs {
    // -- open/close (vfd.rs chokepoint) --
    /// PG_O_DIRECT accepted; -1/errno. Single-shot: the EMFILE-retry loop
    /// stays in fd's LruOpen path.
    fn open(&self, path: &CStr, flags: c_int, mode: mode_t) -> c_int;
    fn close(&self, fd: c_int) -> c_int;

    // -- data plane (io.rs; plus the SLRU transient-fd plane) --
    /// Caller contract: `iov` base pointers are valid for writes of their
    /// lengths (IoSliceMut-shaped).
    fn preadv(&self, fd: c_int, iov: &[libc::iovec], off: off_t) -> isize;
    fn pwritev(&self, fd: c_int, iov: &[libc::iovec], off: off_t) -> isize;
    fn pread(&self, fd: c_int, buf: &mut [u8], off: off_t) -> isize; // slru/xlogrecovery/basebackup
    fn pwrite(&self, fd: c_int, buf: &[u8], off: off_t) -> isize; // slru + transam_xlog write.rs

    // -- durability --
    /// Plain fsync(2), single-shot. The pg_fsync POLICY (enableFsync gate,
    /// wal_sync_method writethrough dispatch, debug asserts) stays in fd; the
    /// macOS F_FULLFSYNC arm is PosixVfs's [`posix::PosixVfs::fsync_writethrough`]
    /// (free-fn shim: [`fsync_writethrough`]).
    fn fsync(&self, fd: c_int) -> c_int;
    fn fdatasync(&self, fd: c_int) -> c_int;
    /// pg_flush_data hint; MAY no-op. Posix: sync_file_range on Linux, no-op
    /// elsewhere (the non-Linux mmap/msync arm stays cfg'd in fd — carve-out).
    fn flush_range(&self, fd: c_int, off: off_t, len: off_t) -> c_int;

    // -- size / space --
    fn ftruncate(&self, fd: c_int, len: off_t) -> c_int;
    fn truncate_path(&self, path: &CStr, len: off_t) -> c_int;
    /// posix_fallocate convention: 0 on success, positive errno on failure
    /// (EINTR possible; caller retries). Non-Linux returns EOPNOTSUPP and the
    /// caller zero-extends (fd's FileZero).
    fn fallocate(&self, fd: c_int, off: off_t, len: off_t) -> c_int;
    /// lseek(SEEK_END); -1/errno on failure.
    fn file_size(&self, fd: c_int) -> off_t;
    /// Advisory readahead hint; MAY no-op. Posix: posix_fadvise(WILLNEED) on
    /// Linux (returns positive errno on failure, EINTR possible), F_RDADVISE
    /// on macOS (-1/errno on failure).
    fn fadvise_willneed(&self, fd: c_int, off: off_t, len: off_t) -> c_int;
    /// Random-access hint over the whole file; MAY no-op. Posix: posix_fadvise(RANDOM)
    /// on Linux (no kernel read-ahead on this fd — exact preads stay exact), F_RDAHEAD 0
    /// on macOS.
    fn fadvise_random(&self, fd: c_int) -> c_int;
    /// Page-cache residency of `[off, off+len)` (a query: changes nothing): `out` = [pages in the page cache, pages the
    /// range spans]; 0 on success, -1/errno on failure. Posix: cachestat(2) on Linux (ENOSYS before 6.5: the caller
    /// takes the residency as unknown), EOPNOTSUPP elsewhere. Sim: its files live in memory, every page is cached.
    fn cachestat(&self, fd: c_int, off: off_t, len: off_t, out: &mut [u64; 2]) -> c_int;

    // -- namespace --
    fn stat(&self, path: &CStr, out: &mut FileInfo) -> c_int;
    fn fstat(&self, fd: c_int, out: &mut FileInfo) -> c_int;
    fn lstat(&self, path: &CStr, out: &mut FileInfo) -> c_int; // copydir symlink_metadata
    /// readlink(2) shape: bytes written (no NUL), or -1/errno.
    fn read_link(&self, path: &CStr, buf: &mut [u8]) -> isize;
    fn unlink(&self, path: &CStr) -> c_int;
    /// Atomic; durable_rename composes ABOVE in fd.
    fn rename(&self, from: &CStr, to: &CStr) -> c_int;
    /// Contract v2: link(2) — `to` becomes a second name of the file at
    /// `from`. -1/errno: EEXIST if `to` exists, EPERM for a directory, EXDEV
    /// across file systems (Posix), ENOENT.
    fn link(&self, from: &CStr, to: &CStr) -> c_int;
    fn mkdir(&self, path: &CStr, mode: mode_t) -> c_int;
    fn rmdir(&self, path: &CStr) -> c_int;
    /// SimVfs: deterministic (BTree) order. Never yields "." or "..".
    fn read_dir(&self, path: &CStr) -> VfsResult<VfsDirIter>;

    // -- process/budget probe (vfd.rs dup(2) + getrlimit) --
    /// SimVfs: fixed pinned budget, no real fds touched.
    fn fd_budget_probe(&self, max_to_probe: usize) -> usize;
}

// ---------------------------------------------------------------------------
// Dispatch — cfg-selected monomorphization (contract §1.2). Zero-cost by
// construction: ActiveVfs is a ZST and every shim is #[inline].
// ---------------------------------------------------------------------------

#[cfg(not(pgrust_sim))]
pub type ActiveVfs = posix::PosixVfs;

// --- append region (WS-C): sim module + cfg hook ---
#[cfg(pgrust_sim)]
pub mod sim;
#[cfg(pgrust_sim)]
pub mod sim_boot;
#[cfg(pgrust_sim)]
pub type ActiveVfs = sim::SimVfs;
// --- end append region (WS-C) ---

/// The one active instance. Both PosixVfs and SimVfs are ZSTs with
/// `pub const fn new()`; sim state lives in thread-locals inside SimVfs.
pub const ACTIVE: ActiveVfs = ActiveVfs::new();

// Conformance assert: both impls must satisfy the trait even though dispatch
// is static (contract §1.2).
const _: () = {
    fn assert_conforms<T: Vfs>() {}
    #[allow(dead_code)]
    fn _both() {
        assert_conforms::<posix::PosixVfs>();
        #[cfg(pgrust_sim)]
        assert_conforms::<sim::SimVfs>();
    }
};

// ---------------------------------------------------------------------------
// Free-function shims — what fd (and controldata_utils) actually call.
// ---------------------------------------------------------------------------

macro_rules! shims {
    ($($(#[$doc:meta])* fn $name:ident($($arg:ident: $ty:ty),*) -> $ret:ty;)+) => {
        $(
            $(#[$doc])*
            #[inline]
            pub fn $name($($arg: $ty),*) -> $ret {
                ACTIVE.$name($($arg),*)
            }
        )+
    };
}

shims! {
    fn open(path: &CStr, flags: c_int, mode: mode_t) -> c_int;
    fn close(fd: c_int) -> c_int;
    fn preadv(fd: c_int, iov: &[libc::iovec], off: off_t) -> isize;
    fn pwritev(fd: c_int, iov: &[libc::iovec], off: off_t) -> isize;
    fn pread(fd: c_int, buf: &mut [u8], off: off_t) -> isize;
    fn pwrite(fd: c_int, buf: &[u8], off: off_t) -> isize;
    fn fsync(fd: c_int) -> c_int;
    fn fdatasync(fd: c_int) -> c_int;
    fn flush_range(fd: c_int, off: off_t, len: off_t) -> c_int;
    fn ftruncate(fd: c_int, len: off_t) -> c_int;
    fn truncate_path(path: &CStr, len: off_t) -> c_int;
    fn fallocate(fd: c_int, off: off_t, len: off_t) -> c_int;
    fn file_size(fd: c_int) -> off_t;
    fn fadvise_willneed(fd: c_int, off: off_t, len: off_t) -> c_int;
    fn fadvise_random(fd: c_int) -> c_int;
    fn cachestat(fd: c_int, off: off_t, len: off_t, out: &mut [u64; 2]) -> c_int;
    fn stat(path: &CStr, out: &mut FileInfo) -> c_int;
    fn fstat(fd: c_int, out: &mut FileInfo) -> c_int;
    fn lstat(path: &CStr, out: &mut FileInfo) -> c_int;
    fn read_link(path: &CStr, buf: &mut [u8]) -> isize;
    fn unlink(path: &CStr) -> c_int;
    fn rename(from: &CStr, to: &CStr) -> c_int;
    fn link(from: &CStr, to: &CStr) -> c_int;
    fn mkdir(path: &CStr, mode: mode_t) -> c_int;
    fn rmdir(path: &CStr) -> c_int;
    fn read_dir(path: &CStr) -> VfsResult<VfsDirIter>;
    fn fd_budget_probe(max_to_probe: usize) -> usize;
}

// ---------------------------------------------------------------------------
// The part files' share of the one-process descriptor budget (D2873), sized
// by demand. Additive, next to the contract: fd records the budget it divides
// (the arithmetic of the split stays fd's), the part reader's pool records how
// many part files it holds, and both already depend on this crate. The parts
// take what they hold plus a floor, at most a cap; each session's file cache
// gets an equal share of the rest. A server with few or no part files keeps
// the per-session share it had before parts were counted.
// ---------------------------------------------------------------------------

/// What `fd::set_max_safe_fds` divides between the sessions' file caches and the
/// part-file pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FdBudget {
    /// The descriptors left for caches: the ceiling less every child's setup
    /// descriptors and the postmaster's reserve.
    pub for_caches: i64,
    /// The sessions that may each fill a file cache.
    pub sessions: i64,
    /// C's per-session answer, min(usable fds, max_files_per_process) less the
    /// reserve: the share never exceeds it.
    pub c_bound: i64,
    /// The descriptors each session keeps outside its cache (NUM_RESERVED_FDS).
    pub reserved: i64,
    /// The fewest and the most descriptors the part pool takes.
    pub part_floor: usize,
    pub part_cap: usize,
}

/// The part pool's slice when it holds `demand` part files: those plus the
/// floor, at most the cap.
pub fn part_fd_slice_of_demand(demand: usize, floor: usize, cap: usize) -> usize {
    demand.saturating_add(floor).min(cap)
}

/// Each session's file-cache limit (max_safe_fds) when the part pool takes
/// `slice`: an equal share of what the parts leave, less the reserve, never
/// above C's answer, never below 1.
pub fn session_fd_limit_of(b: &FdBudget, slice: usize) -> i64 {
    let share = (b.for_caches - slice as i64) / b.sessions.max(1) - b.reserved;
    b.c_bound.min(share).max(1)
}

mod fd_budget {
    use core::sync::atomic::{AtomicI64, AtomicUsize};
    pub(super) static FOR_CACHES: AtomicI64 = AtomicI64::new(0);
    pub(super) static SESSIONS: AtomicI64 = AtomicI64::new(1);
    pub(super) static C_BOUND: AtomicI64 = AtomicI64::new(0);
    pub(super) static RESERVED: AtomicI64 = AtomicI64::new(0);
    pub(super) static PART_FLOOR: AtomicUsize = AtomicUsize::new(0);
    /// 0 = no budget recorded (no RLIMIT_NOFILE known: the sim world, a
    /// library harness): no part slice, no session limit.
    pub(super) static PART_CAP: AtomicUsize = AtomicUsize::new(0);
    pub(super) static PART_DEMAND: AtomicUsize = AtomicUsize::new(0);
    /// 0 = none.
    pub(super) static SESSION_LIMIT: AtomicI64 = AtomicI64::new(0);
}

fn fd_budget_now() -> FdBudget {
    use core::sync::atomic::Ordering::Relaxed;
    use fd_budget::*;
    FdBudget {
        for_caches: FOR_CACHES.load(Relaxed),
        sessions: SESSIONS.load(Relaxed),
        c_bound: C_BOUND.load(Relaxed),
        reserved: RESERVED.load(Relaxed),
        part_floor: PART_FLOOR.load(Relaxed),
        part_cap: PART_CAP.load(Relaxed),
    }
}

fn recompute_session_fd_limit() {
    let b = fd_budget_now();
    if b.part_cap == 0 {
        return;
    }
    let limit = session_fd_limit_of(&b, part_fd_slice());
    fd_budget::SESSION_LIMIT.store(limit, core::sync::atomic::Ordering::Relaxed);
}

/// Record the budget `fd::set_max_safe_fds` divides (at boot, before any part
/// is opened).
pub fn set_fd_budget(b: FdBudget) {
    use core::sync::atomic::Ordering::Relaxed;
    use fd_budget::*;
    FOR_CACHES.store(b.for_caches, Relaxed);
    SESSIONS.store(b.sessions.max(1), Relaxed);
    C_BOUND.store(b.c_bound, Relaxed);
    RESERVED.store(b.reserved, Relaxed);
    PART_FLOOR.store(b.part_floor, Relaxed);
    PART_CAP.store(b.part_cap, Relaxed);
    recompute_session_fd_limit();
}

/// The part files the pool holds now (its caller serializes the calls: the
/// pool's lock).
pub fn set_part_fd_demand(n: usize) {
    fd_budget::PART_DEMAND.store(n, core::sync::atomic::Ordering::Relaxed);
    recompute_session_fd_limit();
}

/// The descriptors the process-wide part-file pool may hold open now: the part
/// files it holds plus the floor, at most the cap. 0 = no budget recorded.
pub fn part_fd_slice() -> usize {
    use core::sync::atomic::Ordering::Relaxed;
    let cap = fd_budget::PART_CAP.load(Relaxed);
    if cap == 0 {
        return 0;
    }
    part_fd_slice_of_demand(fd_budget::PART_DEMAND.load(Relaxed), fd_budget::PART_FLOOR.load(Relaxed), cap)
}

/// Each session's file-cache limit under the part pool's current demand (fd
/// takes the lower of it and the session's own max_safe_fds). 0 = none.
pub fn session_fd_limit() -> i64 {
    fd_budget::SESSION_LIMIT.load(core::sync::atomic::Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// VfsFd — the owned handle for vfs-minted descriptors (DST P4 finding F1b).
// Additive helper NEXT TO the contract, not a trait revision: the Vfs op-set
// and signatures are untouched.
// ---------------------------------------------------------------------------

/// Owned handle to a **vfs-minted** file descriptor.
///
/// Deliberate close paths call [`VfsFd::into_raw`] (which disarms the guard)
/// and then [`close`], exactly as they did with `OwnedFd::into_raw_fd`. The
/// Drop arm exists for the paths that never reach a deliberate close —
/// unwinds and thread-exit TLS teardown — and routes the release through the
/// SAME provider that minted the fd ([`close_on_drop`]). With plain `OwnedFd`
/// those paths closed posix-side: under `--cfg pgrust_sim` the fd is foreign
/// to the kernel, so the close EBADF'd, the sim fd leaked in the sim table,
/// and std's debug IO-safety check aborted the whole process (P4 fault
/// injection unwinds through exactly these states).
///
/// Zero-cost off-sim by construction: `repr(transparent)` over
/// `ManuallyDrop<OwnedFd>` keeps the size AND the niche (`Option<VfsFd>` is
/// fd-sized, like `Option<OwnedFd>` — const-asserted below); `as_raw_fd` /
/// `into_raw` are field reads; and the Drop arm monomorphizes to the same
/// single `close(2)` call `OwnedFd`'s own Drop makes (minus std's debug-only
/// already-closed probe).
#[repr(transparent)]
pub struct VfsFd(core::mem::ManuallyDrop<std::os::fd::OwnedFd>);

// The zero-cost law, machine-checked: same size and same niche as the
// OwnedFd this type replaces inside Option<>-shaped holders.
const _: () = {
    assert!(
        core::mem::size_of::<Option<VfsFd>>()
            == core::mem::size_of::<Option<std::os::fd::OwnedFd>>()
    );
    assert!(core::mem::size_of::<Option<VfsFd>>() == core::mem::size_of::<c_int>());
};

impl VfsFd {
    /// Take ownership of `raw`.
    ///
    /// Thread affinity: drop (or deliberately close) the guard on the thread
    /// that minted `raw`. Under `--cfg pgrust_sim` each thread is its own vfs
    /// universe (thread-local fd table), so a guard moved cross-thread would
    /// release into the DESTINATION thread's sim table — EBADF there, silent
    /// leak in the minting universe. All current holders are FdState-TLS-bound
    /// so this cannot happen today; keep it that way.
    ///
    /// # Safety
    /// `raw` must be a live descriptor minted by the ACTIVE vfs ([`open`]),
    /// exclusively owned by the returned guard (`OwnedFd::from_raw_fd` rules,
    /// with "open file description" read against the active vfs domain).
    #[inline]
    pub unsafe fn from_raw(raw: RawFd) -> VfsFd {
        // SAFETY: caller contract above.
        VfsFd(core::mem::ManuallyDrop::new(unsafe {
            std::os::fd::OwnedFd::from_raw_fd(raw)
        }))
    }

    /// Disarm the guard and hand the descriptor back to the caller, who must
    /// [`close`] it — the deliberate-close path. No close happens here.
    #[inline]
    pub fn into_raw(self) -> RawFd {
        let raw = self.0.as_raw_fd();
        core::mem::forget(self);
        raw
    }
}

impl std::os::fd::AsRawFd for VfsFd {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl Drop for VfsFd {
    #[inline]
    fn drop(&mut self) {
        // Leak/unwind path only: deliberate closes disarm via into_raw().
        // The result is deliberately ignored — Drop has nobody to report to,
        // exactly like OwnedFd's own Drop.
        let _ = close_on_drop(self.0.as_raw_fd());
    }
}

/// The [`VfsFd`] drop arm. Posix: exactly `close(2)` — the identical single
/// call `OwnedFd`'s Drop makes. Sim: [`sim::SimVfs::close_on_drop`] — the
/// same close as [`close`], but tolerant of the sim thread-local already
/// being destroyed (guard drops legally run inside thread-exit TLS
/// destructors, and TLS destructor order is unspecified).
#[cfg(not(pgrust_sim))]
#[inline]
fn close_on_drop(fd: c_int) -> c_int {
    ACTIVE.close(fd)
}
#[cfg(pgrust_sim)]
#[inline]
fn close_on_drop(fd: c_int) -> c_int {
    sim::SimVfs::close_on_drop(fd)
}

/// The macOS F_FULLFSYNC arm of pg_fsync (wal_sync_method=fsync_writethrough).
/// Platform split lives inside PosixVfs; under sim a writethrough is simply a
/// (strictly weaker than requested, strictly correct) sim fsync.
#[cfg(not(pgrust_sim))]
#[inline]
pub fn fsync_writethrough(fd: c_int) -> c_int {
    ACTIVE.fsync_writethrough(fd)
}
#[cfg(pgrust_sim)]
#[inline]
pub fn fsync_writethrough(fd: c_int) -> c_int {
    ACTIVE.fsync(fd)
}

/// Instrumented fd-budget probe: same loop as [`Vfs::fd_budget_probe`], with
/// the diagnostics fd's `count_usable_fds` reports (already_open, getrlimit
/// and dup failure warnings). Sim arm synthesizes a clean report from the
/// pinned budget (already_open = 3: stdin/stdout/stderr).
#[cfg(not(pgrust_sim))]
#[inline]
pub fn fd_budget_probe_report(max_to_probe: usize) -> FdBudgetProbe {
    ACTIVE.fd_budget_probe_report(max_to_probe)
}
#[cfg(pgrust_sim)]
pub fn fd_budget_probe_report(max_to_probe: usize) -> FdBudgetProbe {
    let used = ACTIVE.fd_budget_probe(max_to_probe) as i32;
    FdBudgetProbe {
        used,
        highest_fd: used + 2,
        getrlimit_failed: false,
        getrlimit_errno: 0,
        stop_errno: 0,
    }
}

/// Boot-time RLIMIT_NOFILE raise (soft → hard). Sim arm: the simulated
/// process has a pinned budget and no rlimits, reported as "no getrlimit" so
/// fd keeps its probe-only arithmetic (deterministic across replays).
#[cfg(not(pgrust_sim))]
#[inline]
pub fn raise_fd_soft_limit_to_hard() -> FdSoftLimitRaise {
    ACTIVE.raise_fd_soft_limit_to_hard()
}
#[cfg(pgrust_sim)]
pub fn raise_fd_soft_limit_to_hard() -> FdSoftLimitRaise {
    FdSoftLimitRaise {
        soft_before: 0,
        soft_after: 0,
        hard: 0,
        getrlimit_failed: true,
        getrlimit_errno: libc::ENOSYS,
        setrlimit_errno: 0,
        kernel_per_process_cap: None,
    }
}

#[cfg(test)]
mod tests;
