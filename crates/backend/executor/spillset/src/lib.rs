//! M3.5 spill substrate (design authority: docs/design/m3.5-spill.md §2).
//!
//! One [`SpillSet`] per spill-eligible engagement, owned by the engagement
//! payload: a query-owned fd `FileSet` whose Drop deletes every file on
//! every exit path (success, refusal, cancel, error). Inside it, per-worker
//! SINGLE-WRITER spill files with a partition directory:
//!
//! - A [`SpillFile`] is plain data between flush events (`Send` — it rides
//!   a sink `Local` and moves through SEAL). It holds NO open handle: file
//!   handles are VFD-thread-local (fd/src/vfd.rs — the design doc's §6.2
//!   hazard), so every I/O burst opens, works, and closes within one event
//!   on one thread.
//! - Writers append whole EPOCHS (one budget-crossing flush = one epoch):
//!   partition-contiguous segments recorded as per-partition extents. The
//!   file is complete on disk at the end of every epoch (BufFile close
//!   flushes); an epoch abandoned mid-write is simply never committed to
//!   the directory, and the next epoch reopens at the committed length,
//!   overwriting the tail.
//! - Readers (combine tasks, other threads) open the frozen file BY NAME
//!   via `BufFileOpenFileSet` on their own thread's VFD cache and stream
//!   one partition's extents. Frozen-before-read is the caller's deps-DAG
//!   obligation (a reader task set depends on the writer task set).
//! - Every syscall-bearing region brackets
//!   [`runtime::blocking_io_section`] (design §6.1): armed permit donation
//!   on pool workers, no-op on pinned-regime helpers and the leader.
//!
//! Record CONTENTS are operator-owned byte contracts (agg (key, state)
//! rows, DistinctSet value records, STS batch tuples, sort runs); this
//! crate moves bytes and never interprets them.

use std::sync::Arc;

use ::fd::buffile::{BufFile, BufFileCreateFileSet, BufFileOpenFileSet, SEEK_SET};
use ::fd::fileset::FileSet;
use ::mcx::Mcx;
use ::types_error::PgResult;

const MAX_PHYSICAL_FILESIZE: i64 = 0x4000_0000; // buffile.rs segment size

/// D491 (Gravity's serial owners past the grant pool): `map(fd)` over a temp file of `bytes`, made as a BufFile's
/// (`PrepareTempTablespaces`, `OpenTemporaryFile`: the session's temp tablespace, deleted at close and at abort). Its last
/// byte is written first, so the whole length is charged against `temp_file_limit` (53400 past it, FileWrite's check, as a
/// PostgreSQL spill of those bytes would be), then its blocks are allocated (`FileFallocate`: a mapped page written back
/// never meets a full disk). `map` maps the descriptor; the file is closed after it, which unlinks it: a mapping keeps it.
pub fn with_map_file<T>(bytes: u64, map: impl FnOnce(i32) -> T) -> PgResult<T> {
    let _io = runtime::blocking_io_section();
    ::fd::PrepareTempTablespaces()?;
    let file = ::fd::OpenTemporaryFile(false)?;
    let io_error = |what: &str, state| {
        let e = std::io::Error::from_raw_os_error(::fd::get_errno());
        Box::new(::types_error::PgError::error(format!("could not {what} temporary file of {bytes} bytes for a Gravity owner region: {e}")).with_sqlstate(state))
    };
    let laid = (|| -> PgResult<i32> {
        if bytes == 0 {
            return Err(io_error("size", ::types_error::ERRCODE_IO_ERROR));
        }
        if ::fd::FileWrite(file, &[0u8], bytes as i64 - 1, ::fd::wait_event::WAIT_EVENT_BUFFILE_WRITE)? != 1 {
            return Err(io_error("extend", ::types_error::ERRCODE_IO_ERROR));
        }
        if ::fd::FileFallocate(file, 0, bytes as i64, ::fd::wait_event::WAIT_EVENT_BUFFILE_WRITE)? != 0 {
            let state = if ::fd::get_errno() == 28 { ::types_error::ERRCODE_DISK_FULL } else { ::types_error::ERRCODE_IO_ERROR }; // 28: ENOSPC
            return Err(io_error("allocate", state));
        }
        match ::fd::FileGetRawDesc(file)? {
            raw if raw >= 0 => Ok(raw),
            _ => Err(io_error("open", ::types_error::ERRCODE_IO_ERROR)),
        }
    })();
    let mapped = laid.map(map);
    // the close deletes the file (a temporary file's FD_DELETE_AT_CLOSE); a mapping of it keeps its blocks until unmapped
    let closed = ::fd::FileClose(file);
    let t = mapped?;
    closed?;
    Ok(t)
}

/// Query-owned set of spill files for ONE engagement. Arc'd into the
/// engagement payload; Drop (via the inner `FileSet`) deletes everything.
pub struct SpillSet {
    fileset: FileSet,
}

impl SpillSet {
    /// Create the engagement's spill set. Caller must be a thread with
    /// temp-file access up (leader at admission time, or a bound helper).
    pub fn create() -> PgResult<Arc<SpillSet>> {
        // D2570: a spill file counts against temp_file_limit until it is deleted, closed between epochs too
        Ok(Arc::new(SpillSet { fileset: FileSet::init_charged_for_life()? }))
    }

    /// Canonical spill-file name: unique per (purpose, generation, worker)
    /// within this set. Purpose is operator-chosen ("agg", "dst", "run",
    /// join batches use STS with its own naming).
    pub fn file_name(purpose: &str, generation: u64, worker: usize) -> String {
        format!("m35-{purpose}-g{generation}-w{worker}")
    }

    fn fileset(&self) -> &FileSet {
        &self.fileset
    }

    /// pgrust-only (gravity D3203): the set's files unlinked now, the unlinks run by `fan(n, job)` once they hold
    /// `min_bytes` (fd `FileSet::delete_files_with`: every other effect of a delete stays on this thread), each job in a
    /// blocking-I/O section on the thread that runs it; the set's drop then removes the empty directories.
    /// -> (files, their bytes, whether `fan` ran).
    pub fn delete_files_with(&self, min_bytes: u64, fan: &dyn Fn(usize, &(dyn Fn(usize) + Sync))) -> PgResult<(usize, u64, bool)> {
        self.fileset.delete_files_with(min_bytes, &|n: usize, job: &(dyn Fn(usize) + Sync)| {
            fan(n, &|i: usize| {
                let _io = runtime::blocking_io_section();
                job(i)
            })
        })
    }
}

/// One partition extent: `len` bytes at logical BufFile offset `offset`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Extent {
    pub offset: u64,
    pub len: u64,
}

/// A single-writer spill file with a partition directory. Plain data
/// between flush events; owned by exactly one sink `Local` (single-toucher
/// by the sink contract) and immutable ("frozen") once its owner stops
/// writing — the caller's deps DAG separates the last write from the first
/// read.
pub struct SpillFile {
    set: Arc<SpillSet>,
    name: String,
    nparts: u32,
    /// Committed logical length (== next epoch's start offset).
    len: u64,
    created: bool,
    epochs: usize,
    /// Per partition, its committed extents in epoch order: a reader of every
    /// partition walks each extent once, not every epoch's directory per
    /// partition (an owner over 2^11 partitions of a many-epoch file).
    by_part: Vec<Vec<Extent>>,
}

/// The heap bytes of a list of `cap` capacity whose elements are `list`'s.
fn cap_bytes<T>(_list: &[T], cap: usize) -> usize {
    cap * core::mem::size_of::<T>()
}

impl SpillFile {
    /// Lazy: no file exists until the first epoch is written.
    pub fn new(set: Arc<SpillSet>, name: String, nparts: u32) -> SpillFile {
        assert!(nparts > 0);
        SpillFile { set, name, nparts, len: 0, created: false, epochs: 0, by_part: Vec::new() }
    }

    fn extents_of(&self, part: u32) -> &[Extent] {
        self.by_part.get(part as usize).map_or(&[], |v| v.as_slice())
    }

    pub fn nparts(&self) -> u32 {
        self.nparts
    }

    /// Committed bytes on disk (spill accounting / gate-record counters).
    pub fn spilled_bytes(&self) -> u64 {
        self.len
    }

    pub fn epochs(&self) -> usize {
        self.epochs
    }

    /// The directory's heap bytes: the per-partition extent lists' capacity (`EpochWriter::finish` grows them), so a
    /// caller can charge them to the file's owner (CONVERSION WAVE 2: the gravity pool's spill file).
    pub fn dir_bytes(&self) -> u64 {
        let outer = cap_bytes(&self.by_part, self.by_part.capacity());
        let inner: usize = self.by_part.iter().map(|v| cap_bytes(v, v.capacity())).sum();
        (outer + inner) as u64
    }

    /// Total committed bytes of one partition across all epochs.
    pub fn part_len(&self, part: u32) -> u64 {
        debug_assert!(part < self.nparts);
        self.extents_of(part).iter().map(|x| x.len).sum()
    }

    /// One partition's committed extents, epoch order (M3.5 join batches:
    /// each extent is one morsel-claim unit — extents are RECORD-aligned by
    /// construction, every epoch writes whole records).
    pub fn part_extents(&self, part: u32) -> PgResult<Vec<Extent>> {
        debug_assert!(part < self.nparts);
        ::mcx::stdvec::try_to_vec(self.extents_of(part), "spillset")
    }

    /// Stream ONE extent (from [`SpillFile::part_extents`]) of the frozen
    /// file. Any thread with temp-file access; same deps-DAG obligation as
    /// [`SpillFile::read_part`].
    pub fn read_extent<'mcx>(&self, mcx: Mcx<'mcx>, extent: Extent) -> PgResult<PartReader<'mcx>> {
        debug_assert!(self.created, "read_extent on a never-written file");
        let _io = runtime::blocking_io_section();
        let bf = BufFileOpenFileSet(mcx, self.set.fileset(), &self.name, true)?;
        drop(_io);
        Ok(PartReader { bf, extents: vec![extent], cur: 0, pos_in_cur: 0, seeked: false })
    }

    /// Begin one epoch write. MUST be called by the file's owning worker
    /// thread (single-writer law). The returned writer holds an open
    /// BufFile; every syscall-bearing call (open here, `write_part`,
    /// `finish`) enters its own declared blocking section. The epoch
    /// commits only at [`EpochWriter::finish`].
    pub fn begin_epoch<'a, 'mcx>(&'a mut self, mcx: Mcx<'mcx>) -> PgResult<EpochWriter<'a, 'mcx>> {
        let _io = runtime::blocking_io_section();
        let bf = if self.created {
            let mut bf = BufFileOpenFileSet(mcx, self.set.fileset(), &self.name, false)?;
            // Seek to the committed logical length (segmented offsets).
            let fileno = (self.len as i64) / MAX_PHYSICAL_FILESIZE;
            let off = (self.len as i64) % MAX_PHYSICAL_FILESIZE;
            let r = bf.seek(fileno as i32, off, SEEK_SET)?;
            debug_assert_eq!(r, 0, "seek to committed EOF cannot fail");
            bf
        } else {
            BufFileCreateFileSet(mcx, self.set.fileset(), &self.name)?
        };
        self.created = true;
        // Drop the section while the writer stages in-memory state; each
        // write_part / finish enters its own.
        drop(_io);
        Ok(EpochWriter { file: self, bf: Some(bf), cursor_valid: true, staged: Vec::new(), written: 0 })
    }

    /// Unlink the file's segments (gravity D1327: a sort merge pass's runs once the next pass has read them, the way
    /// PG's logtape reuses a spent tape's blocks). The caller's deps DAG closes every reader first.
    pub fn delete(self) -> PgResult<()> {
        if !self.created {
            return Ok(());
        }
        let _io = runtime::blocking_io_section();
        ::fd::buffile::BufFileDeleteFileSet(self.set.fileset(), &self.name, true)
    }

    /// Stream one partition's committed bytes. Any thread with temp-file
    /// access; the file must be frozen (caller's deps-DAG obligation).
    /// Returns None if the partition has no bytes (or the file was never
    /// created).
    pub fn read_part<'mcx>(&self, mcx: Mcx<'mcx>, part: u32) -> PgResult<Option<PartReader<'mcx>>> {
        debug_assert!(part < self.nparts);
        if self.extents_of(part).is_empty() || !self.created {
            return Ok(None);
        }
        let extents = ::mcx::stdvec::try_to_vec(self.extents_of(part), "spillset")?;
        let _io = runtime::blocking_io_section();
        let bf = BufFileOpenFileSet(mcx, self.set.fileset(), &self.name, true)?;
        drop(_io);
        Ok(Some(PartReader { bf, extents, cur: 0, pos_in_cur: 0, seeked: false }))
    }

    /// [`SpillFile::read_part`] from byte `offset` of the partition on (gravity D3190: a put sort run's fence): the
    /// extents walked to it, the BufFile seeked on the first read. None when no byte lies past `offset`.
    pub fn read_part_at<'mcx>(&self, mcx: Mcx<'mcx>, part: u32, offset: u64) -> PgResult<Option<PartReader<'mcx>>> {
        let Some(mut r) = self.read_part(mcx, part)? else { return Ok(None) };
        if r.position(offset) {
            return Ok(Some(r));
        }
        r.close()?;
        Ok(None)
    }
}

/// In-flight epoch write: partitions strictly ascending, one contiguous
/// extent per partition. Byte layout is the caller's contract; this type
/// records where each partition's bytes landed.
pub struct EpochWriter<'a, 'mcx> {
    file: &'a mut SpillFile,
    /// `finish` takes it; a writer dropped without finishing closes it unwritten (`Drop`).
    bf: Option<BufFile<'mcx>>,
    cursor_valid: bool,
    staged: Vec<(u32, Extent)>,
    written: u64,
}

impl EpochWriter<'_, '_> {
    /// Logical file offset the next byte will land at (fence indexes for
    /// sort runs read this).
    pub fn offset(&self) -> u64 {
        self.file.len + self.written
    }

    /// Append bytes to `part`. Parts must be written in ascending order;
    /// consecutive calls for the SAME part extend its extent.
    pub fn write_part(&mut self, part: u32, bytes: &[u8]) -> PgResult<()> {
        debug_assert!(part < self.file.nparts);
        debug_assert!(self.cursor_valid);
        if bytes.is_empty() {
            return Ok(());
        }
        self.stage(part, self.file.len + self.written, bytes.len() as u64);
        let _io = runtime::blocking_io_section();
        // spillopt #3: a long span goes to the file without the 8 KB buffer's copy (BufFile::write_spans)
        self.bf.as_mut().expect("an EpochWriter holds its BufFile until finish").write_spans(&[bytes])?;
        self.written += bytes.len() as u64;
        Ok(())
    }

    /// spillopt #3: `write_part` for each `(part, bytes)` in order (parts ascending, a part repeated extends its
    /// extent), the bytes handed to the file in gathered writes (`BufFile::write_spans`): an epoch's blocks reach the
    /// file in a few large writes instead of a copy and a syscall per 8 KB.
    pub fn write_parts(&mut self, parts: &[(u32, &[u8])]) -> PgResult<()> {
        debug_assert!(self.cursor_valid);
        let mut spans: Vec<&[u8]> = ::mcx::stdvec::try_vec(parts.len(), "lib")?; // alloc: Exec
        let mut at = self.file.len + self.written;
        for &(part, bytes) in parts {
            debug_assert!(part < self.file.nparts);
            if bytes.is_empty() {
                continue;
            }
            self.stage(part, at, bytes.len() as u64);
            at += bytes.len() as u64;
            spans.push(bytes);
        }
        let _io = runtime::blocking_io_section();
        self.bf.as_mut().expect("an EpochWriter holds its BufFile until finish").write_spans(&spans)?;
        self.written = at - self.file.len;
        Ok(())
    }

    /// The extent bookkeeping of `len` bytes for `part` landing at logical offset `at` (the epoch's end so far).
    fn stage(&mut self, part: u32, at: u64, len: u64) {
        match self.staged.last_mut() {
            Some((p, x)) if *p == part => {
                debug_assert_eq!(x.offset + x.len, at, "extents are contiguous");
                x.len += len;
            }
            last => {
                assert!(last.map_or(true, |(p, _)| *p < part), "partitions must be written in ascending order");
                self.staged.push((part, Extent { offset: at, len }));
            }
        }
    }

    /// Flush + close + commit the epoch into the directory. Not calling
    /// this (drop/unwind) abandons the epoch: the file closes, nothing is
    /// committed and the next epoch overwrites the tail.
    pub fn finish(mut self) -> PgResult<()> {
        let bf = self.bf.take().expect("an EpochWriter holds its BufFile until finish");
        {
            let _io = runtime::blocking_io_section();
            bf.close()?;
        }
        if self.written > 0 {
            let file = &mut *self.file;
            if file.by_part.is_empty() {
                ::mcx::stdvec::try_reserve_more(&mut file.by_part, file.nparts as usize, "spillset")?;
                file.by_part.extend(core::iter::repeat_with(Vec::new).take(file.nparts as usize));
            }
            for &(part, x) in &self.staged {
                file.by_part[part as usize].push(x);
            }
            file.epochs += 1;
            file.len += self.written;
        }
        Ok(())
    }
}

impl Drop for EpochWriter<'_, '_> {
    // D2570: a writer dropped without `finish` (a write_part that failed, e.g. past temp_file_limit on a pool worker)
    // closes its file unwritten. Nothing else would: the BufFile has no Drop, and a pool thread's resource owner
    // never releases, so the descriptor stayed open on the thread, holding the file's disk after its delete.
    fn drop(&mut self) {
        if let Some(bf) = self.bf.take() {
            let _io = runtime::blocking_io_section();
            let _ = bf.abandon();
        }
    }
}

/// Streaming reader of one partition (its extents across all epochs, in
/// epoch order). Owns its thread-local BufFile handle; close it on the
/// reading thread. Callers pass large buffers (each `read` call is one
/// declared blocking section).
pub struct PartReader<'mcx> {
    bf: BufFile<'mcx>,
    extents: Vec<Extent>,
    cur: usize,
    pos_in_cur: u64,
    seeked: bool,
}

impl PartReader<'_> {
    pub fn total_len(&self) -> u64 {
        self.extents.iter().map(|x| x.len).sum()
    }

    /// The reader moved to byte `offset` of its partition (the next read seeks there); false when no byte lies past it.
    fn position(&mut self, offset: u64) -> bool {
        let mut left = offset;
        for (i, x) in self.extents.iter().enumerate() {
            if left < x.len {
                (self.cur, self.pos_in_cur, self.seeked) = (i, left, false);
                return true;
            }
            left -= x.len;
        }
        false
    }

    /// Read up to `buf.len()` bytes; 0 = end of partition.
    pub fn read(&mut self, buf: &mut [u8]) -> PgResult<usize> {
        self.read_each(buf, &mut || true).map(|(n, _)| n)
    }

    /// D4215: `read`, with `each` asked before every piece past the first (a piece is one extent's bytes: one pread, or a
    /// few 8 KB ones under `THROUGH`), all inside the one blocking section. False stops the read there. Returns the bytes
    /// read and whether `each` stopped it. A caller polls in `each`: the extents lie apart, so each is its own device wait.
    pub fn read_each(&mut self, buf: &mut [u8], each: &mut dyn FnMut() -> bool) -> PgResult<(usize, bool)> {
        let mut filled = 0usize;
        let _io = runtime::blocking_io_section();
        while filled < buf.len() {
            let Some(x) = self.extents.get(self.cur) else { break };
            if self.pos_in_cur == x.len {
                self.cur += 1;
                self.pos_in_cur = 0;
                self.seeked = false;
                continue;
            }
            if filled != 0 && !each() {
                return Ok((filled, true));
            }
            if !self.seeked {
                let at = (x.offset + self.pos_in_cur) as i64;
                let r = self.bf.seek(
                    (at / MAX_PHYSICAL_FILESIZE) as i32,
                    at % MAX_PHYSICAL_FILESIZE,
                    SEEK_SET,
                )?;
                debug_assert_eq!(r, 0, "extent lies within the committed file");
                self.seeked = true;
            }
            let want = ((x.len - self.pos_in_cur) as usize).min(buf.len() - filled);
            // spillopt #3: straight into the caller's buffer (BufFile::read_through_exact)
            self.bf.read_through_exact(&mut buf[filled..filled + want])?;
            filled += want;
            self.pos_in_cur += want as u64;
        }
        Ok((filled, false))
    }

    /// Read the whole remaining partition (tests / small partitions).
    pub fn read_to_end(&mut self) -> PgResult<Vec<u8>> {
        let mut out = Vec::new();
        let mut chunk = vec![0u8; 256 * 1024];
        loop {
            let n = self.read(&mut chunk)?;
            if n == 0 {
                return Ok(out);
            }
            out.extend_from_slice(&chunk[..n]);
        }
    }

    pub fn close(self) -> PgResult<()> {
        let _io = runtime::blocking_io_section();
        self.bf.close()
    }
}

#[cfg(test)]
mod tests;
