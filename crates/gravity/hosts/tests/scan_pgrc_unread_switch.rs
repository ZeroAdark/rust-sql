//! Repeated count scans exercise mode changes against warm process-wide entries and inspect decoded column lanes.
use std::sync::{Arc, Mutex};

use gravity_abi::ctx::lane as lf;
use gravity_abi::Lane;
use gravity_driver::{blank_site, Source, Window};
use gravity_hosts::ops::scan::pgrc::cond_cache;
use gravity_hosts::ops::scan::pgrc::granule_source::{unread_census, GranuleRangeSource, GranuleRecipe, PartList, Resolved};
use gravity_program::protos::{VERDICT_COND, VERDICT_UNREAD};
use pgrc_read::PartRegistry;
use guc_tables::ConditionCacheMode;

#[path = "common/granule_fixtures.rs"]
mod granule_fixtures;
use granule_fixtures::*;

static SERIAL: Mutex<()> = Mutex::new(());

/// One execution's windows: `(part, granule, rows presented, lane 0 decoded)`, and the condition cache's hits.
struct Run {
    wins: Vec<(u32, u32, u32, bool)>,
    hits: u64,
}

fn int8_profile() -> pgrc_meta::profile::MetaProfile {
    let c = pgrc_qa::int8_col(1);
    pgrc_meta::profile::MetaProfile::derive_or_opaque(c.class, c.collation_class, c.semantics)
}

/// One count-only execution of `a % 6 = 0` over `list` with the verdict words `(enables, fingerprint)` its prepare set;
/// on a miss window the program's test of each row is written into the verdict lane (what the cache publishes).
fn run(list: &Arc<PartList>, registry: &Arc<PartRegistry>, enables: u64, fp: [u64; 2]) -> Run {
    run_mode(list, registry, enables, fp, ConditionCacheMode::Full)
}

fn run_mode(list: &Arc<PartList>, registry: &Arc<PartRegistry>, enables: u64, fp: [u64; 2], mode: ConditionCacheMode) -> Run {
    guc_tables::backing::set_pgrust_condition_cache(mode as i32);
    let recipe = GranuleRecipe::for_test(4290, &[1], registry.clone()).with_verdict_cell().with_verdict_lane(2).with_profile(0, int8_profile());
    recipe.verdicts.set(1, fp[0]);
    recipe.verdicts.set(2, fp[1]);
    recipe.verdicts.set(0, enables);
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    guc_tables::backing::set_pgrust_condition_cache(ConditionCacheMode::Full as i32);
    let mut src = GranuleRangeSource::new(recipe);
    src.zone(&[], &[0], &[0]);
    src.bind(&binding_ref(&resolved)).unwrap();
    let c0 = cond_cache::census();
    src.seek(&[0, list.granules()], &[0, 0]).unwrap();
    let (mut lanes, mut wins) = ([Lane::NONE; MAX_LANES], Vec::new());
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = src.window(&mut w).unwrap();
        if n == 0 {
            break;
        }
        // the verdict lane is a cache-taking execution's only
        let (a, v) = (w.lanes()[0], w.lanes().get(2).copied().unwrap_or(Lane::NONE));
        let hit = !v.ptr.is_null() && v.flags & lf::VERDICT_MASK == lf::VERDICT_ALL_MATCH;
        if !v.ptr.is_null() && !hit && !a.ptr.is_null() {
            for i in 0..n as usize {
                // SAFETY: a decoded int8 lane holds the window's rows; the miss window's verdict lane is its granule's words.
                unsafe {
                    if (a.ptr as *const i64).add(i).read_unaligned() % 6 == 0 {
                        *(v.ptr as *mut u64).add(i / 64) |= 1 << (i % 64);
                    }
                }
            }
        }
        wins.push((w.base().part_id, w.base().granule, n, !a.ptr.is_null()));
    }
    src.unbind();
    Run { wins, hits: cond_cache::census()[0] - c0[0] }
}

#[test]
fn the_switch_decides_whether_a_repeated_count_reads_its_columns() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let policy = pgrc_write::writer::PartCutPolicy { max_rows: 20_000, ..Default::default() };
    let bf = pgrc_qa::corpus::build_fixture(pgrc_qa::corpus::int8_fixture("gravity_unread_switch", 7327, ROWS, vec![], policy, |i| Some(i as i64)));
    let list = part_list(&bf);
    let r = Arc::new(PartRegistry::new(64 << 20));
    cond_cache::set_capacity(64 << 20);

    // off (exec's prepare under pgrust.condition_cache = off: VERDICT_COND and the fingerprint dropped, VERDICT_UNREAD as
    // the program set it): the first and the repeated execution do the same work, every window decoded, none answered
    let off = (VERDICT_UNREAD, [0, 0]);
    let u0 = unread_census();
    let first = run(&list, &r, off.0, off.1);
    let again = run(&list, &r, off.0, off.1);
    assert!(first.wins.iter().all(|w| w.3), "off: every window decodes its column");
    assert_eq!(again.wins, first.wins, "off: the repeated execution decodes the same windows");
    assert_eq!((first.hits, again.hits, unread_census() - u0), (0, 0, 0), "off: no hit, no unread answer");

    // on: the first execution misses and publishes, the repeated one is answered from the stored counts, no column read
    let on = (VERDICT_COND | VERDICT_UNREAD, [0x5eed_0427, 9]);
    let miss = run(&list, &r, on.0, on.1);
    assert!(miss.wins.iter().all(|w| w.3), "on: a miss decodes its column");
    let u1 = unread_census();
    let hit = run(&list, &r, on.0, on.1);
    assert!(hit.hits > 0 && hit.wins.iter().all(|w| !w.3), "on: the repeated count reads no column");
    assert!(unread_census() > u1, "on: unread answers counted");

    // off again, the entry resident: still no hit and every window decoded
    let after = run(&list, &r, off.0, off.1);
    assert_eq!((after.hits, after.wins.clone()), (0, first.wins.clone()), "off: the resident entry is not read");
}

#[test]
fn granule_mode_rechecks_possible_matches_and_skips_only_empty_granules() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let policy = pgrc_write::writer::PartCutPolicy { max_rows: 20_000, ..Default::default() };
    let bf = pgrc_qa::corpus::build_fixture(pgrc_qa::corpus::int8_fixture("gravity_granule_mode", 7328, ROWS * 2, vec![], policy, |i| Some(if i < ROWS { i as i64 } else { 1 })));
    let list = part_list(&bf);
    let r = Arc::new(PartRegistry::new(64 << 20));
    cond_cache::set_capacity(64 << 20);
    let enables = VERDICT_COND | VERDICT_UNREAD;
    let fp = [0x5eed_0428, 9];
    let full = run_mode(&list, &r, enables, fp, ConditionCacheMode::Full);
    assert!(full.wins.iter().all(|w| w.3));
    let hot = run_mode(&list, &r, enables, fp, ConditionCacheMode::Full);
    assert!(hot.hits > 0 && hot.wins.iter().all(|w| !w.3));
    let before = unread_census();
    let first = run_mode(&list, &r, enables, fp, ConditionCacheMode::Granule);
    assert_eq!(first.wins, full.wins, "granule cannot consume resident full counts or masks");
    let again = run_mode(&list, &r, enables, fp, ConditionCacheMode::Granule);
    assert!(again.hits > 0, "an empty granule can be reused");
    assert!(!again.wins.is_empty() && again.wins.iter().all(|w| w.3), "possible matches always read their column");
    assert!(again.wins.len() < first.wins.len(), "only empty granules are skipped");
    assert_eq!(unread_census(), before, "granule mode never uses unread count answers");
    let off = run_mode(&list, &r, enables, fp, ConditionCacheMode::Off);
    assert_eq!(off.wins, full.wins);
    assert_eq!(off.hits, 0);
    let rows = run_mode(&list, &r, enables, fp, ConditionCacheMode::Rows);
    assert_eq!(rows.wins, full.wins, "rows cannot consume a full-mode entry");
    let rows_hot = run_mode(&list, &r, enables, fp, ConditionCacheMode::Rows);
    assert!(rows_hot.hits > 0);
    assert_eq!(unread_census(), before, "rows mode never uses unread count answers");
    let granule_again = run_mode(&list, &r, enables, fp, ConditionCacheMode::Granule);
    assert_eq!(granule_again.wins, again.wins, "rows entries do not strengthen granule reuse");
}
