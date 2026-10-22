//! decode.packed on `GranuleRangeSource`: in a packed window a candidate lane's `PackedFrames` reads back the eager decode
//! (`reference + delta` at the stream's delta width, every frame, NULLs included), `fmax` bounds every valid value, frame f
//! holds rows `[1024 f, 1024 (f + 1))` (so a 64-row block never straddles two), and the lane fills as a late lane does,
//! leaving its table intact. A stream the block path cannot read (BYTE_FOR at width 4) is never packed.

use std::sync::Arc;

use gravity_abi::Lane;
use gravity_driver::{blank_site, Source, Window};
use gravity_hosts::ops::scan::pgrc::granule_source::{packed_census, GranuleRangeSource, GranuleRecipe, PartList, Resolved};
use gravity_abi::site::{PackedFrames, LANE_PACKED};
use gravity_program::protos::{VERDICT_PACKED, VERDICT_PACKED_ZONE};
use gravity_program::protos::VERDICT_ZONE;
use gravity_program::{ZoneOp, ZonePred};
use pgrc_format::class::{ColSchema, CollationClass, StorageClass, TypeSemantics};
use pgrc_qa::adapters::ForcedPlan;
use pgrc_qa::corpus::{build_fixture, int8_fixture, Fixture, OracleVal};
use pgrc_read::PartRegistry;
use pgrc_write::writer::PartCutPolicy;

#[path = "common/granule_fixtures.rs"]
#[allow(dead_code)]
mod granule_fixtures;
use granule_fixtures::*;

const ROWS: u64 = 20_000;

fn recipe(r: &GranuleRecipe, packed: bool) -> GranuleRecipe {
    recipe_with(r, if packed { VERDICT_ZONE | VERDICT_PACKED } else { VERDICT_ZONE })
}

fn recipe_with(r: &GranuleRecipe, enables: u64) -> GranuleRecipe {
    let r = r.clone().with_verdict_cell();
    r.verdicts.set(4, 1);
    r.verdicts.set(0, enables);
    r
}

/// Every window over the relation: `f` sees the source and the window's lane 0 and returns what it read.
fn windows<T>(src: &mut GranuleRangeSource, total: u64, f: &mut dyn FnMut(&mut GranuleRangeSource, &Lane, usize) -> T) -> Vec<T> {
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
        out.push(f(src, &l, n));
    }
}

fn values(l: &Lane, n: usize) -> Vec<Option<i64>> {
    (0..n).map(|i| unsafe { l.is_valid(i).then(|| l.datum(i) as i64) }).collect()
}

/// The table's frames against the eager values: count, row ranges, width, `reference + delta` and `fmax`. `zext`: an
/// unsigned 4-byte lane, whose eager datum reads back sign-extended and whose frames hold the zero-extended word.
unsafe fn check_table(t: &PackedFrames, want: &[Option<i64>], width: u64, zext: bool) {
    let rows = want.len();
    assert_eq!(t.n as usize, rows.div_ceil(1024));
    for (f, fr) in t.frames[..t.n as usize].iter().enumerate() {
        assert_eq!((fr.first_row, fr.width), (1024 * f as u64, width), "frame {f}");
        for (r, v) in want.iter().enumerate().skip(1024 * f).take(1024) {
            let p = (fr.deltas as usize + (r - 1024 * f) * width as usize) as *const u8;
            let delta = match width {
                0 => 0,
                1 => *p as i64,
                _ => u16::from_le_bytes([*p, *p.add(1)]) as i64,
            };
            if let Some(v) = v.map(|v| if zext { v as u32 as i64 } else { v }) {
                assert_eq!(fr.reference + delta, v, "frame {f} row {r}");
                assert!(fr.fmax >= v, "frame {f} row {r}: fmax {} under {v}", fr.fmax);
            }
        }
    }
}

/// Eager vs packed over the whole relation; `width` None = no window may be packed.
fn check(base: &GranuleRecipe, list: Arc<PartList>, width: Option<u64>) {
    check_zext(base, list, width, false)
}

fn check_zext(base: &GranuleRecipe, list: Arc<PartList>, width: Option<u64>, zext: bool) {
    let total = list.granules();
    let resolved = Resolved::for_recipe(base, list.clone());
    let bind = |r: &GranuleRecipe| {
        let mut s = GranuleRangeSource::new(r.clone());
        s.zone(&[], &[0], &[0]);
        s.bind(&binding_ref(&resolved)).unwrap();
        s
    };
    let mut eager = bind(&recipe(base, false));
    let want = windows(&mut eager, total, &mut |_, l, n| values(l, n));
    eager.unbind();
    assert!(want.iter().flatten().any(|v| v.is_none()), "the fixture carries NULLs");
    let mut packed = bind(&recipe(base, true));
    let before = packed_census();
    let mut w = 0;
    windows(&mut packed, total, &mut |src, l, n| {
        let want = &want[w];
        w += 1;
        let mut out = *l;
        let off = src.lane(LANE_PACKED as u16, &[], &mut out).unwrap() as usize;
        let Some(width) = width else {
            assert_eq!(off, 0, "window {} packed", w - 1);
            assert_eq!(&values(l, n), want);
            return;
        };
        assert_ne!(off, 0, "window {} not packed", w - 1);
        assert!((0..n).all(|i| unsafe { l.is_valid(i) } == want[i].is_some()), "validity is exact before any fill");
        let t = unsafe { &*((l.ptr as usize + off) as *const PackedFrames) };
        unsafe { check_table(t, want, width, zext) };
        let some: Vec<u32> = (0..n as u32).step_by(97).collect();
        assert_eq!(src.lane(0, &some, &mut out).unwrap(), 1);
        let got = values(l, n);
        assert!(some.iter().all(|&r| got[r as usize] == want[r as usize]), "the rows filled");
        assert_eq!(src.lane(0, &[], &mut out).unwrap(), 1);
        assert_eq!(&values(l, n), want);
        unsafe { check_table(t, want, width, zext) };
    });
    packed.unbind();
    assert_eq!(w, want.len());
    let after = packed_census();
    let counted = if width.is_some() { after[0] - before[0] } else { after[1] - before[1] };
    assert!(counted >= w as u64, "the census counted every window ({counted} of {w})");
}

fn int8(rel: u32, plan: Option<ForcedPlan>, f: impl Fn(u64) -> Option<i64>) -> (GranuleRecipe, Arc<PartList>) {
    let bf = build_fixture(int8_fixture("gravity_packed", rel as u64, ROWS, plan.into_iter().collect(), PartCutPolicy::default(), f));
    (GranuleRecipe::for_test(rel, &[1], Arc::new(PartRegistry::new(64 << 20))), part_list(&bf))
}

#[test]
fn a_width_1_byte_for_lane_with_nulls_publishes_frames_that_read_back_the_eager_decode() {
    let (r, list) = int8(7341, Some(ForcedPlan::ByteFor { delta_width: 1, signed: true }), |i| (i % 9 != 4).then(|| -5_000_000_000 + (i % 200) as i64));
    check(&r, list, Some(1));
}

#[test]
fn a_width_2_byte_for_lane_with_nulls_publishes_frames_that_read_back_the_eager_decode() {
    let (r, list) = int8(7342, Some(ForcedPlan::ByteFor { delta_width: 2, signed: true }), |i| (i % 7 != 3).then(|| 5_000_000 - (i % 40_000) as i64));
    check(&r, list, Some(2));
}

#[test]
fn a_const_lane_with_nulls_publishes_width_0_frames_of_its_value() {
    let (r, list) = int8(7343, None, |i| (i % 5 != 0).then_some(-42));
    check(&r, list, Some(0));
}

#[test]
fn a_width_4_byte_for_lane_is_never_packed_and_decodes_as_before() {
    let (r, list) = int8(7344, Some(ForcedPlan::ByteFor { delta_width: 4, signed: true }), |i| (i % 9 != 4).then(|| (i * 1_000_003 % 3_000_000_000) as i64));
    check(&r, list, None);
}

/// An int4 column crossing zero inside frames: the reference and every value are sign-extended words.
#[test]
fn a_signed_int4_lane_crossing_zero_publishes_sign_extended_references() {
    let rel = 7345u32;
    let v = |i: u64| (i % 11 != 6).then(|| -30_000 + (i % 50_000) as i32 - (i % 3) as i32);
    let col = ColSchema { attno: 1, class: StorageClass::ByvalWord { width: 4, signed: true }, typlen: 4, typbyval: true, typalign: b'i', collation_class: CollationClass::C, semantics: TypeSemantics::SignedInt };
    let fx = Fixture {
        name: "gravity_packed_int4",
        dir: format!("/qa/t{rel}"),
        spc: 1663,
        db: 5,
        relfilenumber: rel as u64,
        schema: vec![col],
        oracle: vec![(0..ROWS).map(|i| v(i).map(|x| OracleVal::Word(x as i64 as u64))).collect()],
        plans: vec![ForcedPlan::ByteFor { delta_width: 2, signed: true }],
        policy: PartCutPolicy::default(),
    };
    let bf = build_fixture(fx);
    let r = GranuleRecipe::for_test_widths(rel, &[1], &[4], Arc::new(PartRegistry::new(64 << 20)));
    check(&r, part_list(&bf), Some(2));
}

/// An oid column (an unsigned stream: zero-extended words) whose frames straddle 2^31, then sit above it: every window
/// packs, each frame's reference and values are the zero-extended words (the eager lane's 4-byte values), and `fmax`
/// bounds the values past 2^31 - 1.
#[test]
fn an_unsigned_oid_lane_past_2_31_publishes_zero_extended_frames() {
    let rel = 7346u32;
    let v = |i: u64| (i % 13 != 5).then(|| if i < 10_240 { 2_147_453_648 + i * 7919 % 60_000 } else { 4_294_900_000 + i * 7919 % 60_000 });
    let col = ColSchema { attno: 1, class: StorageClass::ByvalWord { width: 4, signed: false }, typlen: 4, typbyval: true, typalign: b'i', collation_class: CollationClass::C, semantics: TypeSemantics::UnsignedInt };
    let profile = pgrc_meta::profile::MetaProfile::derive_or_opaque(col.class, col.collation_class, col.semantics);
    let fx = Fixture {
        name: "gravity_packed_oid",
        dir: format!("/qa/t{rel}"),
        spc: 1663,
        db: 5,
        relfilenumber: rel as u64,
        schema: vec![col],
        oracle: vec![(0..ROWS).map(|i| v(i).map(OracleVal::Word)).collect()],
        plans: vec![ForcedPlan::ByteFor { delta_width: 2, signed: false }],
        policy: PartCutPolicy::default(),
    };
    let bf = build_fixture(fx);
    let r = GranuleRecipe::for_test_widths(rel, &[1], &[4], Arc::new(PartRegistry::new(64 << 20))).with_profile(0, profile);
    check_zext(&r, part_list(&bf), Some(2), true);
}

/// FRAME lever 1 (a), decode.packed's host half: a Mixed window whose every zone lane is a packed candidate the first op
/// alone reads (VERDICT_PACKED_ZONE), with no PSMA window and no condition cache, is packed and plain: the host neither selects (row_select would keep the few rows under
/// the bound) nor decodes the zone lane first (its slot holds nothing of the granule until it fills late), and the fill
/// reads the eager values. A second lane of the column, no candidate, gives the eager path a rest lane to select for.
#[test]
fn a_zone_lane_that_is_a_packed_candidate_is_neither_decoded_first_nor_selected() {
    let rel = 7346u32;
    let v = |i: u64| (i % 7 != 3).then(|| 5_000_000 - (i % 40_000) as i64);
    let bf = build_fixture(int8_fixture("gravity_packed_zone", rel as u64, ROWS, vec![ForcedPlan::ByteFor { delta_width: 2, signed: true }], PartCutPolicy::default(), v));
    let list = part_list(&bf);
    assert_eq!(list.len(), 1, "one part: a granule's rows are 8192 g + r");
    let total = list.granules();
    let base = GranuleRecipe::for_test(rel, &[1, 1], Arc::new(PartRegistry::new(64 << 20)));
    let resolved = Resolved::for_recipe(&base, list.clone());
    // granule 0 holds 5 000 000 down to 4 991 809: under 4 991 900 are its last ~90 rows (Mixed, few enough to select);
    // granules 1 and 2 lie wholly under it (AllMatch)
    let preds = [ZonePred::word(0, ZoneOp::Lt, 4_991_900)];
    let run = |enables: u64| {
        let mut src = GranuleRangeSource::new(recipe_with(&base, enables));
        src.zone(&preds, &[0], &[0]);
        src.bind(&binding_ref(&resolved)).unwrap();
        src.seek(&[0, total], &[0, 0]).unwrap();
        let mut lanes = [Lane::NONE; MAX_LANES];
        let mut out = Vec::new();
        loop {
            let mut site = blank_site(lanes.as_mut_ptr());
            let mut w = Window::over(&mut site);
            let n = src.window(&mut w).unwrap() as usize;
            if n == 0 {
                break;
            }
            let g = w.base().granule as u64;
            // the rows the program reads: a selection's (D3223 S1: its zone lane, compared on the stored deltas, is gathered
            // for the selection alone), else the window's
            let rows: Vec<u64> = if w.sel.is_null() { (0..n as u64).collect() } else { unsafe { std::slice::from_raw_parts(w.sel, w.sel_n as usize) }.iter().map(|&r| r as u64).collect() };
            let want: Vec<Option<i64>> = rows.iter().map(|&r| v(8192 * g + r)).collect();
            let l0 = w.lanes()[0];
            let mut o = l0;
            let off = src.lane(LANE_PACKED as u16, &[], &mut o).unwrap() as usize;
            let at = |l: &Lane| rows.iter().map(|&r| unsafe { l.is_valid(r as usize).then(|| l.datum(r as usize) as i64) }).collect::<Vec<_>>();
            let before = at(&l0) == want;
            assert_eq!(src.lane(0, &[], &mut o).unwrap(), 1);
            out.push((g, w.sel.is_null(), off != 0, before, at(&l0) == want));
        }
        src.unbind();
        out
    };
    assert_eq!(run(VERDICT_ZONE), vec![(0, false, false, true, true), (1, true, false, true, true), (2, true, false, true, true)], "no candidates: the host selects granule 0 (its zone lane's selected rows read)");
    // a candidate some later op or the end reads (no VERDICT_PACKED_ZONE): as before 1 a, the Mixed granule decodes its
    // zone lane first and selects, and the AllMatch ones pack with the lane decoded too
    assert_eq!(run(VERDICT_ZONE | VERDICT_PACKED), vec![(0, false, false, true, true), (1, true, true, true, true), (2, true, true, true, true)], "1a off: granule 0 selects");
    let before = packed_census();
    assert_eq!(run(VERDICT_ZONE | VERDICT_PACKED | VERDICT_PACKED_ZONE), vec![(0, true, true, false, true), (1, true, true, false, true), (2, true, true, false, true)], "packed and plain, the zone lane undecoded until it fills");
    assert!(packed_census()[0] - before[0] >= 3, "the census counted the packed windows");
}
