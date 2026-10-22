//! The fingerprint seed (shared-tables.md §2.1): one per server process, seen alike by every backend thread, drawn anew by
//! every process (a data directory keeps none), and no table built before the first use.
#[path = "common/granule_fixtures.rs"]
mod granule_fixtures;

use std::ffi::CString;
use std::process::Command;
use std::sync::Arc;

use granule_fixtures::*;
use gravity_abi::Lane;
use gravity_driver::{blank_site, Source, Window};
use gravity_hosts::dict::text_hosts::{fp_seed, fp_tables_resident, TableFn, TableSpec};
use gravity_hosts::ops::scan::pgrc::granule_source::{GranuleRangeSource, GranuleRecipe, PartIoSpec, PartList, PartSpec, Resolved};
use pgrc_format::dirlayout::part_file_name;
use pgrc_read::{OpenPart, PartExpect, PartRegistry, VfsPartIo};

const CHILD: &str = "GRAVITY_FP_SEED_CHILD";
const CHILD_DIR: &str = "GRAVITY_FP_SEED_CHILD_DATADIR";

fn child_seed(pin: Option<&str>) -> (u64, u64) {
    let mut c = Command::new(std::env::current_exe().expect("exe"));
    c.args(["--exact", "child_prints_its_seed", "--nocapture"]).env(CHILD, "1").env_remove("PGRUST_GRAVITY_FP_SEED");
    if let Some(p) = pin {
        c.env("PGRUST_GRAVITY_FP_SEED", p);
    }
    let out = c.output().expect("child");
    let s = String::from_utf8_lossy(&out.stdout);
    // D2481: by word, not by line: under RUST_TEST_THREADS=1 (inherited) libtest prints "test <name> ... " before the println.
    let line = s.split_whitespace().find(|w| w.starts_with("seed=")).unwrap_or_else(|| panic!("no seed line in {s}"));
    let mut it = line[5..].split(',').map(|x| x.parse::<u64>().expect("u64"));
    (it.next().expect("k0"), it.next().expect("k1"))
}

#[test]
fn child_prints_its_seed() {
    if std::env::var_os(CHILD).is_some() {
        let (k0, k1) = fp_seed();
        println!("seed={k0},{k1}");
    }
}

#[test]
fn every_thread_of_one_server_sees_the_seed_the_first_draw_installed_and_nothing_is_built_before_use() {
    assert_eq!(fp_tables_resident().1, 0, "a table built before any statement asked for one");
    let first = fp_seed();
    let seeds: Vec<(u64, u64)> = (0..16).map(|_| std::thread::spawn(fp_seed)).collect::<Vec<_>>().into_iter().map(|h| h.join().expect("thread")).collect();
    assert!(seeds.iter().all(|&s| s == first));
    assert_eq!(gravity_dict::fp::seed(), Some(first));
    assert_eq!(fp_tables_resident().1, 0, "the seed draw built no table");
}

#[test]
fn a_new_server_instance_draws_a_new_seed_and_a_pinned_seed_is_the_pin() {
    let (a, b) = (child_seed(None), child_seed(None));
    assert_ne!(a, b, "two server instances share a seed: the draw is not random");
    let pin = "000102030405060708090a0b0c0d0e0f";
    assert_eq!(child_seed(Some(pin)), (0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908));
    assert_eq!(child_seed(Some(pin)), child_seed(Some(pin)));
}

/// The text fixture's parts as files under `dir`, each opened now (the list holds it open, as the resident table holds a
/// server's), and one pass over them with a fingerprint table over lane 0: the tables it built.
fn build_tables_beside_parts(dir: &std::path::Path) -> u64 {
    let bf = text_fixture();
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
    let list = Arc::new(PartList::new(parts));
    let recipe = GranuleRecipe::for_test_codes(REL_TEXT + 90, &[1], &[1], Arc::new(PartRegistry::new(64 << 20))).with_tables(vec![TableSpec { over: 0, func: TableFn::Fingerprint }]);
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&resolved)).unwrap();
    src.seek(&[0, list.granules()], &[0, 0]).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    let built = fp_tables_resident().1;
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        if src.window(&mut w).unwrap() == 0 {
            break;
        }
        let mut t = Lane::NONE;
        assert_eq!(src.lane(2, &[], &mut t).unwrap(), 1, "the fingerprint lane fills");
    }
    fp_tables_resident().1 - built
}

/// A child "server process" on the data directory `dir`: its seed, and the tables it built over the parts of one table
/// directory of its own under `dir`.
fn child_on_datadir(dir: &std::path::Path, n: u32) -> (u64, u64, u64) {
    let out = Command::new(std::env::current_exe().expect("exe"))
        .args(["--exact", "child_builds_tables_on_its_datadir", "--nocapture"])
        .env(CHILD_DIR, dir)
        .env("GRAVITY_FP_SEED_CHILD_REL", n.to_string())
        .env_remove("PGRUST_GRAVITY_FP_SEED")
        .output()
        .expect("child");
    let s = String::from_utf8_lossy(&out.stdout);
    // D2481: by word (see child_seed).
    let line = s.split_whitespace().find(|w| w.starts_with("cseed=")).unwrap_or_else(|| panic!("no cseed line in {s}{}", String::from_utf8_lossy(&out.stderr)));
    let f: Vec<u64> = line[6..].split(',').map(|x| x.parse().expect("u64")).collect();
    (f[0], f[1], f[2])
}

#[test]
fn child_builds_tables_on_its_datadir() {
    if let Some(d) = std::env::var_os(CHILD_DIR) {
        init_small::globals::SetDataDir(d.to_str().expect("utf-8 path"));
        let (k0, k1) = fp_seed();
        let rel = std::env::var("GRAVITY_FP_SEED_CHILD_REL").expect("rel");
        let built = build_tables_beside_parts(&std::path::Path::new(&d).join(format!("base/5/pgrc_{rel}")));
        println!("cseed={k0},{k1},{built}");
    }
}

/// Every file under `dir`, recursively.
fn files_under(dir: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).expect("read_dir") {
        let p = e.expect("entry").path();
        if p.is_dir() {
            out.extend(files_under(&p));
        } else {
            out.push(p.to_str().unwrap().to_string());
        }
    }
    out
}

/// The USER's ruling of 2026-10-20 (the seed protects against adversarial collisions): two server processes on one data
/// directory draw their own seeds, and neither writes a seed file or a fingerprint table beside its parts.
#[test]
fn two_processes_on_one_data_directory_draw_their_own_seeds_and_write_no_seed_file_or_fp128_sidecar() {
    let dir = std::env::temp_dir().join(format!("gravity-fp-seed-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("global")).expect("mkdir");
    let a = child_on_datadir(&dir, 7401);
    let b = child_on_datadir(&dir, 7402);
    let files = files_under(&dir);
    let _ = std::fs::remove_dir_all(&dir);
    assert_ne!((a.0, a.1), (b.0, b.1), "the second process took the first one's seed");
    assert!(a.2 > 0 && b.2 > 0, "each process built its fingerprint tables over file parts: {a:?} {b:?}");
    assert!(!files.iter().any(|f| f.ends_with("global/pgrust_gravity_fp_seed")), "a seed file was written: {files:?}");
    assert!(!files.iter().any(|f| f.contains("-fp128-")), "an fp128 sidecar was written: {files:?}");
}
