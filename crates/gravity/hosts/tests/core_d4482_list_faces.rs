//! D4482: a part list keeps its faces (the face tables, each read lane's tables, the Stats / bloom / PSMA bodies its
//! statements read) for its generation, shared by every statement over it. They were allocated on whichever thread read
//! them first and charged to no one: the bloom and PSMA bodies alone were ~24 MiB of a TPC-H SF10 session's unattributed
//! bytes. Now they are the part cache's (the holder whose parts the list keeps open), from their fill to the list's drop.

use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use gravity_abi::Lane;
use gravity_driver::{blank_site, BindingRef, CommandId, Identity, PerTuple, Pins, Snapshot, Source, Window};
use gravity_hosts::ops::scan::pgrc::granule_source::{GranuleRangeSource, GranuleRecipe, PartIoSpec, PartList, PartSpec, Resolved};
use gravity_parallel::{ClaimSize, GranuleRange, MorselSource};
use gravity_program::{ZoneOp, ZonePred};
use memledger::{charge, Consumer, Holder, Owner};
use pgrc_format::dirlayout::part_file_name;
use pgrc_qa::corpus::{build_fixture, int8_fixture};
use pgrc_read::io::MemPartIo;
use pgrc_read::{PartExpect, PartRegistry};
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

fn int8_profile() -> pgrc_meta::profile::MetaProfile {
    let c = pgrc_qa::int8_col(1);
    pgrc_meta::profile::MetaProfile::derive_or_opaque(c.class, c.collation_class, c.semantics)
}

const ALL_VERDICTS: u64 = gravity_program::protos::VERDICT_ZONE | gravity_program::protos::VERDICT_BLOOM | gravity_program::protos::VERDICT_PSMA | gravity_program::protos::VERDICT_CLAIM;

#[test]
fn a_part_lists_faces_are_the_part_caches_from_their_fill_to_the_lists_drop() {
    let reg = Arc::new(PartRegistry::new(64 << 20));
    let bf = build_fixture(int8_fixture("gravity_d4482_list_faces", 4482, ROWS, vec![], PartCutPolicy { max_rows: 20_000, ..PartCutPolicy::default() }, |i| Some((i as i64 * 31) % 1000)));
    let (mut pins, mut specs) = (Vec::new(), Vec::new());
    for (i, rec) in bf.manifest.parts.iter().enumerate() {
        let bytes = bf.files.get(&part_file_name(rec.part_no)).expect("part file").clone();
        let ino = 4482 + i as u64;
        let pin = reg.open_pinned(&PartExpect::none(), || Ok(Box::new(MemPartIo::new(bytes.clone(), 7, ino)) as Box<dyn pgrc_read::io::PartIo>)).expect("open part");
        specs.push(PartSpec { io: PartIoSpec::Mem { bytes: Arc::from(bytes.as_slice()), dev: 7, ino }, expect: PartExpect::none(), rows: rec.rows, granules: rec.granule_count, deletes: None, part: Some(Arc::clone(pin.part())) });
        pins.push(pin);
    }
    memledger::bind_backend(197, 9197);
    let holder = Owner::Cache(Holder::Of(Consumer::PartCache));
    let held = || charge::owner_row(holder).committed;

    let before = held();
    let list = Arc::new(PartList::new(specs));
    let built = held();
    assert!(built > before, "the list's face tables are the part cache's from its build ({before} -> {built} B; trunk: no change)");

    // a zone predicate's statement reads its lane's Stats (and its blooms / PSMA where the parts sealed them) into the faces
    let recipe = GranuleRecipe::for_test(4482, &[1], Arc::clone(&reg)).with_profile(0, int8_profile()).with_verdict_cell();
    recipe.verdicts.set(0, ALL_VERDICTS);
    let claims = GranuleRange::over_parts(list.granules(), ClaimSize::Fixed(1), list.starts());
    let mut src = GranuleRangeSource::new(recipe.clone());
    src.zone(&[ZonePred::word(0, ZoneOp::Eq, 31)], &[0], &[]);
    memledger::statement_begin();
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    src.bind(&binding_ref(&resolved)).unwrap();
    assert!(pass(&mut src, &claims) > 0, "the predicate's rows are read");
    src.unbind();
    drop(resolved);
    memledger::statement_end();
    drop(src.release_rel(0));
    drop(src);
    let read = held();
    assert!(read > built, "the statement's reads are the part cache's ({built} -> {read} B)");

    // the parts stay pinned (their faulted streams stay on the row): the list's drop takes off its face tables and the
    // lane tables and bodies its statement read into them, more than the build alone charged
    drop(list);
    let after = held();
    assert!(read - after > built - before, "the list's drop takes its faces off the part cache's row ({read} -> {after} B; the build alone charged {} B)", built - before);
    drop(pins);
}
