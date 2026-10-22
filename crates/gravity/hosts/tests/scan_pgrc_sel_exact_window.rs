//! D4952: a window publishes AllMatch for its selection (D3223 S2, `sel_exact`) only when that window's own row selection
//! applied every zone predicate. A code.sparse window (H08: its walk applies the code verdict's one table) that follows an
//! exact row selection in the same source is a selection of the table's survivors, whatever the other zone predicates
//! say, so it publishes the window's zone verdict (Mixed), never AllMatch.
//! The data: one part of two granules, `code` text, `n` int8 (`n = i % 1000`) and `m` int8 (`m = i`, a lane read past
//! the filter, which a selection gathers). Granule 0 holds no '' (its block base is a word, which the table passes: the
//! whole decode and `row_select`); granule 1 is '' but every 4th row (the table rejects the base: the sparse walk).
//! `code <> '' AND n < 10`, as lowerv4 declares it.

use std::sync::Arc;

use gravity_abi::ctx::lane;
use gravity_abi::Lane;
use gravity_driver::{blank_site, Source, Window};
use gravity_hosts::dict::text_hosts::{TableFn, TableSpec};
use gravity_hosts::ops::scan::pgrc::granule_source::{GranuleRangeSource, GranuleRecipe, PartIoSpec, PartList, PartSpec, Resolved};
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

const REL: u32 = 4273;
const GRANULE: u64 = 8192;
const ROWS: u64 = 2 * GRANULE;
/// `n < CUT`: 1 % of either granule's rows.
const CUT: i64 = 10;

/// Row `i`'s text: one of 16 words in granule 0; in granule 1 a word every 4th row, else ''.
fn text(i: u64) -> Vec<u8> {
    if i < GRANULE || i % 4 == 0 { format!("w{:05}", (i * 7919) % 16).into_bytes() } else { Vec::new() }
}

fn num(i: u64) -> i64 {
    (i % 1000) as i64
}

fn fixture() -> BuiltFixture {
    use pgrc_qa::adapters::Probe;
    use pgrc_qa::corpus::{append_rows_with_sources, finish_with_sources, open_writer};
    use pgrc_write::publish::{effective_manifest, TxnVerdict};
    let fx = Fixture {
        name: "gravity_sel_exact_window",
        dir: "/qa/t7333".to_string(),
        spc: 1663,
        db: 5,
        relfilenumber: 7333,
        schema: vec![text_col(1), int8_col(2), int8_col(3)],
        oracle: vec![
            (0..ROWS).map(|i| Some(OracleVal::Bytes(text(i)))).collect(),
            (0..ROWS).map(|i| Some(OracleVal::Word(num(i) as u64))).collect(),
            (0..ROWS).map(|i| Some(OracleVal::Word(i))).collect(),
        ],
        plans: vec![],
        policy: PartCutPolicy { max_rows: ROWS, ..PartCutPolicy::default() },
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

fn list(bf: &BuiltFixture) -> Arc<PartList> {
    let parts = bf.manifest.parts.iter().enumerate().map(|(i, rec)| {
        let bytes: Arc<[u8]> = Arc::from(bf.files.get(&part_file_name(rec.part_no)).expect("part file").as_slice());
        PartSpec { io: PartIoSpec::Mem { bytes, dev: 7, ino: 730 + i as u64 }, expect: PartExpect::none(), rows: rec.rows, granules: rec.granule_count, deletes: None, part: None }
    });
    Arc::new(PartList::new(parts.collect()))
}

/// The `''` image a bound `<> ''` reads (a 1-byte-header varlena of no payload).
static EMPTY: [u8; 1] = [0x03];

/// One window: its granule, whether it is a selection, its zone verdict (lane `n`'s flags) and its rows' `n`.
struct Win {
    granule: u32,
    selected: bool,
    verdict: u8,
    nums: Vec<i64>,
}

/// Every window of `code <> '' AND n < CUT` with road code.sparse on.
fn walk(l: &Arc<PartList>) -> Vec<Win> {
    let ne = TableSpec { over: 0, func: TableFn::Eq { values: vec![Vec::new()], negated: true } }; // alloc: Cold
    let recipe = GranuleRecipe::for_test_codes(REL, &[1, 2, 3], &[0b1], Arc::new(PartRegistry::new(64 << 20))).with_tables(vec![ne]).with_verdict_cell(); // alloc: Cold
    recipe.verdicts.set(0, VERDICT_ZONE | code_sparse_table_arg(0));
    recipe.verdicts.set_word_zone([(word_zone_at_arg(0, ZoneOp::Ne as u8, BOUND_TEXT_PREFIX, 0), 0usize)].into_iter(), &[EMPTY.as_ptr() as u64], &[0]);
    let resolved = Resolved::for_recipe(&recipe, l.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.zone(&[ZonePred::bound_prefix(0, ZoneOp::Ne, 0, BOUND_TEXT_PREFIX), ZonePred::word(1, ZoneOp::Lt, CUT)], &[0], &[0]);
    src.bind(&binding_ref(&resolved)).unwrap();
    src.seek(&[0, l.granules()], &[0, 0]).unwrap();
    let mut out = Vec::new();
    let mut lanes = [Lane::NONE; MAX_LANES];
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        if src.window(&mut w).unwrap() == 0 {
            break;
        }
        let n = w.lanes()[1];
        // SAFETY: a window's lanes hold its rows at the positions its selection names
        let nums = (0..w.end()).map(|j| unsafe { n.datum(w.index(j)) } as i64).collect();
        out.push(Win { granule: w.base().granule, selected: !w.sel.is_null(), verdict: n.flags & lane::VERDICT_MASK, nums });
    }
    out
}

#[test]
fn a_sparse_window_after_an_exact_selection_publishes_no_all_match() {
    let wins = walk(&list(&fixture()));
    let at = |g: u32| wins.iter().find(|w| w.granule == g).unwrap_or_else(|| panic!("granule {g} has a window"));
    // the sequence under test: granule 0's row selection is exact (it published AllMatch over its survivors), then
    // granule 1 is a selection the walk made
    let (g0, g1) = (at(0), at(1));
    assert!(g0.selected && g0.verdict == lane::VERDICT_ALL_MATCH, "granule 0: an exact row selection");
    assert!(g1.selected && g1.nums.len() as u64 == GRANULE / 4, "granule 1: the walk's selection, every 4th row");
    // an AllMatch window holds no row failing a zone predicate
    for w in &wins {
        if w.verdict == lane::VERDICT_ALL_MATCH {
            let failing = w.nums.iter().filter(|&&v| v >= CUT).count();
            assert_eq!(failing, 0, "granule {}: AllMatch over {} rows, {failing} of them n >= {CUT}", w.granule, w.nums.len());
        }
    }
    assert_eq!(g1.verdict, lane::VERDICT_MIXED, "granule 1: the walk applied the table alone");
}
