//! D4225: pgrcolumnar honours fsync=off as PostgreSQL does. fd.c's pg_fsync_no_writethrough returns 0 before its
//! fsync(2) when `enableFsync` is false, so with fsync=off no fsync reaches the device. One publish through the real
//! vfs (parts, manifest, CURRENT, the table directory, the stats plane) makes no fsync(2) under fsync=off, and under
//! fsync=on exactly the fsyncs the publish order asks for: the MemVfs model's fsync ops of the same publish.
#![cfg(not(any(loom, pgrust_sim)))]

use pgrc_write::elect::CandidateSource;
use pgrc_write::ingest::RawDatum;
use pgrc_write::publish::TxnVerdict;
use pgrc_write::testkit::{int8_col, mem_with_dir, stamp, Kit, Probe, DB, DIR, RELFILENUMBER, SPC};
use pgrc_write::writer::{PartCutPolicy, SealEnv, SubxactEvidence, TableWriter};
use pgrc_write::wvfs::{real_fsyncs, RealVfs, WriteVfs};

/// One writer of 1,000 int8 rows, finished and published into `dir` through `vfs`.
fn publish_once(vfs: &mut dyn WriteVfs, dir: &str) {
    let mut kit = Kit::new();
    let mut w = TableWriter::open(
        dir.to_string(),
        vec![int8_col(1)],
        SPC,
        DB,
        RELFILENUMBER,
        stamp(7, 1),
        &SubxactEvidence::default(),
        PartCutPolicy::default(),
    )
    .expect("open writer");
    let sources: [&dyn CandidateSource; 1] = [&kit.cands];
    {
        let mut env = SealEnv { vfs: &mut *vfs, sources: &sources, resolver: &kit.resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
        for i in 0..1000u64 {
            w.append_row(&[RawDatum::Word(i)], &mut kit.ext, &mut env).expect("append");
        }
        w.finish(&mut env).expect("finish");
    }
    let probe = Probe::new(TxnVerdict::InProgress).set(7, TxnVerdict::Committed);
    w.publish(vfs, &probe).expect("publish");
}

/// A fresh real table directory `<tmp>/pgrc_d4225_<pid>/<arm>/pgrc_777`.
fn real_dir(real: &mut RealVfs, arm: &str) -> String {
    let base = std::env::temp_dir().join(format!("pgrc_d4225_{}", std::process::id()));
    let mut path = String::new();
    for part in [base.to_str().expect("utf8 temp dir").to_string(), arm.to_string(), "pgrc_777".to_string()] {
        path = if path.is_empty() { part } else { format!("{path}/{part}") };
        if !real.exists_path(&path).expect("exists") {
            real.mkdir_path(&path).expect("mkdir");
        }
    }
    path
}

#[ignore = "V5SYNC r14 FOLLOWUP: trunk's publish-owned fsyncs (colload (k) family) are not the line's publish design: the line's pgrc_write publish asks for no fsync, its AM flush pass does through wvfs::pg_fsync, which honours fsync=off (PGRCSERVE M4 / post-switch)"]
#[test]
fn a_publish_under_fsync_off_makes_no_fsync() {
    // The model: the fsync ops one publish asks for (data, manifest, CURRENT twice, the directory, the stats plane).
    let mut mem = mem_with_dir();
    publish_once(&mut mem, DIR);
    let want = mem.ops().iter().filter(|o| o.starts_with("fsync ") || o.starts_with("fsyncdir ")).count() as u64;
    assert!(want > 0, "the model's publish asks for fsyncs: {:#?}", mem.ops());

    let mut real = RealVfs;
    // fsync=on: the real publish makes exactly the model's fsyncs (durability unchanged).
    init_small::globals::set_enableFsync(true);
    let on = real_dir(&mut real, "on");
    let before = real_fsyncs();
    publish_once(&mut real, &on);
    assert_eq!(real_fsyncs() - before, want, "fsync=on: the real publish's fsync(2) calls against the model's fsync ops");
    assert!(real.exists_path(&format!("{on}/CURRENT")).expect("exists"));

    // fsync=off: the same publish makes none, and still publishes.
    init_small::globals::set_enableFsync(false);
    let off = real_dir(&mut real, "off");
    let before = real_fsyncs();
    publish_once(&mut real, &off);
    let made = real_fsyncs() - before;
    init_small::globals::set_enableFsync(true);
    assert_eq!(made, 0, "fsync=off: a publish made {made} fsync(2) calls (PostgreSQL makes none)");
    assert!(real.exists_path(&format!("{off}/CURRENT")).expect("exists"));

    let base = std::env::temp_dir().join(format!("pgrc_d4225_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(base);
}
