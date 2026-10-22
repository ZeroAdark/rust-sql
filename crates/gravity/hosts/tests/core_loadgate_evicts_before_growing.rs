//! The part-cache LOAD GATE (LADDER-DESIGN section 3): a load into the part cache asks first. At the cache's budget the
//! load evicts the parts no running statement uses (finished parts: the elastic bytes) BEFORE its image is counted, so
//! the cache never grows past its budget while finished parts sit in it. Only bytes a running scan pins can keep a load
//! from fitting, and then the load still proceeds (the cache never refuses a running scan's read).
//! On trunk a load never asked: the cache grew by every image a scan faulted, and only the next part's first touch
//! (`RelParts::part`'s `trim_if_over`) or the next statement's start brought it back, so a scan inside one part held its
//! images on top of every finished part's. A process of its own: the part cache's census and budget are process-wide.

use std::sync::Arc;

use pgrc_format::dirlayout::part_file_name;
use pgrc_qa::corpus::{build_fixture, int8_fixture};
use pgrc_read::io::{MemPartIo, PartIo};
use pgrc_read::{partcache, OpenPart, PartExpect, PartPin};
use pgrc_write::writer::PartCutPolicy;

const ROWS: u64 = 60_000;
const REL: u64 = 4517;

/// The three part files of a 60,000-row int8 fixture.
fn part_files() -> Vec<Vec<u8>> {
    let bf = build_fixture(int8_fixture("gravity_loadgate", REL, ROWS, vec![], PartCutPolicy { max_rows: 20_000, ..PartCutPolicy::default() }, |i| Some((i as i64 * 7919) % 100_003)));
    let files: Vec<Vec<u8>> = bf.manifest.parts.iter().map(|rec| bf.files.get(&part_file_name(rec.part_no)).expect("part file").clone()).collect();
    assert_eq!(files.len(), 3, "the fixture cut into three parts");
    files
}

/// Part `i` opened in the process part registry (the one the server's scans and the cache's trims use).
fn open(files: &[Vec<u8>], i: usize) -> PartPin {
    let io = MemPartIo::new(files[i].clone(), 77, 4400 + i as u64);
    pgrc_am::inval::registry().open_pinned(&PartExpect::none(), move || Ok(Box::new(io) as Box<dyn PartIo>)).expect("open")
}

/// Every listed section of `part` faulted one by one; the most the process part cache held after any of the loads.
fn load_all(part: &Arc<OpenPart>) -> u64 {
    let mut peak = partcache::bytes();
    for idx in 0..part.sections().len() {
        part.section_bytes(idx).expect("section");
        peak = peak.max(partcache::bytes());
    }
    peak
}

#[test]
fn a_load_at_the_caches_budget_evicts_finished_parts_before_the_cache_grows() {
    let files = part_files();
    // the server's arming at a statement's start: the cache's hooks; the budget is set below
    gravity_hosts::ops::scan::pgrc::residency::arm_part_cache();
    partcache::set_budget(u64::MAX);

    // a finished part: read whole by a statement whose use has ended (nothing pins its images)
    let a = open(&files, 0);
    let a_use = a.part().use_part();
    load_all(a.part());
    drop(a_use);
    let a_bytes = a.part().cache_bytes();
    assert!(a_bytes > 0, "the finished part's images are cached");
    assert_eq!(partcache::bytes(), a_bytes, "the cache holds the finished part only");

    // the cache's budget: room for one part and a half. A scan now reads a second part of the same size: the finished
    // part's images are elastic, so the loads make room by evicting them before the cache grows past its budget
    let budget = a_bytes + a_bytes / 2;
    partcache::set_budget(budget);
    let b = open(&files, 1);
    let b_use = b.part().use_part();
    let peak = load_all(b.part());
    let b_bytes = b.part().cache_bytes();
    assert!(b_bytes > budget - a_bytes, "the second part's images do not fit beside the first's ({b_bytes} > {})", budget - a_bytes);
    assert!(peak <= budget, "a load past the budget evicted the finished part before the cache grew: peak {peak} <= budget {budget} (trunk: {} = both parts)", a_bytes + b_bytes);
    assert_eq!(a.part().cache_bytes(), 0, "the finished part's images went");
    assert_eq!(b.part().cache_bytes(), b_bytes, "the running scan's part keeps every image it loaded");

    // only pinned bytes block a load: with the running scan's part over the budget alone, a third part's load still
    // proceeds (a running scan's read is never refused) and the cache holds both running parts
    partcache::set_budget(b_bytes / 2);
    let c = open(&files, 2);
    let c_use = c.part().use_part();
    load_all(c.part());
    assert!(c.part().cache_bytes() > 0, "the load past a budget pinned bytes fill proceeds");
    assert_eq!(b.part().cache_bytes(), b_bytes, "a running scan's images are never evicted for another load");
    drop((b_use, c_use));
}
