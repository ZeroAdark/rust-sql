//! D3994 / D3204 (B.1 W): a parked executor's kept cursors let their part images go at the park (the parsed extent tables
//! stay), so a content trim evicts a part only parked executors use when the memory policy's gate opens
//! (`pgrc_read::partcache::set_parked_hook`: Gravity's is `mempolicy::cache::PARKED_GATE`, memory pressure), and the
//! memory goes with the eviction: no clone is left, so D3823's poison fills every image it frees. With the gate closed (a
//! box with room) the parts stay, as a hot repeat needs (D3268). A statement that binds the source again after an
//! eviction faults the images again and reads the same rows; without one it takes the images back from the cache and
//! faults nothing.

use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use gravity_abi::Lane;
use gravity_driver::{blank_site, BindingRef, CommandId, Identity, PerTuple, Pins, Snapshot, Source, Window};
use gravity_hosts::ops::scan::pgrc::granule_source::{GranuleRangeSource, GranuleRecipe, PartIoSpec, PartList, PartSpec, Resolved};
use gravity_parallel::{ClaimSize, GranuleRange, MorselSource};
use pgrc_format::dirlayout::part_file_name;
use pgrc_qa::corpus::{build_fixture, int8_fixture};
use pgrc_read::io::MemPartIo;
use pgrc_read::{OpenPart, PartExpect, PartRegistry};
use pgrc_write::writer::PartCutPolicy;

const MAX_LANES: usize = 32;
const ROWS: u64 = 60_000;
static FLAG: AtomicU8 = AtomicU8::new(0);

fn binding_ref(resolved: &Resolved) -> BindingRef<'_> {
    BindingRef { snapshot: Snapshot(0), command_id: CommandId(0), params: &[], abort_flag: &FLAG, pins: Pins(0), per_tuple: PerTuple(0), identity: Identity::default(), resolved: Some(resolved), ordinal: None }
}

fn pass(src: &mut GranuleRangeSource, claims: &GranuleRange) -> u64 {
    let mut lanes = [Lane::NONE; MAX_LANES];
    claims.reset();
    let mut rows = 0u64;
    while let Some(c) = claims.claim() {
        src.seek(&[c.lo, c.hi], &[0, 0]).unwrap();
        loop {
            let mut site = blank_site(lanes.as_mut_ptr());
            let mut w = Window::over(&mut site);
            let n = src.window(&mut w).unwrap();
            if n == 0 {
                break;
            }
            rows += n as u64;
        }
    }
    rows
}

/// Three parts of a 60,000-row int8 fixture opened through `reg` (so its content trim walks them), their list and recipe.
fn fixture(reg: &Arc<PartRegistry>, rel: u32, name: &'static str, ino0: u64) -> (Vec<Arc<OpenPart>>, Vec<pgrc_read::registry::PartPin>, Arc<PartList>, GranuleRecipe) {
    let bf = build_fixture(int8_fixture(name, rel as u64, ROWS, vec![], PartCutPolicy { max_rows: 20_000, ..PartCutPolicy::default() }, |i| Some((i as i64 * 31) % 1000)));
    let (mut parts, mut pins, mut specs) = (Vec::new(), Vec::new(), Vec::new());
    for (i, rec) in bf.manifest.parts.iter().enumerate() {
        let bytes = bf.files.get(&part_file_name(rec.part_no)).expect("part file").clone();
        let ino = ino0 + i as u64;
        let pin = reg.open_pinned(&PartExpect::none(), || Ok(Box::new(MemPartIo::new(bytes.clone(), 7, ino)) as Box<dyn pgrc_read::io::PartIo>)).expect("open part");
        let part = Arc::clone(pin.part());
        specs.push(PartSpec { io: PartIoSpec::Mem { bytes: Arc::from(bytes.as_slice()), dev: 7, ino }, expect: PartExpect::none(), rows: rec.rows, granules: rec.granule_count, deletes: None, part: Some(part.clone()) });
        parts.push(part);
        pins.push(pin);
    }
    assert_eq!(parts.len(), 3, "the fixture cut into three parts");
    (parts, pins, Arc::new(PartList::new(specs)), GranuleRecipe::for_test(rel, &[1], Arc::clone(reg)))
}

/// One statement over the source: bound, read whole, unbound (parked).
fn statement(src: &mut GranuleRangeSource, recipe: &GranuleRecipe, list: &Arc<PartList>, claims: &GranuleRange) -> u64 {
    memledger::statement_begin();
    let resolved = Resolved::for_recipe(recipe, list.clone());
    src.bind(&binding_ref(&resolved)).unwrap();
    let rows = pass(src, claims);
    src.unbind();
    drop(resolved);
    memledger::statement_end();
    rows
}

fn cached(parts: &[Arc<OpenPart>]) -> u64 {
    parts.iter().map(|p| p.cache_bytes()).sum()
}

fn closed() -> bool {
    false
}

fn open() -> bool {
    true
}

#[test]
fn a_trim_evicts_a_parked_sources_parts_only_through_the_gate_and_a_rebind_reads_the_same_rows() {
    let reg = Arc::new(PartRegistry::new(64 << 20));
    let (parts, _pins, list, recipe) = fixture(&reg, 4510, "gravity_d3994_evict", 970);
    let claims = GranuleRange::over_parts(list.granules(), ClaimSize::Fixed(1), list.starts());
    let mut src = GranuleRangeSource::new(recipe.clone());
    memledger::bind_backend(196, 9196);

    assert_eq!(statement(&mut src, &recipe, &list, &claims), ROWS);
    let warm = cached(&parts);
    assert!(warm > 0 && parts.iter().all(|p| p.users() >= 1 && p.live_users() == 0), "parked: every part used, none live");

    // the gate closed: a trim to nothing keeps the parked source's parts (trunk's pin, now the policy's choice)
    pgrc_read::partcache::set_parked_hook(closed);
    reg.trim_contents(0);
    assert_eq!(cached(&parts), warm, "gate closed: the parked parts stay");
    assert_eq!(statement(&mut src, &recipe, &list, &claims), ROWS, "a hot repeat reads the parked images back");
    assert_eq!(cached(&parts), warm, "and faulted nothing new");
    // D4910 / D4908: what the closed gate keeps reads as the parked holder's bytes
    let u = reg.untrimmable_by_holder();
    assert!(!u.parked_gate && u.live == 0 && u.deferred == 0, "gate closed, nothing live: {u:?}");
    assert!(u.parked > 0 && u.parked <= warm, "the parked parts' bytes the trim cannot drop: {u:?} of {warm}");

    // the gate open: the trim evicts them, and the memory goes: the parked cursors hold no clone of an image (a shared
    // one would stay cached and counted, D3113), and D3823's poison (which fills only an image nothing else references)
    // fills what the trim frees
    pgrc_read::partcache::set_parked_hook(open);
    let u = reg.untrimmable_by_holder();
    assert!(u.parked_gate && u.parked * 10 <= warm, "gate open: a trim may drop the parked bytes no clone shares: {u:?} of {warm}");
    pgrc_read::partcache::set_park_poison(true);
    let poisoned0 = pgrc_read::partcache::park_poisoned_bytes();
    reg.trim_contents(0);
    let left = cached(&parts);
    let poisoned = pgrc_read::partcache::park_poisoned_bytes() - poisoned0;
    assert!(left * 10 <= warm, "gate open: the parked parts' images are evicted ({left} of {warm} bytes left)");
    assert!(poisoned * 2 >= warm - left, "the images freed were the cache's alone: {poisoned} of {} bytes poisoned", warm - left);
    assert!(parts.iter().all(|p| p.users() >= 1), "the cursors (and their part files) stay kept");

    // the next statement faults them again and reads the same rows
    pgrc_read::partcache::set_parked_hook(closed);
    assert_eq!(statement(&mut src, &recipe, &list, &claims), ROWS, "a rebind after the eviction reads the same rows");
    assert!(cached(&parts) > left, "the rebind faulted the images again");
    drop(src.release_rel(0).expect("every relation's kept cursors"));
    assert!(parts.iter().all(|p| p.users() == 0), "released: no use left");
}
