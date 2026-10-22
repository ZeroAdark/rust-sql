//! M4-L6-FP128 on the rig: a dictionary-coded part's `Fp128` table is a process cache. It is built on the part's first touch,
//! served to the open part it was built for with no dictionary stream, rebuilt with the same fingerprints once the cache drops
//! it, and never written beside the part; the fingerprints a lane publishes are the seeded SipHash-128 of the entries.
#[path = "common/granule_fixtures.rs"]
mod granule_fixtures;

use std::collections::BTreeMap;
use std::ffi::CString;
use std::sync::Arc;

use granule_fixtures::*;
use gravity_abi::Lane;
use gravity_driver::{blank_site, Source, Window};
use gravity_hosts::dict::text_hosts::{fp_cache_drop_all, fp_seed, fp_tables_resident, TableFn, TableSpec};
use gravity_hosts::ops::scan::pgrc::granule_source::{dict_census, GranuleRangeSource, GranuleRecipe, PartIoSpec, PartList, PartSpec, Resolved};
use pgrc_format::dirlayout::part_file_name;
use pgrc_qa::corpus::BuiltFixture;
use pgrc_read::{OpenPart, PartExpect, PartRegistry, VfsPartIo};

/// The fixture's parts as files under `dir`, each opened now (the list holds it open, as the resident table holds a server's).
fn open_parts(bf: &BuiltFixture, dir: &std::path::Path) -> Arc<PartList> {
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

/// One pass over every window of `list` with a fingerprint table over lane 0: `(part, code) -> fingerprint`, and the values
/// behind the codes when `render` (a dense pass only: a render on a sparse handle is a block fault, not a stream).
fn pass(list: &Arc<PartList>, rel: u32, render: bool) -> (BTreeMap<(u32, u32), [u64; 2]>, BTreeMap<(u32, u32), Vec<u8>>) {
    let recipe = GranuleRecipe::for_test_codes(rel, &[1], &[1], Arc::new(PartRegistry::new(64 << 20))).with_tables(vec![TableSpec { over: 0, func: TableFn::Fingerprint }]).with_sparse_dicts();
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

#[test]
fn a_parts_fingerprint_table_is_built_once_served_resident_rebuilt_alike_and_never_written() {
    fp_seed();
    let bf = text_fixture();
    let root = std::env::temp_dir().join(format!("gravity-fp128-cache-{}", std::process::id()));
    let dir = root.join("base/5/pgrc_7306");
    let list = open_parts(&bf, &dir);
    let nparts = list.len() as u64;
    let names = || {
        let mut v: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        v.sort();
        v
    };
    let parts_only = names();

    // 1. the first touch: built, one table per part, the dictionary stream walked; each fingerprint the seeded SipHash-128
    let (built0, whole0) = (fp_tables_resident().1, dict_census()[0]);
    let (fps1, vals) = pass(&list, REL_TEXT + 40, true);
    assert!(dict_census()[0] > whole0, "the first build walks the dictionary stream");
    assert_eq!(fp_tables_resident().1 - built0, nparts, "one table built per part");
    for ((p, c), fp) in &fps1 {
        assert_eq!(Some(*fp), gravity_dict::fp::fp128(&vals[&(*p, *c)]), "part {p} code {c}: the seeded SipHash-128 of the entry");
    }

    // 2. resident: the process cache serves the open parts, nothing built, no dictionary stream (the lane opened sparse)
    let (built1, whole1) = (fp_tables_resident().1, dict_census()[0]);
    let (fps2, _) = pass(&list, REL_TEXT + 41, false);
    assert_eq!(fps2, fps1, "the resident tables' fingerprints are the build's");
    assert_eq!(fp_tables_resident().1, built1, "nothing built");
    assert_eq!(dict_census()[0], whole1, "no dictionary stream read: the lane opened sparse");

    // 3. the cache gone (a restart's stand-in): every table rebuilt from its dictionary, the same fingerprints
    assert_eq!(fp_cache_drop_all(), nparts);
    let built2 = fp_tables_resident().1;
    let (fps3, _) = pass(&list, REL_TEXT + 42, false);
    assert_eq!(fps3, fps1, "a rebuilt table's fingerprints are the first build's");
    assert_eq!(fp_tables_resident().1 - built2, nparts, "one table rebuilt per part");

    // 4. nothing beside the parts: the tables live only in this process
    assert_eq!(names(), parts_only, "a file was written beside the parts");
    drop(list);
    let _ = std::fs::remove_dir_all(&root);
}
