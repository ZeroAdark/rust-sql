//! The in-memory WAL the crash batteries run against ([`RecordingSink`], [`WalLog`], its checkpointer [`Clock`], the
//! [`Teeth`] a battery removes): test support, shipped in the lib like [`crate::testkit`] for three crates' batteries.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use pgrc_format::spool::{parse_pending_name, parse_spool_name, pending_name, spool_name};
use pgrc_format::wal::{DirLoc, FileRefRec, InstallRec, PublishRec, ReclaimRec, SmallMasks};
use pgrc_format::wire::crc32c;
use pgsync::{lock, Mutex};

use crate::plan::{self, ApplyMode, RefSource};
use crate::walsink::{Apply, FileRef, RefSync, WalSink};
use crate::wvfs::WriteVfs;
use crate::{WriteError, WriteResult};

/// A record of the in-memory WAL a [`RecordingSink`] keeps; `dir` is the table directory path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalRecord {
    File { dir: String, name: String, off: u64, bytes: Vec<u8>, create: bool },
    Publish { dir: String, rec: PublishRec },
    Install { dir: String, rec: InstallRec },
    Reclaim { dir: String, rec: ReclaimRec },
    Commit { fxid: u64 },
    /// `FILE_REF` (WR); its LSN is its index in [`WalLog::records`].
    FileRef { dir: String, name: String, len: u64, crc: u32, masks: SmallMasks },
}

/// The model's stand-in for a table directory's `DirLoc` (the records carry paths): pending spool names need one.
pub fn model_loc(dir: &str) -> DirLoc {
    DirLoc { spc: 0, db: 0, relnumber: crc32c(dir.as_bytes()) }
}

/// The model spool (WR): `dir` holds one hard link per logged reference, named by the record's index
/// (`spool_name`), plus `pending-*` links made before the insert. Entries go at a checkpoint below both the
/// checkpoint's redo and the slowest consumer (`slot`), as the server drops them with their WAL segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpoolModel {
    pub dir: String,
    /// Files at least this long go by reference.
    pub ref_min: u64,
    /// The oldest record a WAL consumer (a slot) still needs; `None`: no consumer.
    pub slot: Option<usize>,
}

impl SpoolModel {
    pub fn entry(&self, lsn: usize) -> String {
        format!("{}/{}", self.dir, spool_name(lsn as u64))
    }
    pub fn pending(&self, dir: &str, name: &str) -> String {
        format!("{}/{}", self.dir, pending_name(model_loc(dir), name))
    }
}

/// Protocol steps a crash battery removes to show it notices them missing.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Teeth {
    pub skip_delay_chkpt: bool,
    pub skip_sync_registration: bool,
    pub skip_redo_check: bool,
    /// WR T-ref-fsync: log a reference before its file is fsynced.
    pub skip_ref_fsync: bool,
    /// WR T-pending-durable: skip the spool directory's fsync after the pending link.
    pub skip_pending_fsync: bool,
    /// WR T-spool-early: drop spool entries below the checkpoint's redo even when a consumer is behind them.
    pub skip_spool_hold: bool,
}

/// Called at every [`RecordingSink`] event boundary: the model's checkpointer.
pub trait Clock: Send {
    fn tick(&mut self, log: &mut WalLog);
}

/// The in-memory WAL plus the checkpointer state it needs: registered names, the redo pointer, flushed records.
#[derive(Default)]
pub struct WalLog {
    pub records: Vec<WalRecord>,
    /// Per directory, the names registered for the next checkpoint's sync pass.
    pub dirty: BTreeMap<String, BTreeSet<String>>,
    /// The newest begun checkpoint's redo pointer (an index into `records`).
    pub redo: usize,
    /// The newest completed checkpoint's redo pointer: where replay starts.
    pub replay_from: usize,
    /// Records flushed by a commit or a completed checkpoint.
    pub flushed: usize,
    pub delaying: u32,
    pub teeth: Teeth,
    /// Files the sink fsynced itself because a checkpoint began after their first write.
    pub self_synced: Vec<String>,
    first_write: BTreeMap<(String, String), usize>,
    clock: Option<Box<dyn Clock>>,
    /// Parts by reference (WR): a [`RecordingSink`] logs files of at least `ref_min` bytes by reference. The
    /// spool directory must exist in the universe.
    pub spool: Option<SpoolModel>,
}

pub type SharedLog = Arc<Mutex<WalLog>>;

impl WalLog {
    pub fn shared() -> SharedLog {
        Arc::new(Mutex::new(WalLog::default()))
    }

    /// A copy without the clock, for batteries that fork a universe.
    pub fn fork(&self) -> WalLog {
        WalLog {
            records: self.records.clone(),
            dirty: self.dirty.clone(),
            redo: self.redo,
            replay_from: self.replay_from,
            flushed: self.flushed,
            delaying: self.delaying,
            teeth: self.teeth,
            self_synced: self.self_synced.clone(),
            first_write: self.first_write.clone(),
            clock: None,
            spool: self.spool.clone(),
        }
    }

    pub fn set_clock(&mut self, clock: Box<dyn Clock>) {
        self.clock = Some(clock);
    }

    pub fn tick(&mut self) {
        if let Some(mut c) = self.clock.take() {
            c.tick(self);
            self.clock = Some(c);
        }
    }

    pub fn commit(&mut self, fxid: u64) {
        self.tick();
        self.records.push(WalRecord::Commit { fxid });
        self.flushed = self.records.len();
        self.tick();
    }

    pub fn begin_checkpoint(&mut self) {
        self.redo = self.records.len();
    }

    /// The sync pass: fsync every registered name that still exists, then its directory; replay now starts at
    /// the redo pointer, and the checkpoint record flushes everything inserted so far.
    pub fn complete_checkpoint(&mut self, vfs: &mut dyn WriteVfs) -> WriteResult<()> {
        for (dir, names) in std::mem::take(&mut self.dirty) {
            for n in names {
                let path = format!("{dir}/{n}");
                if vfs.exists_path(&path)? {
                    let fd = vfs.open_rw(&path)?;
                    vfs.fsync_file(&fd)?;
                    vfs.close_file(fd)?;
                }
            }
            match vfs.fsync_dir(&dir) {
                Err(WriteError::Io { errno, .. }) if errno == libc::ENOENT => {}
                r => r?,
            }
        }
        self.replay_from = self.redo;
        self.flushed = self.records.len();
        self.remove_old_spool(vfs)
    }

    /// The checkpoint's WAL segment removal, for the spool: entries below the new redo pointer and the slot go.
    fn remove_old_spool(&mut self, vfs: &mut dyn WriteVfs) -> WriteResult<()> {
        let Some(sp) = &self.spool else { return Ok(()) };
        let cutoff = match sp.slot {
            Some(slot) if !self.teeth.skip_spool_hold => slot.min(self.replay_from),
            _ => self.replay_from,
        };
        let mut removed = false;
        for n in vfs.list_dir(&sp.dir)? {
            if parse_spool_name(&n).is_some_and(|lsn| (lsn as usize) < cutoff) {
                vfs.unlink_path(&format!("{}/{n}", sp.dir))?;
                removed = true;
            }
        }
        if removed {
            vfs.fsync_dir(&sp.dir)?;
        }
        Ok(())
    }

    /// Every reference in `records[from..upto]` has a spool entry (or its pending link) holding its bytes: what a
    /// WAL consumer at `from` reads. The error names the first reference without one.
    pub fn check_spool(&self, vfs: &mut dyn WriteVfs, from: usize, upto: usize) -> Result<(), String> {
        let Some(sp) = &self.spool else { return Ok(()) };
        for (lsn, r) in self.records.iter().enumerate().take(upto).skip(from) {
            let WalRecord::FileRef { dir, name, len, crc, masks } = r else { continue };
            let rec = FileRefRec { dir: model_loc(dir), name, len: *len, crc: *crc, masks: *masks };
            let mut found = false;
            for p in [sp.entry(lsn), sp.pending(dir, name)] {
                if plan::verify_ref(vfs, &p, &rec).map_err(|e| e.to_string())? {
                    found = true;
                    break;
                }
            }
            if !found {
                return Err(format!("reference without bytes: FILE_REF {name} at {lsn} has no spool entry that verifies"));
            }
        }
        Ok(())
    }

    /// End of recovery (WR): pending links whose reference never reached WAL, and entries of references past the
    /// end of WAL, are debris.
    pub fn remove_spool_debris(&mut self, vfs: &mut dyn WriteVfs, end: usize) -> WriteResult<()> {
        let Some(sp) = &self.spool else { return Ok(()) };
        let dir = sp.dir.clone();
        let mut removed = false;
        for n in vfs.list_dir(&dir)? {
            let debris = parse_pending_name(&n).is_some() || parse_spool_name(&n).is_some_and(|lsn| lsn as usize >= end);
            if debris {
                vfs.unlink_path(&format!("{dir}/{n}"))?;
                removed = true;
            }
        }
        if removed {
            vfs.fsync_dir(&dir)?;
        }
        Ok(())
    }

    /// Redo of a `FILE_REF` at `lsn`: the spool entry, else the pending link (renamed to the entry), else the table
    /// file; then the entry is made to exist here too. Returns the touched names per directory.
    fn replay_file_ref(
        &mut self,
        vfs: &mut dyn WriteVfs,
        lsn: usize,
        dir: &str,
        rec: &FileRefRec<'_>,
    ) -> WriteResult<Vec<(String, Vec<String>)>> {
        let sp = self.spool.clone().ok_or(WriteError::Contract { detail: "FILE_REF replay without a model spool" })?;
        let entry = sp.entry(lsn);
        let pending = sp.pending(dir, rec.name);
        if !vfs.exists_path(&entry)? && vfs.exists_path(&pending)? {
            vfs.rename_path(&pending, &entry)?;
        }
        let src = if vfs.exists_path(&entry)? { RefSource::Spool(entry.clone()) } else { RefSource::InPlace };
        plan::apply_file_ref(vfs, dir, rec, src)?;
        if !vfs.exists_path(&entry)? {
            vfs.link_path(&format!("{dir}/{}", rec.name), &entry)?;
        }
        Ok(vec![(dir.to_string(), vec![rec.name.to_string()]), (sp.dir.clone(), vec![spool_name(lsn as u64)])])
    }

    pub fn checkpoint(&mut self, vfs: &mut dyn WriteVfs) -> WriteResult<()> {
        self.begin_checkpoint();
        self.complete_checkpoint(vfs)
    }

    /// kill -9: records past `upto` were never flushed, and the checkpointer's queue and any checkpoint in progress
    /// are gone.
    pub fn crash(&mut self, upto: usize) {
        self.records.truncate(upto);
        self.flushed = self.flushed.min(upto);
        self.redo = self.replay_from;
        self.delaying = 0;
        self.dirty.clear();
        self.first_write.clear();
    }

    pub fn committed(&self, upto: usize) -> BTreeSet<u64> {
        self.records[..upto]
            .iter()
            .filter_map(|r| match r {
                WalRecord::Commit { fxid } => Some(*fxid),
                _ => None,
            })
            .collect()
    }

    /// Redo `records[replay_from..upto]` through the functions the real redo uses, registering what each record
    /// touched for the end-of-recovery checkpoint. Returns the publishes skipped because their base was gone.
    pub fn replay(&mut self, vfs: &mut dyn WriteVfs, upto: usize) -> WriteResult<Vec<(String, u64)>> {
        let mut skipped = Vec::new();
        let start = self.replay_from.min(upto);
        let records = pgrc_format::try_to_vec(&self.records[start..upto], "walmodel")?;
        for (i, r) in records.into_iter().enumerate() {
            let (dir, touched) = match r {
                WalRecord::FileRef { dir, name, len, crc, masks } => {
                    let rec = FileRefRec { dir: model_loc(&dir), name: &name, len, crc, masks };
                    for (d, names) in self.replay_file_ref(vfs, start + i, &dir, &rec)? {
                        self.register_all(&d, names);
                    }
                    continue;
                }
                WalRecord::File { dir, name, off, bytes, create } => {
                    plan::apply_file_write(vfs, &dir, &name, off, &bytes, create)?;
                    (dir, vec![name])
                }
                WalRecord::Publish { dir, rec } => match plan::redo_publish(vfs, &dir, &rec)? {
                    Some(_) => (dir, plan::publish_touched(&rec)),
                    None => {
                        skipped.push((dir, rec.header.gen));
                        continue;
                    }
                },
                WalRecord::Install { dir, rec } => {
                    plan::apply_install(vfs, &dir, &rec, ApplyMode::Redo)?;
                    (dir, rec.renames.iter().map(|(_, to)| to.clone()).collect())
                }
                WalRecord::Reclaim { dir, rec } => {
                    plan::apply_reclaim(vfs, &dir, &rec)?;
                    (dir, rec.names)
                }
                WalRecord::Commit { .. } => continue,
            };
            self.register_all(&dir, touched);
        }
        Ok(skipped)
    }

    fn register_all(&mut self, dir: &str, names: Vec<String>) {
        if self.teeth.skip_sync_registration {
            return;
        }
        self.dirty.entry(dir.to_string()).or_default().extend(names);
    }
}

/// A [`WalSink`] that appends to a shared [`WalLog`], ticking its clock at every event boundary.
#[derive(Clone)]
pub struct RecordingSink {
    dir: String,
    log: Arc<Mutex<WalLog>>,
}

impl RecordingSink {
    pub fn new(dir: &str, log: Arc<Mutex<WalLog>>) -> RecordingSink {
        RecordingSink { dir: dir.to_string(), log }
    }

    fn logged(&mut self, rec: WalRecord, touched: Vec<String>, apply: Apply<'_>) -> WriteResult<()> {
        {
            let mut g = lock(&self.log);
            g.delaying += 1;
            g.tick();
            g.records.push(rec);
            g.tick();
        }
        let res = apply();
        let mut g = lock(&self.log);
        g.tick();
        if res.is_ok() {
            g.register_all(&self.dir, touched);
        }
        g.tick();
        g.delaying -= 1;
        g.tick();
        res
    }
}

impl WalSink for RecordingSink {
    fn logs(&self) -> bool {
        true
    }

    fn file_write(&mut self, name: &str, off: u64, bytes: &[u8], create: bool) -> WriteResult<()> {
        let bytes = pgrc_format::try_to_vec(bytes, "walmodel")?;
        let mut g = lock(&self.log);
        g.tick();
        let redo = g.redo;
        g.first_write.entry((self.dir.clone(), name.to_string())).or_insert(redo);
        g.records.push(WalRecord::File { dir: self.dir.clone(), name: name.to_string(), off, bytes, create });
        g.tick();
        Ok(())
    }

    fn file_done(&mut self, name: &str, fsync: &mut dyn FnMut() -> WriteResult<()>) -> WriteResult<()> {
        let mut g = lock(&self.log);
        g.delaying += 1;
        g.tick();
        let first = g.first_write.remove(&(self.dir.clone(), name.to_string()));
        let moved = first.is_some_and(|r| r != g.redo);
        let res = if moved && !g.teeth.skip_redo_check {
            g.self_synced.push(name.to_string());
            fsync()
        } else {
            g.register_all(&self.dir, vec![name.to_string()]);
            Ok(())
        };
        g.tick();
        g.delaying -= 1;
        g.tick();
        res
    }

    fn publish(&mut self, rec: &PublishRec, apply: Apply<'_>) -> WriteResult<()> {
        let rec_ = WalRecord::Publish { dir: self.dir.clone(), rec: rec.clone() };
        self.logged(rec_, plan::publish_touched(rec), apply)
    }

    fn install(&mut self, rec: &InstallRec, apply: Apply<'_>) -> WriteResult<()> {
        let rec_ = WalRecord::Install { dir: self.dir.clone(), rec: rec.clone() };
        self.logged(rec_, rec.renames.iter().map(|(_, to)| to.clone()).collect(), apply)
    }

    fn reclaim(&mut self, rec: &ReclaimRec, apply: Apply<'_>) -> WriteResult<()> {
        let rec_ = WalRecord::Reclaim { dir: self.dir.clone(), rec: rec.clone() };
        self.logged(rec_, rec.names.clone(), apply)
    }

    fn register(&mut self, name: &str) -> WriteResult<()> {
        lock(&self.log).register_all(&self.dir, vec![name.to_string()]);
        Ok(())
    }

    fn ref_min(&self) -> Option<u64> {
        lock(&self.log).spool.as_ref().map(|s| s.ref_min)
    }

    /// XlogSink's sequence (WR L2): fsync the file and its directory, link it into the spool as `pending-*` and
    /// fsync the spool, then inside one delay window insert `FILE_REF`, rename the link to the record's LSN and
    /// register the spool and the table directory for the next checkpoint.
    fn file_ref(&mut self, f: &FileRef<'_>, vfs: &mut dyn WriteVfs, fsync: RefSync<'_>) -> WriteResult<()> {
        let mut g = lock(&self.log);
        let sp = g.spool.clone().ok_or(WriteError::Contract { detail: "a by-reference file without a model spool" })?;
        let pending = sp.pending(&self.dir, f.name);
        g.tick();
        if !g.teeth.skip_ref_fsync {
            fsync(vfs)?;
        }
        g.tick();
        vfs.link_path(&format!("{}/{}", self.dir, f.name), &pending)?;
        g.tick();
        if !g.teeth.skip_pending_fsync {
            vfs.fsync_dir(&sp.dir)?;
        }
        g.delaying += 1;
        g.tick();
        let lsn = g.records.len();
        g.records.push(WalRecord::FileRef {
            dir: self.dir.clone(),
            name: f.name.to_string(),
            len: f.len,
            crc: f.crc,
            masks: f.masks,
        });
        g.tick();
        let res = vfs.rename_path(&pending, &sp.entry(lsn));
        if res.is_ok() {
            g.register_all(&sp.dir, vec![spool_name(lsn as u64)]);
            g.register_all(&self.dir, Vec::new());
        }
        g.tick();
        g.delaying -= 1;
        g.tick();
        res
    }
}
