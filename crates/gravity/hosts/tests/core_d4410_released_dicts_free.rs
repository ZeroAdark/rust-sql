//! D4410: the dictionaries of the parts a running statement released go with the part cache's trim. A code-word
//! statement (D3823's uses-only release whose breaker keeps code words) opened its dictionaries in the list's cells while
//! the part cache was under its level and borrowed them for its life: its clone of the list kept them although it had
//! released their parts, so the evicted hook (which dropped `LISTS`'s clone only) and the trim after it freed nothing
//! (an 8-worker analytic run at 8G: 3-5.8 GB of such images at every trim over target). Now the statement holds its own `Arc` of each
//! dictionary and lets it go with the part; the list's cells are a warm cache the evicted hook empties. A statement whose
//! breaker keeps images by reference (a uses-only release without its own dictionaries: TopK, a hash build) keeps its
//! dictionaries to its end, as before. A process of its own: the hooks, the part cache's bytes and its budget are global.

use std::sync::Arc;

use gravity_abi::ctx::lane;
use gravity_abi::Lane;
use gravity_driver::{blank_site, Source, Window};
use gravity_hosts::ops::scan::pgrc::granule_source::{GranuleRangeSource, GranuleRecipe, Resolved};
use pgrc_read::{partcache, PartRegistry};

#[path = "common/granule_fixtures.rs"]
mod granule_fixtures;
use granule_fixtures::*;

/// The rows of claim `[lo, hi)` through the code lane's windows.
fn code_rows(src: &mut GranuleRangeSource, lo: u64, hi: u64) -> u64 {
    src.seek(&[lo, hi], &[0, 0]).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    let mut rows = 0;
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = src.window(&mut w).unwrap();
        if n == 0 {
            return rows;
        }
        assert_eq!(w.lanes()[0].kind, lane::KIND_CODES);
        rows += n as u64;
    }
}

fn render(src: &mut GranuleRangeSource, p: u32, code: u32) -> Vec<u8> {
    let (ptr, n) = src.dict_value(p, 0, code).unwrap();
    unsafe { core::slice::from_raw_parts(ptr as *const u8, n as usize) }.to_vec()
}

#[test]
fn a_running_statements_released_parts_give_their_dictionaries_back_to_the_trim() {
    gravity_hosts::ops::scan::pgrc::residency::arm_part_cache();
    partcache::set_budget(u64::MAX);
    let bf = text_fixture();
    let reg = Arc::new(PartRegistry::new(64 << 20));

    // a code-word statement under the part cache's level: its dictionaries also go to the list's cells for the next statement
    let list = part_list_at(&bf, 44100);
    assert_eq!(list.len(), 2);
    let recipe = GranuleRecipe::for_test_codes(REL_TEXT, &[1], &[1], Arc::clone(&reg));
    let mut resolved = Resolved::new();
    resolved.set_part_uses_release(true);
    resolved.set_part_own_dicts(true);
    resolved.set_part_release_at(Some(0)); // D3823 gate: release at once
    resolved.set_part_share(Some(u64::MAX)); // under the level
    resolved.install(REL_TEXT, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&resolved)).unwrap();
    let want = render(&mut src, 0, 0);
    let rows: u64 = (0..list.granules()).map(|g| code_rows(&mut src, g, g + 1)).sum();
    assert_eq!(rows, list.rows());
    assert_eq!(resolved.part_release_counts().0, 2, "both parts released while the statement runs");
    assert_eq!(list.shared_dicts(), 2, "with room the list keeps both dictionaries for the next statement");
    let held = partcache::bytes();
    assert!(held > 0, "the dictionaries' images are cached");

    // pressure: the trim walks the released parts, runs the evicted hook, and walks them again
    reg.trim_contents(0);
    let left = partcache::bytes();
    assert_eq!(left, 0, "the released parts' dictionary images stay cached ({left} of {held} bytes): the running statement's list keeps them");
    assert_eq!(list.shared_dicts(), 0, "the hook let the list's warm cells go");
    assert_eq!(render(&mut src, 0, 0), want, "a code word renders the same bytes after the trim (its dictionary opened again)");
    src.unbind();
    drop(src.release_rel(REL_TEXT));
    drop(resolved);

    // a statement whose breaker keeps images by reference (no dictionaries of its own) keeps them to its end
    let list = part_list_at(&bf, 44200);
    let recipe = GranuleRecipe::for_test_codes(REL_TEXT, &[1], &[1], Arc::clone(&reg));
    let mut keeps = Resolved::new();
    keeps.set_part_uses_release(true);
    keeps.set_part_release_at(Some(0));
    keeps.install(REL_TEXT, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&keeps)).unwrap();
    let want = render(&mut src, 1, 0);
    let rows: u64 = (0..list.granules()).map(|g| code_rows(&mut src, g, g + 1)).sum();
    assert_eq!(rows, list.rows());
    assert_eq!(keeps.part_release_counts().0, 2, "its parts' uses go back");
    reg.trim_contents(0);
    assert!(partcache::bytes() > 0, "its dictionaries stay: its kept images point into them");
    assert_eq!(render(&mut src, 1, 0), want);
    src.unbind();
    drop(src.release_rel(REL_TEXT));
    drop(keeps);
    reg.trim_contents(0);
    assert_eq!(partcache::bytes(), 0, "at its end they go too");
}
