// Temp-file BufFile, serial and FileSet (participant-shared) arms. Arena data,
// no Drop: the VFDs are FD_CLOSE_AT_EOXACT+FD_DELETE_AT_CLOSE, so abort
// cleanup is AtEOXact_Files; close() is the normal-path release.
use core::cell::Cell;

use ::elog::{ereport, PCT_M};
use ::mcx::{vec_with_capacity_in, Mcx, PgVec};
use ::types_error::{PgResult, ERROR};
use ::types_storage::{File, WriteChunk};

use crate::fileset::FileSetKey;
use crate::io::{
    file_path_name_lossy, FileCloseGeneration, FileGeneration, FileRead, FileSize, FileTruncate, FileWrite, FileWriteV,
    PG_IOV_MAX,
};
use crate::temp::OpenTemporaryFile;
use crate::vfd::{get_errno, loc};
use crate::wait_event::{
    WAIT_EVENT_BUFFILE_READ, WAIT_EVENT_BUFFILE_TRUNCATE, WAIT_EVENT_BUFFILE_WRITE,
};

const BLCKSZ: usize = 8192;
const MAX_PHYSICAL_FILESIZE: i64 = 0x4000_0000;
const THROUGH: usize = 4 * BLCKSZ;

pub const SEEK_SET: i32 = 0;
pub const SEEK_CUR: i32 = 1;
pub const SEEK_END: i32 = 2;

// pgBufferUsage.temp_blks_* / temp_blk_*_time (instrument.c): BufFile is
// their only writer, so the running totals live here and the instrument
// crate snapshots them. The times are INSTR_TIME ticks (monotonic ns, the
// instrument crate's instr_time unit) and only advance under track_io_timing.
thread_local! {
    static TEMP_BLKS_READ: Cell<i64> = const { Cell::new(0) };
    static TEMP_BLKS_WRITTEN: Cell<i64> = const { Cell::new(0) };
    static TEMP_BLK_READ_TIME: Cell<i64> = const { Cell::new(0) };
    static TEMP_BLK_WRITE_TIME: Cell<i64> = const { Cell::new(0) };
}

pub fn temp_blks_read() -> i64 {
    TEMP_BLKS_READ.with(Cell::get)
}

pub fn temp_blks_written() -> i64 {
    TEMP_BLKS_WRITTEN.with(Cell::get)
}

/// `pgBufferUsage.temp_blk_read_time` ticks (ns).
pub fn temp_blk_read_time() -> i64 {
    TEMP_BLK_READ_TIME.with(Cell::get)
}

/// `pgBufferUsage.temp_blk_write_time` ticks (ns).
pub fn temp_blk_write_time() -> i64 {
    TEMP_BLK_WRITE_TIME.with(Cell::get)
}

/// The thread's temp-file counters, in this order: blocks read, blocks written, read time, write time.
pub type TempUsage = [i64; 4];

fn temp_cells() -> [&'static std::thread::LocalKey<Cell<i64>>; 4] {
    [&TEMP_BLKS_READ, &TEMP_BLKS_WRITTEN, &TEMP_BLK_READ_TIME, &TEMP_BLK_WRITE_TIME]
}

/// This thread's temp-file counters now.
pub fn temp_snapshot() -> TempUsage {
    let c = temp_cells();
    core::array::from_fn(|i| c[i].with(Cell::get))
}

/// D4116: this thread's temp-file counts since `then` (a `temp_snapshot`), taken out: its counters are back at `then`.
/// A Gravity pool claim hands its spill I/O to the section's leader this way (InstrAccumParallelQuery's role).
pub fn temp_take_since(then: &TempUsage) -> TempUsage {
    let c = temp_cells();
    core::array::from_fn(|i| {
        c[i].with(|x| {
            let d = x.get().wrapping_sub(then[i]);
            x.set(then[i]);
            d
        })
    })
}

/// Counts another thread took (`temp_take_since`) added to this thread's counters.
pub fn temp_add(d: &TempUsage) {
    let c = temp_cells();
    for i in 0..4 {
        c[i].with(|x| x.set(x.get().wrapping_add(d[i])));
    }
}

// INSTR_TIME_SET_CURRENT under track_io_timing, INSTR_TIME_SET_ZERO
// otherwise (buffile.c:459/532). The "zero = not timing" sentinel is C's.
#[inline]
fn io_timing_start() -> i64 {
    // bufmgr owns the GUC's backing; a boot without it (unit tests) is
    // C's track_io_timing = off.
    if guc_tables::vars::track_io_timing.installed() && guc_tables::vars::track_io_timing.read() {
        pg_clock::mono_ns() as i64
    } else {
        0
    }
}

// INSTR_TIME_ACCUM_DIFF(pgBufferUsage.temp_blk_*_time, now, io_start).
#[inline]
fn io_timing_accum(acc: &'static std::thread::LocalKey<Cell<i64>>, io_start: i64) {
    if io_start != 0 {
        let now = pg_clock::mono_ns() as i64;
        acc.with(|c| c.set(c.get() + (now - io_start)));
    }
}

pub struct BufFile<'mcx> {
    // All files except the last have length exactly MAX_PHYSICAL_FILESIZE.
    files: PgVec<'mcx, File>,
    gens: PgVec<'mcx, u64>,
    owner: Option<std::thread::ThreadId>,
    is_inter_xact: bool,
    resowner: types_resowner::ResourceOwner,
    dirty: bool,
    read_only: bool,
    // FileSet-backed files (C's fileset BufFiles): segment i is the set's
    // file "<name>.<i>", creatable and openable by any participant thread.
    // `fileset` is the C `buffile->fileset` identity, `name` the C
    // `buffile->name` (NULL for plain temp files).
    fileset: Option<FileSetKey>,
    name: Option<PgVec<'mcx, u8>>,
    cur_file: i32,
    cur_offset: i64,
    pos: i32,
    nbytes: i32,
    buffer: PgVec<'mcx, u8>,
}

// C PrepareTempTablespaces (commands/tablespace.c): the empty-GUC fast path
// stays local; a configured list needs catalog access, so it delegates to
// commands_tablespace.
pub fn PrepareTempTablespaces() -> PgResult<()> {
    if crate::temp::TempTablespacesAreSet() {
        return Ok(());
    }
    let spaces = guc_tables::vars::temp_tablespaces.read();
    if spaces.as_deref().unwrap_or("").is_empty() {
        crate::temp::SetTempTablespaces(&[])?;
        return Ok(());
    }
    tablespace_seams::prepare_temp_tablespaces::call()
}

pub fn BufFileCreateTemp<'mcx>(mcx: Mcx<'mcx>, inter_xact: bool) -> PgResult<BufFile<'mcx>> {
    PrepareTempTablespaces()?;
    let pfile = OpenTemporaryFile(inter_xact)?;
    debug_assert!(pfile.0 >= 0);
    let mut files = vec_with_capacity_in(mcx, 1)?;
    files.push(pfile);
    let mut gens = vec_with_capacity_in(mcx, 1)?;
    gens.push(FileGeneration(pfile));
    let mut buffer = vec_with_capacity_in(mcx, BLCKSZ)?;
    buffer.resize(BLCKSZ, 0);
    Ok(BufFile {
        files,
        gens,
        owner: crate::orphan::owner(),
        is_inter_xact: inter_xact,
        resowner: resowner_seams::current_resource_owner::call(),
        dirty: false,
        read_only: false,
        fileset: None,
        name: None,
        cur_file: 0,
        cur_offset: 0,
        pos: 0,
        nbytes: 0,
        buffer,
    })
}

// `FileSetSegmentName` (buffile.c:222): "<buffile_name>.<segment>".
fn seg_name(name: &[u8], segment: usize) -> String {
    format!("{}.{segment}", core::str::from_utf8(name).expect("fileset name is utf8"))
}

// `MakeNewFileSetSegment` (buffile.c:231): create segment `segment` of the
// named BufFile. Files left over from before a crash restart can carry the
// same name; so that BufFileOpenFileSet() is not confused about how many
// segments there are, unlink the NEXT segment number if it already exists.
fn make_new_fileset_segment(fileset: &FileSetKey, name: &[u8], segment: usize) -> PgResult<File> {
    fileset.delete(&seg_name(name, segment + 1), true)?;
    let file = fileset.create(&seg_name(name, segment))?;
    // FileSetCreate would've errored out.
    debug_assert!(file.0 > 0);
    Ok(file)
}

fn copy_name<'mcx>(mcx: Mcx<'mcx>, name: &str) -> PgResult<PgVec<'mcx, u8>> {
    let mut v = vec_with_capacity_in(mcx, name.len())?;
    v.extend(name.as_bytes().iter().copied());
    Ok(v)
}

/// `BufFileCreateFileSet` (buffile.c:268): a named, participant-shared temp
/// file.
pub fn BufFileCreateFileSet<'mcx>(
    mcx: Mcx<'mcx>,
    fileset: &crate::fileset::FileSet,
    name: &str,
) -> PgResult<BufFile<'mcx>> {
    let key = fileset.key();
    let file = make_new_fileset_segment(&key, name.as_bytes(), 0)?;
    let mut files = vec_with_capacity_in(mcx, 1)?;
    files.push(file);
    let mut gens = vec_with_capacity_in(mcx, 1)?;
    gens.push(FileGeneration(file));
    let name = copy_name(mcx, name)?;
    let mut buffer = vec_with_capacity_in(mcx, BLCKSZ)?;
    buffer.resize(BLCKSZ, 0);
    Ok(BufFile {
        files,
        gens,
        owner: crate::orphan::owner(),
        is_inter_xact: false,
        resowner: resowner_seams::current_resource_owner::call(),
        dirty: false,
        read_only: false,
        fileset: Some(key),
        name: Some(name),
        cur_file: 0,
        cur_offset: 0,
        pos: 0,
        nbytes: 0,
        buffer,
    })
}

// `BufFileOpenFileSet` (buffile.c:296): probe the filesystem for the
// segments; None only when missing_ok and no segment exists.
fn open_fileset_common<'mcx>(
    mcx: Mcx<'mcx>,
    fileset: &crate::fileset::FileSet,
    name: &str,
    read_only: bool,
    missing_ok: bool,
) -> PgResult<Option<BufFile<'mcx>>> {
    let key = fileset.key();
    let mode = if read_only { libc::O_RDONLY } else { libc::O_RDWR };
    let mut files: PgVec<'mcx, File> = vec_with_capacity_in(mcx, 16)?;
    let mut gens: PgVec<'mcx, u64> = vec_with_capacity_in(mcx, 16)?;
    let mut segment_name;
    loop {
        segment_name = seg_name(name.as_bytes(), files.len());
        let f = key.open(&segment_name, mode)?;
        if f.0 <= 0 {
            break;
        }
        files.push(f);
        gens.push(FileGeneration(f));
        // buffile.c:321: cancel point per 1GB segment (TB-scale spill sets
        // mean thousands of opens); crate idiom per copydir.rs.
        postgres_seams::check_for_interrupts::call()?;
    }
    // If we didn't find any files at all, then no BufFile exists with this
    // name.
    if files.is_empty() {
        if missing_ok {
            return Ok(None);
        }
        ereport(ERROR)
            .with_saved_errno(get_errno())
            .errcode_for_file_access()
            .errmsg(format!(
                "could not open temporary file \"{segment_name}\" from BufFile \"{name}\": {PCT_M}"
            ))
            .finish(loc("BufFileOpenFileSet"))?;
    }
    let name = copy_name(mcx, name)?;
    let mut buffer = vec_with_capacity_in(mcx, BLCKSZ)?;
    buffer.resize(BLCKSZ, 0);
    Ok(Some(BufFile {
        files,
        gens,
        owner: crate::orphan::owner(),
        is_inter_xact: false,
        resowner: resowner_seams::current_resource_owner::call(),
        dirty: false,
        read_only,
        fileset: Some(key),
        name: Some(name),
        cur_file: 0,
        cur_offset: 0,
        pos: 0,
        nbytes: 0,
        buffer,
    }))
}

/// `BufFileOpenFileSet(..., missing_ok = false)`: open another participant's
/// file by name; ERROR when it does not exist.
pub fn BufFileOpenFileSet<'mcx>(
    mcx: Mcx<'mcx>,
    fileset: &crate::fileset::FileSet,
    name: &str,
    read_only: bool,
) -> PgResult<BufFile<'mcx>> {
    Ok(open_fileset_common(mcx, fileset, name, read_only, false)?
        .expect("missing_ok=false raised on a missing BufFile"))
}

/// `BufFileOpenFileSet` with C's missing_ok=true arm: None when no segment
/// exists.
pub fn BufFileOpenFileSetMaybe<'mcx>(
    mcx: Mcx<'mcx>,
    fileset: &crate::fileset::FileSet,
    name: &str,
    read_only: bool,
) -> PgResult<Option<BufFile<'mcx>>> {
    open_fileset_common(mcx, fileset, name, read_only, true)
}

/// `BufFileDeleteFileSet` (buffile.c:379): unlink every segment of a named
/// fileset file; an unlink failure on an existing segment is an ERROR
/// (FileSetDelete with error_on_failure = true).
pub fn BufFileDeleteFileSet(
    fileset: &crate::fileset::FileSet,
    name: &str,
    missing_ok: bool,
) -> PgResult<()> {
    let key = fileset.key();
    let mut found = false;
    let mut segment = 0usize;
    loop {
        if !key.delete(&seg_name(name.as_bytes(), segment), true)? {
            break;
        }
        found = true;
        segment += 1;
        // buffile.c:383: cancel point per unlinked segment.
        postgres_seams::check_for_interrupts::call()?;
    }
    if !found && !missing_ok {
        ereport(ERROR)
            .errmsg(format!("could not delete unknown BufFile \"{name}\""))
            .finish(loc("BufFileDeleteFileSet"))?;
    }
    Ok(())
}

impl<'mcx> BufFile<'mcx> {
    /// `BufFileExportFileSet` (buffile.c:398): flush and make the file
    /// read-only so other participants can open it by name.
    pub fn export_fileset(&mut self) -> PgResult<()> {
        debug_assert!(self.fileset.is_some());
        debug_assert!(!self.read_only);
        self.flush()?;
        self.read_only = true;
        Ok(())
    }

    /// `BufFileAppend` (buffile.c:891): concatenate `source`'s segments onto
    /// this fileset BufFile; returns the block number where `source` begins.
    pub fn append(&mut self, source: BufFile<'mcx>) -> PgResult<i64> {
        const BUFFILE_SEG_SIZE: i64 = MAX_PHYSICAL_FILESIZE / BLCKSZ as i64;
        let start_block = self.files.len() as i64 * BUFFILE_SEG_SIZE;
        debug_assert!(self.fileset.is_some());
        debug_assert!(source.read_only);
        debug_assert!(!source.dirty);
        if self.resowner != source.resowner {
            ereport(ERROR)
                .errmsg_internal("could not append BufFile with non-matching resource owner")
                .finish(loc("BufFileAppend"))?;
        }
        // the two Files' indices must name one VFD table, and each carries its slot generation
        if self.owner != source.owner {
            ereport(ERROR)
                .errmsg_internal("could not append BufFile opened on another thread")
                .finish(loc("BufFileAppend"))?;
        }
        ::mcx::pgvec_reserve(&mut self.files, source.files.len())?;
        ::mcx::pgvec_reserve(&mut self.gens, source.gens.len())?;
        self.files.extend(source.files.iter().copied());
        self.gens.extend(source.gens.iter().copied());
        Ok(start_block)
    }

    /// `BufFileTruncateFileSet` (buffile.c:911): truncate at (fileno,
    /// offset), removing whole segments past the point.
    pub fn truncate_fileset(&mut self, fileno: i32, offset: i64) -> PgResult<()> {
        let fileset = self.fileset.expect("truncate_fileset on a fileset BufFile");
        let name: &[u8] = self.name.as_ref().expect("fileset BufFile has a name").as_slice();
        let mut num_files = self.files.len() as i32;
        let mut new_file = fileno;
        let mut new_offset = self.cur_offset;

        // Remove segments past the target; truncate the target in place.
        // A segment truncated to offset 0 is removed too, unless it is the
        // first one.
        let mut i = self.files.len() as i32 - 1;
        while i >= fileno {
            if (i != fileno || offset == 0) && i != 0 {
                let segment_name = seg_name(name, i as usize);
                FileCloseGeneration(self.files[i as usize], self.gens[i as usize])?;
                if !fileset.delete(&segment_name, true)? {
                    ereport(ERROR)
                        .with_saved_errno(get_errno())
                        .errcode_for_file_access()
                        .errmsg(format!("could not delete fileset \"{segment_name}\": {PCT_M}"))
                        .finish(loc("BufFileTruncateFileSet"))?;
                }
                num_files -= 1;
                new_offset = MAX_PHYSICAL_FILESIZE;
                if i == fileno {
                    new_file -= 1;
                }
            } else {
                if FileTruncate(self.files[i as usize], offset, WAIT_EVENT_BUFFILE_TRUNCATE)? < 0 {
                    ereport(ERROR)
                        .with_saved_errno(get_errno())
                        .errcode_for_file_access()
                        .errmsg(format!(
                            "could not truncate file \"{}\": {PCT_M}",
                            file_path_name_lossy(self.files[i as usize])
                        ))
                        .finish(loc("BufFileTruncateFileSet"))?;
                }
                num_files = i + 1;
                new_offset = offset;
            }
            i -= 1;
        }
        self.files.truncate(num_files as usize);
        self.gens.truncate(num_files as usize);

        // Adjust the buffered position to the new end where it overlaps.
        if new_file == self.cur_file
            && new_offset >= self.cur_offset
            && new_offset <= self.cur_offset + self.nbytes as i64
        {
            if new_offset <= self.cur_offset + self.pos as i64 {
                self.pos = (new_offset - self.cur_offset) as i32;
            }
            self.nbytes = (new_offset - self.cur_offset) as i32;
        } else if new_file == self.cur_file && new_offset < self.cur_offset {
            self.cur_offset = new_offset;
            self.pos = 0;
            self.nbytes = 0;
        } else if new_file < self.cur_file {
            self.cur_file = new_file;
            self.cur_offset = new_offset;
            self.pos = 0;
            self.nbytes = 0;
        }
        Ok(())
    }

    fn extend(&mut self) -> PgResult<()> {
        // buffile.c:162: the new segment belongs to the BufFile's owner.
        let old_owner = resowner_seams::current_resource_owner::call();
        resowner_seams::set_current_resource_owner::call(self.resowner);
        let pfile = match &self.fileset {
            None => OpenTemporaryFile(self.is_inter_xact),
            Some(fileset) => {
                let name = self.name.as_ref().expect("fileset BufFile has a name");
                make_new_fileset_segment(fileset, name, self.files.len())
            }
        };
        resowner_seams::set_current_resource_owner::call(old_owner);
        let pfile = pfile?;
        debug_assert!(pfile.0 >= 0);
        self.files.push(pfile);
        self.gens.push(FileGeneration(pfile));
        Ok(())
    }

    pub fn close(mut self) -> PgResult<()> {
        if !self.on_owner(self.dirty) {
            return Ok(());
        }
        // proc_exit already ran the abort resowner release: every temp-file
        // VFD here is freed, and flushing the dirty buffer would write
        // through dead Files. Late drop glue (Tuplesort/Tuplestore on the
        // ProcExitThread unwind or TLS teardown) must be a no-op — C's
        // process exit never revisits sort state (ts-extract grouped-agg run: worker
        // FATAL mid-sort-spill aborted the postmaster via panic-in-drop).
        if ::elog::config::proc_exit_inprogress() {
            return Ok(());
        }
        // likewise once an abort's resource-owner release closed these files (a failed portal's store, ended at
        // the transaction's cleanup): the dirty buffer has no file to go to and nothing is left to close. C never
        // revisits such a BufFile; its memory dies with the portal.
        if self.released() {
            return Ok(());
        }
        // The files close even when the flush fails (a write past temp_file_limit): a pool thread's resource owner
        // never releases them.
        let flushed = self.flush();
        self.close_files().and(flushed)
    }

    pub fn abandon(self) -> PgResult<()> {
        if !self.on_owner(false) || ::elog::config::proc_exit_inprogress() || !crate::vfd::fd_tls_alive() {
            return Ok(());
        }
        self.close_files()
    }

    fn on_owner(&self, unflushed: bool) -> bool {
        match self.owner {
            Some(owner) if !crate::orphan::is_current(owner) => {
                if unflushed {
                    let _ = ereport(::types_error::LOG)
                        .errmsg_internal(format!(
                            "BufFile closed on a thread that did not open it: {} unwritten bytes dropped",
                            self.nbytes
                        ))
                        .finish(loc("BufFileClose"));
                }
                crate::orphan::send(owner, self.files.iter().copied().zip(self.gens.iter().copied()));
                false
            }
            _ => true,
        }
    }

    fn released(&self) -> bool {
        crate::vfd::fd_tls_alive() && self.files.iter().zip(self.gens.iter()).any(|(&f, &g)| FileGeneration(f) != g)
    }

    fn close_files(&self) -> PgResult<()> {
        let mut closed = Ok(());
        for (&file, &gen) in self.files.iter().zip(self.gens.iter()) {
            closed = closed.and(FileCloseGeneration(file, gen));
        }
        closed
    }

    fn load_buffer(&mut self) -> PgResult<()> {
        if self.cur_offset >= MAX_PHYSICAL_FILESIZE
            && (self.cur_file + 1) < self.files.len() as i32
        {
            self.cur_file += 1;
            self.cur_offset = 0;
        }
        let thisfile = self.files[self.cur_file as usize];
        let io_start = io_timing_start();
        let nread = FileRead(
            thisfile,
            &mut self.buffer[..],
            self.cur_offset,
            WAIT_EVENT_BUFFILE_READ,
        )?;
        if nread < 0 {
            self.nbytes = 0;
            return read_failed(thisfile);
        }
        io_timing_accum(&TEMP_BLK_READ_TIME, io_start);
        self.nbytes = nread as i32;
        if self.nbytes > 0 {
            TEMP_BLKS_READ.with(|c| c.set(c.get() + 1));
        }
        Ok(())
    }

    fn dump_buffer(&mut self) -> PgResult<()> {
        let mut wpos: i32 = 0;
        while wpos < self.nbytes {
            if self.cur_offset >= MAX_PHYSICAL_FILESIZE {
                while (self.cur_file + 1) >= self.files.len() as i32 {
                    self.extend()?;
                }
                self.cur_file += 1;
                self.cur_offset = 0;
            }
            let mut bytestowrite = (self.nbytes - wpos) as i64;
            let availbytes = MAX_PHYSICAL_FILESIZE - self.cur_offset;
            if bytestowrite > availbytes {
                bytestowrite = availbytes;
            }
            let thisfile = self.files[self.cur_file as usize];
            let io_start = io_timing_start();
            let written = FileWrite(
                thisfile,
                &self.buffer[wpos as usize..(wpos as i64 + bytestowrite) as usize],
                self.cur_offset,
                WAIT_EVENT_BUFFILE_WRITE,
            )?;
            if written <= 0 {
                return write_failed(thisfile);
            }
            io_timing_accum(&TEMP_BLK_WRITE_TIME, io_start);
            self.cur_offset += written as i64;
            wpos += written as i32;
            TEMP_BLKS_WRITTEN.with(|c| c.set(c.get() + 1));
        }
        self.dirty = false;

        // Make curOffset point to the logical position (original + pos); a
        // small backwards seek in a dirty buffer can leave pos < nbytes.
        self.cur_offset -= (self.nbytes - self.pos) as i64;
        if self.cur_offset < 0 {
            self.cur_file -= 1;
            debug_assert!(self.cur_file >= 0);
            self.cur_offset += MAX_PHYSICAL_FILESIZE;
        }
        self.pos = 0;
        self.nbytes = 0;
        Ok(())
    }

    fn flush(&mut self) -> PgResult<()> {
        if self.dirty {
            self.dump_buffer()?;
        }
        debug_assert!(!self.dirty);
        Ok(())
    }

    fn read_common(&mut self, ptr: &mut [u8], exact: bool, eof_ok: bool) -> PgResult<usize> {
        let start_size = ptr.len();
        let mut size = ptr.len();
        let mut nread = 0usize;

        self.flush()?;

        while size > 0 {
            if self.pos >= self.nbytes {
                self.cur_offset += self.pos as i64;
                self.pos = 0;
                self.nbytes = 0;
                self.load_buffer()?;
                if self.nbytes <= 0 {
                    break;
                }
            }
            let mut nthistime = (self.nbytes - self.pos) as usize;
            if nthistime > size {
                nthistime = size;
            }
            debug_assert!(nthistime > 0);
            ptr[nread..nread + nthistime]
                .copy_from_slice(&self.buffer[self.pos as usize..self.pos as usize + nthistime]);
            self.pos += nthistime as i32;
            size -= nthistime;
            nread += nthistime;
        }

        if exact && nread != start_size && !(nread == 0 && eof_ok) {
            let msg = match &self.name {
                Some(name) => format!(
                    "could not read from file set \"{}\": read only {nread} of {start_size} bytes",
                    String::from_utf8_lossy(name)
                ),
                None => format!(
                    "could not read from temporary file: read only {nread} of {start_size} bytes"
                ),
            };
            ereport(ERROR)
                .errcode_for_file_access()
                .errmsg(msg)
                .finish(loc("BufFileReadCommon"))?;
        }
        Ok(nread)
    }

    pub fn read(&mut self, ptr: &mut [u8]) -> PgResult<usize> {
        self.read_common(ptr, false, false)
    }

    pub fn read_exact(&mut self, ptr: &mut [u8]) -> PgResult<()> {
        self.read_common(ptr, true, false)?;
        Ok(())
    }

    pub fn read_maybe_eof(&mut self, ptr: &mut [u8], eof_ok: bool) -> PgResult<usize> {
        self.read_common(ptr, true, eof_ok)
    }

    pub fn write(&mut self, mut ptr: &[u8]) -> PgResult<()> {
        debug_assert!(!self.read_only);
        while !ptr.is_empty() {
            if self.pos >= BLCKSZ as i32 {
                if self.dirty {
                    self.dump_buffer()?;
                } else {
                    // Went directly from reading to writing.
                    self.cur_offset += self.pos as i64;
                    self.pos = 0;
                    self.nbytes = 0;
                }
            }
            let mut nthistime = BLCKSZ - self.pos as usize;
            if nthistime > ptr.len() {
                nthistime = ptr.len();
            }
            debug_assert!(nthistime > 0);
            self.buffer[self.pos as usize..self.pos as usize + nthistime]
                .copy_from_slice(&ptr[..nthistime]);
            self.dirty = true;
            self.pos += nthistime as i32;
            if self.nbytes < self.pos {
                self.nbytes = self.pos;
            }
            ptr = &ptr[nthistime..];
        }
        Ok(())
    }

    /// spillopt #3 (no C counterpart): `write` of several spans in order, a spill epoch's partition bytes. Under
    /// `THROUGH` bytes in all they take `write`; else the buffer is flushed and the spans go from the caller's memory to
    /// the segment files in gathered writes (`FileWriteV`, up to `PG_IOV_MAX` spans and to the segment's end each), not
    /// copied through the 8 KB buffer (a 2 GB spill was ~260k 8 KB writes, each a copy). The same file layout,
    /// `temp_file_limit` (in `FileWriteV`) and counters (blocks = bytes / BLCKSZ rounded up).
    pub fn write_spans(&mut self, spans: &[&[u8]]) -> PgResult<()> {
        debug_assert!(!self.read_only);
        if spans.iter().map(|s| s.len()).sum::<usize>() < THROUGH {
            return spans.iter().try_for_each(|s| self.write(s));
        }
        self.flush()?;
        self.cur_offset += self.pos as i64;
        (self.pos, self.nbytes) = (0, 0);
        let (mut at, mut skip) = (0usize, 0usize); // the next span, and its bytes already written
        let mut iov: [WriteChunk<'_>; PG_IOV_MAX] = [WriteChunk::from_slice(&[]); PG_IOV_MAX];
        while at < spans.len() {
            if spans[at].len() == skip {
                (at, skip) = (at + 1, 0);
                continue;
            }
            if self.cur_offset >= MAX_PHYSICAL_FILESIZE {
                while (self.cur_file + 1) >= self.files.len() as i32 {
                    self.extend()?;
                }
                self.cur_file += 1;
                self.cur_offset = 0;
            }
            let mut room = (MAX_PHYSICAL_FILESIZE - self.cur_offset) as usize;
            let mut n = 0usize;
            for (k, sp) in spans[at..].iter().enumerate() {
                let sp = if k == 0 { &sp[skip..] } else { *sp };
                if sp.is_empty() {
                    continue;
                }
                if n == PG_IOV_MAX || room == 0 {
                    break;
                }
                let take = sp.len().min(room);
                iov[n] = WriteChunk::from_slice(&sp[..take]);
                (n, room) = (n + 1, room - take);
            }
            let thisfile = self.files[self.cur_file as usize];
            let io_start = io_timing_start();
            let written = FileWriteV(thisfile, &iov[..n], self.cur_offset, WAIT_EVENT_BUFFILE_WRITE)?;
            if written <= 0 {
                return write_failed(thisfile);
            }
            io_timing_accum(&TEMP_BLK_WRITE_TIME, io_start);
            self.cur_offset += written as i64;
            TEMP_BLKS_WRITTEN.with(|c| c.set(c.get() + (written as u64).div_ceil(BLCKSZ as u64) as i64));
            let mut left = written as usize;
            while left > 0 {
                let rest = spans[at].len() - skip;
                if left < rest {
                    skip += left;
                    break;
                }
                left -= rest;
                (at, skip) = (at + 1, 0);
            }
        }
        Ok(())
    }

    pub fn read_through_exact(&mut self, ptr: &mut [u8]) -> PgResult<()> {
        if ptr.len() < THROUGH {
            return self.read_exact(ptr);
        }
        self.flush()?;
        let mut nread = 0usize;
        if self.pos < self.nbytes {
            let n = ((self.nbytes - self.pos) as usize).min(ptr.len());
            ptr[..n].copy_from_slice(&self.buffer[self.pos as usize..self.pos as usize + n]);
            self.pos += n as i32;
            nread = n;
        }
        if nread < ptr.len() {
            self.cur_offset += self.pos as i64;
            (self.pos, self.nbytes) = (0, 0);
        }
        while nread < ptr.len() {
            if self.cur_offset >= MAX_PHYSICAL_FILESIZE && (self.cur_file + 1) < self.files.len() as i32 {
                self.cur_file += 1;
                self.cur_offset = 0;
            }
            let room = (MAX_PHYSICAL_FILESIZE - self.cur_offset).max(0) as usize;
            let n = (ptr.len() - nread).min(room);
            if n == 0 {
                break;
            }
            let thisfile = self.files[self.cur_file as usize];
            let io_start = io_timing_start();
            let got = FileRead(thisfile, &mut ptr[nread..nread + n], self.cur_offset, WAIT_EVENT_BUFFILE_READ)?;
            if got < 0 {
                return read_failed(thisfile);
            }
            if got == 0 {
                break;
            }
            io_timing_accum(&TEMP_BLK_READ_TIME, io_start);
            self.cur_offset += got as i64;
            nread += got as usize;
            TEMP_BLKS_READ.with(|c| c.set(c.get() + (got as u64).div_ceil(BLCKSZ as u64) as i64));
        }
        if nread != ptr.len() {
            let msg = match &self.name {
                Some(name) => format!("could not read from file set \"{}\": read only {nread} of {} bytes", String::from_utf8_lossy(name), ptr.len()),
                None => format!("could not read from temporary file: read only {nread} of {} bytes", ptr.len()),
            };
            ereport(ERROR).errcode_for_file_access().errmsg(msg).finish(loc("BufFileReadCommon"))?;
        }
        Ok(())
    }

    /// 0 on success, EOF (-1) if an impossible seek was attempted.
    pub fn seek(&mut self, fileno: i32, offset: i64, whence: i32) -> PgResult<i32> {
        let mut new_file: i32;
        let mut new_offset: i64;
        match whence {
            SEEK_SET => {
                if fileno < 0 {
                    return Ok(-1);
                }
                new_file = fileno;
                new_offset = offset;
            }
            SEEK_CUR => {
                // Relative seek considers only the signed offset, ignoring
                // fileno. C (-fwrapv) wraps the off_t add; the wrapped value
                // then fails the range checks below as EOF.
                new_file = self.cur_file;
                new_offset = (self.cur_offset + self.pos as i64).wrapping_add(offset);
            }
            SEEK_END => {
                new_file = self.files.len() as i32 - 1;
                new_offset = FileSize(self.files[self.files.len() - 1])?;
                if new_offset < 0 {
                    return Err(self.size_failed(loc("BufFileSeek")));
                }
            }
            other => {
                // elog(ERROR, "invalid whence: %d", whence) (buffile.c:779).
                ereport(ERROR)
                    .errmsg_internal(format!("invalid whence: {other}"))
                    .finish(loc("BufFileSeek"))?;
                return Ok(-1);
            }
        }
        while new_offset < 0 {
            new_file -= 1;
            if new_file < 0 {
                return Ok(-1);
            }
            new_offset += MAX_PHYSICAL_FILESIZE;
        }
        if new_file == self.cur_file
            && new_offset >= self.cur_offset
            && new_offset <= self.cur_offset + self.nbytes as i64
        {
            self.pos = (new_offset - self.cur_offset) as i32;
            return Ok(0);
        }
        self.flush()?;
        // The flush may have created a new segment, so only now translate a
        // start-of-next-seg position and range-check.
        if new_file == self.files.len() as i32 && new_offset == 0 {
            new_file -= 1;
            new_offset = MAX_PHYSICAL_FILESIZE;
        }
        while new_offset > MAX_PHYSICAL_FILESIZE {
            new_file += 1;
            if new_file >= self.files.len() as i32 {
                return Ok(-1);
            }
            new_offset -= MAX_PHYSICAL_FILESIZE;
        }
        if new_file >= self.files.len() as i32 {
            return Ok(-1);
        }
        self.cur_file = new_file;
        self.cur_offset = new_offset;
        self.pos = 0;
        self.nbytes = 0;
        Ok(0)
    }

    pub fn tell(&self) -> (i32, i64) {
        (self.cur_file, self.cur_offset + self.pos as i64)
    }

    /// C `BufFileSeekBlock`.
    pub fn seek_block(&mut self, blknum: i64) -> PgResult<i32> {
        const BUFFILE_SEG_BLOCKS: i64 = MAX_PHYSICAL_FILESIZE / BLCKSZ as i64;
        self.seek(
            (blknum / BUFFILE_SEG_BLOCKS) as i32,
            (blknum % BUFFILE_SEG_BLOCKS) * BLCKSZ as i64,
            SEEK_SET,
        )
    }

    /// C `BufFileSize`; like C, does not count a dirty write buffer.
    pub fn size(&self) -> PgResult<i64> {
        let last = self.files[self.files.len() - 1];
        let last_size = FileSize(last)?;
        if last_size < 0 {
            return Err(self.size_failed(loc("BufFileSize")));
        }
        Ok((self.files.len() as i64 - 1) * MAX_PHYSICAL_FILESIZE + last_size)
    }

    // buffile.c:776/862: `... from BufFile "%s": %m` with file->name, which
    // is NULL for a plain temp file — PG's snprintf renders that "(null)".
    #[cold]
    #[inline(never)]
    fn size_failed(&self, location: ::types_error::ErrorLocation) -> Box<::types_error::PgError> {
        let name = match &self.name {
            Some(name) => String::from_utf8_lossy(name).into_owned(),
            None => "(null)".to_string(),
        };
        ereport(ERROR)
            .with_saved_errno(get_errno())
            .errcode_for_file_access()
            .errmsg(format!(
                "could not determine size of temporary file \"{}\" from BufFile \"{name}\": {PCT_M}",
                file_path_name_lossy(self.files[self.files.len() - 1])
            ))
            .finish(location)
            .unwrap_err()
    }
}

#[cold]
#[inline(never)]
fn read_failed(file: File) -> PgResult<()> {
    ereport(ERROR)
        .with_saved_errno(get_errno())
        .errcode_for_file_access()
        .errmsg(format!("could not read file \"{}\": {PCT_M}", file_path_name_lossy(file)))
        .finish(loc("BufFileLoadBuffer"))
        .map(|_| ())
}

#[cold]
#[inline(never)]
fn write_failed(file: File) -> PgResult<()> {
    ereport(ERROR)
        .with_saved_errno(get_errno())
        .errcode_for_file_access()
        .errmsg(format!("could not write to file \"{}\": {PCT_M}", file_path_name_lossy(file)))
        .finish(loc("BufFileDumpBuffer"))
        .map(|_| ())
}
