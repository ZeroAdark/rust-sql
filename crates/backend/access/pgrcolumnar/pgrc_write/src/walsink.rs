//! The seam between the pure writer and WAL (design §7.3-7.4): [`WalSink`] and the logging vfs; the model is walmodel.

use std::collections::BTreeMap;

use pgrc_format::spool::MaskedCrc;
use pgrc_format::wal::{InstallRec, PublishRec, ReclaimRec, SmallMasks, FILE_WRITE_MAX_CHUNK, NO_MASKS};

use crate::wvfs::{WriteVfs, WvFd};
use crate::{WriteError, WriteResult};

pub type Apply<'a> = &'a mut dyn FnMut() -> WriteResult<()>;

/// SEALSIZE fix 3: the front seal's WAL pacing. A seal in a [`pace_scope`] logs its file's bytes at most at the
/// scope's rate: the logging sink calls [`paced`] after each `FILE_WRITE` chunk it inserts, and the scope's closure
/// sleeps (or decides not to). Box 22 (2026-10-04, bytes mode): a wide seal put its whole ~10-20 MB part into WAL in
/// about a second, and every commit behind it waited for that flush (wide/256 p99.9 +20 %).
pub mod pace {
    use std::cell::{Cell, RefCell};

    pub type Pacer = Box<dyn FnMut(u64)>;

    thread_local! {
        static PACER: RefCell<Option<Pacer>> = const { RefCell::new(None) };
        static ON: Cell<bool> = const { Cell::new(false) };
    }

    /// Paces this thread's logged file writes until dropped.
    #[must_use]
    pub struct Scope(());

    impl Drop for Scope {
        fn drop(&mut self) {
            ON.with(|o| o.set(false));
            PACER.with(|p| p.borrow_mut().take());
        }
    }

    pub fn scope(pacer: Pacer) -> Scope {
        PACER.with(|p| *p.borrow_mut() = Some(pacer));
        ON.with(|o| o.set(true));
        Scope(())
    }

    /// A scope is open on this thread (the logged vfs then writes in WAL-chunk pieces, each followed by its record).
    pub fn active() -> bool {
        ON.with(|o| o.get())
    }

    /// `bytes` more went into WAL: the scope's pacer may sleep.
    pub fn paced(bytes: u64) {
        let Some(mut f) = PACER.with(|p| p.borrow_mut().take()) else { return };
        f(bytes);
        PACER.with(|p| {
            let mut slot = p.borrow_mut();
            if slot.is_none() && ON.with(|o| o.get()) {
                *slot = Some(f);
            }
        });
    }

    /// A token bucket over a monotonic clock in ns: `rate` bytes a second after a `burst` of bytes.
    #[derive(Debug, Clone, Copy)]
    pub struct Bucket {
        rate: u64,
        burst: u64,
        start_ns: u64,
        sent: u64,
    }

    impl Bucket {
        pub fn new(rate: u64, burst: u64, now_ns: u64) -> Bucket {
            Bucket { rate: rate.max(1), burst, start_ns: now_ns, sent: 0 }
        }

        /// The ns to wait after `bytes` more at `now_ns` so the total stays within the rate.
        pub fn after(&mut self, bytes: u64, now_ns: u64) -> u64 {
            self.sent = self.sent.saturating_add(bytes);
            let over = self.sent.saturating_sub(self.burst);
            let due = u64::try_from(u128::from(over) * 1_000_000_000 / u128::from(self.rate)).unwrap_or(u64::MAX);
            due.saturating_sub(now_ns.saturating_sub(self.start_ns))
        }
    }
}

/// Makes a by-reference file durable through the vfs it is handed (WR).
pub type RefSync<'a> = &'a mut dyn FnMut(&mut dyn WriteVfs) -> WriteResult<()>;

/// The `Contract` detail of a sink whose WAL insert failed; the backend keeps the insert's own error.
pub const WAL_INSERT_FAILED: &str = "pgrc WAL insert failed";

/// Where one table directory's file writes and namespace changes are logged. `publish`, `install` and `reclaim` run
/// `apply` after inserting their record and before registering the directory's sync, all inside one
/// `DELAY_CHKPT_START` window, so the primary and redo run the same file operations.
pub trait WalSink {
    fn logs(&self) -> bool;
    /// `create`: the first write since the writer created the file; redo truncates first.
    fn file_write(&mut self, name: &str, off: u64, bytes: &[u8], create: bool) -> WriteResult<()>;
    /// The file is complete. If a checkpoint began after its first write, the records before its redo pointer will
    /// not be replayed, so `fsync` (the file, then its directory) runs now; otherwise the name is registered for
    /// the next checkpoint.
    fn file_done(&mut self, name: &str, fsync: &mut dyn FnMut() -> WriteResult<()>) -> WriteResult<()>;
    fn publish(&mut self, rec: &PublishRec, apply: Apply<'_>) -> WriteResult<()>;
    fn install(&mut self, rec: &InstallRec, apply: Apply<'_>) -> WriteResult<()>;
    fn reclaim(&mut self, rec: &ReclaimRec, apply: Apply<'_>) -> WriteResult<()>;
    /// Register a name changed without a record (end-of-recovery cleanup) for the next checkpoint. D4711: a sync
    /// request that could not be queued is the caller's error.
    fn register(&mut self, name: &str) -> WriteResult<()>;
    /// Parts by reference (WR): files at least this long are logged by `file_ref` instead of their bytes. `None`
    /// (the default) logs every byte: no WAL, bytes mode, a spool on another file system.
    fn ref_min(&self) -> Option<u64> {
        None
    }
    /// A complete file goes by reference: run `fsync` over `vfs` (the file, then its directory) so the bytes are
    /// durable before the record names them, then log `FILE_REF` and keep the file readable for WAL consumers
    /// (the spool, linked through `vfs`). Called once per file, when it is closed.
    fn file_ref(&mut self, _f: &FileRef<'_>, _vfs: &mut dyn WriteVfs, _fsync: RefSync<'_>) -> WriteResult<()> {
        Err(WriteError::Contract { detail: "this sink never logs by reference" })
    }
}

/// `inner` with every file logged by its bytes (`ref_min` is `None`): the front seal at `wal_level=minimal`, where a
/// part past the threshold would otherwise cost an inline fsync per seal (user 2026-10-04).
pub struct BytesOnly(pub Box<dyn WalSink + Send>);

impl WalSink for BytesOnly {
    fn logs(&self) -> bool {
        self.0.logs()
    }
    fn file_write(&mut self, name: &str, off: u64, bytes: &[u8], create: bool) -> WriteResult<()> {
        self.0.file_write(name, off, bytes, create)
    }
    fn file_done(&mut self, name: &str, fsync: &mut dyn FnMut() -> WriteResult<()>) -> WriteResult<()> {
        self.0.file_done(name, fsync)
    }
    fn publish(&mut self, rec: &PublishRec, apply: Apply<'_>) -> WriteResult<()> {
        self.0.publish(rec, apply)
    }
    fn install(&mut self, rec: &InstallRec, apply: Apply<'_>) -> WriteResult<()> {
        self.0.install(rec, apply)
    }
    fn reclaim(&mut self, rec: &ReclaimRec, apply: Apply<'_>) -> WriteResult<()> {
        self.0.reclaim(rec, apply)
    }
    fn register(&mut self, name: &str) -> WriteResult<()> {
        self.0.register(name)
    }
}

/// A complete file of the table directory `dir`, named by reference: `len` bytes whose crc32c, with `masks`
/// skipped, is `crc` (`pgrc_format::spool::MaskedCrc`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileRef<'a> {
    pub dir: &'a str,
    pub name: &'a str,
    pub len: u64,
    pub crc: u32,
    pub masks: SmallMasks,
}

/// No WAL and no sync: temp tables, `wal_level=minimal` in the creating transaction (the commit fsyncs the
/// directory), and tables no server will recover (tests, tools).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoWal;

impl WalSink for NoWal {
    fn logs(&self) -> bool {
        false
    }
    fn file_write(&mut self, _: &str, _: u64, _: &[u8], _: bool) -> WriteResult<()> {
        Ok(())
    }
    fn file_done(&mut self, _: &str, _: &mut dyn FnMut() -> WriteResult<()>) -> WriteResult<()> {
        Ok(())
    }
    fn publish(&mut self, _: &PublishRec, apply: Apply<'_>) -> WriteResult<()> {
        apply()
    }
    fn install(&mut self, _: &InstallRec, apply: Apply<'_>) -> WriteResult<()> {
        apply()
    }
    fn reclaim(&mut self, _: &ReclaimRec, apply: Apply<'_>) -> WriteResult<()> {
        apply()
    }
    fn register(&mut self, _: &str) -> WriteResult<()> {
        Ok(())
    }
}

/// How an open file's bytes are logged. Without a `ref_min` every file is `Bytes` from its first write.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Mode {
    /// Sequential writes, all below `ref_min` so far: the chunks wait in `buf`.
    Pending,
    /// Logged chunk by chunk, as W0 does.
    #[default]
    Bytes,
    /// Past `ref_min` with sequential writes: logged by `file_ref` at close.
    Ref,
    /// Logged by reference; the file takes no more writes.
    Referenced,
}

#[derive(Debug, Default)]
struct OpenFile {
    create: bool,
    dirty: bool,
    mode: Mode,
    /// `Pending`: each write's `(offset, bytes)`, logged as W0 would have if the file ends below `ref_min`.
    buf: Vec<(u64, Vec<u8>)>,
    buffered: u64,
    /// Bytes written so far (`Pending`, `Ref`): the next sequential offset.
    len: u64,
    masks: SmallMasks,
    crc: Option<MaskedCrc>,
}

impl OpenFile {
    fn new(ref_min: Option<u64>) -> OpenFile {
        match ref_min {
            Some(_) => OpenFile { create: true, mode: Mode::Pending, crc: Some(MaskedCrc::new(NO_MASKS)), ..OpenFile::default() },
            None => OpenFile { create: true, ..OpenFile::default() },
        }
    }
}

/// W0's logging of one write: chunks of at most `FILE_WRITE_MAX_CHUNK`, the first after the create flagged.
fn log_chunks(sink: &mut dyn WalSink, name: &str, off: u64, bytes: &[u8], create: &mut bool) -> WriteResult<()> {
    let mut at = 0usize;
    loop {
        let end = (at + FILE_WRITE_MAX_CHUNK).min(bytes.len());
        sink.file_write(name, off + at as u64, &bytes[at..end], *create)?;
        *create = false;
        at = end;
        if at >= bytes.len() {
            return Ok(());
        }
    }
}

/// Log a `Pending` file's buffered writes as W0 would have logged them; it continues as `Bytes`.
fn flush_pending(sink: &mut dyn WalSink, name: &str, f: &mut OpenFile) -> WriteResult<()> {
    for (off, bytes) in std::mem::take(&mut f.buf) {
        log_chunks(sink, name, off, &bytes, &mut f.create)?;
    }
    f.buffered = 0;
    f.crc = None;
    f.mode = Mode::Bytes;
    Ok(())
}

/// A [`WriteVfs`] over one table directory that logs every byte it writes. Files are created and written here;
/// renames, unlinks, re-opens and mkdirs are refused, because every namespace change goes through a
/// [`WalSink`] record whose apply closure uses the inner vfs. `fsync_file` and `close_file` become
/// [`WalSink::file_done`]; `fsync_dir` is the sink's registration.
///
/// With a sink `ref_min` (WR), a new file's writes are held back while they are sequential and the file is shorter:
/// a file that ends below it logs exactly W0's records at close, one that passes it with sequential writes goes by
/// [`WalSink::file_ref`] and its buffered chunks are dropped, and a write anywhere else logs what was held and
/// continues as W0.
pub struct LoggedVfs<'a> {
    inner: &'a mut dyn WriteVfs,
    sink: &'a mut dyn WalSink,
    dir: String,
    open: BTreeMap<String, OpenFile>,
    ref_min: Option<u64>,
    /// K1 L6: the bytes written through it (a publish's metadata files).
    written: u64,
}

impl<'a> LoggedVfs<'a> {
    pub fn new(inner: &'a mut dyn WriteVfs, sink: &'a mut dyn WalSink, dir: &str) -> LoggedVfs<'a> {
        let ref_min = if sink.logs() { sink.ref_min() } else { None };
        LoggedVfs { inner, sink, dir: dir.to_string(), open: BTreeMap::new(), ref_min, written: 0 }
    }

    /// The bytes written through this vfs so far (logged as `FILE_WRITE` when the sink logs).
    pub fn written(&self) -> u64 {
        self.written
    }

    pub fn into_parts(self) -> (&'a mut dyn WriteVfs, &'a mut dyn WalSink) {
        (self.inner, self.sink)
    }

    fn name_of<'p>(&self, path: &'p str) -> WriteResult<&'p str> {
        match path.strip_prefix(self.dir.as_str()).and_then(|r| r.strip_prefix('/')) {
            Some(name) if !name.is_empty() && !name.contains('/') => Ok(name),
            _ => Err(WriteError::Contract { detail: "logged write outside the table directory" }),
        }
    }

    fn done(&mut self, fd: &WvFd) -> WriteResult<()> {
        let name = self.name_of(fd.path())?.to_string();
        let Some(f) = self.open.get_mut(&name) else { return Ok(()) };
        if !f.create && !f.dirty {
            return Ok(());
        }
        let dir = &self.dir;
        if f.mode == Mode::Ref {
            let r = FileRef { dir, name: &name, len: f.len, crc: f.crc.take().map_or(0, |c| c.finish()), masks: f.masks };
            f.mode = Mode::Referenced;
            f.create = false;
            f.dirty = false;
            return self.sink.file_ref(&r, &mut *self.inner, &mut |v: &mut dyn WriteVfs| {
                v.fsync_file(fd)?;
                v.fsync_dir(dir)
            });
        }
        let inner = &mut *self.inner;
        let mut fsync = || {
            inner.fsync_file(fd)?;
            inner.fsync_dir(dir)
        };
        if f.mode == Mode::Pending {
            flush_pending(&mut *self.sink, &name, f)?;
        }
        if f.create {
            self.sink.file_write(&name, 0, &[], true)?;
        }
        f.create = false;
        f.dirty = false;
        self.sink.file_done(&name, &mut fsync)
    }
}

fn refused<T>() -> WriteResult<T> {
    Err(WriteError::Contract { detail: "unlogged namespace change through the logged vfs" })
}

impl WriteVfs for LoggedVfs<'_> {
    fn create_rw(&mut self, path: &str) -> WriteResult<WvFd> {
        let name = self.name_of(path)?.to_string();
        let fd = self.inner.create_rw(path)?;
        self.open.insert(name, OpenFile::new(self.ref_min));
        Ok(fd)
    }

    fn open_rw(&mut self, _: &str) -> WriteResult<WvFd> {
        refused()
    }

    fn pwrite_at(&mut self, fd: &WvFd, off: u64, bytes: &[u8]) -> WriteResult<()> {
        let name = self.name_of(fd.path())?;
        let Some(f) = self.open.get_mut(name) else {
            return Err(WriteError::Contract { detail: "logged write to a file the logged vfs did not create" });
        };
        // SEALSIZE fix 3: under a pace scope a large write goes in WAL-chunk pieces, each file write followed by its
        // record (and the record by the pacer's wait), so the file's writes are paced with its WAL, not ahead of it
        if pace::active() && self.sink.logs() && bytes.len() > FILE_WRITE_MAX_CHUNK && f.mode == Mode::Bytes {
            let mut at = 0usize;
            while at < bytes.len() {
                let end = (at + FILE_WRITE_MAX_CHUNK).min(bytes.len());
                self.pwrite_at(fd, off + at as u64, &bytes[at..end])?;
                at = end;
            }
            return Ok(());
        }
        self.inner.pwrite_at(fd, off, bytes)?;
        self.written += bytes.len() as u64;
        if !self.sink.logs() {
            f.create = false;
            f.dirty = true;
            return Ok(());
        }
        f.dirty = true;
        let sequential = off == f.len;
        match f.mode {
            Mode::Pending if sequential => {
                f.crc.as_mut().expect("a pending file keeps its crc").update(off, bytes);
                f.len += bytes.len() as u64;
                if f.buffered + bytes.len() as u64 >= self.ref_min.expect("pending needs ref_min") {
                    f.buf = Vec::new();
                    f.buffered = 0;
                    f.mode = Mode::Ref;
                } else {
                    f.buf.push((off, pgrc_format::try_to_vec(bytes, "walsink")?));
                    f.buffered += bytes.len() as u64;
                }
                Ok(())
            }
            Mode::Pending => {
                flush_pending(&mut *self.sink, name, f)?;
                log_chunks(&mut *self.sink, name, off, bytes, &mut f.create)
            }
            Mode::Bytes => log_chunks(&mut *self.sink, name, off, bytes, &mut f.create),
            Mode::Ref if sequential => {
                f.crc.as_mut().expect("a file going by reference keeps its crc").update(off, bytes);
                f.len += bytes.len() as u64;
                Ok(())
            }
            Mode::Ref => Err(WriteError::Contract { detail: "a file going by reference takes sequential writes only" }),
            Mode::Referenced => Err(WriteError::Contract { detail: "a write to a file already logged by reference" }),
        }
    }

    fn note_ref_masks(&mut self, fd: &WvFd, masks: SmallMasks) -> WriteResult<()> {
        let name = self.name_of(fd.path())?;
        let Some(f) = self.open.get_mut(name) else {
            return Err(WriteError::Contract { detail: "ref masks for a file the logged vfs did not create" });
        };
        match f.mode {
            Mode::Pending if f.len == 0 => {
                f.masks = masks;
                f.crc = Some(MaskedCrc::new(masks));
                Ok(())
            }
            // K1-MEM: a streamed seal notes its footer's mask once it knows the offset, before writing there.
            Mode::Pending | Mode::Ref => {
                let crc = f.crc.as_mut().expect("a pending or referenced file keeps its crc");
                crc.widen_masks(masks).map_err(|detail| WriteError::Contract { detail })?;
                f.masks = masks;
                Ok(())
            }
            Mode::Bytes | Mode::Referenced => Ok(()),
        }
    }

    fn fsync_file(&mut self, fd: &WvFd) -> WriteResult<()> {
        self.done(fd)
    }

    fn close_file(&mut self, fd: WvFd) -> WriteResult<()> {
        self.done(&fd)?;
        let name = self.name_of(fd.path())?.to_string();
        self.open.remove(&name);
        self.inner.close_file(fd)
    }

    fn read_full(&mut self, path: &str) -> WriteResult<Vec<u8>> {
        self.inner.read_full(path)
    }

    fn read_prefix(&mut self, path: &str, max: usize) -> WriteResult<Vec<u8>> {
        self.inner.read_prefix(path, max)
    }

    fn read_at(&mut self, path: &str, off: u64, len: usize) -> WriteResult<Vec<u8>> {
        self.inner.read_at(path, off, len)
    }

    fn rename_path(&mut self, _: &str, _: &str) -> WriteResult<()> {
        refused()
    }

    fn unlink_path(&mut self, _: &str) -> WriteResult<()> {
        refused()
    }

    fn fsync_dir(&mut self, _: &str) -> WriteResult<()> {
        Ok(())
    }

    fn list_dir(&mut self, dir: &str) -> WriteResult<Vec<String>> {
        self.inner.list_dir(dir)
    }

    fn mkdir_path(&mut self, _: &str) -> WriteResult<()> {
        refused()
    }

    fn exists_path(&mut self, path: &str) -> WriteResult<bool> {
        self.inner.exists_path(path)
    }
}

/// One file a parallel-COPY worker wrote: `(offset, bytes, create)` chunks in write order, or (WR) the reference
/// of a file the worker already fsynced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Forwarded {
    pub name: String,
    pub chunks: Vec<(u64, Vec<u8>, bool)>,
    pub by_ref: Option<ForwardedRef>,
}

/// A worker's file that goes by reference: durable already, so the leader only logs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForwardedRef {
    pub len: u64,
    pub crc: u32,
    pub masks: SmallMasks,
}

/// A parallel-COPY worker's sink. rtpool workers have no WAL insert state, so their chunks are kept for the leader,
/// which logs each part as it finishes ([`log_forwarded`], decision W0-3). With the leader's `ref_min` (WR), a
/// large file is fsynced in the worker and only its reference is forwarded.
#[derive(Debug, Default)]
pub struct ForwardSink {
    pub files: Vec<Forwarded>,
    ref_min: Option<u64>,
}

impl ForwardSink {
    pub fn with_ref_min(ref_min: Option<u64>) -> ForwardSink {
        ForwardSink { files: Vec::new(), ref_min }
    }
}

impl WalSink for ForwardSink {
    fn logs(&self) -> bool {
        true
    }
    fn ref_min(&self) -> Option<u64> {
        self.ref_min
    }
    fn file_ref(&mut self, f: &FileRef<'_>, vfs: &mut dyn WriteVfs, fsync: RefSync<'_>) -> WriteResult<()> {
        fsync(vfs)?;
        let by_ref = Some(ForwardedRef { len: f.len, crc: f.crc, masks: f.masks });
        self.files.push(Forwarded { name: f.name.to_string(), chunks: Vec::new(), by_ref });
        Ok(())
    }
    fn file_write(&mut self, name: &str, off: u64, bytes: &[u8], create: bool) -> WriteResult<()> {
        let bytes = pgrc_format::try_to_vec(bytes, "walsink")?;
        if self.files.last().is_none_or(|f| f.name != name) {
            self.files.push(Forwarded { name: name.to_string(), chunks: Vec::new(), by_ref: None });
        }
        self.files.last_mut().expect("pushed").chunks.push((off, bytes, create));
        Ok(())
    }
    fn file_done(&mut self, _: &str, _: &mut dyn FnMut() -> WriteResult<()>) -> WriteResult<()> {
        Ok(())
    }
    fn publish(&mut self, _: &PublishRec, _: Apply<'_>) -> WriteResult<()> {
        Err(WriteError::Contract { detail: "a parallel worker never publishes" })
    }
    fn install(&mut self, _: &InstallRec, _: Apply<'_>) -> WriteResult<()> {
        Err(WriteError::Contract { detail: "a parallel worker never publishes" })
    }
    fn reclaim(&mut self, _: &ReclaimRec, _: Apply<'_>) -> WriteResult<()> {
        Err(WriteError::Contract { detail: "a parallel worker never publishes" })
    }
    fn register(&mut self, _: &str) -> WriteResult<()> {
        Ok(())
    }
}

/// The leader's half of [`ForwardSink`]: log a worker's file and finish it, fsyncing through `vfs` when a
/// checkpoint began since the first logged chunk. A reference is logged as is: the worker fsynced the file.
pub fn log_forwarded(sink: &mut dyn WalSink, vfs: &mut dyn WriteVfs, dir: &str, f: &Forwarded) -> WriteResult<()> {
    if let Some(r) = f.by_ref {
        let fr = FileRef { dir, name: &f.name, len: r.len, crc: r.crc, masks: r.masks };
        return sink.file_ref(&fr, vfs, &mut |_: &mut dyn WriteVfs| Ok(()));
    }
    for (off, bytes, create) in &f.chunks {
        sink.file_write(&f.name, *off, bytes, *create)?;
    }
    let path = format!("{dir}/{}", f.name);
    sink.file_done(&f.name, &mut || {
        let fd = vfs.open_rw(&path)?;
        vfs.fsync_file(&fd)?;
        vfs.close_file(fd)?;
        vfs.fsync_dir(dir)
    })
}
