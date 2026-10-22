//! Dictionary predicate tables cross statements only in rows and full modes. Counters are process-wide.
#[path = "common/granule_fixtures.rs"]
mod granule_fixtures;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use granule_fixtures::*;
use gravity_abi::Lane;
use gravity_driver::{blank_site, Source, Window};
use gravity_hosts::dict::text_hosts::{cond_tables_built, fp_cache_drop_all, fp_seed, fp_tables_resident, TableFn, TableSpec};
use gravity_hosts::ops::scan::pgrc::cond_cache::reuse;
use gravity_hosts::ops::scan::pgrc::granule_source::{GranuleRangeSource, GranuleRecipe, PartIoSpec, PartList, PartSpec, Resolved};
use pgrc_format::dirlayout::part_file_name;
use pgrc_qa::corpus::{BuiltFixture, Fixture, OracleVal};
use pgrc_qa::text_col;
use pgrc_read::{MemPartIo, OpenPart, PartExpect, PartRegistry};
use pgrc_write::dict::TextSemantics;
use pgrc_write::elect::{CandidateSource, CodecCandidates, ColumnPosture, DictPolicy, ReferenceCandidates};
use pgrc_write::writer::PartCutPolicy;
use pgrc_write::wvfs::WriteVfs;
use guc_tables::ConditionCacheMode;

const NROWS: u64 = 16_384;
const WORDS: u64 = 23;
const DEV: u64 = 7;
const INO: u64 = 32_070;

static SERIAL: Mutex<()> = Mutex::new(());

fn word(i: u64) -> Vec<u8> {
    format!("w{:02}", i % WORDS).into_bytes()
}

/// One dictionary-coded text part of `NROWS` rows over the 23 words `w00` .. `w22`.
fn fixture() -> BuiltFixture {
    use pgrc_qa::adapters::Probe;
    use pgrc_qa::corpus::{append_rows_with_sources, finish_with_sources, open_writer};
    use pgrc_write::publish::{effective_manifest, TxnVerdict};
    let fx = Fixture {
        name: "gravity_cond_table_switch",
        dir: "/qa/condswitch".to_string(),
        spc: 1663,
        db: 5,
        relfilenumber: 3207,
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

/// The part in memory (no sidecar home), held open by the list for the list's life.
fn mem_list(bf: &BuiltFixture) -> Arc<PartList> {
    let rec = &bf.manifest.parts[0];
    let bytes = bf.files.get(&part_file_name(rec.part_no)).expect("part file").clone();
    let part = Some(Arc::new(OpenPart::open(Box::new(MemPartIo::new(bytes.clone(), DEV, INO)), &PartExpect::none()).expect("open")));
    Arc::new(PartList::new(vec![PartSpec { io: PartIoSpec::Mem { bytes: Arc::from(bytes.as_slice()), dev: DEV, ino: INO }, expect: PartExpect::none(), rows: NROWS, granules: rec.granule_count, deletes: None, part }]))
}

fn recipe(rel: u32, pattern: &[u8]) -> GranuleRecipe {
    let tables = vec![TableSpec { over: 0, func: TableFn::Fingerprint }, TableSpec { over: 0, func: TableFn::Like { pattern: pattern.to_vec(), ilike: false, negated: false } }];
    GranuleRecipe::for_test_codes(rel, &[1], &[1], Arc::new(PartRegistry::new(64 << 20))).with_tables(tables)
}

/// A statement whose memo holds its tables until execution ends.
fn statement(rel: u32, pattern: &[u8], list: &Arc<PartList>, mode: ConditionCacheMode) -> Resolved {
    guc_tables::backing::set_pgrust_condition_cache(mode as i32);
    let stmt = Resolved::for_recipe(&recipe(rel, pattern), list.clone());
    guc_tables::backing::set_pgrust_condition_cache(ConditionCacheMode::Full as i32);
    stmt.set_table_cap(64 << 20);
    assert_eq!(stmt.cross_statement(), mode >= ConditionCacheMode::Rows);
    stmt
}

/// One source of `stmt` over the whole list: each word with its LIKE verdict, as the LIKE lane gave it.
fn pass(stmt: &Resolved, rel: u32, pattern: &[u8], list: &Arc<PartList>) -> BTreeSet<(Vec<u8>, bool)> {
    let mut src = GranuleRangeSource::new(recipe(rel, pattern));
    src.bind(&binding_ref(stmt)).unwrap();
    src.seek(&[0, list.granules()], &[0, 0]).unwrap();
    let by_fp: std::collections::HashMap<[u64; 2], Vec<u8>> = (0..WORDS).map(|i| (gravity_dict::fp::fp128(&word(i)).expect("seeded"), word(i))).collect();
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
            seen.insert((by_fp[&unsafe { table.get(c) }].clone(), unsafe { bits.get(c) }));
        }
        rows += n as u64;
    }
    assert_eq!(rows, NROWS);
    seen
}

fn expected(prefix: &[u8]) -> BTreeSet<(Vec<u8>, bool)> {
    (0..WORDS).map(|i| (word(i), word(i).starts_with(prefix))).collect()
}

/// `(condition tables built, condition-table reuses, resident tables)` so far.
fn counts() -> (u64, u64, u64) {
    (cond_tables_built(), reuse::census()[reuse::Kind::CodeTables as usize], fp_tables_resident().2)
}

fn delta(a: (u64, u64, u64), b: (u64, u64, u64)) -> (u64, u64, i64) {
    (b.0 - a.0, b.1 - a.1, b.2 as i64 - a.2 as i64)
}

#[test]
fn the_switch_decides_whether_a_condition_table_crosses_statements() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    fp_seed();
    let bf = fixture();
    let list = mem_list(&bf);
    let rel = REL_TEXT + 80;
    let (w1, w2) = (expected(b"w1"), expected(b"w2"));
    fp_cache_drop_all();

    // 1. on: the first statement builds its LIKE table and keeps it (with the part's fingerprint table: 2 resident)
    let c0 = counts();
    let s1 = statement(rel, b"w1%", &list, ConditionCacheMode::Full);
    assert_eq!(pass(&s1, rel, b"w1%", &list), w1);
    assert_eq!(delta(c0, counts()), (1, 0, 2), "on: built once, kept");
    drop(s1);

    // 2. on: the next statement with the same pattern reads the kept table, a counted reuse
    let c1 = counts();
    let s2 = statement(rel, b"w1%", &list, ConditionCacheMode::Full);
    assert_eq!(pass(&s2, rel, b"w1%", &list), w1);
    assert_eq!(delta(c1, counts()), (0, 1, 0), "on: the earlier statement's table reused");
    drop(s2);

    for mode in [ConditionCacheMode::Granule, ConditionCacheMode::Rows] {
        let before = counts();
        let stmt = statement(rel, b"w1%", &list, mode);
        assert_eq!(pass(&stmt, rel, b"w1%", &list), w1);
        let expected = if mode == ConditionCacheMode::Granule { (1, 0, 0) } else { (0, 1, 0) };
        assert_eq!(delta(before, counts()), expected, "{mode:?}");
    }

    // 3. off: the same pattern with the table resident: built again, nothing read from or added to the process cache
    let c2 = counts();
    let s3 = statement(rel, b"w1%", &list, ConditionCacheMode::Off);
    assert_eq!(pass(&s3, rel, b"w1%", &list), w1);
    assert_eq!(delta(c2, counts()), (1, 0, 0), "off: the statement builds its own");
    // the statement's second source shares its first source's build
    let c3 = counts();
    assert_eq!(pass(&s3, rel, b"w1%", &list), w1);
    assert_eq!(delta(c3, counts()), (0, 0, 0), "off: one build per statement");
    drop(s3);

    // 4. off again: the earlier off statement left nothing behind, so this one builds too, the same work as its first run
    let c4 = counts();
    let s4 = statement(rel, b"w1%", &list, ConditionCacheMode::Off);
    assert_eq!(pass(&s4, rel, b"w1%", &list), w1);
    assert_eq!(delta(c4, counts()), (1, 0, 0), "off: a repeated statement does the same work");
    drop(s4);

    // 5. off on a cold cache: an off statement keeps nothing for an on statement after it
    fp_cache_drop_all();
    let s5 = statement(rel, b"w1%", &list, ConditionCacheMode::Off);
    assert_eq!(pass(&s5, rel, b"w1%", &list), w1);
    drop(s5);
    let c5 = counts();
    let s6 = statement(rel, b"w1%", &list, ConditionCacheMode::Full);
    assert_eq!(pass(&s6, rel, b"w1%", &list), w1);
    // the fingerprint table is the part's data (kept either way); the LIKE table is the one the on statement adds
    assert_eq!(delta(c5, counts()), (1, 0, 1), "on after off: nothing to reuse, built and kept");
    drop(s6);

    // 6. a changed pattern is a new table, on as off: never the other pattern's verdicts
    for mode in [ConditionCacheMode::Rows, ConditionCacheMode::Granule, ConditionCacheMode::Off] {
        let c6 = counts();
        let s7 = statement(rel, b"w2%", &list, mode);
        assert_eq!(pass(&s7, rel, b"w2%", &list), w2);
        assert_eq!(delta(c6, counts()), (1, 0, i64::from(mode >= ConditionCacheMode::Rows)), "a new pattern builds (mode: {mode:?})");
        drop(s7);
    }
}

/// `pgrust.dict_filter_cache = off`: under rows and full a statement builds its table, reads none another statement kept
/// and keeps none (a repeat does the same work); on again, the mode decides. The part's fingerprint table is not this
/// switch's (it stays resident either way).
#[test]
fn dict_filter_cache_off_keeps_no_condition_table_in_any_mode() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    fp_seed();
    let bf = fixture();
    let list = mem_list(&bf);
    let rel = REL_TEXT + 81;
    let w1 = expected(b"w1");
    let run = |mode: ConditionCacheMode, on: bool| {
        guc_tables::backing::set_pgrust_condition_cache(mode as i32);
        guc_tables::backing::set_pgrust_dict_filter_cache(on);
        let s = Resolved::for_recipe(&recipe(rel, b"w1%"), list.clone());
        guc_tables::backing::set_pgrust_condition_cache(ConditionCacheMode::Full as i32);
        guc_tables::backing::set_pgrust_dict_filter_cache(true);
        s.set_table_cap(64 << 20);
        let cross = s.cross_statement();
        let c = counts();
        assert_eq!(pass(&s, rel, b"w1%", &list), w1);
        let d = delta(c, counts());
        drop(s);
        (cross, d)
    };
    for mode in [ConditionCacheMode::Full, ConditionCacheMode::Rows] {
        fp_cache_drop_all();
        assert_eq!(run(mode, false), (false, (1, 0, 1)), "{mode:?} off: built, only the fingerprint table resident");
        assert_eq!(run(mode, false), (false, (1, 0, 0)), "{mode:?} off: the repeat builds again");
        assert_eq!(run(mode, true), (true, (1, 0, 1)), "{mode:?} on: built and kept");
        assert_eq!(run(mode, true), (true, (0, 1, 0)), "{mode:?} on: reused");
        assert_eq!(run(mode, false), (false, (1, 0, 0)), "{mode:?} off: the resident table is not read");
    }
    for mode in [ConditionCacheMode::Granule, ConditionCacheMode::Off] {
        assert!(!run(mode, true).0, "{mode:?}: never across statements");
    }
}
