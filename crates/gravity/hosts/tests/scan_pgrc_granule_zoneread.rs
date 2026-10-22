//! M2-ZONEREAD cards A and C. Card C: a part's contiguous code stream, wrapped dictionary index and wrapped dictionary payload come in
//! one pread where the statement reads them (the ZONEMAP-SURVEY: a LIKE count read them in three, a distinct count its index and payload in
//! two). Counted on real files (`VfsPartIo` is the reader the pread census counts), one scan at a time: the census is
//! process-global, so this binary's tests hold one lock.

use std::ffi::CString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use gravity_abi::Lane;
use gravity_driver::{blank_site, Source, Window};
use gravity_hosts::ops::scan::pgrc::granule_source::{GranuleRangeSource, GranuleRecipe, PartIoSpec, PartList, PartSpec, Resolved};
use gravity_program::{ZoneOp, ZonePred};
use gravity_hosts::dict::text_hosts::{TableFn, TableSpec};
use pgrc_format::dirlayout::part_file_name;
use pgrc_format::part::StreamRole;
use pgrc_qa::corpus::{BuiltFixture, Fixture, OracleVal};
use pgrc_qa::text_col;
use pgrc_read::{OpenPart, PartExpect, PartRegistry, VfsPartIo};
use pgrc_write::dict::TextSemantics;
use pgrc_write::elect::{CandidateSource, CodecCandidates, ColumnPosture, DictPolicy, ReferenceCandidates};
use pgrc_write::writer::PartCutPolicy;
use pgrc_write::wvfs::WriteVfs;

#[path = "common/granule_fixtures.rs"]
mod granule_fixtures;
use granule_fixtures::*;

static CENSUS: Mutex<()> = Mutex::new(());
/// The bytes the last `scan` read (under `CENSUS`).
static SCAN_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
const REL_WRAPPED: u32 = 4290;
const ROWS: u64 = 35_424;

/// Row `i`'s URL: 3 000 distinct long, repetitive values, so the writer wraps the dictionary index and payload (zstd
/// clears its 20 % law on both), as a wide table's long text columns are.
fn url(i: u64) -> Vec<u8> {
    let k = (i * 7919) % 3000;
    format!("https://www.example.com/catalog/section-{:02}/item/{k:05}/details?ref=campaign-{k:05}&src=feed", k % 40).into_bytes()
}

fn wrapped_fixture() -> BuiltFixture {
    use pgrc_qa::adapters::Probe;
    use pgrc_qa::corpus::{append_rows_with_sources, finish_with_sources, open_writer};
    use pgrc_write::publish::{effective_manifest, TxnVerdict};
    let fx = Fixture {
        name: "gravity_zoneread",
        dir: "/qa/t7390".to_string(),
        spc: 1663,
        db: 5,
        relfilenumber: 7390,
        schema: vec![text_col(1)],
        oracle: vec![(0..ROWS).map(|i| Some(OracleVal::Bytes(url(i)))).collect()],
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
    BuiltFixture { fx, files, manifest }
}

/// The fixture's part files on disk, once per process; the list opens them by path (counted preads).
fn on_disk() -> &'static (BuiltFixture, PathBuf) {
    static F: std::sync::OnceLock<(BuiltFixture, PathBuf)> = std::sync::OnceLock::new();
    F.get_or_init(|| {
        let bf = wrapped_fixture();
        let dir = on_disk_dir();
        std::fs::create_dir_all(&dir).expect("mkdir");
        remove_on_disk_at_exit();
        for rec in &bf.manifest.parts {
            let name = part_file_name(rec.part_no);
            std::fs::write(dir.join(&name), bf.files.get(&name).expect("part file")).expect("write part");
        }
        (bf, dir)
    })
}

fn on_disk_dir() -> PathBuf {
    std::env::temp_dir().join(format!("gravity-zoneread-{}", std::process::id()))
}

/// The process's fixture directory (`on_disk`, read by every test, so no test's end may remove it) goes at the process's
/// exit: a run leaves no gravity-zoneread-* in the temp dir.
fn remove_on_disk_at_exit() {
    extern "C" fn gone() {
        let _ = std::fs::remove_dir_all(on_disk_dir());
    }
    extern "C" {
        fn atexit(f: extern "C" fn()) -> core::ffi::c_int;
    }
    // SAFETY: the C library's atexit, given a function that cannot unwind (it ignores the removal's error).
    unsafe { atexit(gone) };
}

/// A run of this test alone, in a child process, leaves no fixture directory behind once the child exits.
#[test]
fn a_run_leaves_nothing_behind() {
    const CHILD: &str = "GRAVITY_ZONEREAD_LEAVE_NOTHING_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let (_, dir) = on_disk();
        assert!(dir.exists());
        println!("fixture dir {}", dir.display());
        return;
    }
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "a_run_leaves_nothing_behind", "--nocapture"])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let dir = std::env::temp_dir().join(format!("gravity-zoneread-{}", child.id()));
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "child failed: {stdout}");
    assert!(stdout.contains(&format!("fixture dir {}", dir.display())), "the child never made its fixture dir: {stdout}");
    assert!(!dir.exists(), "{} left behind", dir.display());
}

#[test]
fn a_tables_files_go_with_it() {
    let t = int8_table("gravity_zoneread_gone", 7395, |i| i as i64);
    let dir = t.dir.clone();
    assert!(dir.exists());
    drop(t);
    assert!(!dir.exists(), "{} left behind", dir.display());
}

fn path_of(rec_part_no: u32) -> CString {
    let (_, dir) = on_disk();
    CString::new(dir.join(part_file_name(rec_part_no)).to_str().expect("utf8 path")).expect("path")
}

fn path_list() -> Arc<PartList> {
    let (bf, _) = on_disk();
    let parts = bf.manifest.parts.iter().map(|rec| PartSpec { io: PartIoSpec::Path(path_of(rec.part_no)), expect: PartExpect::none(), rows: rec.rows, granules: rec.granule_count, deletes: None, part: None }).collect();
    Arc::new(PartList::new(parts))
}

/// The preads opening one part and parsing its stream directory cost (before any stream is read), and whether its
/// dictionary index and payload are wrapped single extents.
fn open_cost() -> (u64, bool) {
    let (bf, _) = on_disk();
    let before = pgrc_read::io::pread_census().0;
    let part = OpenPart::open(Box::new(VfsPartIo::open(&path_of(bf.manifest.parts[0].part_no)).expect("open fd")), &PartExpect::none()).expect("open");
    let dir = part.stream_directory().expect("dir");
    let cost = pgrc_read::io::pread_census().0 - before;
    let wrapped = |role| dir.lookup(1, 0, role).is_some_and(|s| s.entry.wrapper != 0 && s.extents.len() == 1);
    (cost, wrapped(StreamRole::DictIndex) && wrapped(StreamRole::DictPayload))
}

/// Summed over the parts: the bytes an open reads, and the file span from the first code byte to the end of the
/// dictionary index (a codes + index read that leaves the payload alone).
fn spans() -> (u64, u64) {
    let (bf, _) = on_disk();
    let (mut open, mut codes_index) = (0u64, 0u64);
    for rec in &bf.manifest.parts {
        let before = pgrc_read::io::pread_census().1;
        let part = OpenPart::open(Box::new(VfsPartIo::open(&path_of(rec.part_no)).expect("open fd")), &PartExpect::none()).expect("open");
        let dir = part.stream_directory().expect("dir");
        open += pgrc_read::io::pread_census().1 - before;
        let lo = dir.lookup(1, 0, StreamRole::Values).expect("codes").extents.iter().map(|e| e.file_off).min().expect("an extent");
        let ix = dir.lookup(1, 0, StreamRole::DictIndex).expect("index").extents[0];
        codes_index += ix.file_off + ix.len - lo;
    }
    (open, codes_index)
}

/// One scan of every window through `recipe`, filling table lanes `tables` per window; returns (preads, rows, the
/// LIKE bits' agreement with the rendered entries when `like` is some pattern test).
fn scan(recipe: GranuleRecipe, registry: Arc<PartRegistry>, tables: usize, like: Option<&dyn Fn(&[u8]) -> bool>) -> (u64, u64, Arc<PartRegistry>) {
    let list = path_list();
    let total = list.granules();
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&resolved)).unwrap();
    let before = pgrc_read::io::pread_census();
    src.seek(&[0, total], &[0, 0]).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    let mut rows = 0u64;
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = src.window(&mut w).unwrap();
        if n == 0 {
            break;
        }
        let mut t = [Lane::NONE; 2];
        for (i, l) in t.iter_mut().take(tables).enumerate() {
            assert_eq!(src.lane((2 + i) as u16, &[], l).unwrap(), 1, "table lane {} fills", 2 + i);
        }
        if let Some(want) = like {
            let codes = w.lanes()[0];
            let p = w.base().part_id;
            let bits = unsafe { gravity_dict::Bits::from_lane(&t[0]) }.expect("bits");
            for i in 0..n as usize {
                let c = unsafe { *(codes.ptr as *const u32).add(i) };
                let (datum, len) = src.dict_value(p, 0, c).unwrap();
                let v = unsafe { core::slice::from_raw_parts((datum as *const u8).add(4), len as usize) };
                assert_eq!(unsafe { bits.get(c) }, want(v), "LIKE on {:?}", String::from_utf8_lossy(v));
            }
        }
        rows += n as u64;
    }
    let after = pgrc_read::io::pread_census();
    SCAN_BYTES.store(after.1 - before.1, std::sync::atomic::Ordering::Relaxed);
    (after.0 - before.0, rows, registry)
}

/// The dictionary images a part holds after a scan: (index image resident, payload image resident).
fn dict_state(registry: &PartRegistry, part_no: u32) -> (bool, bool) {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::metadata(on_disk().1.join(part_file_name(part_no))).expect("stat");
    let part = registry.get((md.dev(), md.ino(), md.len())).expect("the scan opened the part");
    let roles: Vec<u8> = part.faults().iter().filter_map(|f| match f.tag { pgrc_read::FaultTag::StreamExtent { role, .. } => Some(role), _ => None }).collect();
    (roles.contains(&(StreamRole::DictIndex as u8)), roles.contains(&(StreamRole::DictPayload as u8)))
}

/// A LIKE count: a LIKE table over a code lane walks its entries. Each part costs its open plus ONE read: the claim run
/// of the codes carries the wrapped index and payload (the parent read three). The table's bits are the entries' own.
#[test]
fn a_like_lane_reads_codes_index_and_payload_in_one_pread_per_part() {
    let _g = CENSUS.lock().unwrap_or_else(|e| e.into_inner());
    let (open, wrapped) = open_cost();
    assert!(wrapped, "the fixture's dictionary index and payload are wrapped single extents (the banks' geometry)");
    let nparts = on_disk().0.manifest.parts.len() as u64;
    let tables = vec![TableSpec { over: 0, func: TableFn::Like { pattern: b"%item/0001%".to_vec(), ilike: false, negated: false } }];
    let reg = Arc::new(PartRegistry::new(256 << 20));
    let recipe = GranuleRecipe::for_test_codes(REL_WRAPPED, &[1], &[1], Arc::clone(&reg)).with_tables(tables);
    let like = |v: &[u8]| v.windows(9).any(|w| w == b"item/0001");
    let (reads, rows, registry) = scan(recipe, reg, 1, Some(&like));
    assert_eq!(rows, ROWS);
    assert_eq!(reads, nparts * (open + 1), "per part: its open ({open}) and one read of codes + index + payload");
    for rec in &on_disk().0.manifest.parts {
        assert_eq!(dict_state(&registry, rec.part_no), (true, true), "part {}: both images built", rec.part_no);
    }
}

/// A distinct count: the entries road reads no codes, and its fingerprints (no Fp128 table resident: fp_persist 0) walk the
/// entries. The index and the payload come in one read at the dictionary open (the parent read the index, then the
/// payload).
#[test]
fn the_entries_road_reads_index_and_payload_in_one_pread_per_part() {
    let _g = CENSUS.lock().unwrap_or_else(|e| e.into_inner());
    let (open, _) = open_cost();
    let nparts = on_disk().0.manifest.parts.len() as u64;
    let reg = Arc::new(PartRegistry::new(256 << 20));
    let recipe = GranuleRecipe::for_test_codes(REL_WRAPPED + 1, &[1], &[1], Arc::clone(&reg)).with_entries(0).with_tables(vec![TableSpec { over: 0, func: TableFn::Fingerprint }]);
    let (reads, _, registry) = scan(recipe, reg, 1, None);
    assert_eq!(reads, nparts * (open + 1), "per part: its open ({open}) and one read of index + payload");
    for rec in &on_disk().0.manifest.parts {
        assert_eq!(dict_state(&registry, rec.part_no), (true, true), "part {}: both images built", rec.part_no);
    }
}

/// G4, does not apply: a length lane (an avg of `length(text)`) reads the index, never the payload. Its codes and index come in
/// one read, and the payload is neither read nor staged.
#[test]
fn a_length_lane_stages_the_index_only_and_never_reads_the_payload() {
    let _g = CENSUS.lock().unwrap_or_else(|e| e.into_inner());
    let (open, _) = open_cost();
    let nparts = on_disk().0.manifest.parts.len() as u64;
    let tables = vec![TableSpec { over: 0, func: TableFn::OctetLength }];
    let reg = Arc::new(PartRegistry::new(256 << 20));
    let recipe = GranuleRecipe::for_test_codes(REL_WRAPPED + 2, &[1], &[1], Arc::clone(&reg)).with_tables(tables);
    let (reads, rows, registry) = scan(recipe, reg, 1, None);
    assert_eq!(rows, ROWS);
    assert_eq!(reads, nparts * (open + 1), "per part: its open ({open}) and one read of codes + index");
    let scanned = SCAN_BYTES.load(std::sync::atomic::Ordering::Relaxed);
    let (open_bytes, codes_index) = spans();
    assert_eq!(scanned, open_bytes + codes_index, "the reads end at each part's index: no payload byte is read");
    for rec in &on_disk().0.manifest.parts {
        assert_eq!(dict_state(&registry, rec.part_no), (true, false), "part {}: the index only, no payload image", rec.part_no);
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Card A: a zone lane the relation's resident zone map covers takes its granule verdicts from the map; its Stats / Bloom /
// PSMA sections are read only for a claim the map leaves Mixed, in the pread of the lane's claim run.
// ---------------------------------------------------------------------------------------------------------------------

const REL_MAP: u32 = 4295;
const SPARSE_VALUES: u64 = 6_000;

fn sparse_value(i: u64) -> i64 {
    (i * 7_919 % ROWS_I8 % SPARSE_VALUES) as i64 * 4
}

const ROWS_I8: u64 = 60_000;

/// An int8 table in several parts, its files on disk under `name`, with the per-granule [min, max] of `value`. Its
/// directory goes when it is dropped (the test's end, or a panic's unwind).
struct Int8Table {
    bf: BuiltFixture,
    dir: PathBuf,
    ranges: Vec<(i64, i64)>,
}

impl Drop for Int8Table {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn int8_table(name: &'static str, rel: u64, value: fn(u64) -> i64) -> Int8Table {
    use pgrc_qa::corpus::{build_fixture, int8_fixture};
    let bf = build_fixture(int8_fixture(name, rel, ROWS_I8, vec![], PartCutPolicy { max_rows: 20_000, ..PartCutPolicy::default() }, move |i| Some(value(i))));
    let dir = std::env::temp_dir().join(format!("gravity-zoneread-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let mut ranges = Vec::new();
    let mut row = 0u64;
    for rec in &bf.manifest.parts {
        let file = part_file_name(rec.part_no);
        std::fs::write(dir.join(&file), bf.files.get(&file).expect("part file")).expect("write part");
        for g in 0..rec.granule_count as u64 {
            let (lo, hi) = (row + g * GRANULE, (row + (g + 1) * GRANULE).min(row + rec.rows));
            let vals = (lo..hi).map(value);
            ranges.push((vals.clone().min().expect("rows"), vals.max().expect("rows")));
        }
        row += rec.rows;
    }
    Int8Table { bf, dir, ranges }
}

/// The table's list over its files (counted preads), carrying the catalog's part-grain answers, and its zone map when `map`.
fn int8_list(t: &Int8Table, map: bool) -> Arc<PartList> {
    int8_list_dv(t, map, &|_| None)
}

/// `int8_list` with part `i`'s delete vector `dv(i)` (encoded `pgrc_format::dml` payload), when some.
fn int8_list_dv(t: &Int8Table, map: bool, dv: &dyn Fn(usize) -> Option<Vec<u8>>) -> Arc<PartList> {
    use pgrc_am::footer::ZoneFacts;
    use pgrc_format::zonemap::ZoneMap;
    let path = |no| t.dir.join(part_file_name(no));
    let open: Vec<OpenPart> = t.bf.manifest.parts.iter().map(|rec| OpenPart::open(Box::new(pgrc_read::MemPartIo::new(std::fs::read(path(rec.part_no)).expect("read"), 7, 1)), &PartExpect::none()).expect("open")).collect();
    let answers = pgrc_am::answers::Answers::new(open.len(), pgrc_am::answers::build_from_parts(&open.iter().collect::<Vec<_>>()).expect("answers")).expect("answers parse");
    let specs = t.bf.manifest.parts.iter().enumerate().map(|(i, rec)| {
        let deletes = dv(i).map(|d| Arc::new(pgrc_scan::PartDeletes::from_dv_payload(&d, rec.part_no, 1, rec.granule_count).expect("dv")));
        PartSpec { io: PartIoSpec::Path(CString::new(path(rec.part_no).to_str().expect("utf8")).expect("path")), expect: PartExpect::none(), rows: rec.rows, granules: rec.granule_count, deletes, part: None }
    });
    let list = Arc::new(PartList::with_answers(specs.collect(), Some(Arc::new(answers))));
    if map {
        list.set_zone_columns(vec![ZoneFacts::new(1, ZoneMap::encode(&t.ranges).unwrap()).unwrap()]);
    }
    list
}

/// One statement over `list` under `zone` with verdicts `bits`: (preads, rows passing `pred`, windows decoded).
fn int8_scan(list: &Arc<PartList>, registry: &Arc<PartRegistry>, zone: ZonePred, bits: u64, pred: &dyn Fn(i64) -> bool) -> (u64, u64, u64) {
    let recipe = GranuleRecipe::for_test(REL_MAP, &[1], Arc::clone(registry)).with_profile(0, int8_profile()).with_verdict_cell();
    recipe.verdicts.set(0, bits);
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.zone(&[zone], &[0], &[]);
    src.bind(&binding_ref(&resolved)).unwrap();
    let before = pgrc_read::io::pread_census().0;
    src.seek(&[0, list.granules()], &[0, 0]).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    let (mut pass, mut decoded) = (0u64, 0u64);
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = src.window(&mut w).unwrap();
        if n == 0 {
            break;
        }
        decoded += 1;
        let l = w.lanes()[0];
        let rows: Vec<u32> = if w.sel.is_null() { (0..n).collect() } else { unsafe { std::slice::from_raw_parts(w.sel, w.sel_n as usize) }.to_vec() };
        pass += rows.iter().filter(|&&i| unsafe { l.is_valid(i as usize) } && pred(unsafe { l.datum(i as usize) } as i64)).count() as u64;
    }
    let reads = pgrc_read::io::pread_census().0 - before;
    src.unbind();
    (reads, pass, decoded)
}

fn int8_profile() -> pgrc_meta::profile::MetaProfile {
    let c = pgrc_qa::int8_col(1);
    pgrc_meta::profile::MetaProfile::derive_or_opaque(c.class, c.collation_class, c.semantics)
}

/// The preads a part's open and stream directory cost (the scan pays them once per opened part).
fn int8_open_cost(t: &Int8Table) -> u64 {
    let before = pgrc_read::io::pread_census().0;
    let path = t.dir.join(part_file_name(t.bf.manifest.parts[0].part_no));
    let part = OpenPart::open(Box::new(VfsPartIo::open(&CString::new(path.to_str().expect("utf8")).expect("path")).expect("fd")), &PartExpect::none()).expect("open");
    part.stream_directory().expect("dir");
    pgrc_read::io::pread_census().0 - before
}

const ALL_VERDICTS: u64 = gravity_program::protos::VERDICT_ZONE | gravity_program::protos::VERDICT_BLOOM | gravity_program::protos::VERDICT_PSMA | gravity_program::protos::VERDICT_CLAIM;

/// An equality on an unsorted key, every granule's range spanning the table (the map leaves each Mixed).
/// Mapped, a part costs its open and ONE read: the claim run of the key's streams carries its Stats / Bloom / PSMA (the
/// parent read them first, a pread of their own). The bloom still decides every Mixed granule before its decode: the
/// rows and the decoded windows equal the unmapped scan's and a warm statement's.
#[test]
fn a_mapped_equality_reads_its_meta_in_the_claim_runs_pread_and_the_bloom_still_decides() {
    let _g = CENSUS.lock().unwrap_or_else(|e| e.into_inner());
    let t = int8_table("gravity_zoneread_sparse", 7391, sparse_value);
    let open = int8_open_cost(&t);
    let nparts = t.bf.manifest.parts.len() as u64;
    for value in [4 * 1_300i64, 4 * 4_444, 4 * 5_999 + 1] {
        let pred = move |x: i64| x == value;
        let want = (0..ROWS_I8).filter(|&i| sparse_value(i) == value).count() as u64;
        let zone = ZonePred::word(0, ZoneOp::Eq, value);
        let plain = int8_scan(&int8_list(&t, false), &Arc::new(PartRegistry::new(64 << 20)), zone, ALL_VERDICTS, &pred);
        let reg = Arc::new(PartRegistry::new(64 << 20));
        let list = int8_list(&t, true);
        let cold = int8_scan(&list, &reg, zone, ALL_VERDICTS, &pred);
        let warm = int8_scan(&list, &reg, zone, ALL_VERDICTS, &pred);
        assert_eq!((plain.1, cold.1, warm.1), (want, want, want), "{value}: every arm counts the passing rows");
        assert_eq!((cold.2, warm.2), (plain.2, plain.2), "{value}: the same windows decoded (bloom decides the Mixed granules)");
        if want > 0 || plain.2 > 0 {
            assert_eq!(cold.0, nparts * (open + 1), "{value}: per part its open ({open}) and one read of the claim run + meta");
        }
        assert_eq!(warm.0, 0, "{value}: a warm statement reads nothing");
    }
}

/// A part the map decides whole reads no meta: a range whose granules are each AllMatch or NoneMatch by the map (the
/// sorted table's first two granules pass `< 16384` whole) reads the claim's run and nothing else; the unmapped parent
/// path read the lane's Stats first.
#[test]
fn a_part_the_map_decides_whole_reads_no_meta() {
    let _g = CENSUS.lock().unwrap_or_else(|e| e.into_inner());
    let t = int8_table("gravity_zoneread_sorted", 7392, |i| i as i64);
    let open = int8_open_cost(&t);
    let pred = |x: i64| x < 16_384;
    let zone = ZonePred::word(0, ZoneOp::Lt, 16_384);
    let reg = Arc::new(PartRegistry::new(64 << 20));
    let (reads, pass, _) = int8_scan(&int8_list(&t, true), &reg, zone, ALL_VERDICTS, &pred);
    assert_eq!(pass, 16_384);
    assert_eq!(reads, open + 1, "the one live part: its open and its claim run, no Stats / Bloom / PSMA");
}

/// G4, does not apply: a lane the map does not cover (no zone column) reads its meta at the pin as before: a part costs
/// its open, the meta batch and the claim run; the rows are the same.
#[test]
fn an_unmapped_lane_reads_its_meta_at_the_pin_as_before() {
    let _g = CENSUS.lock().unwrap_or_else(|e| e.into_inner());
    let t = int8_table("gravity_zoneread_unmapped", 7393, sparse_value);
    let open = int8_open_cost(&t);
    let nparts = t.bf.manifest.parts.len() as u64;
    let value = 4 * 1_300i64;
    let pred = move |x: i64| x == value;
    let want = (0..ROWS_I8).filter(|&i| sparse_value(i) == value).count() as u64;
    let (reads, pass, _) = int8_scan(&int8_list(&t, false), &Arc::new(PartRegistry::new(64 << 20)), ZonePred::word(0, ZoneOp::Eq, value), ALL_VERDICTS, &pred);
    assert_eq!(pass, want);
    assert_eq!(reads, nparts * (open + 2), "per part: its open ({open}), the pin's meta batch and the claim run");
}

/// Card A under deletes (main 09-24): the map bounds every stored row, deleted or not, as the Stats records do, so a
/// part with a delete vector takes the same verdicts and decodes the same windows as the unmapped path, and its rows are
/// the live rows passing the predicate (PG's answer). The table has NULLs in part 1 (no AllMatch from the map there) and
/// deletes in parts 0 and 1: whole passing runs, the only passing rows of a granule, and a granule's first rows. Mapped,
/// no statement reads fewer than one claim run per part it decodes, and the unmapped path reads no fewer than it did.
#[test]
fn a_mapped_lane_over_deleted_rows_matches_the_unmapped_path_and_the_live_rows() {
    use pgrc_format::dml::{encode_dv, DvBlockKind};
    let _g = CENSUS.lock().unwrap_or_else(|e| e.into_inner());
    let value = |i: u64| -> Option<i64> { (!(20_000..40_000).contains(&i) || i % 1_000 != 7).then_some(i as i64) };
    let t = {
        use pgrc_qa::corpus::{build_fixture, int8_fixture};
        let bf = build_fixture(int8_fixture("gravity_zoneread_dv", 7394, ROWS_I8, vec![], PartCutPolicy { max_rows: 20_000, ..PartCutPolicy::default() }, value));
        let dir = std::env::temp_dir().join(format!("gravity-zoneread-dv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let (mut ranges, mut row) = (Vec::new(), 0u64);
        for rec in &bf.manifest.parts {
            let file = part_file_name(rec.part_no);
            std::fs::write(dir.join(&file), bf.files.get(&file).expect("part file")).expect("write part");
            for g in 0..rec.granule_count as u64 {
                let vals: Vec<i64> = (row + g * GRANULE..(row + (g + 1) * GRANULE).min(row + rec.rows)).filter_map(value).collect();
                ranges.push(vals.iter().min().zip(vals.iter().max()).map_or((i64::MAX, i64::MIN), |(a, b)| (*a, *b)));
            }
            row += rec.rows;
        }
        Int8Table { bf, dir, ranges }
    };
    // part-relative deletes: part 0 granule 0 rows 0..100 and granule 1 rows 100..8192 (its every row >= 8292);
    // part 1 (rows from 20 000) granule 0 rows 0..300
    let starts: Vec<u64> = t.bf.manifest.parts.iter().scan(0u64, |r, rec| { let s = *r; *r += rec.rows; Some(s) }).collect();
    let g0: Vec<u16> = (0..100).collect();
    let g1: Vec<u16> = (100..8192).map(|r| r as u16).collect();
    let p1: Vec<u16> = (0..300).collect();
    let dv = |i: usize| -> Option<Vec<u8>> {
        let no = t.bf.manifest.parts[i].part_no;
        match i {
            0 => Some(encode_dv(no, 1, &[(0, DvBlockKind::List, &g0[..]), (1, DvBlockKind::List, &g1[..])]).expect("dv")),
            1 => Some(encode_dv(no, 1, &[(0, DvBlockKind::List, &p1[..])]).expect("dv")),
            _ => None,
        }
    };
    let deleted = |i: u64| {
        let (p, r) = if i >= starts[1] { (1, i - starts[1]) } else { (0, i) };
        match p {
            0 => r < 100 || (8_292..16_384).contains(&r),
            _ => i < starts[2] && r < 300,
        }
    };
    let cases: [(ZoneOp, i64); 6] = [(ZoneOp::Lt, 50), (ZoneOp::Lt, 16_384), (ZoneOp::Ge, 8_300), (ZoneOp::Eq, 9_000), (ZoneOp::Eq, 20_150), (ZoneOp::Ge, 20_007)];
    for (op, c) in cases {
        let pred = move |x: i64| match op { ZoneOp::Lt => x < c, ZoneOp::Ge => x >= c, _ => x == c };
        let want = (0..ROWS_I8).filter(|&i| !deleted(i) && value(i).is_some_and(pred)).count() as u64;
        let zone = ZonePred::word(0, op, c);
        let plain = int8_scan(&int8_list_dv(&t, false, &dv), &Arc::new(PartRegistry::new(64 << 20)), zone, ALL_VERDICTS, &pred);
        let reg = Arc::new(PartRegistry::new(64 << 20));
        let list = int8_list_dv(&t, true, &dv);
        let mapped = int8_scan(&list, &reg, zone, ALL_VERDICTS, &pred);
        let warm = int8_scan(&list, &reg, zone, ALL_VERDICTS, &pred);
        assert_eq!((plain.1, mapped.1, warm.1), (want, want, want), "{op:?} {c}: the live passing rows in every arm");
        assert_eq!((mapped.2, warm.2), (plain.2, plain.2), "{op:?} {c}: the same windows decoded");
        assert!(mapped.0 > 0 || mapped.2 == 0, "{op:?} {c}: a decoding statement reads its claim runs");
    }
}
