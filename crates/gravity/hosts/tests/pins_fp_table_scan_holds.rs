//! PINS: a scan that references the resident fingerprint table counts the entry's bytes on its statement (or portal) beside
//! `held`; once the cache evicts the entry, those bytes are evicted-pinned on every holder, shared by the holders; the
//! scan's end takes everything back to 0, wherever it runs.
#[path = "common/granule_fixtures.rs"]
mod granule_fixtures;

use std::sync::{Arc, Mutex};

use granule_fixtures::*;
use gravity_abi::Lane;
use gravity_driver::{blank_site, Source, Window};
use gravity_hosts::dict::text_hosts::{fp_cache_drop_all, fp_seed, fp_tables_resident, TableFn, TableSpec};
use memledger::charge::{context, enter_portal, pins_of, portal_begin};
use memledger::{account_row, bind_backend, statement_begin, statement_end, unbind_backend, Acct};
use gravity_hosts::ops::scan::pgrc::granule_source::{GranuleRangeSource, GranuleRecipe, PartIoSpec, PartList, PartSpec, Resolved};
use pgrc_format::dirlayout::part_file_name;
use pgrc_qa::corpus::{BuiltFixture, Fixture, OracleVal};
use pgrc_qa::text_col;
use pgrc_read::{MemPartIo, OpenPart, PartExpect, PartRegistry};
use pgrc_write::dict::TextSemantics;
use pgrc_write::elect::{CandidateSource, CodecCandidates, ColumnPosture, DictPolicy, ReferenceCandidates};
use pgrc_write::writer::PartCutPolicy;
use pgrc_write::wvfs::WriteVfs;

const ROWS: u64 = 16_384;
/// Distinct words: a 16 B entry each puts the table past the pool's smallest kept buffer (DICT_POOL_MIN_WORDS, 64 KiB).
const WORDS: u64 = 6_000;
const DEV: u64 = 7;
const INO: u64 = 42_790;

static SERIAL: Mutex<()> = Mutex::new(());

fn datadir_of(pid: u32) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("gravity-pins-{pid}"))
}

/// The process's data directory, set once before the first seed draw; removed at the process's exit.
fn datadir() {
    static DD: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    DD.get_or_init(|| {
        let dd = datadir_of(std::process::id());
        std::fs::create_dir_all(dd.join("global")).unwrap();
        extern "C" fn gone() {
            let _ = std::fs::remove_dir_all(datadir_of(std::process::id()));
        }
        extern "C" {
            fn atexit(f: extern "C" fn()) -> core::ffi::c_int;
        }
        // SAFETY: the C library's atexit, given a function that cannot unwind (it ignores the removal's error).
        unsafe { atexit(gone) };
        init_small::globals::SetDataDir(dd.to_str().unwrap());
        fp_seed();
    });
}

fn word(i: u64) -> Vec<u8> {
    format!("w{:05}", i % WORDS).into_bytes()
}

/// One dictionary-coded text part of `ROWS` rows over `WORDS` words.
fn fixture() -> BuiltFixture {
    use pgrc_qa::adapters::Probe;
    use pgrc_qa::corpus::{append_rows_with_sources, finish_with_sources, open_writer};
    use pgrc_write::publish::{effective_manifest, TxnVerdict};
    let fx = Fixture {
        name: "gravity_pins",
        dir: "/qa/t4279".to_string(),
        spc: 1663,
        db: 5,
        relfilenumber: 4279,
        schema: vec![text_col(1)],
        oracle: vec![(0..ROWS).map(|i| Some(OracleVal::Bytes(word(i)))).collect()],
        plans: vec![],
        policy: PartCutPolicy { max_rows: 20_000, ..PartCutPolicy::default() },
    };
    let posture = ColumnPosture { dict: Some(DictPolicy { ndv_cap: 100_000, exec_ok: true, sem: TextSemantics::BytesOnly }), ..ColumnPosture::default() };
    let cands = CodecCandidates::new(ColumnPosture::default()).with_column(1, 0, posture);
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
    assert_eq!(manifest.parts.len(), 1, "one part");
    BuiltFixture { fx, files, manifest }
}

/// The part in memory, held open by the list for the test's length.
fn mem_list(bf: &BuiltFixture) -> Arc<PartList> {
    mem_list_at(bf, INO)
}

fn mem_list_at(bf: &BuiltFixture, ino: u64) -> Arc<PartList> {
    let rec = &bf.manifest.parts[0];
    let bytes = bf.files.get(&part_file_name(rec.part_no)).expect("part file").clone();
    let part = OpenPart::open(Box::new(MemPartIo::new(bytes.clone(), DEV, ino)), &PartExpect::none()).expect("open");
    Arc::new(PartList::new(vec![PartSpec {
        io: PartIoSpec::Mem { bytes: Arc::from(bytes.as_slice()), dev: DEV, ino },
        expect: PartExpect::none(),
        rows: ROWS,
        granules: rec.granule_count,
        deletes: None,
        part: Some(Arc::new(part)),
    }]))
}

/// An open scan over the part: its recipe and source bound and read to its first window (the table built and referenced).
struct Scan {
    src: GranuleRangeSource,
    _resolved: Box<Resolved>, // the source reads it for its life; dropped after the source
}

fn open(list: &Arc<PartList>, rel: u32) -> Scan {
    let recipe = GranuleRecipe::for_test_codes(rel, &[1], &[1], Arc::new(PartRegistry::new(64 << 20))).with_tables(vec![TableSpec { over: 0, func: TableFn::Fingerprint }]);
    let resolved = Box::new(Resolved::for_recipe(&recipe, list.clone()));
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&resolved)).unwrap();
    src.seek(&[0, list.granules()], &[0, 0]).unwrap();
    Scan { src, _resolved: resolved }
}

/// One window of the scan (all the lanes it fills).
fn step(scan: &mut Scan) -> u32 {
    let src = &mut scan.src;
    let mut lanes = [Lane::NONE; MAX_LANES];
    let mut site = blank_site(lanes.as_mut_ptr());
    let mut w = Window::over(&mut site);
    let n = src.window(&mut w).unwrap();
    if n > 0 {
        let mut t = Lane::NONE;
        assert_eq!(src.lane(2, &[], &mut t).unwrap(), 1, "the fingerprint lane fills");
    }
    n
}

#[test]
fn a_scan_holding_an_evicted_table_counts_it_pinned_and_a_cursor_keeps_its_row() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    datadir();
    let bf = fixture();
    let list = mem_list(&bf);
    fp_cache_drop_all();
    let list2 = list.clone();
    std::thread::spawn(move || {
        bind_backend(3953, 1);
        let a = Acct::backend(3953);
        let sum = || {
            let r = account_row(a);
            (r.pins_bytes, r.pins_evicted_bytes, r.pins_share_bytes)
        };
        statement_begin();
        portal_begin(5);
        // the cursor's scan: opened under its portal, its first window read (the table built, in the cache, referenced)
        let (cursor_scan, cursor) = {
            let _p = enter_portal(5);
            let mut s = open(&list2, REL_TEXT + 90);
            assert!(step(&mut s) > 0);
            (s, context().0)
        };
        let bytes = fp_tables_resident().3 as i64;
        assert!(bytes > 0, "the table is in the cache");
        assert_eq!(pins_of(cursor), (bytes, 0, 0), "the cursor's scan references the entry: pinned, not evicted");
        assert_eq!(sum(), (bytes, 0, 0));
        // the cache evicts the entry while the scan holds it: all of its bytes are evicted-pinned, the one holder's share
        assert_eq!(fp_cache_drop_all(), 1);
        assert_eq!(pins_of(cursor), (bytes, bytes, bytes));
        // between two FETCHes an unrelated statement runs on the backend: it builds its own table (the cache is empty), holds
        // it, and the cursor's row is unchanged
        statement_end();
        statement_begin();
        let other = context().0;
        let mut s2 = open(&list2, REL_TEXT + 91);
        assert!(step(&mut s2) > 0);
        assert_eq!(pins_of(cursor), (bytes, bytes, bytes), "the cursor's pins stay on the cursor's row");
        assert_eq!(pins_of(other).0, bytes, "the statement's scan holds the rebuilt table");
        assert_eq!(pins_of(other).1, 0);
        drop(s2);
        assert_eq!(pins_of(other), (0, 0, 0), "a dropped scan takes its pins off");
        statement_end();
        assert_eq!(pins_of(cursor), (bytes, bytes, bytes));
        assert_eq!(sum(), (bytes, bytes, bytes), "the backend's sum is the cursor's alone");
        // two holders of one evicted entry: its bytes shared, none counted twice
        statement_begin();
        let st3 = context().0;
        fp_cache_drop_all();
        let mut s3 = open(&list2, REL_TEXT + 92);
        assert!(step(&mut s3) > 0);
        let mut s4 = open(&list2, REL_TEXT + 92);
        assert!(step(&mut s4) > 0);
        assert_eq!(pins_of(st3).0, 2 * bytes, "each scan references it");
        assert_eq!(pins_of(st3), (2 * bytes, 0, 0));
        assert_eq!(fp_cache_drop_all(), 1, "the one shared entry evicted");
        assert_eq!(pins_of(st3), (2 * bytes, 2 * bytes, bytes), "two holders share the one entry's bytes: counted once in share");
        drop(s3);
        assert_eq!(pins_of(st3), (bytes, bytes, bytes), "the last holder has the whole");
        drop(s4);
        assert_eq!(pins_of(st3), (0, 0, 0));
        statement_end();
        drop(cursor_scan);
        assert_eq!(sum(), (0, 0, 0), "the cursor's scan ended: everything is back to 0");
        unbind_backend();
    })
    .join()
    .unwrap();
}

/// L7 (ladder): a table the cache evicted while a scan holds it is no cache byte the ladder can free. The fp tables' hooks
/// (the cache's own count; its held entries as the floor) keep it out of R2a's count, R2b's and the backstop's, the reclaim
/// never drops a held entry, and its bytes stay on the holder's pins_evicted until the scan ends.
#[test]
fn an_evicted_table_a_scan_holds_stays_out_of_every_freeable_count() {
    use gravity_driver::mem::budget::{hooks, Consumer};
    use gravity_driver::mem::reclaim::{backstop_bytes, fast_bytes, ladder_elastic, ladder_protected};
    use gravity_hosts::dict::text_hosts::{fp_cache_trim_to, fp_pinned_bytes};
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    datadir();
    gravity_hosts::memory_budget::register_all();
    let bf = fixture();
    let list = mem_list(&bf);
    fp_cache_drop_all();
    std::thread::spawn(move || {
        bind_backend(3954, 1);
        statement_begin();
        let st = context().0;
        let fp = hooks(Consumer::FpTables).expect("registered");
        let fp_free = || (fp.current)().saturating_sub((fp.floor)());
        let counts = || (ladder_elastic(), ladder_protected(), backstop_bytes(), fast_bytes());
        let mut s = open(&list, REL_TEXT + 93);
        assert!(step(&mut s) > 0);
        let bytes = fp_tables_resident().3;
        assert!(bytes > 0, "the table is in the cache");
        assert_eq!((fp_pinned_bytes(), fp_free()), (bytes, 0), "held by the scan: the fp tables' floor, nothing freeable");
        assert_eq!(fp_cache_trim_to(0), 0, "the reclaim never drops a held entry");
        let held = counts();
        assert_eq!(fp_cache_drop_all(), 1, "the cache evicts it while the scan holds it");
        assert_eq!((fp_tables_resident().3, fp_free()), (0, 0), "out of the cache's count");
        assert_eq!(pins_of(st), (bytes as i64, bytes as i64, bytes as i64), "its bytes are the holder's, evicted-pinned");
        assert_eq!(counts(), held, "R2a's, R2b's and the backstop's counts gain none of them");
        drop(s);
        assert_eq!(pins_of(st), (0, 0, 0), "the scan's end lets them go");
        statement_end();
        unbind_backend();
    })
    .join()
    .unwrap();
}

/// L7, the cap path: under the ladder's fp budget an insert past it evicts the least recently used table. One a scan holds
/// frees nothing (its bytes go to the holder's pins_evicted) yet counts as cap-evicted bytes, the ladder's raise demand.
/// With cache_fp_cap_pins the cap passes over it: it stays in the count and its floor, and the new table is kept past the
/// budget, counted. Either way droppable is what a trim to 0 frees.
#[test]
fn the_fp_cap_passes_over_a_table_a_scan_holds_with_cache_fp_cap_pins() {
    use gravity_driver::mem::budget::{hooks, Consumer};
    use gravity_hosts::dict::text_hosts::{fp_cache_trim_to, fp_cap_evicted_bytes, fp_over_cap_pinned, fp_pinned_bytes};
    use mempolicy::consts::{set, K};
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            mcx::ladder::publish(mcx::ladder::OFF, 0);
            set(K::CacheFpCapPins, 0);
            set(K::CacheFloor, 0);
        }
    }
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _knobs = pgsync::lock(&mempolicy::consts::TEST_LOCK);
    let _restore = Restore;
    datadir();
    gravity_hosts::memory_budget::register_all();
    let bf = fixture();
    let (one, two) = (mem_list_at(&bf, INO), mem_list_at(&bf, INO + 1));
    set(K::CacheFloor, 1);
    mcx::ladder::publish(mcx::ladder::ON, 1 << 40);
    assert!(mcx::ladder::floors_on());
    for skip in [0u64, 1] {
        set(K::CacheFpCapPins, skip);
        fp_cache_drop_all();
        let (one, two) = (one.clone(), two.clone());
        std::thread::spawn(move || {
            bind_backend(3955 + skip as u32, 1);
            statement_begin();
            let st = context().0;
            let mut s = open(&one, REL_TEXT + 94);
            assert!(step(&mut s) > 0);
            let bytes = fp_tables_resident().3;
            assert!(bytes > 0, "the table is in the cache");
            mcx::ladder::note_taken(mcx::ladder::SIZED_FP, bytes);
            let fp = hooks(Consumer::FpTables).expect("registered");
            let droppable = || (fp.current)().saturating_sub((fp.floor)());
            let (ev, over) = (fp_cap_evicted_bytes(), fp_over_cap_pinned());
            let mut s2 = open(&two, REL_TEXT + 95);
            assert!(step(&mut s2) > 0);
            let total = pins_of(st).0 as u64;
            assert!(total > bytes, "both scans hold a table");
            let now = (fp_tables_resident().3, pins_of(st).1 as u64, fp_cap_evicted_bytes() - ev, fp_pinned_bytes());
            let past = (fp_over_cap_pinned().0 - over.0, fp_over_cap_pinned().1 - over.1);
            if skip == 0 {
                assert_eq!(now, (total - bytes, bytes, bytes, total - bytes), "today: the held table is evicted, its bytes stay");
                assert_eq!(past, (0, 0));
            } else {
                assert_eq!(now, (total, 0, 0, total), "the cap passes over the held table: none evicted, both in the floor");
                assert_eq!(past, (1, total - bytes), "the new table kept past the budget, counted");
            }
            assert_eq!(droppable(), 0, "both held: nothing droppable");
            drop(s);
            let d = droppable();
            assert_eq!(d, skip * bytes, "the first table: gone with its scan today, else in the cache unheld");
            assert_eq!(fp_cache_trim_to(0), d, "droppable is what a trim to 0 frees");
            drop(s2);
            assert_eq!(pins_of(st), (0, 0, 0));
            statement_end();
            unbind_backend();
        })
        .join()
        .unwrap();
    }
}
