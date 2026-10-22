//! M6 Decision 8c (R13-L03 / L04): a shredded jsonb path lane's faces are its path's. A Text path is a code lane over the
//! dictionary its part sealed at the path's `path_ord` (the zero-null `DICT_EXEC` form or P-a's nullable twin), else the
//! part's byte ranks of the path's values; a lane's zone verdicts read the Stats sealed at its `path_ord`, and the catalog
//! (the footer's zone maps, the part answers) never speaks for a path.

#[path = "common/granule_fixtures.rs"]
mod granule_fixtures;
use granule_fixtures::*;

use std::sync::Arc;

use gravity_abi::ctx::lane;
use gravity_abi::Lane;
use gravity_driver::{blank_site, Source, Window};
use gravity_hosts::ops::scan::pgrc::granule_source::{answered_parts, answered_whole, shred_lane_ord, GranuleRangeSource, GranuleRecipe, Resolved};
use gravity_program::{ZoneOp, ZonePred};
use pgrc_format::class::StorageClass;
use pgrc_format::dirlayout::part_file_name;
use pgrc_format::enc::EncodingId;
use pgrc_format::part::{StreamRole, STREAMF_DICT_EXEC, STREAMF_DICT_EXEC_NULLABLE};
use pgrc_format::shredlane::ShredLaneKind;
use pgrc_qa::corpus::{open_part_bytes, BuiltFixture, Fixture, OracleVal};
use pgrc_qa::text_col;
use pgrc_read::PartRegistry;
use pgrc_write::dict::TextSemantics;
use pgrc_write::elect::{CandidateSource, CodecCandidates, ColumnPosture, DictPolicy, ReferenceCandidates};
use pgrc_write::writer::PartCutPolicy;
use pgrc_write::wvfs::WriteVfs;

/// Two parts: 24 576 and 10 848 rows (5 granules).
const N: u64 = 35_424;

/// The production lane posture (`pgrc_am` ingest `codec_candidates`, its [json-rung1] arm) with `exec_ok` as given:
/// `false` seals a NULL-bearing path's dictionary with no exec flag, as every seal before P-a did.
fn lane_dict(exec_ok: bool) -> CodecCandidates {
    let dict = DictPolicy { ndv_cap: 1_000_000, exec_ok, sem: TextSemantics::BytesOnly };
    CodecCandidates::new(ColumnPosture::default()).with_lane_default(1, ColumnPosture { dict: Some(dict), ..ColumnPosture::default() })
}

/// A jsonb column (attno 1) of `doc(i)` documents (`None`: SQL NULL) sealed through the production shred source under `cands`.
fn lane_fixture(relfilenumber: u64, doc: impl Fn(u64) -> Option<String>, cands: &CodecCandidates) -> BuiltFixture {
    use pgrc_qa::adapters::Probe;
    use pgrc_qa::corpus::{append_rows_with_shred, finish_with_shred, open_writer};
    use pgrc_write::publish::{effective_manifest, TxnVerdict};
    use pgrc_write::shred_jsonb::JsonbShredSource;
    let _ = mbutils::SetDatabaseEncoding(wchar::PG_UTF8);
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(mbutils::init_seams);
    let ctx = mcx::MemoryContext::new("granule-shred-codes-test");
    let images: Vec<Option<OracleVal>> = (0..N)
        .map(|i| {
            doc(i).map(|d| {
                let img = adt_jsonb::io::jsonb_in(ctx.mcx(), d.as_bytes(), None).unwrap_or_else(|e| panic!("jsonb_in: {}", e.message())).expect("jsonb");
                let hdr = u32::from_le_bytes([img[0], img[1], img[2], img[3]]);
                assert_eq!((hdr >> 2) as usize, img.len(), "a 4-byte varlena header");
                OracleVal::Bytes(img[4..].to_vec())
            })
        })
        .collect();
    let dir = format!("/qa/t{relfilenumber}");
    let fx = Fixture { name: "gravity_shred_codes", dir, spc: 1663, db: 5, relfilenumber, schema: vec![text_col(1)], oracle: vec![images], plans: vec![], policy: PartCutPolicy { max_rows: 20_000, ..PartCutPolicy::default() } };
    let sources: [&dyn CandidateSource; 2] = [cands, &ReferenceCandidates];
    let mut shred = JsonbShredSource::new().with_column(1);
    let mut vfs = pgrc_qa::simvfs::SimVfs::new();
    vfs.mkdir_path(&fx.dir).expect("mkdir");
    let fxid = 900 + fx.relfilenumber;
    let mut probe = Probe::new(TxnVerdict::Aborted);
    probe.mark(fxid, TxnVerdict::InProgress);
    let mut w = open_writer(&fx, fxid).expect("writer");
    append_rows_with_shred(&mut vfs, &mut w, &fx, 0, N, &sources, &mut shred).expect("append");
    finish_with_shred(&mut vfs, &mut w, &sources, &mut shred).expect("finish");
    w.publish(&mut vfs, &probe).expect("publish");
    probe.mark(fxid, TxnVerdict::Committed);
    let files = vfs.snapshot_dir(&fx.dir);
    let manifest = effective_manifest(&mut vfs, &fx.dir, &probe, None).expect("effective").expect("committed generation");
    BuiltFixture { fx, files, manifest }
}

/// `name` is `j->>'name'`'s text, SQL NULL where the document is NULL (every 13th row) or has no `name` (every 17th).
fn name_doc(i: u64) -> Option<String> {
    if i % 13 == 5 {
        return None;
    }
    if i % 17 == 3 {
        return Some(format!(r#"{{"k": {}}}"#, i % 1000));
    }
    Some(format!(r#"{{"k": {}, "name": "tok{:04}"}}"#, i % 1000, i % 97))
}

/// `k` climbs by one every 1 000 rows (granule-grain ranges), `name` as in `name_doc` without the NULLs.
fn step_doc(i: u64) -> Option<String> {
    Some(format!(r#"{{"k": {}, "name": "tok{:04}"}}"#, i / 1000, i % 97))
}

#[test]
fn a_text_path_is_a_code_lane_over_the_dictionary_its_part_sealed_at_the_path() {
    for (exec_ok, relfile, rel) in [(true, 7321u32, 4281u32), (false, 7322, 4282)] {
        let bf = lane_fixture(relfile as u64, name_doc, &lane_dict(exec_ok));
        // the part sealed the path's codes executable (the nullable twin) or storage-only (the pre-P-a form)
        let part = open_part_bytes(bf.files.get(&part_file_name(0)).expect("part"), 960 + relfile as u64).expect("open part");
        let ord = shred_lane_ord(&part, 1, "name", ShredLaneKind::Text, 0).unwrap().expect("the path is elected");
        assert_ne!(ord, 0);
        let dir = part.stream_directory().unwrap();
        let v = dir.lookup(1, ord, StreamRole::Values).expect("values");
        assert_eq!(v.entry.encoding, EncodingId::DictCodes.as_u16(), "exec_ok {exec_ok}: the path dict-elects");
        let want = if exec_ok { STREAMF_DICT_EXEC_NULLABLE } else { 0 };
        assert_eq!(v.entry.flags & (STREAMF_DICT_EXEC | STREAMF_DICT_EXEC_NULLABLE), want, "exec_ok {exec_ok}");
        let list = part_list(&bf);
        let recipe = GranuleRecipe::for_test_classes(rel, &[1, 1, 1], &[StorageClass::VarlenaVerbatim; 3], Arc::new(PartRegistry::new(64 << 20)))
            .with_shred_code_lane(1, "name")
            .with_shred_lane(2, "k", ShredLaneKind::NumericFs, 0);
        assert_eq!((recipe.kinds[1], recipe.widths[1]), (lane::KIND_CODES, 4), "the typid gate is the column's, not a path's");
        assert!(recipe.has_codes());
        let resolved = Resolved::for_recipe(&recipe, list.clone());
        let mut src = GranuleRangeSource::new(recipe);
        src.bind(&binding_ref(&resolved)).unwrap();
        src.seek(&[0, list.granules()], &[0, 0]).unwrap();
        let mut lanes = [Lane::NONE; MAX_LANES];
        let mut coded: Vec<(u32, u32, u64)> = Vec::new();
        let mut i = 0u64;
        loop {
            let mut site = blank_site(lanes.as_mut_ptr());
            let mut w = Window::over(&mut site);
            let n = src.window(&mut w).unwrap();
            if n == 0 {
                break;
            }
            let l = w.lanes();
            assert_eq!((l[1].kind, l[1].width, l[1].region), (lane::KIND_CODES, 4, lane::REGION_DICTIONARY));
            assert_eq!(l[1].ncodes_or_scale, 97, "exec_ok {exec_ok}: the path's 97 words, not the column's images");
            let part_id = w.base().part_id;
            for r in 0..n as usize {
                let d = name_doc(i);
                let has_name = d.as_ref().is_some_and(|d| d.contains("name"));
                // SAFETY: `r < n`; the lanes' cells are at their widths.
                unsafe {
                    assert_eq!(l[0].is_valid(r), d.is_some(), "row {i}: the image");
                    assert_eq!(l[1].is_valid(r), has_name, "row {i}: NULL exactly where j->>'name' is");
                    if has_name {
                        coded.push((part_id, *(l[1].ptr as *const u32).add(r), i));
                    }
                    assert_eq!(l[2].is_valid(r), d.is_some(), "row {i}: k");
                    if d.is_some() {
                        assert_eq!(*(l[2].ptr as *const i64).add(r), (i % 1000) as i64, "row {i}: k");
                    }
                }
                i += 1;
            }
        }
        assert_eq!(i, N);
        assert!(coded.len() as u64 > N / 2);
        for (p, c, row) in coded {
            let (datum, len) = src.dict_value(p, 1, c).unwrap();
            // SAFETY: a dictionary entry's image: the 4-byte header, then `len` bytes.
            let bytes = unsafe { core::slice::from_raw_parts((datum as *const u8).add(4), len as usize) };
            assert_eq!(bytes, format!("tok{:04}", row % 97).as_bytes(), "exec_ok {exec_ok}, row {row}: the code is its value's");
        }
    }
}

#[test]
fn a_shred_lanes_zone_verdicts_read_its_paths_stats_and_never_the_catalog() {
    let bf = lane_fixture(7323, step_doc, &lane_dict(true));
    let list = part_list(&bf);
    let total = list.granules();
    assert_eq!(total, 5);
    // a catalog zone map of the jsonb column claiming every granule holds only 0: were a path's predicate asked of it (the
    // serial census, the mapped lanes), `k = 20` would prune every granule
    list.set_zone_columns(vec![pgrc_am::footer::ZoneFacts::new(1, pgrc_format::zonemap::ZoneMap::encode(&[(0, 0); 5]).unwrap()).unwrap()]);
    let recipe = GranuleRecipe::for_test_classes(4283, &[1, 1], &[StorageClass::VarlenaVerbatim; 2], Arc::new(PartRegistry::new(64 << 20)))
        .with_shred_lane(0, "k", ShredLaneKind::NumericFs, 0)
        .with_shred_code_lane(1, "name");
    let preds = [ZonePred::word(0, ZoneOp::Eq, 20)];
    assert!(!answered_whole(&list, &recipe, &preds, &[0]), "the catalog's part answers are the column's, never a path's");
    assert_eq!(answered_parts(&list, &recipe, &preds, &[0]), None, "nor do they count a path's parts");
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.zone(&preds, &[0b1], &[0]);
    src.bind(&binding_ref(&resolved)).unwrap();
    src.seek(&[0, total], &[0, 0]).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    let mut count = 0u64;
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = src.window(&mut w).unwrap();
        if n == 0 {
            break;
        }
        if w.is_answered() {
            count += n as u64;
            continue;
        }
        let l = w.lanes();
        assert_eq!(l[1].ncodes_or_scale, 97, "the name path's dictionary, not k's");
        for r in 0..n as usize {
            // SAFETY: `r < n`; lane 0 is an int8 lane.
            unsafe {
                if l[0].is_valid(r) && *(l[0].ptr as *const i64).add(r) == 20 {
                    count += 1;
                }
            }
        }
    }
    assert_eq!(count, 1000, "k = 20 on rows 20 000..21 000");
    // granule 2 (rows 16 384..24 576, k 16..24) decodes; granules 0-1 and the second part (k 24..35) are skipped on the
    // path's own granule and part records
    assert_eq!(src.granules_decoded(), 1, "one Mixed granule");
    assert_eq!(src.granules_skipped(), total - 1, "the path's records prune the rest");
}
