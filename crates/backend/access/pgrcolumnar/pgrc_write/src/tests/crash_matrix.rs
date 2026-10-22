//! The crash matrix under WAL (delta v2 design §7.6): a crash at EVERY vfs op of a second transaction's seal and
//! publish, with and without a checkpoint after the first, and with only the flushed WAL or all of it surviving.
//! Recovery is replay from the last checkpoint + `cleanup_dir` + the end-of-recovery checkpoint, and must give
//! exactly old-or-new: generation 2 iff its commit record survived, acked bytes intact, no residue, recovery
//! idempotent (a crash inside it changes nothing), and a retry publishes cleanly.

use super::*;
use crate::cleanup::cleanup_dir;
use crate::publish::{effective_manifest, TxnVerdict};
use crate::walmodel::{RecordingSink, WalLog};
use crate::WriteResult;
use pgsync::{lock, Mutex};
use std::sync::Arc;

struct Scenario {
    vfs: MemVfs,
    log: Arc<Mutex<WalLog>>,
    gen1_part: Vec<u8>,
    completed: bool,
    ops_before_window: u64,
}

/// Gen 1 by txn 100 (1000 rows, committed), then txn 200 seals + publishes gen 2 (500 rows) with a crash armed
/// `k` ops into that window; txn 200 commits iff its publish completed and `commit`.
fn scenario(crash_at: Option<u64>, checkpoint_after_gen1: bool, commit: bool) -> Scenario {
    let mut vfs = mem_with_dir();
    vfs.fsync_dir(DIR).expect("dir");
    let log = WalLog::shared();
    let mut kit = Kit::new();
    let probe = Probe::new(TxnVerdict::InProgress).set(100, TxnVerdict::Committed);
    let mut w1 = logged_writer(vec![int8_col(1)], stamp(100, 1), &log);
    append_int8_rows(&mut w1, &mut vfs, &mut kit, 1000, |i| Some(i as i64));
    finish_and_publish(&mut w1, &mut vfs, &mut kit, &probe);
    lock(&log).commit(100);
    if checkpoint_after_gen1 {
        lock(&log).checkpoint(&mut vfs).expect("checkpoint");
    }
    let gen1_part = vfs.read_full(&format!("{DIR}/part-0.pgrc")).expect("part");

    let mut w2 = logged_writer(vec![int8_col(1)], stamp(200, 1), &log);
    append_int8_rows(&mut w2, &mut vfs, &mut kit, 500, |i| Some(i as i64 * 3));
    let ops_before_window = vfs.op_count();
    if let Some(k) = crash_at {
        vfs.crash_at_op(k);
    }
    let completed = (|| -> WriteResult<()> {
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv { vfs: &mut vfs, sources: &sources, resolver: &kit.resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
        w2.finish(&mut env)?;
        w2.publish(&mut vfs, &probe)?;
        Ok(())
    })()
    .is_ok();
    if completed && commit {
        lock(&log).commit(200);
    }
    Scenario { vfs, log, gen1_part, completed, ops_before_window }
}

fn image(vfs: &mut MemVfs) -> BTreeMap<String, Vec<u8>> {
    vfs.list_dir(DIR).expect("list").into_iter().map(|n| {
        let b = vfs.read_full(&format!("{DIR}/{n}")).expect("read");
        (n, b)
    }).collect()
}

fn allowed(gen: u64, name: &str) -> bool {
    let g1 = ["CURRENT", "manifest-1.pgrcm", "part-0.pgrc", "sections-1.pgrcx", "part-0-stats-1.pgrcs"];
    let g2 = ["manifest-2.pgrcm", "part-1.pgrc", "sections-2.pgrcx", "part-1-stats-1.pgrcs"];
    g1.contains(&name) || name == format!("bankstats-{gen}.pgrcbs") || (gen == 2 && g2.contains(&name))
}

fn check_recovered(s: &mut Scenario, want_gen: u64, gen2_part: &[u8], what: &str) {
    let probe = committed_probe(&s.log);
    let eff = effective_manifest(&mut s.vfs, DIR, &probe, None).expect("walk").expect("an effective generation");
    assert_eq!(eff.header.gen, want_gen, "{what}");
    assert!(s.vfs.read_full(&format!("{DIR}/part-0.pgrc")).expect("part-0") == s.gen1_part, "acked gen-1 bytes ({what})");
    if want_gen == 2 {
        assert!(s.vfs.read_full(&format!("{DIR}/part-1.pgrc")).expect("part-1") == gen2_part, "acked gen-2 bytes ({what})");
    }
    let names = s.vfs.list_dir(DIR).expect("list");
    for n in &names {
        assert!(allowed(want_gen, n), "residue {n} survived recovery ({what}): {names:?}");
    }
    let cp = pgrc_format::manifest::CommitPointer::decode(&s.vfs.read_full(&format!("{DIR}/CURRENT")).expect("CURRENT")).expect("pointer");
    assert_eq!(cp.gen, want_gen, "CURRENT names the effective generation ({what})");
}

#[test]
fn every_crash_point_recovers_to_exactly_old_or_new() {
    let mut rehearsal = scenario(None, false, true);
    assert!(rehearsal.completed);
    let window = rehearsal.vfs.op_count() - rehearsal.ops_before_window;
    assert!(window > 20, "window too small to be a real sweep: {window}");
    let ops = rehearsal.vfs.ops()[rehearsal.ops_before_window as usize..].to_vec();
    let gen2_part = rehearsal.vfs.read_full(&format!("{DIR}/part-1.pgrc")).expect("part-1");
    for k in 1..=window + 1 {
        for ckpt in [false, true] {
            for all in [false, true] {
                let what = format!("crash at op {k}, checkpoint after gen 1 {ckpt}, all WAL {all}");
                let mut s = scenario(Some(k), ckpt, true);
                assert_eq!(s.completed, k > window, "{what}: {:?}", ops.get(k as usize - 1));
                let upto = {
                    let g = lock(&s.log);
                    if all { g.records.len() } else { g.flushed }
                };
                let want = if s.completed { 2 } else { 1 };
                crash_and_recover(&mut s.vfs, &s.log, upto);
                check_recovered(&mut s, want, &gen2_part, &what);

                let settled = image(&mut s.vfs);
                let all_records = lock(&s.log).records.len();
                crash_and_recover(&mut s.vfs, &s.log, all_records);
                assert_eq!(image(&mut s.vfs), settled, "a second crash + recovery changes nothing ({what})");

                let mut kit = Kit::new();
                let probe = committed_probe(&s.log).set(300, TxnVerdict::InProgress);
                let mut w3 = logged_writer(vec![int8_col(1)], stamp(300, 1), &s.log);
                append_int8_rows(&mut w3, &mut s.vfs, &mut kit, 7, |i| Some(i as i64 * 5));
                let out = finish_and_publish(&mut w3, &mut s.vfs, &mut kit, &probe);
                assert_eq!(out.gen, want + 1, "retry ({what})");
            }
        }
    }
}

/// A publish that completed but whose commit record never made it: the replayed generation is dead, cleanup
/// reclaims every byte of it, and gen 1 stays effective.
#[test]
fn a_completed_publish_without_its_commit_is_reclaimed() {
    let mut rehearsal = scenario(None, false, true);
    let gen2_part = rehearsal.vfs.read_full(&format!("{DIR}/part-1.pgrc")).expect("part-1");
    for ckpt in [false, true] {
        let mut s = scenario(None, ckpt, false);
        assert!(s.completed);
        let upto = lock(&s.log).records.len();
        let rep = crash_and_recover(&mut s.vfs, &s.log, upto);
        assert_eq!(rep.effective_gen, 1);
        assert!(rep.removed.iter().any(|n| n == "part-1.pgrc"), "{:?}", rep.removed);
        check_recovered(&mut s, 1, &gen2_part, "uncommitted");
    }
}

/// A crash inside recovery, after replay and cleanup but before the end-of-recovery checkpoint, recovers to the
/// same bytes as an uninterrupted recovery.
#[test]
fn a_crash_inside_recovery_recovers_to_the_same_bytes() {
    let mut clean = scenario(Some(40), false, true);
    let upto = lock(&clean.log).records.len();
    crash_and_recover(&mut clean.vfs, &clean.log, upto);
    let want = image(&mut clean.vfs);

    let mut s = scenario(Some(40), false, true);
    let upto = lock(&s.log).records.len();
    crate::testkit::crash_and_revive(&mut s.vfs);
    {
        let mut g = lock(&s.log);
        g.crash(upto);
        g.replay(&mut s.vfs, upto).expect("replay");
    }
    let probe = committed_probe(&s.log);
    cleanup_dir(&mut s.vfs, &mut RecordingSink::new(DIR, s.log.clone()), DIR, &probe).expect("cleanup");
    crash_and_recover(&mut s.vfs, &s.log, upto);
    assert_eq!(image(&mut s.vfs), want);
}

// ---------------------------------------------------------------------------
// The compile publish (F2 L7): a crash at every op of a second compile, before its PUBLISH, after it and after
// its commit, recovers to the old DV or the new one, never a mix.
// ---------------------------------------------------------------------------

use crate::dml::DvCompile;
use crate::publish::publish_compile;
use crate::walmodel::RecordingSink as Sink;
use pgrc_format::sidecar::SidecarKind;

const OLD_ROWS: std::ops::RangeInclusive<u16> = 1..=10;
const NEW_ROWS: std::ops::RangeInclusive<u16> = 5..=20;

fn compile(vfs: &mut MemVfs, log: &Arc<Mutex<WalLog>>, spec: &crate::seal::PartSpec, rows: &[(u32, u16)], fxid: u64, probe: &dyn TxnProbe) -> WriteResult<u64> {
    let mut sink = Sink::new(DIR, log.clone());
    let out = publish_compile(vfs, &mut sink, DIR, spec, &[(0, rows)], fxid, probe, (None, None))?;
    Ok(out.expect("a compile that adds rows publishes").gen)
}

/// Gen 1 (txn 100, 1000 rows), gen 2 = the first compile (txn 150, rows 1..=10 of granule 0), then txn 200 compiles
/// rows 5..=20 with a crash armed `k` ops in; txn 200 commits iff its compile completed and `commit`.
fn compile_scenario(crash_at: Option<u64>, ckpt: bool, commit: bool) -> (MemVfs, Arc<Mutex<WalLog>>, crate::seal::PartSpec, bool, u64) {
    let mut vfs = mem_with_dir();
    vfs.fsync_dir(DIR).expect("dir");
    let log = WalLog::shared();
    let mut kit = Kit::new();
    let probe = Probe::new(TxnVerdict::InProgress).set(100, TxnVerdict::Committed);
    let mut w1 = logged_writer(vec![int8_col(1)], stamp(100, 1), &log);
    append_int8_rows(&mut w1, &mut vfs, &mut kit, 1000, |i| Some(i as i64));
    finish_and_publish(&mut w1, &mut vfs, &mut kit, &probe);
    lock(&log).commit(100);
    let spec = *w1.spec();
    let old: Vec<(u32, u16)> = OLD_ROWS.map(|r| (0, r)).collect();
    assert_eq!(compile(&mut vfs, &log, &spec, &old, 150, &probe).expect("first compile"), 2);
    lock(&log).commit(150);
    if ckpt {
        lock(&log).checkpoint(&mut vfs).expect("checkpoint");
    }
    let probe = probe.set(150, TxnVerdict::Committed);
    let new: Vec<(u32, u16)> = NEW_ROWS.map(|r| (0, r)).collect();
    let ops_before = vfs.op_count();
    if let Some(k) = crash_at {
        vfs.crash_at_op(k);
    }
    let completed = compile(&mut vfs, &log, &spec, &new, 200, &probe).is_ok();
    if completed && commit {
        lock(&log).commit(200);
    }
    (vfs, log, spec, completed, ops_before)
}

/// The effective generation is `want`; part 0's record names a DV file that matches its triplet and holds exactly
/// that generation's deletion set; no temp and no DV of a dead compile survives.
fn check_compiled(vfs: &mut MemVfs, log: &Arc<Mutex<WalLog>>, want: u64, what: &str) {
    let probe = committed_probe(log);
    let eff = effective_manifest(vfs, DIR, &probe, None).expect("walk").expect("effective");
    assert_eq!(eff.header.gen, want, "{what}");
    let rec = eff.parts[0];
    assert_eq!(rec.dv_gen, want, "{what}");
    let file = vfs.read_full(&format!("{DIR}/part-0-dv-{want}.pgrcs")).expect("the named DV");
    let (_, payload) = pgrc_format::sidecar::SidecarFileHeader::validate_file(&file).expect("envelope");
    assert_eq!((payload.len() as u64, pgrc_format::wire::crc32c(payload)), (rec.dv_len, rec.dv_crc), "triplet ({what})");
    let mut set = DvCompile::new();
    OLD_ROWS.for_each(|r| set.mark(0, r));
    if want == 3 {
        NEW_ROWS.for_each(|r| set.mark(0, r));
    }
    assert_eq!(payload, &set.encode(0, want).expect("encode")[..], "old set or new set, never a mix ({what})");
    for n in vfs.list_dir(DIR).expect("list") {
        let dead_dv = pgrc_format::dirlayout::parse_sidecar_file_name(&n).is_some_and(|(_, _, g)| g > want);
        assert!(!n.starts_with("tmp-") && !dead_dv, "residue {n} ({what})");
    }
}

#[test]
fn every_crash_point_of_a_compile_recovers_to_the_old_dv_or_the_new() {
    let (rehearsal, _, _, completed, before) = compile_scenario(None, false, true);
    assert!(completed);
    let window = rehearsal.op_count() - before;
    assert!(window > 5, "window too small to be a real sweep: {window}");
    for k in 1..=window + 1 {
        for ckpt in [false, true] {
            for all in [false, true] {
                let what = format!("crash at op {k}, checkpoint {ckpt}, all WAL {all}");
                let (mut vfs, log, spec, completed, _) = compile_scenario(Some(k), ckpt, true);
                assert_eq!(completed, k > window, "{what}");
                let want = if completed { 3 } else { 2 };
                crash_and_recover(&mut vfs, &log, durable_upto(&log, all));
                check_compiled(&mut vfs, &log, want, &what);
                let probe = committed_probe(&log).set(300, TxnVerdict::InProgress);
                assert_eq!(compile(&mut vfs, &log, &spec, &[(0, 30)], 300, &probe).expect("retry"), want + 1, "retry ({what})");
            }
        }
    }
}

/// Killed after its PUBLISH, before its commit: the old DV stays effective and the new one is reclaimed.
#[test]
fn a_compile_published_but_not_committed_keeps_the_old_dv() {
    for ckpt in [false, true] {
        let (mut vfs, log, _, completed, _) = compile_scenario(None, ckpt, false);
        assert!(completed);
        let rep = crash_and_recover(&mut vfs, &log, durable_upto(&log, true));
        assert!(rep.removed.iter().any(|n| n == "part-0-dv-3.pgrcs"), "{:?}", rep.removed);
        check_compiled(&mut vfs, &log, 2, "uncommitted");
    }
}

/// A stale DV file at the name a fresh part's first compile will use (a directory reset under it, or a dead
/// compile's leftover) is never read: a part with `dv_gen == 0` seeds nothing, and the staged file replaces it.
#[test]
fn a_stale_dv_file_never_seeds_a_fresh_part() {
    let mut vfs = mem_with_dir();
    let log = WalLog::shared();
    let mut kit = Kit::new();
    let probe = Probe::new(TxnVerdict::InProgress).set(100, TxnVerdict::Committed);
    let mut w1 = logged_writer(vec![int8_col(1)], stamp(100, 1), &log);
    append_int8_rows(&mut w1, &mut vfs, &mut kit, 1000, |i| Some(i as i64));
    finish_and_publish(&mut w1, &mut vfs, &mut kit, &probe);
    let spec = *w1.spec();
    let mut stale = DvCompile::new();
    (0..100u16).for_each(|r| stale.mark(0, r));
    let image = crate::sidecar::sidecar_file_image(SidecarKind::Dv, [0; 16], 0, &stale.encode(0, 2).expect("encode"));
    let fd = vfs.create_rw(&format!("{DIR}/part-0-dv-2.pgrcs")).expect("create");
    vfs.pwrite_at(&fd, 0, &image).expect("write");
    vfs.close_file(fd).expect("close");
    assert_eq!(compile(&mut vfs, &log, &spec, &[(0, 500)], 150, &probe).expect("compile"), 2);
    let mut want = DvCompile::new();
    want.mark(0, 500);
    let file = vfs.read_full(&format!("{DIR}/part-0-dv-2.pgrcs")).expect("dv");
    assert_eq!(pgrc_format::sidecar::SidecarFileHeader::validate_file(&file).expect("envelope").1, &want.encode(0, 2).expect("encode")[..]);
}

#[test]
fn an_empty_compile_publishes_nothing() {
    let mut vfs = mem_with_dir();
    let log = WalLog::shared();
    let spec = crate::seal::PartSpec { spc: SPC, db: DB, relfilenumber: RELFILENUMBER, schema_fingerprint: 0 };
    let mut sink = Sink::new(DIR, log.clone());
    let probe = Probe::new(TxnVerdict::Committed);
    let out = publish_compile(&mut vfs, &mut sink, DIR, &spec, &[(0, &[][..])], 100, &probe, (None, None)).expect("compile");
    assert!(out.is_none());
    assert!(lock(&log).records.is_empty());
}

// ---------------------------------------------------------------------------
// The compact publish (K1 L3): parts 0 and 1 (txns 100 and 110) rewritten by txn 200 into part 2, which retires both
// in one `PUBLISH` of kind `Compact`. A crash at every op, before the publish, after it and after its commit,
// recovers to the old generation (the inputs, no output, no temp) or the new one (the output with its map, flags and
// row-ID ranges; the inputs gone), never a mix.
// ---------------------------------------------------------------------------

use pgrc_format::manifest::{next_row_id, range_entries, PART_FLAG_COMPACTED, PART_FLAG_ROWMAP, PART_ROWMAP_FORM_SHIFT, PART_ROWMAP_GRANULES};

/// The compaction output's row IDs (IX0): its two inputs' ranges, and its map's runs within them.
const OUT_IDS: [(u64, u64); 2] = [(291, 291 + 1164), (2_910_000, 2_910_000 + 582)];

fn out_map() -> Vec<u8> {
    pgrc_format::rowmap::encode_runs(&[(391, 900), (2_910_000, 500)], pgrc_format::rowmap::Opts::default()).expect("the map")
}

struct Compaction {
    vfs: MemVfs,
    log: Arc<Mutex<WalLog>>,
    inputs: [Vec<u8>; 2],
    completed: bool,
    ops_before: u64,
}

/// Gen 1 and gen 2 (1000 and 500 rows, committed), then txn 200 seals their 1400 survivors and publishes the
/// compaction with a crash armed `k` ops in; txn 200 commits iff its publish completed and `commit`.
fn compaction(crash_at: Option<u64>, ckpt: bool, commit: bool) -> Compaction {
    let mut vfs = mem_with_dir();
    vfs.fsync_dir(DIR).expect("dir");
    let log = WalLog::shared();
    let mut kit = Kit::new();
    let mut probe = Probe::new(TxnVerdict::InProgress);
    for (fxid, rows) in [(100, 1000), (110, 500)] {
        probe = probe.set(fxid, TxnVerdict::Committed);
        let mut w = logged_writer(vec![int8_col(1)], stamp(fxid, 1), &log);
        append_int8_rows(&mut w, &mut vfs, &mut kit, rows, |i| Some(i as i64));
        finish_and_publish(&mut w, &mut vfs, &mut kit, &probe);
        lock(&log).commit(fxid);
    }
    if ckpt {
        lock(&log).checkpoint(&mut vfs).expect("checkpoint");
    }
    let inputs = [0, 1].map(|p| vfs.read_full(&format!("{DIR}/part-{p}.pgrc")).expect("input"));
    let mut w = logged_writer(vec![int8_col(1)], stamp(200, 1), &log);
    append_int8_rows(&mut w, &mut vfs, &mut kit, 1400, |i| Some(i as i64 * 7));
    let map = out_map();
    let ops_before = vfs.op_count();
    if let Some(k) = crash_at {
        vfs.crash_at_op(k);
    }
    let completed = (|| -> WriteResult<()> {
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv { vfs: &mut vfs, sources: &sources, resolver: &kit.resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
        w.finish(&mut env)?;
        let (out, no) = w.publish_compact(&mut vfs, &probe, (None, None), &[(0, 0), (1, 0)], (PART_ROWMAP_GRANULES, &map), &OUT_IDS)?;
        assert_eq!((out.gen, no), (3, Some(2)));
        Ok(())
    })()
    .is_ok();
    if completed && commit {
        lock(&log).commit(200);
    }
    Compaction { vfs, log, inputs, completed, ops_before }
}

fn check_compacted(c: &mut Compaction, want: u64, output: &[u8], what: &str) {
    let probe = committed_probe(&c.log);
    let eff = effective_manifest(&mut c.vfs, DIR, &probe, None).expect("walk").expect("effective");
    assert_eq!(eff.header.gen, want, "{what}");
    let names = c.vfs.list_dir(DIR).expect("list");
    assert!(names.iter().all(|n| !n.starts_with("tmp-")), "a temp survived ({what}): {names:?}");
    if want == 2 {
        assert_eq!(eff.parts.iter().map(|p| (p.part_no, p.flags)).collect::<Vec<_>>(), vec![(0, 0), (1, 0)], "{what}");
        for p in 0..2 {
            assert!(c.vfs.read_full(&format!("{DIR}/part-{p}.pgrc")).expect("input") == c.inputs[p], "input {p} intact ({what})");
        }
        assert!(!names.iter().any(|n| n.starts_with("part-2")), "no output residue ({what}): {names:?}");
    } else {
        let flags = PART_FLAG_ROWMAP | PART_ROWMAP_GRANULES << PART_ROWMAP_FORM_SHIFT | PART_FLAG_COMPACTED;
        assert_eq!(eff.parts.iter().map(|p| (p.part_no, p.flags, p.rows)).collect::<Vec<_>>(), vec![(2, flags, 1400)], "{what}");
        assert_eq!(range_entries(&eff).expect("ranges"), vec![(2, OUT_IDS.to_vec())], "{what}");
        assert_eq!(next_row_id(&eff).expect("floor"), OUT_IDS[1].1, "{what}");
        assert!(c.vfs.read_full(&format!("{DIR}/part-2.pgrc")).expect("output") == output, "output bytes ({what})");
        assert!(names.iter().any(|n| n == "part-2-rowmap-1.pgrcs"), "the output's map ({what}): {names:?}");
        assert!(!names.iter().any(|n| n.starts_with("part-0") || n.starts_with("part-1")), "inputs cleaned ({what}): {names:?}");
    }
}

#[test]
fn every_crash_point_of_a_compaction_recovers_to_the_inputs_or_the_output() {
    let mut rehearsal = compaction(None, false, true);
    assert!(rehearsal.completed);
    let window = rehearsal.vfs.op_count() - rehearsal.ops_before;
    assert!(window > 20, "window too small to be a real sweep: {window}");
    let output = rehearsal.vfs.read_full(&format!("{DIR}/part-2.pgrc")).expect("output");
    let kinds: Vec<_> = lock(&rehearsal.log).records.iter().filter_map(|r| match r {
        crate::walmodel::WalRecord::Publish { rec, .. } => Some((rec.kind, rec.retire.clone(), rec.installs.len())),
        _ => None,
    }).collect();
    assert_eq!(kinds.last(), Some(&(pgrc_format::wal::PublishKind::Compact, vec![0, 1], 1)), "one PUBLISH: retire 2, install 1");
    for k in 1..=window + 1 {
        for ckpt in [false, true] {
            for all in [false, true] {
                let what = format!("crash at op {k}, checkpoint {ckpt}, all WAL {all}");
                let mut c = compaction(Some(k), ckpt, true);
                assert_eq!(c.completed, k > window, "{what}");
                let want = if c.completed { 3 } else { 2 };
                crash_and_recover(&mut c.vfs, &c.log, durable_upto(&c.log, all));
                check_compacted(&mut c, want, &output, &what);
            }
        }
    }
}

/// Published, not committed: the inputs stay effective and the output is reclaimed.
#[test]
fn a_compaction_published_but_not_committed_keeps_its_inputs() {
    let output = compaction(None, false, true).vfs.read_full(&format!("{DIR}/part-2.pgrc")).expect("output");
    for ckpt in [false, true] {
        let mut c = compaction(None, ckpt, false);
        assert!(c.completed);
        let rep = crash_and_recover(&mut c.vfs, &c.log, durable_upto(&c.log, true));
        assert!(rep.removed.iter().any(|n| n == "part-2.pgrc"), "{:?}", rep.removed);
        check_compacted(&mut c, 2, &output, "uncommitted");
    }
}

/// A compaction whose inputs moved (a compile changed a DV, or an input is gone) refuses before it logs anything.
#[test]
fn a_compaction_over_moved_inputs_refuses() {
    let mut c = compaction(None, false, true);
    let mut sink = Sink::new(DIR, c.log.clone());
    let probe = committed_probe(&c.log).set(300, TxnVerdict::InProgress);
    let spec = crate::seal::PartSpec { spc: SPC, db: DB, relfilenumber: RELFILENUMBER, schema_fingerprint: effective_manifest(&mut c.vfs, DIR, &probe, None).unwrap().unwrap().header.schema_fingerprint };
    let publishes = |c: &Compaction| lock(&c.log).records.iter().filter(|r| matches!(r, crate::walmodel::WalRecord::Publish { .. })).count();
    let before = publishes(&c);
    for retire in [&[(2, 5)][..], &[(0, 0)], &[], &[(2, 0)]] {
        let err = crate::publish::publish_compact(&mut c.vfs, &mut sink, DIR, &spec, None, retire, 300, &probe, (None, None));
        assert!(matches!(err, Err(crate::WriteError::Contract { .. })) == (retire != [(2, 0)]), "{retire:?}");
    }
    assert_eq!(publishes(&c), before + 1, "only the pure retire of the live part publishes");
    let eff = effective_manifest(&mut c.vfs, DIR, &probe.set(300, TxnVerdict::Committed), None).unwrap().unwrap();
    assert_eq!((eff.parts.len(), range_entries(&eff).unwrap(), next_row_id(&eff).unwrap()), (0, vec![], OUT_IDS[1].1), "a pure retire drops the entry; the floor stays");
}


// ---------------------------------------------------------------------------
// Stats runs (P-1, gate 2): a crash at every op of a publish that FOLDS the loose parts into a small run, and of one
// that MERGES sixteen small runs into a big run (each with its root), recovers to the old or the new generation, the
// recovered stats cover the effective part set and equal the walk over its parts, no staged or dead-generation run
// survives, recovery twice gives identical bytes, and a retry publishes.
// ---------------------------------------------------------------------------

use crate::stats::{StatsState, LOOSE_MAX, SMALL_RUNS_MAX};
use crate::writer::PartCutPolicy;

/// One part per row.
fn per_row() -> PartCutPolicy {
    PartCutPolicy { max_rows: 1, cut_granule_rows: 1, ..PartCutPolicy::default() }
}

/// Txn `fxid` publishes `parts` one-row parts (values from `start`) and commits.
fn publish_rows(vfs: &mut MemVfs, log: &Arc<Mutex<WalLog>>, fxid: u64, parts: u64, start: i64) -> crate::publish::PublishOutcome {
    let mut kit = Kit::new();
    let probe = committed_probe(log).set(fxid, TxnVerdict::InProgress);
    let mut w = open_writer_policy(vec![int8_col(1)], stamp(fxid, 1), per_row()).with_sink(Box::new(RecordingSink::new(DIR, log.clone())));
    append_int8_rows(&mut w, vfs, &mut kit, parts, |i| Some(start + i as i64));
    let out = finish_and_publish(&mut w, vfs, &mut kit, &probe);
    lock(log).commit(fxid);
    out
}

/// The walk over `m`'s parts (the fold in manifest order) and the effective generation's stats as loaded from the
/// directory: equal as served, zones included.
fn check_stats(vfs: &mut MemVfs, m: &Manifest, what: &str) -> StatsState {
    check_stats_at(vfs, m, what, true)
}

fn check_stats_at(vfs: &mut MemVfs, m: &Manifest, what: &str, effective: bool) -> StatsState {
    let state = StatsState::load(vfs, DIR, m, None, &mut |v, rec| crate::publish::part_stats_on_disk(v, DIR, m, rec)).unwrap_or_else(|e| panic!("stats of gen {} ({what}): {e}", m.header.gen));
    let mut walk = crate::facts::FactsAcc::new();
    for rec in &m.parts {
        let bytes = vfs.read_full(&format!("{DIR}/{}", pgrc_format::dirlayout::part_file_name(rec.part_no))).expect("part");
        let part = pgrc_read::OpenPart::open(Box::new(pgrc_read::MemPartIo::new(bytes, 7, u64::from(rec.part_no) + 1)), &pgrc_read::PartExpect::none()).expect("open");
        walk.fold_open_parts([(&part, rec.file_len)]).expect("walk");
    }
    let h = m.header;
    let walk = walk.into_image(h.gen, h.publisher_fxid, h.schema_fingerprint).unwrap();
    let all = |_: &pgrc_format::facts::ColFacts| true;
    let img = state.image(m).expect("image");
    assert_eq!(crate::facts::serve(&img, &all).unwrap(), crate::facts::serve(&walk, &all).unwrap(), "stats == walk ({what})");
    for n in vfs.list_dir(DIR).expect("list").into_iter().filter(|_| effective) {
        let dead = pgrc_format::dirlayout::parse_run_file_name(&n).is_some_and(|(g, _)| g > h.gen);
        assert!(!n.ends_with(".pgrcr.tmp") && !dead, "residue {n} ({what})");
    }
    state
}

/// `setup` builds the committed base (returning the next fxid); txn `fxid` then publishes `parts` parts with a crash
/// armed `k` ops in, and commits iff it completed. With `full`, the publish takes its base as the catalog would
/// (`BaseGen`, loaded before the crash window) with that full test for `hold`; else it loads its base itself.
fn runs_scenario(setup: &dyn Fn(&mut MemVfs, &Arc<Mutex<WalLog>>), parts: u64, crash_at: Option<u64>, ckpt: bool, full: Option<&crate::stats::FullTest>) -> (MemVfs, Arc<Mutex<WalLog>>, bool, u64) {
    let mut vfs = mem_with_dir();
    vfs.fsync_dir(DIR).expect("dir");
    let log = WalLog::shared();
    setup(&mut vfs, &log);
    if ckpt {
        lock(&log).checkpoint(&mut vfs).expect("checkpoint");
    }
    let mut kit = Kit::new();
    let probe = committed_probe(&log).set(900, TxnVerdict::InProgress);
    let mut w = open_writer_policy(vec![int8_col(1)], stamp(900, 1), per_row()).with_sink(Box::new(RecordingSink::new(DIR, log.clone())));
    append_int8_rows(&mut w, &mut vfs, &mut kit, parts, |i| Some(90_000 + i as i64));
    let base = full.map(|_| {
        let m = effective_manifest(&mut vfs, DIR, &probe, Some(900)).expect("walk").expect("a base");
        let stats = StatsState::load(&mut vfs, DIR, &m, None, &mut |v, rec| crate::publish::part_stats_on_disk(v, DIR, &m, rec)).expect("base stats");
        (m, stats)
    });
    let before = vfs.op_count();
    if let Some(k) = crash_at {
        vfs.crash_at_op(k);
    }
    let completed = (|| -> WriteResult<()> {
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv { vfs: &mut vfs, sources: &sources, resolver: &kit.resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
        w.finish(&mut env)?;
        match &base {
            Some((m, stats)) => {
                let g = crate::publish::BaseGen { manifest: m, pointer: crate::publish::commit_pointer(m), stats, full };
                w.publish_with_gen(&mut vfs, &probe, Some(g))?;
            }
            None => {
                w.publish(&mut vfs, &probe)?;
            }
        }
        Ok(())
    })()
    .is_ok();
    if completed {
        lock(&log).commit(900);
    }
    (vfs, log, completed, before)
}

/// The sweep: every op of the last publish; `combos` picks which (checkpoint, all WAL) pairs run at op `k`.
fn sweep_runs(setup: &dyn Fn(&mut MemVfs, &Arc<Mutex<WalLog>>), parts: u64, base_gen: u64, combos: &dyn Fn(u64) -> Vec<(bool, bool)>, full: Option<&crate::stats::FullTest>, expect: &dyn Fn(&StatsState, &StatsState)) {
    let (mut rehearsal, rlog, completed, before) = runs_scenario(setup, parts, None, false, full);
    assert!(completed);
    let window = rehearsal.op_count() - before;
    let eff = effective_manifest(&mut rehearsal, DIR, &committed_probe(&rlog), None).expect("walk").expect("eff");
    let base_m = crate::plan::read_manifest(&mut rehearsal, DIR, base_gen).expect("base manifest");
    let (old, new) = (check_stats_at(&mut rehearsal, &base_m, "rehearsal base", false), check_stats(&mut rehearsal, &eff, "rehearsal"));
    expect(&old, &new);
    // the PUBLISH record carries the new root (EXT_STATS), and the run bytes rode FILE_WRITE records
    let g = lock(&rlog);
    let rec = g.records.iter().rev().find_map(|r| match r { crate::walmodel::WalRecord::Publish { rec, .. } => Some(rec.clone()), _ => None }).expect("publish");
    let root = new.root.expect("a root");
    assert_eq!(pgrc_format::manifest::stats_ref(&eff).unwrap(), Some(root));
    assert!(pgrc_format::manifest::ext_records(&rec.ext).unwrap().iter().any(|(t, _)| *t == pgrc_format::manifest::EXT_STATS));
    assert!(g.records.iter().any(|r| matches!(r, crate::walmodel::WalRecord::File { name, .. } if name.ends_with(".pgrcr.tmp"))));
    drop(g);
    for k in 1..=window + 1 {
        for (ckpt, all) in combos(k) {
            let what = format!("crash at op {k} of {window}, checkpoint {ckpt}, all WAL {all}");
            let (mut vfs, log, completed, _) = runs_scenario(setup, parts, Some(k), ckpt, full);
            assert_eq!(completed, k > window, "{what}");
            crash_and_recover(&mut vfs, &log, durable_upto(&log, all));
            let probe = committed_probe(&log);
            let m = effective_manifest(&mut vfs, DIR, &probe, None).expect("walk").expect("eff");
            assert_eq!(m.header.gen, if completed { base_gen + 1 } else { base_gen }, "{what}");
            check_stats(&mut vfs, &m, &what);
            let settled = image(&mut vfs);
            let n = lock(&log).records.len();
            crash_and_recover(&mut vfs, &log, n);
            assert_eq!(image(&mut vfs), settled, "a second recovery changes nothing ({what})");
            let out = publish_rows(&mut vfs, &log, 950, 1, 7);
            assert_eq!(out.gen, m.header.gen + 1, "retry ({what})");
            check_stats(&mut vfs, &out.manifest, &format!("retry {what}"));
        }
    }
}

#[test]
fn every_crash_point_of_a_fold_publish_recovers_with_its_runs() {
    let setup = |vfs: &mut MemVfs, log: &Arc<Mutex<WalLog>>| {
        for i in 0..(LOOSE_MAX - 1) as u64 {
            publish_rows(vfs, log, 100 + i, 1, i as i64 * 10);
        }
    };
    let all = |_: u64| vec![(false, false), (false, true), (true, false), (true, true)];
    sweep_runs(&setup, 1, (LOOSE_MAX - 1) as u64, &all, None, &|old, new| {
        assert_eq!((old.runs.len(), old.loose.len()), (0, LOOSE_MAX - 1));
        assert_eq!((new.runs.len(), new.loose.len()), (1, 0), "the fold wrote one small run");
    });
}

#[test]
fn every_crash_point_of_a_merge_publish_recovers_with_its_runs() {
    let setup = |vfs: &mut MemVfs, log: &Arc<Mutex<WalLog>>| {
        for i in 0..(SMALL_RUNS_MAX - 1) as u64 {
            publish_rows(vfs, log, 100 + i, LOOSE_MAX as u64, i as i64 * 1000);
        }
    };
    let one = |k: u64| vec![[(false, false), (false, true), (true, false), (true, true)][(k % 4) as usize]];
    // P-1 L2: `hold` keeps runs of parts below the bulk cut out of merges; these one-row parts are full under a
    // one-row cut, and the publish takes its base as the catalog would
    let cols = vec![crate::facts::ColSize { attno: 1, class: pgrc_format::class::CLASS_BYVAL, fixed_len: 0, stats: None }];
    let full = crate::stats::FullTest::with_cut(cols, 1, u64::MAX);
    sweep_runs(&setup, LOOSE_MAX as u64, (SMALL_RUNS_MAX - 1) as u64, &one, Some(&full), &|old, new| {
        assert_eq!(old.runs.len(), SMALL_RUNS_MAX - 1);
        assert_eq!(new.runs.len(), 1, "the merge wrote one big run");
        assert_eq!(new.runs[0].parts.len(), LOOSE_MAX * SMALL_RUNS_MAX);
    });
}

/// The born-red tooth in the battery: a directory whose root lists a run that is gone (a plan that dropped one) is
/// refused by the load every reader and publisher uses, and a publish over it walks the parts instead of trusting it.
#[test]
fn a_root_naming_a_missing_run_is_refused_and_the_next_publish_rebuilds() {
    let mut vfs = mem_with_dir();
    let log = WalLog::shared();
    for i in 0..3u64 {
        publish_rows(&mut vfs, &log, 100 + i, LOOSE_MAX as u64, i as i64 * 1000);
    }
    let m = effective_manifest(&mut vfs, DIR, &committed_probe(&log), None).unwrap().unwrap();
    let state = check_stats(&mut vfs, &m, "three small runs");
    let victim = state.runs.iter().find(|r| Some(r.rref) != state.root).expect("a listed run").rref;
    vfs.unlink_path(&format!("{DIR}/{}", pgrc_format::dirlayout::run_file_name(victim.gen, victim.seq))).unwrap();
    assert!(StatsState::load(&mut vfs, DIR, &m, None, &mut |v, rec| crate::publish::part_stats_on_disk(v, DIR, &m, rec)).is_err());
    let out = publish_rows(&mut vfs, &log, 200, 1, 5);
    check_stats(&mut vfs, &out.manifest, "rebuilt after a refused root");
}
