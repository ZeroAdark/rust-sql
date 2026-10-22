//! D4464: a granule source parked past its statement (`Source::park`: its Node into the backend's park, its worker set
//! into the slot depot) kept its bind-sized lanes, selections and image arenas at the statement's high water, charged
//! to the session: natts x (ROWS + PF_WORDS) datum words alone are 64 KiB a column, per parked plan. Now the park frees
//! each one over the planned region's floor, the session's row drops by exactly those bytes, and the next bind sizes
//! them again and reads the same rows. Its kept cursors' own boxes were in no charge (only their slot table was); now
//! they are in the source's held charge.

use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use gravity_abi::Lane;
use gravity_driver::{blank_site, BindingRef, CommandId, Identity, PerTuple, Pins, Snapshot, Source, Window};
use gravity_hosts::ops::scan::pgrc::granule_source::{self, GranuleRangeSource, GranuleRecipe, PartIoSpec, PartList, PartSpec, Resolved};
use gravity_parallel::{ClaimSize, GranuleRange, MorselSource};
use memledger::{charge, Acct, Owner};
use pgrc_format::dirlayout::part_file_name;
use pgrc_qa::corpus::{build_fixture, int8_fixture};
use pgrc_read::io::MemPartIo;
use pgrc_read::{PartExpect, PartPin, PartRegistry, StreamCursor};
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

fn fixture(reg: &Arc<PartRegistry>, rel: u32, name: &'static str, ino0: u64) -> (Vec<pgrc_read::registry::PartPin>, Arc<PartList>, GranuleRecipe) {
    let bf = build_fixture(int8_fixture(name, rel as u64, ROWS, vec![], PartCutPolicy { max_rows: 20_000, ..PartCutPolicy::default() }, |i| Some((i as i64 * 31) % 1000)));
    let (mut pins, mut specs) = (Vec::new(), Vec::new());
    for (i, rec) in bf.manifest.parts.iter().enumerate() {
        let bytes = bf.files.get(&part_file_name(rec.part_no)).expect("part file").clone();
        let ino = ino0 + i as u64;
        let pin = reg.open_pinned(&PartExpect::none(), || Ok(Box::new(MemPartIo::new(bytes.clone(), 7, ino)) as Box<dyn pgrc_read::io::PartIo>)).expect("open part");
        let part = Arc::clone(pin.part());
        specs.push(PartSpec { io: PartIoSpec::Mem { bytes: Arc::from(bytes.as_slice()), dev: 7, ino }, expect: PartExpect::none(), rows: rec.rows, granules: rec.granule_count, deletes: None, part: Some(part) });
        pins.push(pin);
    }
    (pins, Arc::new(PartList::new(specs)), GranuleRecipe::for_test(rel, &[1], Arc::clone(reg)))
}

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

#[test]
fn a_parked_source_frees_its_bind_sized_buffers_and_the_next_bind_sizes_them_again() {
    let reg = Arc::new(PartRegistry::new(64 << 20));
    let (_pins, list, recipe) = fixture(&reg, 4464, "gravity_d4464_parked_scratch", 4464);
    let claims = GranuleRange::over_parts(list.granules(), ClaimSize::Fixed(1), list.starts());
    let mut src = GranuleRangeSource::new(recipe.clone());
    memledger::bind_backend(196, 9196);
    let session = Owner::Session(Acct::backend(196));
    let lane_bytes = granule_source::ROWS * core::mem::size_of::<u64>();

    assert_eq!(statement(&mut src, &recipe, &list, &claims), ROWS);
    let (kept, held) = (src.kept_bytes(), charge::owner_row(session).committed);
    assert!(kept >= lane_bytes, "the statement sized one lane's datum words: {kept} B kept");
    assert!(held >= lane_bytes as i64, "unbound: the kept buffers are the session's ({held} B)");

    src.park();
    let (parked, parked_held) = (src.kept_bytes(), charge::owner_row(session).committed);
    assert!(kept - parked >= lane_bytes, "parked: the lane's words are freed ({kept} -> {parked} B; trunk keeps them)");
    assert_eq!(held - parked_held, (kept - parked) as i64, "the session's row drops by exactly the bytes freed");

    src.unpark();
    assert_eq!(statement(&mut src, &recipe, &list, &claims), ROWS, "the next bind sizes them again and reads the same rows");
    assert!(src.kept_bytes() >= lane_bytes, "sized again by the bind");

    // the kept cursors' own boxes are in the held charge (trunk counted only their slot table): their release takes them
    // off the session's row
    let (kept, held) = (src.kept_bytes(), charge::owner_row(session).committed);
    let released = src.release_rel(0).expect("every relation's kept cursors");
    let n = released.downcast_ref::<(Vec<Box<StreamCursor<'static>>>, Option<PartPin>)>().expect("the kept cursors").0.len();
    assert_eq!(n, 3, "one cursor per part of the one lane");
    let boxes = n * core::mem::size_of::<StreamCursor<'static>>();
    assert_eq!(kept - src.kept_bytes(), boxes, "the cursors' boxes left the source's kept bytes");
    assert_eq!(held - charge::owner_row(session).committed, boxes as i64, "and the session's row");
}
