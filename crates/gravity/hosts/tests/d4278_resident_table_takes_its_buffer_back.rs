//! D4278: a part's resident fingerprint table is built into a buffer of the build pool (text_hosts `reuse`), as a derived
//! table is. Every `FpTable`'s drop gives its buffer to the pool, but the resident builds allocated fresh and never took
//! one, so each rebuild of a dropped table (the fp cache off, or an eviction) left its old buffer in the pool for good: a
//! text-keyed statement repeated in one session kept ~34 MiB more each time, up to the pool's cap (RAM / 8). A rebuild takes its old buffer
//! back: the pool's kept words stay where the first drop left them.
#[path = "common/granule_fixtures.rs"]
mod granule_fixtures;

use std::sync::{Arc, Mutex};

use granule_fixtures::*;
use gravity_abi::Lane;
use gravity_driver::{blank_site, Source, Window};
use gravity_hosts::dict::text_hosts::{fp_cache_drop_all, fp_seed, fp_tables_resident, reuse, TableFn, TableSpec};
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
const INO: u64 = 42_780;

static SERIAL: Mutex<()> = Mutex::new(());

fn datadir_of(pid: u32) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("gravity-d4278-{pid}"))
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
        name: "gravity_d4278",
        dir: "/qa/t4278".to_string(),
        spc: 1663,
        db: 5,
        relfilenumber: 4278,
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
    let rec = &bf.manifest.parts[0];
    let bytes = bf.files.get(&part_file_name(rec.part_no)).expect("part file").clone();
    let part = OpenPart::open(Box::new(MemPartIo::new(bytes.clone(), DEV, INO)), &PartExpect::none()).expect("open");
    Arc::new(PartList::new(vec![PartSpec {
        io: PartIoSpec::Mem { bytes: Arc::from(bytes.as_slice()), dev: DEV, ino: INO },
        expect: PartExpect::none(),
        rows: ROWS,
        granules: rec.granule_count,
        deletes: None,
        part: Some(Arc::new(part)),
    }]))
}

/// One statement's pass over the part's `Fp128` lane (its resident fingerprint table: built on a miss). The rows read.
fn pass(list: &Arc<PartList>, rel: u32) -> u64 {
    let recipe = GranuleRecipe::for_test_codes(rel, &[1], &[1], Arc::new(PartRegistry::new(64 << 20))).with_tables(vec![TableSpec { over: 0, func: TableFn::Fingerprint }]);
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&resolved)).unwrap();
    src.seek(&[0, list.granules()], &[0, 0]).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    let mut rows = 0u64;
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = src.window(&mut w).unwrap();
        if n == 0 {
            break;
        }
        let mut t = Lane::NONE;
        assert_eq!(src.lane(2, &[], &mut t).unwrap(), 1, "the fingerprint lane fills");
        rows += n as u64;
    }
    rows
}

#[test]
fn a_rebuilt_resident_table_takes_back_the_buffer_its_drop_gave() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    datadir();
    let bf = fixture();
    let list = mem_list(&bf);
    fp_cache_drop_all();
    let w0 = reuse::held().1;
    let built0 = fp_tables_resident().1;
    assert_eq!(pass(&list, REL_TEXT + 70), ROWS);
    assert_eq!(fp_tables_resident().1 - built0, 1, "the part's fingerprint table built");
    // the fp cache off (or an eviction): the table goes, its buffer kept for a later build
    fp_cache_drop_all();
    let w1 = reuse::held().1;
    assert!(w1 > w0, "the dropped table's buffer kept ({w0} -> {w1} words)");
    for k in 0..3u32 {
        let built = fp_tables_resident().1;
        assert_eq!(pass(&list, REL_TEXT + 71 + k), ROWS);
        assert_eq!(fp_tables_resident().1 - built, 1, "repeat {k}: the table rebuilt");
        fp_cache_drop_all();
        assert_eq!(reuse::held().1, w1, "repeat {k}: the rebuild took the kept buffer back, so the pool keeps no more than after the first drop");
    }
}
