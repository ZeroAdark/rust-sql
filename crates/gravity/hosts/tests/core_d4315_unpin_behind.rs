//! D4315: a releasing scan pins only about its read-ahead, not the whole part it reads. At W8 each worker's claim is a
//! whole part; the claim read every extent of every lane at the part's entry, and the part's use plus the cursors' clones
//! of every extent they loaded kept the whole part (big-ord: 55-64 MB) pinned until the claim left it, so 8 workers pinned
//! 8 whole parts and the trim could evict none of it. Now each lane reads ahead about a read run, and the extents behind
//! the windows are passed: still cached (a hot repeat finds them), but a trim past the cache's level evicts them while the
//! part stays in use, and the cache's floor (`in_use_bytes`) counts only what the scan pins. A process of its own: the part
//! cache's budget and census are process-wide.

use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use gravity_abi::Lane;
use gravity_driver::{blank_site, BindingRef, CommandId, Identity, PerTuple, Pins, Snapshot, Source, Window};
use gravity_hosts::ops::scan::pgrc::granule_source::{GranuleRangeSource, GranuleRecipe, PartIoSpec, PartList, PartSpec, Resolved};
use pgrc_format::dirlayout::part_file_name;
use pgrc_qa::corpus::{build_fixture, int8_fixture};
use pgrc_read::{PartExpect, PartRegistry};
use pgrc_write::writer::PartCutPolicy;

const MAX_LANES: usize = 32;
/// Sixteen bands of 65,536 rows: sixteen extents per stream in one part.
const ROWS: u64 = 16 * 65_536;
const REL: u32 = 4531;
static FLAG: AtomicU8 = AtomicU8::new(0);

fn binding_ref(resolved: &Resolved) -> BindingRef<'_> {
    BindingRef { snapshot: Snapshot(0), command_id: CommandId(0), params: &[], abort_flag: &FLAG, pins: Pins(0), per_tuple: PerTuple(0), identity: Identity::default(), resolved: Some(resolved), ordinal: None }
}

/// One part of a 1M-row int8 column whose words do not encode small (about 8 bytes a row on file).
fn list() -> Arc<PartList> {
    let bf = build_fixture(int8_fixture("gravity_d4315_unpin", REL as u64, ROWS, vec![], PartCutPolicy { max_rows: ROWS, ..PartCutPolicy::default() }, |i| Some(i.wrapping_mul(0x9E37_79B9_7F4A_7C15) as i64)));
    let specs = bf
        .manifest
        .parts
        .iter()
        .enumerate()
        .map(|(i, rec)| {
            let bytes = bf.files.get(&part_file_name(rec.part_no)).expect("part file").clone();
            PartSpec { io: PartIoSpec::Mem { bytes: Arc::from(bytes.as_slice()), dev: 7, ino: 4315 + i as u64 }, expect: PartExpect::none(), rows: rec.rows, granules: rec.granule_count, deletes: None, part: None }
        })
        .collect();
    let list = Arc::new(PartList::new(specs));
    assert_eq!(list.len(), 1, "the fixture is one part");
    list
}

/// One statement's scan of the whole part as one claim (a W8 worker's whole-part claim), the uses released as the
/// server's are; after each window `(registry in-use bytes, part cache bytes)`.
fn scan(list: &Arc<PartList>, registry: &Arc<PartRegistry>) -> Vec<(u64, u64)> {
    let recipe = GranuleRecipe::for_test(REL, &[1], Arc::clone(registry));
    let mut resolved = Resolved::new();
    resolved.set_part_uses_release(true);
    resolved.set_part_release_at(None);
    resolved.install(REL, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&resolved)).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    src.seek(&[0, list.granules()], &[0, 0]).unwrap();
    let mut seen = Vec::new();
    let mut rows = 0u64;
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = src.window(&mut w).unwrap();
        if n == 0 {
            break;
        }
        rows += n as u64;
        seen.push((registry.in_use_bytes(), pgrc_read::partcache::bytes()));
    }
    assert_eq!(rows, ROWS);
    src.unbind();
    seen
}

#[test]
fn a_whole_part_claim_pins_its_read_ahead_not_the_part() {
    let list = list();
    let registry = Arc::new(PartRegistry::new(1 << 30));

    // memory to spare: nothing is evicted, and the part's images stay cached for the next run
    pgrc_read::partcache::set_budget(u64::MAX);
    scan(&list, &registry);
    let whole = pgrc_read::partcache::bytes();
    assert!(whole >= 6 << 20, "the part's sixteen extents are cached after the scan: {whole} bytes");
    let evicted = pgrc_read::partcache::counters().2;
    scan(&list, &registry);
    assert_eq!(pgrc_read::partcache::bytes(), whole, "a hot repeat finds every image cached");
    assert_eq!(pgrc_read::partcache::counters().2, evicted, "with room nothing is evicted");

    // past the level: the trim takes what the scan passed while the part stays in use
    let cap = whole / 8;
    pgrc_read::partcache::set_budget(cap);
    registry.trim_contents(0);
    assert_eq!(pgrc_read::partcache::bytes(), 0, "the unused part's images went back");
    let seen = scan(&list, &registry);
    let pinned = seen.iter().map(|s| s.0).max().unwrap();
    let resident = seen.iter().map(|s| s.1).max().unwrap();
    assert!(pinned <= whole / 2, "pinned at most about the read-ahead: {pinned} of the part's {whole} bytes (trunk: the whole part) {seen:?}");
    assert!(resident <= whole / 2, "resident at most about the read-ahead: {resident} of {whole} bytes {seen:?}");
}
