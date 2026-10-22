//! The #254 clog-fence regression (§5 M3-K: "the #254 clog-fence regression
//! (xid recycling cannot resurrect an aborted publish)"): a logged but
//! UNCOMMITTED publish that recovery replays is structurally invisible on
//! BOTH manifest walks (writer `effective_manifest`, reader
//! `resolve_effective`) until cleanup reclaims it; a recycled low-32 xid
//! cannot resurrect it (fxids are epoch-qualified u64); residue is reclaimed
//! BY NAME; and the fence — not luck — is what discriminates (the
//! lying-probe tooth flips the verdict).

use pgrc_qa::adapters::{memdir_of, Probe};
use pgrc_qa::corpus::{
    append_fixture_rows, finish, int8_fixture, open_writer, verify_manifest, Fixture,
};
use pgrc_qa::simvfs::SimVfs;
use pgrc_qa::walmodel::replay;
use pgrc_qa::XorShift;
use pgrc_format::dirlayout::{manifest_file_name, part_file_name};
use pgrc_format::manifest::Manifest;
use pgrc_read::manifest_walk::{resolve_effective, TableExpect};
use pgrc_write::cleanup::cleanup_dir;
use pgrc_write::publish::{effective_manifest, TxnVerdict};
use pgrc_write::walmodel::{RecordingSink, SharedLog, WalLog};
use pgrc_write::writer::{PartCutPolicy, TableWriter};
use pgrc_write::wvfs::WriteVfs;
use pgsync::lock;

const N: u64 = 700;

/// Low 32 bits shared between the dead and the live publisher — the
/// recycled-xid shape. The u64 fxids differ by epoch.
const XID_LOW: u64 = 77;
const FXID_DEAD: u64 = XID_LOW; // epoch 0
const FXID_LIVE: u64 = (1 << 32) | XID_LOW; // epoch 1, same low 32
const FXID_GEN1: u64 = 11;

fn fence_fixture() -> Fixture {
    int8_fixture(
        "clog_fence",
        301,
        2 * N,
        Vec::new(),
        PartCutPolicy::default(),
        |i| {
            if i % 9 == 2 {
                None
            } else {
                Some(1_000 + i as i64 * 7)
            }
        },
    )
}

fn logged(fx: &Fixture, fxid: u64, log: &SharedLog) -> TableWriter {
    open_writer(fx, fxid).expect("open").with_sink(Box::new(RecordingSink::new(&fx.dir, log.clone())))
}

/// gen1 committed; gen2 published by FXID_DEAD, never committed; then a
/// crash that keeps every record, and the replay (cleanup not yet run).
fn dead_publish_state(fx: &Fixture) -> (SimVfs, SharedLog) {
    let mut vfs = SimVfs::new();
    vfs.mkdir_path(&fx.dir).expect("mkdir");
    let log = WalLog::shared();
    let mut probe = Probe::new(TxnVerdict::Aborted);
    probe.mark(FXID_GEN1, TxnVerdict::InProgress);
    let mut w = logged(fx, FXID_GEN1, &log);
    append_fixture_rows(&mut vfs, &mut w, fx, 0, N).expect("rows1");
    finish(&mut vfs, &mut w, fx).expect("finish1");
    w.publish(&mut vfs, &probe).expect("publish1");
    lock(&log).commit(FXID_GEN1);
    probe.mark(FXID_GEN1, TxnVerdict::Committed);

    probe.mark(FXID_DEAD, TxnVerdict::InProgress);
    let mut w2 = logged(fx, FXID_DEAD, &log);
    append_fixture_rows(&mut vfs, &mut w2, fx, N, 2 * N).expect("rows2");
    finish(&mut vfs, &mut w2, fx).expect("finish2");
    w2.publish(&mut vfs, &probe).expect("publish2");
    // NO commit record. Crash; every record survives, so replay rebuilds
    // gen2 whole — the fence alone must hide it.
    let mut rng = XorShift::new(0x254);
    vfs.crash_and_revive(&mut rng);
    let upto = lock(&log).records.len();
    replay(&mut vfs, &log, upto).expect("replay");
    (vfs, log)
}

#[test]
fn durable_uncommitted_publish_is_invisible_on_both_walks() {
    let fx = fence_fixture();
    let (mut vfs, _log) = dead_publish_state(&fx);
    let probe = Probe::new(TxnVerdict::Aborted).set(FXID_GEN1, TxnVerdict::Committed);

    // Writer-side walk: gen1, with the dead gen2 walked past.
    let eff = effective_manifest(&mut vfs, &fx.dir, &probe, None)
        .expect("effective")
        .expect("gen1 present");
    assert_eq!(eff.header.gen, 1, "dead publish resurrected on writer walk");

    // Reader-side walk: same verdict, and the walked_past witness proves
    // the fence actually skipped the dead generation (CURRENT points at it).
    let files = vfs.snapshot_dir(&fx.dir);
    let rdir = memdir_of(&files);
    let reff = resolve_effective(&rdir, &probe, &TableExpect::default())
        .expect("reader walk")
        .expect("gen1 present");
    assert_eq!(reff.manifest.header.gen, 1);
    assert_eq!(
        reff.walked_past, 1,
        "reader walk did not walk past the dead generation — CURRENT no longer points at it?"
    );

    // Old data intact.
    let rows = verify_manifest(&files, &eff, &fx).expect("old decode");
    assert_eq!(rows, N);

    // The dead generation's manifest file IS present (replay rebuilt it) —
    // visibility is the fence's doing, not absence.
    assert!(
        files.contains_key(&manifest_file_name(2)),
        "test premise broken: replay did not rebuild manifest-2"
    );

    // Lying-probe tooth: a clog that claims FXID_DEAD committed WOULD
    // surface gen2 — proving the fence is the discriminator.
    let lying = Probe::new(TxnVerdict::Aborted)
        .set(FXID_GEN1, TxnVerdict::Committed)
        .set(FXID_DEAD, TxnVerdict::Committed);
    let eff_lied = effective_manifest(&mut vfs, &fx.dir, &lying, None)
        .expect("effective under lie")
        .expect("present");
    assert_eq!(
        eff_lied.header.gen, 2,
        "fence tooth broken: even a committed publisher's generation is hidden"
    );
}

// D2876 (trunk 9afaaac28b5, test ported onto the line's cleanup): a generation recovery keeps because its publisher
// is in progress (a prepared transaction's) keeps its parts. On trunk only the effective generation's parts counted
// as live, so the kept generation lost its newest part as an orphan and the next load failed ENOENT on it. The line's
// `cleanup_dir_keeping` counts every in-progress generation's parts live; this pins it.
#[test]
fn a_kept_in_progress_generation_keeps_its_parts() {
    let fx = fence_fixture();
    let (mut vfs, log) = dead_publish_state(&fx);
    let probe = Probe::new(TxnVerdict::Aborted)
        .set(FXID_GEN1, TxnVerdict::Committed)
        .set(FXID_DEAD, TxnVerdict::InProgress);
    let report = cleanup_dir(&mut vfs, &mut RecordingSink::new(&fx.dir, log.clone()), &fx.dir, &probe).expect("recover");
    let files = vfs.snapshot_dir(&fx.dir);
    let m2 = Manifest::decode(files.get(&manifest_file_name(2)).expect("manifest-2 kept")).expect("manifest-2 decodes");
    assert!(!m2.parts.is_empty());
    for p in &m2.parts {
        assert!(
            files.contains_key(&part_file_name(p.part_no)),
            "part {} of the kept generation 2 was reclaimed: removed = {:?}",
            p.part_no,
            report.removed
        );
    }
}

#[test]
fn recycled_xid_cannot_resurrect_and_name_is_reclaimed() {
    let fx = fence_fixture();
    let (mut vfs, log) = dead_publish_state(&fx);
    let probe = Probe::new(TxnVerdict::Aborted)
        .set(FXID_GEN1, TxnVerdict::Committed)
        .set(FXID_LIVE, TxnVerdict::InProgress);

    // Recovery cleans the dead residue (manifest-2 + its orphan parts).
    let report = cleanup_dir(&mut vfs, &mut RecordingSink::new(&fx.dir, log.clone()), &fx.dir, &probe)
        .expect("recover");
    assert!(
        report.removed.iter().any(|n| n == &manifest_file_name(2)),
        "dead manifest-2 not reclaimed: removed = {:?}",
        report.removed
    );

    // The epoch-recycled publisher (same low 32 bits, different fxid)
    // publishes gen2 anew and commits.
    let mut w = logged(&fx, FXID_LIVE, &log);
    append_fixture_rows(&mut vfs, &mut w, &fx, N, 2 * N).expect("rows live");
    finish(&mut vfs, &mut w, &fx).expect("finish live");
    w.publish(&mut vfs, &probe).expect("publish live");
    let committed = Probe::new(TxnVerdict::Aborted)
        .set(FXID_GEN1, TxnVerdict::Committed)
        .set(FXID_LIVE, TxnVerdict::Committed);

    // gen2 is now FXID_LIVE's — the name was reclaimed; FXID_DEAD's bytes
    // are gone, and its verdict (still Aborted) is irrelevant forever.
    let eff = effective_manifest(&mut vfs, &fx.dir, &committed, None)
        .expect("effective")
        .expect("gen2");
    assert_eq!(eff.header.gen, 2);
    assert_eq!(eff.header.publisher_fxid, FXID_LIVE);
    let files = vfs.snapshot_dir(&fx.dir);
    let m2 = Manifest::decode(files.get(&manifest_file_name(2)).expect("manifest-2"))
        .expect("manifest-2 decodes");
    assert_eq!(
        m2.header.publisher_fxid, FXID_LIVE,
        "gen2 file still carries the dead publisher — name not reclaimed"
    );
    let rows = verify_manifest(&files, &eff, &fx).expect("decode");
    assert_eq!(rows, 2 * N);
}

#[test]
fn all_uncommitted_is_an_empty_table_and_fully_reclaimed() {
    let fx = int8_fixture(
        "clog_fence_empty",
        302,
        N,
        Vec::new(),
        PartCutPolicy::default(),
        |i| Some(i as i64),
    );
    let mut vfs = SimVfs::new();
    vfs.mkdir_path(&fx.dir).expect("mkdir");
    let log = WalLog::shared();
    let probe = Probe::new(TxnVerdict::Aborted).set(9_001, TxnVerdict::InProgress);
    let mut w = logged(&fx, 9_001, &log);
    append_fixture_rows(&mut vfs, &mut w, &fx, 0, N).expect("rows");
    finish(&mut vfs, &mut w, &fx).expect("finish");
    w.publish(&mut vfs, &probe).expect("publish");
    let mut rng = XorShift::new(0xE);
    vfs.crash_and_revive(&mut rng);
    let upto = lock(&log).records.len();
    replay(&mut vfs, &log, upto).expect("replay");

    let dead = Probe::new(TxnVerdict::Aborted);
    // Both walks: empty table, not an error.
    assert!(effective_manifest(&mut vfs, &fx.dir, &dead, None)
        .expect("effective")
        .is_none());
    let files = vfs.snapshot_dir(&fx.dir);
    let rdir = memdir_of(&files);
    assert!(resolve_effective(&rdir, &dead, &TableExpect::default())
        .expect("reader walk")
        .is_none());
    // Cleanup reclaims EVERYTHING except (possibly) CURRENT.
    cleanup_dir(&mut vfs, &mut RecordingSink::new(&fx.dir, log.clone()), &fx.dir, &dead).expect("recover");
    let after = vfs.list_dir(&fx.dir).expect("listdir");
    for n in &after {
        assert_eq!(
            n,
            pgrc_format::dirlayout::CURRENT_FILE_NAME,
            "residue survived full reclaim: {n}"
        );
    }
}
