//! Parallel COPY under WAL (W0-3): the leader logs each worker's part as it finishes, the log equals serial's per
//! file, a part's slot is held until it is logged, replay rebuilds every temp, and a failed session reclaims its
//! temps under one `RECLAIM`.

use super::*;
use crate::par::{ParEngine, ParIngestOpts, RowChunk};
use crate::seal::PartSpec;
use crate::walmodel::{RecordingSink, WalLog, WalRecord};
use pgrc_format::dirlayout::temp_file_name;
use pgsync::{lock, Mutex};
use std::sync::Arc;

const FXID: u64 = 42;
const CHUNK_ROWS: u32 = 128;
const POLICY: PartCutPolicy = PartCutPolicy { max_rows: 256, max_bytes: u64::MAX, cut_granule_rows: CHUNK_ROWS };

fn logged_engine(shared: &SharedMemVfs, log: &Arc<Mutex<WalLog>>, policy: PartCutPolicy) -> Arc<ParEngine> {
    let schema = vec![int8_col(1), text_col(2)];
    let spec = PartSpec { spc: SPC, db: DB, relfilenumber: RELFILENUMBER, schema_fingerprint: pgrc_format::ident::schema_fingerprint(&schema) };
    let providers = logged_par_providers(shared, log);
    let opts = ParIngestOpts { chunk_rows: CHUNK_ROWS, max_chunks_in_flight: 4, max_parts_in_flight: 1 };
    Arc::new(ParEngine::new(providers, schema, spec, DIR.to_string(), FXID, policy, opts, 0).expect("engine"))
}

fn run_dop4(engine: &Arc<ParEngine>, chunks: Vec<RowChunk>) {
    run_dop4_holding(engine, chunks, None)
}

/// [`run_dop4`], publishing chunk `hold` only once a part is assigned (bounded: a stuck cut fails the test).
fn run_dop4_holding(engine: &Arc<ParEngine>, chunks: Vec<RowChunk>, hold: Option<u64>) {
    let total = chunks.len() as u64;
    std::thread::scope(|s| {
        for w in 0..4usize {
            let engine = &engine;
            s.spawn(move || engine.run_worker(w));
        }
        for (id, ch) in chunks.into_iter().enumerate() {
            if hold == Some(id as u64) {
                let t0 = std::time::Instant::now();
                while engine.assigned_parts() == 0 {
                    assert!(t0.elapsed() < std::time::Duration::from_secs(30), "no part assigned before the held chunk");
                    std::thread::yield_now();
                }
            }
            if engine.publish_chunk(ch, id as u64).is_err() {
                break;
            }
        }
        engine.close_input(total);
    });
}

type FileLog = BTreeMap<String, Vec<(u64, Vec<u8>, bool)>>;

fn file_log(log: &WalLog) -> FileLog {
    let mut out = FileLog::new();
    for r in &log.records {
        if let WalRecord::File { name, off, bytes, create, .. } = r {
            out.entry(name.clone()).or_default().push((*off, bytes.clone(), *create));
        }
    }
    out
}

fn temps(vfs: &mut MemVfs) -> Vec<String> {
    vfs.list_dir(DIR).expect("list").into_iter().filter(|n| n.starts_with("tmp-")).collect()
}

#[test]
fn every_part_is_logged_as_serial_logs_it_and_replay_rebuilds_it() {
    let n_rows = 12 * CHUNK_ROWS as u64 + 37;
    let serial = WalLog::shared();
    {
        let mut vfs = mem_with_dir();
        let mut kit = Kit::new();
        let mut w = open_writer_policy(vec![int8_col(1), text_col(2)], stamp(FXID, 1), POLICY)
            .with_sink(Box::new(RecordingSink::new(DIR, serial.clone())));
        for i in 0..n_rows {
            let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
            let mut env = SealEnv { vfs: &mut vfs, sources: &sources, resolver: &kit.resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
            with_mixed_row(i, |row| w.append_row(row, &mut kit.ext, &mut env).expect("append"));
        }
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv { vfs: &mut vfs, sources: &sources, resolver: &kit.resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
        w.finish(&mut env).expect("finish");
    }
    let want = file_log(&lock(&serial));
    assert!(want.len() >= 4, "enough parts to interleave: {}", want.len());

    let shared = shared_mem_with_dir();
    let log = WalLog::shared();
    let engine = logged_engine(&shared, &log, POLICY);
    run_dop4(&engine, capture_mixed_chunks(n_rows, CHUNK_ROWS));
    let sealed = engine.collect().expect("collect");
    assert_eq!(sealed.len(), want.len());
    let g = lock(&log);
    assert_eq!(file_log(&g), want, "the leader logs each worker part exactly as the serial writer logs it");
    for (p, _) in &sealed {
        assert!(g.dirty[DIR].contains(&p.tmp_name), "{} finished through file_done", p.tmp_name);
    }
    let upto = g.records.len();
    drop(g);

    let mut fresh = mem_with_dir();
    lock(&log).replay(&mut fresh, upto).expect("replay");
    for (p, _) in &sealed {
        let path = format!("{DIR}/{}", p.tmp_name);
        assert_eq!(fresh.read_full(&path).expect("replayed"), shared.with(|v| v.read_full(&path).expect("tmp")), "{}", p.tmp_name);
    }
}

/// Eight chunks of mixed rows, one row of chunk `chunk` a malformed varlena.
fn poisoned_chunks(chunk: u64) -> Vec<RowChunk> {
    let poison_row = chunk * CHUNK_ROWS as u64 + 3;
    let mut chunks = Vec::new();
    let mut cur = RowChunk::new(2, CHUNK_ROWS);
    for i in 0..8 * CHUNK_ROWS as u64 {
        if i == poison_row {
            cur.push_row(&[RawDatum::Bytes(b"xx"), RawDatum::Null]).expect("capture");
        } else {
            with_mixed_row(i, |row| cur.push_row(row).expect("capture"));
        }
        if cur.rows() >= CHUNK_ROWS {
            chunks.push(std::mem::replace(&mut cur, RowChunk::new(2, CHUNK_ROWS)));
        }
    }
    chunks
}

/// The poison is held back until a part is assigned: a worker could otherwise normalize it before part 0 closes,
/// cancelling a session that never named a temp (the next test).
#[test]
fn a_failed_session_reclaims_its_temps_under_one_record() {
    let shared = shared_mem_with_dir();
    let log = WalLog::shared();
    let engine = logged_engine(&shared, &log, POLICY);
    run_dop4_holding(&engine, poisoned_chunks(5), Some(5));
    engine.collect().expect_err("the poisoned session errors");
    engine.cleanup_temps().expect("cleanup");

    let g = lock(&log);
    let Some(WalRecord::Reclaim { rec, .. }) = g.records.last() else { panic!("RECLAIM is the last record") };
    let assigned = rec.names.len() as u32;
    assert!(assigned > 0);
    assert_eq!(rec.names, (0..assigned).map(|i| temp_file_name(FXID, i)).collect::<Vec<_>>());
    assert_eq!(g.records.iter().filter(|r| matches!(r, WalRecord::Reclaim { .. })).count(), 1);
    let upto = g.records.len();
    drop(g);
    assert!(shared.with(|v| temps(v)).is_empty(), "the primary unlinked every temp");

    let mut fresh = mem_with_dir();
    lock(&log).replay(&mut fresh, upto).expect("replay");
    assert!(temps(&mut fresh).is_empty(), "redo of the whole log leaves no temp either");
}

/// A session that fails before any part closes named no temp, so it logs nothing and leaves nothing to reclaim.
#[test]
fn a_session_failing_before_any_part_logs_nothing() {
    let shared = shared_mem_with_dir();
    let log = WalLog::shared();
    let engine = logged_engine(&shared, &log, POLICY);
    run_dop4(&engine, poisoned_chunks(0));
    engine.collect().expect_err("the poisoned session errors");
    engine.cleanup_temps().expect("cleanup");
    assert_eq!(engine.assigned_parts(), 0);
    assert!(lock(&log).records.is_empty(), "no record for a session that named no temp");
    assert!(shared.with(|v| temps(v)).is_empty());
}

/// WR: at a threshold between the parts' sizes, the leader logs each worker's part as the serial writer does: the
/// larger by reference (fsynced in the worker), the smaller as bytes.
#[test]
fn a_parallel_copy_at_the_threshold_logs_what_a_serial_one_logs() {
    type View = BTreeMap<String, (Vec<(u64, Vec<u8>, bool)>, Option<(u64, u32)>)>;
    fn view(log: &WalLog) -> View {
        let mut out = View::new();
        for r in &log.records {
            match r {
                WalRecord::File { name, off, bytes, create, .. } => out.entry(name.clone()).or_default().0.push((*off, bytes.clone(), *create)),
                WalRecord::FileRef { name, len, crc, .. } => out.entry(name.clone()).or_default().1 = Some((*len, *crc)),
                _ => {}
            }
        }
        out
    }
    let n_rows = 12 * CHUNK_ROWS as u64 + 37;
    let serial_sizes = {
        let log = WalLog::shared();
        let mut vfs = mem_with_dir();
        serial_mixed(&mut vfs, &log, n_rows);
        let mut sizes: Vec<u64> = file_log(&lock(&log)).values().map(|c| c.iter().map(|(_, b, _)| b.len() as u64).sum()).collect();
        sizes.sort_unstable();
        sizes
    };
    let ref_min = serial_sizes[serial_sizes.len() / 2];
    assert!(serial_sizes[0] < ref_min, "the threshold splits the parts: {serial_sizes:?}");

    let serial = spool_log(ref_min, None);
    let mut vfs = mem_with_spool();
    serial_mixed(&mut vfs, &serial, n_rows);
    let want = view(&lock(&serial));
    assert!(want.values().any(|v| v.1.is_some()) && want.values().any(|v| v.1.is_none()));

    let shared = SharedMemVfs::new(mem_with_spool());
    let log = spool_log(ref_min, None);
    let engine = logged_engine(&shared, &log, POLICY);
    run_dop4(&engine, capture_mixed_chunks(n_rows, CHUNK_ROWS));
    engine.collect().expect("collect");
    let g = lock(&log);
    assert_eq!(view(&g), want);
    let upto = g.records.len();
    assert!(g.check_spool(&mut shared.clone(), 0, upto).is_ok());
}

fn serial_mixed(vfs: &mut MemVfs, log: &Arc<Mutex<WalLog>>, n_rows: u64) {
    let mut kit = Kit::new();
    let mut w = open_writer_policy(vec![int8_col(1), text_col(2)], stamp(FXID, 1), POLICY)
        .with_sink(Box::new(RecordingSink::new(DIR, log.clone())));
    for i in 0..n_rows {
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = SealEnv { vfs: &mut *vfs, sources: &sources, resolver: &kit.resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
        with_mixed_row(i, |row| w.append_row(row, &mut kit.ext, &mut env).expect("append"));
    }
    let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
    let mut env = SealEnv { vfs, sources: &sources, resolver: &kit.resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
    w.finish(&mut env).expect("finish");
}
