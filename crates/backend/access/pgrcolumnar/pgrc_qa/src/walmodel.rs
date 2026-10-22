//! The WAL model the crash batteries share (delta v2 design §7.6): scheduled checkpoints, the born-red teeth,
//! recovery, and the kill -9 ladder's on-disk WAL.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use pgrc_format::wal::{FileRefRec, InstallRec, PublishRec, ReclaimRec};
use pgrc_format::wire::crc32c;
use pgrc_write::cleanup::{cleanup_dir, CleanupReport};
use pgrc_write::publish::TxnVerdict;
use pgrc_write::walmodel::{model_loc, Clock, RecordingSink, Teeth, WalLog, WalRecord};
use pgrc_write::walsink::WalSink;
use pgrc_write::wvfs::{RealVfs, WriteVfs, WvFd};
use pgrc_write::{WriteError, WriteResult};
use pgsync::{lock, Mutex};

use crate::adapters::Probe;

pub fn seed_skip_delay_chkpt() -> Teeth {
    Teeth { skip_delay_chkpt: true, ..Teeth::default() }
}

pub fn seed_skip_sync_registration() -> Teeth {
    Teeth { skip_sync_registration: true, ..Teeth::default() }
}

pub fn seed_skip_redo_check() -> Teeth {
    Teeth { skip_redo_check: true, ..Teeth::default() }
}

/// WR T-ref-fsync: a reference logged before its file is durable.
pub fn seed_skip_ref_fsync() -> Teeth {
    Teeth { skip_ref_fsync: true, ..Teeth::default() }
}

/// WR T-pending-durable: the pending spool link is not fsynced before the insert.
pub fn seed_skip_pending_fsync() -> Teeth {
    Teeth { skip_pending_fsync: true, ..Teeth::default() }
}

/// WR T-spool-early: a checkpoint drops spool entries a consumer still needs.
pub fn seed_skip_spool_hold() -> Teeth {
    Teeth { skip_spool_hold: true, ..Teeth::default() }
}

/// One checkpoint: it begins at tick `begin` and completes at the first tick from `begin + delay` that is outside
/// every `DELAY_CHKPT_START` window (at that tick regardless, under the `skip_delay_chkpt` tooth).
pub struct ScheduledCheckpoint<V> {
    vfs: V,
    begin: u64,
    delay: u64,
    tick: u64,
    begun: Option<u64>,
    done: bool,
}

impl<V: WriteVfs + Send> ScheduledCheckpoint<V> {
    pub fn new(vfs: V, begin: u64, delay: u64) -> ScheduledCheckpoint<V> {
        ScheduledCheckpoint { vfs, begin, delay, tick: 0, begun: None, done: false }
    }
}

impl<V: WriteVfs + Send> Clock for ScheduledCheckpoint<V> {
    fn tick(&mut self, log: &mut WalLog) {
        let t = self.tick;
        self.tick += 1;
        if self.done {
            return;
        }
        if self.begun.is_none() && t >= self.begin {
            log.begin_checkpoint();
            self.begun = Some(t);
        }
        if let Some(b) = self.begun {
            if t >= b + self.delay && (log.delaying == 0 || log.teeth.skip_delay_chkpt) {
                log.complete_checkpoint(&mut self.vfs).expect("the model checkpoint's sync pass");
                self.done = true;
            }
        }
    }
}

/// Counts ticks, to size the window a checkpoint schedule sweeps.
pub struct TickCounter(pub Arc<AtomicU64>);

impl Clock for TickCounter {
    fn tick(&mut self, _: &mut WalLog) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

/// The clog after a crash: surviving commit records commit, `prepared` stay in progress, everything else aborted.
pub fn probe_of(committed: &BTreeSet<u64>, prepared: &[u64]) -> Probe {
    let mut p = Probe::new(TxnVerdict::Aborted);
    for f in committed {
        p.mark(*f, TxnVerdict::Committed);
    }
    for f in prepared {
        p.mark(*f, TxnVerdict::InProgress);
    }
    p
}

/// The universe already lost what was not durable and `records[..upto]` survived: replay from the last completed
/// checkpoint. Returns the clog those records imply.
pub fn replay(vfs: &mut dyn WriteVfs, log: &Arc<Mutex<WalLog>>, upto: usize) -> WriteResult<BTreeSet<u64>> {
    let mut g = lock(log);
    g.crash(upto);
    g.replay(vfs, upto)?;
    Ok(g.committed(upto))
}

/// Crash recovery of `dir`: [`replay`], `cleanup_dir` under the implied clog, then the end-of-recovery checkpoint.
pub fn recover(
    vfs: &mut dyn WriteVfs,
    log: &Arc<Mutex<WalLog>>,
    dir: &str,
    upto: usize,
    prepared: &[u64],
) -> WriteResult<CleanupReport> {
    let committed = replay(vfs, log, upto)?;
    lock(log).remove_spool_debris(vfs, upto)?;
    let rep = cleanup_dir(vfs, &mut RecordingSink::new(dir, log.clone()), dir, &probe_of(&committed, prepared))?;
    lock(log).checkpoint(vfs)?;
    Ok(rep)
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u16).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

/// `[len u32][crc32c u32][kind u8, payload]`; the formats' own codecs carry the namespace records.
pub fn encode_frame(r: &WalRecord, out: &mut Vec<u8>) -> WriteResult<()> {
    let mut p = Vec::new();
    match r {
        WalRecord::File { dir, name, off, bytes, create } => {
            p.push(1);
            put_str(&mut p, dir);
            put_str(&mut p, name);
            p.extend_from_slice(&off.to_le_bytes());
            p.push(*create as u8);
            p.extend_from_slice(bytes);
        }
        WalRecord::Publish { dir, rec } => {
            p.push(2);
            put_str(&mut p, dir);
            rec.encode(&mut p)?;
        }
        WalRecord::Install { dir, rec } => {
            p.push(3);
            put_str(&mut p, dir);
            rec.encode(&mut p)?;
        }
        WalRecord::Reclaim { dir, rec } => {
            p.push(4);
            put_str(&mut p, dir);
            rec.encode(&mut p)?;
        }
        WalRecord::Commit { fxid } => {
            p.push(5);
            p.extend_from_slice(&fxid.to_le_bytes());
        }
        WalRecord::FileRef { dir, name, len, crc, masks } => {
            p.push(6);
            put_str(&mut p, dir);
            FileRefRec { dir: model_loc(dir), name, len: *len, crc: *crc, masks: *masks }.encode(&mut p)?;
        }
    }
    out.extend_from_slice(&(p.len() as u32).to_le_bytes());
    out.extend_from_slice(&crc32c(&p).to_le_bytes());
    out.extend_from_slice(&p);
    Ok(())
}

fn take<'a>(b: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
    if b.len() < n {
        return None;
    }
    let (h, t) = b.split_at(n);
    *b = t;
    Some(h)
}

fn take_str(b: &mut &[u8]) -> WriteResult<Option<String>> {
    let Some(n) = take(b, 2).and_then(|h| <[u8; 2]>::try_from(h).ok()).map(u16::from_le_bytes) else { return Ok(None) };
    let Some(s) = take(b, n as usize) else { return Ok(None) };
    Ok(String::from_utf8(pgrc_format::try_to_vec(s, "pgrc WAL model")?).ok())
}

fn take_u64(b: &mut &[u8]) -> Option<u64> {
    Some(u64::from_le_bytes(take(b, 8)?.try_into().ok()?))
}

fn decode_payload(mut p: &[u8]) -> WriteResult<WalRecord> {
    let bad = || WriteError::Contract { detail: "malformed WAL model frame" };
    let kind = take(&mut p, 1).ok_or_else(bad)?[0];
    if kind == 5 {
        return Ok(WalRecord::Commit { fxid: take_u64(&mut p).ok_or_else(bad)? });
    }
    let dir = take_str(&mut p)?.ok_or_else(bad)?;
    Ok(match kind {
        1 => {
            let name = take_str(&mut p)?.ok_or_else(bad)?;
            let off = take_u64(&mut p).ok_or_else(bad)?;
            let create = take(&mut p, 1).ok_or_else(bad)?[0] != 0;
            WalRecord::File { dir, name, off, bytes: pgrc_format::try_to_vec(p, "pgrc WAL model")?, create }
        }
        2 => WalRecord::Publish { dir, rec: PublishRec::decode(p)? },
        3 => WalRecord::Install { dir, rec: InstallRec::decode(p)? },
        4 => WalRecord::Reclaim { dir, rec: ReclaimRec::decode(p)? },
        6 => {
            let r = FileRefRec::decode(p)?;
            WalRecord::FileRef { name: r.name.to_string(), len: r.len, crc: r.crc, masks: r.masks, dir }
        }
        _ => return Err(bad()),
    })
}

/// Decode frames up to the first torn one: the end of what a killed writer finished.
pub fn decode_frames(mut b: &[u8], out: &mut Vec<WalRecord>) -> WriteResult<()> {
    loop {
        let Some(h) = take(&mut b, 8) else { return Ok(()) };
        let len = u32::from_le_bytes(h[..4].try_into().expect("4 bytes")) as usize;
        let crc = u32::from_le_bytes(h[4..].try_into().expect("4 bytes"));
        let Some(p) = take(&mut b, len) else { return Ok(()) };
        if crc32c(p) != crc {
            return Ok(());
        }
        out.push(decode_payload(p)?);
    }
}

const SEG_PREFIX: &str = "wal.";
const CKPT: &str = "ckpt";
const CKPT_TMP: &str = "ckpt.tmp";

struct Segment {
    path: String,
    fd: Option<WvFd>,
    off: u64,
    persisted: usize,
}

/// Appends the records inserted since the last tick, so a killed process leaves every record it inserted.
struct Persist(Arc<Mutex<Segment>>);

impl Clock for Persist {
    fn tick(&mut self, log: &mut WalLog) {
        let mut s = lock(&self.0);
        if s.persisted >= log.records.len() {
            return;
        }
        let mut buf = Vec::new();
        for r in &log.records[s.persisted..] {
            encode_frame(r, &mut buf).expect("encode a WAL model frame");
        }
        let mut vfs = RealVfs;
        if s.fd.is_none() {
            s.fd = Some(vfs.create_rw(&s.path).expect("create a WAL segment"));
        }
        let off = s.off;
        vfs.pwrite_at(s.fd.as_ref().expect("open segment"), off, &buf).expect("append to the WAL segment");
        s.off += buf.len() as u64;
        s.persisted = log.records.len();
    }
}

/// The ladder's WAL in a real base directory: `wal.<n>` segments (each process appends to a new one, so a torn tail
/// ends its segment) and `ckpt`, the last completed checkpoint's redo index.
pub struct FileWal {
    base: String,
    log: Arc<Mutex<WalLog>>,
    seg: Arc<Mutex<Segment>>,
}

impl FileWal {
    pub fn open(base: &str) -> WriteResult<FileWal> {
        let mut vfs = RealVfs;
        let mut segs: Vec<u64> =
            vfs.list_dir(base)?.iter().filter_map(|n| n.strip_prefix(SEG_PREFIX)?.parse().ok()).collect();
        segs.sort_unstable();
        let mut log = WalLog::default();
        for s in &segs {
            decode_frames(&vfs.read_full(&format!("{base}/{SEG_PREFIX}{s}"))?, &mut log.records)?;
        }
        let ckpt = format!("{base}/{CKPT}");
        if vfs.exists_path(&ckpt)? {
            let b = vfs.read_full(&ckpt)?;
            if b.len() != 12 || crc32c(&b[..8]).to_le_bytes() != b[8..] {
                return Err(WriteError::Contract { detail: "torn WAL model checkpoint pointer" });
            }
            let redo = u64::from_le_bytes(b[..8].try_into().expect("8 bytes")) as usize;
            log.redo = redo.min(log.records.len());
            log.replay_from = log.redo;
        }
        log.flushed = log.records.len();
        let path = format!("{base}/{SEG_PREFIX}{}", segs.last().map_or(0, |s| s + 1));
        let seg = Arc::new(Mutex::new(Segment { path, fd: None, off: 0, persisted: log.records.len() }));
        log.set_clock(Box::new(Persist(seg.clone())));
        Ok(FileWal { base: base.to_string(), log: Arc::new(Mutex::new(log)), seg })
    }

    pub fn sink(&self, dir: &str) -> Box<dyn WalSink + Send> {
        Box::new(RecordingSink::new(dir, self.log.clone()))
    }

    pub fn committed(&self) -> BTreeSet<u64> {
        let g = lock(&self.log);
        g.committed(g.records.len())
    }

    /// Replay from the checkpoint pointer, clean `dir` under the logged clog, and checkpoint.
    pub fn recover(&self, vfs: &mut dyn WriteVfs, dir: &str) -> WriteResult<CleanupReport> {
        {
            let mut g = lock(&self.log);
            let n = g.records.len();
            g.replay(vfs, n)?;
            g.remove_spool_debris(vfs, n)?;
        }
        let probe = probe_of(&self.committed(), &[]);
        let rep = cleanup_dir(vfs, &mut RecordingSink::new(dir, self.log.clone()), dir, &probe)?;
        self.checkpoint(vfs)?;
        Ok(rep)
    }

    /// The commit record, flushed: a round's ack point.
    pub fn commit(&self, fxid: u64) -> WriteResult<()> {
        lock(&self.log).commit(fxid);
        self.flush()
    }

    fn flush(&self) -> WriteResult<()> {
        match &lock(&self.seg).fd {
            Some(fd) => RealVfs.fsync_file(fd),
            None => Ok(()),
        }
    }

    /// Sync what was registered, then durably point replay at the checkpoint's redo.
    pub fn checkpoint(&self, vfs: &mut dyn WriteVfs) -> WriteResult<()> {
        let redo = {
            let mut g = lock(&self.log);
            g.checkpoint(vfs)?;
            g.replay_from as u64
        };
        self.flush()?;
        let mut b = pgrc_format::try_to_vec(&redo.to_le_bytes(), "pgrc WAL model")?;
        b.extend_from_slice(&crc32c(&redo.to_le_bytes()).to_le_bytes());
        let tmp = format!("{}/{CKPT_TMP}", self.base);
        let fd = vfs.create_rw(&tmp)?;
        vfs.pwrite_at(&fd, 0, &b)?;
        vfs.fsync_file(&fd)?;
        vfs.close_file(fd)?;
        vfs.rename_path(&tmp, &format!("{}/{CKPT}", self.base))?;
        vfs.fsync_dir(&self.base)
    }
}
