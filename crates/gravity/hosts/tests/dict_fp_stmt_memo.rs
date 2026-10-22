//! D3206: a part's fingerprint-cache tables (its `Fp128` table, a LIKE table) are built once per statement, not once per
//! source: on a process-cache miss (the cache off, a cold part, an evicted table) the first of the statement's sources builds
//! a table under the statement's memo (`Resolved::fp_memo`) and the others read that build. The memo is the statement's own:
//! a new statement builds again, and a memo capped at 0 bytes keeps a table only while a source pins it.
#[path = "common/granule_fixtures.rs"]
mod granule_fixtures;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use granule_fixtures::*;
use gravity_abi::Lane;
use gravity_driver::{blank_site, Source, Window};
use gravity_hosts::dict::text_hosts::{fp_cache_drop_all, fp_memo_shared, fp_seed, fp_tables_resident, TableFn, TableSpec};
use gravity_hosts::ops::scan::pgrc::granule_source::{GranuleRangeSource, GranuleRecipe, PartIoSpec, PartList, PartSpec, Resolved};
use pgrc_format::dirlayout::part_file_name;
use pgrc_qa::corpus::{BuiltFixture, Fixture, OracleVal};
use pgrc_qa::text_col;
use pgrc_read::{MemPartIo, OpenPart, PartExpect, PartRegistry};
use pgrc_write::dict::TextSemantics;
use pgrc_write::elect::{CandidateSource, CodecCandidates, ColumnPosture, DictPolicy, ReferenceCandidates};
use pgrc_write::writer::PartCutPolicy;
use pgrc_write::wvfs::WriteVfs;

const NROWS: u64 = 16_384;
const WORDS: u64 = 23;
const DEV: u64 = 7;
const INO: u64 = 32_060;

/// The resident cache and the census are process-wide: the tests take turns.
static SERIAL: Mutex<()> = Mutex::new(());

fn datadir_of(pid: u32) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("gravity-fp-memo-{pid}"))
}

/// The process's data directory: set once, before the first seed draw, and gone at the process's exit
/// (every test reads it, so no test's end may remove it).
fn datadir() {
    static DD: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    DD.get_or_init(|| {
        let dd = datadir_of(std::process::id());
        std::fs::create_dir_all(dd.join("global")).unwrap();
        remove_datadir_at_exit();
        init_small::globals::SetDataDir(dd.to_str().unwrap());
        fp_seed();
    });
}

fn remove_datadir_at_exit() {
    extern "C" fn gone() {
        let _ = std::fs::remove_dir_all(datadir_of(std::process::id()));
    }
    extern "C" {
        fn atexit(f: extern "C" fn()) -> core::ffi::c_int;
    }
    // SAFETY: the C library's atexit, given a function that cannot unwind (it ignores the removal's error).
    unsafe { atexit(gone) };
}

/// A run of this test alone, in a child process, leaves no data directory behind once the child exits.
#[test]
fn a_run_leaves_nothing_behind() {
    const CHILD: &str = "GRAVITY_FP_MEMO_LEAVE_NOTHING_CHILD";
    if std::env::var_os(CHILD).is_some() {
        datadir();
        let dd = datadir_of(std::process::id());
        assert!(dd.exists());
        println!("data dir {}", dd.display());
        return;
    }
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "a_run_leaves_nothing_behind", "--nocapture"])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let dd = datadir_of(child.id());
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "child failed: {stdout}");
    assert!(stdout.contains(&format!("data dir {}", dd.display())), "the child never made its data dir: {stdout}");
    assert!(!dd.exists(), "{} left behind", dd.display());
}

fn word(i: u64) -> Vec<u8> {
    format!("w{:02}", i % WORDS).into_bytes()
}

/// One dictionary-coded text part of `NROWS` rows over the 23 words `w00` .. `w22`.
fn fixture() -> BuiltFixture {
    use pgrc_qa::adapters::Probe;
    use pgrc_qa::corpus::{append_rows_with_sources, finish_with_sources, open_writer};
    use pgrc_write::publish::{effective_manifest, TxnVerdict};
    let fx = Fixture {
        name: "gravity_fp_memo",
        dir: "/qa/t3206".to_string(),
        spc: 1663,
        db: 5,
        relfilenumber: 3206,
        schema: vec![text_col(1)],
        oracle: vec![(0..NROWS).map(|i| Some(OracleVal::Bytes(word(i)))).collect()],
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
    append_rows_with_sources(&mut vfs, &mut w, &fx, 0, NROWS, &sources).expect("append");
    finish_with_sources(&mut vfs, &mut w, &sources).expect("finish");
    w.publish(&mut vfs, &probe).expect("publish");
    probe.mark(fxid, TxnVerdict::Committed);
    let files = vfs.snapshot_dir(&fx.dir);
    let manifest = effective_manifest(&mut vfs, &fx.dir, &probe, None).expect("effective").expect("committed generation");
    assert_eq!(manifest.parts.len(), 1, "one part");
    BuiltFixture { fx, files, manifest }
}

/// The part in memory, held open by the list for the list's life.
fn mem_list(bf: &BuiltFixture) -> Arc<PartList> {
    let rec = &bf.manifest.parts[0];
    let bytes = bf.files.get(&part_file_name(rec.part_no)).expect("part file").clone();
    let part = Some(Arc::new(OpenPart::open(Box::new(MemPartIo::new(bytes.clone(), DEV, INO)), &PartExpect::none()).expect("open")));
    Arc::new(PartList::new(vec![PartSpec { io: PartIoSpec::Mem { bytes: Arc::from(bytes.as_slice()), dev: DEV, ino: INO }, expect: PartExpect::none(), rows: NROWS, granules: rec.granule_count, deletes: None, part }]))
}

fn recipe(rel: u32) -> GranuleRecipe {
    let tables = vec![TableSpec { over: 0, func: TableFn::Fingerprint }, TableSpec { over: 0, func: TableFn::Like { pattern: b"w1%".to_vec(), ilike: false, negated: false } }];
    GranuleRecipe::for_test_codes(rel, &[1], &[1], Arc::new(PartRegistry::new(64 << 20))).with_tables(tables)
}

/// One source of the statement `resolved` over the whole list: the fingerprints and LIKE 'w1%' verdicts its lanes gave the
/// rows (by word), so every source is seen to read the same tables.
fn pass(resolved: &Resolved, rel: u32, list: &Arc<PartList>) -> BTreeSet<([u64; 2], bool)> {
    let mut src = GranuleRangeSource::new(recipe(rel));
    src.bind(&binding_ref(resolved)).unwrap();
    src.seek(&[0, list.granules()], &[0, 0]).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    let (mut seen, mut rows) = (BTreeSet::new(), 0u64);
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = src.window(&mut w).unwrap();
        if n == 0 {
            break;
        }
        let codes = w.lanes()[0];
        let (mut t, mut b) = (Lane::NONE, Lane::NONE);
        assert_eq!(src.lane(2, &[], &mut t).unwrap(), 1, "the fingerprint lane fills");
        assert_eq!(src.lane(3, &[], &mut b).unwrap(), 1, "the LIKE lane fills");
        let table = unsafe { gravity_dict::Fp128::from_lane(&t) }.expect("fp128");
        let bits = unsafe { gravity_dict::Bits::from_lane(&b) }.expect("bits");
        for i in 0..n as usize {
            let c = unsafe { *(codes.ptr as *const u32).add(i) };
            seen.insert((unsafe { table.get(c) }, unsafe { bits.get(c) }));
        }
        rows += n as u64;
    }
    assert_eq!(rows, NROWS);
    seen
}

fn expected() -> BTreeSet<([u64; 2], bool)> {
    (0..WORDS).map(|i| (gravity_dict::fp::fp128(&word(i)).expect("seeded"), word(i).starts_with(b"w1"))).collect()
}

fn built() -> u64 {
    fp_tables_resident().1
}

#[test]
fn a_statements_sources_build_each_table_once_and_a_new_statement_builds_again() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    datadir();
    let bf = fixture();
    let list = mem_list(&bf);
    let rel = REL_TEXT + 70;
    let want = expected();

    // 1. one statement, its first source: the fingerprint and LIKE tables built
    fp_cache_drop_all();
    let stmt = Resolved::for_recipe(&recipe(rel), list.clone());
    stmt.set_table_cap(64 << 20);
    let b0 = built();
    assert_eq!(pass(&stmt, rel, &list), want);
    assert_eq!(built() - b0, 2, "the first source builds both tables");

    // 2. the process cache emptied (the cache off, or evicted): the statement's next source reads the first one's builds
    fp_cache_drop_all();
    let (b1, s1) = (built(), fp_memo_shared());
    assert_eq!(pass(&stmt, rel, &list), want);
    assert_eq!(built() - b1, 0, "no table built again in the statement");
    assert_eq!(fp_memo_shared() - s1, 2, "both tables shared");

    // 3. two sources of the statement at once, the cache emptied: still no build
    fp_cache_drop_all();
    let b2 = built();
    std::thread::scope(|s| {
        let a = s.spawn(|| pass(&stmt, rel, &list));
        let b = s.spawn(|| pass(&stmt, rel, &list));
        assert_eq!(a.join().unwrap(), want);
        assert_eq!(b.join().unwrap(), want);
    });
    assert_eq!(built() - b2, 0, "the held tables serve both");
    drop(stmt);

    // 4. a new statement: its own memo, nothing kept across statements: built again
    fp_cache_drop_all();
    let stmt2 = Resolved::for_recipe(&recipe(rel), list.clone());
    stmt2.set_table_cap(64 << 20);
    let b3 = built();
    assert_eq!(pass(&stmt2, rel, &list), want);
    assert_eq!(built() - b3, 2, "a new statement builds its tables");
    drop(stmt2);

    // 5. two sources of a new statement at once, the cache emptied before: each table built once, by one of them
    fp_cache_drop_all();
    let stmt3 = Resolved::for_recipe(&recipe(rel), list.clone());
    stmt3.set_table_cap(64 << 20);
    let b4 = built();
    std::thread::scope(|s| {
        let a = s.spawn(|| pass(&stmt3, rel, &list));
        let b = s.spawn(|| pass(&stmt3, rel, &list));
        assert_eq!(a.join().unwrap(), want);
        assert_eq!(b.join().unwrap(), want);
    });
    assert_eq!(built() - b4, 2, "each table built once, by one of the two");
}

#[test]
fn a_memo_capped_at_zero_keeps_a_table_only_while_a_source_pins_it() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    datadir();
    let bf = fixture();
    let list = mem_list(&bf);
    let rel = REL_TEXT + 71;
    let want = expected();
    fp_cache_drop_all();
    let stmt = Resolved::for_recipe(&recipe(rel), list.clone());
    stmt.set_table_cap(0);
    let b0 = built();
    assert_eq!(pass(&stmt, rel, &list), want);
    assert_eq!(built() - b0, 2);
    assert_eq!(stmt.fp_memo().held(), (0, 0), "nothing held past a zero cap");
    // the first source is gone (its pins with it), the process cache emptied: the next source builds
    fp_cache_drop_all();
    let b1 = built();
    assert_eq!(pass(&stmt, rel, &list), want);
    assert_eq!(built() - b1, 2, "an unpinned table past the cap is built again");
}
