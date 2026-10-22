//! The tests that assert on a delta of a process-global `*_census()`: this binary is their own process, and each takes `SERIAL` before it runs a recipe.

use std::sync::{Arc, Mutex};

use gravity_abi::Lane;
use gravity_driver::{blank_site, Source, Window};
use gravity_hosts::ops::scan::pgrc::granule_source::{GranuleRangeSource, GranuleRecipe, Resolved};
use gravity_program::{ZoneOp, ZonePred};
use pgrc_read::PartRegistry;

#[path = "common/granule_fixtures.rs"]
mod granule_fixtures;
use granule_fixtures::*;

static SERIAL: Mutex<()> = Mutex::new(());

#[test]
fn a_sparse_recipe_opens_its_dictionaries_block_by_block_and_the_image_is_the_parts() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    use gravity_hosts::ops::scan::pgrc::granule_source::dict_census;
    let bf = text_fixture();
    let list = part_list(&bf);
    let total = list.granules();
    // one window through the code lane opens the part's dictionary; the first value rendered faults its payload
    let touch = |recipe: GranuleRecipe| {
        let resolved = Resolved::for_recipe(&recipe, list.clone());
        let mut src = GranuleRangeSource::new(recipe);
        src.bind(&binding_ref(&resolved)).unwrap();
        src.seek(&[0, total], &[0, 0]).unwrap();
        let mut lanes = [Lane::NONE; MAX_LANES];
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        assert!(src.window(&mut w).unwrap() > 0);
        let before = dict_census();
        let (_, len) = src.dict_value(0, 0, 1).unwrap();
        assert!(len > 0);
        let after = dict_census();
        (after[0] - before[0], after[1] - before[1])
    };
    let (whole, blocks) = touch(GranuleRecipe::for_test_codes(REL_TEXT, &[1], &[1], Arc::new(PartRegistry::new(64 << 20))).with_sparse_dicts());
    assert!(blocks >= 1 && whole == 0, "the sparse recipe faults the block the entry lives in: whole={whole} blocks={blocks}");
    // a later plain open of the same part serves from the resident image: no fault of either kind
    let (whole, blocks) = touch(GranuleRecipe::for_test_codes(REL_TEXT, &[1], &[1], Arc::new(PartRegistry::new(64 << 20))));
    assert_eq!((whole, blocks), (0, 0), "the image is the part's");
}

/// P5-18: a sparse site carries the bound on its renders; each part's dictionary opens whole where one stream of it costs
/// less than the blocks the part's share of the bound touches, else for probes with the bytes-touched election live (and so
/// does every part of a site with no bound); a site that declares its probes (no renders: the entries window, the heads)
/// stays at block grain whatever it renders. The 49-block wrapped dictionary is in memory, the cold regime: one render's
/// block read costs less than the stream, and three faults cost more.
#[test]
fn a_site_opens_its_dictionaries_by_the_bound_on_its_renders_and_a_declared_probe_site_stays_at_block_grain() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    use gravity_hosts::ops::scan::pgrc::granule_source::dict_census;
    let run = |name: &'static str, rel: u32, renders: Option<u64>| {
        let bf = wide_text_fixture(name, rel);
        let list = part_list_at(&bf, rel as u64);
        let total = list.granules();
        let recipe = GranuleRecipe::for_test_codes(rel, &[1], &[1], Arc::new(PartRegistry::new(64 << 20))).with_sparse_dicts();
        let recipe = match renders {
            Some(n) => recipe.with_dict_renders(n),
            None => recipe,
        };
        let resolved = Resolved::for_recipe(&recipe, list.clone());
        let mut src = GranuleRangeSource::new(recipe);
        src.bind(&binding_ref(&resolved)).unwrap();
        src.seek(&[0, total], &[0, 0]).unwrap();
        let mut lanes = [Lane::NONE; MAX_LANES];
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        assert!(src.window(&mut w).unwrap() > 0);
        let before = dict_census();
        // code 1,024 b is the first entry of block b
        for b in 0..5u32 {
            let (_, len) = src.dict_value(0, 0, b * 1024).unwrap();
            assert_eq!(len, 16, "code {}", b * 1024);
        }
        let after = dict_census();
        (after[0] - before[0], after[1] - before[1])
    };
    let (whole, blocks) = run("gravity_p518_declared", 7311, None);
    assert_eq!((whole, blocks), (0, 5), "a declared probe site faults the five blocks it renders from and never streams");
    let (whole, blocks) = run("gravity_p518_unbounded", 7312, Some(u64::MAX));
    assert!(whole > 0 && (1..5).contains(&blocks), "a site with no bound opens for probes, and the election streams the rest before the fifth render: whole={whole} blocks={blocks}");
    let (whole, blocks) = run("gravity_p518_one", 7313, Some(1));
    assert!(whole > 0 && (1..5).contains(&blocks), "a bound of one render opens for probes as well: whole={whole} blocks={blocks}");
    let (whole, blocks) = run("gravity_p518_many", 7314, Some(1_000_000));
    assert!(whole > 0 && blocks == 0, "a bound past a stream's cost opens the dictionary whole: whole={whole} blocks={blocks}");
}

/// The per-lane verdict: a filter-only lane whose own predicate holds over a `Mixed` granule stays
/// undecoded and carries `FLAG_LANE_ALL_MATCH`; the lane the granule is `Mixed` on decodes as before.
#[test]
fn a_filter_only_lane_stays_undecoded_where_its_own_predicate_holds_inside_a_mixed_granule() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    use gravity_abi::ctx::lane as lf;
    use gravity_hosts::ops::scan::pgrc::granule_source::lane_elided_census;
    let bf = sorted_fixture();
    let list = part_list(&bf);
    let total = list.granules();
    // lane 0: `a < 3·GRANULE` (granules 0–2 AllMatch, the rest NoneMatch); lane 1: `a < 2·GRANULE + 100` (granule 2 Mixed)
    let preds = [ZonePred::word(0, ZoneOp::Lt, 3 * GRANULE as i64), ZonePred::word(1, ZoneOp::Lt, 2 * GRANULE as i64 + 100)];
    let walk = |recipe: GranuleRecipe| {
        let resolved = Resolved::for_recipe(&recipe, list.clone());
        let mut src = GranuleRangeSource::new(recipe);
        src.zone(&preds, &[0b1], &[0]);
        src.bind(&binding_ref(&resolved)).unwrap();
        src.seek(&[0, total], &[0, 0]).unwrap();
        let mut lanes = [Lane::NONE; MAX_LANES];
        let mut out: Vec<(u32, bool, bool, u8, u8)> = Vec::new();
        loop {
            let mut site = blank_site(lanes.as_mut_ptr());
            let mut w = Window::over(&mut site);
            if src.window(&mut w).unwrap() == 0 {
                break;
            }
            let (l0, l1) = (w.lanes()[0], w.lanes()[1]);
            out.push((w.base().granule, l0.ptr.is_null(), l1.ptr.is_null(), l0.flags & (lf::VERDICT_MASK | lf::FLAG_LANE_ALL_MATCH), l1.flags & (lf::VERDICT_MASK | lf::FLAG_LANE_ALL_MATCH)));
        }
        out
    };
    let all = lf::VERDICT_ALL_MATCH | lf::FLAG_LANE_ALL_MATCH;
    let before = lane_elided_census();
    let elide = walk(GranuleRecipe::for_test(REL_SORTED, &[1, 1], Arc::new(PartRegistry::new(64 << 20))).with_lane_elide());
    assert_eq!(elide, vec![(0, true, false, all, all), (1, true, false, all, all), (2, true, false, lf::VERDICT_MIXED | lf::FLAG_LANE_ALL_MATCH, lf::VERDICT_MIXED)]);
    assert_eq!(lane_elided_census() - before, 1, "one lane-granule elided inside the Mixed granule");
    // without the recipe's opt-in the Mixed granule decodes both lanes; the flag still names the lane whose predicate holds
    let plain = walk(GranuleRecipe::for_test(REL_SORTED, &[1, 1], Arc::new(PartRegistry::new(64 << 20))));
    assert_eq!(plain[2], (2, false, false, lf::VERDICT_MIXED | lf::FLAG_LANE_ALL_MATCH, lf::VERDICT_MIXED));
    assert_eq!(lane_elided_census() - before, 1);
}

/// One execution of a condition-cache recipe (rel `rel`, verdict lane 2, fingerprint `fp`, `enables`) under `preds` on
/// lane 0, the test playing the program: on a miss window it sets the verdict lane's bit of each presented row whose value
/// `pass` keeps (the frame's `passed`, through the selection as lowerv4 frame/site.rs `row_index` maps it).
struct CondRun {
    /// Per window: (part, granule, the presented rows, the verdict lane's words on a hit that selects).
    wins: Vec<(u32, u32, Vec<u32>, Option<Vec<u64>>)>,
    /// `cond_cache::census` delta [hits, published].
    cond: [u64; 2],
    /// `verdict_census` delta [narrowed, cond skips].
    verdicts: [u64; 2],
    /// Windows whose lane 0 came undecoded (elided).
    undecoded: usize,
}

fn lane_value(l: &Lane, i: u32) -> i64 {
    // SAFETY: row `i` of a presented window lies inside the lane's decoded rows at its width.
    unsafe {
        let p = l.ptr.add(i as usize * l.width as usize);
        match l.width {
            1 => *(p as *const i8) as i64,
            2 => (p as *const i16).read_unaligned() as i64,
            4 => (p as *const i32).read_unaligned() as i64,
            _ => (p as *const i64).read_unaligned(),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn cond_run(rel: u32, list: &Arc<gravity_hosts::ops::scan::pgrc::granule_source::PartList>, registry: &Arc<PartRegistry>, profile: &pgrc_meta::profile::MetaProfile, lanes: (&[i16], u64), preds: &[ZonePred], enables: u64, fp: u64, pass: &dyn Fn(&[i64]) -> bool) -> CondRun {
    use gravity_abi::ctx::lane as lf;
    use gravity_hosts::ops::scan::pgrc::cond_cache;
    use gravity_hosts::ops::scan::pgrc::granule_source::verdict_census;
    guc_tables::backing::set_pgrust_condition_cache(guc_tables::ConditionCacheMode::Full as i32);
    let (attnos, filter_only) = lanes;
    let mut recipe = GranuleRecipe::for_test(rel, attnos, registry.clone()).with_verdict_cell().with_verdict_lane(2);
    for k in 0..attnos.len() {
        recipe = recipe.with_profile(k, profile.clone());
    }
    recipe.verdicts.set(1, fp);
    recipe.verdicts.set(2, 9);
    recipe.verdicts.set(0, enables);
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.zone(preds, &[filter_only], &[0]);
    src.bind(&binding_ref(&resolved)).unwrap();
    let (c0, v0) = (cond_cache::census(), verdict_census());
    src.seek(&[0, list.granules()], &[0, 0]).unwrap();
    let (mut lanes, mut wins, mut undecoded) = ([Lane::NONE; MAX_LANES], Vec::new(), 0);
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = src.window(&mut w).unwrap();
        if n == 0 {
            break;
        }
        // SAFETY: a window's selection holds `sel_n` rows.
        let rows: Vec<u32> = if w.sel.is_null() { (0..n).collect() } else { unsafe { std::slice::from_raw_parts(w.sel, w.sel_n as usize) }.to_vec() };
        let (attrs, v) = (w.lanes()[..attnos.len()].to_vec(), w.lanes()[2]);
        undecoded += attrs[0].ptr.is_null() as usize;
        let hit = !v.ptr.is_null() && v.flags & lf::VERDICT_MASK == lf::VERDICT_ALL_MATCH;
        // SAFETY: the verdict lane holds a granule's words.
        let words = (hit && !w.sel.is_null()).then(|| unsafe { std::slice::from_raw_parts(v.ptr as *const u64, (n as usize).div_ceil(64)) }.to_vec());
        if !v.ptr.is_null() && !hit {
            for &i in rows.iter().filter(|&&i| pass(&attrs.iter().map(|a| lane_value(a, i)).collect::<Vec<i64>>())) {
                // SAFETY: the miss window's verdict lane is the host's words for its granule.
                unsafe { *(v.ptr as *mut u64).add(i as usize / 64) |= 1 << (i % 64) };
            }
        }
        wins.push((w.base().part_id, w.base().granule, rows, words));
    }
    src.unbind();
    let (c1, v1) = (cond_cache::census(), verdict_census());
    CondRun { wins, cond: [c1[0] - c0[0], c1[2] - c0[2]], verdicts: [v1[2] - v0[2], v1[3] - v0[3]], undecoded }
}

fn int8_profile() -> pgrc_meta::profile::MetaProfile {
    let c = pgrc_qa::int8_col(1);
    pgrc_meta::profile::MetaProfile::derive_or_opaque(c.class, c.collation_class, c.semantics)
}

/// The oracle: every row of every granule of `list` (lane 0 = attno 1), no zone, no cache, keyed (part, granule).
fn granule_values(rel: u32, list: &Arc<gravity_hosts::ops::scan::pgrc::granule_source::PartList>, registry: &Arc<PartRegistry>, profile: &pgrc_meta::profile::MetaProfile) -> std::collections::BTreeMap<(u32, u32), Vec<i64>> {
    let recipe = GranuleRecipe::for_test(rel, &[1], registry.clone()).with_profile(0, profile.clone());
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&resolved)).unwrap();
    src.seek(&[0, list.granules()], &[0, 0]).unwrap();
    let (mut lanes, mut out) = ([Lane::NONE; MAX_LANES], std::collections::BTreeMap::new());
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = src.window(&mut w).unwrap();
        if n == 0 {
            break;
        }
        assert!(w.sel.is_null());
        out.insert((w.base().part_id, w.base().granule), (0..n).map(|i| lane_value(&w.lanes()[0], i)).collect());
    }
    out
}

/// An equality on an unsorted key (D278 follow-up): a Mixed granule PSMA narrows to a window none of whose rows passes is cached NoMatch,
/// the rows outside the window failing the zone predicates, so the next execution skips it on the cache; a window row
/// that passes publishes the program's words (TH14 L1); a delete mask on the granule publishes nothing.
#[test]
fn a_psma_window_with_no_passing_row_is_cached_none_and_a_passing_row_or_a_delete_mask_is_not() {
    use gravity_hosts::ops::scan::pgrc::granule_source::{PartIoSpec, PartList, PartSpec};
    use gravity_program::protos::{VERDICT_COND, VERDICT_PSMA, VERDICT_ZONE};
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // a = 2i sorted: part 0 holds granules 0..3, granule 2 the even values [32 768, 49 150]
    let policy = pgrc_write::writer::PartCutPolicy { max_rows: 20_000, ..Default::default() };
    let bf = pgrc_qa::corpus::build_fixture(pgrc_qa::corpus::int8_fixture("gravity_cond_psma", 7321, ROWS, vec![], policy, |i| Some(2 * i as i64)));
    let plain = part_list(&bf);
    // the same parts, part 0 with its granule 2's row 0 deleted
    let deleted = Arc::new(PartList::new(bf.manifest.parts.iter().enumerate().map(|(i, rec)| {
        let bytes: Arc<[u8]> = Arc::from(bf.files.get(&pgrc_format::dirlayout::part_file_name(rec.part_no)).expect("part file").as_slice());
        let dv = (i == 0).then(|| pgrc_format::dml::encode_dv(rec.part_no, 1, &[(2, pgrc_format::dml::DvBlockKind::List, &[0u16][..])]).unwrap());
        let deletes = dv.map(|d| Arc::new(pgrc_scan::PartDeletes::from_dv_payload(&d, rec.part_no, 1, rec.granule_count).unwrap()));
        PartSpec { io: PartIoSpec::Mem { bytes, dev: 7, ino: 600 + i as u64 }, expect: pgrc_read::PartExpect::none(), rows: rec.rows, granules: rec.granule_count, deletes, part: None }
    }).collect()));
    let profile = int8_profile();
    let on = VERDICT_ZONE | VERDICT_PSMA | VERDICT_COND;
    // one execution under `a = v`: (rows presented, [hits, published], [narrowed, cond skips])
    let run = |list: &Arc<PartList>, registry: &Arc<PartRegistry>, v: i64, fp: u64| {
        let r = cond_run(4252, list, registry, &profile, (&[1], 0), &[ZonePred::word(0, ZoneOp::Eq, v)], on, fp, &|a| a[0] == v);
        (r.wins.iter().map(|w| w.2.len() as u64).sum::<u64>(), r.cond, r.verdicts)
    };
    // absent (odd) value inside granule 2's window: published NoMatch, then a cache skip
    let r = Arc::new(PartRegistry::new(64 << 20));
    let (rows, c, v) = run(&plain, &r, 40_001, 0x5eed_0101);
    assert_eq!((rows, c[1], v[1]), (0, 1, 0), "the empty window published NoMatch");
    assert!(v[0] >= 1, "PSMA narrowed granule 2");
    let (rows, c, v) = run(&plain, &r, 40_001, 0x5eed_0101);
    assert_eq!((rows, c, v), (0, [1, 0], [0, 1]), "the next execution skips it on the cache (asked before PSMA)");
    // a present value: the staged window publishes the program's one bit, and the next execution selects that row from it
    let (rows, c, _) = run(&plain, &r, 40_002, 0x5eed_0102);
    assert_eq!((rows, c[1]), (1, 1));
    assert_eq!(run(&plain, &r, 40_002, 0x5eed_0102), (1, [1, 0], [0, 0]), "a hit narrows nothing: PSMA runs on a miss alone");
    // a delete mask on granule 2: nothing published, never a hit
    let r = Arc::new(PartRegistry::new(64 << 20));
    assert_eq!(run(&deleted, &r, 40_001, 0x5eed_0103).1, [0, 0]);
    assert_eq!(run(&deleted, &r, 40_001, 0x5eed_0103).1, [0, 0]);
}

/// The hit selection memo (TH06 / TH14, main 09-24): a sparse granule's published words keep its survivors as a row
/// list, and the next execution's hit selects exactly those rows from it; the same part under a delete mask hits the same
/// entry but selects from the words with the mask applied (the list never carries a delete: the deleted row 0 is gone).
/// `a = 2i` sorted: part 0's granule 2 holds [32 768, 39 998]; the covered quals keep a < 39 000 and a % 32 = 0 there,
/// 195 of its 3 616 rows (sparse: at most 1 / 8), row 0 among them; the zone range makes the granule Mixed so a miss
/// publishes.
#[test]
fn a_sparse_hit_selects_from_the_kept_list_and_a_delete_mask_takes_the_words() {
    use gravity_hosts::ops::scan::pgrc::granule_source::{PartIoSpec, PartList, PartSpec};
    use gravity_program::protos::{VERDICT_COND, VERDICT_ZONE};
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let policy = pgrc_write::writer::PartCutPolicy { max_rows: 20_000, ..Default::default() };
    let bf = pgrc_qa::corpus::build_fixture(pgrc_qa::corpus::int8_fixture("gravity_cond_memo", 7324, ROWS, vec![], policy, |i| Some(2 * i as i64)));
    let list_of = |dv_row0: bool| {
        Arc::new(PartList::new(bf.manifest.parts.iter().enumerate().map(|(i, rec)| {
            let bytes: Arc<[u8]> = Arc::from(bf.files.get(&pgrc_format::dirlayout::part_file_name(rec.part_no)).expect("part file").as_slice());
            let dv = (dv_row0 && i == 0).then(|| pgrc_format::dml::encode_dv(rec.part_no, 1, &[(2, pgrc_format::dml::DvBlockKind::List, &[0u16][..])]).unwrap());
            let deletes = dv.map(|d| Arc::new(pgrc_scan::PartDeletes::from_dv_payload(&d, rec.part_no, 1, rec.granule_count).unwrap()));
            PartSpec { io: PartIoSpec::Mem { bytes, dev: 7, ino: 700 + i as u64 }, expect: pgrc_read::PartExpect::none(), rows: rec.rows, granules: rec.granule_count, deletes, part: None }
        }).collect()))
    };
    let (plain, deleted) = (list_of(false), list_of(true));
    let profile = int8_profile();
    let preds = [ZonePred::word(0, ZoneOp::Ge, 32_768), ZonePred::word(0, ZoneOp::Lt, 39_000)];
    let pass = |a: &[i64]| a[0] >= 32_768 && a[0] < 39_000 && a[0] % 32 == 0;
    let want: Vec<u32> = (0..3_616u32).filter(|&i| (2 * (16_384 + i as i64)) < 39_000 && (16_384 + i) % 16 == 0).collect();
    assert_eq!((want.len(), want[0]), (195, 0));
    let r = Arc::new(PartRegistry::new(64 << 20));
    let run = |list: &Arc<PartList>| cond_run(4254, list, &r, &profile, (&[1], 0), &preds, VERDICT_ZONE | VERDICT_COND, 0x5eed_0301, &pass);
    let granule2 = |c: &CondRun| c.wins.iter().find(|w| (w.0, w.1) == (0, 2)).map(|w| w.2.clone()).expect("granule 2 presented");
    let miss = run(&plain);
    assert_eq!(miss.cond[1], 1, "the miss published granule 2");
    let hit = run(&plain);
    assert_eq!(hit.cond, [1, 0], "the next execution hits it");
    assert_eq!(granule2(&hit), want, "the hit selects the kept list's rows: the words' survivors");
    let dv = run(&deleted);
    assert_eq!(dv.cond, [1, 0], "the same part under a delete mask hits the same entry");
    assert_eq!(granule2(&dv), want[1..].to_vec(), "with a delete mask the words path applies it: row 0 is gone");
}

/// TH14 L1 (main 09-24): a granule's cached words are the covered quals' verdict for every row whichever path filled
/// them. `a = 2i` with every tenth row far above (200 000 + 2i), so a range's PSMA window over granule 2 runs from the
/// first far row to the range's last row: a strict superset of the matches the host then selects (with no other lane
/// to decode, only a PSMA window selects). Filled through that window and filled with PSMA off, the hits present the same
/// rows with byte-identical words, exactly the rows the quals keep. PSMA off stages the whole granule when no other lane
/// decodes (cases 0 and 1); cases 2 and 3 add a covered qual with no zone (`a % 4 = 0` over a second lane of the column,
/// decoded with the rest: gathered and scattered under a selection), where PSMA off samples a selection for the few
/// matches (case 3) and stages the whole granule for the many (case 2). TH14 L2: a hit leaves a filter-only lane undecoded and selects the same rows.
#[test]
fn the_cached_words_are_the_quals_verdict_whether_a_psma_window_or_the_whole_granule_filled_them() {
    use gravity_hosts::ops::scan::pgrc::granule_source::lane_elided_census;
    use gravity_program::protos::{VERDICT_COND, VERDICT_ELIDE, VERDICT_PSMA, VERDICT_ZONE};
    use std::collections::BTreeMap;
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let f = |i: u64| if i % 10 == 5 { 200_000 + 2 * i as i64 } else { 2 * i as i64 };
    let policy = pgrc_write::writer::PartCutPolicy { max_rows: 20_000, ..Default::default() };
    let bf = pgrc_qa::corpus::build_fixture(pgrc_qa::corpus::int8_fixture("gravity_cond_superset", 7322, ROWS, vec![], policy, move |i| Some(f(i as u64))));
    let list = part_list(&bf);
    let profile = int8_profile();
    let r = Arc::new(PartRegistry::new(64 << 20));
    // the oracle: every row of every granule, no zone, no cache
    let values: BTreeMap<(u32, u32), Vec<i64>> = granule_values(4253, &list, &r, &profile);
    let never = |_: &[i64]| -> bool { panic!("a hit evaluates nothing") };
    for (case, (lo, hi), mod4, matches) in [(0u64, (40_000i64, 40_020i64), false, 9usize), (1, (36_000, 46_000), false, 4_500), (2, (36_000, 46_000), true, 2_500), (3, (40_000, 40_020), true, 5)] {
        let preds = [ZonePred::word(0, ZoneOp::Ge, lo), ZonePred::word(0, ZoneOp::Lt, hi)];
        let attnos: &[i16] = if mod4 { &[1, 1] } else { &[1] };
        let keep = move |a: i64| a >= lo && a < hi && (!mod4 || a % 4 == 0);
        let pass = move |v: &[i64]| v[0] >= lo && v[0] < hi && (!mod4 || v[1] % 4 == 0);
        // the oracle's survivors per granule with any, and their words
        let want: Vec<(u32, u32, Vec<u32>, Option<Vec<u64>>)> = values.iter().filter_map(|(&(p, g), vs)| {
            let rows: Vec<u32> = (0..vs.len() as u32).filter(|&i| keep(vs[i as usize])).collect();
            let mut words = vec![0u64; vs.len().div_ceil(64)];
            rows.iter().for_each(|&i| words[i as usize / 64] |= 1 << (i % 64));
            (!rows.is_empty()).then_some((p, g, rows, Some(words)))
        }).collect();
        assert_eq!(want.iter().map(|w| w.2.len()).sum::<usize>(), matches, "case {case}: the fixture's matches");
        let (fp_psma, fp_full) = (0x5eed_0200 + 2 * case, 0x5eed_0201 + 2 * case);
        let (psma, full) = (VERDICT_ZONE | VERDICT_PSMA | VERDICT_COND, VERDICT_ZONE | VERDICT_COND);
        let miss = cond_run(4253, &list, &r, &profile, (attnos, 0), &preds, psma, fp_psma, &pass);
        assert!(miss.verdicts[0] >= 1, "case {case}: PSMA narrowed granule 2");
        if !mod4 {
            // the matches' granules: their PSMA window's selection is exactly the matches (a granule PSMA leaves whole, with
            // no other lane to decode, is staged whole and the program's words say NoMatch)
            let staged: Vec<(u32, u32, Vec<u32>)> = miss.wins.iter().filter(|w| want.iter().any(|x| (x.0, x.1) == (w.0, w.1))).map(|w| (w.0, w.1, w.2.clone())).collect();
            assert_eq!(staged, want.iter().map(|w| (w.0, w.1, w.2.clone())).collect::<Vec<_>>(), "case {case}: the window presents only the matches (the host's selection)");
        }
        let filled = cond_run(4253, &list, &r, &profile, (attnos, 0), &preds, full, fp_full, &pass);
        assert_eq!(filled.verdicts[0], 0, "case {case}: no PSMA window on the full path");
        assert!(miss.cond[1] >= 1 && filled.cond[1] == miss.cond[1], "case {case}: both paths publish the same granules");
        let hit_psma = cond_run(4253, &list, &r, &profile, (attnos, 0), &preds, psma, fp_psma, &never);
        let hit_full = cond_run(4253, &list, &r, &profile, (attnos, 0), &preds, full, fp_full, &never);
        assert_eq!(hit_psma.cond[1] + hit_full.cond[1], 0, "case {case}: a hit publishes nothing");
        assert!(hit_psma.cond[0] >= 1 && hit_full.cond[0] == hit_psma.cond[0], "case {case}: both hit");
        assert_eq!(hit_psma.wins, want, "case {case}: the PSMA-filled words are the quals' verdict");
        assert_eq!(hit_full.wins, hit_psma.wins, "case {case}: byte-identical words whichever path filled them");
        // the key is the fingerprint alone: the PSMA-filled words serve an execution with PSMA off unchanged
        let crossed = cond_run(4253, &list, &r, &profile, (attnos, 0), &preds, full, fp_psma, &never);
        assert_eq!(crossed.wins, want, "case {case}: no PSMA state in the key");
        // L2: with lane 0 filter-only, a hit leaves it undecoded (lane elision) and selects the same rows
        let before = lane_elided_census();
        let elided = cond_run(4253, &list, &r, &profile, (attnos, 0b1), &preds, psma | VERDICT_ELIDE, fp_psma, &never);
        assert_eq!(elided.wins, want, "case {case}: the elided hit selects the same rows");
        assert_eq!((elided.undecoded, lane_elided_census() - before), (want.len(), want.len() as u64), "case {case}: lane 0 undecoded on every hit");
    }
}

/// decode.unread (main 09-24): under VERDICT_UNREAD a condition-cache hit decodes no lane and publishes a plain window of
/// its survivors' count (the end counts it whole): an All hit (part 0's granule 0) every row, a Mixed hit its words'
/// popcount, the last part's tail granule (not a multiple of 64 rows) included, a None hit nothing. A delete mask the
/// fill never saw (the same parts, part 0's granule 2 with a kept row deleted) takes that row off. A miss decodes.
/// V4R-33: a claim over a whole part the cache knows granule by granule is ONE window of the part's survivor total; a part
/// with a delete vector stays granule by granule (its counts less the deleted rows).
#[test]
fn an_unread_hit_is_a_plain_window_of_its_survivors_count_and_decodes_no_lane() {
    use gravity_hosts::ops::scan::pgrc::granule_source::{unread_census, PartIoSpec, PartList, PartSpec};
    use gravity_program::protos::VERDICT_UNREAD;
    use gravity_program::protos::VERDICT_COND;
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let policy = pgrc_write::writer::PartCutPolicy { max_rows: 20_000, ..Default::default() };
    let bf = pgrc_qa::corpus::build_fixture(pgrc_qa::corpus::int8_fixture("gravity_unread", 7325, ROWS, vec![], policy, |i| Some(2 * i as i64)));
    let plain = part_list(&bf);
    // the same part files and identities (the cache serves a lookup through the same open part), part 0's granule 2 with
    // its row 2 deleted: a = 2 (16 384 + 2) = 32 772, a kept row
    let deleted = Arc::new(PartList::new(bf.manifest.parts.iter().enumerate().map(|(i, rec)| {
        let bytes: Arc<[u8]> = Arc::from(bf.files.get(&pgrc_format::dirlayout::part_file_name(rec.part_no)).expect("part file").as_slice());
        let dv = (i == 0).then(|| pgrc_format::dml::encode_dv(rec.part_no, 1, &[(2, pgrc_format::dml::DvBlockKind::List, &[2u16][..])]).unwrap());
        let deletes = dv.map(|d| Arc::new(pgrc_scan::PartDeletes::from_dv_payload(&d, rec.part_no, 1, rec.granule_count).unwrap()));
        PartSpec { io: PartIoSpec::Mem { bytes, dev: 7, ino: 500 + i as u64 }, expect: pgrc_read::PartExpect::none(), rows: rec.rows, granules: rec.granule_count, deletes, part: None }
    }).collect()));
    let profile = int8_profile();
    let r = Arc::new(PartRegistry::new(64 << 20));
    let keep = |a: i64| a < 16_384 || (a >= 32_768 && a % 6 == 0);
    assert!(keep(32_772));
    let values = granule_values(4255, &plain, &r, &profile);
    let counts = |minus: (u32, u32)| -> Vec<(u32, u32, Vec<u32>, Option<Vec<u64>>)> {
        values.iter().filter_map(|(&(p, g), vs)| {
            let n = vs.iter().filter(|&&a| keep(a)).count() as u32 - ((p, g) == minus) as u32;
            (n > 0).then(|| (p, g, (0..n).collect(), None))
        }).collect()
    };
    let (want, want_dv) = (counts((u32::MAX, 0)), counts((0, 2)));
    // whole parts (V4R-33): part p's window is granule 0's, of the part's total; with `dv_part` (a delete vector) that
    // part's granules stay as they were
    let by_part = |ws: &[(u32, u32, Vec<u32>, Option<Vec<u64>>)], dv_part: Option<u32>| -> Vec<(u32, u32, Vec<u32>, Option<Vec<u64>>)> {
        let mut out: Vec<(u32, u32, Vec<u32>, Option<Vec<u64>>)> = Vec::new();
        for w in ws {
            match out.last_mut() {
                _ if Some(w.0) == dv_part => out.push(w.clone()),
                Some(l) if l.0 == w.0 => l.2 = (0..l.2.len() as u32 + w.2.len() as u32).collect(),
                _ => out.push((w.0, 0, (0..w.2.len() as u32).collect(), None)),
            }
        }
        out
    };
    let (want_whole, want_dv_whole) = (by_part(&want, None), by_part(&want_dv, Some(0)));
    assert!(values.get(&(0, 0)).is_some_and(|vs| vs.iter().all(|&a| keep(a))), "an All granule");
    assert!(values.values().any(|vs| vs.len() % 64 != 0 && vs.iter().any(|&a| keep(a)) && !vs.iter().all(|&a| keep(a))), "a Mixed tail granule");
    let never = |_: &[i64]| -> bool { panic!("a hit evaluates nothing") };
    let (on, fp, lanes) = (VERDICT_COND | VERDICT_UNREAD, 0x5eed_0400, (&[1i16][..], 0));
    let miss = cond_run(4255, &plain, &r, &profile, lanes, &[], on, fp, &|v| keep(v[0]));
    assert_eq!(miss.undecoded, 0, "a miss decodes lane 0: the program tests it");
    let u0 = unread_census();
    let hit = cond_run(4255, &plain, &r, &profile, lanes, &[], on, fp, &never);
    assert_eq!(hit.wins, want_whole, "a plain window of each whole part's survivors");
    assert_eq!((hit.undecoded, unread_census() - u0), (want_whole.len(), want_whole.len() as u64), "no lane decoded, each an unread window");
    assert_eq!(hit.wins.iter().map(|w| w.2.len()).sum::<usize>(), want.iter().map(|w| w.2.len()).sum::<usize>());
    let hit_dv = cond_run(4255, &deleted, &r, &profile, lanes, &[], on, fp, &never);
    assert_eq!(hit_dv.wins, want_dv_whole, "the deleted row is off its granule's count; that part stays granule by granule");
    // UNREAD off: the same hit decodes its lane and selects from the words
    let decoded = cond_run(4255, &plain, &r, &profile, lanes, &[], VERDICT_COND, fp, &never);
    assert_eq!(decoded.undecoded, 0);
    assert_eq!(decoded.wins.iter().map(|w| (w.0, w.1, w.2.len())).collect::<Vec<_>>(), want.iter().map(|w| (w.0, w.1, w.2.len())).collect::<Vec<_>>());
}

/// TH06 lever a (VERDICT_ELIDE_HIT, V4R-4): the covered quals are the zone's `a < 70 000` (lane 1) and a non-zone
/// `a % 4 = 0` (lane 0), both lanes filter-only. Plain VERDICT_ELIDE (a zone covering every covered op) answers the zone
/// AllMatch part 0 whole and part 1's AllMatch granule 0 with no lane decoded. Under ELIDE_HIT neither is answered or
/// elided: a miss decodes every window, the program tests every row, and the AllMatch granules publish their words with
/// the Mixed granule 1's; the next execution's hits leave both lanes undecoded and select exactly the quals' rows.
#[test]
fn an_elide_hit_site_elides_only_in_a_hit_and_answers_no_zone_allmatch_window() {
    use gravity_hosts::ops::scan::pgrc::granule_source::granule_census;
    use gravity_program::protos::VERDICT_ELIDE_HIT;
    use gravity_program::protos::{VERDICT_COND, VERDICT_ELIDE, VERDICT_ZONE};
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // a = 2i, three granules a part: part 0 is [0, 49 150] (AllMatch whole), part 1's granule 0 [49 152, 65 534]
    // AllMatch, its granule 1 [65 536, 81 918] Mixed
    let policy = pgrc_write::writer::PartCutPolicy { max_rows: 20_000, ..Default::default() };
    let bf = pgrc_qa::corpus::build_fixture(pgrc_qa::corpus::int8_fixture("gravity_elide_hit", 7326, ROWS, vec![], policy, |i| Some(2 * i as i64)));
    let list = part_list(&bf);
    let profile = int8_profile();
    let r = Arc::new(PartRegistry::new(64 << 20));
    let values = granule_values(4256, &list, &r, &profile);
    let keep = |a: i64| a < 70_000 && a % 4 == 0;
    let want: Vec<(u32, u32, Vec<u32>, Option<Vec<u64>>)> = values.iter().filter_map(|(&(p, g), vs)| {
        let rows: Vec<u32> = (0..vs.len() as u32).filter(|&i| keep(vs[i as usize])).collect();
        let mut words = vec![0u64; vs.len().div_ceil(64)];
        rows.iter().for_each(|&i| words[i as usize / 64] |= 1 << (i % 64));
        (!rows.is_empty()).then_some((p, g, rows, Some(words)))
    }).collect();
    assert_eq!(want.iter().map(|w| (w.0, w.1)).collect::<Vec<_>>(), vec![(0, 0), (0, 1), (0, 2), (1, 0), (1, 1)]);
    let preds = [ZonePred::word(1, ZoneOp::Lt, 70_000)];
    let lanes = (&[1i16, 1][..], 0b11);
    let never = |_: &[i64]| -> bool { panic!("a hit evaluates nothing") };
    let run = |enables: u64, fp: u64, pass: &dyn Fn(&[i64]) -> bool| {
        let g0 = granule_census();
        let run = cond_run(4256, &list, &r, &profile, lanes, &preds, enables, fp, pass);
        let g1 = granule_census();
        (run, [g1[2] - g0[2], g1[3] - g0[3]])
    };
    // no cache: (windows with no lane or lane 0 undecoded, the census' [granules answered, parts answered] delta)
    let walk = |enables: u64| {
        let mut recipe = GranuleRecipe::for_test(4256, lanes.0, r.clone()).with_verdict_cell();
        for k in 0..lanes.0.len() {
            recipe = recipe.with_profile(k, profile.clone());
        }
        recipe.verdicts.set(0, enables);
        let resolved = Resolved::for_recipe(&recipe, list.clone());
        let mut src = GranuleRangeSource::new(recipe);
        src.zone(&preds, &[lanes.1], &[0]);
        src.bind(&binding_ref(&resolved)).unwrap();
        let g0 = granule_census();
        src.seek(&[0, list.granules()], &[0, 0]).unwrap();
        let (mut ls, mut undecoded) = ([Lane::NONE; MAX_LANES], 0);
        loop {
            let mut site = blank_site(ls.as_mut_ptr());
            let mut w = Window::over(&mut site);
            if src.window(&mut w).unwrap() == 0 {
                break;
            }
            undecoded += w.lanes().first().is_none_or(|l| l.ptr.is_null()) as usize;
        }
        src.unbind();
        let g1 = granule_census();
        (undecoded, [g1[2] - g0[2], g1[3] - g0[3]])
    };
    // plain VERDICT_ELIDE: part 0 answered whole, part 1's granule 0 answered
    assert_eq!(walk(VERDICT_ZONE | VERDICT_ELIDE), (2, [4, 1]), "the zone's AllMatch windows answered, no lane decoded");
    assert_eq!(walk(VERDICT_ZONE | VERDICT_ELIDE | VERDICT_ELIDE_HIT), (0, [0, 0]), "ELIDE_HIT with no hit: every window decodes, none answered");
    let on = VERDICT_ZONE | VERDICT_COND | VERDICT_ELIDE | VERDICT_ELIDE_HIT;
    let (miss, answered) = run(on, 0x5eed_0502, &|v| v[1] < 70_000 && v[0] % 4 == 0);
    assert_eq!((miss.undecoded, answered, miss.cond), (0, [0, 0], [0, 5]), "a miss decodes; the AllMatch granules publish too");
    let (hit, answered) = run(on, 0x5eed_0502, &never);
    assert_eq!((hit.undecoded, answered, hit.cond), (5, [0, 0], [5, 0]), "every hit leaves the filter lanes undecoded");
    assert_eq!(hit.wins, want, "the hits select the quals' rows");
}

/// V4R-340 (M6-REPS, C12 hot): a hit-only site with no zone predicate. The driver hands the host its filter-only lanes
/// as `zone(&[], filter, &[])` (driver/src/node.rs), and the empty predicate list prunes, marks and answers nothing: a miss
/// presents every granule whole, decodes the lane and tests every row. Only the next execution's hits leave the lane
/// undecoded, selecting exactly the qual's rows.
#[test]
fn a_hit_only_site_with_no_zone_predicate_elides_in_a_hit_and_prunes_nothing() {
    use gravity_hosts::ops::scan::pgrc::granule_source::granule_census;
    use gravity_program::protos::VERDICT_ELIDE_HIT;
    use gravity_program::protos::{VERDICT_COND, VERDICT_ELIDE};
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let policy = pgrc_write::writer::PartCutPolicy { max_rows: 20_000, ..Default::default() };
    let bf = pgrc_qa::corpus::build_fixture(pgrc_qa::corpus::int8_fixture("gravity_elide_hit_nozone", 7328, ROWS, vec![], policy, |i| Some(2 * i as i64)));
    let list = part_list(&bf);
    let profile = int8_profile();
    let r = Arc::new(PartRegistry::new(64 << 20));
    let values = granule_values(4257, &list, &r, &profile);
    let keep = |a: i64| a % 12 == 4;
    let want: Vec<(u32, u32, Vec<u32>, Option<Vec<u64>>)> = values.iter().filter_map(|(&(p, g), vs)| {
        let rows: Vec<u32> = (0..vs.len() as u32).filter(|&i| keep(vs[i as usize])).collect();
        let mut words = vec![0u64; vs.len().div_ceil(64)];
        rows.iter().for_each(|&i| words[i as usize / 64] |= 1 << (i % 64));
        (!rows.is_empty()).then_some((p, g, rows, Some(words)))
    }).collect();
    let n = list.granules();
    assert!(n > 1 && want.len() as u64 == n, "every granule has a surviving row: {n} / {}", want.len());
    let lanes = (&[1i16][..], 0b1);
    let on = VERDICT_COND | VERDICT_ELIDE | VERDICT_ELIDE_HIT;
    let never = |_: &[i64]| -> bool { panic!("a hit evaluates nothing") };
    let g0 = granule_census();
    let miss = cond_run(4257, &list, &r, &profile, lanes, &[], on, 0x5eed_0503, &|v| keep(v[0]));
    let g1 = granule_census();
    assert_eq!(miss.wins.len() as u64, n, "a miss presents every granule");
    assert!(miss.wins.iter().all(|w| w.2.len() == values[&(w.0, w.1)].len() && w.3.is_none()), "no selection: every row tested");
    assert_eq!((miss.undecoded, miss.cond, [g1[1] - g0[1], g1[2] - g0[2], g1[3] - g0[3]]), (0, [0, n], [0, 0, 0]), "a miss decodes, publishes every granule, skips and answers none");
    let hit = cond_run(4257, &list, &r, &profile, lanes, &[], on, 0x5eed_0503, &never);
    let g2 = granule_census();
    assert_eq!((hit.undecoded as u64, hit.cond, [g2[1] - g1[1], g2[2] - g1[2], g2[3] - g1[3]]), (n, [n, 0], [0, 0, 0]), "every hit leaves the lane undecoded, skips and answers none");
    assert_eq!(hit.wins, want, "the hits select the qual's rows");
}

/// TH14 L4: a source's census counts reach the process statics at its claim's end, at seek and at unbind (a per-source
/// tally, not a fetch_add per granule), so after a parallel scan's sources unbind the deltas are the sum of their own
/// counts, a source a stop left mid-claim (a LIMIT) included.
#[test]
fn a_parallel_scans_census_deltas_are_its_sources_counts_after_unbind() {
    use gravity_hosts::ops::scan::pgrc::granule_source::granule_census;
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let bf = sorted_fixture();
    let list = part_list(&bf);
    let total = list.granules();
    // granules 0 and 1 AllMatch, 2 Mixed, the rest NoneMatch
    let preds = [ZonePred::word(0, ZoneOp::Lt, 2 * GRANULE as i64 + 100)];
    let before = granule_census();
    // (lo, hi, windows to read before the stop: 0 reads the claim out)
    let counts: Vec<[u64; 3]> = std::thread::scope(|sc| {
        let hs: Vec<_> = [(0, total, 1usize), (0, total / 2, 0), (total / 2, total, 0)].into_iter().map(|(lo, hi, stop)| {
            let (list, preds) = (list.clone(), &preds);
            sc.spawn(move || {
                let recipe = GranuleRecipe::for_test(REL_SORTED, &[1], Arc::new(PartRegistry::new(64 << 20)));
                let resolved = Resolved::for_recipe(&recipe, list);
                let mut src = GranuleRangeSource::new(recipe);
                src.zone(preds, &[0], &[0]);
                src.bind(&binding_ref(&resolved)).unwrap();
                src.seek(&[lo, hi], &[0, 0]).unwrap();
                let (mut lanes, mut read) = ([Lane::NONE; MAX_LANES], 0);
                loop {
                    let mut site = blank_site(lanes.as_mut_ptr());
                    let mut w = Window::over(&mut site);
                    if src.window(&mut w).unwrap() == 0 {
                        break;
                    }
                    read += 1;
                    if read == stop {
                        break;
                    }
                }
                src.unbind();
                [src.granules_decoded(), src.granules_skipped(), src.granules_answered()]
            })
        }).collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let after = granule_census();
    let sum = counts.iter().fold([0u64; 3], |a, c| [a[0] + c[0], a[1] + c[1], a[2] + c[2]]);
    assert!(counts[0][0] >= 1 && counts[0].iter().sum::<u64>() < total, "the stopped source left its claim early: {:?}", counts[0]);
    assert_eq!([after[0] - before[0], after[1] - before[1], after[2] - before[2]], sum, "the census is the sources' counts: {counts:?}");
}

/// V4R-19 (A): per presented window `(part, granule, lane 0's aux, lane 0's element 0)` under `enables` (slot 3 `late`).
fn const_run(list: &Arc<gravity_hosts::ops::scan::pgrc::granule_source::PartList>, registry: &Arc<PartRegistry>, profile: &pgrc_meta::profile::MetaProfile, enables: u64, late: u64) -> Vec<(u32, u32, u32, i64)> {
    let recipe = GranuleRecipe::for_test(4256, &[1], registry.clone()).with_profile(0, profile.clone()).with_verdict_cell();
    recipe.verdicts.set(3, late);
    recipe.verdicts.set(0, enables);
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&resolved)).unwrap();
    src.seek(&[0, list.granules()], &[0, 0]).unwrap();
    let (mut lanes, mut out) = ([Lane::NONE; MAX_LANES], Vec::new());
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        if src.window(&mut w).unwrap() == 0 {
            break;
        }
        let l = w.lanes()[0];
        out.push((w.base().part_id, w.base().granule, l.aux, lane_value(&l, 0)));
    }
    src.unbind();
    out
}

/// V4R-19 (A), the constant-key window: under VERDICT_CONST_KEY the key lane's aux is 1 exactly in the windows its exact
/// zone records prove one non-NULL key, and its element 0 is that key even when the lane is late (never decoded here).
/// Four parts of three granules of 8 192 rows (the writer cuts at a granule): part 0 is 7 throughout (its part record
/// decides); part 1 ascends: 11, then i / 100, then 1 000 with its last row 1 001 (a perturbed constant); part 2 is 20
/// with one NULL in its middle granule (its part record has a NULL: its granule records decide); part 3 is unsorted:
/// -5, then i % 2, then -5 (its constant granules are never asked: an unsorted part's granule records are not read). A
/// delete mask on part 0's granule 1 unmarks that granule alone; the bit off marks nothing and asks nothing.
#[test]
fn a_constant_key_window_marks_its_key_lane_and_publishes_the_key() {
    use gravity_hosts::ops::scan::pgrc::granule_source::{const_key_census, PartIoSpec, PartList, PartSpec};
    use gravity_program::protos::{const_key_arg, VERDICT_LATE};
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let key = |i: u64| match i {
        0..24_576 => Some(7),
        24_576..32_768 => Some(11),
        32_768..40_960 => Some(i as i64 / 100),
        49_151 => Some(1_001),
        40_960..49_152 => Some(1_000),
        61_000 => None,
        49_152..73_728 => Some(20),
        81_920..90_112 => Some(i as i64 % 2),
        _ => Some(-5),
    };
    let policy = pgrc_write::writer::PartCutPolicy { max_rows: 20_000, ..Default::default() };
    let bf = pgrc_qa::corpus::build_fixture(pgrc_qa::corpus::int8_fixture("gravity_const_key", 7326, 4 * 3 * GRANULE, vec![], policy, key));
    let list_of = |dv: bool| {
        Arc::new(PartList::new(bf.manifest.parts.iter().enumerate().map(|(i, rec)| {
            let bytes: Arc<[u8]> = Arc::from(bf.files.get(&pgrc_format::dirlayout::part_file_name(rec.part_no)).expect("part file").as_slice());
            let dv = (dv && i == 0).then(|| pgrc_format::dml::encode_dv(rec.part_no, 1, &[(1, pgrc_format::dml::DvBlockKind::List, &[5u16][..])]).unwrap());
            let deletes = dv.map(|d| Arc::new(pgrc_scan::PartDeletes::from_dv_payload(&d, rec.part_no, 1, rec.granule_count).unwrap()));
            PartSpec { io: PartIoSpec::Mem { bytes, dev: 7, ino: 800 + i as u64 }, expect: pgrc_read::PartExpect::none(), rows: rec.rows, granules: rec.granule_count, deletes, part: None }
        }).collect()))
    };
    // the footer's zone map of the key (attno 1) as `resolve` leaves it on the list: per granule the non-NULL keys' (min,
    // max), (1, 0) when none; const_key_window reads no Stats body for a window this map does not show as one value
    let zoned = |l: Arc<PartList>| {
        use pgrc_am::footer::ZoneFacts;
        let (mut ranges, mut at) = (Vec::new(), 0u64);
        for rec in &bf.manifest.parts {
            for g in 0..rec.granule_count as u64 {
                let ks: Vec<i64> = (at + g * GRANULE..at + ((g + 1) * GRANULE).min(rec.rows as u64)).filter_map(key).collect();
                ranges.push(ks.iter().min().zip(ks.iter().max()).map_or((1, 0), |(a, b)| (*a, *b)));
            }
            at += rec.rows as u64;
        }
        l.set_zone_columns(vec![ZoneFacts::new(1, pgrc_format::zonemap::ZoneMap::encode(&ranges).unwrap()).unwrap()]);
        l
    };
    let (plain, deleted, bare) = (zoned(list_of(false)), zoned(list_of(true)), list_of(false));
    assert_eq!(plain.len(), 4);
    let (profile, r) = (int8_profile(), Arc::new(PartRegistry::new(64 << 20)));
    let marked = |w: &[(u32, u32, u32, i64)]| w.iter().filter(|w| w.2 == 1).map(|w| (w.0, w.1, w.3)).collect::<Vec<_>>();
    let want = vec![(0, 0, 7), (0, 1, 7), (0, 2, 7), (1, 0, 11), (2, 0, 20), (2, 2, 20)];
    let c0 = const_key_census();
    let eager = const_run(&plain, &r, &profile, const_key_arg(0), 0);
    assert_eq!(eager.len(), 12, "{eager:?}");
    assert_eq!(marked(&eager), want);
    let c1 = const_key_census();
    assert_eq!([c1[0] - c0[0], c1[1] - c0[1]], [6, 12], "[constant, asked]");
    let late = const_run(&plain, &r, &profile, const_key_arg(0) | VERDICT_LATE, 1);
    assert_eq!(marked(&late), want, "a late lane's element 0 is the key though nothing decoded it");
    let dv = const_run(&deleted, &r, &profile, const_key_arg(0), 0);
    assert_eq!(marked(&dv), [&want[..1], &want[2..]].concat(), "a delete mask unmarks its granule alone");
    let c2 = const_key_census();
    assert!(const_run(&plain, &r, &profile, gravity_program::protos::VERDICT_ZONE, 0).iter().all(|w| w.2 == 0));
    assert_eq!(const_key_census(), c2, "the bit off asks nothing");
    // no zone map and no catalog record (a bare key column): every window is asked and none marked, before any Stats read
    assert!(const_run(&bare, &r, &profile, const_key_arg(0), 0).iter().all(|w| w.2 == 0), "no zone map");
    let c3 = const_key_census();
    assert_eq!([c3[0] - c2[0], c3[1] - c2[1]], [0, 12], "[constant, asked] without a zone map");
}

/// V4R-33: an unread execution over `claims` (granule ranges): (windows, rows) presented, and the parts counted whole.
fn unread_claims(rel: u32, list: &Arc<gravity_hosts::ops::scan::pgrc::granule_source::PartList>, registry: &Arc<PartRegistry>, fp: u64, claims: &[(u64, u64)], pass: &dyn Fn(i64) -> bool) -> (usize, u64, u64) {
    use gravity_abi::ctx::lane as lf;
    use gravity_hosts::ops::scan::pgrc::granule_source::cond_parts_census;
    use gravity_program::protos::VERDICT_UNREAD;
    use gravity_program::protos::VERDICT_COND;
    guc_tables::backing::set_pgrust_condition_cache(guc_tables::ConditionCacheMode::Full as i32);
    let recipe = GranuleRecipe::for_test(rel, &[1], registry.clone()).with_verdict_cell().with_verdict_lane(2).with_profile(0, int8_profile());
    recipe.verdicts.set(1, fp);
    recipe.verdicts.set(2, 9);
    recipe.verdicts.set(0, VERDICT_COND | VERDICT_UNREAD);
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.zone(&[], &[0], &[0]);
    src.bind(&binding_ref(&resolved)).unwrap();
    let p0 = cond_parts_census();
    let (mut lanes, mut wins, mut rows) = ([Lane::NONE; MAX_LANES], 0usize, 0u64);
    for &(lo, hi) in claims {
        src.seek(&[lo, hi], &[0, 0]).unwrap();
        loop {
            let mut site = blank_site(lanes.as_mut_ptr());
            let mut w = Window::over(&mut site);
            let n = src.window(&mut w).unwrap();
            if n == 0 {
                break;
            }
            wins += 1;
            let v = w.lanes()[2];
            let hit = !v.ptr.is_null() && v.flags & lf::VERDICT_MASK == lf::VERDICT_ALL_MATCH;
            let sel: Vec<u32> = if w.sel.is_null() { (0..n).collect() } else { unsafe { std::slice::from_raw_parts(w.sel, w.sel_n as usize) }.to_vec() };
            if hit {
                rows += sel.len() as u64;
            } else {
                let a = w.lanes()[0];
                for &i in &sel {
                    if pass(lane_value(&a, i)) {
                        rows += 1;
                        unsafe { *(v.ptr as *mut u64).add(i as usize / 64) |= 1 << (i % 64) };
                    }
                }
            }
        }
    }
    src.unbind();
    (wins, rows, cond_parts_census() - p0)
}

/// V4R-33, does not apply: claims narrower than a part (the claim grain at a width) keep one window a granule, each its
/// stored count, so no worker's claim grows; the whole-table claim over the same cache is one window a part, the same
/// rows. A reopened part (a new registry: a new OpenPart owner) never serves the old counts: its first execution is a
/// miss that evaluates every row again.
#[test]
fn whole_part_counts_need_a_claim_over_the_part_and_its_own_owner() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let policy = pgrc_write::writer::PartCutPolicy { max_rows: 20_000, ..Default::default() };
    let bf = pgrc_qa::corpus::build_fixture(pgrc_qa::corpus::int8_fixture("gravity_cond_counts", 7327, ROWS, vec![], policy, |i| Some(i as i64 * 3)));
    let list = part_list(&bf);
    let total = list.granules();
    let keep = |a: i64| a % 7 == 0;
    let want = (0..ROWS).filter(|&i| keep(i as i64 * 3)).count() as u64;
    let never = |_: i64| -> bool { panic!("a hit evaluates nothing") };
    let r = Arc::new(PartRegistry::new(64 << 20));
    let fp = 0x5eed_0433;
    let per_granule: Vec<(u64, u64)> = (0..total).map(|g| (g, g + 1)).collect();
    let miss = unread_claims(4257, &list, &r, fp, &per_granule, &keep);
    assert_eq!((miss.1, miss.2), (want, 0), "the fill: every row evaluated");
    let narrow = unread_claims(4257, &list, &r, fp, &per_granule, &never);
    assert_eq!(narrow, (total as usize, want, 0), "granule claims: a window each, no part counted whole");
    let whole = unread_claims(4257, &list, &r, fp, &[(0, total)], &never);
    assert_eq!(whole, (list.len(), want, list.len() as u64), "one claim: a window a part");
    let r2 = Arc::new(PartRegistry::new(64 << 20));
    let reopened = unread_claims(4257, &list, &r2, fp, &[(0, total)], &keep);
    assert_eq!((reopened.1, reopened.2), (want, 0), "a new owner: a miss, no old count served");
}

/// D3042, count.only (VERDICT_COUNT_ONLY): with no predicate the host reads no lane and stages a claim's whole part as ONE
/// window of its rows less its delete vector's (`deleted_rows`), a claim entering a part past its first granule granule
/// by granule (its rows less its delete mask's); the windows' rows sum to the live rows either way. Off, every granule
/// decodes its lane.
#[test]
fn a_count_only_site_counts_live_rows_per_part_and_decodes_no_lane() {
    use gravity_hosts::ops::scan::pgrc::granule_source::{PartIoSpec, PartList, PartSpec};
    use gravity_program::protos::VERDICT_COUNT_ONLY;
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let policy = pgrc_write::writer::PartCutPolicy { max_rows: 20_000, ..Default::default() };
    let bf = pgrc_qa::corpus::build_fixture(pgrc_qa::corpus::int8_fixture("gravity_count_only", 7327, ROWS, vec![], policy, |i| Some(i as i64)));
    let plain = part_list(&bf);
    // part 0's granule 2 with rows 2, 3 and 4 deleted, part 1's granule 0 with row 0
    let deleted = Arc::new(PartList::new(bf.manifest.parts.iter().enumerate().map(|(i, rec)| {
        let bytes: Arc<[u8]> = Arc::from(bf.files.get(&pgrc_format::dirlayout::part_file_name(rec.part_no)).expect("part file").as_slice());
        let dv = match i {
            0 => Some(pgrc_format::dml::encode_dv(rec.part_no, 1, &[(2, pgrc_format::dml::DvBlockKind::List, &[2u16, 3, 4][..])]).unwrap()),
            1 => Some(pgrc_format::dml::encode_dv(rec.part_no, 1, &[(0, pgrc_format::dml::DvBlockKind::List, &[0u16][..])]).unwrap()),
            _ => None,
        };
        let deletes = dv.map(|d| Arc::new(pgrc_scan::PartDeletes::from_dv_payload(&d, rec.part_no, 1, rec.granule_count).unwrap()));
        PartSpec { io: PartIoSpec::Mem { bytes, dev: 7, ino: 600 + i as u64 }, expect: pgrc_read::PartExpect::none(), rows: rec.rows, granules: rec.granule_count, deletes, part: None }
    }).collect()));
    let profile = int8_profile();
    let r = Arc::new(PartRegistry::new(64 << 20));
    let parts = bf.manifest.parts.len();
    assert!(parts >= 3, "three parts at least");
    let total: u64 = bf.manifest.parts.iter().map(|p| p.rows).sum();
    // (windows as (part, granule, rows), undecoded windows) over granules [from, all)
    let run = |list: &Arc<PartList>, enables: u64, from: u64| -> (Vec<(u32, u32, u32)>, usize) {
        let mut recipe = GranuleRecipe::for_test(4257, &[1], r.clone()).with_verdict_cell().with_profile(0, profile.clone());
        recipe.verdicts.set(0, enables);
        let resolved = Resolved::for_recipe(&recipe, list.clone());
        let mut src = GranuleRangeSource::new(recipe);
        src.bind(&binding_ref(&resolved)).unwrap();
        src.seek(&[from, list.granules()], &[0, 0]).unwrap();
        let (mut lanes, mut wins, mut undecoded) = ([Lane::NONE; MAX_LANES], Vec::new(), 0);
        loop {
            let mut site = blank_site(lanes.as_mut_ptr());
            let mut w = Window::over(&mut site);
            let n = src.window(&mut w).unwrap();
            if n == 0 {
                break;
            }
            let rows = if w.sel.is_null() { n } else { w.sel_n };
            undecoded += w.lanes()[0].ptr.is_null() as usize;
            wins.push((w.base().part_id, w.base().granule, rows));
        }
        src.unbind();
        (wins, undecoded)
    };
    let sum = |ws: &[(u32, u32, u32)]| ws.iter().map(|w| w.2 as u64).sum::<u64>();
    let (whole, undecoded) = run(&plain, VERDICT_COUNT_ONLY, 0);
    assert_eq!(whole.len(), parts, "one window a part");
    assert_eq!((sum(&whole), undecoded), (total, parts), "every row, no lane decoded");
    let (dv, undecoded) = run(&deleted, VERDICT_COUNT_ONLY, 0);
    assert_eq!((dv.len(), undecoded), (parts, parts), "a part with a delete vector is still one window");
    assert_eq!(sum(&dv), total - 4, "its deleted rows are off");
    // a claim entering part 0 at granule 1: part 0 granule by granule (granule 2 less its 3 deleted rows), the rest whole
    let (mid, undecoded) = run(&deleted, VERDICT_COUNT_ONLY, 1);
    let g0 = bf.manifest.parts[0].granule_count as usize;
    assert_eq!((mid.len(), undecoded), (g0 - 1 + parts - 1, g0 - 1 + parts - 1));
    assert_eq!(sum(&mid), total - 8192 - 4);
    // off: every granule decodes its lane, the same live rows
    let (off, undecoded) = run(&deleted, 0, 0);
    assert_eq!((sum(&off), undecoded), (total - 4, 0));
}
