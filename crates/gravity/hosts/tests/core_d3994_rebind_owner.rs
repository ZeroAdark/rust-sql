//! D3994 / D3204 (B.1 R): a granule source parked past its statement keeps its decode cursors (D1507), each a use of its
//! part, and `unbind` passes those uses to the backend's session (C8). The next statement that binds the parked source
//! again reads through them, but on trunk they stayed the session's: the watchdog's rank (`in_use_by_running` counts a
//! part only when the running statement or the portal it executes owns every use) and the part cache's holder split saw
//! no running statement pinning those parts (D3994's broker-on stress: 9.6 GiB of in-use parts on 'session', 0-3 MB
//! statements canceled at a full cgroup). Now a bind moves the kept cursors' uses to the binding statement, and the unbind
//! gives them back to the session.

use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use gravity_abi::Lane;
use gravity_driver::{blank_site, BindingRef, CommandId, Identity, PerTuple, Pins, Snapshot, Source, Window};
use gravity_hosts::ops::scan::pgrc::granule_source::{GranuleRangeSource, GranuleRecipe, PartIoSpec, PartList, PartSpec, Resolved};
use gravity_parallel::{ClaimSize, GranuleRange, MorselSource};
use memledger::{charge, Acct, Owner};
use mempolicy::consts::{self, K};
use pgrc_format::dirlayout::part_file_name;
use pgrc_qa::corpus::{build_fixture, int8_fixture, open_part_bytes};
use pgrc_read::{OpenPart, PartExpect, PartRegistry};
use pgrc_write::writer::PartCutPolicy;

const MAX_LANES: usize = 32;
const REL: u32 = 4508;
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

/// Every part's uses by owner key (account, key): the user lists the rank reads.
fn owner_keys(parts: &[Arc<OpenPart>]) -> Vec<Vec<(u32, u64)>> {
    parts
        .iter()
        .map(|p| {
            let mut v: Vec<(u32, u64)> = p.user_accounts().into_iter().map(|(a, k, _)| (a, k)).collect();
            v.sort();
            v.dedup();
            v
        })
        .collect()
}

#[test]
fn a_rebound_sources_kept_cursors_are_the_binding_statements_uses_and_the_sessions_again_after() {
    // the rejoin this test pins is the `kept_parked = 0` arm since the knob defaults on (D4665); the parked arm is
    // d4554_kept_parked's and d4665_kept_defaults'
    consts::set(K::KeptParked, 0);
    let bf = build_fixture(int8_fixture("gravity_d3994", REL as u64, ROWS, vec![], PartCutPolicy { max_rows: 20_000, ..PartCutPolicy::default() }, |i| Some((i as i64 * 31) % 1000)));
    let parts: Vec<Arc<OpenPart>> = bf.manifest.parts.iter().enumerate().map(|(i, rec)| open_part_bytes(bf.files.get(&part_file_name(rec.part_no)).expect("part file"), 950 + i as u64).expect("open part")).collect();
    assert_eq!(parts.len(), 3, "the fixture cut into three parts");
    let specs = bf
        .manifest
        .parts
        .iter()
        .enumerate()
        .map(|(i, rec)| {
            let bytes = bf.files.get(&part_file_name(rec.part_no)).expect("part file");
            PartSpec { io: PartIoSpec::Mem { bytes: Arc::from(bytes.as_slice()), dev: 7, ino: 950 + i as u64 }, expect: PartExpect::none(), rows: rec.rows, granules: rec.granule_count, deletes: None, part: Some(parts[i].clone()) }
        })
        .collect();
    let list = Arc::new(PartList::new(specs));
    let recipe = GranuleRecipe::for_test(REL, &[1], Arc::new(PartRegistry::new(64 << 20)));
    let claims = GranuleRange::over_parts(list.granules(), ClaimSize::Fixed(1), list.starts());
    let mut src = GranuleRangeSource::new(recipe.clone());

    memledger::bind_backend(194, 9194);
    let acct = Acct::backend(194).0;
    let session = vec![vec![(acct, 0u64)]; 3];

    // statement 1 opens the cursors (its own uses), reads, and parks the source: the kept uses pass to the session
    memledger::statement_begin();
    let first = charge::context().0;
    assert!(matches!(first, Owner::Statement { .. }), "a statement runs: {first:?}");
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    src.bind(&binding_ref(&resolved)).unwrap();
    assert_eq!(pass(&mut src, &claims), ROWS);
    assert_eq!(owner_keys(&parts), vec![vec![(acct, first.key())]; 3], "statement 1 owns the uses of the cursors it opened");
    src.unbind();
    drop(resolved);
    memledger::statement_end();
    assert_eq!(owner_keys(&parts), session, "parked past statement 1: the kept cursors' uses are the session's");

    // statement 2 binds the parked source again: the kept cursors' uses are its own while it runs (trunk: the session's)
    memledger::statement_begin();
    let second = charge::context().0;
    assert!(matches!(second, Owner::Statement { .. }) && second != first, "a new statement runs: {second:?}");
    let resolved = Resolved::for_recipe(&recipe, list.clone());
    src.bind(&binding_ref(&resolved)).unwrap();
    assert_eq!(owner_keys(&parts), vec![vec![(acct, second.key())]; 3], "bound again: the kept cursors' uses are statement 2's, the parts it pins");
    assert_eq!(charge::running_owners(Acct(acct)).0, second.key(), "the key the watchdog's rank counts on this account");
    assert_eq!(pass(&mut src, &claims), ROWS, "the kept cursors read the same rows");
    src.unbind();
    drop(resolved);
    memledger::statement_end();
    assert_eq!(owner_keys(&parts), session, "parked again: the session's");

    drop(src.release_rel(0).expect("every relation's kept cursors"));
    assert!(parts.iter().all(|p| p.users() == 0), "released: no use left");
    consts::reset();
}
