//! Road code.sparse's host half on real parts (V4R-137, M3-LEVERS-A; the table form, H08): with the code verdict's table
//! of `code <> ''` named (`code_sparse_table_arg`) the granule host reads a granule whose code block's base the table
//! rejects as packed codes (hosts v4 m3_codes.rs) and selects the non-base rows; the rows a program passes (`code <> ''`,
//! valid, selected) and their lanes are bit-exact with the whole decode + row selection path, over granules sparse, dense,
//! all-'' (skipped), without any '' (the whole decode), with NULLs and deletes. `''` is a constant like any other here.

use std::sync::Arc;

use gravity_abi::Lane;
use gravity_driver::{blank_site, Source, Window};
use gravity_hosts::ops::scan::pgrc::granule_source::{GranuleRangeSource, GranuleRecipe, PartIoSpec, PartList, PartSpec, Resolved};
use gravity_hosts::dict::text_hosts::{TableFn, TableSpec};
use gravity_program::protos::{code_sparse_table_arg, word_zone_at_arg, VERDICT_ZONE};
use gravity_program::{ZoneOp, ZonePred, BOUND_TEXT_PREFIX};
use pgrc_format::dirlayout::part_file_name;
use pgrc_qa::corpus::{BuiltFixture, Fixture, OracleVal};
use pgrc_qa::{int8_col, text_col};
use pgrc_read::{PartExpect, PartRegistry};
use pgrc_write::dict::TextSemantics;
use pgrc_write::elect::{CandidateSource, CodecCandidates, ColumnPosture, DictPolicy, ReferenceCandidates};
use pgrc_write::writer::PartCutPolicy;
use pgrc_write::wvfs::WriteVfs;

#[path = "common/granule_fixtures.rs"]
mod granule_fixtures;
use granule_fixtures::{binding_ref, MAX_LANES};

const REL: u32 = 4271;
const ROWS: u64 = 45_000;
const GRANULE: u64 = 8192;

/// Row `i`'s text (`None` = NULL), by granule: 0 every 13th row of 5 words (a 3-bit field); 1 every 7th of 300; 2 all '';
/// 3 no '' at all (its base is no empty code); 4 every other row of 3 000 (a dense survivor set); 5 every 3rd of 40 with
/// every 101st row NULL. A second part starts at row 24 576 (its own dictionary).
fn value(i: u64) -> Option<Vec<u8>> {
    let word = |n: u64| format!("w{:05}", (i * 7919) % n).into_bytes();
    Some(match i / GRANULE {
        _ if i / GRANULE == 5 && i % 101 == 0 => return None,
        0 if i % 13 == 0 => word(5),
        1 if i % 7 == 0 => word(300),
        3 => word(64),
        4 if i % 2 == 0 => word(3000),
        5 if i % 3 == 0 => word(40),
        _ => Vec::new(),
    })
}

fn fixture() -> BuiltFixture {
    use pgrc_qa::adapters::Probe;
    use pgrc_qa::corpus::{append_rows_with_sources, finish_with_sources, open_writer};
    use pgrc_write::publish::{effective_manifest, TxnVerdict};
    let fx = Fixture {
        name: "gravity_code_sparse",
        dir: "/qa/t7331".to_string(),
        spc: 1663,
        db: 5,
        relfilenumber: 7331,
        schema: vec![text_col(1), int8_col(2)],
        oracle: vec![(0..ROWS).map(|i| value(i).map(OracleVal::Bytes)).collect(), (0..ROWS).map(|i| Some(OracleVal::Word(i))).collect()],
        plans: vec![],
        policy: PartCutPolicy { max_rows: 24_576, ..PartCutPolicy::default() },
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

/// The parts, part 0's granule 1 with every 5th row deleted when `deletes`.
fn list(bf: &BuiltFixture, deletes: bool) -> Arc<PartList> {
    let parts = bf.manifest.parts.iter().enumerate().map(|(i, rec)| {
        let bytes: Arc<[u8]> = Arc::from(bf.files.get(&part_file_name(rec.part_no)).expect("part file").as_slice());
        let rows: Vec<u16> = (0..GRANULE as u16).step_by(5).collect();
        let dv = (deletes && i == 0).then(|| pgrc_format::dml::encode_dv(rec.part_no, 1, &[(1, pgrc_format::dml::DvBlockKind::List, &rows[..])]).unwrap());
        let dv = dv.map(|d| Arc::new(pgrc_scan::PartDeletes::from_dv_payload(&d, rec.part_no, 1, rec.granule_count).unwrap()));
        PartSpec { io: PartIoSpec::Mem { bytes, dev: 7, ino: 700 + i as u64 }, expect: PartExpect::none(), rows: rec.rows, granules: rec.granule_count, deletes: dv, part: None }
    });
    Arc::new(PartList::new(parts.collect()))
}

/// The `''` image a bound `<> ''` reads (a 1-byte-header varlena of no payload).
static EMPTY: [u8; 1] = [0x03];

/// Every row a program passes: (global row, code, int lane), and the (selected, dense) window counts. `code <> ''` as
/// lowerv4 declares it: a bound prefix zone predicate and the code verdict's `EQ` table (negated) over lane 0.
fn walk(l: &Arc<PartList>, enables: u64) -> (Vec<(u64, u32, u64)>, u64, u64) {
    let ne = TableSpec { over: 0, func: TableFn::Eq { values: vec![Vec::new()], negated: true } }; // alloc: Cold
    let recipe = GranuleRecipe::for_test_codes(REL, &[1, 2], &[0b1], Arc::new(PartRegistry::new(64 << 20))).with_tables(vec![ne]).with_verdict_cell(); // alloc: Cold
    recipe.verdicts.set(0, enables);
    recipe.verdicts.set_word_zone([(word_zone_at_arg(0, ZoneOp::Ne as u8, BOUND_TEXT_PREFIX, 0), 0usize)].into_iter(), &[EMPTY.as_ptr() as u64], &[0]);
    let resolved = Resolved::for_recipe(&recipe, l.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.zone(&[ZonePred::bound_prefix(0, ZoneOp::Ne, 0, BOUND_TEXT_PREFIX)], &[0], &[0]);
    src.bind(&binding_ref(&resolved)).unwrap();
    src.seek(&[0, l.granules()], &[0, 0]).unwrap();
    let (mut rows, mut selected, mut dense) = (Vec::new(), 0, 0);
    let mut lanes = [Lane::NONE; MAX_LANES];
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        if src.window(&mut w).unwrap() == 0 {
            break;
        }
        let (codes, ints) = (w.lanes()[0], w.lanes()[1]);
        let first = l.parts()[..w.base().part_id as usize].iter().map(|p| p.rows).sum::<u64>() + w.base().granule as u64 * GRANULE;
        if w.sel.is_null() { dense += 1 } else { selected += 1 }
        for j in 0..w.end() {
            let i = unsafe { w.index(j) };
            let code = unsafe { *(codes.ptr as *const u32).add(i) };
            let a = unsafe { ints.datum(i) } as u64;
            if value(a).is_some_and(|v| !v.is_empty()) && unsafe { codes.is_valid(i) } {
                rows.push((first + i as u64, code, a));
            }
        }
    }
    (rows, selected, dense)
}

#[test]
fn the_sparse_select_passes_exactly_the_rows_the_whole_decode_passes() {
    let bf = fixture();
    for deletes in [false, true] {
        let l = list(&bf, deletes);
        let (off, _, off_dense) = walk(&l, VERDICT_ZONE);
        let (on, on_sel, on_dense) = walk(&l, VERDICT_ZONE | code_sparse_table_arg(0));
        assert_eq!(on, off, "deletes {deletes}: the same rows, codes and int lane");
        let want = (0..ROWS).filter(|&i| value(i).is_some_and(|v| !v.is_empty()) && !(deletes && i / GRANULE == 1 && i % 5 == 0)).count();
        assert_eq!(on.len(), want, "deletes {deletes}: every non-empty live row once");
        assert!(on.iter().all(|&(i, _, a)| a == i), "a selected row's int lane is its own");
        // the road selects where the row selection alone kept the window whole; granule 3 (no '') keeps its dense window
        assert!(on_sel >= 1 && on_dense < off_dense, "deletes {deletes}: sparse windows {on_sel}, dense {on_dense} vs {off_dense}");
    }
}
