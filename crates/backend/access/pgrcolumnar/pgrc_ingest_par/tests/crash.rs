//! Mid-COPY crash/cancel cleanup at dop>1 (M3-I slice leg 3) — the #253/#254
//! crash laws UNWEAKENED under parallel assembly. Real rtpool runtime, a
//! shared MemVfs universe, and the same exactly-old-or-new bar as the M3-D
//! matrix: an errored or killed parallel session leaves NOTHING readable —
//! committed generations survive recovery (WAL replay + cleanup)
//! byte-for-byte, residue is reclaimed by name, and a retry succeeds.

use pgrc_write::testkit::*;
use pgrc_write::par::SharedMemVfs;
use pgrc_write::wvfs::WriteVfs;
use pgrc_write::ingest::RawDatum;
use pgrc_write::writer::{PartCutPolicy, SealEnv};
use pgrc_write::WriteResult;
use pgrc_write::par::{ParIngestOpts, ParProviders};
use pgrc_write::walmodel::{RecordingSink, SharedLog, WalLog};
use pgrc_write::writer::TableWriter;
use runtime::{Runtime, RuntimeConfig, WorkerPool};
use std::sync::Arc;

const FXID: u64 = 42;

fn rt_pool(workers: usize) -> (Arc<Runtime>, WorkerPool) {
    let rt = Runtime::new(RuntimeConfig::new(workers));
    let pool = WorkerPool::spawn_std(Arc::clone(&rt)).expect("pool");
    (rt, pool)
}

fn opts_small() -> ParIngestOpts {
    ParIngestOpts {
        chunk_rows: 128,
        max_chunks_in_flight: 8,
        max_parts_in_flight: 4,
    }
}

fn policy_small() -> PartCutPolicy {
    PartCutPolicy {
        max_rows: 256,
        max_bytes: u64::MAX,
        // Must equal opts_small()'s chunk_rows (M3-I).
        cut_granule_rows: 128,
    }
}

fn tmp_residue(shared: &SharedMemVfs) -> Vec<String> {
    shared.with(|v| {
        v.list_dir(DIR)
            .expect("list")
            .into_iter()
            .filter(|n| n.starts_with("tmp-"))
            .collect()
    })
}

/// Feed rows [0, n) of the mixed corpus into a parallel session; `fail_at`
/// makes the feed return a typed error after that many rows.
fn run_session(
    w: &mut TableWriter,
    rt: &Arc<Runtime>,
    providers: ParProviders,
    n: u64,
    fail_at: Option<u64>,
) -> WriteResult<u64> {
    pgrc_ingest_par::parallel_ingest(w, rt, providers, opts_small(), |sess| {
        for i in 0..n {
            if fail_at == Some(i) {
                return Err(pgrc_write::WriteError::Refused {
                    what: "scripted mid-COPY failure",
                });
            }
            with_mixed_row(i, |row| sess.append_row(row))?;
        }
        Ok(n)
    })
}

use pgrc_write::WriteError;

/// Leg 1: a mid-COPY feed error at dop>1 cancels, drains, unlinks every
/// temp; the SAME writer retries cleanly and publishes.
#[test]
fn feed_error_cancels_unlinks_and_retry_publishes() {
    let (rt, pool) = rt_pool(4);
    let shared = shared_mem_with_dir();
    let mut w = open_writer_policy(vec![int8_col(1), text_col(2)], stamp(FXID, 1), policy_small());

    let err = run_session(&mut w, &rt, par_providers(&shared), 2000, Some(1500)).expect_err("must fail");
    assert!(matches!(err, WriteError::Refused { .. }), "typed: {err:?}");
    assert!(tmp_residue(&shared).is_empty(), "no temp residue after cancel");
    assert!(w.sealed_parts().is_empty(), "nothing deposited on error");

    // Retry on the same writer: full success, then publish.
    let n = run_session(&mut w, &rt, par_providers(&shared), 2000, None).expect("retry");
    assert_eq!(n, 2000);
    assert_eq!(w.sealed_parts().len(), 8, "2000 rows / 256-row parts");
    let probe = Probe::new(pgrc_write::publish::TxnVerdict::Committed);
    let mut vfs_handle = pgrc_write::par::SharedMemVfs::clone(&shared);
    let out = w.publish(&mut vfs_handle, &probe).expect("publish");
    assert_eq!(out.part_nos.len(), 8);
    assert!(tmp_residue(&shared).is_empty(), "publish renamed every temp");
    pool.shutdown();
}

/// Leg 2: a WORKER-side normalize error (corrupt pglz toast in one chunk)
/// surfaces typed, cancels the session, and cleanup is total.
#[test]
fn worker_error_is_typed_and_cleanup_total() {
    let (rt, pool) = rt_pool(4);
    let shared = shared_mem_with_dir();
    let mut w = open_writer_policy(vec![int8_col(1), text_col(2)], stamp(FXID, 1), policy_small());

    let err = pgrc_ingest_par::parallel_ingest(&mut w, &rt, par_providers(&shared), opts_small(), |sess| {
            for i in 0..1000u64 {
                if i == 700 {
                    // A 4B-C image whose pglz payload is garbage.
                    let mut img = img_pglz(b"a compressible payload a compressible payload");
                    let l = img.len();
                    for b in &mut img[8..l] {
                        *b ^= 0xA5;
                    }
                    sess.append_row(&[RawDatum::Word(1), RawDatum::Bytes(&img)])?;
                } else {
                    with_mixed_row(i, |row| sess.append_row(row))?;
                }
            }
            Ok(())
        })
        .expect_err("corrupt toast must fail the session");
    assert!(
        matches!(err, WriteError::Contract { detail } if detail.contains("pglz")),
        "typed pglz refusal, got {err:?}"
    );
    assert!(tmp_residue(&shared).is_empty(), "no residue after worker error");
    pool.shutdown();
}

/// Leg 3: the kill-9 sweep at dop>1 — crash the shared universe at EVERY
/// op boundary of a logged parallel session + publish, then recover (WAL
/// replay + cleanup): the committed generation-1 bytes survive wholesale,
/// the uncommitted session leaves no residue, and a full parallel retry
/// publishes generation 2.
#[test]
fn kill9_sweep_over_parallel_session_exactly_old_or_new() {
    let total_ops = {
        let (rt, pool) = rt_pool(2);
        let shared = shared_mem_with_dir();
        let log = WalLog::shared();
        seed_acked_generation(&shared, &log);
        let before = shared.with(|v| v.op_count());
        let mut w = session_writer(FXID + 1, &log);
        run_session(&mut w, &rt, logged_par_providers(&shared, &log), 700, None).expect("session");
        let probe = Probe::new(pgrc_write::publish::TxnVerdict::Committed);
        let mut vfs_handle = SharedMemVfs::clone(&shared);
        w.publish(&mut vfs_handle, &probe).expect("publish");
        pool.shutdown();
        shared.with(|v| v.op_count()) - before
    };
    assert!(total_ops > 20, "the window must be real (got {total_ops})");

    let mut crashed_points = 0u64;
    for crash_at in 1..=total_ops {
        let (rt, pool) = rt_pool(2);
        let shared = shared_mem_with_dir();
        let log = WalLog::shared();
        let gen1 = seed_acked_generation(&shared, &log);
        shared.with(|v| v.crash_at_op(crash_at));

        let mut w = session_writer(FXID + 1, &log);
        let probe = Probe::new(pgrc_write::publish::TxnVerdict::Committed);
        let res: WriteResult<Vec<u32>> =
            run_session(&mut w, &rt, logged_par_providers(&shared, &log), 700, None).and_then(|_| {
                let mut vfs_handle = SharedMemVfs::clone(&shared);
                w.publish(&mut vfs_handle, &probe).map(|out| out.part_nos)
            });
        if !shared.with(|v| v.killed()) {
            pool.shutdown();
            // The armed op was never reached (scheduling variance shifted
            // ops past the session end): the run simply succeeded.
            res.expect("uncrashed run succeeds");
            continue;
        }
        crashed_points += 1;
        if res.is_ok() {
            log_commit(&log, FXID + 1);
        }
        let upto = durable_upto(&log, crash_at % 2 == 0);
        shared.with(|v| crash_and_recover(v, &log, upto));
        assert!(tmp_residue(&shared).is_empty(), "crash_at={crash_at}: recovery reclaims all temp residue");
        for (name, bytes) in &gen1 {
            let now = shared.with(|v| v.read_full(&format!("{DIR}/{name}")).expect("committed part"));
            assert_eq!(&now, bytes, "crash_at={crash_at}: committed bytes moved");
        }
        let mut vfs_handle = SharedMemVfs::clone(&shared);
        let m = pgrc_write::publish::effective_manifest(&mut vfs_handle, DIR, &committed_probe(&log), None)
            .expect("manifest readable")
            .expect("generation 1 survives");
        assert_eq!(m.header.gen, if res.is_ok() { 2 } else { 1 }, "crash_at={crash_at}: exactly old or new");

        // Retry: a fresh parallel session + publish succeeds post-recovery
        // (same pool — the runtime survived; only the vfs universe died).
        let mut w2 = session_writer(FXID + 2, &log);
        run_session(&mut w2, &rt, logged_par_providers(&shared, &log), 700, None).expect("retry session");
        let mut vfs_handle = SharedMemVfs::clone(&shared);
        w2.publish(&mut vfs_handle, &probe).expect("retry publish");
        assert!(tmp_residue(&shared).is_empty());
        pool.shutdown();
    }
    // The sweep must have BITTEN (both-teeth law: a sweep that never
    // crashed proved nothing).
    assert!(
        crashed_points * 2 >= total_ops,
        "sweep must crash at most points: {crashed_points}/{total_ops}"
    );
}

fn session_writer(fxid: u64, log: &SharedLog) -> TableWriter {
    open_writer_policy(vec![int8_col(1), text_col(2)], stamp(fxid, 1), policy_small())
        .with_sink(Box::new(RecordingSink::new(DIR, log.clone())))
}

/// Seed one committed serial generation through `log`; return its published
/// part files (name → bytes) as the survival oracle.
fn seed_acked_generation(shared: &SharedMemVfs, log: &SharedLog) -> Vec<(String, Vec<u8>)> {
    let mut w = open_writer_policy(
        vec![int8_col(1), text_col(2)],
        stamp(FXID, 1),
        PartCutPolicy {
            max_rows: 512,
            max_bytes: u64::MAX,
            cut_granule_rows: 512,
        },
    )
    .with_sink(Box::new(RecordingSink::new(DIR, log.clone())));
    let mut kit = Kit::new();
    let mut vfs_handle = pgrc_write::par::SharedMemVfs::clone(shared);
    for i in 0..600u64 {
        let sources: [&dyn pgrc_write::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv {
            vfs: &mut vfs_handle,
            sources: &sources,
            resolver: &kit.resolver,
            shred: &mut kit.shred,
            shred_opts: &kit.opts,
        };
        with_mixed_row(i, |row| w.append_row(row, &mut kit.ext, &mut env).expect("append"));
    }
    let sources: [&dyn pgrc_write::elect::CandidateSource; 1] = [&kit.cands];
    let mut env = SealEnv {
        vfs: &mut vfs_handle,
        sources: &sources,
        resolver: &kit.resolver,
        shred: &mut kit.shred,
        shred_opts: &kit.opts,
    };
    w.finish(&mut env).expect("finish");
    let probe = Probe::new(pgrc_write::publish::TxnVerdict::Committed);
    let out = w.publish(&mut vfs_handle, &probe).expect("publish gen1");
    log_commit(log, FXID);
    out.part_nos
        .iter()
        .map(|pn| {
            let name = pgrc_format::dirlayout::part_file_name(*pn);
            let bytes = shared.with(|v| v.read_full(&format!("{DIR}/{name}")).expect("part"));
            (name, bytes)
        })
        .collect()
}
