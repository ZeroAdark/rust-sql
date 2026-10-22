use std::fs::File as StdFile;
use std::os::fd::{AsRawFd, IntoRawFd, RawFd};

use ::elog::{ereport, PCT_M};
use ::types_core::SubTransactionId;
use ::types_error::{ErrorLevel, PgError, PgResult, ERRCODE_INSUFFICIENT_RESOURCES, ERROR, LOG, WARNING};
use ::types_storage::{Dir, DirEnt, FD_MINFREE};

use crate::vfd::{self, get_errno, loc, set_errno, with_fd, FdState};

pub(crate) enum AllocatedHandle {
    // DST P1 carve-out: buffered stdio streams (AllocateFile fopen) and pipes
    // (OpenPipeStream popen) stay posix-only in P1 — out of Vfs scope.
    File(StdioStream),
    Dir(Option<vfs::VfsDirIter>),
    // vfs-minted, so the holder must be the vfs-close guard (finding F1b):
    // an unwind/thread-exit drop releases into the active vfs namespace,
    // never posix-side.
    RawFd(vfs::VfsFd),
    Pipe(PipeHandle),
}

// fopen's FILE: the fwrite buffer lives with the stream, so fflush(NULL)
// (OpenPipeStream) and fclose (FreeDesc) reach it. bufsize starts at BUFSIZ;
// StdioSetBuf is the caller's st_blksize choice (__smakebuf).
pub(crate) struct StdioStream {
    pub file: StdFile,
    wbuf: Vec<u8>,
    bufsize: usize,
}

impl StdioStream {
    fn new(file: StdFile) -> Self {
        StdioStream { file, wbuf: Vec::new(), bufsize: 1024 }
    }

    fn write(&mut self, bytes: &[u8]) -> Result<(), i32> {
        use std::io::Write;

        let mut rest = bytes;
        if !self.wbuf.is_empty() {
            let take = rest.len().min(self.bufsize.saturating_sub(self.wbuf.len()));
            self.wbuf.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if self.wbuf.len() < self.bufsize {
                return Ok(());
            }
            let r = self.file.write_all(&self.wbuf);
            self.wbuf.clear();
            if let Err(e) = r {
                return Err(e.raw_os_error().unwrap_or(0));
            }
        }
        let whole = rest.len() - rest.len() % self.bufsize;
        if whole > 0 {
            if let Err(e) = self.file.write_all(&rest[..whole]) {
                return Err(e.raw_os_error().unwrap_or(0));
            }
        }
        self.wbuf.extend_from_slice(&rest[whole..]);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), i32> {
        use std::io::Write;

        if self.wbuf.is_empty() {
            return Ok(());
        }
        let r = self.file.write_all(&self.wbuf);
        self.wbuf.clear();
        r.map_err(|e| e.raw_os_error().unwrap_or(0))
    }
}

pub(crate) struct PipeHandle {
    pub child: std::process::Child,
    pub stdout: Option<std::process::ChildStdout>,
    pub stdin: Option<std::process::ChildStdin>,
}

impl Drop for PipeHandle {
    fn drop(&mut self) {
        init_small::globals::unregister_backend_child(self.child.id());
    }
}

pub(crate) struct AllocateDesc {
    pub create_subid: SubTransactionId,
    pub desc: AllocatedHandle,
}

pub(crate) fn reserveAllocatedDesc(fd: &mut FdState) -> PgResult<bool> {
    let num = vfd::occupied_descs(fd);

    if num < fd.max_allocated_descs {
        return Ok(true);
    }

    // First allocation: FD_MINFREE / 3 slots (set_max_safe_fds may not have
    // run yet). C's malloc failure is ereport(ERROR, ERRCODE_OUT_OF_MEMORY).
    if fd.max_allocated_descs == 0 {
        let new_max = FD_MINFREE / 3;
        fd.allocated_descs
            .try_reserve_exact(new_max as usize)
            .map_err(|_| oom_descs(new_max as usize))?;
        fd.max_allocated_descs = new_max;
        return Ok(true);
    }

    // Cap at max_safe_fds / 3 so allocated descriptors can't hog all the FDs.
    let new_max = vfd::effective_max_safe_fds() / 3;
    if new_max > fd.max_allocated_descs {
        fd.allocated_descs
            .try_reserve_exact((new_max - num) as usize)
            .map_err(|_| oom_descs(new_max as usize))?;
        fd.max_allocated_descs = new_max;
        return Ok(true);
    }

    Ok(false)
}

/// fd.c reserveAllocatedDesc: `ereport(ERROR, (errcode(ERRCODE_OUT_OF_MEMORY),
/// errmsg("out of memory")))` on a failed realloc of the descriptor array.
#[cold]
fn oom_descs(slots: usize) -> PgError {
    ::mcx::oom_named("AllocateDesc", slots * core::mem::size_of::<AllocateDesc>())
}

fn exceeded_max_descs(what: &str, name: &str, func: &'static str) -> PgResult<()> {
    let max = with_fd(|fd| fd.max_allocated_descs);
    ereport(ERROR)
        .errcode(ERRCODE_INSUFFICIENT_RESOURCES)
        .errmsg(format!(
            "exceeded maxAllocatedDescs ({max}) while trying to {what} \"{name}\""
        ))
        .finish(loc(func))
}

fn out_of_fds_log() -> PgResult<()> {
    let en = get_errno();
    ereport(LOG)
        .with_saved_errno(en)
        .errcode(ERRCODE_INSUFFICIENT_RESOURCES)
        .errmsg(format!("out of file descriptors: {PCT_M}; release and retry"))
        .finish(loc("AllocateFile"))
}

fn current_subid() -> SubTransactionId {
    xact_seams::get_current_sub_transaction_id::call()
}

// C compacts and re-finds by FILE*; index handles need stable slots, so the
// slot is tombstoned instead (trailing tombstones trimmed). Returns the
// underlying close result.
pub(crate) fn FreeDesc(index: i32) -> i32 {
    let desc = with_fd(|fd| {
        let desc = fd.allocated_descs[index as usize].take().expect("FreeDesc: occupied slot");
        while matches!(fd.allocated_descs.last(), Some(None)) {
            fd.allocated_descs.pop();
        }
        desc
    });
    match desc.desc {
        AllocatedHandle::File(mut stream) => {
            let flushed = stream.flush();
            // DST P1 carve-out boundary (contract §1.1, Ruling 3 Class C):
            // this fd was minted posix-side by open_stdio (the fopen plane,
            // permanently out of Vfs scope), so it must close posix-side too
            // — vfs::close is reserved for vfs-minted fds. Under sim, SimVfs
            // EBADFs foreign fds below SIM_FD_BASE (the domain tripwire that
            // caught the original misroute; the fd then leaked). Allowlist
            // row retained under the P0 fencing lint with the fopen sites.
            // Live descriptor released from its guard; closed once here.
            let raw = stream.file.into_raw_fd();
            // SAFETY: into_raw_fd transferred ownership; closed exactly once.
            let rc = unsafe { libc::close(raw) };
            match flushed {
                Err(en) => {
                    set_errno(en);
                    -1
                }
                Ok(()) => rc,
            }
        }
        AllocatedHandle::Pipe(pipe) => pclose(pipe),
        AllocatedHandle::Dir(dir) => {
            drop(dir);
            0
        }
        AllocatedHandle::RawFd(file) => {
            crate::pgaio_closing_fd_if_engine_present(file.as_raw_fd());
            // Live descriptor released from its guard (disarmed); closed
            // exactly once here.
            let raw = file.into_raw();
            vfs::close(raw)
        }
    }
}


// First tombstoned slot, else append (stable indices; see FreeDesc).
fn install_desc(fd: &mut FdState, desc: AllocateDesc) -> i32 {
    match fd.allocated_descs.iter().position(Option::is_none) {
        Some(i) => {
            fd.allocated_descs[i] = Some(desc);
            i as i32
        }
        None => {
            fd.allocated_descs.push(Some(desc));
            (fd.allocated_descs.len() - 1) as i32
        }
    }
}

// C contract: table index >= 0, or -1 with errno set on fopen failure.
pub fn AllocateFile(name: &str, mode: &str) -> PgResult<i32> {
    AllocateFilePath(&vfs::PgPath::from(name), mode)
}

/// AllocateFile over a byte path.
pub fn AllocateFilePath(name: &vfs::PgPath, mode: &str) -> PgResult<i32> {
    if !with_fd(reserveAllocatedDesc)? {
        exceeded_max_descs("open file", &name.to_string_lossy(), "AllocateFile")?;
    }

    with_fd(vfd::ReleaseLruFiles)?;

    loop {
        match open_stdio(name.as_os_str(), mode) {
            Ok(file) => {
                let create_subid = current_subid();
                return with_fd(|fd| {
                    Ok(install_desc(
                        fd,
                        AllocateDesc {
                            create_subid,
                            desc: AllocatedHandle::File(StdioStream::new(file)),
                        },
                    ))
                });
            }
            Err(en) => {
                if en == libc::EMFILE || en == libc::ENFILE {
                    set_errno(en);
                    out_of_fds_log()?;
                    if with_fd(vfd::ReleaseLruFile)? {
                        continue;
                    }
                }
                set_errno(en);
                return Ok(-1);
            }
        }
    }
}

pub fn FreeFile(index: i32) -> PgResult<i32> {
    let found = with_fd(|fd| {
        matches!(
            fd.allocated_descs.get(index as usize),
            Some(Some(AllocateDesc { desc: AllocatedHandle::File(_), .. }))
        )
    });
    if found {
        return Ok(FreeDesc(index));
    }

    ereport(WARNING)
        .errmsg("file passed to FreeFile was not obtained from AllocateFile")
        .finish(loc("FreeFile"))?;
    Ok(-1)
}

pub fn OpenTransientFile(file_name: &str, file_flags: i32) -> PgResult<i32> {
    OpenTransientFilePerm(file_name, file_flags, vfd::pg_file_create_mode())
}

// C contract: the raw kernel fd, or -1 with errno set on open failure.
pub fn OpenTransientFilePerm(file_name: &str, file_flags: i32, file_mode: u32) -> PgResult<i32> {
    OpenTransientFilePermPath(&vfs::PgPath::from(file_name), file_flags, file_mode)
}

/// OpenTransientFile over a byte path (basebackup opens each dirent by the
/// name the kernel returned).
pub fn OpenTransientFilePath(file_name: &vfs::PgPath, file_flags: i32) -> PgResult<i32> {
    OpenTransientFilePermPath(file_name, file_flags, vfd::pg_file_create_mode())
}

pub fn OpenTransientFilePermPath(
    file_name: &vfs::PgPath,
    file_flags: i32,
    file_mode: u32,
) -> PgResult<i32> {
    if !with_fd(reserveAllocatedDesc)? {
        exceeded_max_descs("open file", &file_name.to_string_lossy(), "OpenTransientFilePerm")?;
    }

    with_fd(|fd| {
        vfd::ReleaseLruFiles(fd)?;

        let raw = vfd::BasicOpenFilePermInternalBytes(fd, file_name, file_flags, file_mode)?;
        if raw >= 0 {
            install_desc(
                fd,
                AllocateDesc {
                    create_subid: current_subid(),
                    // SAFETY: freshly vfs-opened descriptor now owned by the
                    // table's guard.
                    desc: AllocatedHandle::RawFd(unsafe { vfs::VfsFd::from_raw(raw) }),
                },
            );
        }
        Ok(raw)
    })
}

// C contract: close(2)'s result.
pub fn CloseTransientFile(fd_to_close: i32) -> i32 {
    let index = with_fd(|fd| {
        for i in (0..fd.allocated_descs.len()).rev() {
            if let Some(AllocateDesc { desc: AllocatedHandle::RawFd(f), .. }) =
                &fd.allocated_descs[i]
            {
                if f.as_raw_fd() == fd_to_close {
                    return Some(i as i32);
                }
            }
        }
        None
    });
    if let Some(index) = index {
        return FreeDesc(index);
    }

    let _ = ereport(WARNING)
        .errmsg("fd passed to CloseTransientFile was not obtained from OpenTransientFile")
        .finish(loc("CloseTransientFile"));

    crate::pgaio_closing_fd_if_engine_present(fd_to_close);
    // Caller asserts ownership of this descriptor, as in C.
    vfs::close(fd_to_close)
}

// C contract: table index >= 0, or -1 with errno set on popen failure.
pub fn OpenPipeStream(command: &str, mode: &str) -> PgResult<i32> {
    if !with_fd(reserveAllocatedDesc)? {
        exceeded_max_descs("execute command", command, "OpenPipeStream")?;
    }

    with_fd(vfd::ReleaseLruFiles)?;

    // C flips SIGPIPE to SIG_DFL around popen; the child's signal state is
    // restored in popen's pre_exec instead.
    loop {
        with_fd(flush_all_stdio);
        match popen(command, mode) {
            Ok(pipe) => {
                let create_subid = current_subid();
                return with_fd(|fd| {
                    Ok(install_desc(
                        fd,
                        AllocateDesc { create_subid, desc: AllocatedHandle::Pipe(pipe) },
                    ))
                });
            }
            Err(en) => {
                if en == libc::EMFILE || en == libc::ENFILE {
                    set_errno(en);
                    out_of_fds_log()?;
                    if with_fd(vfd::ReleaseLruFile)? {
                        continue;
                    }
                }
                set_errno(en);
                return Ok(-1);
            }
        }
    }
}

// fgets(buf, size, fh) over an OpenPipeStream handle: fills up to
// buf.len()-1 bytes, stops after '\n'. Ok(0) is EOF-without-bytes (C NULL
// with !ferror); Err(errno) is C NULL with ferror set.
pub fn PipeStreamGets(index: i32, buf: &mut [u8]) -> Result<usize, i32> {
    use std::io::Read;

    let raw = with_pipe(index, |p| p.stdout.as_ref().map(pipe_raw_fd).ok_or(libc::EBADF))?;
    let mut n = 0usize;
    while n + 1 < buf.len() {
        let mut byte = [0u8; 1];
        wait_pipe(raw, PIPE_READABLE);
        let got = with_pipe(index, |p| {
            let out = p.stdout.as_mut().ok_or(libc::EBADF)?;
            match out.read(&mut byte) {
                Ok(k) => Ok(Some(k)),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => Ok(None),
                Err(e) => Err(e.raw_os_error().unwrap_or(libc::EIO)),
            }
        })?;
        match got {
            None => continue,
            Some(0) => break,
            Some(_) => {
                buf[n] = byte[0];
                n += 1;
                if byte[0] == b'\n' {
                    break;
                }
            }
        }
    }
    if n < buf.len() {
        buf[n] = 0;
    }
    Ok(n)
}

// fread(buf, 1, buf.len(), fh) over an OpenPipeStream "r" handle: greedy
// fill to buf.len() (stdio fread loops internally), short only at EOF.
// Ok(0) is EOF-without-bytes; Err(errno) is C's ferror case.
pub fn PipeStreamRead(index: i32, buf: &mut [u8]) -> Result<usize, i32> {
    use std::io::Read;

    let raw = with_pipe(index, |p| p.stdout.as_ref().map(pipe_raw_fd).ok_or(libc::EBADF))?;
    let mut filled = 0usize;
    while filled < buf.len() {
        wait_pipe(raw, PIPE_READABLE);
        let got = with_pipe(index, |p| {
            let out = p.stdout.as_mut().ok_or(libc::EBADF)?;
            match out.read(&mut buf[filled..]) {
                Ok(k) => Ok(Some(k)),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => Ok(None),
                Err(e) => Err(e.raw_os_error().unwrap_or(libc::EIO)),
            }
        })?;
        match got {
            None => continue,
            Some(0) => break,
            Some(k) => filled += k,
        }
    }
    Ok(filled)
}

// fwrite over an OpenPipeStream "w" handle. Err(errno) is C's
// fwrite-short/ferror case; a dead child surfaces as EPIPE (SIGPIPE is
// ignored process-wide in the Rust runtime, matching the backend's SIG_IGN).
pub fn PipeStreamWrite(index: i32, buf: &[u8]) -> Result<(), i32> {
    use std::io::Write;

    let raw = with_pipe(index, |p| p.stdin.as_ref().map(pipe_raw_fd).ok_or(libc::EBADF))?;
    let mut at = 0;
    while at < buf.len() {
        wait_pipe(raw, PIPE_WRITABLE);
        let end = buf.len().min(at + PIPE_WRITE_CHUNK);
        let wrote = with_pipe(index, |p| {
            let child_stdin = p.stdin.as_mut().ok_or(libc::EBADF)?;
            match child_stdin.write(&buf[at..end]) {
                Ok(0) => Err(libc::EIO),
                Ok(k) => Ok(k),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => Ok(0),
                Err(e) => Err(e.raw_os_error().unwrap_or(libc::EIO)),
            }
        })?;
        at += wrote;
    }
    Ok(())
}

fn with_pipe<R>(index: i32, f: impl FnOnce(&mut PipeHandle) -> Result<R, i32>) -> Result<R, i32> {
    with_fd(|fd| {
        let Some(Some(desc)) = fd.allocated_descs.get_mut(index as usize) else {
            return Err(libc::EBADF);
        };
        let AllocatedHandle::Pipe(pipe) = &mut desc.desc else {
            return Err(libc::EBADF);
        };
        f(pipe)
    })
}

// C's SIGALRM interrupts a fread/fwrite blocked on the program's pipe and
// StatementTimeoutHandler's group SIGINT kills the program. Here the timer
// thread only posts the alarm, so a pipe wait runs the timeout handlers
// between bounded polls (outside the fd borrow); the handler's thread signal
// reaches the program through procsignal's group fan-out and the I/O call
// then sees EOF or EPIPE.
#[cfg(not(target_family = "wasm"))]
const PIPE_READABLE: i16 = libc::POLLIN;
#[cfg(not(target_family = "wasm"))]
const PIPE_WRITABLE: i16 = libc::POLLOUT;
#[cfg(target_family = "wasm")]
const PIPE_READABLE: i16 = 0;
#[cfg(target_family = "wasm")]
const PIPE_WRITABLE: i16 = 0;
// POLLOUT on a pipe guarantees room for PIPE_BUF bytes, so a chunk this size
// never blocks after the wait.
#[cfg(not(target_family = "wasm"))]
const PIPE_WRITE_CHUNK: usize = libc::PIPE_BUF;
#[cfg(target_family = "wasm")]
const PIPE_WRITE_CHUNK: usize = 512;
const PIPE_POLL_MS: i32 = 100;

#[cfg(not(target_family = "wasm"))]
fn pipe_raw_fd(end: &impl std::os::fd::AsRawFd) -> i32 {
    end.as_raw_fd()
}

#[cfg(target_family = "wasm")]
fn pipe_raw_fd<T>(_end: &T) -> i32 {
    -1
}

#[cfg(not(target_family = "wasm"))]
fn wait_pipe(raw: i32, events: i16) {
    loop {
        let mut p = libc::pollfd { fd: raw, events, revents: 0 };
        // SAFETY: poll(2) over one valid pollfd on a pipe end we own.
        if unsafe { libc::poll(&mut p, 1, PIPE_POLL_MS) } != 0 {
            return;
        }
        timeout::ProcessTimeoutInterrupt();
    }
}

#[cfg(target_family = "wasm")]
fn wait_pipe(_raw: i32, _events: i16) {}

// C contract: pclose()'s wait status, -1 on a stream we don't track.
pub fn ClosePipeStream(index: i32) -> PgResult<i32> {
    let found = with_fd(|fd| {
        matches!(
            fd.allocated_descs.get(index as usize),
            Some(Some(AllocateDesc { desc: AllocatedHandle::Pipe(_), .. }))
        )
    });
    if found {
        return Ok(FreeDesc(index));
    }

    ereport(WARNING)
        .errmsg("file passed to ClosePipeStream was not obtained from OpenPipeStream")
        .finish(loc("ClosePipeStream"))?;
    Ok(-1)
}

// C contract: Ok(None) mirrors a NULL DIR* with errno set; the following
// ReadDir reports the failure.
pub fn AllocateDir(dirname: &str) -> PgResult<Option<Dir>> {
    AllocateDirPath(&vfs::PgPath::from(dirname))
}

/// AllocateDir over a byte path.
pub fn AllocateDirPath(dirname: &vfs::PgPath) -> PgResult<Option<Dir>> {
    if !with_fd(reserveAllocatedDesc)? {
        exceeded_max_descs("open directory", &dirname.to_string_lossy(), "AllocateDir")?;
    }

    with_fd(vfd::ReleaseLruFiles)?;

    let cdir = dirname.to_c_string();
    loop {
        match vfs::read_dir(&cdir) {
            Ok(iter) => {
                let create_subid = current_subid();
                return with_fd(|fd| {
                    Ok(Some(install_desc(
                        fd,
                        AllocateDesc { create_subid, desc: AllocatedHandle::Dir(Some(iter)) },
                    )))
                });
            }
            Err(en) => {
                if en == libc::EMFILE || en == libc::ENFILE {
                    set_errno(en);
                    out_of_fds_log()?;
                    if with_fd(vfd::ReleaseLruFile)? {
                        continue;
                    }
                }
                set_errno(en);
                return Ok(None);
            }
        }
    }
}

pub fn ReadDir(dir: Option<Dir>, dirname: &str) -> PgResult<Option<DirEnt>> {
    ReadDirExtended(dir, dirname, ERROR)
}

pub fn ReadDirExtended(
    dir: Option<Dir>,
    dirname: &str,
    elevel: ErrorLevel,
) -> PgResult<Option<DirEnt>> {
    // The historical String view: a name the kernel returned that is not
    // UTF-8 renders with U+FFFD here. Byte-faithful walkers (basebackup)
    // use ReadDirPath / with_allocated_dir_path instead.
    Ok(ReadDirPath(dir, &vfs::PgPath::from(dirname), elevel)?
        .map(|d_name| DirEnt { d_name: d_name.to_string_lossy() }))
}

/// ReadDirExtended yielding the raw `d_name` bytes (C's `struct dirent`).
pub fn ReadDirPath(
    dir: Option<Dir>,
    dirname: &vfs::PgPath,
    elevel: ErrorLevel,
) -> PgResult<Option<vfs::PgPath>> {
    Ok(ReadDirEntry(dir, dirname, elevel)?.map(|de| de.d_name))
}

/// ReadDirExtended yielding the whole `struct dirent` (`d_name` bytes and
/// `d_type`), for walkers that classify entries through get_dirent_type.
pub fn ReadDirEntry(
    dir: Option<Dir>,
    dirname: &vfs::PgPath,
    elevel: ErrorLevel,
) -> PgResult<Option<vfs::VfsDirEnt>> {
    let index = match dir {
        None => {
            ereport(elevel)
                .with_saved_errno(get_errno())
                .errcode_for_file_access()
                .errmsg(format!("could not open directory \"{dirname}\": {PCT_M}"))
                .finish(loc("ReadDirExtended"))?;
            return Ok(None);
        }
        Some(index) => index,
    };

    match next_dirent_entry(index) {
        Some(Ok(de)) => Ok(Some(de)),
        Some(Err(en)) => {
            ereport(elevel)
                .with_saved_errno(en)
                .errcode_for_file_access()
                .errmsg(format!("could not read directory \"{dirname}\": {PCT_M}"))
                .finish(loc("ReadDirExtended"))?;
            Ok(None)
        }
        None => Ok(None),
    }
}

// One readdir(3) step: None at end of directory, Err(errno) on a read
// failure (C's errno != 0 after a NULL readdir).
pub(crate) fn next_dirent(index: Dir) -> Option<Result<vfs::PgPath, i32>> {
    next_dirent_entry(index).map(|r| r.map(|de| de.d_name))
}

pub(crate) fn next_dirent_entry(index: Dir) -> Option<Result<vfs::VfsDirEnt, i32>> {
    with_fd(|fd| match fd.allocated_descs[index as usize].as_mut() {
        Some(AllocateDesc { desc: AllocatedHandle::Dir(iter), .. }) => {
            iter.as_mut().and_then(vfs::VfsDirIter::next_entry)
        }
        _ => None,
    })
}

pub fn FreeDir(dir: Option<Dir>) -> PgResult<i32> {
    let index = match dir {
        None => return Ok(0),
        Some(index) => index,
    };

    let found = with_fd(|fd| {
        matches!(
            fd.allocated_descs.get(index as usize),
            Some(Some(AllocateDesc { desc: AllocatedHandle::Dir(_), .. }))
        )
    });
    if found {
        return Ok(FreeDesc(index));
    }

    ereport(WARNING)
        .errmsg("dir passed to FreeDir was not obtained from AllocateDir")
        .finish(loc("FreeDir"))?;
    Ok(-1)
}

pub fn closeAllVfds() -> PgResult<()> {
    with_fd(|fd| {
        if fd.size_vfd_cache() > 0 {
            debug_assert!(vfd::FileIsNotOpen(fd, 0));
            for i in 1..fd.size_vfd_cache() as i32 {
                if !vfd::FileIsNotOpen(fd, i) {
                    vfd::LruDelete(fd, i)?;
                }
            }
        }
        Ok(())
    })
}

// The `with_allocated_dir` seam: AllocateDir + ReadDir + FreeDir as one owned
// walk. `f` returns Ok(true) to stop early; the walk returns the last
// callback value (false once the directory is exhausted).
pub fn with_allocated_dir(
    dirname: &str,
    f: &mut dyn FnMut(&str) -> PgResult<bool>,
) -> PgResult<bool> {
    with_allocated_dir_path(&vfs::PgPath::from(dirname), &mut |name| f(&name.to_string_lossy()))
}

/// `with_allocated_dir` over byte paths: the callback sees each `d_name`
/// exactly as the kernel returned it.
pub fn with_allocated_dir_path(
    dirname: &vfs::PgPath,
    f: &mut dyn FnMut(&vfs::PgPath) -> PgResult<bool>,
) -> PgResult<bool> {
    let dir = AllocateDirPath(dirname)?;

    let mut last = false;
    loop {
        let ent = match ReadDirPath(dir, dirname, ERROR) {
            Ok(Some(ent)) => ent,
            Ok(None) => break,
            Err(e) => {
                let _ = FreeDir(dir);
                return Err(e);
            }
        };

        match f(&ent) {
            Ok(stop) => {
                last = stop;
                if stop {
                    break;
                }
            }
            Err(e) => {
                let _ = FreeDir(dir);
                return Err(e);
            }
        }
    }

    FreeDir(dir)?;
    Ok(last)
}

// DST P1 carve-out: the buffered-stream (fopen) plane stays posix-only in P1;
// stdio is permanently out of Vfs scope (contract §1.1). Allowlist row
// retained under the P0 fencing lint.
fn open_stdio(name: &std::ffi::OsStr, mode: &str) -> Result<StdFile, i32> {
    use std::fs::OpenOptions;

    // fopen mode map for the modes the backend passes (optional trailing 'b').
    let m = mode.trim_end_matches('b');
    let mut opts = OpenOptions::new();
    match m {
        "r" => opts.read(true),
        "w" => opts.write(true).create(true).truncate(true),
        "a" => opts.append(true).create(true),
        "r+" => opts.read(true).write(true),
        "w+" => opts.read(true).write(true).create(true).truncate(true),
        "a+" => opts.read(true).append(true).create(true),
        _ => opts.read(true),
    };
    opts.open(name).map_err(|e| e.raw_os_error().unwrap_or(0))
}

fn popen(command: &str, mode: &str) -> Result<PipeHandle, i32> {
    use std::process::{Command, Stdio};

    let reading = mode.starts_with('r');
    let mut cmd = Command::new("/bin/sh");
    #[cfg(not(target_family = "wasm"))]
    {
        use std::os::unix::process::CommandExt;
        cmd.arg0("sh");
    }
    cmd.arg("-c").arg(command);
    // A C backend's popen child starts with an empty signal mask and the
    // backend's dispositions after exec: handled signals back to default,
    // SIGUSR2 (and the postmaster's SIGPIPE/TTIN/TTOU/XFSZ) still ignored.
    // The thread model blocks the process signals on every backend thread
    // and ignores SIGALRM; both would survive into the child, leaving it
    // deaf to the group SIGINT/SIGTERM of pg_signal_backend.
    #[cfg(not(target_family = "wasm"))]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: async-signal-safe calls only (sigemptyset, pthread_sigmask, setpgid,
        // signal), run in the child between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut set);
                libc::pthread_sigmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
                libc::signal(libc::SIGALRM, libc::SIG_DFL);
                libc::signal(libc::SIGUSR2, libc::SIG_IGN);
                // C's program shares the backend's process group, which
                // kill(-backend, sig) reaches whole; here the shell leads
                // its own group so the fan-out's kill(-child) reaches the
                // program the shell runs, not just the shell.
                libc::setpgid(0, 0);
                Ok(())
            });
        }
    }
    if reading {
        cmd.stdout(Stdio::piped());
    } else {
        cmd.stdin(Stdio::piped());
    }
    match cmd.spawn() {
        Ok(mut child) => {
            init_small::globals::register_backend_child(child.id());
            let stdout = child.stdout.take();
            let stdin = child.stdin.take();
            Ok(PipeHandle {
                child,
                stdout,
                stdin,
            })
        }
        Err(e) => Err(e.raw_os_error().unwrap_or(0)),
    }
}

// pclose: close our pipe end (child sees EOF/SIGPIPE), wait, and return the
// raw wait(2) status word.
fn pclose(mut pipe: PipeHandle) -> i32 {
    drop(pipe.stdout.take());
    drop(pipe.stdin.take());
    match pipe.child.wait() {
        Ok(status) => {
            if let Some(code) = status.code() {
                (code & 0xff) << 8
            } else {
                #[cfg(not(target_family = "wasm"))]
                {
                    use std::os::unix::process::ExitStatusExt;
                    status.signal().unwrap_or(0) & 0x7f
                }
                // wasm32: WASI has no processes/signals; unreachable (the
                // popen arm can never spawn), keep the C -1 failure word.
                #[cfg(target_family = "wasm")]
                {
                    -1
                }
            }
        }
        Err(_) => -1,
    }
}

// Reach the buffered stream behind an AllocateFile index (the C callers'
// direct `FILE *` reads/writes).
pub fn with_allocated_stdio<R>(index: i32, f: impl FnOnce(&mut StdFile) -> R) -> Option<R> {
    with_fd(|fd| match fd.allocated_descs.get_mut(index as usize) {
        Some(Some(AllocateDesc {
            desc: AllocatedHandle::File(stream),
            ..
        })) => Some(f(&mut stream.file)),
        _ => None,
    })
}

fn with_stdio_stream<R>(index: i32, f: impl FnOnce(&mut StdioStream) -> Result<R, i32>) -> Result<R, i32> {
    with_fd(|fd| match fd.allocated_descs.get_mut(index as usize) {
        Some(Some(AllocateDesc {
            desc: AllocatedHandle::File(stream),
            ..
        })) => f(stream),
        _ => Err(libc::EBADF),
    })
}

// fwrite over an AllocateFile stream; Err is the errno of a failed write.
pub fn StdioWrite(index: i32, bytes: &[u8]) -> Result<(), i32> {
    with_stdio_stream(index, |s| s.write(bytes))
}

// fflush of one AllocateFile stream; Err is the errno of a failed write.
pub fn StdioFlush(index: i32) -> Result<(), i32> {
    with_stdio_stream(index, StdioStream::flush)
}

// setvbuf-style buffer size (0 keeps BUFSIZ) for an AllocateFile stream.
pub fn StdioSetBuf(index: i32, bufsize: usize) -> Result<(), i32> {
    with_stdio_stream(index, |s| {
        if bufsize > 0 {
            s.bufsize = bufsize;
        }
        Ok(())
    })
}

// fflush(NULL) before fork: every open stream's pending output, this
// backend's stdout included, reaches the kernel before the child starts.
fn flush_all_stdio(fd: &mut FdState) {
    use std::io::Write;

    for desc in fd.allocated_descs.iter_mut().flatten() {
        if let AllocatedHandle::File(stream) = &mut desc.desc {
            let _ = stream.flush();
        }
    }
    let _ = std::io::stdout().flush();
}

// Raw kernel fd behind a transient-file value (fstat-style callers).
// Raw-fd escape hatch (DST P1 contract §1.1 carve-out): the returned kernel
// descriptor bypasses the VFS. Sim-scope callers must not use it.
pub fn TransientFileRawFd(fd_value: i32) -> Option<RawFd> {
    with_fd(|fd| {
        fd.allocated_descs.iter().rev().flatten().find_map(|d| match &d.desc {
            AllocatedHandle::RawFd(f) if f.as_raw_fd() == fd_value => Some(f.as_raw_fd()),
            _ => None,
        })
    })
}

#[cfg(test)]
mod stdio_stream_tests {
    use super::StdioStream;

    fn scratch(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("fd-stdio-{}-{}", name, std::process::id()))
    }

    #[test]
    fn write_preserves_bytes_across_chunk_boundaries() {
        let path = scratch("bytes");
        let mut s = StdioStream::new(std::fs::File::create(&path).unwrap());
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let mut off = 0;
        for step in [1usize, 7, 1023, 1024, 1025, 4096, 3, 50_000].iter().cycle() {
            if off >= data.len() {
                break;
            }
            let end = (off + step).min(data.len());
            s.write(&data[off..end]).unwrap();
            assert!(s.wbuf.len() < s.bufsize);
            off = end;
        }
        s.flush().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), data);
        let _ = std::fs::remove_file(&path);
    }

    // This thread's CPU time: a quadratic copy of a large write burns seconds of it, while a loaded box's writeback can
    // hold the write itself for minutes of wall clock (D3459: 271 s on a box at 90 % io-full).
    fn thread_cpu() -> std::time::Duration {
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: clock_gettime only writes the timespec it is given.
        assert_eq!(unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) }, 0);
        std::time::Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
    }

    #[test]
    fn large_single_write_is_linear() {
        let path = scratch("large");
        let mut s = StdioStream::new(std::fs::File::create(&path).unwrap());
        let data = vec![b'D'; 32 << 20];
        let start = thread_cpu();
        s.write(&data).unwrap();
        s.flush().unwrap();
        let spent = thread_cpu() - start;
        assert!(spent < std::time::Duration::from_secs(5), "{spent:?} of CPU");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), data.len() as u64);
        let _ = std::fs::remove_file(&path);
    }
}
