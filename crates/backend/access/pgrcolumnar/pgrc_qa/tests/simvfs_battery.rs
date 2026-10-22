//! SimVfs composition battery under WAL (§5 M3-K; delta v2 design §7.6): crash at EVERY vfs-op boundary of the
//! seal and publish windows × S adversarial persistence outcomes (torn sectors + dirent subsets/reorder), with only
//! the flushed WAL or all of it surviving ⇒ recovery (replay + `cleanup_dir`) leaves the table exactly OLD; retry
//! then yields exactly NEW; residue is reclaimed by name; a committed publish survives ANY adversarial outcome
//! wholesale. Born-RED: a recovery that skips replay loses a committed publish under some outcome.
//!
//! #462 extension: the RECOVERY window gets the same treatment — crash at every vfs-op boundary of recovery ×
//! adversarial outcomes, then recover again: old-or-new, and CURRENT never survives naming a removed generation.
//! Born-RED: the pre-#462 cleanup shape (remove without repoint) is caught, and so is a crashed recovery that is
//! not rerun (cleanup has no barrier; rerunning recovery is what repairs it).

mod common;

use pgrc_qa::adapters::{memdir_of, Probe};
use pgrc_qa::corpus::{
    append_fixture_rows, finish, int8_fixture, open_writer, verify_manifest, Fixture,
};
use pgrc_qa::harness::{check_sections_sidecar, round_fxid};
use pgrc_qa::simvfs::SimVfs;
use pgrc_qa::walmodel::{recover, replay};
use pgrc_qa::XorShift;
use pgrc_format::dirlayout::{
    manifest_file_name, part_file_name, sidecar_file_name, CURRENT_FILE_NAME,
};
use pgrc_format::manifest::Manifest;
use pgrc_format::sidecar::SidecarKind;
use pgrc_read::manifest_walk::{resolve_effective, TableExpect};
use pgrc_write::cleanup::cleanup_dir;
use pgrc_write::publish::{effective_manifest, publish_parts, TxnVerdict};
use pgrc_write::seal::{PartSpec, SealedPart};
use pgrc_write::testkit::durable_upto;
use pgrc_write::walmodel::{RecordingSink, SharedLog, SpoolModel, WalLog, WalRecord};
use pgrc_write::writer::{PartCutPolicy, TableWriter};
use pgrc_write::wvfs::WriteVfs;
use pgsync::{lock, Mutex};
use std::collections::BTreeSet;
use std::sync::Arc;

const N: u64 = 900; // rows per generation (2 generations in the oracle)

fn sweep_fixture() -> Fixture {
    int8_fixture(
        "simvfs_sweep",
        201,
        2 * N,
        vec![pgrc_qa::adapters::ForcedPlan::ByteFor {
            delta_width: 8,
            signed: true,
        }],
        PartCutPolicy::default(),
        |i| {
            if i % 6 == 1 {
                None
            } else {
                Some((i as i64).wrapping_mul(0x9E37_79B9) ^ 0x55AA)
            }
        },
    )
}

struct Ctx {
    fx: Fixture,
    fxid1: u64,
    fxid2: u64,
    /// Parts by reference (WR, vfs contract v2 `link`): large files are linked into a spool directory.
    spool: Option<SpoolModel>,
}

fn ctx() -> Ctx {
    Ctx { fx: sweep_fixture(), fxid1: round_fxid(1, 0), fxid2: round_fxid(2, 0), spool: None }
}

/// [`ctx`] with parts by reference and a WAL consumer that never advanced (every spool entry is held).
fn ctx_ref() -> Ctx {
    Ctx { spool: Some(SpoolModel { dir: "/pgdata/pg_pgrc_spool".to_string(), ref_min: 1024, slot: Some(0) }), ..ctx() }
}

/// A fresh universe and WAL for `ctx`: the table directory, and the spool (durable) when by reference.
fn universe(ctx: &Ctx) -> (SimVfs, SharedLog) {
    let mut vfs = SimVfs::new();
    vfs.mkdir_path(&ctx.fx.dir).expect("mkdir");
    let log = WalLog::shared();
    if let Some(sp) = &ctx.spool {
        vfs.mkdir_path(&sp.dir).expect("mkdir spool");
        vfs.fsync_dir(&sp.dir).expect("fsync spool");
        lock(&log).spool = Some(sp.clone());
    }
    (vfs, log)
}

fn logged(fx: &Fixture, fxid: u64, log: &SharedLog) -> TableWriter {
    open_writer(fx, fxid).expect("open").with_sink(Box::new(RecordingSink::new(&fx.dir, log.clone())))
}

fn fork(log: &SharedLog) -> SharedLog {
    Arc::new(Mutex::new(lock(log).fork()))
}

/// Gen1 (rows 0..N) published, committed and checkpointed; gen2's rows (N..2N) SEALED through the WAL (temps
/// logged, not durable), with the SealedPart descriptors for replayable publishes.
fn base_state(ctx: &Ctx) -> (SimVfs, SharedLog, Probe, Vec<SealedPart>, PartSpec) {
    let (mut vfs, log) = universe(ctx);
    let mut probe = Probe::new(TxnVerdict::Aborted);
    probe.mark(ctx.fxid1, TxnVerdict::InProgress);
    let mut w = logged(&ctx.fx, ctx.fxid1, &log);
    append_fixture_rows(&mut vfs, &mut w, &ctx.fx, 0, N).expect("rows gen1");
    finish(&mut vfs, &mut w, &ctx.fx).expect("finish gen1");
    w.publish(&mut vfs, &probe).expect("publish gen1");
    lock(&log).commit(ctx.fxid1);
    lock(&log).checkpoint(&mut vfs).expect("checkpoint after gen1");
    probe.mark(ctx.fxid1, TxnVerdict::Committed);

    probe.mark(ctx.fxid2, TxnVerdict::InProgress);
    let mut w2 = logged(&ctx.fx, ctx.fxid2, &log);
    append_fixture_rows(&mut vfs, &mut w2, &ctx.fx, N, 2 * N).expect("rows gen2");
    finish(&mut vfs, &mut w2, &ctx.fx).expect("finish gen2");
    let sealed = w2.sealed_parts().to_vec();
    let spec = *w2.spec();
    (vfs, log, probe, sealed, spec)
}

fn publish_gen2(vfs: &mut SimVfs, log: &SharedLog, ctx: &Ctx, sealed: &[SealedPart], spec: &PartSpec, probe: &Probe) -> Result<(), String> {
    let mut sink = RecordingSink::new(&ctx.fx.dir, log.clone());
    publish_parts(vfs, &mut sink, &ctx.fx.dir, spec, sealed, ctx.fxid2, probe, pgrc_write::publish::BaseStats::None)
        .map(drop)
        .map_err(|e| e.to_string())
}

/// After recovery the directory holds exactly the effective generation's live set: its parts and their stats
/// sidecars, manifests and sections up to it, `CURRENT`, and its stats plane (which must prove its binding).
fn residue_check(vfs: &mut SimVfs, dir: &str, eff: Option<&Manifest>, label: &str) -> Result<(), String> {
    let after: BTreeSet<String> = vfs.list_dir(dir).map_err(|e| format!("{label}: listdir: {e}"))?.into_iter().collect();
    let mut allowed: BTreeSet<String> = BTreeSet::new();
    allowed.insert(CURRENT_FILE_NAME.to_string());
    if let Some(m) = eff {
        for p in &m.parts {
            allowed.insert(part_file_name(p.part_no));
            allowed.insert(sidecar_file_name(p.part_no, SidecarKind::Stats, 1));
        }
        for g in 1..=m.header.gen {
            allowed.insert(manifest_file_name(g));
            // D356: a kept generation's sections file is live exactly as long as its manifest (allowed, never
            // required; bound when present)
            if let Some(s) = check_sections_sidecar(vfs, dir, g).map_err(|e| format!("{label}: {e}"))? {
                allowed.insert(s);
            }
        }
        let plane = pgrc_format::bankstats::bankstats_file_name(m.header.gen);
        if after.contains(&plane) {
            let bytes = vfs.read_full(&format!("{dir}/{plane}")).map_err(|e| format!("{label}: read live plane: {e}"))?;
            let (h, pidents, _cols) = pgrc_format::bankstats::decode_meta(&bytes)
                .map_err(|e| format!("{label}: live plane undecodable: {e:?}"))?;
            let bound = h.gen == m.header.gen
                && h.schema_fingerprint == m.header.schema_fingerprint
                && h.part_count as usize == m.parts.len()
                && pidents.iter().zip(m.parts.iter()).all(|(a, b)| {
                    a.part_no == b.part_no
                        && a.granule_count == b.granule_count
                        && a.band_count == b.band_count
                        && a.rows == b.rows
                        && a.footer_off == b.footer_off
                });
            if !bound {
                return Err(format!("{label}: live plane fails the identity witness"));
            }
        }
        allowed.insert(plane);
    }
    for f in &after {
        if !allowed.contains(f) {
            return Err(format!("{label}: residue survived recovery: {f}"));
        }
    }
    Ok(())
}

/// Recover a post-crash universe where gen2's publisher never committed: effective must be EXACTLY gen1 on both
/// walks, decode == oracle prefix, the directory holds exactly the live set, and a fresh publish + commit yields
/// exactly NEW.
fn adjudicate_old_then_retry(vfs: &mut SimVfs, log: &SharedLog, upto: usize, ctx: &Ctx, label: &str) -> Result<(), String> {
    recover(vfs, log, &ctx.fx.dir, upto, &[]).map_err(|e| format!("{label}: recover: {e}"))?;
    let probe = Probe::new(TxnVerdict::Aborted)
        .set(ctx.fxid1, TxnVerdict::Committed)
        .set(ctx.fxid2, TxnVerdict::Aborted);
    let eff = effective_manifest(vfs, &ctx.fx.dir, &probe, None)
        .map_err(|e| format!("{label}: effective: {e}"))?
        .ok_or_else(|| format!("{label}: table lost gen1"))?;
    if eff.header.gen != 1 {
        return Err(format!("{label}: expected gen1, got gen{}", eff.header.gen));
    }
    let files = vfs.snapshot_dir(&ctx.fx.dir);
    match resolve_effective(&memdir_of(&files), &probe, &TableExpect::default()) {
        Ok(Some(r)) if r.manifest.header.gen == 1 => {}
        other => return Err(format!("{label}: the reader walk must agree on gen1 (#462 law), got {other:?}")),
    }
    let rows = verify_manifest(&files, &eff, &ctx.fx).map_err(|e| format!("{label}: old decode-vs-oracle: {e:?}"))?;
    if rows != N {
        return Err(format!("{label}: old rows {rows} != {N}"));
    }
    residue_check(vfs, &ctx.fx.dir, Some(&eff), label)?;

    let fxid3 = round_fxid(7, 3);
    let probe2 = probe.clone().set(fxid3, TxnVerdict::InProgress);
    let mut w3 = logged(&ctx.fx, fxid3, log);
    append_fixture_rows(vfs, &mut w3, &ctx.fx, N, 2 * N).map_err(|e| format!("{label}: reseal: {e}"))?;
    finish(vfs, &mut w3, &ctx.fx).map_err(|e| format!("{label}: finish3: {e}"))?;
    w3.publish(vfs, &probe2).map_err(|e| format!("{label}: retry publish: {e}"))?;
    lock(log).commit(fxid3);
    let probe3 = Probe::new(TxnVerdict::Aborted)
        .set(ctx.fxid1, TxnVerdict::Committed)
        .set(fxid3, TxnVerdict::Committed);
    let eff2 = effective_manifest(vfs, &ctx.fx.dir, &probe3, None)
        .map_err(|e| format!("{label}: effective2: {e}"))?
        .ok_or_else(|| format!("{label}: retry lost table"))?;
    let rows2 = verify_manifest(&vfs.snapshot_dir(&ctx.fx.dir), &eff2, &ctx.fx)
        .map_err(|e| format!("{label}: new decode-vs-oracle: {e:?}"))?;
    if rows2 != 2 * N {
        return Err(format!("{label}: new rows {rows2} != {}", 2 * N));
    }
    Ok(())
}

/// Recover a universe whose gen2 publisher committed: gen2 is effective and decodes to the full oracle.
fn adjudicate_new(vfs: &mut SimVfs, ctx: &Ctx, label: &str) -> Result<(), String> {
    let probe = Probe::new(TxnVerdict::Aborted)
        .set(ctx.fxid1, TxnVerdict::Committed)
        .set(ctx.fxid2, TxnVerdict::Committed);
    let eff = effective_manifest(vfs, &ctx.fx.dir, &probe, None)
        .map_err(|e| format!("{label}: effective: {e}"))?
        .ok_or_else(|| format!("{label}: committed table lost"))?;
    if eff.header.gen != 2 {
        return Err(format!("{label}: committed gen2 not effective (gen{})", eff.header.gen));
    }
    let rows = verify_manifest(&vfs.snapshot_dir(&ctx.fx.dir), &eff, &ctx.fx)
        .map_err(|e| format!("{label}: committed decode-vs-oracle: {e:?}"))?;
    if rows != 2 * N {
        return Err(format!("{label}: committed rows {rows} != {}", 2 * N));
    }
    Ok(())
}

/// The main sweep: every op boundary of the publish window × seeds × {flushed WAL, all WAL}.
#[test]
fn publish_boundary_sweep_torn_sectors_and_dirent_loss() {
    let s = common::scale();
    let ctx = ctx();
    let (vfs0, log0, probe0, sealed, spec) = base_state(&ctx);
    let total_ops = {
        let mut v = vfs0.clone();
        let before = v.op_count();
        publish_gen2(&mut v, &fork(&log0), &ctx, &sealed, &spec, &probe0).expect("uncrashed publish");
        v.op_count() - before
    };
    assert!(total_ops >= 15, "publish window has only {total_ops} vfs ops — sweep denominator broken");

    let mut trials = 0u64;
    for n in 1..=total_ops {
        let mut crashed = vfs0.clone();
        let log_n = fork(&log0);
        crashed.crash_at_op(n);
        assert!(publish_gen2(&mut crashed, &log_n, &ctx, &sealed, &spec, &probe0).is_err(), "op {n}: a killed publish acked");
        assert!(crashed.killed(), "crash flag not set at op {n}");
        for seed in 0..s.simvfs_seeds {
            let mut universe = crashed.clone();
            universe.crash_and_revive(&mut XorShift::new(0xC0FFEE ^ (n << 8) ^ seed));
            let log_s = fork(&log_n);
            let upto = durable_upto(&log_s, seed % 2 == 1);
            trials += 1;
            if let Err(msg) = adjudicate_old_then_retry(&mut universe, &log_s, upto, &ctx, &format!("op{n} seed{seed}")) {
                panic!("SimVfs battery failure: {msg}");
            }
        }
    }
    assert_eq!(trials, total_ops * s.simvfs_seeds);
    println!("simvfs sweep: {total_ops} boundaries x {} seeds = {trials} universes, all old-or-new", s.simvfs_seeds);
}

/// Seal-window crashes compose the same way (coarse stride — D's matrix owns the exhaustive seal sweep; this leg
/// proves the COMPOSITION).
#[test]
fn seal_window_crash_composes() {
    seal_window(&ctx());
}

/// WR: the same seal-window composition with parts by reference: every crash point of write, fsync, pending link,
/// spool fsync, rename and publish, under torn sectors and dirent subsets in both directories.
#[test]
fn seal_window_crash_composes_by_reference() {
    let ctx = ctx_ref();
    let (mut vfs, log) = universe(&ctx);
    let mut probe = Probe::new(TxnVerdict::Aborted).set(ctx.fxid1, TxnVerdict::InProgress);
    let mut w = logged(&ctx.fx, ctx.fxid1, &log);
    append_fixture_rows(&mut vfs, &mut w, &ctx.fx, 0, N).expect("rows");
    finish(&mut vfs, &mut w, &ctx.fx).expect("finish");
    w.publish(&mut vfs, &probe).expect("publish");
    probe.mark(ctx.fxid1, TxnVerdict::Committed);
    let refs = lock(&log).records.iter().filter(|r| matches!(r, WalRecord::FileRef { .. })).count();
    assert!(refs >= 1, "the by-reference fixture logged no reference");
    assert!(vfs.ops().iter().any(|o| o.starts_with("link ")), "no link went through the vfs");
    seal_window(&ctx);
}

fn seal_window(ctx: &Ctx) {
    let s = common::scale();
    let (mut vfs, log) = universe(ctx);
    let mut probe = Probe::new(TxnVerdict::Aborted);
    probe.mark(ctx.fxid1, TxnVerdict::InProgress);
    let mut w = logged(&ctx.fx, ctx.fxid1, &log);
    append_fixture_rows(&mut vfs, &mut w, &ctx.fx, 0, N).expect("rows gen1");
    finish(&mut vfs, &mut w, &ctx.fx).expect("finish gen1");
    w.publish(&mut vfs, &probe).expect("publish gen1");
    lock(&log).commit(ctx.fxid1);
    lock(&log).checkpoint(&mut vfs).expect("checkpoint after gen1");
    let base_ops = vfs.op_count();
    let p = Probe::new(TxnVerdict::Aborted)
        .set(ctx.fxid1, TxnVerdict::Committed)
        .set(ctx.fxid2, TxnVerdict::InProgress);
    let run = |v: &mut SimVfs, log: &SharedLog| -> Result<(), String> {
        let mut w2 = logged(&ctx.fx, ctx.fxid2, log);
        append_fixture_rows(v, &mut w2, &ctx.fx, N, 2 * N)
            .and_then(|()| finish(v, &mut w2, &ctx.fx))
            .and_then(|()| w2.publish(v, &p).map(drop))
            .map_err(|e| e.to_string())
    };
    let window = {
        let mut v = vfs.clone();
        run(&mut v, &fork(&log)).expect("uncrashed seal + publish");
        v.op_count() - base_ops
    };
    let stride = (window / 9).max(1);
    let mut points = 0u64;
    let mut n = 1;
    while n <= window {
        let mut crashed = vfs.clone();
        let log_n = fork(&log);
        crashed.crash_at_op(n);
        assert!(run(&mut crashed, &log_n).is_err(), "seal op {n}: a killed seal + publish acked");
        assert!(crashed.killed(), "armed crash at op {n} did not fire");
        for seed in 0..s.simvfs_seeds.min(2) {
            let mut universe = crashed.clone();
            universe.crash_and_revive(&mut XorShift::new(0xBEEF ^ (n << 6) ^ seed));
            let log_s = fork(&log_n);
            let upto = durable_upto(&log_s, seed % 2 == 1);
            if let Err(msg) = adjudicate_old_then_retry(&mut universe, &log_s, upto, ctx, &format!("seal op{n} seed{seed}")) {
                panic!("seal-window composition failure: {msg}");
            }
            let n_all = all_records(&log_s);
            let spool = lock(&log_s).check_spool(&mut universe, 0, n_all);
            if let Err(msg) = spool {
                panic!("seal op{n} seed{seed}: {msg}");
            }
        }
        points += 1;
        n += stride;
    }
    assert!(points >= 8, "seal window stride produced only {points} points");
}

/// D356's teeth: the checkers keep a generation's sections file only when it
/// names its own manifest (gen, publisher fxid, the manifest file's crc). A
/// sections file assembled for another manifest, or bytes that do not decode,
/// fail the check instead of riding through as allowed residue.
#[test]
fn born_red_foreign_sections_file_is_caught() {
    let ctx = Ctx { fxid1: round_fxid(3, 0), fxid2: round_fxid(4, 0), ..ctx() };
    let (mut vfs, _, _, _, _) = base_state(&ctx);
    let dir = ctx.fx.dir.clone();
    let name = pgrc_format::dirlayout::sections_file_name(1);
    assert_eq!(check_sections_sidecar(&mut vfs, &dir, 1), Ok(None), "P-1 L2: gen1's publisher writes no sections file");
    let mbytes = vfs.read_full(&format!("{dir}/{}", manifest_file_name(1))).expect("manifest-1");
    // a pre-P-1 directory's bound sections file for gen 1 passes the check
    let m1 = Manifest::decode(&mbytes).expect("decode manifest-1");
    let bound = pgrc_format::sections::assemble(&m1, pgrc_format::sections::manifest_crc(&mbytes), &[]).expect("assemble");
    let mut v = vfs.clone();
    let fd = v.create_rw(&format!("{dir}/{name}")).expect("create");
    v.pwrite_at(&fd, 0, &bound).expect("write");
    v.close_file(fd).expect("close");
    assert_eq!(check_sections_sidecar(&mut v, &dir, 1), Ok(Some(name.clone())), "a bound sections file is kept");
    let mut other = Manifest::decode(&mbytes).expect("decode manifest-1");
    other.header.publisher_fxid ^= 1;
    let foreign = pgrc_format::sections::assemble(&other, pgrc_format::sections::manifest_crc(&mbytes), &[]).expect("assemble");
    for (what, bytes) in [("another manifest's", &foreign[..]), ("undecodable", &b"x"[..])] {
        let mut v = vfs.clone();
        let fd = v.create_rw(&format!("{dir}/{name}")).expect("create");
        v.pwrite_at(&fd, 0, bytes).expect("write");
        v.close_file(fd).expect("close");
        assert!(check_sections_sidecar(&mut v, &dir, 1).is_err(), "{what} sections file passed the check");
    }
}

fn committed_gen2(ctx: &Ctx) -> (SimVfs, SharedLog) {
    let (vfs0, log0, probe0, sealed, spec) = base_state(ctx);
    let mut v = vfs0.clone();
    let log = fork(&log0);
    publish_gen2(&mut v, &log, ctx, &sealed, &spec, &probe0).expect("publish");
    lock(&log).commit(ctx.fxid2);
    (v, log)
}

/// The acked arm: a COMPLETED publish whose commit record was flushed survives every adversarial persistence
/// outcome wholesale once recovery replays it (#253).
#[test]
fn committed_publish_survives_any_adversary() {
    committed_survives(&ctx());
}

/// WR: a committed by-reference publish survives every adversarial outcome, and every reference in the flushed
/// WAL still has its bytes in the spool after recovery (the pending link or the renamed entry, whichever survived).
#[test]
fn committed_publish_by_reference_survives_any_adversary() {
    committed_survives(&ctx_ref());
}

fn committed_survives(ctx: &Ctx) {
    let s = common::scale();
    let (v, log) = committed_gen2(ctx);
    for seed in 0..(s.simvfs_seeds * 2).max(4) {
        let mut universe = v.clone();
        universe.crash_and_revive(&mut XorShift::new(0xACED ^ seed));
        let log_s = fork(&log);
        let upto = durable_upto(&log_s, false);
        recover(&mut universe, &log_s, &ctx.fx.dir, upto, &[]).expect("recover");
        adjudicate_new(&mut universe, ctx, &format!("seed {seed}")).unwrap_or_else(|e| panic!("{e}"));
        let spool = lock(&log_s).check_spool(&mut universe, 0, upto);
        spool.unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        let eff = effective_manifest(&mut universe, &ctx.fx.dir, &Probe::new(TxnVerdict::Committed), None)
            .expect("effective");
        residue_check(&mut universe, &ctx.fx.dir, eff.as_ref(), &format!("seed {seed}")).unwrap_or_else(|e| panic!("{e}"));
    }
}

/// Born-RED: recovery WITHOUT the replay (cleanup alone) must lose the committed publish under some outcome —
/// nothing of gen2 was fsynced, so the WAL is what makes it durable.
#[test]
fn born_red_recovery_without_replay_is_caught() {
    let ctx = ctx();
    let (v, _log) = committed_gen2(&ctx);
    let probe = Probe::new(TxnVerdict::Aborted)
        .set(ctx.fxid1, TxnVerdict::Committed)
        .set(ctx.fxid2, TxnVerdict::Committed);
    let mut caught = false;
    for seed in 0..16u64 {
        let mut universe = v.clone();
        universe.crash_and_revive(&mut XorShift::new(0xDEAD_2026 ^ seed));
        let cleaned = cleanup_dir(&mut universe, &mut pgrc_write::walsink::NoWal, &ctx.fx.dir, &probe);
        if cleaned.is_err() || adjudicate_new(&mut universe, &ctx, "no replay").is_err() {
            caught = true;
            break;
        }
    }
    assert!(caught, "born-RED failure: recovery without replay kept every committed byte under every outcome — the battery does not depend on the WAL");
}

/// The #462 state every recovery leg below starts from: gen1 committed and checkpointed, gen2 published by an
/// fxid that never committed, then a crash; recovery will replay gen2 (CURRENT names it) and must reclaim it.
fn dead_gen2_state(ctx: &Ctx) -> (SimVfs, SharedLog, Probe) {
    let (vfs0, log0, probe0, sealed, spec) = base_state(ctx);
    let mut v = vfs0.clone();
    let log = fork(&log0);
    publish_gen2(&mut v, &log, ctx, &sealed, &spec, &probe0).expect("publish of gen2");
    v.crash_and_revive(&mut XorShift::new(0x0462));
    let probe = Probe::new(TxnVerdict::Aborted)
        .set(ctx.fxid1, TxnVerdict::Committed)
        .set(ctx.fxid2, TxnVerdict::Aborted);
    (v, log, probe)
}

fn all_records(log: &SharedLog) -> usize {
    durable_upto(log, true)
}

/// #462 leg 1: crash at EVERY op boundary of the recovery window × seeds. A crash mid-recovery must itself be
/// old-or-new: re-recovery completes, both walks agree on gen1, residue is reclaimed, retry yields NEW.
#[test]
fn recovery_window_crash_sweep_never_dangles() {
    let s = common::scale();
    let ctx = ctx();
    let (v, log, _probe) = dead_gen2_state(&ctx);
    let upto = all_records(&log);
    let window = {
        let mut c = v.clone();
        let before = c.op_count();
        let rep = recover(&mut c, &fork(&log), &ctx.fx.dir, upto, &[]).expect("uncrashed recovery");
        assert!(rep.current_repointed, "premise: this state requires a repoint");
        assert!(rep.removed.iter().any(|n| n == &manifest_file_name(2)), "premise: dead gen2 reclaimed");
        c.op_count() - before
    };
    assert!(window >= 10, "recovery window has only {window} vfs ops — sweep denominator broken");
    let mut trials = 0u64;
    for n in 1..=window {
        let mut crashed = v.clone();
        let log_n = fork(&log);
        crashed.crash_at_op(n);
        assert!(recover(&mut crashed, &log_n, &ctx.fx.dir, upto, &[]).is_err(), "armed crash at recovery op {n} did not fire");
        assert!(crashed.killed(), "crash flag not set at recovery op {n}");
        for seed in 0..s.simvfs_seeds {
            let mut universe = crashed.clone();
            universe.crash_and_revive(&mut XorShift::new(0x4620_0462 ^ (n << 8) ^ seed));
            trials += 1;
            if let Err(msg) = adjudicate_old_then_retry(&mut universe, &fork(&log_n), upto, &ctx, &format!("recovery op{n} seed{seed}")) {
                panic!("recovery-window sweep failure: {msg}");
            }
        }
    }
    assert_eq!(trials, window * s.simvfs_seeds);
    println!("recovery sweep: {window} boundaries x {} seeds = {trials} universes, all old-or-new", s.simvfs_seeds);
}

/// #462 leg 2, empty-table shape: the ONLY generation was published but never committed; recovery unlinks CURRENT
/// (empty-table posture). Crash at every boundary of THAT window: re-recovery must leave both walks agreeing on
/// EMPTY, and a fresh publish + commit must yield exactly NEW.
#[test]
fn recovery_window_crash_sweep_empty_table() {
    let s = common::scale();
    let ctx = ctx();
    let mut v = SimVfs::new();
    v.mkdir_path(&ctx.fx.dir).expect("mkdir");
    let log = WalLog::shared();
    let probe_pub = Probe::new(TxnVerdict::Aborted).set(ctx.fxid1, TxnVerdict::InProgress);
    let mut w = logged(&ctx.fx, ctx.fxid1, &log);
    append_fixture_rows(&mut v, &mut w, &ctx.fx, 0, N).expect("rows gen1");
    finish(&mut v, &mut w, &ctx.fx).expect("finish gen1");
    w.publish(&mut v, &probe_pub).expect("publish gen1");
    v.crash_and_revive(&mut XorShift::new(0x4620));
    let probe = Probe::new(TxnVerdict::Aborted);
    let upto = all_records(&log);
    let window = {
        let mut c = v.clone();
        let before = c.op_count();
        let rep = recover(&mut c, &fork(&log), &ctx.fx.dir, upto, &[]).expect("uncrashed recovery");
        assert!(rep.current_repointed, "premise: CURRENT unlinked");
        assert_eq!(rep.effective_gen, 0);
        c.op_count() - before
    };
    assert!(window >= 6, "empty-table recovery window only {window} ops");
    for n in 1..=window {
        let mut crashed = v.clone();
        let log_n = fork(&log);
        crashed.crash_at_op(n);
        assert!(recover(&mut crashed, &log_n, &ctx.fx.dir, upto, &[]).is_err(), "armed crash at op {n} did not fire");
        for seed in 0..s.simvfs_seeds {
            let mut universe = crashed.clone();
            universe.crash_and_revive(&mut XorShift::new(0x4620_EE ^ (n << 8) ^ seed));
            let label = format!("empty op{n} seed{seed}");
            let log_s = fork(&log_n);
            recover(&mut universe, &log_s, &ctx.fx.dir, upto, &[]).unwrap_or_else(|e| panic!("{label}: re-recovery: {e}"));
            let eff = effective_manifest(&mut universe, &ctx.fx.dir, &probe, None)
                .unwrap_or_else(|e| panic!("{label}: effective: {e}"));
            assert!(eff.is_none(), "{label}: writer walk resurrected a dead gen");
            let files = universe.snapshot_dir(&ctx.fx.dir);
            match resolve_effective(&memdir_of(&files), &probe, &TableExpect::default()) {
                Ok(None) => {}
                other => panic!("{label}: reader walk must see EMPTY, got {other:?}"),
            }
            residue_check(&mut universe, &ctx.fx.dir, None, &label).unwrap_or_else(|e| panic!("{e}"));
            let fxid3 = round_fxid(9, 5);
            let probe2 = Probe::new(TxnVerdict::Aborted).set(fxid3, TxnVerdict::InProgress);
            let mut w3 = logged(&ctx.fx, fxid3, &log_s);
            append_fixture_rows(&mut universe, &mut w3, &ctx.fx, 0, N).unwrap_or_else(|e| panic!("{label}: rows: {e}"));
            finish(&mut universe, &mut w3, &ctx.fx).unwrap_or_else(|e| panic!("{label}: finish: {e}"));
            w3.publish(&mut universe, &probe2).unwrap_or_else(|e| panic!("{label}: publish: {e}"));
            lock(&log_s).commit(fxid3);
            let probe3 = Probe::new(TxnVerdict::Aborted).set(fxid3, TxnVerdict::Committed);
            let eff2 = effective_manifest(&mut universe, &ctx.fx.dir, &probe3, None)
                .unwrap_or_else(|e| panic!("{label}: effective2: {e}"))
                .unwrap_or_else(|| panic!("{label}: retry lost table"));
            assert_eq!(eff2.header.gen, 1, "{label}");
            let rows = verify_manifest(&universe.snapshot_dir(&ctx.fx.dir), &eff2, &ctx.fx)
                .unwrap_or_else(|e| panic!("{label}: decode-vs-oracle: {e:?}"));
            assert_eq!(rows, N, "{label}");
        }
    }
}

/// Born-RED tooth 1: the PRE-#462 cleanup shape — remove the dead generation, never touch CURRENT — must be
/// REJECTED by the strict post-recovery agreement check (the exact signature the old code left).
#[test]
fn born_red_remove_without_repoint_is_caught() {
    let ctx = ctx();
    let (mut v, log, probe) = dead_gen2_state(&ctx);
    let upto = all_records(&log);
    replay(&mut v, &log, upto).expect("replay");
    // SIN (the old cleanup): unlink manifest-2, leave CURRENT.
    v.unlink_path(&format!("{}/{}", ctx.fx.dir, manifest_file_name(2))).expect("unlink manifest-2");
    let files = v.snapshot_dir(&ctx.fx.dir);
    match resolve_effective(&memdir_of(&files), &probe, &TableExpect::default()) {
        Err(pgrc_read::ReadError::ManifestMissing { gen: 2 }) => {}
        other => panic!(
            "born-RED failure: remove-without-repoint was NOT caught by the strict post-recovery check — got {other:?}"
        ),
    }
}

/// Born-RED tooth 2: cleanup has no barrier between its repoint and its removals (the end-of-recovery checkpoint
/// is the barrier), so a crash inside it that is NOT followed by a rerun of recovery must leave some outcome
/// dangling — rerunning recovery is what the sweep above proves repairs it.
#[test]
fn born_red_crashed_recovery_not_rerun_is_caught() {
    let ctx = ctx();
    let (v, log, probe) = dead_gen2_state(&ctx);
    let upto = all_records(&log);
    let window = {
        let mut c = v.clone();
        let before = c.op_count();
        recover(&mut c, &fork(&log), &ctx.fx.dir, upto, &[]).expect("uncrashed recovery");
        c.op_count() - before
    };
    let mut caught = false;
    'sweep: for n in 1..=window {
        let mut crashed = v.clone();
        crashed.crash_at_op(n);
        let _ = recover(&mut crashed, &fork(&log), &ctx.fx.dir, upto, &[]);
        for seed in 0..64u64 {
            let mut universe = crashed.clone();
            universe.crash_and_revive(&mut XorShift::new(0x0462_BAD0 ^ (n << 8) ^ seed));
            let files = universe.snapshot_dir(&ctx.fx.dir);
            if matches!(
                resolve_effective(&memdir_of(&files), &probe, &TableExpect::default()),
                Err(pgrc_read::ReadError::ManifestMissing { gen: 2 })
            ) {
                caught = true;
                break 'sweep;
            }
        }
    }
    assert!(
        caught,
        "born-RED failure: no crash inside cleanup left CURRENT dangling without a rerun — the dirent-loss detector is toothless"
    );
}
