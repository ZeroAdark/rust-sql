//! D4554 (the `kept_parked` switch): a binding statement took every kept cursor's part use (D3994 B.1 R's rejoin) and gave
//! each back at its end, so every statement moved every cursor a worker set had ever kept, on the leader. Under
//! `kept_parked = 1` the kept cursors stay parked while a statement reads through them: the statement's bind moves none,
//! a part it reads is its in-use part through its own use, and its end visits only the parts it opened. Same rows. The
//! slot depot's park after several binds with no park between still moves every kept use to the depot. An unbind leaves
//! its uses on the statement, parked, so the depot's park moves each once; with no park the statement's end
//! (`settle_kept`) or the source's drop moves them to the session: no statement ends holding one. One test: the knob is
//! process-wide.

use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use gravity_abi::Lane;
use gravity_driver::{blank_site, BindingRef, CommandId, Identity, PerTuple, Pins, Snapshot, Source, Window};
use gravity_hosts::ops::scan::pgrc::granule_source::{GranuleRangeSource, GranuleRecipe, PartIoSpec, PartList, PartSpec, Resolved};
use gravity_parallel::{ClaimSize, GranuleRange, MorselSource};
use memledger::{Holder, Owner, Release};
use mempolicy::consts::{self, K};
use pgrc_format::dirlayout::part_file_name;
use pgrc_qa::corpus::{build_fixture, int8_fixture, open_part_bytes};
use pgrc_read::{OpenPart, PartExpect, PartRegistry};
use pgrc_write::writer::PartCutPolicy;

const MAX_LANES: usize = 32;
const REL: u32 = 4555;
const ROWS: u64 = 60_000;
static FLAG: AtomicU8 = AtomicU8::new(0);

fn binding_ref(resolved: &Resolved) -> BindingRef<'_> {
    BindingRef { snapshot: Snapshot(0), command_id: CommandId(0), params: &[], abort_flag: &FLAG, pins: Pins(0), per_tuple: PerTuple(0), identity: Identity::default(), resolved: Some(resolved), ordinal: None }
}

/// A worker slot's bind (`Binding::fork`): only its sources park in the slot depot.
fn slot_ref(resolved: &Resolved) -> BindingRef<'_> {
    BindingRef { ordinal: Some(0), ..binding_ref(resolved) }
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
fn kept_cursors_stay_parked_while_a_statement_reads_and_its_end_visits_only_the_parts_it_opened() {
    let bf = build_fixture(int8_fixture("gravity_d4554p", REL as u64, ROWS, vec![], PartCutPolicy { max_rows: 20_000, ..PartCutPolicy::default() }, |i| Some((i as i64 * 31) % 1000)));
    let parts: Vec<Arc<OpenPart>> = bf.manifest.parts.iter().enumerate().map(|(i, rec)| open_part_bytes(bf.files.get(&part_file_name(rec.part_no)).expect("part file"), 4555 + i as u64).expect("open part")).collect();
    assert_eq!(parts.len(), 3, "the fixture cut into three parts");
    let specs = bf
        .manifest
        .parts
        .iter()
        .enumerate()
        .map(|(i, rec)| {
            let bytes = bf.files.get(&part_file_name(rec.part_no)).expect("part file");
            PartSpec { io: PartIoSpec::Mem { bytes: Arc::from(bytes.as_slice()), dev: 7, ino: 4555 + i as u64 }, expect: PartExpect::none(), rows: rec.rows, granules: rec.granule_count, deletes: None, part: Some(parts[i].clone()) }
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
    let live = || parts.iter().map(|p| p.live_users()).collect::<Vec<_>>();

    // a backend slot (a procno past the slots is UNBOUND: its uses never a statement's)
    memledger::bind_backend(199, 9199);
    // one statement: bind, the live uses the bind left (before any read), the rows, the live uses mid-statement, unbind
    let stmt = |src: &mut GranuleRangeSource, claims: &GranuleRange| {
        memledger::statement_begin();
        let resolved = Resolved::for_recipe(&recipe, list.clone());
        src.bind(&binding_ref(&resolved)).unwrap();
        let at_bind = live();
        let rows = pass(src, claims);
        let mid = live();
        src.unbind();
        src.settle_kept();
        drop(resolved);
        memledger::statement_end();
        (at_bind, rows, mid)
    };

    for parked in [1u64, 0] {
        consts::set(K::KeptParked, parked);
        let mut src = GranuleRangeSource::new(recipe.clone());
        let (_, rows, _) = stmt(&mut src, &all);
        assert_eq!(rows, ROWS, "parked={parked}: bind 1 reads every part");
        assert_eq!((users(), live()), (vec![1, 1, 1], vec![0, 0, 0]), "parked={parked}: one kept cursor a part, parked after the end");
        let (at_bind, rows, mid) = stmt(&mut src, &first);
        assert_eq!(rows, part0_rows, "parked={parked}: bind 2 reads part 0 alone, the same rows");
        if parked == 1 {
            assert_eq!(at_bind, vec![0, 0, 0], "parked: the bind takes no kept cursor's use");
            assert_eq!(&mid[1..], &[0, 0], "parked: parts 1 and 2, unread, stay parked through the statement");
        } else {
            assert_eq!(at_bind, vec![1, 1, 1], "off: the bind takes every kept cursor's use (B.1 R's rejoin)");
            assert_eq!(&mid[1..], &[1, 1], "off: parts 1 and 2 stay the statement's while it runs");
        }
        assert_eq!((users(), live()), (vec![1, 1, 1], vec![0, 0, 0]), "parked={parked}: the end parks every kept use again");
        let (_, rows, _) = stmt(&mut src, &all);
        assert_eq!(rows, ROWS, "parked={parked}: bind 3 reads every part through the kept cursors");
        assert_eq!((users(), live()), (vec![1, 1, 1], vec![0, 0, 0]), "parked={parked}: no cursor more, none left live");
        drop(src.release_rel(0).expect("every relation's kept cursors"));
        assert!(parts.iter().all(|p| p.users() == 0), "parked={parked}: released, no use left");
    }

    // The slot depot's park after several binds (a worker slot binds its sources again at each section of a statement):
    // bind 1 opens parts 0 and 1, bind 2 part 2 alone, no park between; each unbind moved its own cursors' uses to the
    // session. The park then leaves no kept use there: every one is the depot holder's (Cache(Parked)), knob on and off.
    let mut front = GranuleRange::over_parts(list.granules(), ClaimSize::Fixed(1), list.starts());
    front.set_dead(&[(list.starts()[2], list.granules())]);
    let mut back = GranuleRange::over_parts(list.granules(), ClaimSize::Fixed(1), list.starts());
    back.set_dead(&[(0, list.starts()[2])]);
    let (front_rows, back_rows) = (bf.manifest.parts[0].rows + bf.manifest.parts[1].rows, bf.manifest.parts[2].rows);
    let depot = Owner::Cache(Holder::Parked);
    for parked in [1u64, 0] {
        consts::set(K::KeptParked, parked);
        let mut src = GranuleRangeSource::new(recipe.clone());
        let (_, rows, _) = stmt(&mut src, &front);
        assert_eq!(rows, front_rows, "parked={parked}: bind 1 reads parts 0 and 1");
        let (_, rows, _) = stmt(&mut src, &back);
        assert_eq!(rows, back_rows, "parked={parked}: bind 2 reads part 2 alone");
        assert_eq!(users(), vec![1, 1, 1], "parked={parked}: one kept cursor a part");
        src.kept_uses_to(depot, Release::Evict);
        let on = parts.iter().map(|p| p.user_accounts()).collect::<Vec<_>>();
        assert_eq!(on, vec![vec![(depot.acct().0, depot.key(), 1)]; 3], "parked={parked}: the park moves every kept use to the depot's holder, those of bind 1's parts too");
        drop(src.release_rel(0).expect("every relation's kept cursors"));
        assert!(parts.iter().all(|p| p.users() == 0), "parked={parked}: released, no use left");
    }
    // The pending park (knob on, a worker slot's source): the unbind moves no use; the depot's park moves each statement ->
    // depot once, else the statement's end (`settle_kept`) moves them to the session, else the source's drop does; a Node's
    // own source (no ordinal: it never parks in the depot) moves them to the session at the unbind, once. The statement's
    // account ends holding none of them (owner-end residues, uses).
    consts::set(K::KeptParked, 1);
    let residue_uses = || memledger::owner_end::totals(memledger::owner_end::Source::Owner, memledger::OwnerClass::Statement).uses;
    let on = || parts.iter().map(|p| p.user_accounts()).collect::<Vec<_>>();
    let unbound = |src: &mut GranuleRangeSource, slot: bool| {
        memledger::statement_begin();
        let st = memledger::charge::context().0;
        let resolved = Resolved::for_recipe(&recipe, list.clone());
        src.bind(&if slot { slot_ref(&resolved) } else { binding_ref(&resolved) }).unwrap();
        assert_eq!(pass(src, &all), ROWS, "every part read");
        src.unbind();
        drop(resolved);
        st
    };
    let r0 = residue_uses();
    for path in ["park", "settle", "drop", "node"] {
        let mut src = GranuleRangeSource::new(recipe.clone());
        let st = unbound(&mut src, path != "node");
        assert!(matches!(st, Owner::Statement { .. }), "{path}: the uses' owner is the statement");
        let sess = Owner::Session(st.acct());
        if path == "node" {
            assert_eq!((on(), live()), (vec![vec![(sess.acct().0, sess.key(), 1)]; 3], vec![0, 0, 0]), "node: a Node's own source moves its uses to the session at the unbind");
            src.settle_kept();
            assert_eq!(on(), vec![vec![(sess.acct().0, sess.key(), 1)]; 3], "node: nothing pending for the statement's end");
        } else {
            assert_eq!((on(), live()), (vec![vec![(st.acct().0, st.key(), 1)]; 3], vec![0, 0, 0]), "{path}: unbound, every use still the statement's, parked");
        }
        match path {
            "park" => {
                src.kept_uses_to(depot, Release::Evict);
                assert_eq!(on(), vec![vec![(depot.acct().0, depot.key(), 1)]; 3], "park: statement -> depot, once");
                src.settle_kept();
                assert_eq!(on(), vec![vec![(depot.acct().0, depot.key(), 1)]; 3], "park: nothing left for the statement's end");
            }
            "settle" => {
                src.settle_kept();
                assert_eq!(on(), vec![vec![(sess.acct().0, sess.key(), 1)]; 3], "settle: no park, the statement's end moves them to the session");
            }
            "node" => {}
            _ => {
                drop(core::mem::replace(&mut src, GranuleRangeSource::new(recipe.clone())));
                assert!(parts.iter().all(|p| p.users() == 0), "drop: the dropped source's uses gone");
            }
        }
        memledger::statement_end();
        assert_eq!(residue_uses(), r0, "{path}: no statement ended holding a kept use");
        drop(src.release_rel(0));
        assert!(parts.iter().all(|p| p.users() == 0), "{path}: released, no use left");
    }
    consts::reset();
    memledger::unbind_backend();
}
