// fileset.c + sharedfileset.c, thread-native: the set of named temp files a
// parallel query's participants share. C keys the set to creator_pid + a
// per-backend counter; threads share a pid, so the counter is process-wide.
// Placement follows fileset.c exactly: every file name (a BufFile segment is
// a name of its own, "<name>.<n>") is hashed with hash_any across the
// captured tablespaces, so the segments of one BufFile can live in different
// tablespaces just as in C. Deletion: C ties SharedFileSetDeleteAll to DSM
// detach; here the owner (Arc'd parallel node state) drops the FileSet when
// the last participant releases it, which deletes the directories.
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

use ::types_core::{InvalidOid, Oid};
use ::types_error::PgResult;
use ::types_storage::File;

use crate::vfd::{with_fd, FD_TEMP_FILE_LIMIT};
use crate::temp::{
    GetTempTablespaces, PathNameCreateTemporaryDir, PathNameCreateTemporaryFile,
    PathNameDeleteTemporaryDir, PathNameDeleteTemporaryFile, PathNameDeleteTemporaryFilesWith,
    PathNameOpenTemporaryFile, TempTablespacePath,
};

const PG_TEMP_FILE_PREFIX: &str = "pgsql_tmp";

// storage/fileset.h FILESET_MAX_TABLESPACES.
const FILESET_MAX_TABLESPACES: usize = 8;

// fileset.c:70 `static uint32 counter`, `counter = (counter + 1) % INT_MAX`.
static FILESET_NUMBER: AtomicU32 = AtomicU32::new(0);

/// The C `FileSet` struct: the identity every participant (and every BufFile
/// built on the set) needs to compute paths. Plain data, freely copied — a
/// BufFile carries one so it can create/delete its own segments without a
/// reference to the owning [`FileSet`] (C's `buffile->fileset` pointer).
#[derive(Clone, Copy, Debug)]
pub struct FileSetKey {
    // C creator_pid; captured ONCE — MyProcPid is per-thread (virtual backend
    // pid) in the thread-native substrate, so lazy resolution would give each
    // participant a different directory.
    creator_pid: i32,
    number: u32,
    ntablespaces: usize,
    tablespaces: [Oid; FILESET_MAX_TABLESPACES],
    for_life: bool,
}

impl FileSetKey {
    // `FileSetPath`: <tempdir>/pgsql_tmp<creator_pid>.<set>.fileset
    fn dir_path(&self, tablespace: Oid) -> String {
        format!(
            "{}/{PG_TEMP_FILE_PREFIX}{}.{}.fileset",
            TempTablespacePath(tablespace),
            self.creator_pid,
            self.number
        )
    }

    // `ChooseTablespace` (fileset.c:186): hash_any over the file name.
    fn choose_tablespace(&self, name: &str) -> Oid {
        let hash = hashfn::hash_bytes(name.as_bytes());
        self.tablespaces[(hash as usize) % self.ntablespaces]
    }

    /// `FilePath` (fileset.c:200): the full path of a named file in the set.
    pub fn file_path(&self, name: &str) -> String {
        format!("{}/{name}", self.dir_path(self.choose_tablespace(name)))
    }

    /// `FileSetCreate` (fileset.c:98): create a new file in the set, making
    /// the per-tablespace directory on demand; errors on failure.
    pub fn create(&self, name: &str) -> PgResult<File> {
        let path = self.file_path(name);
        let mut file = PathNameCreateTemporaryFile(&path, false)?;
        if file.0 <= 0 {
            let tablespace = self.choose_tablespace(name);
            PathNameCreateTemporaryDir(&TempTablespacePath(tablespace), &self.dir_path(tablespace))?;
            file = PathNameCreateTemporaryFile(&path, true)?;
        }
        if self.for_life {
            crate::vfd::charge_for_life(file, true);
        }
        Ok(file)
    }

    /// `FileSetOpen` (fileset.c:123): File(<=0) when the file doesn't exist.
    pub fn open(&self, name: &str, mode: i32) -> PgResult<File> {
        let file = PathNameOpenTemporaryFile(&self.file_path(name), mode)?;
        if self.for_life {
            // PathNameOpenTemporaryFile no longer charges a reopen (fd.c:1922);
            // a writer reopening a file charged for its life (spillset's next
            // epoch) must be, so the file takes its charge back.
            if file.0 > 0 && mode & libc::O_RDWR != 0 {
                with_fd(|fd| fd.vfd_cache[file.0 as usize].fdstate |= FD_TEMP_FILE_LIMIT);
            }
            crate::vfd::charge_for_life(file, false);
        }
        Ok(file)
    }

    /// `FileSetDelete` (fileset.c:136): true if the file existed.
    pub fn delete(&self, name: &str, error_on_failure: bool) -> PgResult<bool> {
        PathNameDeleteTemporaryFile(&self.file_path(name), error_on_failure)
    }

    /// `FileSetDeleteAll` (fileset.c:163): remove the directory created in
    /// each tablespace. Used on cleanup paths: failures are logged, not
    /// raised (PathNameDeleteTemporaryDir), and a directory that was never
    /// created is silently skipped.
    pub fn delete_all(&self) -> PgResult<()> {
        for &tblspc in &self.tablespaces[..self.ntablespaces] {
            PathNameDeleteTemporaryDir(&self.dir_path(tblspc))?;
        }
        Ok(())
    }

    pub fn delete_files_with(&self, min_bytes: u64, fan: &dyn Fn(usize, &(dyn Fn(usize) + Sync))) -> PgResult<(usize, u64, bool)> {
        let mut all = (0, 0, false);
        for &tblspc in &self.tablespaces[..self.ntablespaces] {
            let (n, bytes, fanned) = PathNameDeleteTemporaryFilesWith(&self.dir_path(tblspc), min_bytes, fan)?;
            all = (all.0 + n, all.1 + bytes, all.2 || fanned);
        }
        Ok(all)
    }
}

pub struct FileSet {
    key: FileSetKey,
}

impl FileSet {
    /// `FileSetInit` (fileset.c:68): capture the temp tablespaces to spread
    /// files across so every participant agrees on them.
    pub fn init() -> PgResult<FileSet> {
        crate::buffile::PrepareTempTablespaces()?;
        let mut tablespaces = [InvalidOid; FILESET_MAX_TABLESPACES];
        let mut ntablespaces = GetTempTablespaces(&mut tablespaces).max(0) as usize;
        let my_tablespace = init_small::globals::MyDatabaseTableSpace();
        if ntablespaces == 0 {
            // If the GUC is empty, use current database's default tablespace.
            tablespaces[0] = my_tablespace;
            ntablespaces = 1;
        } else {
            // An entry of InvalidOid means use the default tablespace for the
            // current database. Replace that now, to be sure that all users
            // of the FileSet agree on what to do.
            for t in &mut tablespaces[..ntablespaces] {
                if *t == InvalidOid {
                    *t = my_tablespace;
                }
            }
        }
        Ok(FileSet {
            key: FileSetKey {
                creator_pid: init_small::globals::MyProcPid(),
                number: FILESET_NUMBER
                    .fetch_update(Relaxed, Relaxed, |n| Some((n + 1) % (i32::MAX as u32)))
                    .expect("fetch_update closure never fails"),
                ntablespaces,
                tablespaces,
                for_life: false,
            },
        })
    }

    pub fn init_charged_for_life() -> PgResult<FileSet> {
        let mut fs = FileSet::init()?;
        fs.key.for_life = true;
        Ok(fs)
    }

    /// The set's identity, for BufFiles and other name-keyed users.
    pub fn key(&self) -> FileSetKey {
        self.key
    }

    /// `FilePath` for a named file (see [`FileSetKey::file_path`]).
    pub fn file_path(&self, name: &str) -> String {
        self.key.file_path(name)
    }

    /// `FileSetCreate` (see [`FileSetKey::create`]).
    pub fn create(&self, name: &str) -> PgResult<File> {
        self.key.create(name)
    }

    /// `FileSetOpen` (see [`FileSetKey::open`]).
    pub fn open(&self, name: &str, mode: i32) -> PgResult<File> {
        self.key.open(name, mode)
    }

    /// `FileSetDelete` (see [`FileSetKey::delete`]).
    pub fn delete(&self, name: &str, error_on_failure: bool) -> PgResult<bool> {
        self.key.delete(name, error_on_failure)
    }

    /// `FileSetDeleteAll`.
    pub fn delete_all(&self) -> PgResult<()> {
        self.key.delete_all()
    }

    pub fn delete_files_with(&self, min_bytes: u64, fan: &dyn Fn(usize, &(dyn Fn(usize) + Sync))) -> PgResult<(usize, u64, bool)> {
        if !crate::vfd::fd_tls_alive() {
            return Ok((0, 0, false));
        }
        let _held = init_small::globals::hold_interrupts();
        self.key.delete_files_with(min_bytes, fan)
    }

    // ---- pgrust-only base-name placement (sqe_spill's segment scheme) ----
    //
    // sqe_spill names its own segments "<name_path>.<seg>" and keeps every
    // segment of a name in one directory. That layout has no C counterpart;
    // it is hashed on the BASE name so all segments share a tablespace.

    /// Path prefix for a base name; sqe_spill's segment i lives at
    /// "<prefix>.<i>".
    pub fn name_path(&self, name: &str) -> String {
        self.key.file_path(name)
    }

    /// Create one sqe_spill segment at `path` (from [`FileSet::name_path`]).
    pub fn create_seg(&self, name: &str, path: &str) -> PgResult<File> {
        let file = PathNameCreateTemporaryFile(path, false)?;
        if file.0 > 0 {
            return Ok(file);
        }
        // The directories may not exist yet; create them and retry loudly.
        let tblspc = self.key.choose_tablespace(name);
        PathNameCreateTemporaryDir(&TempTablespacePath(tblspc), &self.key.dir_path(tblspc))?;
        PathNameCreateTemporaryFile(path, true)
    }

    /// Open one sqe_spill segment: File(<=0) when it doesn't exist. A
    /// writer's per-event reopen is charged against temp_file_limit like the
    /// creating VFD was (pgrust-only; C's PathNameOpenTemporaryFile never
    /// charges a reopen).
    pub fn open_seg(&self, path: &str, mode: i32) -> PgResult<File> {
        let file = PathNameOpenTemporaryFile(path, mode)?;
        if file.0 > 0 && mode & libc::O_RDWR != 0 {
            with_fd(|fd| fd.vfd_cache[file.0 as usize].fdstate |= FD_TEMP_FILE_LIMIT);
        }
        Ok(file)
    }
}

impl Drop for FileSet {
    // tls-dtor: try_with-safe — a TLS-parked SpillSet/FileSet can be dropped
    // during TLS teardown, where delete_all's dir walk (AllocateDir -> FD.with)
    // aborts. Skip then; the startup pgsql_tmp reaper removes the leak.
    // the walk runs with interrupts held. Its CHECK_FOR_INTERRUPTS
    // (walkdir, the only one in fd.c) would otherwise raise a pending cancel,
    // statement_timeout or terminate into this `let _`: ProcessInterrupts
    // clears the flags before it raises, so the interrupt was lost (the
    // statement ran on; a terminated backend lived). Held, the walk always
    // completes and the interrupt stays pending for the caller's next check.
    fn drop(&mut self) {
        if crate::vfd::fd_tls_alive() {
            // dsm.c:813: the last-detach cleanup runs with interrupts held so
            // a cancel cannot abandon the remaining files.
            let _held = init_small::globals::hold_interrupts();
            let _ = self.delete_all();
        }
    }
}
