#![cfg(not(loom))]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use pgrc_format::class::{ColSchema, CollationClass, StorageClass, TypeSemantics};
use pgrc_format::relopt::ShredOptions;
use pgrc_write::elect::ReferenceCandidates;
use pgrc_write::ingest::RawDatum;
use pgrc_write::par::{
    machine_parts_in_flight, NoDetoastProvider, NoShredProvider, ParEngine, ParIngestOpts, ParProviders, RowChunk,
    SharedMemVfs, SharedMemVfsProvider, VfsProvider, LEGACY_MAX_PARTS_IN_FLIGHT, PARTS_IN_FLIGHT_ENV,
};
use pgrc_write::seal::{PartSpec, ReferenceResolver};
use pgrc_write::writer::PartCutPolicy;
use pgrc_write::wvfs::{MemVfs, WriteVfs};

const DIR: &str = "/tbl/pgrc_3343";
const FXID: u64 = 3343;
const CHUNKS: i64 = 40;

static GOVERNOR: Mutex<()> = Mutex::new(());

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

/// The VFS of a session whose first part's seal waits for the test (`release`): its seal's `make` (one per part) blocks
/// until then, so that part stays sealing while the leader publishes the next chunk.
struct HeldFirstSeal {
    inner: SharedMemVfs,
    first: AtomicBool,
    released: Arc<(Mutex<bool>, Condvar)>,
}

impl VfsProvider for HeldFirstSeal {
    fn make(&self) -> Box<dyn WriteVfs + Send> {
        if self.first.swap(false, Ordering::SeqCst) {
            let (m, cv) = &*self.released;
            let mut r = m.lock().unwrap_or_else(|e| e.into_inner());
            while !*r {
                r = cv.wait(r).unwrap_or_else(|e| e.into_inner());
            }
        }
        Box::new(self.inner.clone())
    }
}

fn release(released: &(Mutex<bool>, Condvar)) {
    *released.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
    released.1.notify_all();
}

fn engine(vfs: Arc<dyn VfsProvider>) -> Arc<ParEngine> {
    let schema = vec![int8_col(1)];
    let spec = PartSpec {
        spc: 1663,
        db: 5,
        relfilenumber: 3343,
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
            PartCutPolicy { max_rows: 128, max_bytes: u64::MAX, cut_granule_rows: 64 },
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

/// One whole session (two workers beside the leader): the part files it wrote and its memory report. `held`: the first
/// part's seal waits until the leader's next chunk has started a gate poll (or passed without one), so a chunk is
/// published while a part seals whatever the scheduling (under a loaded box the leader could otherwise publish every
/// chunk before the workers closed a part, and no chunk ever met a sealing part).
fn load(held: bool) -> (BTreeMap<String, Vec<u8>>, (u64, u64, u64)) {
    let mut v = MemVfs::new();
    v.mkdir_path(DIR).expect("mkdir");
    let shared = SharedMemVfs::new(v);
    let released = Arc::new((Mutex::new(!held), Condvar::new()));
    let vfs: Arc<dyn VfsProvider> = match held {
        true => Arc::new(HeldFirstSeal { inner: shared.clone(), first: AtomicBool::new(true), released: Arc::clone(&released) }),
        false => Arc::new(SharedMemVfsProvider(shared.clone())),
    };
    let e = engine(vfs);
    let workers: Vec<_> = (0..2)
        .map(|w| {
            let e = Arc::clone(&e);
            std::thread::spawn(move || e.run_worker(w))
        })
        .collect();
    let mut releaser = None;
    for id in 0..CHUNKS {
        if held && id == 2 {
            // chunks 0 and 1 are the first part (128 rows): wait for its close, its seal now held in `make`
            while e.peak_parts_open() == 0 {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            let (e2, r2) = (Arc::clone(&e), Arc::clone(&released));
            let passed = Arc::new(AtomicBool::new(false));
            let p2 = Arc::clone(&passed);
            releaser = Some((
                std::thread::spawn(move || {
                    while e2.memory_report().2 == 0 && !p2.load(Ordering::SeqCst) {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    release(&r2);
                }),
                passed,
            ));
        }
        e.publish_chunk(chunk(id * 64), id as u64).expect("publish");
        if let (2, Some((_, passed))) = (id, releaser.as_ref()) {
            passed.store(true, Ordering::SeqCst);
        }
    }
    e.close_input(CHUNKS as u64);
    if let Some((r, _)) = releaser {
        r.join().expect("releaser");
    }
    release(&released);
    for w in workers {
        w.join().expect("worker");
    }
    assert_eq!(e.collect().expect("collect").len(), (CHUNKS / 2) as usize);
    e.release_charge();
    let files = shared.with(|v| {
        v.list_dir(DIR).expect("list").into_iter().map(|n| (n.clone(), v.read_full(&format!("{DIR}/{n}")).expect("read"))).collect()
    });
    (files, e.memory_report())
}

#[test]
fn a_load_past_the_governors_bar_passes_one_part_at_a_time_and_writes_the_same_parts() {
    let _g = GOVERNOR.lock().unwrap_or_else(|e| e.into_inner());
    let before = mcx::global_footprint::bytes();
    mcx::governor::publish(0, 0, 0);
    let (open, (peak, waits, _)) = load(false);
    assert!(peak > 0, "the session counts the bytes it holds");
    assert_eq!(waits, 0, "no bar: no gate");
    assert_eq!(mcx::global_footprint::bytes(), before, "the charge is given back whole");

    mcx::governor::publish(1, 1 << 40, mcx::global_footprint::bytes());
    let (gated, (_, waits, _)) = load(true);
    mcx::governor::publish(0, 0, 0);
    assert!(waits > 0, "past the bar a chunk waits while a part seals");
    assert_eq!(mcx::global_footprint::bytes(), before, "the charge is given back whole");
    assert_eq!(gated, open, "the gate moves no byte of any part");
}

#[test]
fn the_open_parts_bound_reads_the_governed_limit() {
    let _g = GOVERNOR.lock().unwrap_or_else(|e| e.into_inner());
    if std::env::var(PARTS_IN_FLIGHT_ENV).is_ok() {
        return;
    }
    let policy = PartCutPolicy::default();
    mcx::governor::publish_limit(1 << 20);
    let parts = machine_parts_in_flight(&policy);
    mcx::governor::publish_limit(0);
    assert_eq!(parts, LEGACY_MAX_PARTS_IN_FLIGHT, "a limit with no room left takes the floor, whatever the host has free");
}
