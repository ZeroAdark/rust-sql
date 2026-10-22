//! decode.late (H01e) on `GranuleRangeSource`: a late lane is left undecoded at the window, its validity exact, and
//! `lane_fill` decodes the rows asked, each once, at their positions: per encoding the host decodes (DeltaFor, BYTE_FOR,
//! dictionary codes and verbatim text read by reference, NULL maps), the filled rows equal the eager decode's.

use std::sync::Arc;

use gravity_abi::ctx::lane;
use gravity_abi::Lane;
use gravity_driver::{blank_site, Source, Window};
use gravity_hosts::ops::scan::pgrc::granule_source::{GranuleRangeSource, GranuleRecipe, PartList, Resolved};
use gravity_program::protos::VERDICT_LATE;
use gravity_program::protos::VERDICT_ZONE;
use pgrc_qa::adapters::ForcedPlan;
use pgrc_qa::corpus::{build_fixture, int8_fixture, BuiltFixture, Fixture, OracleVal};
use pgrc_qa::text_col;
use pgrc_read::PartRegistry;
use pgrc_write::elect::{CandidateSource, CodecCandidates, ColumnPosture, ReferenceCandidates};
use pgrc_write::writer::PartCutPolicy;
use pgrc_write::wvfs::WriteVfs;

#[path = "common/granule_fixtures.rs"]
#[allow(dead_code)]
mod granule_fixtures;
use granule_fixtures::*;

/// Row `i`'s cell: `None` for NULL, else the word (by value) or the image's bytes (by reference).
type Cell = Option<Vec<u8>>;

fn cell(l: &Lane, i: usize, byref: bool) -> Cell {
    unsafe {
        if !l.is_valid(i) {
            return None;
        }
        let d = l.datum(i);
        if !byref {
            return Some(d.to_le_bytes().to_vec());
        }
        let p = d as *const u8;
        let (hdr, len) = if *p & 1 == 1 { (1, (*p >> 1) as usize) } else { (4, (*(p as *const u32) >> 2) as usize) };
        Some(core::slice::from_raw_parts(p.add(hdr), len - hdr).to_vec())
    }
}

/// Every window's lane 0 over granules `[0, total)`: `fill` runs the late fills of one window (after which it reads).
fn windows(src: &mut GranuleRangeSource, total: u64, byref: bool, fill: &mut dyn FnMut(&mut GranuleRangeSource, &Lane, usize) -> Vec<(usize, Cell)>) -> Vec<Vec<(usize, Cell)>> {
    src.seek(&[0, total], &[0, 0]).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    let mut out = Vec::new();
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = src.window(&mut w).unwrap() as usize;
        if n == 0 {
            return out;
        }
        assert!(w.sel.is_null() && !w.is_answered(), "a plain window");
        let l = w.lanes()[0];
        assert_eq!(l.kind == lane::KIND_BYREF, byref);
        out.push(fill(src, &l, n));
    }
}

fn late_recipe(r: &GranuleRecipe, late: bool) -> GranuleRecipe {
    let r = r.clone().with_verdict_cell();
    r.verdicts.set(3, 1);
    r.verdicts.set(0, if late { VERDICT_ZONE | VERDICT_LATE } else { VERDICT_ZONE });
    r
}

/// Eager vs late over the whole relation: each window filled by `plan`'s selections in turn (an empty one = every row),
/// every filled row checked against the eager decode after each fill, and a fill of rows already decoded left unchanged.
fn check(recipe: &GranuleRecipe, list: Arc<PartList>, byref: bool) {
    let total = list.granules();
    let resolved = Resolved::for_recipe(recipe, list.clone());
    let bind = |r: &GranuleRecipe| {
        let mut s = GranuleRangeSource::new(r.clone());
        s.zone(&[], &[0], &[0]);
        s.bind(&binding_ref(&resolved)).unwrap();
        s
    };
    let mut eager = bind(&late_recipe(recipe, false));
    let want: Vec<Vec<Cell>> = windows(&mut eager, total, byref, &mut |_, l, n| (0..n).map(|i| (i, cell(l, i, byref))).collect()).into_iter().map(|w| w.into_iter().map(|(_, c)| c).collect()).collect();
    eager.unbind();
    assert!(byref || want.iter().flatten().any(|c| c.is_none()), "the fixture carries NULLs");
    let plans: [&dyn Fn(usize) -> Vec<Vec<u32>>; 3] = [
        &|n| vec![(0..n as u32).step_by(3).collect(), (0..n as u32).step_by(3).collect(), vec![]],
        &|n| vec![vec![1, 2, 63, 64, 65], vec![(n - 1) as u32], vec![1000.min(n as u32 - 1), 3000.min(n as u32 - 1)], (0..n as u32).rev().step_by(2).collect::<Vec<_>>().into_iter().rev().collect(), vec![]],
        &|_| vec![vec![]],
    ];
    for plan in plans {
        let mut late = bind(&late_recipe(recipe, true));
        let mut w = 0;
        let got = windows(&mut late, total, byref, &mut |src, l, n| {
            let expect = &want[w];
            w += 1;
            let mut seen = vec![false; n];
            for sel in plan(n) {
                let mut out = *l;
                assert_eq!(src.lane(0, &sel, &mut out).unwrap(), 1);
                let rows: Vec<u32> = if sel.is_empty() { (0..n as u32).collect() } else { sel };
                rows.iter().for_each(|&r| seen[r as usize] = true);
                for (i, s) in seen.iter().enumerate().filter(|(_, s)| **s) {
                    assert_eq!(cell(l, i, byref), expect[i], "window {} row {i} after filling {} rows", w - 1, rows.len());
                    let _ = s;
                }
            }
            (0..n).filter(|&i| seen[i]).map(|i| (i, cell(l, i, byref))).collect()
        });
        late.unbind();
        assert_eq!(got.len(), want.len());
    }
}

#[test]
fn a_late_delta_for_lane_with_nulls_fills_the_rows_asked_as_the_eager_decode() {
    let bf = build_fixture(int8_fixture("gravity_late_delta", 7331, 20_000, vec![ForcedPlan::DeltaFor], PartCutPolicy::default(), |i| (i % 9 != 4).then(|| 1_000_000_000_000 + (i * 3) as i64 - (i % 5) as i64)));
    check(&GranuleRecipe::for_test(7331, &[1], Arc::new(PartRegistry::new(64 << 20))), part_list(&bf), false);
}

#[test]
fn a_late_byte_for_lane_with_nulls_fills_the_rows_asked_as_the_eager_decode() {
    let bf = build_fixture(int8_fixture("gravity_late_bytefor", 7332, 20_000, vec![ForcedPlan::ByteFor { delta_width: 2, signed: true }], PartCutPolicy::default(), |i| (i % 7 != 3).then(|| 5_000_000 + (i % 40_000) as i64)));
    check(&GranuleRecipe::for_test(7332, &[1], Arc::new(PartRegistry::new(64 << 20))), part_list(&bf), false);
}

fn byref_recipe(rel: u32) -> GranuleRecipe {
    let mut r = GranuleRecipe::for_test(rel, &[1], Arc::new(PartRegistry::new(64 << 20)));
    r.kinds[0] = lane::KIND_BYREF;
    r
}

#[test]
fn a_late_dictionary_text_lane_read_by_reference_fills_the_rows_asked_as_the_eager_decode() {
    let bf: BuiltFixture = text_fixture();
    check(&byref_recipe(REL_TEXT), part_list(&bf), true);
}

/// A text column with no dictionary policy (stored verbatim), every seventh row NULL and every eleventh empty.
fn verbatim_nulls_fixture() -> BuiltFixture {
    use pgrc_qa::adapters::Probe;
    use pgrc_qa::corpus::{append_rows_with_sources, finish_with_sources, open_writer};
    use pgrc_write::publish::{effective_manifest, TxnVerdict};
    let rows = 20_000u64;
    let value = |i: u64| (i % 7 != 0).then(|| if i % 11 == 0 { Vec::new() } else { format!("v{:05}-{}", i * 13 % 9_973, "x".repeat((i % 29) as usize)).into_bytes() });
    let fx = Fixture {
        name: "gravity_late_verbatim",
        dir: "/qa/t7333".to_string(),
        spc: 1663,
        db: 5,
        relfilenumber: 7333,
        schema: vec![text_col(1)],
        oracle: vec![(0..rows).map(|i| value(i).map(OracleVal::Bytes)).collect()],
        plans: vec![],
        policy: PartCutPolicy::default(),
    };
    let cands = CodecCandidates::new(ColumnPosture::default());
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

#[test]
fn a_late_verbatim_text_lane_with_nulls_fills_its_images_as_the_eager_decode_and_keeps_earlier_fills_in_place() {
    let bf = verbatim_nulls_fixture();
    check(&byref_recipe(7333), part_list(&bf), true);
}

// ===== the whole-lane switch (q27 M1-REP): a fill of every row, or a lane whose fills turn dense, decodes the lane whole =====

/// The late source of `recipe` bound over `resolved` (which outlives it), the eager decode's cells per window, and the
/// relation's granules.
fn late_and_want(recipe: &GranuleRecipe, resolved: &Resolved, byref: bool) -> (GranuleRangeSource, Vec<Vec<Cell>>, u64) {
    let total = resolved.list(recipe.rel).expect("the recipe's relation").granules();
    let bind = |r: &GranuleRecipe| {
        let mut s = GranuleRangeSource::new(r.clone());
        s.zone(&[], &[0], &[0]);
        s.bind(&binding_ref(resolved)).unwrap();
        s
    };
    let mut eager = bind(&late_recipe(recipe, false));
    let want = windows(&mut eager, total, byref, &mut |_, l, n| (0..n).map(|i| (i, cell(l, i, byref))).collect()).into_iter().map(|w| w.into_iter().map(|(_, c)| c).collect()).collect();
    eager.unbind();
    (bind(&late_recipe(recipe, true)), want, total)
}

/// Block `b`'s rows (64 from `64 b`, inside `n`): all of them, or every `step`-th.
fn block(b: usize, n: usize, step: usize) -> Vec<u32> {
    (b * 64..((b + 1) * 64).min(n)).step_by(step).map(|r| r as u32).collect()
}

fn fill(src: &mut GranuleRangeSource, l: &Lane, sel: &[u32]) {
    let mut out = *l;
    assert_eq!(src.lane(0, sel, &mut out).unwrap(), 1);
}

/// H09: a by-value late lane's fills switch it to the whole decode at the first fill whose fitted cost (the calls, the
/// frames each one unpacked, the rows asked: `LaneDecode::gather_ns`) reaches the whole decode of the frames its span
/// reaches (`whole_ns`), once the span reaches the sample; before that it stays gathered. DeltaFor: a fill unpacks each
/// frame holding a row no earlier fill decoded, and decodes that frame whole.
#[test]
fn a_lane_whose_block_fills_turn_dense_mid_granule_switches_to_the_whole_decode_at_the_fitted_crossing() {
    use gravity_hosts::ops::scan::pgrc::granule_source::SAMPLE_ROWS;
    use gravity_program::cost::pgrc::LaneDecode;
    const FRAME: usize = 1024;
    let bf = build_fixture(int8_fixture("gravity_late_switch", 7334, 20_000, vec![ForcedPlan::DeltaFor], PartCutPolicy::default(), |i| (i % 9 != 4).then(|| 1_000_000_000_000 + (i * 3) as i64)));
    let recipe = GranuleRecipe::for_test(7334, &[1], Arc::new(PartRegistry::new(64 << 20)));
    let resolved = Resolved::for_recipe(&recipe, part_list(&bf));
    let (mut late, want, total) = late_and_want(&recipe, &resolved, false);
    let mut w = 0;
    windows(&mut late, total, false, &mut |src, l, n| {
        let expect = &want[w];
        w += 1;
        assert!(n > 2048, "a row past the two frames the fills touch");
        // a non-NULL row in a frame no fill reaches before the switch
        let far = (0..n).rev().find(|&i| expect[i].is_some()).unwrap();
        // rows [0, 1024) two a block (frame 0, decoded whole by the first), then whole blocks
        let plan: Vec<Vec<u32>> = (0..16).map(|b| block(b, n, 32)).chain((16..n.div_ceil(64)).map(|b| block(b, n, 1))).collect();
        let (mut calls, mut frames, mut asked, mut span, mut decoded) = (0u64, 0u64, 0u64, 0usize, vec![false; n.div_ceil(FRAME)]);
        let mut switched = None;
        for (i, sel) in plan.iter().enumerate() {
            fill(src, l, sel);
            let mut fresh: Vec<usize> = sel.iter().map(|&r| r as usize / FRAME).filter(|&f| !decoded[f]).collect();
            fresh.dedup();
            for &f in &fresh {
                decoded[f] = true;
            }
            (calls, frames, asked) = (calls + !fresh.is_empty() as u64, frames + fresh.len() as u64, asked + sel.len() as u64);
            span = span.max(*sel.last().unwrap() as usize + 1);
            let d = LaneDecode::DeltaFor;
            if span >= SAMPLE_ROWS && d.gather_ns(calls, frames, asked) >= d.whole_ns(8, span.next_multiple_of(FRAME).min(n) as u64) {
                switched = Some(i);
                break;
            }
            assert_ne!(cell(l, far, false), expect[far], "window {} fill {i}: under the crossing the lane stays gathered", w - 1);
        }
        let i = switched.expect("the whole blocks cross");
        assert!(i >= 16, "window {}: the sparse fills of frame 0 stay gathered (switched at fill {i})", w - 1);
        for r in 0..n {
            assert_eq!(cell(l, r, false), expect[r], "window {} row {r}: the lane is whole after the crossing", w - 1);
        }
        // done for the window: a later fill of any rows decodes nothing (the words stay)
        let words: Vec<u64> = (0..n).map(|i| unsafe { l.datum(i) as u64 }).collect();
        fill(src, l, &block(40, n, 1));
        fill(src, l, &[]);
        assert!((0..n).all(|i| unsafe { l.datum(i) as u64 } == words[i]), "window {}: no fill after the switch rewrites a row", w - 1);
        vec![]
    });
    late.unbind();
    assert_eq!(w, want.len());
}

/// H34 (kept: a late lane's switch to its whole decode is decided from the fills observed at run time, against the
/// selective decode's crossover): the near form that must not switch does not. Every row of the window's first `SAMPLE_ROWS / 2` asked
/// (all of its span, past any crossover) spans less than the sample, so the lane stays gathered: the asked rows decoded,
/// a row past them never; the fill that carries the span past the sample at a dense share switches it whole.
#[test]
fn a_dense_late_fill_over_a_span_shorter_than_the_sample_stays_gathered() {
    use gravity_hosts::ops::scan::pgrc::granule_source::SAMPLE_ROWS;
    let bf = build_fixture(int8_fixture("gravity_late_short", 7337, 20_000, vec![ForcedPlan::ByteFor { delta_width: 2, signed: true }], PartCutPolicy::default(), |i| (i % 11 != 5).then(|| 9_000_000 + (i * 7) as i64)));
    let recipe = GranuleRecipe::for_test(7337, &[1], Arc::new(PartRegistry::new(64 << 20)));
    let resolved = Resolved::for_recipe(&recipe, part_list(&bf));
    let (mut late, want, total) = late_and_want(&recipe, &resolved, false);
    let half = SAMPLE_ROWS / 2 / 64;
    let mut w = 0;
    windows(&mut late, total, false, &mut |src, l, n| {
        let expect = &want[w];
        w += 1;
        assert!(n > 2 * SAMPLE_ROWS, "rows past the sample");
        let far = (0..n).rev().find(|&i| expect[i].is_some()).unwrap();
        for b in 0..half {
            fill(src, l, &block(b, n, 1));
        }
        for i in 0..half * 64 {
            assert_eq!(cell(l, i, false), expect[i], "window {} row {i}: an asked row is decoded", w - 1);
        }
        assert_ne!(cell(l, far, false), expect[far], "window {}: a span under the sample stays gathered", w - 1);
        for b in half..2 * half {
            fill(src, l, &block(b, n, 1));
        }
        for i in 0..n {
            assert_eq!(cell(l, i, false), expect[i], "window {} row {i}: the lane is whole once the span reaches the sample", w - 1);
        }
        vec![]
    });
    late.unbind();
    assert_eq!(w, want.len());
}

#[test]
fn a_selective_late_lane_stays_gathered_over_the_whole_granule() {
    let bf = build_fixture(int8_fixture("gravity_late_selective", 7335, 20_000, vec![ForcedPlan::ByteFor { delta_width: 2, signed: true }], PartCutPolicy::default(), |i| (i % 7 != 3).then(|| 5_000_000 + (i % 40_000) as i64)));
    let recipe = GranuleRecipe::for_test(7335, &[1], Arc::new(PartRegistry::new(64 << 20)));
    let resolved = Resolved::for_recipe(&recipe, part_list(&bf));
    let (mut late, want, total) = late_and_want(&recipe, &resolved, false);
    let mut w = 0;
    windows(&mut late, total, false, &mut |src, l, n| {
        let expect = &want[w];
        w += 1;
        // one block in eight, 4 of its 64 rows: the calls and rows stay under the whole decode however far they reach
        // (H09: fills of every block would pay a call a block past it)
        for b in (0..n.div_ceil(64)).step_by(8) {
            fill(src, l, &block(b, n, 16));
        }
        let (mut asked, mut stale) = (0, 0);
        for i in (0..n).filter(|&i| expect[i].is_some()) {
            if i / 64 % 8 == 0 && i % 64 % 16 == 0 {
                assert_eq!(cell(l, i, false), expect[i], "window {} row {i}: an asked row is decoded", w - 1);
                asked += 1;
            } else {
                assert_ne!(cell(l, i, false), expect[i], "window {} row {i}: a row no fill asked is never decoded", w - 1);
                stale += 1;
            }
        }
        assert!(asked > 0 && stale > 0);
        vec![]
    });
    late.unbind();
    assert_eq!(w, want.len());
}

/// An image's bytes at `p` (a 1- or 4-byte varlena header), as `cell` reads them.
unsafe fn image(p: *const u8) -> Vec<u8> {
    let (hdr, len) = if *p & 1 == 1 { (1, (*p >> 1) as usize) } else { (4, (*(p as *const u32) >> 2) as usize) };
    core::slice::from_raw_parts(p.add(hdr), len - hdr).to_vec()
}

#[test]
fn a_by_reference_lane_switched_whole_keeps_the_images_earlier_fills_handed_out_and_never_redecodes() {
    let bf = verbatim_nulls_fixture();
    let recipe = byref_recipe(7333);
    let resolved = Resolved::for_recipe(&recipe, part_list(&bf));
    let (mut late, want, total) = late_and_want(&recipe, &resolved, true);
    let mut w = 0;
    windows(&mut late, total, true, &mut |src, l, n| {
        let expect = &want[w];
        w += 1;
        // sparse fills over [0, 1024): the images they hand out (address and bytes) as a consumer holds them
        let mut held = Vec::new();
        for b in 0..16 {
            let sel = block(b, n, 32);
            fill(src, l, &sel);
            for &r in &sel {
                if let Some(v) = &expect[r as usize] {
                    held.push((r as usize, unsafe { l.datum(r as usize) } as *const u8, v.clone()));
                }
            }
        }
        assert!(!held.is_empty());
        // two whole blocks cross 1/8: the lane decodes whole into the arena's next bytes
        fill(src, l, &block(16, n, 1));
        fill(src, l, &block(17, n, 1));
        for i in 0..n {
            assert_eq!(cell(l, i, true), expect[i], "window {} row {i}: the lane is whole after the crossing", w - 1);
        }
        for (r, p, v) in &held {
            assert_eq!(unsafe { image(*p) }, *v, "window {} row {r}: an image handed out before the switch is intact", w - 1);
        }
        // a straight body's fill of every row after the switch decodes nothing: every row keeps its image
        let words: Vec<u64> = (0..n).map(|i| unsafe { l.datum(i) as u64 }).collect();
        fill(src, l, &[]);
        fill(src, l, &block(3, n, 1));
        assert!((0..n).all(|i| unsafe { l.datum(i) as u64 } == words[i]), "window {}: no fill after the switch moves an image", w - 1);
        vec![]
    });
    late.unbind();
    assert_eq!(w, want.len());
}

#[test]
fn a_straight_body_fill_of_every_row_decodes_the_lane_whole_once() {
    let bf = build_fixture(int8_fixture("gravity_late_straight", 7336, 20_000, vec![ForcedPlan::DeltaFor], PartCutPolicy::default(), |i| (i % 9 != 4).then(|| 7_000_000 + (i * 5) as i64)));
    let recipe = GranuleRecipe::for_test(7336, &[1], Arc::new(PartRegistry::new(64 << 20)));
    let resolved = Resolved::for_recipe(&recipe, part_list(&bf));
    let (mut late, want, total) = late_and_want(&recipe, &resolved, false);
    let mut w = 0;
    windows(&mut late, total, false, &mut |src, l, n| {
        let expect = &want[w];
        w += 1;
        // one sparse fill first (frame 0 gathered), then the straight body's fill of every row
        fill(src, l, &[5, 700]);
        fill(src, l, &[]);
        for i in 0..n {
            assert_eq!(cell(l, i, false), expect[i], "window {} row {i}", w - 1);
        }
        let words: Vec<u64> = (0..n).map(|i| unsafe { l.datum(i) as u64 }).collect();
        fill(src, l, &[]);
        assert!((0..n).all(|i| unsafe { l.datum(i) as u64 } == words[i]));
        vec![]
    });
    late.unbind();
    assert_eq!(w, want.len());
}
