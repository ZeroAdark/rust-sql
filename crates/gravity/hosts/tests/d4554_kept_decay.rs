//! D4554 (the `kept_decay` bound): a granule source kept a decode cursor over every part it ever read (D1507), so a worker
//! set's kept set grew with its statements (a worker claims other parts each statement) and so did every statement's
//! per-cursor moves. Under `kept_decay = D` the unbind closes its cursors over a part none of its last D binds read: what
//! repeats stays warm, the rest goes, and a later read opens it again. One test: the knob is process-wide.

use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use gravity_abi::Lane;
use gravity_driver::{blank_site, BindingRef, CommandId, Identity, PerTuple, Pins, Snapshot, Source, Window};
use gravity_hosts::ops::scan::pgrc::granule_source::{GranuleRangeSource, GranuleRecipe, PartIoSpec, PartList, PartSpec, Resolved};
use gravity_parallel::{ClaimSize, GranuleRange, MorselSource};
use mempolicy::consts::{self, K};
use pgrc_format::dirlayout::part_file_name;
use pgrc_qa::corpus::{build_fixture, int8_fixture, open_part_bytes};
use pgrc_read::{OpenPart, PartExpect, PartRegistry};
use pgrc_write::writer::PartCutPolicy;

const MAX_LANES: usize = 32;
const REL: u32 = 4554;
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

#[test]
fn a_sources_cursors_over_parts_its_last_binds_did_not_read_close_and_reopen_when_read_again() {
    let bf = build_fixture(int8_fixture("gravity_d4554", REL as u64, ROWS, vec![], PartCutPolicy { max_rows: 20_000, ..PartCutPolicy::default() }, |i| Some((i as i64 * 31) % 1000)));
    let parts: Vec<Arc<OpenPart>> = bf.manifest.parts.iter().enumerate().map(|(i, rec)| open_part_bytes(bf.files.get(&part_file_name(rec.part_no)).expect("part file"), 4554 + i as u64).expect("open part")).collect();
    assert_eq!(parts.len(), 3, "the fixture cut into three parts");
    let specs = bf
        .manifest
        .parts
        .iter()
        .enumerate()
        .map(|(i, rec)| {
            let bytes = bf.files.get(&part_file_name(rec.part_no)).expect("part file");
            PartSpec { io: PartIoSpec::Mem { bytes: Arc::from(bytes.as_slice()), dev: 7, ino: 4554 + i as u64 }, expect: PartExpect::none(), rows: rec.rows, granules: rec.granule_count, deletes: None, part: Some(parts[i].clone()) }
        })
        .collect();
    let list = Arc::new(PartList::new(specs));
    let recipe = GranuleRecipe::for_test(REL, &[1], Arc::new(PartRegistry::new(64 << 20)));
    let all = GranuleRange::over_parts(list.granules(), ClaimSize::Fixed(1), list.starts());
    // part 0 alone: parts 1 and 2 dead
    let mut first = GranuleRange::over_parts(list.granules(), ClaimSize::Fixed(1), list.starts());
    first.set_dead(&[(list.starts()[1], list.granules())]);
    let part0_rows = bf.manifest.parts[0].rows;
    let users = || parts.iter().map(|p| p.users()).collect::<Vec<_>>();

    consts::set(K::KeptDecay, 2);
    memledger::bind_backend(4554, 94554);
    let mut src = GranuleRangeSource::new(recipe.clone());
    let mut run = |src: &mut GranuleRangeSource, claims: &GranuleRange| {
        memledger::statement_begin();
        let resolved = Resolved::for_recipe(&recipe, list.clone());
        src.bind(&binding_ref(&resolved)).unwrap();
        let rows = pass(src, claims);
        src.unbind();
        drop(resolved);
        memledger::statement_end();
        rows
    };

    assert_eq!(run(&mut src, &all), ROWS, "bind 1 reads every part");
    assert_eq!(users(), vec![1, 1, 1], "one kept cursor a part");
    assert_eq!(run(&mut src, &first), part0_rows, "bind 2 reads part 0 alone");
    assert_eq!(users(), vec![1, 1, 1], "parts 1 and 2 read one bind ago: kept");
    assert_eq!(run(&mut src, &first), part0_rows, "bind 3 reads part 0 alone");
    assert_eq!(users(), vec![1, 0, 0], "parts 1 and 2 unread for two binds: their cursors closed");
    assert_eq!(run(&mut src, &all), ROWS, "bind 4 reads every part again, through new cursors");
    assert_eq!(users(), vec![1, 1, 1], "kept again");

    // off: nothing closes
    consts::set(K::KeptDecay, 0);
    for _ in 0..3 {
        assert_eq!(run(&mut src, &first), part0_rows);
    }
    assert_eq!(users(), vec![1, 1, 1], "off: every cursor kept");
    drop(src.release_rel(0).expect("every relation's kept cursors"));
    assert!(parts.iter().all(|p| p.users() == 0), "released: no use left");
    consts::reset();
    memledger::unbind_backend();
}
