//! The v4 host verdicts of `GranuleRangeSource` (plan §5.2: H02 bloom / PSMA, H03b verdict-aware claim reads, H02i the
//! condition cache), each against the plain scan: a verdict skips or narrows, never changes a row.

use std::sync::Arc;

use gravity_abi::ctx::lane;
use gravity_abi::Lane;
use gravity_driver::{blank_site, Source, Window};
use gravity_hosts::ops::scan::pgrc::granule_source::{contested_census, verdict_census, GranuleRangeSource, GranuleRecipe, Resolved, RtFilter, VerdictCell};
use gravity_program::protos::{word_zone_arg, word_zone_key_arg};
use gravity_program::protos::{VERDICT_BLOOM, VERDICT_CLAIM, VERDICT_COND, VERDICT_COUNT_SELECT, VERDICT_PSMA, VERDICT_ZONE};
use gravity_program::{ZoneOp, ZonePred};
use pgrc_meta::profile::MetaProfile;
use pgrc_qa::corpus::{build_fixture, int8_fixture, BuiltFixture};
use pgrc_qa::int8_col;
use pgrc_read::PartRegistry;
use pgrc_write::writer::PartCutPolicy;

#[path = "common/granule_fixtures.rs"]
#[allow(dead_code)]
mod granule_fixtures;
use granule_fixtures::*;

/// The site's verdict lane (after the one attribute lane and the offset lane).
const VK: usize = 2;

#[derive(Debug, Default, PartialEq)]
struct Scan {
    count: u64,
    decoded: u64,
    answered: u64,
    hits: u64,
    misses: u64,
}

fn rows_of(w: &Window<'_>, n: u32) -> Vec<u32> {
    if w.sel.is_null() {
        (0..n).collect()
    } else {
        unsafe { std::slice::from_raw_parts(w.sel, w.sel_n as usize) }.to_vec()
    }
}

/// Every window of `src` over granules `[lo, hi)`: the rows passing `pred` counted. A window whose verdict lane says
/// covered (a hit) must select exactly passing rows; a miss gets the passing rows' bits (the IR's `passed()`).
fn scan_range(src: &mut GranuleRangeSource, lo: u64, hi: u64, pred: &dyn Fn(i64) -> bool, consume: bool) -> Scan {
    src.seek(&[lo, hi], &[0, 0]).unwrap();
    windows(src, pred, consume)
}

/// `scan_range`'s windows after a seek the caller made.
fn windows(src: &mut GranuleRangeSource, pred: &dyn Fn(i64) -> bool, consume: bool) -> Scan {
    let mut lanes = [Lane::NONE; MAX_LANES];
    let mut s = Scan::default();
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = src.window(&mut w).unwrap();
        if n == 0 {
            break;
        }
        if w.is_answered() {
            s.answered += 1;
            s.count += n as u64;
            continue;
        }
        s.decoded += 1;
        let l = w.lanes()[0];
        let rows = rows_of(&w, n);
        let v = w.lanes().get(VK).copied().filter(|v| lane::id_kind(v.id) == lane::ID_VERDICT);
        let value = |i: u32| unsafe { l.is_valid(i as usize).then(|| l.datum(i as usize) as i64) };
        match v {
            Some(v) if v.flags & lane::VERDICT_MASK == lane::VERDICT_ALL_MATCH => {
                s.hits += 1;
                for &i in &rows {
                    assert!(value(i).is_some_and(pred), "a covered window selects only passing rows (row {i})");
                    s.count += 1;
                }
            }
            Some(v) => {
                assert_eq!(v.flags & lane::VERDICT_MASK, lane::VERDICT_MIXED);
                s.misses += 1;
                let words = v.ptr as *mut u64;
                for &i in &rows {
                    if value(i).is_some_and(pred) {
                        s.count += 1;
                        unsafe { *words.add(i as usize / 64) |= 1 << (i % 64) };
                    }
                }
            }
            None => s.count += rows.iter().filter(|&&i| value(i).is_some_and(pred)).count() as u64,
        }
        if !consume {
            break;
        }
    }
    s
}

fn scan(src: &mut GranuleRangeSource, total: u64, pred: &dyn Fn(i64) -> bool) -> Scan {
    scan_range(src, 0, total, pred, true)
}

/// A fresh source of `recipe` under `zone`, bound over `resolved` (it reads the recipe's verdict cell at bind).
fn source(recipe: &GranuleRecipe, zone: &[ZonePred], resolved: &Resolved) -> GranuleRangeSource {
    let mut s = GranuleRangeSource::new(recipe.clone());
    s.zone(zone, &[0], &[0]);
    s.bind(&binding_ref(resolved)).unwrap();
    s
}

fn int8_profile() -> MetaProfile {
    let c = int8_col(1);
    MetaProfile::derive_or_opaque(c.class, c.collation_class, c.semantics)
}

/// The distinct values of the bloom fixture: each multiple of four below `4 * BLOOM_VALUES`, `ROWS / BLOOM_VALUES` times.
const BLOOM_VALUES: u64 = 600;

/// Multiples of four in no order: every granule's zone range spans the table, so an equality is Mixed everywhere. The
/// writer arms a bloom on an unsorted granule of two or more values and keeps the block its stored bytes pay for
/// (D3070: `pgrc_meta::bloom::bloom_part_choice`), so each full granule of the 600 values keeps a small block; a granule
/// of thousands of distinct 8-byte values would keep none.
fn sparse_fixture() -> BuiltFixture {
    build_fixture(int8_fixture("gravity_verdicts_sparse", 7311, ROWS, vec![], PartCutPolicy { max_rows: 20_000, ..PartCutPolicy::default() }, |i| Some((i * 7_919 % ROWS % BLOOM_VALUES) as i64 * 4)))
}

fn verdict_recipe(rel: u32, registry: Arc<PartRegistry>) -> GranuleRecipe {
    guc_tables::backing::set_pgrust_condition_cache(guc_tables::ConditionCacheMode::Full as i32);
    GranuleRecipe::for_test(rel, &[1], registry).with_profile(0, int8_profile()).with_verdict_cell().with_verdict_lane(VK)
}

#[test]
fn a_bloom_skips_a_zone_mixed_granule_that_lacks_the_value_and_keeps_one_that_has_it() {
    let bf = sparse_fixture();
    let list = part_list(&bf);
    let total = list.granules();
    let recipe = verdict_recipe(REL_SORTED, Arc::new(PartRegistry::new(64 << 20)));
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let before = verdict_census()[0];
    let (mut skipped_by_bloom, mut mixed_absent) = (0, 0);
    for k in [50u64, 130, 277, 310, 444, 520] {
        let (absent, present) = (k as i64 * 4 + 1, k as i64 * 4);
        for (value, want) in [(absent, 0u64), (present, ROWS / BLOOM_VALUES)] {
            let zone = [ZonePred::word(0, ZoneOp::Eq, value)];
            let pred = |x: i64| x == value;
            recipe.verdicts.set(0, VERDICT_ZONE);
            let mut zoned = source(&recipe, &zone, &resolved);
            let z = scan(&mut zoned, total, &pred);
            assert_eq!(z.count, want, "zone alone: {value}");
            assert!(z.decoded >= total - 1, "zone alone: {value} is Mixed in (nearly) every granule ({} of {total})", z.decoded);
            recipe.verdicts.set(0, VERDICT_ZONE | VERDICT_BLOOM | VERDICT_CLAIM);
            let mut bloomed = source(&recipe, &zone, &resolved);
            let b = scan(&mut bloomed, total, &pred);
            assert_eq!(b.count, want, "bloom: {value}");
            assert_eq!(bloomed.granules_skipped() + b.decoded, total, "{value}: every granule is skipped or decoded");
            assert!(b.decoded <= z.decoded, "{value}: the bloom skips only zone-Mixed granules");
            if want == 0 {
                skipped_by_bloom += z.decoded - b.decoded;
                mixed_absent += z.decoded;
            }
            zoned.unbind();
            bloomed.unbind();
        }
    }
    assert!(skipped_by_bloom * 2 >= mixed_absent, "the bloom skips the absent values' Mixed granules ({skipped_by_bloom} of {mixed_absent})");
    assert!(verdict_census()[0] - before >= skipped_by_bloom);
}

#[test]
fn psma_narrows_a_zone_mixed_granule_and_every_operator_keeps_the_plain_count() {
    let bf = sorted_fixture();
    let list = part_list(&bf);
    let total = list.granules();
    let recipe = verdict_recipe(REL_SORTED, Arc::new(PartRegistry::new(64 << 20)));
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let before = verdict_census()[2];
    for (op, value) in [(ZoneOp::Eq, 12_345i64), (ZoneOp::Lt, 20_000), (ZoneOp::Le, 20_000), (ZoneOp::Gt, 40_000), (ZoneOp::Ge, 40_000), (ZoneOp::Ne, 7), (ZoneOp::Eq, 60_000)] {
        let zone = [ZonePred::word(0, op, value)];
        let pred = move |x: i64| match op {
            ZoneOp::Eq => x == value,
            ZoneOp::Lt => x < value,
            ZoneOp::Le => x <= value,
            ZoneOp::Gt => x > value,
            ZoneOp::Ge => x >= value,
            _ => x != value,
        };
        let want = (0..ROWS as i64).filter(|&x| pred(x)).count() as u64;
        for enables in [VERDICT_ZONE, VERDICT_ZONE | VERDICT_PSMA, VERDICT_ZONE | VERDICT_PSMA | VERDICT_BLOOM | VERDICT_CLAIM, VERDICT_PSMA] {
            recipe.verdicts.set(0, enables);
            let mut s = source(&recipe, &zone, &resolved);
            assert_eq!(scan(&mut s, total, &pred).count, want, "{op:?} {value} under enables {enables:#x}");
            s.unbind();
        }
    }
    assert!(verdict_census()[2] > before, "PSMA narrowed at least one Mixed granule");
}

#[test]
fn a_miss_publishes_on_the_next_advance_and_the_next_execution_hits_or_skips() {
    let bf = sorted_fixture();
    let list = part_list(&bf);
    let total = list.granules();
    let recipe = verdict_recipe(REL_SORTED, Arc::new(PartRegistry::new(64 << 20)));
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let zone = [ZonePred::word(0, ZoneOp::Lt, 20_000)];
    // granules 0-1 zone AllMatch, 2 zone Mixed, 3-7 NoneMatch; the zone does not cover the cached qual (no
    // VERDICT_ZONE_COVERS), so the frame tests it in the AllMatch windows too and their words publish as granule 2's (sortpath)
    for (fp, pred, survivors) in [([0x5eed_0001u64, 1u64], (|x: i64| x < 20_000 && x % 3 == 0) as fn(i64) -> bool, true), ([0x5eed_0002, 2], |x: i64| x < 16_000, false)] {
        let want = (0..ROWS as i64).filter(|&x| pred(x)).count() as u64;
        recipe.verdicts.set(1, fp[0]);
        recipe.verdicts.set(2, fp[1]);
        recipe.verdicts.set(0, VERDICT_ZONE | VERDICT_COND | VERDICT_CLAIM);
        let mut first = source(&recipe, &zone, &resolved);
        assert_eq!(scan(&mut first, total, &pred), Scan { count: want, decoded: 3, answered: 0, hits: 0, misses: 3 });
        let skipped = first.granules_skipped();
        first.unbind();
        let mut second = source(&recipe, &zone, &resolved);
        let s = scan(&mut second, total, &pred);
        assert_eq!(s.count, want);
        if survivors {
            assert_eq!((s.hits, s.misses, second.granules_skipped()), (3, 0, skipped), "granules 0-2 served from their words");
        } else {
            assert_eq!((s.hits, s.misses, second.granules_skipped()), (2, 0, skipped + 1), "granules 0-1 served, granule 2 known empty: skipped unread");
        }
        second.unbind();
    }
}

/// sortpath: under VERDICT_ZONE_COVERS (the zone covers every cached qual) a zone AllMatch window publishes AllMatch with
/// no words of the frame's (which elides the covered steps there), and the next execution hits it.
#[test]
fn a_zone_allmatch_window_the_zone_covers_publishes_allmatch() {
    let bf = sorted_fixture();
    let list = part_list(&bf);
    let total = list.granules();
    let recipe = verdict_recipe(REL_SORTED, Arc::new(PartRegistry::new(64 << 20)));
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let zone = [ZonePred::word(0, ZoneOp::Lt, 20_000)];
    let pred = |x: i64| x < 20_000;
    let want = (0..ROWS as i64).filter(|&x| pred(x)).count() as u64;
    recipe.verdicts.set(1, 0x5eed_0301);
    recipe.verdicts.set(2, 1);
    recipe.verdicts.set(0, VERDICT_ZONE | VERDICT_COND | VERDICT_CLAIM | gravity_program::protos::VERDICT_ZONE_COVERS);
    let mut first = source(&recipe, &zone, &resolved);
    assert_eq!(scan(&mut first, total, &pred).misses, 3);
    let skipped = first.granules_skipped();
    first.unbind();
    let mut second = source(&recipe, &zone, &resolved);
    let s = scan(&mut second, total, &pred);
    assert_eq!((s.count, s.hits, s.misses, second.granules_skipped()), (want, 3, 0, skipped), "granules 0-1 AllMatch, 2 from its words");
    second.unbind();
}

#[test]
fn a_miss_never_publishes_across_a_seek_and_another_literal_never_hits() {
    let bf = sorted_fixture();
    let list = part_list(&bf);
    let recipe = verdict_recipe(REL_SORTED, Arc::new(PartRegistry::new(64 << 20)));
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let zone = [ZonePred::word(0, ZoneOp::Lt, 20_000)];
    let pred = |x: i64| x < 20_000 && x % 5 == 0;
    recipe.verdicts.set(1, 0x5eed_0003);
    recipe.verdicts.set(2, 3);
    recipe.verdicts.set(0, VERDICT_ZONE | VERDICT_COND);
    let mut s = source(&recipe, &zone, &resolved);
    // granule 2 alone, its window taken but the program never asked for the next: a re-seek drops the due words
    assert_eq!(scan_range(&mut s, 2, 3, &pred, false).misses, 1);
    assert_eq!(scan_range(&mut s, 2, 3, &pred, false).misses, 1, "a seek never publishes the previous window's words");
    assert_eq!(scan_range(&mut s, 2, 3, &pred, true).misses, 1);
    assert_eq!(scan_range(&mut s, 2, 3, &pred, true).hits, 1, "consumed whole: published at the next advance");
    s.unbind();
    // another execution's literal is another fingerprint (exec finishes it over the bound words): no hit
    recipe.verdicts.set(1, 0x5eed_0004);
    let mut t = source(&recipe, &zone, &resolved);
    assert_eq!(scan_range(&mut t, 2, 3, &|x| x < 20_000 && x % 7 == 0, true).hits, 0);
    t.unbind();
}

#[test]
fn a_reopened_part_never_reads_the_old_generation_s_words_and_a_temp_relation_never_caches() {
    let bf = sorted_fixture();
    let list = part_list(&bf);
    let total = list.granules();
    let zone = [ZonePred::word(0, ZoneOp::Lt, 20_000)];
    let pred = |x: i64| x < 20_000 && x % 11 == 0;
    let want = (0..ROWS as i64).filter(|&x| pred(x)).count() as u64;
    let fill_as = |registry: Arc<PartRegistry>, cond_ok: bool, cond_unlogged: bool| {
        let mut recipe = verdict_recipe(REL_SORTED, registry);
        recipe.cond_ok = cond_ok;
        recipe.cond_unlogged = cond_unlogged;
        recipe.verdicts.set(1, 0x5eed_0005);
        recipe.verdicts.set(2, 5);
        recipe.verdicts.set(0, VERDICT_ZONE | VERDICT_COND);
        let resolved = Resolved::for_recipe(&recipe, list.clone());
        let mut s = source(&recipe, &zone, &resolved);
        let r = scan(&mut s, total, &pred);
        s.unbind();
        r
    };
    let fill = |registry: Arc<PartRegistry>, cond_ok: bool| fill_as(registry, cond_ok, false);
    let old = Arc::new(PartRegistry::new(64 << 20));
    assert_eq!(fill(old.clone(), true).misses, 3);
    assert_eq!(fill(old.clone(), true).hits, 3, "same parts, same generation: a hit (granules 0-1 AllMatch on the zone, 2 Mixed)");
    // the same part files under a new registry are new open parts (another generation): the words are not theirs
    let new = Arc::new(PartRegistry::new(64 << 20));
    let r = fill(new.clone(), true);
    assert_eq!((r.count, r.hits, r.misses), (want, 0, 3));
    assert_eq!(fill(new, true).hits, 3);
    // a temp relation (neither flag): no verdict lane, nothing published, never a hit
    let other = Arc::new(PartRegistry::new(64 << 20));
    for _ in 0..2 {
        let r = fill(other.clone(), false);
        assert_eq!((r.count, r.hits, r.misses), (want, 0, 0));
    }
    // `pgrust.condition_cache_unlogged`: an unlogged relation whose execution takes the cache (exec grants VERDICT_COND only under
    // pgrust.condition_cache_unlogged) publishes and hits as a permanent one, under the same generation rule
    let unlogged = Arc::new(PartRegistry::new(64 << 20));
    let r = fill_as(unlogged.clone(), false, true);
    assert_eq!((r.count, r.hits, r.misses), (want, 0, 3));
    let r = fill_as(unlogged, false, true);
    assert_eq!((r.count, r.hits), (want, 3), "an unlogged relation's repeat hits");
    // a new generation of the same files (the parts reopened): not the old words
    let reopened = Arc::new(PartRegistry::new(64 << 20));
    assert_eq!(fill_as(reopened, false, true).hits, 0);
}

#[test]
fn a_join_s_runtime_filter_skips_the_granules_no_build_key_reaches_and_never_filters_a_row() {
    let bf = sorted_fixture();
    let list = part_list(&bf);
    let total = list.granules();
    let base = verdict_recipe(REL_SORTED, Arc::new(PartRegistry::new(64 << 20)));
    let resolved = Resolved::for_recipe(&base, list.clone());
    let before = verdict_census()[4];
    let every = |_: i64| true;
    // granule g holds [8192 g, 8192 (g + 1)); the tail granule 7 holds 57 344 .. 59 999
    let cases: [(i64, i64, &[i64], &[u64]); 5] = [
        (20_000, 25_000, &[], &[2, 3]),
        (3, 50_000, &[3, 50_000], &[0, 6]),
        (3, 50_000, &[3, 4, 9_000, 50_000], &[0, 1, 6]),
        (70_000, 90_000, &[], &[]),
        (i64::MAX, i64::MIN, &[], &[]),
    ];
    for (lo, hi, keys, live) in cases {
        let recipe = base.clone().with_verdict_cell();
        recipe.verdicts.arm_rt(RtFilter::charged(1, lo, hi, &RtFilter::runs_of(&mut cancelpoll::step("test"), keys).unwrap(), &[], None).unwrap());
        let mut s = source(&recipe, &[], &resolved);
        let r = scan(&mut s, total, &every);
        let want: u64 = live.iter().map(|&g| (ROWS - g * GRANULE).min(GRANULE)).sum();
        assert_eq!(r.count, want, "[{lo}, {hi}] {keys:?}: every row of a kept granule reaches the probe");
        assert_eq!(r.decoded, live.len() as u64, "[{lo}, {hi}] {keys:?}: only the granules a build key reaches decode");
        assert_eq!(s.granules_skipped() + r.decoded, total);
        s.unbind();
    }
    assert!(verdict_census()[4] - before >= 3 * total);
    // a filter whose column is not a lane of the site, or whose key is not the lane's datum word, arms nothing
    let recipe = base.clone().with_verdict_cell();
    recipe.verdicts.arm_rt(RtFilter::charged(2, 0, 1, &[], &[], None).unwrap());
    let mut s = source(&recipe, &[], &resolved);
    assert_eq!(scan(&mut s, total, &every).count, ROWS);
    s.unbind();
    // PS1-22: a text key's fingerprints over an integer lane (not a code lane) arm nothing: every row reaches the probe
    let recipe = base.clone().with_verdict_cell();
    recipe.verdicts.arm_rt(RtFilter::charged(1, i64::MIN, i64::MAX, &[], &[7u128], None).unwrap());
    let mut s = source(&recipe, &[], &resolved);
    assert_eq!(scan(&mut s, total, &every).count, ROWS);
    s.unbind();
}

/// D4656: a source lets go of the runtime filters it took at its unbind. A parked source (kept warm past its statement)
/// held them until its next bind, past the statement's end, where a filter's runs (charged to its build's operator owner)
/// were an operator residue; now the execution's last rt claim frees them before that end.
#[test]
fn a_source_lets_go_of_the_runtime_filters_it_took_at_its_unbind() {
    let bf = sorted_fixture();
    let list = part_list(&bf);
    let total = list.granules();
    let base = verdict_recipe(REL_SORTED, Arc::new(PartRegistry::new(64 << 20)));
    let resolved = Resolved::for_recipe(&base, list.clone());
    let recipe = base.clone().with_verdict_cell();
    recipe.verdicts.arm_rt(RtFilter::charged(1, 20_000, 25_000, &[], &[], None).unwrap());
    // the references to the armed filter besides this probe's own clone
    let refs = || recipe.verdicts.rt().map_or(0, |f| Arc::strong_count(&f) - 1);
    assert_eq!(refs(), 1, "armed: the cell's reference alone");
    let mut s = source(&recipe, &[], &resolved);
    assert_eq!(scan(&mut s, total, &|_| true).decoded, 2, "the source took the filter: granules 2 and 3 alone decode");
    assert_eq!(refs(), 2, "the cell's reference and the source's");
    s.unbind();
    assert_eq!(refs(), 1, "unbound: the source holds none, the cell's alone");
    s.park();
    assert_eq!(refs(), 1, "parked: still none");
}

/// D3245: a site's filters in several slots skip a granule when any one proves it holds no key of its build: each alone
/// keeps its own granules, together only those both keep decode; a filter in a later slot arms with none in slot 0, and a
/// key set past one run a granule keeps only the granules its runs meet. No row is filtered.
#[test]
fn a_site_s_runtime_filters_skip_a_granule_when_any_one_rules_it_out() {
    let bf = sorted_fixture();
    let list = part_list(&bf);
    let total = list.granules();
    let base = verdict_recipe(REL_SORTED, Arc::new(PartRegistry::new(64 << 20)));
    let resolved = Resolved::for_recipe(&base, list.clone());
    let every = |_: i64| true;
    // granule g holds [8192 g, 8192 (g + 1)); the tail granule 7 holds 57 344 .. 59 999
    fn a() -> RtFilter {
        RtFilter::charged(1, 3, 50_000, &RtFilter::runs_of(&mut cancelpoll::step("test"), &[3, 9_000, 50_000]).unwrap(), &[], None).unwrap()
    }
    fn b() -> RtFilter {
        RtFilter::charged(1, 9_000, 20_000, &[], &[], None).unwrap()
    }
    fn c() -> RtFilter {
        RtFilter::charged(1, 0, 59_999, &RtFilter::runs_of(&mut cancelpoll::step("test"), &(0..100).chain(24_576..24_600).collect::<Vec<i64>>()).unwrap(), &[], None).unwrap()
    }
    let cases: [(&[(usize, fn() -> RtFilter)], &[u64]); 5] = [(&[(0, a)], &[0, 1, 6]), (&[(1, b)], &[1, 2]), (&[(0, a), (1, b)], &[1]), (&[(3, b), (2, a)], &[1]), (&[(2, c)], &[0, 3])];
    for (armed, live) in cases {
        let recipe = base.clone().with_verdict_cell();
        for &(k, f) in armed {
            recipe.verdicts.arm_rt_at(k, f());
        }
        let mut s = source(&recipe, &[], &resolved);
        let r = scan(&mut s, total, &every);
        let want: u64 = live.iter().map(|&g| (ROWS - g * GRANULE).min(GRANULE)).sum();
        let slots: Vec<usize> = armed.iter().map(|&(k, _)| k).collect();
        assert_eq!(r.count, want, "slots {slots:?}: every row of a kept granule reaches the probe");
        assert_eq!(r.decoded, live.len() as u64, "slots {slots:?}: only the granules every filter keeps decode");
        s.unbind();
    }
}

/// D3138 (colskip): a rerun set's width-1 member is bound once per execution and seeks again at every run. Its source skips
/// by each run's filter: the build's next run arms new keys (the cell's generation moves) and the source takes them at its
/// next part, never keeping an earlier run's; a cleared cell (the set re-opened, or a run whose build gave no filter) skips
/// nothing, and the run after it skips again.
#[test]
fn a_rerun_source_skips_by_each_run_s_filter_and_never_an_earlier_one() {
    let bf = sorted_fixture();
    let list = part_list(&bf);
    let total = list.granules();
    let base = verdict_recipe(REL_SORTED, Arc::new(PartRegistry::new(64 << 20)));
    let resolved = Resolved::for_recipe(&base, list.clone());
    let every = |_: i64| true;
    let recipe = base.clone().with_verdict_cell();
    let mut s = source(&recipe, &[], &resolved);
    // granule g holds [8192 g, 8192 (g + 1)); the tail granule 7 holds 57 344 .. 59 999
    let runs: [(Option<(i64, i64)>, &[u64]); 5] = [(Some((20_000, 25_000)), &[2, 3]), (Some((50_000, 50_000)), &[6]), (None, &[]), (Some((3, 4)), &[0]), (Some((70_000, 90_000)), &[])];
    for (f, live) in runs {
        match f {
            Some((lo, hi)) => recipe.verdicts.arm_rt(RtFilter::charged(1, lo, hi, &[], &[], None).unwrap()),
            None => recipe.verdicts.clear_rt(),
        }
        let r = scan(&mut s, total, &every);
        if f.is_none() {
            assert_eq!(r.count, ROWS, "a cleared filter skips nothing");
            continue;
        }
        let want: u64 = live.iter().map(|&g| (ROWS - g * GRANULE).min(GRANULE)).sum();
        assert_eq!(r.count, want, "{f:?}: every row of this run's kept granules reaches the probe");
        assert_eq!(r.decoded, live.len() as u64, "{f:?}: only the granules this run's keys reach decode");
    }
    s.unbind();
}

/// D289: a generic plan's `lane op $n`, written into the site's cell by its one live claimant, prunes as the literal's
/// zone predicate; a second live claim on the same cell (two executions of one Admitted) contests it, and both then read
/// LEGACY (every granule decodes, the rows unchanged); the cell is owned again only after every claim has ended.
#[test]
fn a_word_zone_prunes_for_its_claimant_and_a_second_live_claim_contests_the_cell() {
    let bf = sorted_fixture();
    let list = part_list(&bf);
    let total = list.granules();
    let recipe = verdict_recipe(REL_SORTED, Arc::new(PartRegistry::new(64 << 20)));
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let cell = &recipe.verdicts;
    let below = |x: i64| x < 16_384;
    let words = [16_384u64];
    let lt = || [(word_zone_arg(0, ZoneOp::Lt as u8, 8), 0usize)].into_iter();
    let full = { let mut s = source(&recipe, &[], &resolved); let r = scan(&mut s, total, &below); s.unbind(); r };
    assert_eq!((full.count, full.decoded), (16_384, total), "LEGACY (no claim yet): the zone has no predicate, every granule decodes");
    // the one live claimant writes the word-valued predicate: granules 0 and 1 alone decode
    let first = cell.claim();
    assert!(first.owner());
    cell.set(0, VERDICT_ZONE);
    cell.set_word_zone(lt(), &words, &[0]);
    let mut s = source(&recipe, &[], &resolved);
    let r = scan(&mut s, total, &below);
    assert_eq!((r.count, r.decoded), (16_384, 2), "the claimant's `a < $1` prunes as the literal's would");
    s.unbind();
    // a NULL word is no predicate
    cell.set_word_zone(lt(), &words, &[1]);
    let mut s = source(&recipe, &[], &resolved);
    assert_eq!(scan(&mut s, total, &below).decoded, total, "a NULL word prunes nothing");
    s.unbind();
    cell.set_word_zone(lt(), &words, &[0]);
    // a second live claim contests the cell: cleared, neither owns it, the sources read LEGACY
    let before = contested_census();
    let second = cell.claim();
    assert!(!first.owner() && !second.owner(), "a contested cell has no writer");
    assert_eq!(contested_census() - before, 1);
    assert_eq!(cell.get().0, VerdictCell::LEGACY);
    let mut s = source(&recipe, &[], &resolved);
    let r = scan(&mut s, total, &below);
    assert_eq!((r.count, r.decoded), (16_384, total), "contested: no word-valued predicate of either execution");
    s.unbind();
    // a third claim while one is live stays unowned; only after every claim ends is the cell owned again
    drop(second);
    let third = cell.claim();
    assert!(!third.owner(), "the cell stays contested while a claim of the contest lives");
    drop(first);
    drop(third);
    let fourth = cell.claim();
    assert!(fourth.owner(), "every claim ended: the next claimant owns the cell");
    drop(fourth);
    assert_eq!(cell.get().0, VerdictCell::LEGACY, "the last claim's end clears the cell");
}

/// PS1-05 (road nest.eagersafe): open-time zone predicates (`lane op <key cell>`: an eager-safe InitPlan's value the
/// source's open stores in its key cell) prune from the values each seek reads, a NULL value is no predicate, and the
/// cells are never the source's own: two zone cells make four key cells, which without the recipe's `zone_keys` read as a
/// TID scan's granule range (the LEGACY run below would then scan granules [40 000, total): none).
#[test]
fn open_time_zone_keys_prune_from_each_seeks_values_and_are_never_the_tid_range() {
    let bf = sorted_fixture();
    let list = part_list(&bf);
    let total = list.granules();
    let recipe = verdict_recipe(REL_SORTED, Arc::new(PartRegistry::new(64 << 20))).with_zone_keys(2);
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let cell = &recipe.verdicts;
    let claim = cell.claim();
    assert!(claim.owner());
    cell.set(0, VERDICT_ZONE);
    // `a >= <cell 2> AND a < <cell 3>`
    let preds = [(word_zone_key_arg(0, ZoneOp::Ge as u8, 8, 2), 0usize), (word_zone_key_arg(0, ZoneOp::Lt as u8, 8, 3), 0)];
    cell.set_word_zone(preds.into_iter(), &[], &[]);
    let mut s = source(&recipe, &[], &resolved);
    let granules = |lo: i64, hi: i64| (hi as u64 - 1) / GRANULE - lo as u64 / GRANULE + 1;
    // each seek's values (a re-open after the InitPlan ran again), one seek repeated
    for (lo, hi) in [(0i64, 16_384i64), (40_000, 50_000), (16_384, 16_385), (16_384, 16_385), (0, 60_000)] {
        s.seek(&[0, total, lo as u64, hi as u64], &[0; 4]).unwrap();
        let r = windows(&mut s, &|x| x >= lo && x < hi, true);
        assert_eq!((r.count, r.decoded), ((hi - lo) as u64, granules(lo, hi)), "[{lo}, {hi}): only the granules the values reach decode");
    }
    // a NULL value is no predicate: `a < 16 384` alone
    s.seek(&[0, total, 40_000, 16_384], &[0, 0, 1, 0]).unwrap();
    let r = windows(&mut s, &|x| x < 16_384, true);
    assert_eq!((r.count, r.decoded), (16_384, 2), "a NULL cell prunes nothing; the other cell's predicate stands");
    s.unbind();
    // LEGACY (the claim ended): no open-time predicate, and the four cells are not a TID range
    drop(claim);
    let mut s = source(&recipe, &[], &resolved);
    s.seek(&[0, total, 40_000, 50_000], &[0; 4]).unwrap();
    let r = windows(&mut s, &|x| (40_000..50_000).contains(&x), true);
    assert_eq!((r.count, r.decoded), (10_000, total), "LEGACY: every granule decodes, none is cut as a TID range");
    s.unbind();
}

/// A condition-cache hit keeping more of its granule than a gather pays for decodes its lane whole instead of gathering the
/// selected rows (M1-REP; H09: by the stream's fitted costs, for this int8 lane at two-byte deltas past ~1 / 7, so
/// 1 205 of 8 192 decode whole and 205 gather): the window stays covered with its selection, every row sits at its position as a gather leaves the
/// selected ones (the first selected row is not row 0), and a sparse hit's lane stays gathered (an unselected row is
/// never decoded).
#[test]
fn a_dense_condition_cache_hit_decodes_its_lane_whole_at_the_rows_positions_and_a_sparse_one_gathers() {
    let bf = sorted_fixture();
    let list = part_list(&bf);
    let total = list.granules();
    let recipe = verdict_recipe(REL_SORTED, Arc::new(PartRegistry::new(64 << 20)));
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let zone = [ZonePred::word(0, ZoneOp::Lt, 20_000)];
    // granule 2 holds [16 384, 24 576): 1 205 of its rows pass the first (> 8192 / 8), 205 the second
    for (fp, pred, dense) in [([0x5eed_0201u64, 1u64], (|x: i64| x < 20_000 && x % 3 == 0) as fn(i64) -> bool, true), ([0x5eed_0202, 2], |x: i64| x < 17_000 && x % 3 == 0, false)] {
        recipe.verdicts.set(1, fp[0]);
        recipe.verdicts.set(2, fp[1]);
        recipe.verdicts.set(0, VERDICT_ZONE | VERDICT_COND | VERDICT_CLAIM);
        let mut first = source(&recipe, &zone, &resolved);
        scan(&mut first, total, &pred);
        first.unbind();
        let mut second = source(&recipe, &zone, &resolved);
        second.seek(&[0, total], &[0, 0]).unwrap();
        let mut lanes = [Lane::NONE; MAX_LANES];
        let mut hits = 0;
        loop {
            let mut site = blank_site(lanes.as_mut_ptr());
            let mut w = Window::over(&mut site);
            let n = second.window(&mut w).unwrap();
            if n == 0 {
                break;
            }
            let covered = w.lanes().get(VK).is_some_and(|v| lane::id_kind(v.id) == lane::ID_VERDICT && v.flags & lane::VERDICT_MASK == lane::VERDICT_ALL_MATCH);
            // sortpath: granules 0-1 (zone AllMatch, the cached qual tested there too) are hits of their own
            if !covered || w.sel.is_null() || w.base().granule != 2 {
                continue;
            }
            hits += 1;
            let l = w.lanes()[0];
            assert_eq!((w.base().part_id, w.base().granule), (0, 2), "the hit is part 0's granule 2");
            let base = 2 * 8192i64;
            let sel = rows_of(&w, n);
            assert_ne!(sel[0], 0, "the first selected row is not row 0");
            let value = |i: usize| unsafe { l.datum(i) as i64 };
            assert!(sel.iter().all(|&i| value(i as usize) == base + i as i64 && pred(base + i as i64)), "a selected row holds its own value");
            let mut unselected = (0..n as usize).filter(|i| !sel.contains(&(*i as u32)));
            if dense {
                assert!(unselected.all(|i| value(i) == base + i as i64), "a dense hit's lane is whole: every row at its position");
            } else {
                assert!(unselected.any(|i| value(i) != base + i as i64), "a sparse hit's lane is gathered: an unselected row is not decoded");
            }
        }
        assert_eq!(hits, 1, "granule 2 served from its words");
        second.unbind();
    }
}

/// The serial census (M1-REP): a seek over the whole relation with no claims takes the zone census from the list's
/// footer zone columns and jumps every part wholly inside a dead range before it is looked up or pinned (the runner's
/// claims never reach one); a part with any live granule is opened as before, and the rows are the plain scan's. A list
/// without zone columns (no `resolve`) opens every part.
#[test]
fn a_serial_seek_jumps_the_parts_its_zone_census_proves_dead_without_opening_them() {
    use pgrc_am::footer::ZoneFacts;
    use pgrc_format::zonemap::ZoneMap;
    let bf = sorted_fixture();
    // parts of granules [0, 3), [3, 6), [6, 8); granule g holds a = [8192 g, 8192 (g + 1))
    for (zone, pred, opened) in [
        (ZonePred::word(0, ZoneOp::Lt, 20_000), (|x: i64| x < 20_000) as fn(i64) -> bool, 1u32),
        (ZonePred::word(0, ZoneOp::Ge, 30_000), |x: i64| x >= 30_000, 2),
        (ZonePred::word(0, ZoneOp::Eq, 45_000), |x: i64| x == 45_000, 1),
    ] {
        let want = (0..ROWS as i64).filter(|&x| pred(x)).count() as u64;
        for zoned in [true, false] {
            let list = part_list(&bf);
            let total = list.granules();
            if zoned {
                let ranges: Vec<(i64, i64)> = (0..total).map(|g| ((g * 8192) as i64, (((g + 1) * 8192).min(ROWS) - 1) as i64)).collect();
                list.set_zone_columns(vec![ZoneFacts::new(1, ZoneMap::encode(&ranges).unwrap()).unwrap()]);
            }
            let recipe = verdict_recipe(REL_SORTED, Arc::new(PartRegistry::new(64 << 20)));
            recipe.verdicts.set(0, VERDICT_ZONE);
            let resolved = Resolved::for_recipe(&recipe, list.clone());
            let mut s = source(&recipe, &[zone], &resolved);
            assert_eq!(scan(&mut s, total, &pred).count, want, "{zone:?} zoned {zoned}");
            let held = resolved.rel(REL_SORTED).unwrap().residency().parts;
            if zoned {
                assert_eq!(held, opened, "{zone:?}: only the parts with a live granule are opened");
            } else {
                assert_eq!(held, 3, "{zone:?}: without zone columns every part is opened");
            }
            s.unbind();
        }
    }
}

/// D1536 (the instrumented zone): an EXPLAIN ANALYZE execution's recipe (`with_skipped_rows`) still skips the parts and
/// granules its zone verdicts prove empty, counts their rows (`skipped_rows`), and selects no row inside a granule (no PSMA
/// window, no row_select): the rows its windows carry plus the rows it skipped are the table's, and the passing count is
/// the plain scan's, with and without the zone census, PSMA and bloom on.
#[test]
fn a_counting_recipe_skips_parts_and_granules_counts_their_rows_and_selects_no_row() {
    use pgrc_am::footer::ZoneFacts;
    use pgrc_format::zonemap::ZoneMap;
    let bf = sorted_fixture();
    // parts of granules [0, 3), [3, 6), [6, 8); granule g holds a = [8192 g, 8192 (g + 1))
    for (zone, pred, skips) in [
        (ZonePred::word(0, ZoneOp::Lt, 20_000), (|x: i64| x < 20_000) as fn(i64) -> bool, true),
        (ZonePred::word(0, ZoneOp::Ge, 30_000), |x: i64| x >= 30_000, true),
        (ZonePred::word(0, ZoneOp::Eq, 45_000), |x: i64| x == 45_000, true),
        (ZonePred::word(0, ZoneOp::Gt, 12_345), |x: i64| x > 12_345, true),
        (ZonePred::word(0, ZoneOp::Ne, 7), |x: i64| x != 7, false),
    ] {
        let want = (0..ROWS as i64).filter(|&x| pred(x)).count() as u64;
        for (zoned, enables) in [(true, VERDICT_ZONE), (false, VERDICT_ZONE | VERDICT_PSMA | VERDICT_BLOOM | VERDICT_CLAIM)] {
            let list = part_list(&bf);
            let total = list.granules();
            if zoned {
                let ranges: Vec<(i64, i64)> = (0..total).map(|g| ((g * 8192) as i64, (((g + 1) * 8192).min(ROWS) - 1) as i64)).collect();
                list.set_zone_columns(vec![ZoneFacts::new(1, ZoneMap::encode(&ranges).unwrap()).unwrap()]);
            }
            let recipe = verdict_recipe(REL_SORTED, Arc::new(PartRegistry::new(64 << 20))).with_skipped_rows(true);
            recipe.verdicts.set(0, enables);
            let resolved = Resolved::for_recipe(&recipe, list.clone());
            let mut s = source(&recipe, &[zone], &resolved);
            s.seek(&[0, total], &[0, 0]).unwrap();
            let mut lanes = [Lane::NONE; MAX_LANES];
            let (mut seen, mut passing) = (0u64, 0u64);
            loop {
                let mut site = blank_site(lanes.as_mut_ptr());
                let mut w = Window::over(&mut site);
                let n = src_window(&mut s, &mut w);
                if n == 0 {
                    break;
                }
                assert!(!w.is_answered(), "{zone:?}: a counting recipe answers no window");
                assert!(w.sel.is_null(), "{zone:?} zoned {zoned}: a counting recipe selects no row (no deletes here)");
                let l = w.lanes()[0];
                seen += n as u64;
                passing += (0..n).filter(|&i| unsafe { l.is_valid(i as usize) && pred(l.datum(i as usize) as i64) }).count() as u64;
            }
            let skipped = recipe.skipped_rows().expect("a counting recipe");
            assert_eq!(passing, want, "{zone:?} zoned {zoned}");
            assert_eq!(seen + skipped, ROWS, "{zone:?} zoned {zoned}: every row is in a window or counted skipped");
            assert_eq!(skipped > 0, skips, "{zone:?} zoned {zoned}: skipped {skipped}");
            s.unbind();
        }
    }
    // an execution that is not instrumented counts nothing
    assert_eq!(verdict_recipe(REL_SORTED, Arc::new(PartRegistry::new(64 << 20))).with_skipped_rows(false).skipped_rows(), None);
}

/// D1536 (VERDICT_COUNT_SELECT, a scan that runs to its end): a counting recipe keeps its selection (a PSMA window,
/// row_select) and counts the rows the selection drops, so the rows its windows hand the program plus the rows it counted
/// are still the table's, each once, and the passing count is unchanged.
#[test]
fn a_counting_recipe_whose_scan_runs_whole_counts_the_rows_its_selection_drops() {
    use pgrc_am::footer::ZoneFacts;
    use pgrc_format::zonemap::ZoneMap;
    let bf = sorted_fixture();
    for (zone, pred) in [
        (zone_word(0, ZoneOp::Lt, 20_000), (|x: i64| x < 20_000) as fn(i64) -> bool),
        (zone_word(0, ZoneOp::Eq, 45_000), |x: i64| x == 45_000),
        (zone_word(0, ZoneOp::Gt, 12_345), |x: i64| x > 12_345),
        (zone_word(0, ZoneOp::Ne, 7), |x: i64| x != 7),
    ] {
        let want = (0..ROWS as i64).filter(|&x| pred(x)).count() as u64;
        for zoned in [true, false] {
            let list = part_list(&bf);
            let total = list.granules();
            if zoned {
                let ranges: Vec<(i64, i64)> = (0..total).map(|g| ((g * 8192) as i64, (((g + 1) * 8192).min(ROWS) - 1) as i64)).collect();
                list.set_zone_columns(vec![ZoneFacts::new(1, ZoneMap::encode(&ranges).unwrap()).unwrap()]);
            }
            let recipe = verdict_recipe(REL_SORTED, Arc::new(PartRegistry::new(64 << 20))).with_skipped_rows(true);
            recipe.verdicts.set(0, VERDICT_ZONE | VERDICT_PSMA | VERDICT_COUNT_SELECT);
            let resolved = Resolved::for_recipe(&recipe, list.clone());
            let mut s = source(&recipe, &[zone], &resolved);
            s.seek(&[0, total], &[0, 0]).unwrap();
            let mut lanes = [Lane::NONE; MAX_LANES];
            let (mut seen, mut passing) = (0u64, 0u64);
            loop {
                let mut site = blank_site(lanes.as_mut_ptr());
                let mut w = Window::over(&mut site);
                let n = src_window(&mut s, &mut w);
                if n == 0 {
                    break;
                }
                assert!(!w.is_answered(), "{zone:?}: a counting recipe answers no window");
                let l = w.lanes()[0];
                let rows = rows_of(&w, n);
                seen += rows.len() as u64;
                passing += rows.iter().filter(|&&i| unsafe { l.is_valid(i as usize) && pred(l.datum(i as usize) as i64) }).count() as u64;
            }
            let skipped = recipe.skipped_rows().expect("a counting recipe");
            assert_eq!(passing, want, "{zone:?} zoned {zoned}");
            assert_eq!(seen + skipped, ROWS, "{zone:?} zoned {zoned}: every row is handed on or counted, once");
            s.unbind();
        }
    }
}

/// A word-valued zone predicate (`lane op value`).
fn zone_word(lane: u16, op: ZoneOp, value: i64) -> ZonePred {
    ZonePred::word(lane, op, value)
}

fn src_window(s: &mut GranuleRangeSource, w: &mut Window<'_>) -> u32 {
    s.window(w).unwrap()
}
