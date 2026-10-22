//! The publish ordering law (delta v2 design §7.3-7.4), witnessed on one timeline of vfs ops and sink events:
//! every file the publish creates is logged and finished before `PUBLISH`; the part renames, the staged renames,
//! the manifest and `CURRENT` happen inside the record's apply, before the directory's registration; nothing is
//! fsynced. The clog fence and the `CURRENT` pointer are pinned as before.

use super::*;
use crate::publish::{effective_manifest, TxnVerdict};
use crate::walmodel::{RecordingSink, WalLog, WalRecord};
use crate::walsink::{Apply, WalSink};
use crate::wvfs::WvFd;
use crate::WriteResult;
use pgrc_format::wal::{InstallRec, PublishRec, ReclaimRec};
use pgsync::{lock, Mutex};
use std::sync::Arc;

type Trace = Arc<Mutex<Vec<String>>>;

fn push(t: &Trace, line: String) {
    lock(t).push(line);
}

fn short(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

struct TraceVfs {
    inner: MemVfs,
    trace: Trace,
}

impl WriteVfs for TraceVfs {
    fn create_rw(&mut self, path: &str) -> WriteResult<WvFd> {
        push(&self.trace, format!("create {}", short(path)));
        self.inner.create_rw(path)
    }
    fn open_rw(&mut self, path: &str) -> WriteResult<WvFd> {
        push(&self.trace, format!("open {}", short(path)));
        self.inner.open_rw(path)
    }
    fn pwrite_at(&mut self, fd: &WvFd, off: u64, bytes: &[u8]) -> WriteResult<()> {
        push(&self.trace, format!("pwrite {} {off}", short(fd.path())));
        self.inner.pwrite_at(fd, off, bytes)
    }
    fn fsync_file(&mut self, fd: &WvFd) -> WriteResult<()> {
        push(&self.trace, format!("fsync {}", short(fd.path())));
        self.inner.fsync_file(fd)
    }
    fn close_file(&mut self, fd: WvFd) -> WriteResult<()> {
        self.inner.close_file(fd)
    }
    fn read_full(&mut self, path: &str) -> WriteResult<Vec<u8>> {
        self.inner.read_full(path)
    }
    fn rename_path(&mut self, from: &str, to: &str) -> WriteResult<()> {
        push(&self.trace, format!("rename {} -> {}", short(from), short(to)));
        self.inner.rename_path(from, to)
    }
    fn unlink_path(&mut self, path: &str) -> WriteResult<()> {
        push(&self.trace, format!("unlink {}", short(path)));
        self.inner.unlink_path(path)
    }
    fn fsync_dir(&mut self, dir: &str) -> WriteResult<()> {
        push(&self.trace, format!("fsyncdir {dir}"));
        self.inner.fsync_dir(dir)
    }
    fn list_dir(&mut self, dir: &str) -> WriteResult<Vec<String>> {
        self.inner.list_dir(dir)
    }
    fn mkdir_path(&mut self, dir: &str) -> WriteResult<()> {
        self.inner.mkdir_path(dir)
    }
    fn exists_path(&mut self, path: &str) -> WriteResult<bool> {
        self.inner.exists_path(path)
    }
    fn link_path(&mut self, from: &str, to: &str) -> WriteResult<()> {
        push(&self.trace, format!("link {} -> {}", short(from), short(to)));
        self.inner.link_path(from, to)
    }
    fn link_count(&mut self, path: &str) -> WriteResult<u64> {
        self.inner.link_count(path)
    }
}

struct TraceSink {
    inner: RecordingSink,
    trace: Trace,
}

impl WalSink for TraceSink {
    fn logs(&self) -> bool {
        true
    }
    fn file_write(&mut self, name: &str, off: u64, bytes: &[u8], create: bool) -> WriteResult<()> {
        push(&self.trace, format!("{} {name} {off}", if create { "FILE_CREATE" } else { "FILE_WRITE" }));
        self.inner.file_write(name, off, bytes, create)
    }
    fn file_done(&mut self, name: &str, fsync: &mut dyn FnMut() -> WriteResult<()>) -> WriteResult<()> {
        push(&self.trace, format!("FILE_DONE {name}"));
        self.inner.file_done(name, fsync)
    }
    fn publish(&mut self, rec: &PublishRec, apply: Apply<'_>) -> WriteResult<()> {
        push(&self.trace, format!("PUBLISH {}", rec.header.gen));
        let t = self.trace.clone();
        let r = self.inner.publish(rec, &mut || {
            push(&t, "APPLY".into());
            apply()?;
            push(&t, "APPLIED".into());
            Ok(())
        });
        push(&self.trace, "REGISTERED".into());
        r
    }
    fn install(&mut self, rec: &InstallRec, apply: Apply<'_>) -> WriteResult<()> {
        self.inner.install(rec, apply)
    }
    fn reclaim(&mut self, rec: &ReclaimRec, apply: Apply<'_>) -> WriteResult<()> {
        self.inner.reclaim(rec, apply)
    }
    fn register(&mut self, name: &str) -> WriteResult<()> {
        self.inner.register(name)
    }
}

fn idx(t: &[String], line: &str) -> usize {
    t.iter().position(|o| o == line).unwrap_or_else(|| panic!("{line:?} not in {t:#?}"))
}

#[test]
fn wal_first_ordering_holds_and_clog_fence_present() {
    let trace: Trace = Arc::new(Mutex::new(Vec::new()));
    let log = WalLog::shared();
    let mut vfs = TraceVfs { inner: mem_with_dir(), trace: trace.clone() };
    let mut kit = Kit::new();
    let mut w = open_writer(vec![int8_col(1)], stamp(7, 1))
        .with_sink(Box::new(TraceSink { inner: RecordingSink::new(DIR, log.clone()), trace: trace.clone() }));
    for i in 0..1000u64 {
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv { vfs: &mut vfs, sources: &sources, resolver: &kit.resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
        w.append_row(&[RawDatum::Word(i)], &mut kit.ext, &mut env).expect("append");
    }
    {
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv { vfs: &mut vfs, sources: &sources, resolver: &kit.resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
        w.finish(&mut env).expect("finish");
    }
    let probe = Probe::new(TxnVerdict::InProgress).set(7, TxnVerdict::Committed);
    let out = w.publish(&mut vfs, &probe).expect("publish");
    assert_eq!((out.gen, out.part_nos.clone()), (1, vec![0]));
    assert!(matches!(out.bankstats_plane, crate::bankplane::PlaneOutcome::Written { .. }));

    let t = lock(&trace).clone();
    assert!(!t.iter().any(|l| l.starts_with("fsync")), "no fsync anywhere in seal + publish: {t:#?}");
    let publish = idx(&t, "PUBLISH 1");
    let (apply, applied, registered) = (idx(&t, "APPLY"), idx(&t, "APPLIED"), idx(&t, "REGISTERED"));
    assert!(publish < apply && apply < applied && applied < registered);

    // Before the record: every staged file created, logged and finished; no namespace change.
    // P-1 L2: no sections file; one part is loose, so no stats run either
    let staged = ["tmp-7-0.pgrct", "bankstats-1.pgrcbs.tmp"];
    assert!(!t.iter().any(|l| l.contains("sections-") || l.contains("srun-")), "no sections file, no run: {t:#?}");
    for name in staged {
        let created = idx(&t, &format!("FILE_CREATE {name} 0"));
        let done = idx(&t, &format!("FILE_DONE {name}"));
        assert!(created < done && done < publish, "{name}");
    }
    assert!(!t[..publish].iter().any(|l| l.starts_with("rename") || l.starts_with("unlink")));

    // Inside the apply: the part patch + rename, the staged renames, then the manifest, then CURRENT.
    let part = idx(&t, "rename tmp-7-0.pgrct -> part-0.pgrc");
    let plane = idx(&t, "rename bankstats-1.pgrcbs.tmp -> bankstats-1.pgrcbs");
    let manifest = idx(&t, "create manifest-1.pgrcm");
    let current = idx(&t, "rename CURRENT.tmp -> CURRENT");
    assert!(apply < part && part < plane && plane < manifest && manifest < current && current < applied);
    assert!(t[applied..].iter().all(|l| l == "APPLIED" || l == "REGISTERED"));

    // The log: the part's bytes, the record last, and the registration covers what the apply wrote.
    let g = lock(&log);
    let part_bytes: usize = g
        .records
        .iter()
        .filter_map(|r| match r {
            WalRecord::File { name, bytes, .. } if name == "tmp-7-0.pgrct" => Some(bytes.len()),
            _ => None,
        })
        .sum();
    assert_eq!(part_bytes as u64, out.manifest.parts[0].file_len);
    let Some(WalRecord::Publish { rec, .. }) = g.records.last() else { panic!("PUBLISH is the last record") };
    assert_eq!(rec.installs.len(), 1);
    for name in ["part-0.pgrc", "manifest-1.pgrcm", "CURRENT", "bankstats-1.pgrcbs"] {
        assert!(g.dirty[DIR].contains(name), "{name} registered");
    }
    drop(g);

    let mut vfs = vfs.inner;
    let m = read_manifest(&mut vfs, 1);
    assert_eq!((m.header.publisher_fxid, m.header.prev_gen, m.header.part_count, m.header.next_part_no), (7, 0, 1, 1));
    assert_eq!(m.parts[0].rows, 1000);
    let cp = pgrc_format::manifest::CommitPointer::decode(&vfs.read_full(&format!("{DIR}/CURRENT")).expect("CURRENT")).expect("pointer");
    let m_bytes = vfs.read_full(&format!("{DIR}/manifest-1.pgrcm")).expect("manifest bytes");
    assert_eq!((cp.gen, cp.manifest_len), (1, m_bytes.len() as u64));
    assert_eq!(cp.manifest_crc, pgrc_format::wire::crc32c(&m_bytes[..m_bytes.len() - 4]));
    assert_eq!(cp.manifest_crc, rec_crc(&log));

    assert_eq!(effective_manifest(&mut vfs, DIR, &probe, None).expect("eff").expect("some").header.gen, 1);
    assert!(effective_manifest(&mut vfs, DIR, &Probe::new(TxnVerdict::InProgress), None).expect("eff").is_none());
    assert!(effective_manifest(&mut vfs, DIR, &Probe::new(TxnVerdict::Aborted), None).expect("eff").is_none());
}

fn rec_crc(log: &Arc<Mutex<WalLog>>) -> u32 {
    match lock(log).records.last() {
        Some(WalRecord::Publish { rec, .. }) => rec.manifest_crc,
        _ => unreachable!(),
    }
}

/// Generation chaining: a second committed publish chains prev_gen and carries the base's parts forward;
/// part_no stays monotone.
#[test]
fn generations_chain_and_part_numbers_are_monotone() {
    let mut vfs = mem_with_dir();
    let mut kit = Kit::new();
    let probe = Probe::new(TxnVerdict::InProgress).set(7, TxnVerdict::Committed).set(8, TxnVerdict::Committed);
    let mut w1 = open_writer(vec![int8_col(1)], stamp(7, 1));
    append_int8_rows(&mut w1, &mut vfs, &mut kit, 100, |i| Some(i as i64));
    let o1 = finish_and_publish(&mut w1, &mut vfs, &mut kit, &probe);
    assert_eq!((o1.gen, o1.part_nos.clone()), (1, vec![0]));
    let mut w2 = open_writer(vec![int8_col(1)], stamp(8, 1));
    append_int8_rows(&mut w2, &mut vfs, &mut kit, 200, |i| Some(i as i64 + 7));
    let o2 = finish_and_publish(&mut w2, &mut vfs, &mut kit, &probe);
    assert_eq!((o2.gen, o2.part_nos.clone()), (2, vec![1]));
    let m = read_manifest(&mut vfs, 2);
    assert_eq!((m.header.prev_gen, m.header.part_count), (1, 2));
    assert_eq!(m.parts.iter().map(|p| p.part_no).collect::<Vec<_>>(), vec![0, 1]);
    assert_eq!(m.parts.iter().map(|p| p.rows).sum::<u64>(), 300);
}

/// One transaction tree for the probe: every fxid is in progress, and the tree's (a top and its subtransactions,
/// sub-committed or not) are one another's own; the clog probe's `TransactionIdIsCurrentTransactionId`.
struct Tree(Vec<u64>);

impl crate::publish::TxnProbe for Tree {
    fn verdict(&self, _fxid: u64) -> TxnVerdict {
        TxnVerdict::InProgress
    }
    fn is_own(&self, publisher: u64, own: u64) -> bool {
        self.0.contains(&publisher) && self.0.contains(&own)
    }
}

/// A savepoint's publish (stamped with its subtransaction's fxid) chains on its tree's in-progress generations,
/// the top's and a released subtransaction's; a foreign transaction is still refused on the tree's head.
#[test]
fn a_subtransaction_publish_chains_on_its_trees_generations() {
    let mut vfs = mem_with_dir();
    let mut kit = Kit::new();
    let tree = Tree(vec![7, 9, 11]);
    for (i, fxid) in [7u64, 9, 11].into_iter().enumerate() {
        let mut w = open_writer(vec![int8_col(1)], stamp(fxid, 1));
        append_int8_rows(&mut w, &mut vfs, &mut kit, 50, |r| Some(r as i64));
        let o = finish_and_publish(&mut w, &mut vfs, &mut kit, &tree);
        assert_eq!((o.gen, o.part_nos.clone()), (i as u64 + 1, vec![i as u32]), "fxid {fxid}");
    }
    let m = read_manifest(&mut vfs, 3);
    assert_eq!((m.header.publisher_fxid, m.header.prev_gen, m.header.part_count), (11, 2, 3));

    let mut foreign = open_writer(vec![int8_col(1)], stamp(8, 1));
    append_int8_rows(&mut foreign, &mut vfs, &mut kit, 10, |r| Some(r as i64));
    let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
    let mut env = SealEnv { vfs: &mut vfs, sources: &sources, resolver: &kit.resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
    foreign.finish(&mut env).expect("finish");
    let err = foreign.publish(&mut vfs, &tree).expect_err("a foreign in-progress head refuses");
    assert_eq!(err, crate::WriteError::PublishBusy { holder_fxid: 11 }, "the hint arm names the head's publisher");
    assert_eq!(foreign.sealed_parts().len(), 1, "the refused writer keeps its sealed part for the retry");
    assert!(!vfs.exists_path(&format!("{DIR}/manifest-4.pgrcm")).expect("exists"), "nothing staged");
    // The scan arm (no CURRENT hint) names the same holder.
    vfs.unlink_path(&format!("{DIR}/CURRENT")).expect("unlink CURRENT");
    let err = foreign.publish(&mut vfs, &tree).expect_err("the scan arm refuses too");
    assert_eq!(err, crate::WriteError::PublishBusy { holder_fxid: 11 }, "the scan arm names the head's publisher");
    // Once the holder's tree is gone (committed), the retry chains on its head.
    let o = foreign.publish(&mut vfs, &Probe::new(TxnVerdict::Committed)).expect("the retry publishes");
    assert_eq!((o.gen, o.part_nos.clone()), (4, vec![3]));
}
