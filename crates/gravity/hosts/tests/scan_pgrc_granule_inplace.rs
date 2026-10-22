//! PGRC-DECODE lever 2: a root VERBATIM by-value lane stored at the lane's width is published in place (a pointer into the
//! loaded extent image) instead of copied into its slot, and every other shape keeps copying. Its own test binary: the
//! tests assert on deltas of the process-global `in_place_census`, each under `SERIAL`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use gravity_abi::Lane;
use gravity_driver::{blank_site, Source, Window};
use gravity_hosts::ops::scan::pgrc::granule_source::{in_place_census, GranuleRangeSource, GranuleRecipe, PartIoSpec, PartList, PartSpec, Resolved};
use gravity_program::{ZoneOp, ZonePred};
use pgrc_qa::corpus::{BuiltFixture, Fixture, OracleVal};
use pgrc_read::PartRegistry;

#[path = "common/granule_fixtures.rs"]
mod granule_fixtures;
use granule_fixtures::*;

static SERIAL: Mutex<()> = Mutex::new(());

const REL: u32 = 4262;
const NROWS: u64 = 3 * GRANULE + 1500;

/// Column 1 (attno 1): `i % 1000` (a two-byte range: BYTE_FOR, an eighth of VERBATIM's bytes). Column 2 (attno 2): a full-range scramble of `i` (no
/// frame of reference narrows it: the writer elects VERBATIM at width 8), NULL every 13th row.
fn key(i: u64) -> Option<i64> {
    Some((i % 1000) as i64)
}
fn wide(i: u64) -> Option<i64> {
    (i % 13 != 5).then(|| (i.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(29) ^ 0xA5A5_5A5A_F00F_0FF0) as i64)
}

fn fixture() -> BuiltFixture {
    use pgrc_qa::adapters::Probe;
    use pgrc_qa::corpus::{append_rows_with_sources, finish_with_sources, open_writer};
    use pgrc_write::elect::{CandidateSource, CodecCandidates, ColumnPosture, ReferenceCandidates};
    use pgrc_write::publish::{effective_manifest, TxnVerdict};
    use pgrc_write::wvfs::WriteVfs;
    let col = |f: fn(u64) -> Option<i64>| (0..NROWS).map(|i| f(i).map(|v| OracleVal::Word(v as u64))).collect();
    let fx = Fixture {
        name: "gravity_inplace",
        dir: "/qa/t7340".to_string(),
        spc: 1663,
        db: 5,
        relfilenumber: 7340,
        schema: vec![pgrc_qa::int8_col(1), pgrc_qa::int8_col(2)],
        oracle: vec![col(key), col(wide)],
        plans: vec![],
        policy: pgrc_write::writer::PartCutPolicy { max_rows: 20_000, ..Default::default() },
    };
    // pgrc's real election (the AM's codec candidates)
    let cands = CodecCandidates::new(ColumnPosture::default());
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
    BuiltFixture { fx, files, manifest }
}

/// The fixture's parts; with `dv`, part 0's granule 1 carries a delete mask of rows 5 and 700.
fn list(bf: &BuiltFixture, dv: bool) -> Arc<PartList> {
    Arc::new(PartList::new(bf.manifest.parts.iter().enumerate().map(|(i, rec)| {
        let bytes: Arc<[u8]> = Arc::from(bf.files.get(&pgrc_format::dirlayout::part_file_name(rec.part_no)).expect("part file").as_slice());
        let d = (dv && i == 0).then(|| pgrc_format::dml::encode_dv(rec.part_no, 1, &[(1, pgrc_format::dml::DvBlockKind::List, &[5u16, 700][..])]).unwrap());
        let deletes = d.map(|d| Arc::new(pgrc_scan::PartDeletes::from_dv_payload(&d, rec.part_no, 1, rec.granule_count).unwrap()));
        PartSpec { io: PartIoSpec::Mem { bytes, dev: 7, ino: 900 + i as u64 }, expect: pgrc_read::PartExpect::none(), rows: rec.rows, granules: rec.granule_count, deletes, part: None }
    }).collect()))
}

fn lane_value(l: &Lane, i: u32) -> i64 {
    // SAFETY: row `i` of a presented window lies inside the lane's decoded rows at its width.
    unsafe { (l.ptr.add(i as usize * l.width as usize) as *const i64).read_unaligned() }
}

fn lane_valid(l: &Lane, i: u32) -> bool {
    // SAFETY: a non-null validity lane holds the granule's words.
    l.valid.is_null() || unsafe { *l.valid.add(i as usize / 64) >> (i % 64) & 1 == 1 }
}

/// One walk over `attnos` under `preds` (on lane 0): every presented row's lane values checked against the oracle (NULL
/// rows by their validity bit); returns the rows presented per window and the `in_place_census` delta `[in place, copied]`.
fn walk(list: &Arc<PartList>, attnos: &[i16], preds: &[ZonePred]) -> (Vec<usize>, [u64; 2]) {
    let recipe = GranuleRecipe::for_test(REL, attnos, Arc::new(PartRegistry::new(64 << 20)));
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    if !preds.is_empty() {
        src.zone(preds, &[0], &[0]);
    }
    src.bind(&binding_ref(&resolved)).unwrap();
    let c0 = in_place_census();
    src.seek(&[0, list.granules()], &[0, 0]).unwrap();
    let starts: BTreeMap<(u32, u32), u64> = {
        let (mut m, mut at) = (BTreeMap::new(), 0u64);
        for (p, s) in list.parts().iter().enumerate() {
            for g in 0..s.granules {
                m.insert((p as u32, g), at + g as u64 * GRANULE);
            }
            at += s.rows;
        }
        m
    };
    let (mut lanes, mut out) = ([Lane::NONE; MAX_LANES], Vec::new());
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = src.window(&mut w).unwrap();
        if n == 0 {
            break;
        }
        // SAFETY: a window's selection holds `sel_n` rows.
        let rows: Vec<u32> = if w.sel.is_null() { (0..n).collect() } else { unsafe { std::slice::from_raw_parts(w.sel, w.sel_n as usize) }.to_vec() };
        let at = starts[&(w.base().part_id, w.base().granule)];
        for (k, &a) in attnos.iter().enumerate() {
            let l = w.lanes()[k];
            let f = if a == 1 { key } else { wide };
            for &r in &rows {
                let want = f(at + r as u64);
                assert_eq!(lane_valid(&l, r), want.is_some(), "attno {a} row {} validity", at + r as u64);
                if let Some(v) = want {
                    assert_eq!(lane_value(&l, r), v, "attno {a} row {}", at + r as u64);
                }
            }
        }
        out.push(rows.len());
    }
    src.unbind();
    let c1 = in_place_census();
    (out, [c1[0] - c0[0], c1[1] - c0[1]])
}

/// The VERBATIM lane reads in place, whole granules and the part's short last granule alike, with its NULLs' validity;
/// the BYTE_FOR lane beside it copies (G4: not VERBATIM). The results are the oracle's either way.
#[test]
fn a_verbatim_word_lane_reads_in_place_and_a_byte_for_lane_copies() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let bf = fixture();
    let l = list(&bf, false);
    let g = l.granules();
    // an extent is a band of 8 granules and a root VERBATIM body runs to its end: a band's last granule has no slack
    // after it and copies, every other granule reads in place
    let last: u64 = l.parts().iter().map(|p| (p.granules as u64).div_ceil(8)).sum();
    assert!(g > last, "the fixture has granules inside a band");
    let (rows, c) = walk(&l, &[2], &[]);
    assert_eq!(rows.iter().sum::<usize>() as u64, NROWS);
    assert_eq!(c, [g - last, last], "the verbatim lane goes in place but for each band's last granule");
    let (_, c) = walk(&l, &[1], &[]);
    assert_eq!(c, [0, g], "a BYTE_FOR lane is never in place");
    let (_, c) = walk(&l, &[1, 2], &[]);
    assert_eq!(c, [g - last, g + last]);
}

/// G4: a zone lane (the host's filter reads its slot) and a gathered lane (a selection writes rows at their positions)
/// copy; a delete mask keeps the whole-granule decode, so its lane stays in place and presents the surviving rows.
#[test]
fn a_zone_lane_and_a_gathered_lane_copy_and_a_delete_mask_stays_in_place() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let bf = fixture();
    let l = list(&bf, false);
    // the verbatim lane as the zone lane: every row passes, the lane is still the filter's
    let (rows, c) = walk(&l, &[2], &[ZonePred::word(0, ZoneOp::Ne, 0)]);
    assert_eq!(rows.iter().sum::<usize>() as u64, NROWS);
    assert_eq!(c[0], 0, "a zone lane is never in place: {c:?}");
    // the key filters to 3 rows in 1 000: the verbatim lane is gathered for them
    let (rows, c) = walk(&l, &[1, 2], &[ZonePred::word(0, ZoneOp::Lt, 3)]);
    assert_eq!(rows.iter().sum::<usize>() as u64, (0..NROWS).filter(|&i| i % 1000 < 3).count() as u64);
    assert_eq!(c[0], 0, "a gathered lane is never in place: {c:?}");
    // a delete mask on part 0's granule 1
    let d = list(&bf, true);
    let (rows, c) = walk(&d, &[2], &[]);
    assert_eq!(rows[1], GRANULE as usize - 2);
    assert!(c[0] >= 2, "the masked granule decodes whole, in place: {c:?}");
}
