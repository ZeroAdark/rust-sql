//! The checkpoint battery (delta v2 design §7.3; W0 gate 1): one checkpoint begins at every event of a
//! transaction's seal, publish and commit, and completes after each of several delays; then a crash keeps only
//! what was fsynced and the flushed WAL, and recovery replays from the last completed checkpoint. The committed
//! generation must survive every schedule. Born-RED: without `DELAY_CHKPT_START`, without sync registration, or
//! without `file_done`'s redo-pointer check, some schedule loses it.

use pgrc_qa::adapters::Probe;
use pgrc_qa::corpus::{append_fixture_rows, finish, int8_fixture, open_writer, verify_manifest, Fixture};
use pgrc_qa::walmodel::{
    recover, seed_skip_delay_chkpt, seed_skip_pending_fsync, seed_skip_redo_check, seed_skip_ref_fsync,
    seed_skip_spool_hold, seed_skip_sync_registration, ScheduledCheckpoint, TickCounter,
};
use pgrc_write::par::SharedMemVfs;
use pgrc_write::publish::{effective_manifest, TxnVerdict};
use pgrc_write::walmodel::{Clock, RecordingSink, SharedLog, SpoolModel, Teeth, WalLog, WalRecord};
use pgrc_write::writer::PartCutPolicy;
use pgrc_write::wvfs::{MemVfs, WriteVfs};
use pgsync::lock;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const N: u64 = 600;
const FXID1: u64 = 101;
const FXID2: u64 = 202;
const DELAYS: [u64; 4] = [0, 1, 3, 9];

fn fixture() -> Fixture {
    let policy = PartCutPolicy { max_rows: 256, max_bytes: u64::MAX, cut_granule_rows: 128 };
    int8_fixture("wal_checkpoint", 211, 2 * N, Vec::new(), policy, |i| if i % 7 == 3 { None } else { Some(i as i64 * 3 - 7) })
}

fn generation(vfs: &mut SharedMemVfs, log: &SharedLog, fx: &Fixture, fxid: u64, lo: u64, probe: &Probe) -> Result<(), String> {
    let mut w = open_writer(fx, fxid).map_err(|e| e.to_string())?.with_sink(Box::new(RecordingSink::new(&fx.dir, log.clone())));
    append_fixture_rows(vfs, &mut w, fx, lo, lo + N).map_err(|e| format!("rows: {e}"))?;
    finish(vfs, &mut w, fx).map_err(|e| format!("finish: {e}"))?;
    w.publish(vfs, probe).map_err(|e| format!("publish: {e}"))?;
    lock(log).commit(fxid);
    Ok(())
}

/// Gen1 committed and checkpointed; then, under `teeth` and with `clock` ticking, gen2 seals, publishes and
/// commits; then a crash and recovery. Gen2 must be effective and decode to the oracle.
fn run(fx: &Fixture, teeth: Teeth, clock: impl FnOnce(&SharedMemVfs) -> Box<dyn Clock>) -> Result<(), String> {
    run_in(fx, None, teeth, clock)
}

/// The model spool of the by-reference runs (WR), with a WAL consumer that never advanced (slot 0).
const SPOOL: &str = "/pgdata/pg_pgrc_spool";
/// Parts go by reference; the smaller files (sections, sidecars) stay bytes.
const REF_MIN: u64 = 1024;

/// [`run`], optionally with parts by reference. Then, before recovery, every reference in the flushed WAL must still
/// have its bytes in the spool, for a consumer at the slot.
fn run_in(fx: &Fixture, spool: Option<SpoolModel>, teeth: Teeth, clock: impl FnOnce(&SharedMemVfs) -> Box<dyn Clock>) -> Result<(), String> {
    let mut mem = MemVfs::new();
    mem.mkdir_path(&fx.dir).map_err(|e| e.to_string())?;
    if let Some(sp) = &spool {
        mem.mkdir_path(&sp.dir).map_err(|e| e.to_string())?;
        mem.fsync_dir(&sp.dir).map_err(|e| e.to_string())?;
        mem.fsync_dir(&fx.dir).map_err(|e| e.to_string())?;
    }
    let shared = SharedMemVfs::new(mem);
    let mut vfs = shared.clone();
    let log = WalLog::shared();
    lock(&log).spool = spool.clone();
    generation(&mut vfs, &log, fx, FXID1, 0, &Probe::new(TxnVerdict::Aborted).set(FXID1, TxnVerdict::InProgress))?;
    lock(&log).checkpoint(&mut vfs).map_err(|e| format!("checkpoint after gen1: {e}"))?;
    {
        let mut g = lock(&log);
        g.teeth = teeth;
        g.set_clock(clock(&shared));
    }
    let probe = Probe::new(TxnVerdict::Aborted).set(FXID1, TxnVerdict::Committed).set(FXID2, TxnVerdict::InProgress);
    generation(&mut vfs, &log, fx, FXID2, N, &probe)?;

    shared.with(pgrc_write::testkit::crash_and_revive);
    let upto = lock(&log).flushed;
    if let Some(sp) = &spool {
        let g = lock(&log);
        let refs = g.records[..upto].iter().filter(|r| matches!(r, WalRecord::FileRef { .. })).count();
        let bytes = g.records[..upto].iter().filter(|r| matches!(r, WalRecord::File { .. })).count();
        if refs < 2 || bytes == 0 {
            return Err(format!("the by-reference run logged {refs} references and {bytes} byte records"));
        }
        g.check_spool(&mut vfs, sp.slot.unwrap_or(g.replay_from), upto)?;
    }
    recover(&mut vfs, &log, &fx.dir, upto, &[]).map_err(|e| format!("recover: {e}"))?;
    let committed = Probe::new(TxnVerdict::Aborted).set(FXID1, TxnVerdict::Committed).set(FXID2, TxnVerdict::Committed);
    let eff = effective_manifest(&mut vfs, &fx.dir, &committed, None)
        .map_err(|e| format!("effective: {e}"))?
        .ok_or("the table is gone")?;
    if eff.header.gen != 2 {
        return Err(format!("committed gen2 lost: effective gen{}", eff.header.gen));
    }
    let mut files = BTreeMap::new();
    for n in vfs.list_dir(&fx.dir).map_err(|e| e.to_string())? {
        let b = vfs.read_full(&format!("{}/{n}", fx.dir)).map_err(|e| e.to_string())?;
        files.insert(n, b);
    }
    let rows = verify_manifest(&files, &eff, fx).map_err(|e| format!("decode-vs-oracle: {e:?}"))?;
    if rows != 2 * N {
        return Err(format!("{rows} rows survived of {}", 2 * N));
    }
    Ok(())
}

/// Every (begin tick, delay) over gen2's window; the first schedule that loses gen2 is the error.
fn sweep(teeth: Teeth) -> Result<u64, String> {
    sweep_in(None, teeth)
}

/// [`sweep`] with parts by reference (WR gate A): a checkpoint begun at every event of a by-reference publish
/// (write, fsync, pending link, spool fsync, insert, rename, publish).
fn sweep_ref(teeth: Teeth) -> Result<u64, String> {
    sweep_in(Some(SpoolModel { dir: SPOOL.to_string(), ref_min: REF_MIN, slot: Some(0) }), teeth)
}

fn sweep_in(spool: Option<SpoolModel>, teeth: Teeth) -> Result<u64, String> {
    let fx = fixture();
    let ticks = Arc::new(AtomicU64::new(0));
    let t = ticks.clone();
    run_in(&fx, spool.clone(), Teeth::default(), move |_| Box::new(TickCounter(t))).expect("the unscheduled run");
    let window = ticks.load(Ordering::Relaxed);
    assert!(window > 40, "gen2's window has only {window} ticks — the sweep denominator is broken");
    let mut schedules = 0;
    for begin in 0..window {
        for delay in DELAYS {
            run_in(&fx, spool.clone(), teeth, |v| Box::new(ScheduledCheckpoint::new(v.clone(), begin, delay)))
                .map_err(|e| format!("checkpoint begun at tick {begin}, completed after {delay}: {e}"))?;
            schedules += 1;
        }
    }
    Ok(schedules)
}

#[test]
fn a_checkpoint_anywhere_in_a_transaction_keeps_its_committed_generation() {
    let n = sweep(Teeth::default()).unwrap_or_else(|e| panic!("{e}"));
    println!("wal checkpoint sweep: {n} schedules, the committed generation survived every one");
}

#[test]
fn born_red_skipping_delay_chkpt_is_caught() {
    let e = sweep(seed_skip_delay_chkpt()).expect_err("born-RED failure: a checkpoint inside a record's apply window lost nothing");
    println!("caught: {e}");
}

#[test]
fn born_red_skipping_sync_registration_is_caught() {
    let e = sweep(seed_skip_sync_registration()).expect_err("born-RED failure: a checkpoint that syncs nothing lost nothing");
    println!("caught: {e}");
}

#[test]
fn born_red_skipping_the_redo_check_is_caught() {
    let e = sweep(seed_skip_redo_check()).expect_err("born-RED failure: a file straddling a checkpoint lost nothing");
    println!("caught: {e}");
}

#[test]
fn by_reference_a_checkpoint_anywhere_keeps_the_generation_and_the_spool() {
    let n = sweep_ref(Teeth::default()).unwrap_or_else(|e| panic!("{e}"));
    println!("wal checkpoint sweep, parts by reference: {n} schedules, the generation and the spool survived every one");
}

#[test]
fn by_reference_the_w0_teeth_still_bite() {
    for teeth in [seed_skip_delay_chkpt(), seed_skip_sync_registration()] {
        let e = sweep_ref(teeth).expect_err("born-RED failure: a W0 tooth lost nothing with parts by reference");
        println!("caught {teeth:?}: {e}");
    }
}

#[test]
fn born_red_a_reference_before_its_fsync_is_caught() {
    let e = sweep_ref(seed_skip_ref_fsync()).expect_err("born-RED failure: a reference logged before its fsync lost nothing");
    println!("caught: {e}");
}

#[test]
fn born_red_a_pending_link_that_is_not_durable_is_caught() {
    let e = sweep_ref(seed_skip_pending_fsync()).expect_err("born-RED failure: a pending link never fsynced lost nothing");
    println!("caught: {e}");
}

#[test]
fn born_red_a_spool_entry_dropped_under_a_slot_is_caught() {
    let e = sweep_ref(seed_skip_spool_hold()).expect_err("born-RED failure: a checkpoint dropping a held spool entry lost nothing");
    println!("caught: {e}");
}
