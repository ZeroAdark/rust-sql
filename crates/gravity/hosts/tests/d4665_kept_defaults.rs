//! D4665 / D4630 (b): at the DEFAULTS a repeated statement's bind takes no kept cursor's part use, however many the
//! source keeps (kept_parked and kept_pool default on). Off, every bind took every kept cursor's use and gave it back at
//! its end, so the per-statement work climbed as the kept set filled (a LIKE group-by: 82 -> 120 M instructions a statement
//! over 20 runs in one session). Born red on the off defaults: the uses a part-0 statement's bind takes grow 1, 2, 3 with
//! the parts kept; on they stay 0. One test: the knobs are process-wide.

use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use gravity_abi::Lane;
use gravity_driver::{blank_site, BindingRef, CommandId, Identity, PerTuple, Pins, Snapshot, Source, Window};
use gravity_hosts::ops::scan::pgrc::granule_source::{GranuleRangeSource, GranuleRecipe, PartIoSpec, PartList, PartSpec, Resolved};
use gravity_parallel::{ClaimSize, GranuleRange, MorselSource};
use mempolicy::consts;
use pgrc_format::dirlayout::part_file_name;
use pgrc_qa::corpus::{build_fixture, int8_fixture, open_part_bytes};
use pgrc_read::{OpenPart, PartExpect, PartRegistry};
use pgrc_write::writer::PartCutPolicy;

const MAX_LANES: usize = 32;
const REL: u32 = 4665;
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
fn at_the_defaults_a_repeated_statements_bind_takes_no_kept_use_however_many_parts_are_kept() {
    consts::reset();
    let bf = build_fixture(int8_fixture("gravity_d4665", REL as u64, ROWS, vec![], PartCutPolicy { max_rows: 20_000, ..PartCutPolicy::default() }, |i| Some((i as i64 * 31) % 1000)));
    let parts: Vec<Arc<OpenPart>> = bf.manifest.parts.iter().enumerate().map(|(i, rec)| open_part_bytes(bf.files.get(&part_file_name(rec.part_no)).expect("part file"), 4665 + i as u64).expect("open part")).collect();
    assert_eq!(parts.len(), 3, "the fixture cut into three parts");
    let specs = bf
        .manifest
        .parts
        .iter()
        .enumerate()
        .map(|(i, rec)| {
            let bytes = bf.files.get(&part_file_name(rec.part_no)).expect("part file");
            PartSpec { io: PartIoSpec::Mem { bytes: Arc::from(bytes.as_slice()), dev: 7, ino: 4665 + i as u64 }, expect: PartExpect::none(), rows: rec.rows, granules: rec.granule_count, deletes: None, part: Some(parts[i].clone()) }
        })
        .collect();
    let list = Arc::new(PartList::new(specs));
    let recipe = GranuleRecipe::for_test(REL, &[1], Arc::new(PartRegistry::new(64 << 20)));
    let starts = list.starts().to_vec();
    // a claim range over parts [0, upto): the later parts dead
    let upto = |n: usize| {
        let mut r = GranuleRange::over_parts(list.granules(), ClaimSize::Fixed(1), list.starts());
        if n < 3 {
            r.set_dead(&[(starts[n], list.granules())]);
        }
        r
    };
    let live = || parts.iter().map(|p| u64::from(p.live_users())).sum::<u64>();
    memledger::bind_backend(198, 9198);
    // one statement over parts [0, n): the kept uses its bind took (live right after the bind), its rows
    let stmt = |src: &mut GranuleRangeSource, n: usize| {
        memledger::statement_begin();
        let resolved = Resolved::for_recipe(&recipe, list.clone());
        src.bind(&binding_ref(&resolved)).unwrap();
        let at_bind = live();
        let rows = pass(src, &upto(n));
        src.unbind();
        src.settle_kept();
        drop(resolved);
        memledger::statement_end();
        (at_bind, rows)
    };
    let mut src = GranuleRangeSource::new(recipe.clone());
    let mut taken = Vec::new();
    for kept in 1..=3 {
        // grow the kept set to `kept` parts, then the repeated statement: part 0 alone
        let (_, rows) = stmt(&mut src, kept);
        assert_eq!(rows, bf.manifest.parts[..kept].iter().map(|p| p.rows).sum::<u64>(), "the statement over {kept} part(s) reads their rows");
        let (at_bind, rows) = stmt(&mut src, 1);
        assert_eq!(rows, bf.manifest.parts[0].rows, "the repeated statement reads part 0's rows");
        taken.push(at_bind);
    }
    assert_eq!(taken, vec![0, 0, 0], "the repeated statement's bind takes no kept use as the kept set grows (off: 1, 2, 3)");
    assert!(mempolicy::keep::kept_parked() && mempolicy::keep::kept_pool(), "kept_parked and kept_pool are on by default");
    assert_eq!(live(), 0, "no kept use live between statements");
    drop(src.release_rel(0).expect("every relation's kept cursors"));
    assert!(parts.iter().all(|p| p.users() == 0), "released, no use left");
    memledger::unbind_backend();
}
