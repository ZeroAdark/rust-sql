#![cfg(not(loom))]
//! D4365: under `pgrust.memory_ladder = on` the parallel load's memory gate tests every chunk against the ladder's line,
//! the first part's chunks (the floor) too: a load whose read stays past the line is refused (53200 at the AM), never
//! admitted into a seal the cgroup cannot hold; one whose read falls back under the line goes on; under the line the
//! gate moves no byte of any part. Born red: trunk admitted every chunk of the first part unconditionally.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use pgrc_format::class::{ColSchema, CollationClass, StorageClass, TypeSemantics};
use pgrc_format::relopt::ShredOptions;
use pgrc_write::elect::ReferenceCandidates;
use pgrc_write::ingest::RawDatum;
use pgrc_write::par::{
    NoDetoastProvider, NoShredProvider, ParEngine, ParIngestOpts, ParProviders, RowChunk, SharedMemVfs,
    SharedMemVfsProvider, VfsProvider,
};
use pgrc_write::seal::{PartSpec, ReferenceResolver};
use pgrc_write::writer::PartCutPolicy;
use pgrc_write::wvfs::{MemVfs, WriteVfs};
use pgrc_write::WriteError;

const DIR: &str = "/tbl/pgrc_4365";
const FXID: u64 = 4365;
const CHUNKS: i64 = 8;
const MIB: u64 = 1 << 20;
const LINE: u64 = 1000 * MIB;

static SERIAL: Mutex<()> = Mutex::new(());
/// The fake ladder's read (memory.current less inactive_file).
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

fn int8_col(attno: u32) -> ColSchema {
    ColSchema {
        attno,
        class: StorageClass::ByvalWord { width: 8, signed: true },
        typlen: 8,
        typbyval: true,
        typalign: b'd',
        collation_class: CollationClass::C,
        semantics: TypeSemantics::SignedInt,
    }
}

fn engine(vfs: Arc<dyn VfsProvider>) -> Arc<ParEngine> {
    let schema = vec![int8_col(1)];
    let spec = PartSpec {
        spc: 1663,
        db: 5,
        relfilenumber: 4365,
        schema_fingerprint: pgrc_format::ident::schema_fingerprint(&schema),
    };
    Arc::new(
        ParEngine::new(
            ParProviders {
                vfs,
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
            DIR.to_string(),
            FXID,
            PartCutPolicy { max_rows: 256, max_bytes: u64::MAX, cut_granule_rows: 64 },
            ParIngestOpts { chunk_rows: 64, max_chunks_in_flight: 64, max_parts_in_flight: 64 },
            0,
        )
        .expect("engine"),
    )
}

fn chunk(base: i64) -> RowChunk {
    let mut c = RowChunk::new(1, 64);
    for i in 0..64i64 {
        c.push_row(&[RawDatum::Word((base + i) as u64)]).expect("push");
    }
    c
}

/// One session (two workers): the first publish's result, and on success the part files written.
fn load() -> Result<BTreeMap<String, Vec<u8>>, WriteError> {
    let mut v = MemVfs::new();
    v.mkdir_path(DIR).expect("mkdir");
    let shared = SharedMemVfs::new(v);
    let e = engine(Arc::new(SharedMemVfsProvider(shared.clone())));
    let workers: Vec<_> = (0..2)
        .map(|w| {
            let e = Arc::clone(&e);
            std::thread::spawn(move || e.run_worker(w))
        })
        .collect();
    let (mut out, mut published) = (Ok(()), 0u64);
    for id in 0..CHUNKS {
        if let Err(err) = e.publish_chunk(chunk(id * 64), id as u64) {
            out = Err(err);
            break;
        }
        published += 1;
    }
    if let Err(err) = &out {
        e.fail(err.clone());
    }
    e.close_input(published);
    for w in workers {
        w.join().expect("worker");
    }
    e.release_charge();
    out?;
    assert_eq!(e.collect().expect("collect").len(), (CHUNKS / 4) as usize);
    Ok(shared.with(|v| {
        v.list_dir(DIR).expect("list").into_iter().map(|n| (n.clone(), v.read_full(&format!("{DIR}/{n}")).expect("read"))).collect()
    }))
}

fn ladder(mode: u8, read: u64) {
    mcx::ladder::install(&ACTS);
    READ.store(read, Ordering::SeqCst);
    mcx::ladder::publish(mode, LINE);
}

#[test]
fn d4365_a_first_part_past_the_ladders_line_is_refused_not_admitted() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    ladder(mcx::ladder::ON, LINE + 64 * MIB);
    let t0 = std::time::Instant::now();
    let r = load();
    ladder(mcx::ladder::OFF, 0);
    match r {
        Err(WriteError::Refused { what }) => assert!(what.contains("memory"), "a memory refusal: {what}"),
        other => panic!("the floor chunk past the ladder's line must be refused, got {:?}", other.map(|f| f.len())),
    }
    assert!(t0.elapsed() < std::time::Duration::from_secs(60), "the refusal comes after the ladder's waits, not never");
}

#[test]
fn d4365_a_load_waits_while_the_read_is_past_the_line_and_goes_on_when_it_falls() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    ladder(mcx::ladder::OFF, 0);
    let off = load().expect("ladder off");
    ladder(mcx::ladder::ON, LINE + 64 * MIB);
    let fall = std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(200));
        READ.store(100 * MIB, Ordering::SeqCst);
    });
    let waited = load();
    fall.join().expect("fall");
    let under = load();
    ladder(mcx::ladder::OFF, 0);
    assert_eq!(waited.expect("served once the read fell under the line"), off, "the wait moves no byte of any part");
    assert_eq!(under.expect("under the line"), off, "the ladder gate moves no byte of any part");
}

#[test]
fn d4365_the_refusals_sentence_is_its_spill_debt_rows_field_8() {
    let row = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR unset: run under cargo test"))
        .join("../../../../gravity/lint/SPILL-DEBT/LoadSealPastLine");
    let row = std::fs::read_to_string(&row).unwrap_or_else(|e| panic!("{}: {e}", row.display()));
    let fields: Vec<&str> = row.trim_end().split('\t').collect();
    assert_eq!(fields[0], "LoadSealPastLine");
    assert_eq!(fields.get(7).copied(), Some(pgrc_write::par::LOAD_SEAL_PAST_LINE), "the server-only line's sentence is the row's field 8");
}
