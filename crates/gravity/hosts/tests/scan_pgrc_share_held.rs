//! D2985 / D3113: `StmtDicts::shares` (a streaming statement keeps its dictionaries in the list's shared cells while the
//! process part cache is under the level, else in its own cells) against a part-cache image a holder keeps after its part
//! let it go: at a key eviction (`OpenPart::evict_extent_images`), or with the part dropped. The image's bytes stay alive
//! with the holder, so they count against the level until its last drop, and then leave it exactly once.
//!
//! Its own test binary: the part cache's census, its account hook and the memory governor are process-wide.

use std::sync::{Arc, Mutex};

use gravity_abi::ctx::lane;
use gravity_abi::Lane;
use gravity_driver::{blank_site, Source, Window};
use gravity_hosts::ops::scan::pgrc::granule_source::{GranuleRangeSource, GranuleRecipe, Resolved};
use pgrc_format::dirlayout::part_file_name;
use pgrc_qa::corpus::{open_part_bytes, BuiltFixture};
use pgrc_read::PartRegistry;

#[path = "common/granule_fixtures.rs"]
mod granule_fixtures;
use granule_fixtures::*;

/// This binary's tests share the process's part cache and governor: one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

/// The account Gravity's runner installs (`residency::part_cache_account`): the part cache's charges and releases to the
/// memory governor, whose part-cache account `StmtDicts::shares` reads beside the census.
fn account(n: u64, charged: bool, rec: &mut pgrc_read::partcache::Charged) {
    let n = usize::try_from(n).unwrap_or(usize::MAX);
    let mut c = mcx::governor::CacheCharge { slot: rec.slot, epoch: rec.epoch, bytes: rec.bytes };
    if charged {
        mcx::governor::charge_cache(n, "pgrc part cache", &mut c);
    } else {
        mcx::governor::release_cache(n, &mut c);
    }
    *rec = pgrc_read::partcache::Charged { slot: c.slot, epoch: c.epoch, bytes: c.bytes };
}

fn census() -> u64 {
    let b = pgrc_read::partcache::bytes();
    assert_eq!(mcx::governor::cache_bytes() as u64, b, "the governor's part-cache account follows the census");
    b
}

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

/// One streaming statement over a fresh list of `bf`'s two parts, sharing its dictionaries below `level`: the list's shared
/// dictionaries after it, and the census at its end (no dictionary it opened saw more).
fn streaming(bf: &BuiltFixture, recipe: &GranuleRecipe, level: u64) -> (usize, u64) {
    let list = part_list(bf);
    let mut resolved = Resolved::new();
    resolved.set_part_release(true);
    resolved.set_part_share(Some(level));
    resolved.install(REL_TEXT, list.clone());
    let mut src = GranuleRangeSource::new(recipe.clone());
    src.bind(&binding_ref(&resolved)).unwrap();
    assert_eq!(code_rows(&mut src, 0, list.granules()), list.rows());
    let end = pgrc_read::partcache::bytes();
    src.unbind();
    (list.shared_dicts(), end)
}

/// The fixture, its recipe, and the most a streaming statement over it adds to the census (`grow`) once its parts are warm.
fn setup() -> (BuiltFixture, GranuleRecipe, u64) {
    pgrc_read::partcache::set_account_hook(account);
    let bf = text_fixture();
    let recipe = GranuleRecipe::for_test_codes(REL_TEXT, &[1], &[1], Arc::new(PartRegistry::new(64 << 20)));
    assert_eq!(streaming(&bf, &recipe, u64::MAX).0, 2, "under the level: the list's cells");
    let before = census();
    let (shared, end) = streaming(&bf, &recipe, u64::MAX);
    assert_eq!(shared, 2);
    (bf, recipe, end.saturating_sub(before))
}

/// Another open of the fixture's first part (its own identity: its images are not the scanned parts'), and the index of its
/// largest section.
fn other_part(bf: &BuiltFixture, ino: u64) -> (Arc<pgrc_read::OpenPart>, usize) {
    let bytes = bf.files.get(&part_file_name(bf.manifest.parts[0].part_no)).expect("part file");
    let part = open_part_bytes(bytes, ino).expect("open");
    let idx = (0..part.sections().len()).max_by_key(|&i| part.sections()[i].len).expect("a section");
    (part, idx)
}

#[test]
fn a_clone_held_across_its_parts_key_eviction_counts_against_the_share_level() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (bf, recipe, grow) = setup();
    let base = census();
    let (part, idx) = other_part(&bf, 9_100);
    let held = part.section_bytes(idx).expect("section");
    let len = held.len() as u64;
    assert!(len >= 2 * grow + 2, "the held image ({len} bytes) is past twice what a statement adds ({grow})");
    let rest = part.resident() - len;
    let key = (part.sections()[idx].off, part.sections()[idx].len);
    let evicted = part.evict_extent_images(&[key]);
    // the bytes alive without the held image, and a level half the image above them and what a statement adds
    let level = base + rest + grow + len / 2;
    assert_eq!(streaming(&bf, &recipe, level).0, 0, "the image a clone holds past its eviction {evicted:?} counts: its own cells");
    drop(held);
    part.evict_extent_images(&[key]);
    assert_eq!(census(), base + rest, "the image's bytes leave the census once, at its last drop");
    assert_eq!(streaming(&bf, &recipe, level).0, 2, "with the image gone, under the level: the list's cells");
    drop(part);
    assert_eq!(census(), base);
}

#[test]
fn a_clone_held_past_its_parts_drop_counts_against_the_share_level() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (bf, recipe, grow) = setup();
    let base = census();
    let (part, idx) = other_part(&bf, 9_200);
    let held = part.section_bytes(idx).expect("section");
    let len = held.len() as u64;
    assert!(len >= 2 * grow + 2, "the held image ({len} bytes) is past twice what a statement adds ({grow})");
    drop(part);
    let level = base + grow + len / 2;
    assert_eq!(streaming(&bf, &recipe, level).0, 0, "the image a clone holds past its part's drop counts: its own cells");
    drop(held);
    assert_eq!(census(), base, "the image's bytes leave the census once, at its last drop");
    assert_eq!(streaming(&bf, &recipe, level).0, 2, "with the image gone, under the level: the list's cells");
}
