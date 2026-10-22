//! D3994 / D3204 (B.1 W): a granule source parked past its statement keeps its decode cursors (D1507), each a use of its
//! part. On trunk those uses pinned the parts: the part cache's floor (`PartRegistry::in_use_bytes`, what no trim may
//! evict: the memory broker's floor and B0's cache_kept) counted every byte of a parked executor's parts, so 48 sessions'
//! parked plans held 10-12 GiB of a 16 GiB unit that no running statement read (D3994's broker-on stress). Now a parked
//! executor's use is parked (`OpenPart::live_users` leaves it out) and its cursors let their images go, so the floor
//! is what running owners read: 0 while the source is parked, its parts again once a statement binds it.

use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use gravity_abi::Lane;
use gravity_driver::{blank_site, BindingRef, CommandId, Identity, PerTuple, Pins, Snapshot, Source, Window};
use gravity_hosts::ops::scan::pgrc::granule_source::{GranuleRangeSource, GranuleRecipe, PartIoSpec, PartList, PartSpec, Resolved};
use gravity_parallel::{ClaimSize, GranuleRange, MorselSource};
use mempolicy::consts::{self, K};
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

#[test]
fn a_parked_sources_parts_leave_the_floor_and_come_back_when_a_statement_binds_it() {
    // the rejoin this test pins is the `kept_parked = 0` arm since the knob defaults on (D4665); the parked arm is
    // d4554_kept_parked's and d4665_kept_defaults'
    consts::set(K::KeptParked, 0);
    let reg = Arc::new(PartRegistry::new(64 << 20));
    let (parts, _pins, list, recipe) = fixture(&reg, 4509, "gravity_d3994_floor", 960);
    let claims = GranuleRange::over_parts(list.granules(), ClaimSize::Fixed(1), list.starts());
    let mut src = GranuleRangeSource::new(recipe.clone());
    memledger::bind_backend(195, 9195);

    assert_eq!(statement(&mut src, &recipe, &list, &claims), ROWS);
    assert!(parts.iter().all(|p| p.users() >= 1), "the parked source keeps a cursor (a use) on every part it read");
    assert!(cached(&parts) > 0, "the parts' streams stay cached");
    assert_eq!(reg.in_use_bytes(), 0, "parked: the parts are in no running owner's read, so not in the cache's floor (trunk: all of them)");

    // a statement binds the parked source again: its cursors' uses are its own and the parts are in the floor while it runs
    memledger::statement_begin();
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    src.bind(&binding_ref(&resolved)).unwrap();
    assert_eq!(reg.in_use_bytes(), cached(&parts), "bound again: every byte of its parts is in use");
    assert_eq!(pass(&mut src, &claims), ROWS);
    src.unbind();
    drop(resolved);
    memledger::statement_end();
    assert_eq!(reg.in_use_bytes(), 0, "parked again");
    drop(src.release_rel(0).expect("every relation's kept cursors"));
    consts::reset();
}
