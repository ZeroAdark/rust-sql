//! D1137: a part's fingerprint and LIKE tables are its own. A pgrcolumnar part's uuid is `(dev, ino, len, footer_off)`, and a
//! later part that reuses a dead part's inode at the same size repeats it (an in-place TRUNCATE restarts at part 0, a
//! ROLLBACK reuses part numbers). The process cache serves an entry only to the open part it was built for, so it never hands
//! a same-uuid part another part's tables.
#[path = "common/granule_fixtures.rs"]
mod granule_fixtures;

use std::collections::BTreeSet;
use std::ffi::CString;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::sync::{Arc, Mutex};

use granule_fixtures::*;
use gravity_abi::Lane;
use gravity_driver::{blank_site, Source, Window};
use gravity_hosts::ops::scan::pgrc::granule_source::{GranuleRangeSource, GranuleRecipe, PartIoSpec, PartList, PartSpec, Resolved};
use gravity_hosts::dict::text_hosts::{fp_seed, fp_tables_resident, TableFn, TableSpec};
use pgrc_format::dirlayout::part_file_name;
use pgrc_qa::corpus::{BuiltFixture, Fixture, OracleVal};
use pgrc_qa::text_col;
use pgrc_read::{MemPartIo, OpenPart, PartExpect, PartRegistry};
use pgrc_write::dict::TextSemantics;
use pgrc_write::elect::{CandidateSource, CodecCandidates, ColumnPosture, DictPolicy, ReferenceCandidates};
use pgrc_write::writer::PartCutPolicy;
use pgrc_write::wvfs::WriteVfs;

const ROWS: u64 = 16_384;
const WORDS: u64 = 23;
const DEV: u64 = 7;
const INO: u64 = 11_370;

static SERIAL: Mutex<()> = Mutex::new(());

fn datadir_of(pid: u32) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("gravity-fp-identity-{pid}"))
}

/// The data directory the parts' files live in: set once for the process, before the first seed draw. It goes at the
/// process's exit (every test reads it, so no test's end may remove it).
fn datadir() -> std::path::PathBuf {
    static DD: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DD.get_or_init(|| {
        let dd = datadir_of(std::process::id());
        std::fs::create_dir_all(dd.join("global")).unwrap();
        remove_datadir_at_exit();
        init_small::globals::SetDataDir(dd.to_str().unwrap());
        fp_seed();
        dd
    })
    .clone()
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
    const CHILD: &str = "GRAVITY_FP_IDENTITY_LEAVE_NOTHING_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let dd = datadir();
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

fn word(lead: u8, i: u64) -> Vec<u8> {
    format!("{}{:02}", lead as char, i % WORDS).into_bytes()
}

/// One dictionary-coded text part of `ROWS` rows over the 23 words `<lead>00` .. `<lead>22`.
fn fixture(lead: u8) -> BuiltFixture {
    use pgrc_qa::adapters::Probe;
    // The premise is two parts of the same byte length (the same uuid) from different words. The spec §8.9 Sample
    // section's zstd-wrapped body depends on the sampled values, so these fixtures are sealed without it.
    pgrc_write::seal::set_sample_section_off(true);
    use pgrc_qa::corpus::{append_rows_with_sources, finish_with_sources, open_writer};
    use pgrc_write::publish::{effective_manifest, TxnVerdict};
    let fx = Fixture {
        name: "gravity_fp_identity",
        dir: "/qa/t7311".to_string(),
        spc: 1663,
        db: 5,
        relfilenumber: 7311,
        schema: vec![text_col(1)],
        oracle: vec![(0..ROWS).map(|i| Some(OracleVal::Bytes(word(lead, i)))).collect()],
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

fn part_bytes(bf: &BuiltFixture) -> (u32, u32, Vec<u8>) {
    let rec = &bf.manifest.parts[0];
    (rec.part_no, rec.granule_count, bf.files.get(&part_file_name(rec.part_no)).expect("part file").clone())
}

fn open_mem(bf: &BuiltFixture) -> OpenPart {
    OpenPart::open(Box::new(MemPartIo::new(part_bytes(bf).2, DEV, INO)), &PartExpect::none()).expect("open")
}

/// The part in memory at `(DEV, INO)`, held open by the list (as the resident table holds a server's parts): it lives as long
/// as the list.
fn mem_list(bf: &BuiltFixture) -> Arc<PartList> {
    let (_, granules, bytes) = part_bytes(bf);
    let part = Some(Arc::new(open_mem(bf)));
    Arc::new(PartList::new(vec![PartSpec { io: PartIoSpec::Mem { bytes: Arc::from(bytes.as_slice()), dev: DEV, ino: INO }, expect: PartExpect::none(), rows: ROWS, granules, deletes: None, part }]))
}

fn path_list(bf: &BuiltFixture, path: &std::path::Path) -> Arc<PartList> {
    let (part_no, granules, _) = part_bytes(bf);
    let mut expect = PartExpect::none();
    expect.part_no = Some(part_no);
    Arc::new(PartList::new(vec![PartSpec { io: PartIoSpec::Path(CString::new(path.to_str().unwrap()).unwrap()), expect, rows: ROWS, granules, deletes: None, part: None }]))
}

/// Every word's fingerprint under the process seed.
fn fps_of(lead: u8) -> BTreeSet<[u64; 2]> {
    (0..WORDS).map(|i| gravity_dict::fp::fp128(&word(lead, i)).expect("seeded")).collect()
}

/// One pass over `list` through its own registry: the fingerprints its `Fp128` lane gave the rows and, with `like`, the verdicts
/// its `LIKE 'w%'` lane gave them (without the LIKE table the lane may open the dictionary sparse).
fn pass(list: &Arc<PartList>, rel: u32, like: bool) -> (BTreeSet<[u64; 2]>, BTreeSet<bool>) {
    let mut tables = vec![TableSpec { over: 0, func: TableFn::Fingerprint }];
    if like {
        tables.push(TableSpec { over: 0, func: TableFn::Like { pattern: b"w%".to_vec(), ilike: false, negated: false } });
    }
    let mut recipe = GranuleRecipe::for_test_codes(rel, &[1], &[1], Arc::new(PartRegistry::new(64 << 20))).with_tables(tables);
    if !like {
        recipe = recipe.with_sparse_dicts();
    }
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&resolved)).unwrap();
    src.seek(&[0, list.granules()], &[0, 0]).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    let (mut fps, mut verdicts, mut rows) = (BTreeSet::new(), BTreeSet::new(), 0u64);
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
        let table = unsafe { gravity_dict::Fp128::from_lane(&t) }.expect("fp128");
        let bits = like.then(|| {
            assert_eq!(src.lane(3, &[], &mut b).unwrap(), 1, "the LIKE lane fills");
            unsafe { gravity_dict::Bits::from_lane(&b) }.expect("bits")
        });
        for i in 0..n as usize {
            assert!(unsafe { codes.is_valid(i) }, "no NULL rows");
            let c = unsafe { *(codes.ptr as *const u32).add(i) };
            fps.insert(unsafe { table.get(c) });
            if let Some(bits) = &bits {
                verdicts.insert(unsafe { bits.get(c) });
            }
        }
        rows += n as u64;
    }
    assert_eq!(rows, ROWS);
    (fps, verdicts)
}

#[test]
fn a_part_repeating_another_parts_uuid_never_reads_its_resident_tables() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    datadir();
    let (a, b) = (fixture(b'w'), fixture(b'x'));
    let (pa, pb) = (open_mem(&a), open_mem(&b));
    assert_eq!(pa.ident(), pb.ident(), "the same (dev, ino, len, footer_off)");
    assert_eq!(pa.uuid(), pb.uuid(), "so the same uuid");
    assert_ne!(pa.footer().section_table_crc, pb.footer().section_table_crc, "different content");
    drop((pa, pb));
    let (fa, fb) = (fps_of(b'w'), fps_of(b'x'));

    // 1. A: its own tables, built
    let built0 = fp_tables_resident().1;
    let list_a = mem_list(&a);
    assert_eq!(pass(&list_a, REL_TEXT + 60, true), (fa.clone(), BTreeSet::from([true])), "A's fingerprints; LIKE 'w%' true");
    assert_eq!(fp_tables_resident().1 - built0, 2, "A's fingerprint and LIKE tables built");

    // 2. A's part dropped (its last holder gone), then B at the same identity: B's own tables, rebuilt
    drop(list_a);
    let (built1, retired1) = (fp_tables_resident().1, fp_tables_resident().4);
    let list_b = mem_list(&b);
    assert_eq!(pass(&list_b, REL_TEXT + 61, true), (fb.clone(), BTreeSet::from([false])), "B's fingerprints, not the dead A's; LIKE 'w%' false");
    assert_eq!(fp_tables_resident().1 - built1, 2, "B's tables built");
    assert!(fp_tables_resident().4 - retired1 >= 2, "A's entries dropped");

    // 3. B's part alive, A again at the same identity (a second open part): A's own tables, B's untouched by value
    let built2 = fp_tables_resident().1;
    let list_a2 = mem_list(&a);
    assert_eq!(pass(&list_a2, REL_TEXT + 62, true), (fa.clone(), BTreeSet::from([true])), "A's fingerprints, not the live B's");
    assert_eq!(fp_tables_resident().1 - built2, 2, "A's tables built again");
    drop(list_a2);
    assert_eq!(pass(&list_b, REL_TEXT + 63, true), (fb, BTreeSet::from([false])), "B's still");
}

#[test]
fn a_part_rewritten_in_place_at_the_same_size_never_reads_the_old_parts_table() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dd = datadir();
    let (a, b) = (fixture(b'w'), fixture(b'x'));
    let (part_no, _, image_a) = part_bytes(&a);
    let image_b = part_bytes(&b).2;
    assert_eq!(image_a.len(), image_b.len());
    let dir = dd.join("base/5/pgrc_7311");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(part_file_name(part_no));
    std::fs::write(&path, &image_a).unwrap();
    let (fa, fb) = (fps_of(b'w'), fps_of(b'x'));

    // 1. A: built
    let built0 = fp_tables_resident().1;
    let list = path_list(&a, &path);
    assert_eq!(pass(&list, REL_TEXT + 70, false).0, fa, "A's fingerprints");
    assert_eq!(fp_tables_resident().1 - built0, 1, "A's table built");
    drop(list);

    // 2. part 0 rewritten IN PLACE with B: the same inode and length, so the same uuid
    let ino = std::fs::metadata(&path).unwrap().ino();
    std::fs::OpenOptions::new().write(true).open(&path).unwrap().write_all(&image_b).unwrap();
    let m = std::fs::metadata(&path).unwrap();
    assert_eq!((m.ino(), m.len()), (ino, image_a.len() as u64), "rewritten in place");
    let built1 = fp_tables_resident().1;
    let list = path_list(&b, &path);
    assert_eq!(pass(&list, REL_TEXT + 71, false).0, fb, "B's fingerprints, not A's resident table");
    assert_eq!(fp_tables_resident().1 - built1, 1, "B's table built");
    drop(list);
    assert!(std::fs::read_dir(&dir).unwrap().all(|e| !e.unwrap().file_name().to_str().unwrap().contains("-fp128-")), "no table written beside the part");
    let _ = std::fs::remove_dir_all(&dir);
}
