//! The rtpool binding (M3-I slice legs: rtpool-only assertion + dop-ladder
//! byte stability on the REAL runtime). Real `runtime::Runtime` +
//! `WorkerPool` — the same pool population the server runs; this crate
//! spawns no thread of its own (structurally pinned below).

use pgrc_write::testkit::*;
use pgrc_write::wvfs::WriteVfs;
use pgrc_write::writer::{PartCutPolicy, SealEnv};
use pgrc_write::par::ParIngestOpts;
use runtime::{Runtime, RuntimeConfig, WorkerPool};
use std::collections::BTreeMap;
use std::sync::Arc;

const FXID: u64 = 42;

fn rt_pool(workers: usize) -> (Arc<Runtime>, WorkerPool) {
    let rt = Runtime::new(RuntimeConfig::new(workers));
    let pool = WorkerPool::spawn_std(Arc::clone(&rt)).expect("pool");
    (rt, pool)
}

/// Run one full parallel ingest of `n` mixed rows at the given pool width;
/// return (tmp bytes per seq, logical totals).
fn run_at_dop(
    workers: usize,
    n: u64,
    policy: PartCutPolicy,
    chunk_rows: u32,
) -> (Vec<Vec<u8>>, BTreeMap<(u32, u32), (u64, u64, u64)>) {
    let (rt, pool) = rt_pool(workers);
    let shared = shared_mem_with_dir();
    let mut w = open_writer_policy(vec![int8_col(1), text_col(2)], stamp(FXID, 1), policy);
    let opts = ParIngestOpts {
        chunk_rows,
        max_chunks_in_flight: 16,
        max_parts_in_flight: 8,
    };
    pgrc_ingest_par::parallel_ingest(&mut w, &rt, par_providers(&shared), opts, |sess| {
        for i in 0..n {
            with_mixed_row(i, |row| sess.append_row(row))?;
        }
        Ok(())
    })
    .expect("parallel ingest");
    let bytes = w
        .sealed_parts()
        .iter()
        .map(|p| shared.with(|v| v.read_full(&format!("{DIR}/{}", p.tmp_name)).expect("tmp")))
        .collect();
    let totals = logical_totals(w.seal_reports());
    pool.shutdown();
    (bytes, totals)
}

/// Leg 1: the dop ladder {1, 2, 4, 8} is byte-stable AND byte-identical to
/// the serial writer on a row-budget partition (chunk-aligned max_rows) —
/// the §1.9 law on the real pool, claim schedules genuinely concurrent.
#[test]
fn dop_ladder_byte_stable_and_serial_identical() {
    let n: u64 = 20_000;
    let chunk_rows: u32 = 1024;
    let policy = PartCutPolicy {
        max_rows: 4096, // 4 chunks per part — divisible, so serial cuts match
        max_bytes: u64::MAX,
        cut_granule_rows: 1024, // == chunk_rows (M3-I)
    };
    let serial = serial_mixed_tmp_bytes(n, FXID, policy);
    assert_eq!(serial.len(), 5, "20000 rows / 4096-row parts + tail");
    for dop in [1usize, 2, 4, 8] {
        let (bytes, _) = run_at_dop(dop, n, policy, chunk_rows);
        assert_eq!(bytes, serial, "dop {dop} diverged from serial");
    }
}

/// Leg 2: full pipeline e2e at dop 4 — parallel ingest deposits into the
/// UNCHANGED lifecycle: publish (spec §13.3, serial, above the frozen
/// boundary) renames every temp, the manifest lists parts in seq order, and
/// the logical totals equal the serial oracle's.
#[test]
fn full_pipeline_publish_e2e_at_dop4() {
    let (rt, pool) = rt_pool(4);
    let shared = shared_mem_with_dir();
    let policy = PartCutPolicy {
        max_rows: 4096,
        max_bytes: u64::MAX,
        cut_granule_rows: 1024, // == chunk_rows (M3-I)
    };
    let mut w = open_writer_policy(vec![int8_col(1), text_col(2)], stamp(FXID, 1), policy);
    let n: u64 = 10_000;
    pgrc_ingest_par::parallel_ingest(
        &mut w,
        &rt,
        par_providers(&shared),
        ParIngestOpts {
            chunk_rows: 1024,
            max_chunks_in_flight: 16,
            max_parts_in_flight: 8,
        },
        |sess| {
            for i in 0..n {
                with_mixed_row(i, |row| sess.append_row(row))?;
            }
            Ok(())
        },
    )
    .expect("ingest");
    let par_totals = logical_totals(w.seal_reports());

    let probe = Probe::new(pgrc_write::publish::TxnVerdict::Committed);
    let mut vfs_handle = pgrc_write::par::SharedMemVfs::clone(&shared);
    let out = w.publish(&mut vfs_handle, &probe).expect("publish");
    assert_eq!(out.part_nos, (0..3).collect::<Vec<u32>>(), "3 parts, seq order");

    // No temp residue; every part file readable.
    let names = shared.with(|v| v.list_dir(DIR).expect("list"));
    assert!(names.iter().all(|s| !s.starts_with("tmp-")), "temps all renamed");
    for pn in &out.part_nos {
        let name = pgrc_format::dirlayout::part_file_name(*pn);
        assert!(names.contains(&name), "part file {name} published");
    }

    // Logical identity vs the serial oracle.
    let serial_totals = {
        let mut vfs = mem_with_dir();
        let mut kit = Kit::new();
        let mut sw =
            open_writer_policy(vec![int8_col(1), text_col(2)], stamp(FXID, 1), policy);
        for i in 0..n {
            let sources: [&dyn pgrc_write::elect::CandidateSource; 1] = [&kit.cands];
            let mut env = SealEnv {
                vfs: &mut vfs,
                sources: &sources,
                resolver: &kit.resolver,
                shred: &mut kit.shred,
                shred_opts: &kit.opts,
            };
            with_mixed_row(i, |row| sw.append_row(row, &mut kit.ext, &mut env).expect("append"));
        }
        let sources: [&dyn pgrc_write::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv {
            vfs: &mut vfs,
            sources: &sources,
            resolver: &kit.resolver,
            shred: &mut kit.shred,
            shred_opts: &kit.opts,
        };
        sw.finish(&mut env).expect("finish");
        logical_totals(sw.seal_reports())
    };
    assert_eq!(par_totals, serial_totals, "O-10 logical identity");
    pool.shutdown();
}

/// Leg 3: empty COPY is a clean no-op (no parts, no files, writer reusable).
#[test]
fn empty_copy_is_a_clean_noop() {
    let (rt, pool) = rt_pool(2);
    let shared = shared_mem_with_dir();
    let mut w = open_writer(vec![int8_col(1), text_col(2)], stamp(FXID, 1));
    pgrc_ingest_par::parallel_ingest(
        &mut w,
        &rt,
        par_providers(&shared),
        ParIngestOpts::default(),
        |_sess| Ok(()),
    )
    .expect("empty session");
    assert!(w.sealed_parts().is_empty());
    assert!(shared.with(|v| v.list_dir(DIR).expect("list")).is_empty());
    pool.shutdown();
}

/// Leg 4: one statement is serial OR parallel, never spliced — the typed
/// guard.
#[test]
fn parallel_over_serial_buffered_rows_refused() {
    let (rt, pool) = rt_pool(2);
    let shared = shared_mem_with_dir();
    let mut kit = Kit::new();
    let mut vfs = mem_with_dir();
    let mut w = open_writer(vec![int8_col(1)], stamp(FXID, 1));
    append_int8_rows(&mut w, &mut vfs, &mut kit, 10, |i| Some(i as i64));
    let err = pgrc_ingest_par::parallel_ingest(
        &mut w,
        &rt,
        par_providers(&shared),
        ParIngestOpts::default(),
        |_s| Ok(()),
    )
    .expect_err("must refuse");
    assert!(matches!(err, pgrc_write::WriteError::Contract { .. }));
    pool.shutdown();
}

/// Leg 5 (the rtpool-only assertion, §7.1 law 6): the parallel-ingest module
/// contains NO thread population of its own — no `std::thread`, no ad-hoc
/// spawn; the ONLY execution vehicle is the runtime (rtpool) task set, and
/// the coordinator's sync comes from pgsync (loom-modelable). Source-pinned
/// so a future edit that sneaks a thread in goes RED here.
#[test]
fn rtpool_only_no_new_thread_population() {
    // Both halves of the split: the engine (pgrc_write/src/par.rs) and the
    // binding (this crate's src/lib.rs).
    let engine = include_str!("../../pgrc_write/src/par.rs");
    let binding = include_str!("../src/lib.rs");
    // colload (k): the publish's fsync fan-out face (its fsyncs run on the binding's task set).
    let wvfs = include_str!("../../pgrc_write/src/wvfs.rs");
    for (name, src) in [("par.rs", engine), ("lib.rs", binding), ("wvfs.rs", wvfs)] {
        // v4 delta: the engine's NotYetPublished claim arm yields
        // (`std::thread::yield_now`) — a scheduling hint on the CURRENT
        // thread, never a population. The pin targets creation surfaces.
        let src = src.replace("std::thread::yield_now", "");
        let src = src.as_str();
        assert!(
            !src.contains("std::thread"),
            "{name} must not touch std::thread (rtpool is the only population)"
        );
        assert!(
            !src.contains("thread::spawn"),
            "{name} must not spawn threads"
        );
        assert!(
            !src.contains("thread_local"),
            "{name}: no thread-resident state (TLS census pinned)"
        );
    }
    assert!(
        binding.contains("runtime::"),
        "the runtime (rtpool) binding is the sanctioned execution vehicle"
    );
}

/// Leg 6: the pool-engagement witness — at dop 4 with many chunks the work
/// provably ran on rtpool worker ordinals (within the pool's range). This is
/// the engagement side of the rtpool-only law; the structural side is leg 5.
#[test]
fn pool_engagement_witness() {
    let (rt, pool) = rt_pool(4);
    let shared = shared_mem_with_dir();
    let schema = vec![int8_col(1), text_col(2)];
    let spec = pgrc_write::seal::PartSpec {
        spc: SPC,
        db: DB,
        relfilenumber: RELFILENUMBER,
        schema_fingerprint: pgrc_format::ident::schema_fingerprint(&schema),
    };
    let engine = Arc::new(
        pgrc_write::par::ParEngine::new(
            par_providers(&shared),
            schema,
            spec,
            DIR.to_string(),
            FXID,
            PartCutPolicy {
                max_rows: 4096,
                max_bytes: u64::MAX,
                cut_granule_rows: 512, // == chunk_rows below (M3-I)
            },
            ParIngestOpts {
                chunk_rows: 512,
                max_chunks_in_flight: 32,
                max_parts_in_flight: 8,
            },
            0,
        )
        .expect("engine"),
    );
    let (_parts, _reports, ()) =
        pgrc_ingest_par::run_parallel(&rt, Arc::clone(&engine), |sess| {
            for i in 0..20_000u64 {
                with_mixed_row(i, |row| sess.append_row(row))?;
            }
            Ok(())
        })
        .expect("run");
    let seen = engine.workers_seen();
    assert!(!seen.is_empty(), "ingest work ran");
    let nthreads = rt.nthreads();
    assert!(
        seen.iter().all(|&wkr| wkr < nthreads),
        "every executing ordinal is a pool worker (seen {seen:?}, pool {nthreads})"
    );
    pool.shutdown();
}

/// D3922: the pool workers run a session's chunks on the leader's memory account, so what they allocate and free (the
/// normalized chunks, the leader's captured chunks they drop, the parts they seal) nets on the loading statement's
/// account, not on the pool threads' own (UNBOUND). The detoast capability is minted on the worker per chunk.
#[test]
fn pool_workers_run_the_chunks_on_the_leaders_memory_account() {
    struct SeenAccounts(std::sync::Mutex<Vec<u32>>);
    impl pgrc_write::par::DetoastProvider for SeenAccounts {
        fn make(&self) -> Box<dyn pgrc_write::ingest::ExternalDetoast + Send> {
            self.0.lock().expect("seen").push(memledger::current().0);
            pgrc_write::par::DetoastProvider::make(&pgrc_write::par::NoDetoastProvider)
        }
    }
    let (rt, pool) = rt_pool(4);
    let shared = shared_mem_with_dir();
    let seen = Arc::new(SeenAccounts(std::sync::Mutex::new(Vec::new())));
    let providers = pgrc_write::par::ParProviders { detoast: Arc::clone(&seen) as Arc<dyn pgrc_write::par::DetoastProvider>, ..par_providers(&shared) };
    memledger::bind_backend(4001, 0);
    let leader = memledger::current();
    let mut w = open_writer_policy(vec![int8_col(1), text_col(2)], stamp(FXID, 1), PartCutPolicy {
        max_rows: 4096,
        max_bytes: u64::MAX,
        cut_granule_rows: 512,
    });
    let opts = ParIngestOpts { chunk_rows: 512, max_chunks_in_flight: 32, max_parts_in_flight: 8 };
    pgrc_ingest_par::parallel_ingest(&mut w, &rt, providers, opts, |sess| {
        for i in 0..20_000u64 {
            with_mixed_row(i, |row| sess.append_row(row))?;
        }
        Ok(())
    })
    .expect("parallel ingest");
    memledger::unbind_backend();
    pool.shutdown();
    let seen = seen.0.lock().expect("seen");
    assert!(!seen.is_empty(), "the chunks were normalized");
    assert!(leader.is_backend(), "the test thread took a backend's account");
    assert!(seen.iter().all(|&a| a == leader.0), "every chunk on the leader's account {} (seen {seen:?})", leader.0);
}

/// colload (k): the publish's fsync fan-out runs on the runtime pool. Every descriptor is fsynced exactly once (the
/// real-fsync count rises by exactly the batch size), a 2-batch set is reached in full, and a failure reports the
/// FIRST failing descriptor in order with its errno.
#[test]
fn publish_fsync_fanout_on_the_pool() {
    let rt = Runtime::new(RuntimeConfig::new(4));
    let pool = WorkerPool::spawn_std(Arc::clone(&rt)).expect("pool");
    runtime::install_global(Arc::clone(&rt));
    pgrc_ingest_par::install_fsync_fanout();
    let dir = std::env::temp_dir().join(format!("colload_fsync_fanout_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    let mut vfs = pgrc_write::wvfs::RealVfs;
    let n = pgrc_write::wvfs::FSYNC_BATCH + 9;
    let mut fds = Vec::new();
    for i in 0..n {
        let p = dir.join(format!("f{i}")).to_string_lossy().into_owned();
        let fd = vfs.create_rw(&p).expect("create");
        vfs.pwrite_at(&fd, 0, &[i as u8; 4096]).expect("write");
        fds.push(fd);
    }
    let before = pgrc_write::wvfs::real_fsyncs();
    vfs.fsync_files(&fds).expect("fsync_files");
    assert_eq!(pgrc_write::wvfs::real_fsyncs() - before, n as u64, "one fsync per descriptor");
    // FIFOs at index 5 and 40 of the first batch (fsync(2) of a FIFO is EINVAL): the error names index 5.
    let mut bad = Vec::new();
    for i in 0..48 {
        let p = dir.join(format!("g{i}")).to_string_lossy().into_owned();
        if i == 5 || i == 40 {
            let c = std::ffi::CString::new(p.clone()).expect("path");
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0, "mkfifo");
            bad.push(vfs.open_rw(&p).expect("open fifo"));
        } else {
            bad.push(vfs.create_rw(&p).expect("create"));
        }
    }
    let (p5, p40) = (bad[5].path().to_string(), bad[40].path().to_string());
    match vfs.fsync_files(&bad) {
        Err(pgrc_write::WriteError::Io { op: "fsync", path, errno }) => {
            assert_eq!(path, p5, "the first failing descriptor in order (not {p40})");
            assert_eq!(errno, libc::EINVAL);
        }
        other => panic!("expected the index-5 EINVAL, got {:?}", other.err()),
    }
    for fd in fds.into_iter().chain(bad) {
        vfs.close_file(fd).expect("close");
    }
    let _ = std::fs::remove_dir_all(&dir);
    pool.shutdown();
}
