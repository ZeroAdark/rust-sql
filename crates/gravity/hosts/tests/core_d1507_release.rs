//! D1507: an unbound granule source keeps its decode cursors across statements (one list generation), each holding its
//! part open (`Arc<OpenPart>`, the part file's descriptor). Parked, a dropped relation's source kept the unlinked part files
//! open until its slot was evicted. `Source::release_rel` moves them out: the relation's (or every relation's, `0`) and
//! only while unbound; the next bind reopens lazily and reads the same rows.

use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use gravity_abi::Lane;
use gravity_driver::{blank_site, BindingRef, CommandId, Identity, PerTuple, Pins, Snapshot, Source, Window};
use gravity_hosts::ops::scan::pgrc::granule_source::{GranuleRangeSource, GranuleRecipe, PartIoSpec, PartList, PartSpec, Resolved};
use gravity_parallel::{ClaimSize, GranuleRange, MorselSource};
use pgrc_format::dirlayout::part_file_name;
use pgrc_qa::corpus::{build_fixture, int8_fixture, open_part_bytes};
use pgrc_read::{OpenPart, PartExpect, PartRegistry};
use pgrc_write::writer::PartCutPolicy;

const MAX_LANES: usize = 32;
const REL: u32 = 4507;
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

fn holds(parts: &[Arc<OpenPart>]) -> Vec<usize> {
    parts.iter().map(|p| Arc::strong_count(p) - 2).collect() // the test's and the list's own
}

#[test]
fn an_unbound_sources_kept_cursors_go_at_release_of_its_relation_and_a_rebind_reads_the_same_rows() {
    let bf = build_fixture(int8_fixture("gravity_d1507", REL as u64, ROWS, vec![], PartCutPolicy { max_rows: 20_000, ..PartCutPolicy::default() }, |i| Some((i as i64 * 31) % 1000)));
    let parts: Vec<Arc<OpenPart>> = bf.manifest.parts.iter().enumerate().map(|(i, rec)| open_part_bytes(bf.files.get(&part_file_name(rec.part_no)).expect("part file"), 900 + i as u64).expect("open part")).collect();
    assert_eq!(parts.len(), 3, "the fixture cut into three parts");
    let specs = bf
        .manifest
        .parts
        .iter()
        .enumerate()
        .map(|(i, rec)| {
            let bytes = bf.files.get(&part_file_name(rec.part_no)).expect("part file");
            PartSpec { io: PartIoSpec::Mem { bytes: Arc::from(bytes.as_slice()), dev: 7, ino: 900 + i as u64 }, expect: PartExpect::none(), rows: rec.rows, granules: rec.granule_count, deletes: None, part: Some(parts[i].clone()) }
        })
        .collect();
    let list = Arc::new(PartList::new(specs));
    let recipe = GranuleRecipe::for_test(REL, &[1], Arc::new(PartRegistry::new(64 << 20)));
    let claims = GranuleRange::over_parts(list.granules(), ClaimSize::Fixed(1), list.starts());
    let mut src = GranuleRangeSource::new(recipe.clone());
    // one statement: its own `Resolved` (the statement's dictionaries hold their parts until it ends), bound, read, unbound
    let statement = |src: &mut GranuleRangeSource| {
        let resolved = Resolved::for_recipe(&recipe, list.clone());
        src.bind(&binding_ref(&resolved)).unwrap();
        assert_eq!(pass(src, &claims), ROWS);
        assert!(src.release_rel(REL).is_none(), "a bound source keeps what its statement reads");
        src.unbind();
    };
    assert_eq!(holds(&parts), [0, 0, 0]);

    statement(&mut src);
    let kept = holds(&parts);
    assert!(kept.iter().all(|&k| k >= 1), "the unbound source keeps a cursor on every part it read: {kept:?}");

    assert!(src.release_rel(REL + 1).is_none(), "another relation's release keeps them");
    assert_eq!(holds(&parts), kept);
    let released = src.release_rel(REL).expect("the relation's kept cursors");
    assert_eq!(holds(&parts), kept, "moved out, not dropped: the caller drops them after its lock");
    drop(released);
    assert_eq!(holds(&parts), [0, 0, 0], "every part the source held is let go");
    assert!(src.release_rel(REL).is_none(), "nothing left to release");

    // the next statement: the rebind reopens lazily and reads the same rows; relid 0 releases any relation's
    statement(&mut src);
    assert_eq!(holds(&parts), kept);
    drop(src.release_rel(0).expect("relid 0: every relation's"));
    assert_eq!(holds(&parts), [0, 0, 0]);
}
