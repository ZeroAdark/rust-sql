#![cfg(not(loom))]
//! D4399: a feed leader's step at the engine's gate for bytes it plans to hold outside the engine (a parallel text
//! COPY's parse window): never blocking, it fits under the ladder's line, waits past it (one climb, R5 polls counted one
//! per poll period, an earlier ask uncounted), goes on once the read falls, refuses with the existing LoadSealPastLine
//! after its waits, and a cancel during the wait ends it with the governor's refusal at once.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pgrc_format::class::{ColSchema, CollationClass, StorageClass, TypeSemantics};
use pgrc_format::relopt::ShredOptions;
use pgrc_write::elect::ReferenceCandidates;
use pgrc_write::par::{
    AheadGate, AheadStep, NoDetoastProvider, NoShredProvider, ParEngine, ParIngestOpts, ParProviders, SharedMemVfs,
    SharedMemVfsProvider, LOAD_PAST_LINE, MEMORY_REFUSED,
};
use pgrc_write::seal::{PartSpec, ReferenceResolver};
use pgrc_write::writer::PartCutPolicy;
use pgrc_write::wvfs::{MemVfs, WriteVfs};
use pgrc_write::WriteError;

const DIR: &str = "/tbl/pgrc_4399";
const MIB: u64 = 1 << 20;
const LINE: u64 = 1000 * MIB;

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

fn engine() -> Arc<ParEngine> {
    let mut v = MemVfs::new();
    v.mkdir_path(DIR).expect("mkdir");
    let schema = vec![ColSchema {
        attno: 1,
        class: StorageClass::ByvalWord { width: 8, signed: true },
        typlen: 8,
        typbyval: true,
        typalign: b'd',
        collation_class: CollationClass::C,
        semantics: TypeSemantics::SignedInt,
    }];
    let spec = PartSpec {
        spc: 1663,
        db: 5,
        relfilenumber: 4399,
        schema_fingerprint: pgrc_format::ident::schema_fingerprint(&schema),
    };
    Arc::new(
        ParEngine::new(
            ParProviders {
                vfs: Arc::new(SharedMemVfsProvider(SharedMemVfs::new(v))),
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
            4399,
            PartCutPolicy { max_rows: 256, max_bytes: u64::MAX, cut_granule_rows: 64 },
            ParIngestOpts { chunk_rows: 64, max_chunks_in_flight: 64, max_parts_in_flight: 64 },
            0,
        )
        .expect("engine"),
    )
}

/// The engine reads the ladder's mode when it is made: publish first.
fn ladder(mode: u8, read: u64) {
    mcx::ladder::install(&ACTS);
    READ.store(read, Ordering::SeqCst);
    mcx::ladder::publish(mode, LINE);
}

fn refused(s: &AheadStep) -> Option<&'static str> {
    match s {
        AheadStep::Refused(WriteError::Refused { what }) => Some(what),
        _ => None,
    }
}

#[test]
fn d4399_under_the_line_or_with_the_ladder_off_the_window_fits_at_once() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    ladder(mcx::ladder::OFF, LINE + 64 * MIB);
    let e = engine();
    let mut g = AheadGate::default();
    assert!(matches!(e.ahead_step(&mut g, 64 * MIB), AheadStep::Fits), "ladder off, no bar: fits");
    ladder(mcx::ladder::ON, 100 * MIB);
    let e = engine();
    let mut g = AheadGate::default();
    assert!(matches!(e.ahead_step(&mut g, 64 * MIB), AheadStep::Fits), "under the line: fits");
    e.set_ahead(900 * MIB);
    assert!(
        matches!(e.ahead_step(&mut g, 64 * MIB), AheadStep::Wait),
        "what is planned but not resident counts in the ask"
    );
    e.set_ahead(0);
    ladder(mcx::ladder::OFF, 0);
}

#[test]
fn d4399_past_the_line_the_leader_waits_and_goes_on_when_the_read_falls() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    ladder(mcx::ladder::ON, LINE + 64 * MIB);
    let e = engine();
    let mut g = AheadGate::default();
    assert!(matches!(e.ahead_step(&mut g, 64 * MIB), AheadStep::Wait), "past the line: the climb, then the wait");
    assert!(matches!(e.ahead_step(&mut g, 64 * MIB), AheadStep::Wait), "an ask inside the poll period waits");
    std::thread::sleep(AheadGate::POLL);
    assert!(matches!(e.ahead_step(&mut g, 64 * MIB), AheadStep::Wait), "still past the line: waits");
    READ.store(100 * MIB, Ordering::SeqCst);
    std::thread::sleep(AheadGate::POLL);
    assert!(matches!(e.ahead_step(&mut g, 64 * MIB), AheadStep::Fits), "the read fell under the line: fits");
    assert!(matches!(e.ahead_step(&mut g, 64 * MIB), AheadStep::Fits), "a pass ends the wait: the next ask is fresh");
    ladder(mcx::ladder::OFF, 0);
}

#[test]
fn d4399_a_leader_that_stays_past_the_line_is_refused_with_load_seal_past_line() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    ladder(mcx::ladder::ON, LINE + 64 * MIB);
    let e = engine();
    let mut g = AheadGate::default();
    let t0 = Instant::now();
    let out = loop {
        let s = e.ahead_step(&mut g, 64 * MIB);
        if !matches!(s, AheadStep::Wait) {
            break s;
        }
        assert!(t0.elapsed() < Duration::from_secs(60), "the refusal comes after the ladder's waits, not never");
        std::thread::sleep(AheadGate::POLL);
    };
    ladder(mcx::ladder::OFF, 0);
    assert_eq!(refused(&out), Some(LOAD_PAST_LINE), "past the line after its waits: LoadSealPastLine");
    assert!(t0.elapsed() >= mempolicy::admit::bytes_wait(), "not before one spent wait: the polls are timed");
}

#[test]
fn d4399_a_cancel_during_the_wait_refuses_at_once() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    ladder(mcx::ladder::ON, LINE + 64 * MIB);
    let e = engine();
    let mut g = AheadGate::default();
    assert!(matches!(e.ahead_step(&mut g, 64 * MIB), AheadStep::Wait), "past the line: waiting");
    // the governor's refusal of the running statement (the watchdog's or R6's cancel lands as it)
    mcx::governor::arm(true);
    mcx::governor::publish(1, 1 << 40, mcx::global_footprint::bytes());
    let s = e.ahead_step(&mut g, 64 * MIB);
    mcx::governor::publish(0, 0, 0);
    mcx::governor::arm(false);
    ladder(mcx::ladder::OFF, 0);
    assert_eq!(refused(&s), Some(MEMORY_REFUSED), "a cancel during the wait, inside the poll period too");
    drop(g);
}
