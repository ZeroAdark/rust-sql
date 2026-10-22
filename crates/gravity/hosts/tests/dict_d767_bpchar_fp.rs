//! D767 (2), V4R-266: a bpchar code lane's `Fp128` table (`TableFn::FingerprintBpchar`, `TableDecl::fingerprint_bpchar`)
//! fingerprints each entry without its trailing blanks (`bcTruelen`: hashbpchar's image), so 'a' and 'a  ' are one image
//! and equal a heap bpchar key's; the padded table (`Fingerprint`) is unchanged for every other consumer; the trimmed table
//! has its own cache entry, and a padded table is never served as a trimmed one.
#[path = "common/granule_fixtures.rs"]
mod granule_fixtures;

use std::collections::BTreeMap;
use std::ffi::CString;
use std::sync::Arc;

use granule_fixtures::*;
use gravity_abi::Lane;
use gravity_driver::{blank_site, Source, Window};
use gravity_hosts::ops::scan::pgrc::granule_source::{GranuleRangeSource, GranuleRecipe, PartIoSpec, PartList, PartSpec, Resolved};
use gravity_hosts::dict::text_hosts::{fp_cache_drop_all, fp_seed, fp_tables_resident, spec_of, TableFn, TableSpec};
use gravity_program::{Arena, DictRef, Rep, SiteDesc, TableDecl};
use pgrc_format::dirlayout::part_file_name;
use pgrc_qa::corpus::{BuiltFixture, Fixture, OracleVal};
use pgrc_qa::text_col;
use pgrc_read::{OpenPart, PartExpect, PartRegistry, VfsPartIo};
use pgrc_write::dict::TextSemantics;
use pgrc_write::elect::{CandidateSource, CodecCandidates, ColumnPosture, DictPolicy, ReferenceCandidates};
use pgrc_write::writer::PartCutPolicy;
use pgrc_write::wvfs::WriteVfs;

const REL: u32 = 4290;
/// The values, padded as a bpchar with typmod -1 stores them: each trimmed class has several renders.
const VALUES: [&[u8]; 10] = [b"a", b"a  ", b"a ", b"b", b"b    ", b"", b"   ", b"c c", b"c c  ", b"d"];

fn bv(i: u64) -> Vec<u8> {
    VALUES[(i % VALUES.len() as u64) as usize].to_vec()
}

fn trimmed(v: &[u8]) -> &[u8] {
    &v[..v.iter().rposition(|&c| c != b' ').map_or(0, |i| i + 1)]
}

/// A dictionary-coded column of `bv` over two parts (text_fixture's election, these values).
fn bpchar_fixture() -> BuiltFixture {
    use pgrc_qa::adapters::Probe;
    use pgrc_qa::corpus::{append_rows_with_sources, finish_with_sources, open_writer};
    use pgrc_write::publish::{effective_manifest, TxnVerdict};
    let rows = 30_000u64;
    let fx = Fixture {
        name: "gravity_bpchar",
        dir: "/qa/t7309".to_string(),
        spc: 1663,
        db: 5,
        relfilenumber: 7309,
        schema: vec![text_col(1)],
        oracle: vec![(0..rows).map(|i| Some(OracleVal::Bytes(bv(i)))).collect()],
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
    append_rows_with_sources(&mut vfs, &mut w, &fx, 0, rows, &sources).expect("append");
    finish_with_sources(&mut vfs, &mut w, &sources).expect("finish");
    w.publish(&mut vfs, &probe).expect("publish");
    probe.mark(fxid, TxnVerdict::Committed);
    let files = vfs.snapshot_dir(&fx.dir);
    let manifest = effective_manifest(&mut vfs, &fx.dir, &probe, None).expect("effective").expect("committed generation");
    BuiltFixture { fx, files, manifest }
}

/// The fixture's parts as files under `dir`, each opened now (the list holds it open, as the resident table holds a server's).
fn file_parts(bf: &BuiltFixture, dir: &std::path::Path) -> Arc<PartList> {
    std::fs::create_dir_all(dir).unwrap();
    let parts = bf
        .manifest
        .parts
        .iter()
        .map(|rec| {
            let name = part_file_name(rec.part_no);
            let path = dir.join(&name);
            std::fs::write(&path, bf.files.get(&name).expect("part file")).unwrap();
            let path = CString::new(path.to_str().unwrap()).unwrap();
            let mut expect = PartExpect::none();
            expect.part_no = Some(rec.part_no);
            let part = OpenPart::open(Box::new(VfsPartIo::open(&path).expect("open fd")), &expect).expect("open");
            PartSpec { io: PartIoSpec::Path(path), expect, rows: rec.rows, granules: rec.granule_count, deletes: None, part: Some(Arc::new(part)) }
        })
        .collect();
    Arc::new(PartList::new(parts))
}

/// The test's scratch directory.
fn scratch() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("gravity-d767-{}", std::process::id()))
}

/// One pass over every window with table `func` over lane 0: `(part, code) -> fingerprint`, and the entries' values when
/// `render` (a dense first pass).
fn pass(list: &Arc<PartList>, rel: u32, func: TableFn, render: bool) -> (BTreeMap<(u32, u32), [u64; 2]>, BTreeMap<(u32, u32), Vec<u8>>) {
    let recipe = GranuleRecipe::for_test_codes(rel, &[1], &[1], Arc::new(PartRegistry::new(64 << 20))).with_tables(vec![TableSpec { over: 0, func }]).with_sparse_dicts();
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&resolved)).unwrap();
    src.seek(&[0, list.granules()], &[0, 0]).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    let (mut fps, mut vals) = (BTreeMap::new(), BTreeMap::new());
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = src.window(&mut w).unwrap();
        if n == 0 {
            break;
        }
        let codes = w.lanes()[0];
        let p = w.base().part_id;
        let mut t = Lane::NONE;
        assert_eq!(src.lane(2, &[], &mut t).unwrap(), 1, "the fingerprint lane fills");
        let table = unsafe { gravity_dict::Fp128::from_lane(&t) }.expect("fp128");
        for i in 0..n as usize {
            if !unsafe { codes.is_valid(i) } {
                continue;
            }
            let c = unsafe { *(codes.ptr as *const u32).add(i) };
            fps.insert((p, c), unsafe { table.get(c) });
            if render && !vals.contains_key(&(p, c)) {
                let (datum, len) = src.dict_value(p, 0, c).unwrap();
                vals.insert((p, c), unsafe { core::slice::from_raw_parts((datum as *const u8).add(4), len as usize) }.to_vec());
            }
        }
    }
    (fps, vals)
}

/// One test (one process state: the fingerprint cache is process-wide, and a part's cache key is its uuid).
#[test]
fn a_bpchar_table_is_hashbpchars_image_with_its_own_cache_entry_and_the_padded_table_is_unchanged() {
    fp_seed();
    let bf = bpchar_fixture();
    let dd = scratch();
    let dir = dd.join("base/5/pgrc_7309");
    let list = file_parts(&bf, &dir);
    let nparts = list.len() as u64;
    assert!(nparts >= 2, "the tables cross parts");

    // 1. the padded tables (every consumer but a bpchar key's): the renders' bytes
    let (pad1, vals) = pass(&list, REL, TableFn::Fingerprint, true);
    assert_eq!(vals.values().collect::<std::collections::BTreeSet<_>>().len(), VALUES.len(), "every render is its own code");
    for (k, v) in &vals {
        assert_eq!(Some(pad1[k]), gravity_dict::fp::fp128(v), "{:?}: the padded table keeps the render's bytes", String::from_utf8_lossy(v));
    }

    // 2. the trimmed tables: hashbpchar's image, each built as its own cache entry (the resident padded one never serves it)
    let b0 = fp_tables_resident().1;
    let (trim1, _) = pass(&list, REL + 1, TableFn::FingerprintBpchar, false);
    assert_eq!(fp_tables_resident().1 - b0, nparts, "one trimmed table built per part");
    for (k, v) in &vals {
        assert_eq!(Some(trim1[k]), gravity_dict::fp::fp128(trimmed(v)), "{:?}: the Fp128 of the bytes without trailing blanks", String::from_utf8_lossy(v));
    }
    // equal under bpchareq <=> one trimmed image, in and across parts; the padded images keep the renders apart
    for (k1, v1) in &vals {
        for (k2, v2) in &vals {
            assert_eq!(trim1[k1] == trim1[k2], trimmed(v1) == trimmed(v2), "{:?} vs {:?}", String::from_utf8_lossy(v1), String::from_utf8_lossy(v2));
            assert_eq!(pad1[k1] == pad1[k2], v1 == v2);
        }
    }

    // 3. both forms resident side by side: each served from its own entry, unchanged, nothing built
    let b1 = fp_tables_resident().1;
    let (trim2, _) = pass(&list, REL + 2, TableFn::FingerprintBpchar, false);
    let (pad2, _) = pass(&list, REL + 3, TableFn::Fingerprint, false);
    assert_eq!((&trim2, &pad2), (&trim1, &pad1));
    assert_eq!(fp_tables_resident().1, b1, "nothing built");

    // 4. the cache gone (a restart's stand-in): each form rebuilt from the dictionary, unchanged, and nothing written beside the parts
    fp_cache_drop_all();
    let b2 = fp_tables_resident().1;
    let (trim3, _) = pass(&list, REL + 4, TableFn::FingerprintBpchar, false);
    let (pad3, _) = pass(&list, REL + 5, TableFn::Fingerprint, false);
    assert_eq!((&trim3, &pad3), (&trim1, &pad1), "the trimmed images, never the padded ones");
    assert_eq!(fp_tables_resident().1 - b2, 2 * nparts, "every table rebuilt");
    assert!(std::fs::read_dir(&dir).unwrap().all(|e| !e.unwrap().file_name().to_str().unwrap().contains("-fp128-")), "no table written beside the parts");
    assert_eq!(gravity_dict::fp::fp128(trimmed(b"   ")), gravity_dict::fp::fp128(b""), "all blanks is the empty string");
    drop(list);
    let _ = std::fs::remove_dir_all(&dd);
}

#[test]
fn declare_table_links_the_bpchar_form_to_its_code_lane_and_the_host_builds_it() {
    let st = Arena::new(1 << 16);
    for (decl, func) in [(TableDecl::fingerprint_bpchar(1), TableFn::FingerprintBpchar), (TableDecl::fingerprint(1), TableFn::Fingerprint)] {
        let mut s = SiteDesc::range_source(st.handle(), 7, &[1, 2]).codes_lane(1);
        let t = s.declare_table(decl).unwrap();
        assert_eq!(s.lanes()[1].rep, Rep::Code { dict: DictRef { lane: 1, fp_table: t as u8 } }, "{decl:?}: the code lane's fingerprint table");
        assert_eq!(spec_of(&s, t as usize), Some(TableSpec { over: 1, func }));
    }
    assert_ne!(TableDecl::fingerprint_bpchar(1), TableDecl::fingerprint(1), "two tables, never one declaration");
}
