//! D4709: a claim's prefetch and advice counts reach the process totals in one add per nonzero count, not per lane and per
//! count (a hot wide-row LIKE scan: ~12k RMWs on nine shared atomics per statement). One real claim over four lanes (each lane its own
//! cursor, five counts each): the stat RMWs the claim made are at most the totals that moved, and the totals are what the
//! lanes read. Red where the lanes add into the globals one by one (20 RMWs a claim whatever the counts).
#[path = "common/granule_fixtures.rs"]
mod granule_fixtures;

use std::sync::Arc;

use granule_fixtures::*;
use gravity_driver::{blank_site, Source, Window};
use gravity_hosts::ops::scan::pgrc::granule_source::{read_census, stat_rmws, GranuleRangeSource, GranuleRecipe, Resolved, ADVISE_BYTES, ADVISE_CALLS};
use pgrc_read::PartRegistry;

#[test]
fn a_claim_over_four_lanes_adds_to_the_stat_totals_once_per_moved_total() {
    if !cfg!(debug_assertions) {
        eprintln!("skipped: the RMW witness counts in debug builds");
        return;
    }
    let bf = sorted_fixture();
    let list = part_list(&bf);
    let recipe = GranuleRecipe::for_test(REL_SORTED, &[1, 1, 1, 1], Arc::new(PartRegistry::new(64 << 20)));
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&resolved)).unwrap();
    let (c0, a0, b0, r0) = (read_census(), ADVISE_CALLS.load(std::sync::atomic::Ordering::Relaxed), ADVISE_BYTES.load(std::sync::atomic::Ordering::Relaxed), stat_rmws());
    src.seek(&[0, list.granules()], &[0, 0]).unwrap();
    let mut lanes = [gravity_abi::Lane::NONE; MAX_LANES];
    let mut rows = 0u64;
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = src.window(&mut w).unwrap();
        if n == 0 {
            break;
        }
        rows += n as u64;
    }
    assert_eq!(rows, ROWS);
    let (c1, a1, b1, r1) = (read_census(), ADVISE_CALLS.load(std::sync::atomic::Ordering::Relaxed), ADVISE_BYTES.load(std::sync::atomic::Ordering::Relaxed), stat_rmws());
    let moved = (0..5).filter(|&i| c1[i] != c0[i]).count() as u64 + u64::from(a1 != a0) + u64::from(b1 != b0);
    let made = r1 - r0;
    // each total that moved took one add for the whole scan's claims, plus the in-flight budget's add per advising claim
    assert!(made <= moved * list.len() as u64 + (a1 - a0), "{made} stat RMWs for {moved} moved totals over {} parts ({} advice calls)", list.len(), a1 - a0);
    src.unbind();
}
