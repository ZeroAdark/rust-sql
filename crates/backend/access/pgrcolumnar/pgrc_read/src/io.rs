//! The reader's I/O seam: positioned reads over part files and whole-file
//! reads over table-directory files, all through the `vfs` choke (the
//! sanctioned file surface — SimVfs-composable for M3-K's crash batteries,
//! zero determinism-ledger rows).
//!
//! [`PartIo`] exists so the open path, fault accounting, and every test can
//! run over injected in-memory parts ([`MemPartIo`]) with controlled
//! identity facts; [`VfsPartIo`] is the production implementation.

use core::ffi::CStr;
use std::ffi::CString;

use pgrc_format::FormatError;

use crate::{ReadError, ReadResult};

/// Positioned-read access to one (immutable, sealed) part file plus the
/// physical identity facts spec §11 keys on. Implementations are read-only
/// and internally immutable; `&self` reads are safe from any thread.
pub trait PartIo: Send + Sync {
    /// File length in bytes (identity word 3).
    fn len(&self) -> u64;
    /// `(st_dev, st_ino)` (identity words 1–2). In-memory implementations
    /// return synthetic facts — tests use them to model copies and reuse.
    fn dev_ino(&self) -> (u64, u64);
    /// Fill `buf` from `off`, exactly. Short data (EOF inside the range) is
    /// a typed `Truncated` at `at`; an I/O failure is `ReadError::Io`.
    fn pread_exact(&self, off: u64, buf: &mut [u8], at: &'static str) -> ReadResult<()>;
    /// Advisory readahead hint over `[off, off+len)` — MAY no-op, never
    /// fails, never changes what a later read returns (the cold-readahead
    /// claim-hook face; C3 inherit). Default no-op keeps every in-memory
    /// implementation honest by construction.
    fn advise_willneed(&self, _off: u64, _len: u64) {}
    /// Whole-file access-pattern hint, called ONCE by the open path before any
    /// content read — MAY no-op, never fails, never changes what a read returns.
    /// The production form asks for no kernel read-ahead on the fd
    /// (`POSIX_FADV_RANDOM`): the reader's preads are exact extents, and the
    /// 128 KiB read-ahead window past each one is over-read.
    fn advise_random(&self) {}
    /// Page-cache residency of `[off, off+len)`: `(pages cached, pages)`; `None` when unknown (an in-memory image, a
    /// kernel without cachestat). A query, never a read: it changes nothing (M3's walk width, `extent_resident`).
    fn page_resident(&self, _off: u64, _len: u64) -> Option<(u64, u64)> {
        None
    }
    /// The file's modification time `(sec, nsec)` as the open's fstat saw it; `None` when unknown (an in-memory
    /// image). The registry compares it on a hit (`PartRegistry::open_pinned`): with the fd pool (D2873) an entry no
    /// longer holds its file open, so `(dev, ino, len)` alone could name a new file that reused a dropped part's inode.
    fn mtime(&self) -> Option<(i64, i64)> {
        None
    }
}

/// The RANDOM hint on every part fd is the default (box 1, 2026-09-14: the
/// kernel's 128 KiB read-ahead over-read 12–29 % of the cold bytes on
/// three grouping scans); `PGRUST_PGRC_FADV_RANDOM=0`/`off` is the kill switch.
pub(crate) fn fadv_random_armed() -> bool {
    static F: pgsync::OnceLock<bool> = pgsync::OnceLock::new();
    *F.get_or_init(|| !matches!(std::env::var("PGRUST_PGRC_FADV_RANDOM").as_deref(), Ok("0") | Ok("off")))
}

// ---------------------------------------------------------------------------
// production: vfs-backed positioned reads
// ---------------------------------------------------------------------------

/// Production [`PartIo`] over a part file, its descriptor a slot of the process-wide pool ([`crate::fdpool`], D2873):
/// opened here, closed by the pool's clock when idle past the budget, reopened on the next read. Identity facts are
/// gathered by ONE fstat at open, before any content read — the stat-before-read discipline: a concurrent replace
/// makes lookups fail spurious-miss, never stale-hit (spec §11); a reopen must find the same identity.
pub struct VfsPartIo {
    slot: std::sync::Arc<crate::fdpool::Slot>,
}

impl VfsPartIo {
    /// Open `path` read-only and gather identity facts.
    pub fn open(path: &CStr) -> ReadResult<VfsPartIo> {
        Ok(VfsPartIo { slot: crate::fdpool::open(path)? })
    }
}

impl Drop for VfsPartIo {
    fn drop(&mut self) {
        crate::fdpool::release(&self.slot);
    }
}

impl PartIo for VfsPartIo {
    fn len(&self) -> u64 {
        self.slot.len()
    }

    fn dev_ino(&self) -> (u64, u64) {
        self.slot.dev_ino()
    }

    fn mtime(&self) -> Option<(i64, i64)> {
        Some(self.slot.mtime())
    }

    fn advise_random(&self) {
        if fadv_random_armed() {
            if let Ok(l) = crate::fdpool::lease(&self.slot) {
                let _ = vfs::fadvise_random(l.fd);
            }
        }
    }

    fn page_resident(&self, off: u64, len: u64) -> Option<(u64, u64)> {
        let mut out = [0u64; 2];
        let ok = off <= libc::off_t::MAX as u64 && len <= libc::off_t::MAX as u64;
        if !ok {
            return None;
        }
        let l = crate::fdpool::lease(&self.slot).ok()?;
        (vfs::cachestat(l.fd, off as libc::off_t, len as libc::off_t, &mut out) == 0).then_some((out[0], out[1]))
    }

    fn advise_willneed(&self, off: u64, len: u64) {
        // Advisory through the vfs choke (posix_fadvise WILLNEED on Linux,
        // F_RDADVISE on macOS, SimVfs benign-gated). Errors are ignored —
        // a failed hint costs speed, never rows.
        if len == 0 || off > libc::off_t::MAX as u64 || len > libc::off_t::MAX as u64 {
            return;
        }
        if let Ok(l) = crate::fdpool::lease(&self.slot) {
            let _ = vfs::fadvise_willneed(l.fd, off as libc::off_t, len as libc::off_t);
        }
    }

    fn pread_exact(&self, off: u64, buf: &mut [u8], at: &'static str) -> ReadResult<()> {
        // [coldopen] Global demand-read census (cache-independent — counts
        // syscalls, not misses): the open/first-touch attribution currency.
        PREAD_CALLS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        PREAD_BYTES.fetch_add(buf.len() as u64, core::sync::atomic::Ordering::Relaxed);
        let depth = READ_DEPTH.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
        READ_DEPTH_MAX.fetch_max(depth, core::sync::atomic::Ordering::Relaxed);
        THREAD_READS.with(|t| {
            let [calls, bytes, depth, max] = t.get();
            t.set([calls + 1, bytes + buf.len() as u64, depth + 1, max.max(depth + 1)]);
        });
        let t0 = std::time::Instant::now();
        let r = self.pread_loop(off, buf, at);
        PREAD_NS.fetch_add(t0.elapsed().as_nanos() as u64, core::sync::atomic::Ordering::Relaxed);
        READ_DEPTH.fetch_sub(1, core::sync::atomic::Ordering::Relaxed);
        THREAD_READS.with(|t| {
            let [calls, bytes, depth, max] = t.get();
            t.set([calls, bytes, depth - 1, max]);
        });
        r
    }
}

impl VfsPartIo {
    fn pread_loop(&self, off: u64, buf: &mut [u8], at: &'static str) -> ReadResult<()> {
        let lease = crate::fdpool::lease(&self.slot)?;
        let mut done = 0usize;
        // bounded("other: the rest of one pread request after a short read or EINTR (a regular file returns it whole); the request's size is its caller's step: pread_polled reads a cancelpoll::PIECE at a time")
        while done < buf.len() {
            let n = vfs::pread(
                lease.fd,
                &mut buf[done..],
                (off + done as u64) as libc::off_t,
            );
            if n < 0 {
                let errno = vfs::get_errno();
                if errno == libc::EINTR {
                    continue;
                }
                return Err(ReadError::Io { at, errno });
            }
            if n == 0 {
                // EOF inside the requested range: the file is shorter than
                // the structure claims — truncation, typed.
                return Err(ReadError::Format(FormatError::Truncated { at }));
            }
            done += n as usize;
        }
        Ok(())
    }
}

/// [coldopen] Process-global positioned-read census over the production
/// VfsPartIo (instrumentation, never state): syscall count + requested
/// bytes. Cache-independent — the honest currency for attributing a cold
/// first-statement's demand reads (COLDFACE/open-probe consumers read the
/// deltas).
pub static PREAD_CALLS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static PREAD_BYTES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Snapshot the read census as (calls, bytes).
pub fn pread_census() -> (u64, u64) {
    (
        PREAD_CALLS.load(core::sync::atomic::Ordering::Relaxed),
        PREAD_BYTES.load(core::sync::atomic::Ordering::Relaxed),
    )
}

/// Nanoseconds spent inside preads, summed over every caller (÷ a statement's wall = its mean read depth).
pub static PREAD_NS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Part-file preads in flight process-wide, and their high-water mark since the last `take_read_depth_max`.
pub static READ_DEPTH: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static READ_DEPTH_MAX: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// The high-water mark of preads in flight since the previous call (the served line's `depth:`); resets it.
pub fn take_read_depth_max() -> u64 {
    READ_DEPTH_MAX.swap(READ_DEPTH.load(core::sync::atomic::Ordering::Relaxed), core::sync::atomic::Ordering::Relaxed)
}

std::thread_local! {
    /// This thread's share of the census: [calls, bytes, in flight, in-flight high-water]. The global census sums
    /// every thread's reads; a caller asking what ITS reads were (a test beside concurrently running tests, D769)
    /// reads this one. pgrc_read issues every pread on its caller's thread.
    static THREAD_READS: core::cell::Cell<[u64; 4]> = const { core::cell::Cell::new([0; 4]) };
}

/// This thread's read census as (calls, bytes): [`pread_census`] without any other thread's reads.
pub fn thread_pread_census() -> (u64, u64) {
    THREAD_READS.with(|t| {
        let [calls, bytes, _, _] = t.get();
        (calls, bytes)
    })
}

/// This thread's preads-in-flight high-water mark since its previous call; resets it ([`take_read_depth_max`]
/// for one thread).
pub fn take_thread_read_depth_max() -> u64 {
    THREAD_READS.with(|t| {
        let [calls, bytes, depth, max] = t.get();
        t.set([calls, bytes, depth, depth]);
        max
    })
}

// ---------------------------------------------------------------------------
// tests/tools: in-memory parts with controlled identity
// ---------------------------------------------------------------------------

/// In-memory [`PartIo`]: the byte image plus synthetic identity facts.
/// Two instances over the same bytes with equal `(dev, ino)` model the same
/// physical file; different `ino` models a copy (new identity, spec §11).
pub struct MemPartIo {
    bytes: std::sync::Arc<[u8]>,
    dev: u64,
    ino: u64,
}

impl MemPartIo {
    pub fn new(bytes: Vec<u8>, dev: u64, ino: u64) -> MemPartIo {
        MemPartIo {
            bytes: bytes.into(),
            dev,
            ino,
        }
    }

    /// Share the same byte image under the same identity (a second open of
    /// the same file).
    pub fn reopen(&self) -> MemPartIo {
        MemPartIo {
            bytes: self.bytes.clone(),
            dev: self.dev,
            ino: self.ino,
        }
    }
}

impl PartIo for MemPartIo {
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn dev_ino(&self) -> (u64, u64) {
        (self.dev, self.ino)
    }

    fn pread_exact(&self, off: u64, buf: &mut [u8], at: &'static str) -> ReadResult<()> {
        let off = off as usize;
        let end = off
            .checked_add(buf.len())
            .ok_or(ReadError::Format(FormatError::Bounds { at }))?;
        if end > self.bytes.len() {
            return Err(ReadError::Format(FormatError::Truncated { at }));
        }
        buf.copy_from_slice(&self.bytes[off..end]);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// table-directory reads (manifest walk, spec §12/§13)
// ---------------------------------------------------------------------------

/// Whole-file reads inside one table directory (`pgrc_<relfilenumber>/`).
/// Manifest generations and the commit pointer are small (spec §13); the
/// walk reads them whole. `Ok(None)` = the file does not exist (ENOENT) —
/// a first-class answer (absent `CURRENT` means "no committed publish"),
/// never an error.
pub trait TableDirIo {
    fn read_file(&self, name: &str) -> ReadResult<Option<Vec<u8>>>;
    /// The first `max` bytes of the file (fewer at its end); the manifest's
    /// section-0 readers ask for the front alone.
    fn read_prefix(&self, name: &str, max: usize) -> ReadResult<Option<Vec<u8>>> {
        Ok(self.read_file(name)?.map(|mut v| {
            v.truncate(max);
            v
        }))
    }
}

/// Production [`TableDirIo`] over a directory path, through vfs.
pub struct VfsTableDir {
    dir: String,
}

impl VfsTableDir {
    /// `dir` is the table directory path (no trailing slash).
    pub fn new(dir: String) -> VfsTableDir {
        VfsTableDir { dir }
    }

    fn join(&self, name: &str) -> ReadResult<CString> {
        CString::new(format!("{}/{}", self.dir, name)).map_err(|_| {
            ReadError::Format(FormatError::Corrupt {
                at: "table dir path NUL",
            })
        })
    }
}

impl TableDirIo for VfsTableDir {
    fn read_file(&self, name: &str) -> ReadResult<Option<Vec<u8>>> {
        self.read_up_to(name, usize::MAX)
    }

    fn read_prefix(&self, name: &str, max: usize) -> ReadResult<Option<Vec<u8>>> {
        self.read_up_to(name, max)
    }
}

impl VfsTableDir {
    fn read_up_to(&self, name: &str, max: usize) -> ReadResult<Option<Vec<u8>>> {
        let path = self.join(name)?;
        let fd = vfs::open(&path, libc::O_RDONLY | libc::O_CLOEXEC, 0);
        if fd < 0 {
            let errno = vfs::get_errno();
            if errno == libc::ENOENT {
                return Ok(None);
            }
            return Err(ReadError::Io {
                at: "dir file open",
                errno,
            });
        }
        let mut info = vfs::FileInfo::zeroed();
        if vfs::fstat(fd, &mut info) != 0 {
            let errno = vfs::get_errno();
            vfs::close(fd);
            return Err(ReadError::Io {
                at: "dir file fstat",
                errno,
            });
        }
        let len = (info.size.max(0) as usize).min(max);
        let mut buf = vec![0u8; len];
        let mut done = 0usize;
        // bounded("other: the rest of one directory file's pread after a short read or EINTR; a regular file's pread returns it whole, so the loop runs once but at a signal")
        while done < len {
            let n = vfs::pread(fd, &mut buf[done..], done as libc::off_t);
            if n < 0 {
                let errno = vfs::get_errno();
                if errno == libc::EINTR {
                    continue;
                }
                vfs::close(fd);
                return Err(ReadError::Io {
                    at: "dir file read",
                    errno,
                });
            }
            if n == 0 {
                // Shrank under us; sealed dir files never shrink — refuse.
                vfs::close(fd);
                return Err(ReadError::Format(FormatError::Truncated {
                    at: "dir file",
                }));
            }
            done += n as usize;
        }
        vfs::close(fd);
        Ok(Some(buf))
    }
}

/// In-memory [`TableDirIo`] for tests: a name → bytes map.
#[derive(Default)]
pub struct MemTableDir {
    pub files: std::collections::BTreeMap<String, Vec<u8>>,
}

impl MemTableDir {
    pub fn new() -> MemTableDir {
        MemTableDir::default()
    }

    pub fn put(&mut self, name: &str, bytes: Vec<u8>) {
        self.files.insert(name.to_string(), bytes);
    }
}

impl TableDirIo for MemTableDir {
    fn read_file(&self, name: &str) -> ReadResult<Option<Vec<u8>>> {
        Ok(self.files.get(name).cloned())
    }
}
