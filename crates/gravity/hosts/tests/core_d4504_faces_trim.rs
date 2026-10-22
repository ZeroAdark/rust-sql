//! D4504: a part list's metadata faces (the Stats walk, bloom and PSMA bodies its statements read) are their parts'
//! part-cache contents: counted in the census as each part's faces, bounded by the cache's budget, and let go by its
//! content trim after the part's data (the faces hook empties exactly those cells). Before, the only bound was the evicted
//! hook's `LISTS.invalidate_relid(0)`: any trim that evicted dictionary images dropped every list, and the next statements
//! re-read and re-decompressed every face (TPC-H SF10 W8: 346 PSMA / 657 Stats reads to hold 62 / 236).

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

fn int8_profile() -> pgrc_meta::profile::MetaProfile {
    let c = pgrc_qa::int8_col(1);
    pgrc_meta::profile::MetaProfile::derive_or_opaque(c.class, c.collation_class, c.semantics)
}

const ALL_VERDICTS: u64 = gravity_program::protos::VERDICT_ZONE | gravity_program::protos::VERDICT_BLOOM | gravity_program::protos::VERDICT_PSMA | gravity_program::protos::VERDICT_CLAIM;

#[test]
fn a_lists_faces_are_their_parts_contents_and_a_trim_lets_them_go_after_the_data() {
    let reg = Arc::new(PartRegistry::new(64 << 20));
    let bf = build_fixture(int8_fixture("gravity_d4504_faces_trim", 4504, ROWS, vec![], PartCutPolicy { max_rows: 20_000, ..PartCutPolicy::default() }, |i| Some((i as i64 * 31) % 1000)));
    let (mut pins, mut specs, mut parts) = (Vec::new(), Vec::new(), Vec::new());
    for (i, rec) in bf.manifest.parts.iter().enumerate() {
        let bytes = bf.files.get(&part_file_name(rec.part_no)).expect("part file").clone();
        let ino = 4504 + i as u64;
        let pin = reg.open_pinned(&PartExpect::none(), || Ok(Box::new(MemPartIo::new(bytes.clone(), 7, ino)) as Box<dyn pgrc_read::io::PartIo>)).expect("open part");
        specs.push(PartSpec { io: PartIoSpec::Mem { bytes: Arc::from(bytes.as_slice()), dev: 7, ino }, expect: PartExpect::none(), rows: rec.rows, granules: rec.granule_count, deletes: None, part: Some(Arc::clone(pin.part())) });
        parts.push(Arc::clone(pin.part()));
        pins.push(pin);
    }
    // the faces hook residency.rs arms in a server
    pgrc_read::partcache::set_faces_hook(gravity_hosts::ops::scan::pgrc::granule_source::let_stale_faces_go);
    memledger::bind_backend(198, 9198);
    let faces = || parts.iter().map(|p: &Arc<OpenPart>| p.face_bytes()).sum::<u64>();
    let list = Arc::new(PartList::new(specs));

    // a zone predicate's statement reads its lane's Stats (and its blooms / PSMA where the parts sealed them)
    let statement = || {
        let recipe = GranuleRecipe::for_test(4504, &[1], Arc::clone(&reg)).with_profile(0, int8_profile()).with_verdict_cell();
        recipe.verdicts.set(0, ALL_VERDICTS);
        let claims = GranuleRange::over_parts(list.granules(), ClaimSize::Fixed(1), list.starts());
        let mut src = GranuleRangeSource::new(recipe.clone());
        src.zone(&[ZonePred::word(0, ZoneOp::Eq, 31)], &[0], &[]);
        memledger::statement_begin();
        let resolved = Resolved::for_recipe(&recipe, list.clone());
        src.bind(&binding_ref(&resolved)).unwrap();
        let rows = pass(&mut src, &claims);
        src.unbind();
        drop(resolved);
        memledger::statement_end();
        drop(src.release_rel(0));
        rows
    };
    let rows = statement();
    assert!(rows > 0, "the predicate's rows are read");
    let read = faces();
    assert!(read > 0, "the statement's faces are its parts' part-cache contents (trunk: 0 B, the list's alone)");

    // the next statement reads the faces the list kept: nothing more is counted
    assert_eq!(statement(), rows, "the same rows");
    assert_eq!(faces(), read, "the list's faces served the second statement");

    // a trim past every target: the unused parts' data go, then their faces; the hook empties the list's cells, and the
    // parts count no faces (the list stays: before D4504 the evicted hook dropped every list instead)
    reg.trim_contents(0);
    assert_eq!(faces(), 0, "the trim let the parts' faces go after their data ({read} B before)");
    assert!(parts.iter().all(|p| p.resident() == 0), "the data went first");

    // the next statement reads them again, into the same list
    assert_eq!(statement(), rows, "the same rows after the trim");
    assert_eq!(faces(), read, "the faces read again are counted again");
    drop(list);
    assert_eq!(faces(), 0, "the list's drop lets them go");
    drop(pins);
}
