//! D4368: under `pgrust.memory_ladder = on` the serial pgrcolumnar writer (INSERT ... SELECT, a COPY below the parallel
//! floor) tests each granule against the ladder's line with the part's seal reserve, as D4365's parallel gate: a write
//! whose read stays past the line is refused (53200 at the AM, SPILL-DEBT LoadSealPastLine), never buffered into a seal
//! the cgroup cannot hold; under the line the gate moves no byte of any part. Born red: the serial writer had no gate.
#![cfg(not(any(loom, pgrust_sim)))]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use pgrc_write::elect::CandidateSource;
use pgrc_write::ingest::RawDatum;
use pgrc_write::testkit::{int8_col, mem_with_dir, stamp, Kit, DB, DIR, RELFILENUMBER, SPC};
use pgrc_write::writer::{PartCutPolicy, SealEnv, SubxactEvidence, TableWriter};
use pgrc_write::wvfs::WriteVfs;
use pgrc_write::WriteError;

const MIB: u64 = 1 << 20;
const LINE: u64 = 1000 * MIB;
const ROWS: u64 = 600;

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

/// One serial write of `ROWS` int8 rows in parts of 256 rows (granules of 64): the first append's error, else the part
/// files written.
fn write() -> Result<BTreeMap<String, Vec<u8>>, WriteError> {
    let mut kit = Kit::new();
    let mut vfs = mem_with_dir();
    let mut w = TableWriter::open(
        DIR.to_string(),
        vec![int8_col(1)],
        SPC,
        DB,
        RELFILENUMBER,
        stamp(7, 1),
        &SubxactEvidence::default(),
        PartCutPolicy { max_rows: 256, max_bytes: u64::MAX, cut_granule_rows: 64 },
    )
    .expect("open writer");
    let sources: [&dyn CandidateSource; 1] = [&kit.cands];
    let mut env = SealEnv { vfs: &mut vfs, sources: &sources, resolver: &kit.resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
    for i in 0..ROWS {
        w.append_row(&[RawDatum::Word(i)], &mut kit.ext, &mut env)?;
    }
    w.finish(&mut env)?;
    Ok(vfs.list_dir(DIR).expect("list").into_iter().map(|n| (n.clone(), vfs.read_full(&format!("{DIR}/{n}")).expect("read"))).collect())
}

#[test]
fn d4368_a_serial_write_past_the_ladders_line_is_refused_not_buffered() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    ladder(mcx::ladder::ON, LINE + 64 * MIB);
    let r = write();
    ladder(mcx::ladder::OFF, 0);
    match r {
        Err(WriteError::Refused { what }) => assert!(what.contains("memory"), "a memory refusal: {what}"),
        other => panic!("a serial write past the ladder's line must be refused, got {:?}", other.map(|f| f.len())),
    }
}

#[test]
fn d4368_under_the_line_the_serial_gate_moves_no_byte() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    ladder(mcx::ladder::OFF, 0);
    let off = write().expect("ladder off");
    ladder(mcx::ladder::ON, 100 * MIB);
    let on = write();
    ladder(mcx::ladder::OFF, 0);
    assert_eq!(on.expect("under the line"), off, "the gate moves no byte of any part");
}
