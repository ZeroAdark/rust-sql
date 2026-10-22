//! Writer lifecycle (M3-D slice legs 8–10): header-first abort, the
//! freeze-only-if-created-in-subxact parity table, per-(xid,cid) eviction +
//! eoxact purge, and the deterministic part-cut policy.

use super::*;
use crate::cleanup::cleanup_dir;
use crate::publish::TxnVerdict;
use crate::walmodel::{WalLog, WalRecord};
use crate::walsink::NoWal;
use crate::writer::{
    freeze_decision, FreezeDecision, PartCutPolicy, SubxactEvidence, WriterRegistry,
};

/// Seal (no publish), then abort: the tmp file is pgrc-identifiable
/// (header magic first) but never readable (tmp namespace, no manifest),
/// and cleanup removes it.
#[test]
fn header_first_abort_leaves_no_readable_part() {
    let mut vfs = mem_with_dir();
    let mut kit = Kit::new();
    let mut w = open_writer(vec![int8_col(1)], stamp(50, 1));
    append_int8_rows(&mut w, &mut vfs, &mut kit, 100, |i| Some(i as i64));
    {
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv {
            vfs: &mut vfs,
            sources: &sources,
            resolver: &kit.resolver,
            shred: &mut kit.shred,
            shred_opts: &kit.opts,
        };
        w.finish(&mut env).expect("finish");
    }
    // Sealed but unpublished: exactly one tmp file, header magic first.
    let names = vfs.list_dir(DIR).expect("list");
    assert_eq!(names, vec!["tmp-50-0.pgrct".to_string()]);
    let bytes = vfs.read_full(&format!("{DIR}/tmp-50-0.pgrct")).expect("tmp");
    assert_eq!(
        u64::from_le_bytes(bytes[..8].try_into().unwrap()),
        pgrc_format::part::PART_MAGIC,
        "header-first: the file is pgrc-identifiable from byte 0"
    );
    // Not readable: no manifest exists at all.
    let probe = Probe::new(TxnVerdict::Aborted);
    assert!(
        crate::publish::effective_manifest(&mut vfs, DIR, &probe, None)
            .expect("eff")
            .is_none()
    );
    // Abort unlinks.
    w.abort(&mut vfs).expect("abort");
    assert!(vfs.list_dir(DIR).expect("list").is_empty());
}

/// Crash instead of abort: recovery's temp scan removes the dead file.
#[test]
fn crashed_writer_temp_removed_by_recovery_scan() {
    let mut vfs = mem_with_dir();
    let mut kit = Kit::new();
    let mut w = open_writer(vec![int8_col(1)], stamp(60, 1));
    append_int8_rows(&mut w, &mut vfs, &mut kit, 10, |i| Some(i as i64));
    {
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv {
            vfs: &mut vfs,
            sources: &sources,
            resolver: &kit.resolver,
            shred: &mut kit.shred,
            shred_opts: &kit.opts,
        };
        w.finish(&mut env).expect("finish");
    }
    drop(w); // kill -9 shaped: no abort ran
    crate::testkit::crash_and_revive(&mut vfs);
    // The tmp never got fsync'd — after a crash it may or may not survive;
    // MemVfs's dirent model drops it (never fsync'd dirent). Either way the
    // recovery scan leaves no temp behind.
    let probe = Probe::new(TxnVerdict::Aborted);
    cleanup_dir(&mut vfs, &mut NoWal, DIR, &probe).expect("clean");
    for n in vfs.list_dir(DIR).expect("list") {
        assert!(
            !pgrc_format::dirlayout::is_temp_file_name(&n),
            "dead temp survived: {n}"
        );
    }
    // An in-progress writer's temp is NOT reaped.
    let mut kit2 = Kit::new();
    let mut w2 = open_writer(vec![int8_col(1)], stamp(61, 1));
    append_int8_rows(&mut w2, &mut vfs, &mut kit2, 10, |i| Some(i as i64));
    {
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit2.cands];
        let mut env = SealEnv {
            vfs: &mut vfs,
            sources: &sources,
            resolver: &kit2.resolver,
            shred: &mut kit2.shred,
            shred_opts: &kit2.opts,
        };
        w2.finish(&mut env).expect("finish");
    }
    let probe_live = Probe::new(TxnVerdict::InProgress);
    cleanup_dir(&mut vfs, &mut NoWal, DIR, &probe_live).expect("clean");
    assert!(vfs
        .list_dir(DIR)
        .expect("list")
        .contains(&"tmp-61-0.pgrct".to_string()));
}

/// The freeze belt decision table (old-writer parity): frozen iff the
/// current subxact is valid AND (created-in-it OR relfilelocator-minted-in
/// -it). Silent downgrade otherwise — never an error.
#[test]
fn freeze_decision_table_pinned() {
    for valid in [false, true] {
        for created in [false, true] {
            for minted in [false, true] {
                let e = SubxactEvidence {
                    cur_subxact_valid: valid,
                    rel_created_in_cur_subxact: created,
                    new_relfilelocator_in_cur_subxact: minted,
                };
                let expect = if valid && (created || minted) {
                    FreezeDecision::Frozen
                } else {
                    FreezeDecision::Downgraded
                };
                assert_eq!(freeze_decision(&e), expect, "{e:?}");
            }
        }
    }
}

/// Per-(xid,cid) eviction: a stamp mismatch aborts the old writer (temps
/// unlinked, rows dropped) — never a publish.
#[test]
fn stale_writer_evicted_by_abort_on_cid_change() {
    let mut vfs = mem_with_dir();
    let mut kit = Kit::new();
    let mut reg = WriterRegistry::new();
    {
        let w = reg
            .get_or_open(&mut vfs, RELFILENUMBER, stamp(70, 1), || {
                Ok(open_writer(vec![int8_col(1)], stamp(70, 1)))
            })
            .expect("open");
        // Buffer rows and force a seal so a temp file exists.
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv {
            vfs: &mut vfs,
            sources: &sources,
            resolver: &kit.resolver,
            shred: &mut kit.shred,
            shred_opts: &kit.opts,
        };
        for i in 0..10 {
            w.append_row(&[RawDatum::Word(i)], &mut kit.ext, &mut env)
                .expect("append");
        }
        w.finish(&mut env).expect("finish");
    }
    assert_eq!(vfs.list_dir(DIR).expect("list").len(), 1, "one temp");
    // Same txn, NEW cid: evict + abort.
    let w2 = reg
        .get_or_open(&mut vfs, RELFILENUMBER, stamp(70, 2), || {
            Ok(open_writer(vec![int8_col(1)], stamp(70, 2)))
        })
        .expect("open2");
    assert_eq!(w2.buffered_rows(), 0, "fresh writer, old rows dropped");
    assert_eq!(w2.sealed_parts().len(), 0);
    assert!(
        vfs.list_dir(DIR).expect("list").is_empty(),
        "evicted writer's temps unlinked"
    );
    // New fxid likewise evicts.
    let _ = reg
        .get_or_open(&mut vfs, RELFILENUMBER, stamp(71, 1), || {
            Ok(open_writer(vec![int8_col(1)], stamp(71, 1)))
        })
        .expect("open3");
    assert_eq!(reg.len(), 1);
}

/// take_for_publish re-checks the stamp: a stale writer is aborted, never
/// handed out (the savepoint-rollback shape).
#[test]
fn take_for_publish_rechecks_stamp() {
    let mut vfs = mem_with_dir();
    let mut kit = Kit::new();
    let mut reg = WriterRegistry::new();
    {
        let w = reg
            .get_or_open(&mut vfs, RELFILENUMBER, stamp(80, 1), || {
                Ok(open_writer(vec![int8_col(1)], stamp(80, 1)))
            })
            .expect("open");
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv {
            vfs: &mut vfs,
            sources: &sources,
            resolver: &kit.resolver,
            shred: &mut kit.shred,
            shred_opts: &kit.opts,
        };
        for i in 0..10 {
            w.append_row(&[RawDatum::Word(i)], &mut kit.ext, &mut env)
                .expect("append");
        }
        w.finish(&mut env).expect("finish");
    }
    let taken = reg
        .take_for_publish(&mut vfs, RELFILENUMBER, stamp(80, 2))
        .expect("take");
    assert!(taken.is_none(), "stale stamp never publishes");
    assert!(vfs.list_dir(DIR).expect("list").is_empty(), "aborted temps");
    assert!(reg.is_empty());

    // Fresh writer with a MATCHING stamp is handed out.
    {
        let _ = reg
            .get_or_open(&mut vfs, RELFILENUMBER, stamp(81, 1), || {
                Ok(open_writer(vec![int8_col(1)], stamp(81, 1)))
            })
            .expect("open");
    }
    let taken = reg
        .take_for_publish(&mut vfs, RELFILENUMBER, stamp(81, 1))
        .expect("take");
    assert!(taken.is_some());
    assert!(reg.is_empty());
}

/// eoxact purge: unconditional abort of everything registered — identical
/// on commit and abort.
#[test]
fn eoxact_purges_all_registered_writers() {
    let mut vfs = mem_with_dir();
    let mut kit = Kit::new();
    let mut reg = WriterRegistry::new();
    // Two tables in one txn would live in two directories in production;
    // this harness shares DIR, so distinct fxids keep temp names disjoint.
    for (rel, fx) in [(RELFILENUMBER, 90), (RELFILENUMBER + 1, 91)] {
        let w = reg
            .get_or_open(&mut vfs, rel, stamp(fx, 1), || {
                Ok(open_writer(vec![int8_col(1)], stamp(fx, 1)))
            })
            .expect("open");
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv {
            vfs: &mut vfs,
            sources: &sources,
            resolver: &kit.resolver,
            shred: &mut kit.shred,
            shred_opts: &kit.opts,
        };
        for i in 0..5 {
            w.append_row(&[RawDatum::Word(i)], &mut kit.ext, &mut env)
                .expect("append");
        }
        w.finish(&mut env).expect("finish");
    }
    assert_eq!(reg.len(), 2);
    reg.at_eoxact(&mut vfs).expect("eoxact");
    assert!(reg.is_empty());
    assert!(
        vfs.list_dir(DIR).expect("list").is_empty(),
        "all abandoned temps unlinked"
    );
}

/// The part-cut policy: deterministic cuts at the row budget; publish
/// numbers the parts monotonically in seal order.
#[test]
fn part_cut_policy_cuts_deterministically() {
    let mut vfs = mem_with_dir();
    let mut kit = Kit::new();
    let mut w = open_writer_policy(
        vec![int8_col(1)],
        stamp(95, 1),
        PartCutPolicy {
            max_rows: 100,
            max_bytes: u64::MAX,
            cut_granule_rows: 100,
        },
    );
    append_int8_rows(&mut w, &mut vfs, &mut kit, 250, |i| Some(i as i64));
    let probe = Probe::new(TxnVerdict::InProgress).set(95, TxnVerdict::Committed);
    let out = finish_and_publish(&mut w, &mut vfs, &mut kit, &probe);
    assert_eq!(out.part_nos, vec![0, 1, 2]);
    let m = read_manifest(&mut vfs, 1);
    assert_eq!(
        m.parts.iter().map(|p| p.rows).collect::<Vec<_>>(),
        vec![100, 100, 50]
    );
}

/// The sections file lives exactly as long as its manifest: recovery keeps the
/// file of every kept generation and reclaims an orphan; the retired
/// `facts-<gen>.pgrcf` sidecar and a completion temp go too.
#[test]
fn orphan_sections_files_retired_facts_and_completion_temps_are_reclaimed_by_recovery() {
    let mut vfs = mem_with_dir();
    let mut kit = Kit::new();
    let mut w = open_writer(vec![int8_col(1)], stamp(95, 1));
    append_int8_rows(&mut w, &mut vfs, &mut kit, 10, |i| Some(i as i64));
    let probe = Probe::new(TxnVerdict::Committed);
    finish_and_publish(&mut w, &mut vfs, &mut kit, &probe);
    let plant = |vfs: &mut MemVfs, name: &str| {
        let fd = vfs.create_rw(&format!("{DIR}/{name}")).expect("create");
        vfs.pwrite_at(&fd, 0, b"x").expect("write");
        vfs.close_file(fd).expect("close");
    };
    let live = pgrc_format::dirlayout::sections_file_name(1);
    // P-1 L2: the publisher writes no sections file; a pre-P-1 directory's generation carries one
    assert!(!vfs.exists_path(&format!("{DIR}/{live}")).unwrap(), "P-1 L2: the publisher writes none");
    plant(&mut vfs, &live);
    plant(&mut vfs, &pgrc_format::dirlayout::sections_file_name(0));
    plant(&mut vfs, &pgrc_format::dirlayout::sections_file_name(9));
    plant(&mut vfs, "facts-1.pgrcf");
    plant(&mut vfs, "sections-1.pgrcx.tmp");
    cleanup_dir(&mut vfs, &mut NoWal, DIR, &probe).expect("clean");
    let names = vfs.list_dir(DIR).expect("list");
    let secs: Vec<&String> = names.iter().filter(|n| n.contains("sections-")).collect();
    assert_eq!(secs, vec![&live], "only a generation with its manifest keeps its sections file: {names:?}");
    assert!(!names.iter().any(|n| n.ends_with(".pgrcf")));
}

fn seal_rows(w: &mut TableWriter, vfs: &mut MemVfs, kit: &mut Kit, n: u64) {
    let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
    let mut env = SealEnv {
        vfs,
        sources: &sources,
        resolver: &kit.resolver,
        shred: &mut kit.shred,
        shred_opts: &kit.opts,
    };
    for i in 0..n {
        w.append_row(&[RawDatum::Word(i)], &mut kit.ext, &mut env).expect("append");
    }
    w.finish(&mut env).expect("finish");
}

/// A tmp name renamed into a part is never reused in its transaction: every writer of the table in one fxid
/// draws from one seq, so redo of an earlier publish cannot install a later statement's file.
#[test]
fn a_transactions_writers_share_the_tmp_seq() {
    let mut vfs = mem_with_dir();
    let mut kit = Kit::new();
    let mut reg = WriterRegistry::new();
    let probe = Probe::new(TxnVerdict::InProgress);
    let mut sealed = Vec::new();
    for (fx, cid) in [(70, 1), (70, 2), (70, 3)] {
        let w = reg
            .get_or_open(&mut vfs, RELFILENUMBER, stamp(fx, cid), || Ok(open_writer(vec![int8_col(1)], stamp(fx, cid))))
            .expect("open");
        seal_rows(w, &mut vfs, &mut kit, 10);
        sealed.push(w.sealed_parts()[0].tmp_name.clone());
        let mut w = reg.take_for_publish(&mut vfs, RELFILENUMBER, stamp(fx, cid)).expect("take").expect("writer");
        w.publish(&mut vfs, &probe).expect("publish");
    }
    assert_eq!(sealed, vec!["tmp-70-0.pgrct", "tmp-70-1.pgrct", "tmp-70-2.pgrct"]);
    reg.at_eoxact(&mut vfs).expect("eoxact");
    let w = reg
        .get_or_open(&mut vfs, RELFILENUMBER, stamp(71, 1), || Ok(open_writer(vec![int8_col(1)], stamp(71, 1))))
        .expect("open");
    seal_rows(w, &mut vfs, &mut kit, 10);
    assert_eq!(w.sealed_parts()[0].tmp_name, "tmp-71-0.pgrct", "a new transaction starts its own seq");
}

/// Abort logs one RECLAIM naming the sealed temps and unlinks them inside it (decision W0-4); the names are
/// registered for the next checkpoint.
#[test]
fn abort_logs_reclaim_of_its_temps() {
    let mut vfs = mem_with_dir();
    let mut kit = Kit::new();
    let log = WalLog::shared();
    let mut w = logged_writer(vec![int8_col(1)], stamp(50, 1), &log);
    seal_rows(&mut w, &mut vfs, &mut kit, 10);
    w.abort(&mut vfs).expect("abort");
    assert!(vfs.list_dir(DIR).expect("list").is_empty());
    let g = pgsync::lock(&log);
    match g.records.last() {
        Some(WalRecord::Reclaim { rec, .. }) => {
            assert_eq!(rec.names, vec!["tmp-50-0.pgrct".to_string()]);
            assert_eq!(rec.xid, 50);
        }
        other => panic!("abort must end with RECLAIM, got {other:?}"),
    }
    assert!(g.dirty[DIR].contains("tmp-50-0.pgrct"));
}

/// COPYLOSS: stamp_of names the registered writer's stamp without disturbing it, so the caller can tell a live
/// writer under another stamp from a stale one before get_or_open would abort it.
#[test]
fn stamp_of_reports_without_evicting() {
    let mut vfs = mem_with_dir();
    let mut reg = WriterRegistry::new();
    assert_eq!(reg.stamp_of(RELFILENUMBER), None);
    let _ = reg
        .get_or_open(&mut vfs, RELFILENUMBER, stamp(90, 3), || Ok(open_writer(vec![int8_col(1)], stamp(90, 3))))
        .expect("open");
    assert_eq!(reg.stamp_of(RELFILENUMBER), Some(stamp(90, 3)));
    assert_eq!(reg.stamp_of(RELFILENUMBER + 1), None);
    assert_eq!(reg.len(), 1);
    let taken = reg.take_for_publish(&mut vfs, RELFILENUMBER, stamp(90, 3)).expect("take");
    taken.expect("its own stamp publishes").abort(&mut vfs).expect("abort");
    assert_eq!(reg.stamp_of(RELFILENUMBER), None);
}

/// RECLAIMRACE C: end-of-recovery cleanup keeps the parts a promoted hot standby's catalog still holds (and their
/// sidecars); every other part no generation names still goes.
#[test]
fn cleanup_keeps_the_held_parts() {
    use pgrc_format::dirlayout::{part_file_name, sidecar_file_name};
    use pgrc_format::sidecar::SidecarKind;
    let mut vfs = mem_with_dir();
    // not DVs: with no effective generation here, a DV at gen 2 is a dead compile's and goes whatever its part
    let names = [part_file_name(3), sidecar_file_name(3, SidecarKind::PredicateCache, 2), part_file_name(4), sidecar_file_name(4, SidecarKind::PredicateCache, 2)];
    for n in &names {
        let fd = vfs.create_rw(&format!("{DIR}/{n}")).expect("create");
        vfs.pwrite_at(&fd, 0, b"x").expect("write");
        vfs.close_file(fd).expect("close");
    }
    let probe = Probe::new(TxnVerdict::Committed);
    let held: std::collections::BTreeSet<u32> = [3].into_iter().collect();
    let rep = crate::cleanup::cleanup_dir_keeping(&mut vfs, &mut NoWal, DIR, &probe, &held).expect("clean");
    let sorted = |mut v: Vec<String>| {
        v.sort();
        v
    };
    assert_eq!(vfs.list_dir(DIR).expect("list"), sorted(names[..2].to_vec()), "part 3 and its sidecar kept");
    assert_eq!(sorted(rep.removed), sorted(names[2..].to_vec()));
    cleanup_dir(&mut vfs, &mut NoWal, DIR, &probe).expect("clean");
    assert!(vfs.list_dir(DIR).expect("list").is_empty(), "nothing held: no generation names them");
}
