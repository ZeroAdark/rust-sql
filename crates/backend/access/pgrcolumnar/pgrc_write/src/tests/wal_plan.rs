//! The plan and the shared apply (design §7.4, §7.6): manifest laws, idempotence, redo from every prefix of a
//! primary apply, the part-identity check, and the file-write redo rules.

use super::*;
use crate::plan::{apply_file_write, apply_publish, manifest_of, redo_publish, ApplyMode, PublishPlan};
use crate::seal::SealedPart;
use crate::wvfs::WvFd;
use crate::{WriteError, WriteResult};
use pgrc_format::manifest::PartRecord;
use pgrc_format::wal::PublishKind;
use pgrc_format::FormatError;

fn sealed_parts(vfs: &mut MemVfs, fxid: u64, rows: &[u64]) -> Vec<SealedPart> {
    let mut kit = Kit::new();
    let mut w = open_writer(vec![int8_col(1)], stamp(fxid, 1));
    for &n in rows {
        append_int8_rows(&mut w, vfs, &mut kit, n, |i| Some(i as i64 * fxid as i64));
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv { vfs, sources: &sources, resolver: &kit.resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
        w.finish(&mut env).expect("seal");
    }
    w.sealed_parts().to_vec()
}

fn image(vfs: &mut MemVfs) -> BTreeMap<String, Vec<u8>> {
    vfs.list_dir(DIR).expect("list").into_iter().map(|n| {
        let b = vfs.read_full(&format!("{DIR}/{n}")).expect("read");
        (n, b)
    }).collect()
}

fn durable_all(vfs: &mut MemVfs) {
    for n in vfs.list_dir(DIR).expect("list") {
        let fd = vfs.open_rw(&format!("{DIR}/{n}")).expect("open");
        vfs.fsync_file(&fd).expect("fsync");
        vfs.close_file(fd).expect("close");
    }
    vfs.fsync_dir(DIR).expect("fsyncdir");
}

fn spec() -> crate::seal::PartSpec {
    *open_writer(vec![int8_col(1)], stamp(1, 1)).spec()
}

fn rec(part_no: u32, rows: u64) -> PartRecord {
    PartRecord { rows, file_len: 100, footer_off: 4, dv_gen: 0, dv_len: 0, part_no, flags: 0, granule_count: 1, band_count: 1, dv_crc: 0, granule_rows: 8192 }
}

#[test]
fn manifest_of_retires_upserts_by_part_no_and_carries_ext() {
    let s = spec();
    let mut b = PublishPlan::build(None, &s, 5, PublishKind::Bulk);
    for p in [0, 1, 2] {
        b.upsert(rec(p, 10));
    }
    b.ext({
        let mut e = Vec::new();
        pgrc_format::manifest::put_ext_record(&mut e, 1, b"seg");
        e
    });
    let base = b.finish().expect("plan").manifest;
    let mut b = PublishPlan::build(Some(&base), &s, 6, PublishKind::Compact);
    b.retire(0);
    b.upsert(rec(2, 99));
    b.upsert(rec(7, 1));
    let plan = b.finish().expect("plan");
    let m = manifest_of(Some(&base), &plan.rec);
    assert_eq!(m, plan.manifest);
    assert_eq!(m.parts.iter().map(|p| (p.part_no, p.rows)).collect::<Vec<_>>(), vec![(1, 10), (2, 99), (7, 1)]);
    assert_eq!((m.header.gen, m.header.prev_gen, m.header.part_count, m.header.next_part_no), (2, 1, 3, 8));
    assert_eq!(m.ext, base.ext, "the base's extension block carries forward");
    assert_eq!(plan.rec.base_gen, 1);
}

#[test]
fn install_as_keeps_an_explicit_part_number() {
    let mut vfs = mem_with_dir();
    let sealed = sealed_parts(&mut vfs, 7, &[50, 60]);
    let mut b = PublishPlan::build(None, &spec(), 7, PublishKind::Seal);
    b.install_as(&sealed[0], 12).expect("install");
    assert_eq!(b.install(&sealed[1]).expect("install"), 13);
    let plan = b.finish().expect("plan");
    assert_eq!(plan.part_nos(), vec![12, 13]);
    assert_eq!(plan.manifest.header.next_part_no, 14);
}

fn plan_for(sealed: &[SealedPart]) -> PublishPlan {
    let mut b = PublishPlan::build(None, &spec(), 7, PublishKind::Bulk);
    for s in sealed {
        b.install(s).expect("install");
    }
    b.finish().expect("plan")
}

#[test]
fn apply_is_idempotent_and_redo_after_it_changes_nothing() {
    let mut vfs = mem_with_dir();
    let sealed = sealed_parts(&mut vfs, 7, &[40, 70]);
    let plan = plan_for(&sealed);
    let m = apply_publish(&mut vfs, DIR, None, &plan.rec, ApplyMode::Primary).expect("apply");
    assert_eq!(m, plan.manifest);
    let once = image(&mut vfs);
    assert!(once.contains_key("part-0.pgrc") && once.contains_key("part-1.pgrc") && once.contains_key("CURRENT"));
    redo_publish(&mut vfs, DIR, &plan.rec).expect("redo").expect("applied");
    assert_eq!(image(&mut vfs), once);
}

/// V5SYNC (fd pool): redo over parts its publish already installed whole writes none of their bytes, so their mtimes
/// stay and a hot-standby reader's pooled reopen still finds the file it opened (the registry's identity is
/// (dev, ino, len, mtime)).
#[test]
fn redo_over_an_installed_part_leaves_its_bytes_untouched() {
    let mut vfs = mem_with_dir();
    let sealed = sealed_parts(&mut vfs, 7, &[40, 70]);
    let plan = plan_for(&sealed);
    apply_primary(&mut vfs, &plan);
    let once = image(&mut vfs);
    let from = vfs.ops().len();
    redo_publish(&mut vfs, DIR, &plan.rec).expect("redo").expect("applied");
    let part_writes: Vec<&String> = vfs.ops()[from..].iter().filter(|l| l.starts_with("pwrite ") && l.contains("/part-")).collect();
    assert!(part_writes.is_empty(), "redo rewrote installed parts: {part_writes:?}");
    assert_eq!(image(&mut vfs), once);
}

/// Stops the apply after `left` mutating ops; everything done by then is on disk.
struct StopVfs<'a> {
    inner: &'a mut MemVfs,
    left: usize,
}

impl StopVfs<'_> {
    fn step(&mut self) -> WriteResult<()> {
        if self.left == 0 {
            return Err(WriteError::Io { op: "stop", path: String::new(), errno: 0 });
        }
        self.left -= 1;
        Ok(())
    }
}

impl WriteVfs for StopVfs<'_> {
    fn create_rw(&mut self, path: &str) -> WriteResult<WvFd> {
        self.step()?;
        self.inner.create_rw(path)
    }
    fn open_rw(&mut self, path: &str) -> WriteResult<WvFd> {
        self.inner.open_rw(path)
    }
    fn pwrite_at(&mut self, fd: &WvFd, off: u64, bytes: &[u8]) -> WriteResult<()> {
        self.step()?;
        self.inner.pwrite_at(fd, off, bytes)
    }
    fn fsync_file(&mut self, fd: &WvFd) -> WriteResult<()> {
        self.inner.fsync_file(fd)
    }
    fn close_file(&mut self, fd: WvFd) -> WriteResult<()> {
        self.inner.close_file(fd)
    }
    fn read_full(&mut self, path: &str) -> WriteResult<Vec<u8>> {
        self.inner.read_full(path)
    }
    fn rename_path(&mut self, from: &str, to: &str) -> WriteResult<()> {
        self.step()?;
        self.inner.rename_path(from, to)
    }
    fn unlink_path(&mut self, path: &str) -> WriteResult<()> {
        self.step()?;
        self.inner.unlink_path(path)
    }
    fn fsync_dir(&mut self, dir: &str) -> WriteResult<()> {
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
        self.step()?;
        self.inner.link_path(from, to)
    }
    fn link_count(&mut self, path: &str) -> WriteResult<u64> {
        self.inner.link_count(path)
    }
}

/// Redo repairs every prefix of a primary apply, whether that prefix reached disk (the stop leaves it) or not
/// (a crash drops it), to the same bytes a complete apply leaves.
#[test]
fn redo_completes_every_prefix_of_an_apply_byte_identically() {
    let mut reference = mem_with_dir();
    let sealed = sealed_parts(&mut reference, 7, &[40, 70]);
    let plan = plan_for(&sealed);
    let pre = image(&mut reference);
    let staged = || {
        let mut v = mem_with_dir();
        for (n, b) in &pre {
            apply_file_write(&mut v, DIR, n, 0, b, true).expect("stage");
        }
        durable_all(&mut v);
        v
    };
    apply_publish(&mut reference, DIR, None, &plan.rec, ApplyMode::Primary).expect("apply");
    let want = image(&mut reference);
    let mut k = 0;
    loop {
        let mut kept = staged();
        let done = apply_publish(&mut StopVfs { inner: &mut kept, left: k }, DIR, None, &plan.rec, ApplyMode::Primary).is_ok();
        durable_all(&mut kept);
        crate::testkit::crash_and_revive(&mut kept);
        redo_publish(&mut kept, DIR, &plan.rec).expect("redo").expect("applied");
        assert_eq!(image(&mut kept), want, "prefix of {k} ops on disk");

        let mut lost = staged();
        let _ = apply_publish(&mut StopVfs { inner: &mut lost, left: k }, DIR, None, &plan.rec, ApplyMode::Primary);
        crate::testkit::crash_and_revive(&mut lost);
        redo_publish(&mut lost, DIR, &plan.rec).expect("redo").expect("applied");
        assert_eq!(image(&mut lost), want, "prefix of {k} ops lost");
        if done {
            break;
        }
        k += 1;
    }
    assert!(k > 8, "the apply is more than a handful of ops: {k}");
}

fn apply_primary(vfs: &mut MemVfs, plan: &PublishPlan) {
    apply_publish(vfs, DIR, None, &plan.rec, ApplyMode::Primary).expect("apply");
}

#[test]
fn redo_repatches_its_own_renamed_part_and_skips_a_foreign_one() {
    let mut vfs = mem_with_dir();
    let sealed = sealed_parts(&mut vfs, 7, &[40]);
    let plan = plan_for(&sealed);
    let unpatched = vfs.read_full(&format!("{DIR}/{}", sealed[0].tmp_name)).expect("tmp");
    apply_primary(&mut vfs, &plan);
    let want = image(&mut vfs);

    // The rename reached disk and the patch did not: redo finds its own sealed image under part-0 and patches it.
    let fd = vfs.create_rw(&format!("{DIR}/part-0.pgrc")).expect("rw");
    vfs.pwrite_at(&fd, 0, &unpatched).expect("w");
    vfs.close_file(fd).expect("close");
    redo_publish(&mut vfs, DIR, &plan.rec).expect("redo").expect("applied");
    assert_eq!(image(&mut vfs), want);

    // part-0 now holds another publish's part (a dead generation's number reused): redo leaves it alone.
    let mut other = mem_with_dir();
    let foreign = sealed_parts(&mut other, 8, &[90]);
    let foreign_bytes = other.read_full(&format!("{DIR}/{}", foreign[0].tmp_name)).expect("tmp");
    let fd = vfs.create_rw(&format!("{DIR}/part-0.pgrc")).expect("rw");
    vfs.pwrite_at(&fd, 0, &foreign_bytes).expect("w");
    vfs.close_file(fd).expect("close");
    redo_publish(&mut vfs, DIR, &plan.rec).expect("redo").expect("applied");
    assert_eq!(vfs.read_full(&format!("{DIR}/part-0.pgrc")).expect("part"), foreign_bytes);
}

#[test]
fn redo_refuses_a_tmp_that_is_not_the_sealed_part() {
    let mut vfs = mem_with_dir();
    let sealed = sealed_parts(&mut vfs, 7, &[40]);
    let plan = plan_for(&sealed);
    let mut other = mem_with_dir();
    let foreign = sealed_parts(&mut other, 8, &[90]);
    let foreign_bytes = other.read_full(&format!("{DIR}/{}", foreign[0].tmp_name)).expect("tmp");
    apply_file_write(&mut vfs, DIR, &sealed[0].tmp_name, 0, &foreign_bytes, true).expect("clobber");
    assert!(matches!(
        redo_publish(&mut vfs, DIR, &plan.rec),
        Err(WriteError::Format(FormatError::Corrupt { .. }))
    ));
}

#[test]
fn a_crc_mismatch_refuses_before_touching_a_file() {
    let mut vfs = mem_with_dir();
    let sealed = sealed_parts(&mut vfs, 7, &[40]);
    let mut plan = plan_for(&sealed);
    plan.rec.manifest_crc ^= 1;
    let before = image(&mut vfs);
    assert!(matches!(
        apply_publish(&mut vfs, DIR, None, &plan.rec, ApplyMode::Primary),
        Err(WriteError::Format(FormatError::Corrupt { .. }))
    ));
    assert_eq!(image(&mut vfs), before);
}

#[test]
fn redo_of_a_publish_whose_base_is_gone_is_skipped() {
    let mut vfs = mem_with_dir();
    let sealed = sealed_parts(&mut vfs, 7, &[40]);
    let mut b = PublishPlan::build(None, &spec(), 7, PublishKind::Bulk);
    b.install(&sealed[0]).expect("install");
    let first = b.finish().expect("plan");
    let mut b = PublishPlan::build(Some(&first.manifest), &spec(), 7, PublishKind::Bulk);
    b.upsert(rec(9, 1));
    let second = b.finish().expect("plan");
    assert_eq!(second.rec.base_gen, 1);
    assert!(redo_publish(&mut vfs, DIR, &second.rec).expect("redo").is_none());
}

#[test]
fn a_skipped_publish_unlinks_the_temps_file_write_redo_recreated() {
    // P4: replay restarted from an older redo point; the first pass applied this publish and a later RECLAIM retired
    // its base, so the second pass skips it, and the temp FILE_WRITE redo recreated would stay behind.
    let mut vfs = mem_with_dir();
    let base = sealed_parts(&mut vfs, 7, &[40]);
    let mut b = PublishPlan::build(None, &spec(), 7, PublishKind::Bulk);
    b.install(&base[0]).expect("install");
    let first = b.finish().expect("plan");
    let sealed = sealed_parts(&mut vfs, 8, &[30]);
    let mut b = PublishPlan::build(Some(&first.manifest), &spec(), 8, PublishKind::Bulk);
    b.install(&sealed[0]).expect("install");
    b.rename("tmp-8-9.pgrct".into(), "part-1.stats-1.pgrcs".into());
    let second = b.finish().expect("plan");
    apply_file_write(&mut vfs, DIR, "tmp-8-9.pgrct", 0, b"stats", true).expect("w");
    apply_file_write(&mut vfs, DIR, "part-0.pgrc", 0, b"kept", true).expect("w");
    let tmp = format!("{DIR}/{}", sealed[0].tmp_name);
    assert!(vfs.exists_path(&tmp).expect("stat"));
    assert!(redo_publish(&mut vfs, DIR, &second.rec).expect("redo").is_none());
    assert!(!vfs.exists_path(&tmp).expect("stat"), "the install's temp is unlinked");
    assert!(!vfs.exists_path(&format!("{DIR}/tmp-8-9.pgrct")).expect("stat"), "a rename's temp source is unlinked");
    assert_eq!(vfs.read_full(&format!("{DIR}/part-0.pgrc")).expect("r"), b"kept", "no other file is touched");
    assert!(redo_publish(&mut vfs, DIR, &second.rec).expect("again").is_none(), "idempotent: the temps are gone");
}

#[test]
fn file_write_redo_truncates_on_create_and_skips_a_vanished_file() {
    let mut vfs = mem_with_dir();
    apply_file_write(&mut vfs, DIR, "sections-4.pgrcx", 0, b"a much longer stale image", true).expect("w");
    apply_file_write(&mut vfs, DIR, "sections-4.pgrcx", 0, b"new", true).expect("w");
    assert_eq!(vfs.read_full(&format!("{DIR}/sections-4.pgrcx")).expect("r"), b"new");
    apply_file_write(&mut vfs, DIR, "sections-4.pgrcx", 3, b"+tail", false).expect("w");
    assert_eq!(vfs.read_full(&format!("{DIR}/sections-4.pgrcx")).expect("r"), b"new+tail");
    apply_file_write(&mut vfs, DIR, "tmp-3-0.pgrct", 4096, b"late chunk", false).expect("skip");
    assert!(!vfs.exists_path(&format!("{DIR}/tmp-3-0.pgrct")).expect("stat"), "no sparse file from a later chunk");
}

#[test]
fn redo_refuses_to_skip_a_temp_a_restartpoint_swept() {
    // WR L5f: a hot standby's restartpoint removed these temps as a finished writer's. Redo that would then skip one
    // as missing (a FILE_WRITE chunk, a PUBLISH install, a rename) refuses: that writer was alive. Without the guard
    // each skip is silent. Fxid 9_000_007 keeps the swept names apart from the other tests'.
    use crate::cleanup::{forget_swept, note_swept};
    use crate::plan::apply_install;
    use pgrc_format::wal::{DirLoc, InstallRec};
    let fxid = 9_000_007;
    let mut vfs = mem_with_dir();
    let sealed = sealed_parts(&mut vfs, fxid, &[40]);
    let plan = plan_for(&sealed);
    let tmp = format!("{DIR}/{}", sealed[0].tmp_name);
    vfs.unlink_path(&tmp).expect("the sweep");
    let late = format!("tmp-{fxid}-99.pgrct");
    let loc = DirLoc { spc: 1663, db: 5, relnumber: 1 };
    let install = InstallRec { dir: loc, renames: vec![(late.clone(), "part-7.pgrc".into())] };
    let refused = |r: WriteResult<()>| matches!(r, Err(WriteError::SweptAlive { fxid: f, .. }) if f == fxid);

    note_swept(tmp.clone(), fxid);
    note_swept(format!("{DIR}/{late}"), fxid);
    assert!(refused(apply_file_write(&mut vfs, DIR, &sealed[0].tmp_name, 4096, b"late chunk", false)));
    assert!(refused(redo_publish(&mut vfs, DIR, &plan.rec).map(drop)), "the install of a swept temp");
    assert!(refused(apply_install(&mut vfs, DIR, &install, ApplyMode::Redo)), "the rename of a swept temp");
    let msg = apply_file_write(&mut vfs, DIR, &sealed[0].tmp_name, 0, b"x", false).unwrap_err().to_string();
    assert_eq!(msg, format!("pgrc: a swept writer was alive: {tmp} fxid {fxid}"));

    // After a restart nothing is swept: the designed skips stand.
    forget_swept();
    apply_file_write(&mut vfs, DIR, &sealed[0].tmp_name, 4096, b"late chunk", false).expect("skip");
    assert!(!vfs.exists_path(&tmp).expect("stat"));
    redo_publish(&mut vfs, DIR, &plan.rec).expect("skip").expect("applied");
    apply_install(&mut vfs, DIR, &install, ApplyMode::Redo).expect("skip");
}
