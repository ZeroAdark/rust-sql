#![cfg(not(any(loom, pgrust_sim)))]
//! D4387: under `pgrust.memory_ladder = on` a load's gate asks for every load's seal reserve not yet resident, not only
//! its own: three COPYs at once each fit their own part's seal under the ladder's line, then sealed together past the
//! kernel's limit (memtest c3, 2G). Here one serial writer holds a ~10 MiB part reserve (a buffered part, not cut) and a
//! second load (a parallel session, then a serial writer) whose own ask is a few KiB starts with the read 2 MiB under
//! the line: it is refused (53200 at the AM) while the first holds its reserve, and served once the first is gone. Born
//! red: each gate asked for its own load's reserve only, so the second load fit.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use pgrc_format::relopt::ShredOptions;
use pgrc_write::elect::{CandidateSource, ReferenceCandidates};
use pgrc_write::ingest::RawDatum;
use pgrc_write::par::{
    NoDetoastProvider, NoShredProvider, ParEngine, ParIngestOpts, ParProviders, RowChunk, SharedMemVfs,
    SharedMemVfsProvider,
};
use pgrc_write::seal::{PartSpec, ReferenceResolver};
use pgrc_write::testkit::{int8_col, mem_with_dir, stamp, Kit, DB, DIR, RELFILENUMBER, SPC};
use pgrc_write::writer::{PartCutPolicy, SealEnv, SubxactEvidence, TableWriter};
use pgrc_write::wvfs::{MemVfs, WriteVfs};
use pgrc_write::WriteError;

const MIB: u64 = 1 << 20;
const LINE: u64 = 1000 * MIB;
/// The first load's buffered rows: 640k int8 values, ~5 MiB buffered, a ~10 MiB seal reserve.
const HOLDER_ROWS: u64 = 640_000;

static SERIAL: Mutex<()> = Mutex::new(());
static READ: AtomicU64 = AtomicU64::new(0);

fn read() -> Option<u64> {
    Some(READ.load(Ordering::SeqCst))
}
fn zero() -> u64 {
    0
}
fn none(_: u64) -> u64 {
    0
}
fn no_cancel(_: u64) -> bool {
    false
}
fn no() -> bool {
    false
}

static ACTS: mcx::ladder::Acts = mcx::ladder::Acts {
    read,
    reclaimable: zero,
    reclaim: none,
    cancel: no_cancel,
    cancel_in_flight: no,
    protected: zero,
    evict_protected: none,
};

fn ladder(mode: u8, read: u64) {
    mcx::ladder::install(&ACTS);
    READ.store(read, Ordering::SeqCst);
    mcx::ladder::publish(mode, LINE);
}

fn writer(max_rows: u64) -> TableWriter {
    TableWriter::open(
        DIR.to_string(),
        vec![int8_col(1)],
        SPC,
        DB,
        RELFILENUMBER,
        stamp(7, 1),
        &SubxactEvidence::default(),
        PartCutPolicy { max_rows, max_bytes: u64::MAX, cut_granule_rows: 64 },
    )
    .expect("open writer")
}

/// A serial load of `rows` rows into `w` (no finish: what it buffered stays buffered).
fn append(w: &mut TableWriter, rows: u64) -> Result<(), WriteError> {
    let mut kit = Kit::new();
    let mut vfs = mem_with_dir();
    let sources: [&dyn CandidateSource; 1] = [&kit.cands];
    let mut env = SealEnv { vfs: &mut vfs, sources: &sources, resolver: &kit.resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
    for i in 0..rows {
        w.append_row(&[RawDatum::Word(i)], &mut kit.ext, &mut env)?;
    }
    Ok(())
}

/// The first load: a buffered part under the line (read 0), its reserve left in place.
fn holder() -> TableWriter {
    READ.store(0, Ordering::SeqCst);
    buffered()
}

/// A holder's writer, the read left as it is.
fn buffered() -> TableWriter {
    let mut w = writer(u64::MAX);
    append(&mut w, HOLDER_ROWS).expect("the holder's rows, under the line");
    w
}

/// A second, small serial load: two granules, then its finish.
fn small_serial() -> Result<(), WriteError> {
    let mut kit = Kit::new();
    let mut vfs = mem_with_dir();
    let mut w = writer(1024);
    let sources: [&dyn CandidateSource; 1] = [&kit.cands];
    let mut env = SealEnv { vfs: &mut vfs, sources: &sources, resolver: &kit.resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
    for i in 0..200u64 {
        w.append_row(&[RawDatum::Word(i)], &mut kit.ext, &mut env)?;
    }
    w.finish(&mut env)
}

/// A parallel session (two workers) of `chunk_rows`-row chunks cut at `max_rows` rows: the engine and its workers.
fn session(chunk_rows: u32, max_rows: u64) -> (Arc<ParEngine>, Vec<std::thread::JoinHandle<()>>) {
    const PDIR: &str = "/tbl/pgrc_4387";
    let mut v = MemVfs::new();
    v.mkdir_path(PDIR).expect("mkdir");
    let shared = SharedMemVfs::new(v);
    let schema = vec![int8_col(1)];
    let spec = PartSpec { spc: 1663, db: 5, relfilenumber: 4387, schema_fingerprint: pgrc_format::ident::schema_fingerprint(&schema) };
    let e = Arc::new(
        ParEngine::new(
            ParProviders {
                vfs: Arc::new(SharedMemVfsProvider(shared)),
                detoast: Arc::new(NoDetoastProvider),
                shred: Arc::new(NoShredProvider),
                sources: vec![Arc::new(ReferenceCandidates)],
                resolver: Arc::new(ReferenceResolver),
                shred_opts: ShredOptions::default(),
                structural: pgrc_write::structural::StructuralPolicy::default(),
                wal: Arc::new(pgrc_write::par::NoWalProvider),
            },
            schema,
            spec,
            PDIR.to_string(),
            4387,
            PartCutPolicy { max_rows, max_bytes: u64::MAX, cut_granule_rows: chunk_rows },
            ParIngestOpts { chunk_rows, max_chunks_in_flight: 64, max_parts_in_flight: 64 },
            0,
        )
        .expect("engine"),
    );
    let workers = (0..2)
        .map(|w| {
            let e = Arc::clone(&e);
            std::thread::spawn(move || e.run_worker(w))
        })
        .collect();
    (e, workers)
}

/// Publish chunks `from..to` of `rows` rows each (values from `from * rows`).
fn publish(e: &ParEngine, rows: u32, from: u64, to: u64) -> Result<(), WriteError> {
    for id in from..to {
        let mut c = RowChunk::new(1, rows);
        for i in 0..rows as u64 {
            c.push_row(&[RawDatum::Word(id * rows as u64 + i)]).expect("push");
        }
        e.publish_chunk(c, id)?;
    }
    Ok(())
}

/// End a session after `out` with `published` chunks: its workers joined, its parts collected.
fn end(e: Arc<ParEngine>, workers: Vec<std::thread::JoinHandle<()>>, out: Result<(), WriteError>, published: u64) -> Result<(), WriteError> {
    if let Err(err) = &out {
        e.fail(err.clone());
    }
    e.close_input(published);
    for w in workers {
        w.join().expect("worker");
    }
    e.release_charge();
    out?;
    e.collect().map(|_| ())
}

/// A second, small parallel load: one session of 4 chunks of 64 rows (one part), two workers.
fn small_parallel() -> Result<(), WriteError> {
    let (e, workers) = session(64, 256);
    let out = publish(&e, 64, 0, 4);
    end(e, workers, out, 4)
}

/// Finish `w` (its buffered part sealed) in a fresh vfs.
fn finish(w: &mut TableWriter) -> Result<(), WriteError> {
    let mut kit = Kit::new();
    let mut vfs = mem_with_dir();
    let sources: [&dyn CandidateSource; 1] = [&kit.cands];
    let mut env = SealEnv { vfs: &mut vfs, sources: &sources, resolver: &kit.resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
    w.finish(&mut env)
}

fn refused(r: Result<(), WriteError>, who: &str) {
    match r {
        Err(WriteError::Refused { what }) => assert!(what.contains("memory"), "{who}: a memory refusal: {what}"),
        other => panic!("{who}: a load whose ask fits only without the other load's seal reserve must be refused, got {other:?}"),
    }
}

#[test]
fn d4387_a_parallel_load_asks_for_another_loads_seal_reserve() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    ladder(mcx::ladder::ON, 0);
    let first = holder();
    READ.store(LINE - 2 * MIB, Ordering::SeqCst);
    let beside = small_parallel();
    drop(first);
    let alone = small_parallel();
    ladder(mcx::ladder::OFF, 0);
    refused(beside, "beside the holder");
    alone.expect("the same load alone fits under the line");
}

#[test]
fn d4387_a_serial_load_asks_for_another_loads_seal_reserve() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    ladder(mcx::ladder::ON, 0);
    let first = holder();
    READ.store(LINE - 2 * MIB, Ordering::SeqCst);
    let beside = small_serial();
    drop(first);
    let alone = small_serial();
    ladder(mcx::ladder::OFF, 0);
    refused(beside, "beside the holder");
    alone.expect("the same load alone fits under the line");
}


#[test]
fn d4387_a_load_that_ends_leaves_no_reserve_behind() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    ladder(mcx::ladder::ON, 0);
    let before = pgrc_write::par::seal_ahead_all();
    drop(holder());
    small_serial().expect("under the line");
    small_parallel().expect("under the line");
    ladder(mcx::ladder::OFF, 0);
    assert_eq!(pgrc_write::par::seal_ahead_all(), before, "every load's reserve leaves the process-wide one when it ends");
}

/// Three loads (two serial writers, one parallel session) each hold a ~10 MiB seal reserve of an open part; then the read
/// leaves 15 MiB under the line: any one fits alone, no two together, and nothing else changes. All three block at their
/// gates at once; one passes, finishes and seals, then the next, then the last: all are served. (Each counting the
/// others' whole reserves, all three waited on one another and all were refused, the c3 rig at 2G.)
#[test]
fn d4387_loads_that_each_fit_alone_all_blocked_are_served_one_after_another() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    ladder(mcx::ladder::ON, 0);
    let before = pgrc_write::par::seal_ahead_all();
    const ROWS: u32 = 8192;
    const CHUNKS: u64 = HOLDER_ROWS / ROWS as u64;
    let at = Arc::new(std::sync::Barrier::new(4));
    let serial: Vec<_> = (0..2)
        .map(|_| {
            let at = Arc::clone(&at);
            std::thread::spawn(move || {
                let mut w = buffered();
                at.wait();
                at.wait();
                append(&mut w, 20_000)?;
                finish(&mut w)
            })
        })
        .collect();
    let parallel = {
        let at = Arc::clone(&at);
        std::thread::spawn(move || {
            let (e, workers) = session(ROWS, u64::MAX);
            let first = publish(&e, ROWS, 0, CHUNKS);
            at.wait();
            at.wait();
            let out = first.and_then(|()| publish(&e, ROWS, CHUNKS, CHUNKS + 1));
            end(e, workers, out, CHUNKS + 1)
        })
    };
    at.wait();
    READ.store(LINE - 15 * MIB, Ordering::SeqCst);
    at.wait();
    let served: Vec<Result<(), WriteError>> =
        serial.into_iter().chain(std::iter::once(parallel)).map(|t| t.join().expect("load thread")).collect();
    ladder(mcx::ladder::OFF, 0);
    for (i, r) in served.into_iter().enumerate() {
        r.unwrap_or_else(|e| panic!("load {i}: each fits alone, so each is served in its turn, got {e:?}"));
    }
    assert_eq!(pgrc_write::par::seal_ahead_all(), before, "every load's reserve leaves the process-wide one when it ends");
}
