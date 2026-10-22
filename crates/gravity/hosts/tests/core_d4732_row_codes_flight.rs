//! D4732 (a): one build of a verbatim part's ranks at a time (`text_hosts::row_codes`, single-flight). Openers of a part's
//! table while another opener builds it wait for that build and read the table it keeps (one build for every opener);
//! a waiter's statement cancel ends its wait with the build's cancel error; a build that ends with no table (an error,
//! a cancel) leaves the table to a waiter, which builds it itself. The flight is held with `hold_row_codes_flight` (a
//! build in flight's stand-in), so no case depends on timing. `ranks_resident` (the claims' tail rule's look) reads the
//! built table. Its own binary: it reads the process-wide build count.
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gravity_hosts::dict::text_hosts::{fp_flight_waits, fp_tables_resident, hold_row_codes_flight, ranks_resident, row_codes, FpTable, TableBuildError, TextScratch};
use pgrc_format::dirlayout::part_file_name;
use pgrc_qa::corpus::{open_part_bytes, BuiltFixture, Fixture, OracleVal};
use pgrc_qa::text_col;
use pgrc_read::{OpenPart, ReadError, StreamCursor};
use pgrc_write::elect::{CandidateSource, CodecCandidates, ColumnPosture, ReferenceCandidates};
use pgrc_write::writer::PartCutPolicy;
use pgrc_write::wvfs::WriteVfs;

const ROWS: u64 = 20_000;

/// A distinct 120-byte value a row: no dictionary policy, so the writer stores the column verbatim.
fn value(i: u64) -> Vec<u8> {
    format!("{:08}-{}", (i * 7919) % ROWS, "y".repeat(111)).into_bytes()
}

fn verbatim_fixture() -> BuiltFixture {
    use pgrc_qa::adapters::Probe;
    use pgrc_qa::corpus::{append_rows_with_sources, finish_with_sources, open_writer};
    use pgrc_write::publish::{effective_manifest, TxnVerdict};
    let fx = Fixture {
        name: "gravity_d4732_flight",
        dir: "/qa/t4732".to_string(),
        spc: 1663,
        db: 5,
        relfilenumber: 4732,
        schema: vec![text_col(1)],
        oracle: vec![(0..ROWS).map(|i| Some(OracleVal::Bytes(value(i)))).collect()],
        plans: vec![],
        policy: PartCutPolicy { max_rows: ROWS, ..PartCutPolicy::default() },
    };
    let cands = CodecCandidates::new(ColumnPosture::default());
    let sources: [&dyn CandidateSource; 2] = [&cands, &ReferenceCandidates];
    let mut vfs = pgrc_qa::simvfs::SimVfs::new();
    vfs.mkdir_path(&fx.dir).expect("mkdir");
    let fxid = 900 + fx.relfilenumber;
    let mut probe = Probe::new(TxnVerdict::Aborted);
    probe.mark(fxid, TxnVerdict::InProgress);
    let mut w = open_writer(&fx, fxid).expect("writer");
    append_rows_with_sources(&mut vfs, &mut w, &fx, 0, ROWS, &sources).expect("append");
    finish_with_sources(&mut vfs, &mut w, &sources).expect("finish");
    w.publish(&mut vfs, &probe).expect("publish");
    probe.mark(fxid, TxnVerdict::Committed);
    let files = vfs.snapshot_dir(&fx.dir);
    let manifest = effective_manifest(&mut vfs, &fx.dir, &probe, None).expect("effective").expect("committed generation");
    BuiltFixture { fx, files, manifest }
}

/// The fixture's first part under inode `ino` (a fresh part identity: no resident table serves it).
fn open(bf: &BuiltFixture, ino: u64) -> Arc<OpenPart> {
    let rec = &bf.manifest.parts[0];
    open_part_bytes(bf.files.get(&part_file_name(rec.part_no)).expect("part file"), ino).expect("open part")
}

/// An opener's `row_codes` of column 1 of `part`, through its own cursor.
fn build(part: &Arc<OpenPart>) -> Result<Arc<FpTable>, TableBuildError> {
    let mut cur = StreamCursor::open(Arc::clone(part), pgrc_am::scan::codec_binding(), 1, 0).expect("cursor");
    let mut scratch = TextScratch::new();
    row_codes(part, &mut cur, 1, &mut scratch, None)
}

/// The process's table builds so far.
fn built() -> u64 {
    fp_tables_resident().1
}

/// Waits until `n` openers in all have waited on a flight (bounded: 60 s).
fn waits_reach(n: u64) {
    let until = Instant::now() + Duration::from_secs(60);
    while fp_flight_waits() < n {
        assert!(Instant::now() < until, "openers never waited: {} of {n}", fp_flight_waits());
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn a_parts_ranks_build_once_for_racing_openers_a_waiter_cancels_and_a_failed_build_leaves_it_to_a_waiter() {
    let bf = verbatim_fixture();

    // racing openers: six openers of one part's table while its build is in flight wait; one builds when it ends with no
    // table, the other five read that table (one build in all, one table)
    let part = open(&bf, 4_732_000);
    assert!(!ranks_resident(&part, 1), "a fresh part has no resident ranks (the tail rule cuts no part of it)");
    let (b0, w0) = (built(), fp_flight_waits());
    let hold = hold_row_codes_flight(&part, 1).expect("the part's flight is free");
    assert!(hold_row_codes_flight(&part, 1).is_none(), "one flight a key");
    let openers: Vec<_> = (0..6)
        .map(|_| {
            let p = Arc::clone(&part);
            std::thread::spawn(move || build(&p).map_err(|e| format!("{e:?}")).expect("built"))
        })
        .collect();
    waits_reach(w0 + 6);
    assert_eq!(built(), b0, "no opener built while another's build was in flight");
    drop(hold);
    let tables: Vec<Arc<FpTable>> = openers.into_iter().map(|h| h.join().expect("opener")).collect();
    assert_eq!(built(), b0 + 1, "one build for six racing openers");
    assert!(tables.iter().all(|t| Arc::ptr_eq(t, &tables[0])), "every opener reads the one table");
    let ranks = tables[0].ranks().expect("a verbatim part's ranks");
    let granules = part.footer().granule_count;
    assert_eq!((0..granules).map(|lg| ranks.granule_codes(lg).len()).sum::<usize>() as u64, ROWS, "every row of the part coded");
    // a later opener hits the resident table: no build, no wait
    let w1 = fp_flight_waits();
    assert!(Arc::ptr_eq(&build(&part).expect("resident"), &tables[0]));
    assert_eq!((built(), fp_flight_waits()), (b0 + 1, w1));
    assert!(ranks_resident(&part, 1) && !ranks_resident(&part, 2), "the built table is resident (the tail rule may cut the part)");

    // a waiter's cancel: its wait ends with the build's cancel error and builds nothing; the flight it waited on is
    // untouched, and the part builds once it ends
    let part = open(&bf, 4_732_100);
    let (b0, w0) = (built(), fp_flight_waits());
    let hold = hold_row_codes_flight(&part, 1).expect("the part's flight is free");
    let byte = Arc::new(AtomicU8::new(0));
    let waiter = {
        let (p, byte) = (Arc::clone(&part), Arc::clone(&byte));
        std::thread::spawn(move || cancelpoll::scoped(&byte, || build(&p).map(|_| ())))
    };
    waits_reach(w0 + 1);
    byte.store(cancelpoll::ABORT_BIT, Ordering::Release);
    let r = waiter.join().expect("waiter");
    assert!(matches!(r, Err(TableBuildError::Read(ReadError::Canceled))), "a canceled wait: {r:?}");
    assert_eq!(built(), b0, "the canceled waiter built nothing");
    assert!(hold_row_codes_flight(&part, 1).is_none(), "the builder's flight outlives its waiter's cancel");
    drop(hold);
    build(&part).map_err(|e| format!("{e:?}")).expect("built after the flight ended");
    assert_eq!(built(), b0 + 1);

    // a build that ends with no table (its error): the waiter looks again, finds none, and builds the table itself
    let part = open(&bf, 4_732_200);
    let (b0, w0) = (built(), fp_flight_waits());
    let hold = hold_row_codes_flight(&part, 1).expect("the part's flight is free");
    let waiter = {
        let p = Arc::clone(&part);
        std::thread::spawn(move || build(&p).map_err(|e| format!("{e:?}")))
    };
    waits_reach(w0 + 1);
    drop(hold);
    let t = waiter.join().expect("waiter").expect("the waiter built the table itself");
    assert_eq!(built(), b0 + 1, "one build, the waiter's");
    assert!(Arc::ptr_eq(&build(&part).expect("resident"), &t));
    assert!(hold_row_codes_flight(&part, 1).is_some(), "no flight left behind");
}
