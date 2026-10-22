//! `pg_pgrc_spool/<16-hex LSN>` is a hard link to the file referenced by the `FILE_REF` record starting at that LSN.
//! It is made before the reference is logged and removed at the checkpoint that moves WAL retention past it.

use std::ffi::CString;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};

use pgrc_format::spool::{parse_pending_name, parse_spool_name, pending_name, spool_name, SPOOL_DIR};
use pgrc_format::wal::DirLoc;
use pgsync::{lock, Condvar, Mutex};

pub type Lsn = u64;

fn cstr(p: &str) -> io::Result<CString> {
    CString::new(p).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
}

fn last_err() -> io::Error {
    io::Error::from_raw_os_error(vfs::get_errno())
}

fn check(rc: libc::c_int) -> io::Result<()> {
    if rc < 0 {
        return Err(last_err());
    }
    Ok(())
}

fn fsync_path(p: &str) -> io::Result<()> {
    let c = cstr(p)?;
    let fd = vfs::open(&c, libc::O_RDONLY, 0);
    if fd < 0 {
        return Err(last_err());
    }
    let r = loop {
        if vfs::fsync(fd) >= 0 {
            break Ok(());
        }
        let e = last_err();
        if e.raw_os_error() != Some(libc::EINTR) {
            break Err(e);
        }
    };
    vfs::close(fd);
    r
}

fn file_size(p: &str) -> io::Result<u64> {
    let mut info = vfs::FileInfo::zeroed();
    check(vfs::stat(&cstr(p)?, &mut info))?;
    Ok(info.size.max(0) as u64)
}

fn is_enoent(e: &io::Error) -> bool {
    e.raw_os_error() == Some(libc::ENOENT)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spool {
    dir: String,
    parent: String,
}

impl Spool {
    pub fn data() -> Spool {
        Spool { dir: SPOOL_DIR.to_string(), parent: ".".to_string() }
    }

    pub fn at(dir: &str) -> Spool {
        let parent = match dir.rsplit_once('/') {
            Some((p, _)) if !p.is_empty() => p.to_string(),
            _ => ".".to_string(),
        };
        Spool { dir: dir.to_string(), parent }
    }

    pub fn dir(&self) -> &str {
        &self.dir
    }

    pub fn path(&self, lsn: Lsn) -> String {
        format!("{}/{}", self.dir, spool_name(lsn))
    }

    pub fn pending_path(&self, dir: DirLoc, name: &str) -> String {
        format!("{}/{}", self.dir, pending_name(dir, name))
    }

    pub fn ensure_dir(&self) -> io::Result<()> {
        let c = cstr(&self.dir)?;
        let mut info = vfs::FileInfo::zeroed();
        if vfs::stat(&c, &mut info) == 0 {
            return Ok(());
        }
        if vfs::mkdir(&c, 0o700) < 0 {
            let e = last_err();
            if e.raw_os_error() != Some(libc::EEXIST) {
                return Err(e);
            }
        }
        fsync_path(&self.dir)?;
        fsync_path(&self.parent)
    }

    pub fn fsync_dir(&self) -> io::Result<()> {
        fsync_path(&self.dir)
    }

    /// Link the complete, fsynced table file `table_path` into the spool as its pending entry and fsync the spool,
    /// so a crash after the reference is logged always finds a source.
    pub fn link_pending(&self, table_path: &str, dir: DirLoc, name: &str) -> io::Result<String> {
        self.ensure_dir()?;
        let pending = self.pending_path(dir, name);
        let (from, to) = (cstr(table_path)?, cstr(&pending)?);
        if vfs::link(&from, &to) < 0 {
            let e = last_err();
            if e.raw_os_error() != Some(libc::EEXIST) {
                return Err(e);
            }
            check(vfs::unlink(&to))?;
            check(vfs::link(&from, &to))?;
        }
        self.fsync_dir()?;
        Ok(pending)
    }

    /// Rename a pending entry to the LSN of its reference. The caller registers the spool's sync.
    pub fn name_final(&self, pending: &str, lsn: Lsn) -> io::Result<()> {
        check(vfs::rename(&cstr(pending)?, &cstr(&self.path(lsn))?))?;
        self.account(lsn);
        Ok(())
    }

    fn account(&self, lsn: Lsn) {
        if self.dir == SPOOL_DIR {
            if let Ok(n) = file_size(&self.path(lsn)) {
                lock(&LEDGER).add(lsn, n);
            }
        }
    }

    /// Give the table file `table_path` a spool entry at `lsn` if it has none. The caller registers the spool's sync.
    pub fn link_final(&self, table_path: &str, lsn: Lsn) -> io::Result<()> {
        self.ensure_dir()?;
        if vfs::link(&cstr(table_path)?, &cstr(&self.path(lsn))?) < 0 {
            let e = last_err();
            if e.raw_os_error() != Some(libc::EEXIST) {
                return Err(e);
            }
        }
        self.account(lsn);
        Ok(())
    }

    pub fn list(&self, from: Lsn, to: Lsn) -> io::Result<Vec<(Lsn, u64)>> {
        let mut out = Vec::new();
        for (lsn, name) in self.entries()? {
            if lsn >= from && lsn < to {
                match file_size(&format!("{}/{name}", self.dir)) {
                    Ok(n) => out.push((lsn, n)),
                    Err(e) if is_enoent(&e) => {}
                    Err(e) => return Err(e),
                }
            }
        }
        out.sort_unstable();
        Ok(out)
    }

    fn entries(&self) -> io::Result<Vec<(Lsn, String)>> {
        Ok(self.names()?.into_iter().filter_map(|n| parse_spool_name(&n).map(|l| (l, n))).collect())
    }

    fn names(&self) -> io::Result<Vec<String>> {
        let it = match vfs::read_dir(&cstr(&self.dir)?) {
            Ok(it) => it,
            Err(e) if e == libc::ENOENT => return Ok(Vec::new()),
            Err(e) => return Err(io::Error::from_raw_os_error(e)),
        };
        let mut v = Vec::new();
        for n in it {
            v.push(String::from_utf8_lossy(n.map_err(io::Error::from_raw_os_error)?.as_bytes()).into_owned());
        }
        v.sort();
        Ok(v)
    }

    fn remove(&self, names: &[String]) -> io::Result<(u32, u64)> {
        let (mut files, mut bytes) = (0u32, 0u64);
        for n in names {
            let p = format!("{}/{n}", self.dir);
            let size = file_size(&p).unwrap_or(0);
            match check(vfs::unlink(&cstr(&p)?)) {
                Ok(()) => {
                    files += 1;
                    bytes += size;
                }
                Err(e) if is_enoent(&e) => {}
                Err(e) => return Err(e),
            }
        }
        if files > 0 {
            self.fsync_dir()?;
        }
        Ok((files, bytes))
    }

    pub fn remove_below(&self, cutoff: Lsn) -> io::Result<(u32, u64)> {
        let old: Vec<String> = self.entries()?.into_iter().filter(|(l, _)| *l < cutoff).map(|(_, n)| n).collect();
        let r = self.remove(&old)?;
        if self.dir == SPOOL_DIR {
            lock(&LEDGER).remove_below(cutoff);
        }
        Ok(r)
    }

    /// End of recovery: pending entries and entries at or past `end_of_wal` (a discarded WAL tail).
    /// `end_of_wal == 0` (unknown) removes pending entries only.
    pub fn remove_debris(&self, end_of_wal: Lsn) -> io::Result<u32> {
        let debris: Vec<String> = self
            .names()?
            .into_iter()
            .filter(|n| {
                parse_pending_name(n).is_some()
                    || n.ends_with(RECV_SUFFIX)
                    || (end_of_wal != 0 && parse_spool_name(n).is_some_and(|l| l >= end_of_wal))
            })
            .collect();
        let n = self.remove(&debris)?.0;
        if self.dir == SPOOL_DIR && end_of_wal != 0 {
            lock(&LEDGER).remove_from(end_of_wal);
        }
        Ok(n)
    }

    pub fn recv_path(&self, lsn: Lsn) -> String {
        format!("{}/{}{RECV_SUFFIX}", self.dir, spool_name(lsn))
    }

    pub fn open_entry(&self, lsn: Lsn) -> io::Result<EntryReader> {
        let fd = vfs::open(&cstr(&self.path(lsn))?, libc::O_RDONLY, 0);
        if fd < 0 {
            return Err(last_err());
        }
        Ok(EntryReader { fd })
    }

    pub fn land_begin(&self, lsn: Lsn) -> io::Result<Landing> {
        self.ensure_dir()?;
        let path = self.recv_path(lsn);
        let fd = vfs::open(&cstr(&path)?, libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC, 0o600);
        if fd < 0 {
            return Err(last_err());
        }
        Ok(Landing { spool: self.clone(), lsn, fd, path })
    }

    pub fn bytes_held(&self) -> io::Result<u64> {
        Ok(self.list(0, u64::MAX)?.iter().map(|(_, n)| n).sum())
    }

    /// Whether a file in `table_dir` can be linked into the spool: both on one file system.
    pub fn same_device(&self, table_dir: &str) -> bool {
        let dev = |p: &str| -> Option<u64> {
            let mut info = vfs::FileInfo::zeroed();
            (vfs::stat(&cstr(p).ok()?, &mut info) == 0).then_some(info.dev)
        };
        let spool = dev(&self.dir).or_else(|| dev(&self.parent));
        matches!((spool, dev(table_dir)), (Some(a), Some(b)) if a == b)
    }
}

const RECV_SUFFIX: &str = ".recv";

#[derive(Debug)]
pub struct EntryReader {
    fd: libc::c_int,
}

impl EntryReader {
    /// Fill `buf` from `off`; fewer bytes only at end of file.
    pub fn read_at(&mut self, off: u64, buf: &mut [u8]) -> io::Result<usize> {
        let mut got = 0;
        while got < buf.len() {
            let n = vfs::pread(self.fd, &mut buf[got..], (off + got as u64) as libc::off_t);
            if n < 0 {
                let e = last_err();
                if e.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(e);
            }
            if n == 0 {
                break;
            }
            got += n as usize;
        }
        Ok(got)
    }
}

impl Drop for EntryReader {
    fn drop(&mut self) {
        vfs::close(self.fd);
    }
}

/// A streamed file being received. [`Landing::finish`] makes it durable under its entry name; dropping it early
/// leaves a `.recv` that the next receipt truncates and end-of-recovery debris removal takes.
#[derive(Debug)]
pub struct Landing {
    spool: Spool,
    lsn: Lsn,
    fd: libc::c_int,
    path: String,
}

impl Landing {
    pub fn lsn(&self) -> Lsn {
        self.lsn
    }

    pub fn write_at(&mut self, off: u64, bytes: &[u8]) -> io::Result<()> {
        let mut put = 0;
        while put < bytes.len() {
            let n = vfs::pwrite(self.fd, &bytes[put..], (off + put as u64) as libc::off_t);
            if n < 0 {
                let e = last_err();
                if e.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(e);
            }
            put += n as usize;
        }
        Ok(())
    }

    /// fdatasync, rename to the entry name, fsync the spool, and name it in the registry.
    pub fn finish(self) -> io::Result<()> {
        loop {
            if vfs::fdatasync(self.fd) >= 0 {
                break;
            }
            let e = last_err();
            if e.raw_os_error() != Some(libc::EINTR) {
                return Err(e);
            }
        }
        check(vfs::rename(&cstr(&self.path)?, &cstr(&self.spool.path(self.lsn))?))?;
        self.spool.fsync_dir()?;
        self.spool.account(self.lsn);
        note_named(self.lsn);
        Ok(())
    }
}

impl Drop for Landing {
    fn drop(&mut self) {
        vfs::close(self.fd);
    }
}


pgsync::process_global! {
    static SAME_DEVICE: Mutex<Vec<(String, bool)>> = Mutex::new(Vec::new());
}

fn tablespace_of(table_dir: &str) -> String {
    let mut it = table_dir.split('/');
    match (it.next(), it.next()) {
        (Some("pg_tblspc"), Some(oid)) => format!("pg_tblspc/{oid}"),
        (Some(first), _) => first.to_string(),
        _ => String::new(),
    }
}

pub fn same_device(table_dir: &str) -> bool {
    let key = tablespace_of(table_dir);
    if let Some(&(_, v)) = lock(&SAME_DEVICE).iter().find(|(k, _)| *k == key) {
        return v;
    }
    let v = Spool::data().same_device(table_dir);
    lock(&SAME_DEVICE).push((key, v));
    v
}

pub fn forget_devices() {
    lock(&SAME_DEVICE).clear();
}


#[derive(Debug)]
#[must_use = "every begun reference must be ended"]
pub struct RefTicket(u64);

#[derive(Debug, Default)]
struct InFlight {
    next: u64,
    open: std::collections::BTreeSet<u64>,
    /// Low-level backups running; while any is, writers log bytes, not references.
    lowlevel_backups: u32,
}

pub struct Registry {
    state: Mutex<InFlight>,
    done: Condvar,
    latest: AtomicU64,
}

impl Registry {
    pub const fn new() -> Registry {
        Registry {
            state: Mutex::new(InFlight { next: 0, open: std::collections::BTreeSet::new(), lowlevel_backups: 0 }),
            done: Condvar::new(),
            latest: AtomicU64::new(0),
        }
    }

    /// Taken before the `FILE_REF` insert.
    pub fn begin_ref(&self) -> RefTicket {
        let mut g = lock(&self.state);
        g.next += 1;
        let t = g.next;
        g.open.insert(t);
        RefTicket(t)
    }

    /// The check and the ticket are taken under one lock, and [`Registry::lowlevel_backup_begin`] counts the backup
    /// and then waits for every ticket taken before it, so a reference is either logged before the backup's
    /// checkpoint or not at all.
    pub fn begin_ref_unless_backup(&self) -> Option<RefTicket> {
        let mut g = lock(&self.state);
        if g.lowlevel_backups > 0 {
            return None;
        }
        g.next += 1;
        let t = g.next;
        g.open.insert(t);
        Some(RefTicket(t))
    }

    /// Call before its checkpoint. Returns once every reference begun before it
    /// is logged or failed; references begun after it are bytes until [`Registry::lowlevel_backup_end`].
    pub fn lowlevel_backup_begin(&self) {
        let mut g = lock(&self.state);
        g.lowlevel_backups += 1;
        let upto = g.next;
        while g.open.first().is_some_and(|&t| t <= upto) {
            g = self.done.wait(g).unwrap_or_else(|e| e.into_inner());
        }
    }

    pub fn lowlevel_backup_end(&self) {
        let mut g = lock(&self.state);
        g.lowlevel_backups = g.lowlevel_backups.saturating_sub(1);
    }

    pub fn lowlevel_backups(&self) -> u32 {
        lock(&self.state).lowlevel_backups
    }

    /// `None`: the insert or the rename failed.
    pub fn end_ref(&self, t: RefTicket, lsn: Option<Lsn>) {
        if let Some(l) = lsn {
            self.latest.fetch_max(l, Ordering::AcqRel);
        }
        let mut g = lock(&self.state);
        g.open.remove(&t.0);
        drop(g);
        self.done.notify_all();
    }

    /// Returns once every reference begun before the call is named (or failed). Read the WAL flush pointer FIRST:
    /// a reference below it was inserted, so it began before this call.
    pub fn settle(&self) {
        let mut g = lock(&self.state);
        let upto = g.next;
        while g.open.first().is_some_and(|&t| t <= upto) {
            g = self.done.wait(g).unwrap_or_else(|e| e.into_inner());
        }
    }

    pub fn note_named(&self, lsn: Lsn) {
        self.latest.fetch_max(lsn, Ordering::AcqRel);
    }

    pub fn latest_ref_lsn(&self) -> Lsn {
        self.latest.load(Ordering::Acquire)
    }

    #[cfg(test)]
    fn in_flight(&self) -> usize {
        lock(&self.state).open.len()
    }
}

impl Default for Registry {
    fn default() -> Registry {
        Registry::new()
    }
}

pgsync::process_global! {
    static REGISTRY: Registry = Registry::new();
}

pub fn begin_ref() -> RefTicket {
    REGISTRY.begin_ref()
}

pub fn end_ref(t: RefTicket, lsn: Option<Lsn>) {
    REGISTRY.end_ref(t, lsn)
}

pub fn begin_ref_unless_backup() -> Option<RefTicket> {
    REGISTRY.begin_ref_unless_backup()
}

pub fn lowlevel_backup_begin() {
    REGISTRY.lowlevel_backup_begin()
}

pub fn lowlevel_backup_end() {
    REGISTRY.lowlevel_backup_end()
}

pub fn lowlevel_backups() -> u32 {
    REGISTRY.lowlevel_backups()
}

pub fn settle() {
    REGISTRY.settle()
}

pub fn note_named(lsn: Lsn) {
    REGISTRY.note_named(lsn)
}

pub fn latest_ref_lsn() -> Lsn {
    REGISTRY.latest_ref_lsn()
}


/// The spool's entries as `(lsn, running byte sum through the entry)`, sorted, so WAL retention counts in O(log n).
#[derive(Debug, Default)]
pub struct Ledger {
    entries: std::collections::VecDeque<(Lsn, u64)>,
    base: u64,
    /// One named below it later (a race with removal) is not counted.
    cut: Lsn,
    /// The newest removed entry + 1: a slot that needs a point below it has lost a file.
    removed_upto: Lsn,
    seeded: bool,
}

impl Ledger {
    pub const fn new() -> Ledger {
        Ledger { entries: std::collections::VecDeque::new(), base: 0, cut: 0, removed_upto: 0, seeded: false }
    }

    fn index(&self, lsn: Lsn) -> usize {
        self.entries.partition_point(|&(l, _)| l < lsn)
    }

    fn sum_before(&self, i: usize) -> u64 {
        if i == 0 { self.base } else { self.entries[i - 1].1 }
    }

    fn sum_all(&self) -> u64 {
        self.entries.back().map_or(self.base, |e| e.1)
    }

    pub fn add(&mut self, lsn: Lsn, bytes: u64) {
        let i = self.index(lsn);
        if lsn < self.cut || self.entries.get(i).is_some_and(|&(l, _)| l == lsn) {
            return;
        }
        self.entries.insert(i, (lsn, self.sum_before(i) + bytes));
        self.entries.iter_mut().skip(i + 1).for_each(|e| e.1 += bytes);
    }

    pub fn remove_below(&mut self, cutoff: Lsn) {
        let i = self.index(cutoff);
        if i > 0 {
            self.removed_upto = self.removed_upto.max(self.entries[i - 1].0 + 1);
            self.base = self.entries[i - 1].1;
            self.entries.drain(..i);
        }
        self.cut = self.cut.max(cutoff);
    }

    pub fn remove_from(&mut self, from: Lsn) {
        let i = self.index(from);
        self.entries.truncate(i);
    }

    pub fn bytes_from(&self, lsn: Lsn) -> u64 {
        self.sum_all() - self.sum_before(self.index(lsn))
    }

    pub fn total(&self) -> u64 {
        self.sum_all() - self.base
    }

    /// The lowest point at or past `lo` from which the entries hold at most `budget` bytes.
    pub fn cut_to_budget(&self, lo: Lsn, budget: u64) -> Lsn {
        let all = self.sum_all();
        if all - self.sum_before(self.index(lo)) <= budget {
            return lo;
        }
        let j = self.entries.partition_point(|&(_, s)| all - s > budget);
        self.entries[j].0 + 1
    }

    /// The newest entry in `[lo, hi)` from which the entries hold at least `need` bytes.
    pub fn keep_for(&self, lo: Lsn, hi: Lsn, need: u64) -> Option<Lsn> {
        let all = self.sum_all();
        let k = self.entries.partition_point(|&(_, s)| all - s >= need);
        let top = self.index(hi).checked_sub(1)?;
        let j = k.min(top);
        (j >= self.index(lo) && all - self.sum_before(j) >= need).then(|| self.entries[j].0)
    }

    pub fn last_below(&self, lsn: Lsn) -> Option<Lsn> {
        self.index(lsn).checked_sub(1).map(|i| self.entries[i].0)
    }

    pub fn removed_upto(&self) -> Lsn {
        self.removed_upto
    }
}

pgsync::process_global! {
    static LEDGER: Mutex<Ledger> = Mutex::new(Ledger::new());
}

pub fn with_ledger<R>(f: impl FnOnce(&Ledger) -> R) -> R {
    if !lock(&LEDGER).seeded {
        if let Ok(listed) = Spool::data().list(0, u64::MAX) {
            let mut g = lock(&LEDGER);
            listed.into_iter().for_each(|(lsn, n)| g.add(lsn, n));
            g.seeded = true;
        }
    }
    f(&lock(&LEDGER))
}


/// A stock client streaming the backup's WAL gets no files; the restore replays that WAL. So once the backup has
/// listed the spool, the stream is held at the first reference at or past the cover.
#[derive(Debug)]
pub struct BackupCover {
    seg_start: Lsn,
    cover: AtomicU64,
}

const COVER_OPEN: Lsn = u64::MAX;
/// The backup is listing: an attached stream sends no reference until the cover is known.
const COVER_LISTING: Lsn = u64::MAX - 1;
const BACKUPS_KEPT: usize = 16;

impl BackupCover {
    /// Before the backup reads the bound of its listing: a stream's flush pointer read before this call is below it.
    pub fn begin_listing(&self) {
        self.cover.store(COVER_LISTING, Ordering::SeqCst);
    }

    pub fn set_cover(&self, cover: Lsn) {
        self.cover.store(cover.min(COVER_LISTING - 1), Ordering::SeqCst);
    }

    pub fn abort(&self) {
        self.cover.store(COVER_OPEN, Ordering::SeqCst);
    }

    /// `None` while the backup lists; else the first LSN whose reference an attached stream must not send
    /// (`u64::MAX`: none).
    pub fn limit(&self) -> Option<Lsn> {
        match self.cover.load(Ordering::SeqCst) {
            COVER_LISTING => None,
            c => Some(c),
        }
    }
}

pgsync::process_global! {
    static BACKUPS: Mutex<Vec<std::sync::Arc<BackupCover>>> = Mutex::new(Vec::new());
}

pub fn backup_begin(start: Lsn, seg_size: u64) -> std::sync::Arc<BackupCover> {
    let b = std::sync::Arc::new(BackupCover { seg_start: start - start % seg_size, cover: AtomicU64::new(COVER_OPEN) });
    let mut all = lock(&BACKUPS);
    if all.len() >= BACKUPS_KEPT {
        all.remove(0);
    }
    all.push(b.clone());
    b
}

pub fn backup_stream(start: Lsn) -> Option<std::sync::Arc<BackupCover>> {
    lock(&BACKUPS).iter().rev().find(|b| b.seg_start == start).cloned()
}

#[cfg(test)]
mod tests;
