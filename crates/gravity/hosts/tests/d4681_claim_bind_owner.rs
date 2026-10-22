//! D4681 / D4630 (b): a worker slot's granule source bound at its first claim, on the claiming thread (the switch
//! `slots::bind_at_claim`): the claim's thread runs under the leader's account and its statement's owner (the claim
//! census's memory account: `charge::enter_as(context_of(acct))`), so the bind homes the kept cursors' part uses on the
//! statement, never on the pool thread's own account or the session. Knob off (the rejoin): every kept use is the
//! statement's while it runs. Knob on (kept_parked): the kept uses stay parked where the last park put them, and the
//! statement's reads take its own uses, on the statement. One test: the knobs are process-wide.

use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use gravity_abi::Lane;
use gravity_driver::{blank_site, BindingRef, CommandId, Identity, PerTuple, Pins, Snapshot, Source, Window};
use gravity_hosts::ops::scan::pgrc::granule_source::{GranuleRangeSource, GranuleRecipe, PartIoSpec, PartList, PartSpec, Resolved};
use gravity_parallel::{ClaimSize, GranuleRange, MorselSource};
use memledger::charge::owner_row;
use memledger::Owner;
use mempolicy::consts::{self, K};
use pgrc_format::dirlayout::part_file_name;
use pgrc_qa::corpus::{build_fixture, int8_fixture, open_part_bytes};
use pgrc_read::{OpenPart, PartExpect, PartRegistry};
use pgrc_write::writer::PartCutPolicy;

const MAX_LANES: usize = 32;
const REL: u32 = 4681;
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
fn a_source_bound_on_a_claiming_thread_homes_its_kept_uses_on_the_statement() {
    let bf = build_fixture(int8_fixture("gravity_d4681", REL as u64, ROWS, vec![], PartCutPolicy { max_rows: 20_000, ..PartCutPolicy::default() }, |i| Some((i as i64 * 31) % 1000)));
    let parts: Vec<Arc<OpenPart>> = bf.manifest.parts.iter().enumerate().map(|(i, rec)| open_part_bytes(bf.files.get(&part_file_name(rec.part_no)).expect("part file"), 4681 + i as u64).expect("open part")).collect();
    assert_eq!(parts.len(), 3, "the fixture cut into three parts");
    let specs = bf
        .manifest
        .parts
        .iter()
        .enumerate()
        .map(|(i, rec)| {
            let bytes = bf.files.get(&part_file_name(rec.part_no)).expect("part file");
            PartSpec { io: PartIoSpec::Mem { bytes: Arc::from(bytes.as_slice()), dev: 7, ino: 4681 + i as u64 }, expect: PartExpect::none(), rows: rec.rows, granules: rec.granule_count, deletes: None, part: Some(parts[i].clone()) }
        })
        .collect();
    let list = Arc::new(PartList::new(specs));
    let recipe = GranuleRecipe::for_test(REL, &[1], Arc::new(PartRegistry::new(64 << 20)));
    let all = GranuleRange::over_parts(list.granules(), ClaimSize::Fixed(1), list.starts());
    let on = || parts.iter().map(|p| p.user_accounts()).collect::<Vec<_>>();
    memledger::bind_backend(197, 9197);
    for parked in [0u64, 1] {
        consts::set(K::KeptParked, parked);
        let mut src = GranuleRangeSource::new(recipe.clone());
        // statement 1 on the leader keeps a cursor a part; its end parks them on the session
        memledger::statement_begin();
        let resolved = Resolved::for_recipe(&recipe, list.clone());
        src.bind(&slot_ref(&resolved)).unwrap();
        assert_eq!(pass(&mut src, &all), ROWS, "parked={parked}: statement 1 reads every part");
        src.unbind();
        src.settle_kept();
        drop(resolved);
        memledger::statement_end();
        // statement 2: the slot's bind and reads on a claiming thread, under the statement's owner as a claim runs
        memledger::statement_begin();
        let st = memledger::charge::context().0;
        assert!(matches!(st, Owner::Statement { .. }), "parked={parked}: the leader's context is its statement");
        let sess = Owner::Session(st.acct());
        let resolved = Resolved::for_recipe(&recipe, list.clone());
        let (st0, sess0) = (owner_row(st).committed, owner_row(sess).committed);
        let ((at_bind, st_bind, sess_bind), (mid, st_mid, sess_mid)) = std::thread::scope(|s| {
            s.spawn(|| {
                let _claim = memledger::charge::enter_as(st);
                assert_eq!(memledger::charge::context().0, st, "parked={parked}: the claiming thread charges the statement");
                src.bind(&slot_ref(&resolved)).unwrap();
                let at_bind = (on(), owner_row(st).committed, owner_row(sess).committed);
                assert_eq!(pass(&mut src, &all), ROWS, "parked={parked}: statement 2 reads every part on the claiming thread");
                let mid = (on(), owner_row(st).committed, owner_row(sess).committed);
                // a worker slot's detach unbinds on its own thread
                src.unbind();
                (at_bind, mid)
            })
            .join()
            .unwrap()
        });
        // the bind's held bytes (and the kept cursors' read owner with them) are the statement's: the session's parked
        // bytes moved there, nothing new on the session
        assert!(st_bind > st0 && st_mid > st0, "parked={parked}: the held charge homes on the statement (statement committed {st0} -> bind {st_bind}, mid {st_mid})");
        assert!(sess_bind <= sess0 && sess_mid <= sess0, "parked={parked}: nothing new on the session (session committed {sess0} -> bind {sess_bind}, mid {sess_mid})");
        let stmt_use = (st.acct().0, st.key(), 1);
        if parked == 0 {
            assert_eq!(at_bind, vec![vec![stmt_use]; 3], "off: the claiming thread's bind takes every kept use onto the statement");
            // the kept use beside the reads' own (the count may be 2): every one the statement's
            assert!(mid.iter().all(|u| matches!(u[..], [(a, k, n)] if (a, k) == (st.acct().0, st.key()) && n >= 1)), "off: every use of a part the statement's while it runs: {mid:?}");
        } else {
            let parked_on = vec![vec![(sess.acct().0, sess.key(), 1)]; 3];
            assert_eq!(at_bind, parked_on, "on: the bind leaves the kept uses parked where the last statement's end put them");
            assert!(mid.iter().flatten().all(|&(a, k, _)| (a, k) == (st.acct().0, st.key()) || (a, k) == (sess.acct().0, sess.key())), "on: every use the statement's or still parked: {mid:?}");
        }
        assert!(at_bind.iter().chain(mid.iter()).flatten().all(|&(a, _, _)| a == st.acct().0), "parked={parked}: no use on another account (the claiming thread's own, UNBOUND)");
        src.settle_kept();
        drop(resolved);
        memledger::statement_end();
        drop(src.release_rel(0).expect("every relation's kept cursors"));
        assert!(parts.iter().all(|p| p.users() == 0), "parked={parked}: released, no use left");
    }
    consts::reset();
    memledger::unbind_backend();
}
