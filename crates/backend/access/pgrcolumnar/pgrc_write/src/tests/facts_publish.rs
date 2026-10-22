//! The generation's stats at publish (P-1: `stats.rs`, `facts.rs`): every publish carries its generation's stats
//! (runs and loose parts) instead of a sections file; the image assembled from them is byte-identical to a walk over
//! the generation's parts (the storage widths aside), the answers carry every part's Stats record, and the base comes
//! from the catalog's copy (`BaseGen`) or, without one, from the directory, with equal results.

use super::*;
use crate::facts::FactsAcc;
use crate::publish::{commit_pointer, BaseGen, PublishOutcome};
use pgrc_format::dirlayout::{manifest_file_name, part_file_name, sections_file_name};
use pgrc_format::facts::FactsImage;
use pgrc_read::{MemPartIo, OpenPart, PartExpect};

fn publish(vfs: &mut MemVfs, fxid: u64, rows: u64, start: i64, base: Option<&PublishOutcome>) -> PublishOutcome {
    let base = base.map(|b| BaseGen { manifest: &b.manifest, pointer: commit_pointer(&b.manifest), stats: &b.stats, full: None });
    publish_on(vfs, fxid, rows, start, base.into())
}

fn publish_on(vfs: &mut MemVfs, fxid: u64, rows: u64, start: i64, base: crate::publish::BaseStats<'_>) -> PublishOutcome {
    let mut kit = Kit::new();
    let mut w = open_writer(vec![int8_col(1)], stamp(fxid, 1));
    append_int8_rows(&mut w, vfs, &mut kit, rows, |i| Some(start + i as i64));
    {
        let sources: [&dyn crate::elect::CandidateSource; 1] = [&kit.cands];
        let mut env = crate::writer::SealEnv { vfs, sources: &sources, resolver: &kit.resolver, shred: &mut kit.shred, shred_opts: &kit.opts };
        w.finish(&mut env).expect("finish");
    }
    w.publish_with_gen(vfs, &Probe::new(TxnVerdict::Committed), base).expect("publish")
}

fn write(vfs: &mut MemVfs, path: &str, bytes: &[u8]) {
    let fd = vfs.create_rw(path).expect("create");
    vfs.pwrite_at(&fd, 0, bytes).expect("write");
    vfs.close_file(fd).expect("close");
}

/// A published image with the storage widths blanked, as a walk would write it.
fn mask(mut img: FactsImage) -> Vec<u8> {
    img.cols.iter_mut().for_each(|c| c.width = 0);
    img.encode().unwrap()
}

fn walk(vfs: &mut MemVfs, m: &Manifest) -> FactsImage {
    let mut acc = FactsAcc::new();
    let opened: Vec<(std::sync::Arc<OpenPart>, u64)> = m
        .parts
        .iter()
        .map(|rec| {
            let bytes = vfs.read_full(&format!("{DIR}/{}", part_file_name(rec.part_no))).expect("part");
            let part = OpenPart::open(Box::new(MemPartIo::new(bytes, 7, u64::from(rec.part_no) + 1)), &PartExpect::none()).expect("open");
            (std::sync::Arc::new(part), rec.file_len)
        })
        .collect();
    acc.fold_open_parts(opened.iter().map(|(p, len)| (&**p, *len))).expect("walk");
    acc.into_image(m.header.gen, m.header.publisher_fxid, m.header.schema_fingerprint).unwrap()
}

#[test]
fn every_publish_carries_the_generation_stats_and_the_image_equals_the_walk() {
    let mut vfs = mem_with_dir();
    let out1 = publish(&mut vfs, 90, 30, 0, None);
    let out2 = publish(&mut vfs, 91, 20, 1000, Some(&out1));
    let m2 = &read_manifest(&mut vfs, 2);
    assert_eq!(m2, &out2.manifest);
    assert_eq!(m2.parts.len(), 2);
    assert!(!vfs.exists_path(&format!("{DIR}/{}", sections_file_name(2))).unwrap(), "P-1 L2: no sections file");
    assert_eq!((out2.runs_written, out2.run_bytes), (0, 0), "two loose parts: no run");
    assert!(!out1.base_given && out2.base_given, "the catalog's generation served the base: CURRENT still named it");
    // the manifest file is a version-1 manifest byte for byte: the reader main ships (whole
    // file, exact length, trailing crc) parses it
    let mbytes = vfs.read_full(&format!("{DIR}/{}", manifest_file_name(2))).unwrap();
    assert_eq!(&Manifest::decode(&mbytes).expect("main's decode").encode()[..], &mbytes[..]);
    let (img, cols) = out2.stats.generation(m2, true).expect("generation");
    assert_eq!((img.header.gen, img.header.publisher_fxid, img.header.parts, img.header.rows), (2, 91, 2, 50));
    // The publisher knows the storage width, a walk does not (0): everything else is byte-identical.
    assert_eq!(img.cols[0].width, 8, "the publisher recorded int8's storage width");
    assert_eq!(walk(&mut vfs, m2).encode().unwrap(), mask(img.clone()), "the generation's image == walk over the parts, byte for byte");
    let served = crate::facts::serve(&img, &|c| crate::facts::zone_rule_without_catalog(c).unwrap()).expect("serve");
    let c1 = served.iter().find(|c| c.attno == 1).expect("int8 column");
    assert_eq!(c1.codec, crate::facts::CODEC_WORD);
    let zones = c1.zones.as_ref().expect("a word column carries zones").decode().unwrap();
    assert_eq!(zones.len() as u64, img.header.granules);
    assert_eq!(zones.first(), Some(&(0, 29)));
    assert_eq!(zones.last(), Some(&(1000, 1019)));
    assert!(c1.ndv >= 40 && c1.ndv <= 60, "ndv {}", c1.ndv);
    // the answers: one column, every part's Stats record by part, the base part's carried verbatim
    assert_eq!(cols.len(), 1);
    let col = pgrc_format::answers::ColumnAnswers::parse(&cols[0].1, 2).expect("column");
    let m1 = read_manifest(&mut vfs, 1);
    let (img1, cols1) = out1.stats.generation(&m1, true).expect("generation 1");
    let col1 = pgrc_format::answers::ColumnAnswers::parse(&cols1[0].1, 1).expect("column");
    assert_eq!(col.stats_body(0), col1.stats_body(0), "the base part's record carried verbatim");
    let rec = |b: &[u8]| { assert_eq!(b.len(), pgrc_format::meta::STATS_RECORD_LEN, "part-grain: one record"); pgrc_format::meta::StatsRecord::decode(&mut pgrc_format::wire::Cur::new(b)).unwrap() };
    assert_eq!((rec(col.stats_body(0).unwrap()).nonnull, rec(col.stats_body(1).unwrap()).nonnull), (30, 20));
    assert_eq!(walk(&mut vfs, &m1).encode().unwrap(), mask(img1));
}

#[test]
fn a_base_without_the_catalogs_copy_is_loaded_from_the_directory_with_equal_results() {
    let mut dir_vfs = mem_with_dir();
    let mut gen_vfs = mem_with_dir();
    let (mut a, mut b) = (None::<PublishOutcome>, None::<PublishOutcome>);
    // 40 one-part publishes: two folds of 16 loose parts into small runs, 8 loose at the end
    for k in 0..40u64 {
        let fxid = 200 + k;
        let from_dir = publish(&mut dir_vfs, fxid, 10, (k * 100) as i64, None);
        let from_gen = publish(&mut gen_vfs, fxid, 10, (k * 100) as i64, b.as_ref());
        // the same policy decisions either way; the bytes may differ in the storage widths only (a footer does not
        // carry them, a seal does), so the images are compared as served
        assert_eq!(from_dir.manifest.parts, from_gen.manifest.parts, "publish {k}: the same parts either way");
        let runs = |o: &PublishOutcome| o.stats.runs.iter().map(|r| r.parts.clone()).collect::<Vec<_>>();
        let loose = |o: &PublishOutcome| o.stats.loose.iter().map(|p| p.ident).collect::<Vec<_>>();
        assert_eq!((runs(&from_dir), loose(&from_dir)), (runs(&from_gen), loose(&from_gen)), "publish {k}: the same runs either way");
        assert_eq!(from_dir.runs_written, from_gen.runs_written, "publish {k}");
        let all = |_: &pgrc_format::facts::ColFacts| true;
        let served = |o: &PublishOutcome| crate::facts::serve(&o.stats.image(&o.manifest).expect("image"), &all).expect("serve");
        assert_eq!(served(&from_dir), served(&from_gen), "publish {k}: served-equal");
        a = Some(from_dir);
        b = Some(from_gen);
    }
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!((a.stats.runs.len(), a.stats.loose.len()), (2, 8));
    let m = read_manifest(&mut dir_vfs, 40);
    assert!(pgrc_format::manifest::stats_ref(&m).unwrap().is_some(), "a root once runs exist");
    let img = a.stats.image(&m).expect("image");
    assert_eq!(walk(&mut dir_vfs, &m).encode().unwrap(), mask(img), "the assembled image == the walk, byte for byte");
    let mb = read_manifest(&mut gen_vfs, 40);
    assert_eq!(walk(&mut gen_vfs, &mb).encode().unwrap(), mask(b.stats.image(&mb).expect("image")));
}

/// OC-5: a root-less generation of `LOOSE_MAX` parts or more (a directory written before P-1: no `EXT_STATS`, no run
/// file). The next publish writes one legacy run of every base part, seeded from the catalog's image and answers
/// (`BaseStats::Folded`) or built from the parts' footers, and says so (`legacy_parts`); its image equals the walk; the
/// publish after it writes no run.
#[test]
fn a_rootless_base_gets_one_legacy_run_and_the_next_publish_writes_none() {
    use crate::publish::BaseStats;
    for seeded in [false, true] {
        let mut vfs = mem_with_dir();
        let mut last: Option<PublishOutcome> = None;
        for k in 0..20u64 {
            last = Some(publish(&mut vfs, 300 + k, 10, (k * 100) as i64, last.as_ref()));
        }
        let m = read_manifest(&mut vfs, 20);
        assert!(pgrc_format::manifest::stats_ref(&m).unwrap().is_some());
        // the base binary's directory: the manifest without EXT_STATS, CURRENT naming it, no run file
        let old = m.clone().with_ext(pgrc_format::manifest::with_stats_ref(&m.ext, None).unwrap());
        write(&mut vfs, &format!("{DIR}/{}", manifest_file_name(20)), &old.encode());
        write(&mut vfs, &format!("{DIR}/CURRENT"), &commit_pointer(&old).encode());
        for n in vfs.list_dir(DIR).unwrap().into_iter().filter(|n| n.starts_with("srun-")) {
            vfs.unlink_path(&format!("{DIR}/{n}")).unwrap();
        }
        // the catalog's copy of such a generation: the fold over its parts, and the answers of their footers
        let img = walk(&mut vfs, &old);
        let loose = old.parts.iter().map(|rec| crate::publish::part_stats_on_disk(&mut vfs, DIR, &old, rec).map(std::sync::Arc::new)).collect::<crate::WriteResult<Vec<_>>>().unwrap();
        let (_, cols) = crate::stats::StatsState { root: None, runs: Vec::new(), loose }.generation(&old, true).unwrap();
        let base = if seeded { BaseStats::Folded(Some(&img), Some(&cols)) } else { BaseStats::None };
        let o = publish_on(&mut vfs, 400, 10, 5000, base);
        assert_eq!(o.legacy_parts, 20, "seeded {seeded}: one legacy run of the base's parts");
        assert!(o.stats.root.is_some() && o.runs_written >= 1, "seeded {seeded}");
        assert!(o.stats.runs.iter().any(|r| r.parts.len() == 20), "seeded {seeded}: the legacy run holds every base part");
        let m21 = read_manifest(&mut vfs, 21);
        assert_eq!(walk(&mut vfs, &m21).encode().unwrap(), mask(o.stats.image(&m21).expect("image")), "seeded {seeded}: the image == the walk");
        let o2 = publish(&mut vfs, 401, 10, 6000, Some(&o));
        assert_eq!((o2.legacy_parts, o2.runs_written, o2.run_bytes), (0, 0, 0), "seeded {seeded}: flat after the legacy run");
        assert!(o2.base_given);
    }
}

/// P5-28: the walk's coalescing gap is the device regime's (`cost_profile::device`) over the residency of the sections'
/// span; an in-memory part cannot tell, so it takes the cold gap, and no sections (no span) is cold too.
#[test]
fn the_meta_walk_gap_comes_from_the_device_regime() {
    use cost_profile::device::Device;
    let mut vfs = mem_with_dir();
    publish(&mut vfs, 90, 30, 0, None);
    let m = read_manifest(&mut vfs, 1);
    let rec = &m.parts[0];
    let bytes = vfs.read_full(&format!("{DIR}/{}", part_file_name(rec.part_no))).expect("part");
    let part = OpenPart::open(Box::new(MemPartIo::new(bytes, 7, 1)), &PartExpect::none()).expect("open");
    let idxs: Vec<usize> = (0..part.sections().len()).collect();
    assert_eq!(crate::facts::meta_coalesce_gap(&part, &idxs), Device::COLD.coalesce_gap());
    assert_eq!(crate::facts::meta_coalesce_gap(&part, &[]), Device::COLD.coalesce_gap());
}
