//! D3994 (level gate): a running statement's part that every claim left goes back when memory is short, though the part
//! cache is inside its budget. On trunk such a part stayed held (deferred) to the statement's end whenever the cache had
//! room by its own budget: in D3994's broker-on stress the box was short (B 15 GiB, free 0) while the cache was inside its
//! budget, and one statement held all 511 parts (7.8 GiB) to its end, counted in the cache's floor. Now the release reads
//! the memory policy's short signal (`mempolicy::cache::memory_short`: B's free bytes under its last quarter with the
//! governor off), the same signal as the parked parts' gate. With room the part stays held for the statement's next run
//! (D3268). The arm follows the policy's one switch (`mempolicy::cache::release_short` under `PARKED_GATE`): the Cap arm
//! adds nothing past the level. A process of its own: the signals (the fallback limit, B's snapshot, the cache's budget)
//! are process-wide.

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
const ROWS: u64 = 60_000;
const REL: u32 = 4511;
const G: u64 = 1 << 30;
static FLAG: AtomicU8 = AtomicU8::new(0);

fn binding_ref(resolved: &Resolved) -> BindingRef<'_> {
    BindingRef { snapshot: Snapshot(0), command_id: CommandId(0), params: &[], abort_flag: &FLAG, pins: Pins(0), per_tuple: PerTuple(0), identity: Identity::default(), resolved: Some(resolved), ordinal: None }
}

/// Three parts of a 60,000-row int8 fixture.
fn list() -> Arc<PartList> {
    let bf = build_fixture(int8_fixture("gravity_d3994_short", REL as u64, ROWS, vec![], PartCutPolicy { max_rows: 20_000, ..PartCutPolicy::default() }, |i| Some((i as i64 * 31) % 1000)));
    let specs = bf
        .manifest
        .parts
        .iter()
        .enumerate()
        .map(|(i, rec)| {
            let bytes = bf.files.get(&part_file_name(rec.part_no)).expect("part file").clone();
            PartSpec { io: PartIoSpec::Mem { bytes: Arc::from(bytes.as_slice()), dev: 7, ino: 990 + i as u64 }, expect: PartExpect::none(), rows: rec.rows, granules: rec.granule_count, deletes: None, part: None }
        })
        .collect();
    let list = Arc::new(PartList::new(specs));
    assert_eq!(list.len(), 3, "the fixture cut into three parts");
    list
}

/// One claim of granules `[g, g + 1)` read whole.
fn claim(src: &mut GranuleRangeSource, g: u64) -> u64 {
    let mut lanes = [Lane::NONE; MAX_LANES];
    src.seek(&[g, g + 1], &[0, 0]).unwrap();
    let mut rows = 0u64;
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = src.window(&mut w).unwrap();
        if n == 0 {
            return rows;
        }
        rows += n as u64;
    }
}

/// One statement's scan at the cache's own level (`set_part_release_at(None)`, the server's): the parts released after
/// each claim, granule by granule.
fn scan(list: &Arc<PartList>) -> Vec<u64> {
    let recipe = GranuleRecipe::for_test(REL, &[1], Arc::new(PartRegistry::new(64 << 20)));
    let mut resolved = Resolved::new();
    resolved.set_part_uses_release(true);
    resolved.set_part_release_at(None);
    resolved.install(REL, list.clone());
    let mut src = GranuleRangeSource::new(recipe);
    src.bind(&binding_ref(&resolved)).unwrap();
    let mut released = Vec::new();
    let mut rows = 0;
    for g in 0..list.granules() {
        rows += claim(&mut src, g);
        released.push(resolved.part_release_counts().0);
    }
    assert_eq!(rows, ROWS);
    src.unbind();
    released
}

#[test]
fn a_finished_part_goes_back_inside_the_caches_budget_when_memory_is_short() {
    let list = list();
    let last_of_first = list.starts()[1] - 1;
    // the part cache far inside its budget: by the level alone nothing is due
    pgrc_read::partcache::set_budget(1 << 40);
    memledger::set_fallback_limit(16 * G);
    // B's free bytes as of a snapshot taken with the grant pool empty (`free_now` = these less what the pool drew since)
    let held = 0;

    // a box with room (B's free bytes 12 GiB): the finished parts stay held for the statement's next run (D3268)
    memledger::note_snapshot(held, 12 * G);
    let room = scan(&list);
    assert!(room.iter().all(|&n| n == 0), "with room no part goes back before the statement's end: {room:?}");

    // memory short (B's free bytes 0): the first part goes back once every claim left it, the rest as theirs end
    memledger::note_snapshot(held, 0);
    let short = scan(&list);
    assert_eq!(short[last_of_first as usize], 1, "short: the first part went back at its last claim (trunk: held, 0): {short:?}");
    assert_eq!(*short.last().unwrap(), 3, "short: every part went back as its claims ended: {short:?}");
}
