//! The runner's backend bookkeeping (docs/gravity/exec.md): the served-plan

use gravity_exec::{Backend, Verdict, MAX_EXECS, MAX_PLANS};
use gravity_program::{Generation, Handle, PlanKey};

fn key(handle: u64, stmt: u32) -> PlanKey {
    PlanKey { handle: Handle(handle), generation: Generation(7 | stmt << 24) }
}

#[test]
fn a_backend_starts_empty() {
    let b = Backend::new_for_test();
    assert_eq!(b.plans.iter().filter(|p| p.is_some()).count(), 0);
    assert_eq!(b.execs.iter().filter(|e| e.is_some()).count(), 0);
    assert!(b.memo.is_empty());
    assert_eq!(b.plans.len(), MAX_PLANS);
    assert_eq!(b.execs.len(), MAX_EXECS);
}

#[test]
fn a_refusal_is_memoized_and_probes_as_refused() {
    let mut b = Backend::new_for_test();
    let k = key(0x10, 0);
    assert_eq!(b.lookup(k), None);
    b.remember_refusal(k);
    assert_eq!(b.lookup(k), Some(Verdict::Refused));
}

#[test]
fn dropping_a_handle_forgets_every_statement() {
    let mut b = Backend::new_for_test();
    for s in 0..4 {
        b.remember_refusal(key(0x20, s));
    }
    b.remember_refusal(key(0x21, 0));
    b.drop_plans(Some(Handle(0x20)));
    for s in 0..4 {
        assert_eq!(b.lookup(key(0x20, s)), None, "statement {s} re-walks once");
    }
    assert_eq!(b.lookup(key(0x21, 0)), Some(Verdict::Refused));
    b.drop_plans(None);
    assert!(b.memo.is_empty());
}

#[test]
fn the_token_encoding_round_trips_the_slot_index_and_generation() {
    for idx in 0..MAX_EXECS {
        for gen in [1u32, 2, 255, 0x00ff_ffff] {
            let token = (idx as u64 + 1) | ((gen as u64) << 8);
            assert_eq!((token & 0xff) as usize - 1, idx);
            assert_eq!((token >> 8) as u32, gen);
        }
    }
}

/// The entry's refusal (fallthrough-cost.md): probed as its own verdict, never over one bind wrote, gone with the handle.
#[test]
fn the_entrys_refusal_is_memoized_beside_binds() {
    let mut b = Backend::new_for_test();
    let k = key(0x30, 0);
    b.remember_entry_refusal(k);
    assert_eq!(b.lookup(k), Some(Verdict::EntryRefused));
    // bind's own verdict stands (its crossing is wider than the entry's)
    let k2 = key(0x31, 0);
    b.remember_refusal(k2);
    b.remember_entry_refusal(k2);
    assert_eq!(b.lookup(k2), Some(Verdict::Refused));
    b.drop_plans(Some(Handle(0x30)));
    assert_eq!(b.lookup(k), None);
    assert_eq!(b.lookup(k2), Some(Verdict::Refused));
    assert_eq!(b.admissions, 0, "a memo probe runs no admission");
}

/// D2908: a refusal is memoized as the entry's with its reason, read back while the memo holds it, never over bind's
/// verdict, and gone with the plan's handle.
#[test]
fn a_refusal_is_kept_with_its_reason() {
    let mut b = Backend::new_for_test();
    let k = key(0x40, 0);
    b.remember_off_refusal(k, "graph: lower: why");
    assert_eq!(b.lookup(k), Some(Verdict::EntryRefused));
    assert_eq!(b.off_refusal(k), Some("graph: lower: why"));
    // bind's own verdict stands, and no reason is kept for it
    let k2 = key(0x41, 0);
    b.remember_refusal(k2);
    b.remember_off_refusal(k2, "graph: lower: other");
    assert_eq!(b.off_refusal(k2), None);
    // another generation of the same handle is another plan
    assert_eq!(b.off_refusal(key(0x40, 1)), None);
    b.drop_plans(Some(Handle(0x40)));
    assert_eq!(b.off_refusal(k), None);
    b.remember_off_refusal(k, "graph: lower: again");
    b.drop_plans(None);
    assert_eq!(b.off_refusal(k), None);
    assert_eq!(b.admissions, 0, "a memo probe runs no admission");
}

/// perfdiag3 fix 1: whole-part claims only while every worker gets a part; per-part code groups always (V4R-10: not a site
/// that only reads a code lane); an answerable site unless sparse.
#[test]
fn whole_part_claims_split_inside_a_part_below_the_width() {
    use gravity_exec::runner::whole_part_claims as law;
    assert!(law(false, false, false, 8, 8), "8 live parts at W8: one claim per part");
    assert!(law(false, false, false, 40, 8), "more parts than workers: whole parts");
    assert!(!law(false, false, false, 2, 8), "2 parts at W8: the sizer cuts claims inside the parts");
    assert!(law(false, false, false, 2, 2), "2 parts at W2: whole parts");
    assert!(law(false, false, false, 2, 1), "serial: one part per claim");
    assert!(law(true, false, true, 1, 8), "per-part code groups (the text-groups fold) run whole parts, sparse or not");
    assert!(law(false, true, false, 1, 8), "an answerable site answers per part");
    assert!(!law(false, true, true, 1, 8), "a sparse survivor set is cut one granule per claim (no code lane)");
}

/// The tail rule over the law's whole parts: the first ⌊P/W⌋·W live parts whole, the last P mod W cut inside each part.
/// Does not apply: every wave full (P mod W = 0), fewer parts than the width (the law cuts already), serial.
#[test]
fn whole_part_claims_cut_only_the_last_p_mod_w_parts() {
    use gravity_exec::runner::tail_from_part as tail;
    assert_eq!(tail(10, 8), Some(8), "claims kit: 10 parts at W8, the last 2 cut");
    assert_eq!(tail(10, 4), Some(8), "10 parts at W4: two full waves, the last 2 cut");
    assert_eq!(tail(15, 8), Some(8), "TPC-H orders at W8: the last 7 cut");
    assert_eq!(tail(60, 8), Some(56), "TPC-H lineitem at W8: the last 4 cut");
    assert_eq!(tail(511, 8), Some(504), "511 parts at W8: the last 7 cut");
    assert_eq!(tail(16, 8), None, "two full waves: nothing cut");
    assert_eq!(tail(8, 8), None, "one full wave: nothing cut");
    assert_eq!(tail(60, 4), None, "TPC-H lineitem at W4: every wave full");
    assert_eq!(tail(2, 8), None, "fewer parts than workers: the law cuts every part already");
    assert_eq!(tail(10, 1), None, "serial");
    assert_eq!(tail(10, 0), None, "no section");
}

/// D2845: the admission arena is mapped at the level's first admission (an address-space rail's refusal is that
/// admission's, by name), once: later admissions get the same arena.
#[test]
fn the_admission_arena_is_mapped_at_first_use_once() {
    let mut b = Backend::new_for_test();
    let a = b.graph_arena().expect("mapped without a rail");
    assert!(a.capacity() >= gravity_exec::admit::ARENA_BYTES);
    assert_eq!(b.graph_arena().expect("kept"), a, "one mapping per level");
}

/// D2845: the grant pool's out-of-memory (`glue::pg_error`'s 53200) is the statement's error, never a refusal: the recheck
/// licence's `refusal_of` names a refusal alone.
#[test]
fn the_pool_out_of_memory_is_the_statements_error() {
    use gravity_exec::runner::{refusal_of, refused};
    use types_error::ERRCODE_OUT_OF_MEMORY;
    let pool = gravity_exec::glue::pg_error(gravity_driver::Error::OutOfMemory { bytes: 160 << 20, held: 2048 << 20, capacity: 2112 << 20, why: None });
    assert_eq!(pool.sqlstate(), ERRCODE_OUT_OF_MEMORY);
    assert_eq!(refusal_of(&pool), None);
    assert_eq!(refusal_of(&refused("hosts: x")), Some("hosts: x"));
}

/// D3816: a licence never unserves a statement. A graph that took a licence and whose lowering
/// refuses by Shape is admitted again in plan order (a read to its end: `deny_all`; one a count may stop: bounded) and
/// served there; where the plan-order form refuses too the first reason stands. A refusal by another reason or stage, one
/// of a graph that took no licence, and one inside the plan-order run are not admitted again.
#[test]
fn a_licensed_shape_refusal_is_admitted_again_in_plan_order() {
    use gravity_decompose::effects::{deny_all, denying, denying_bounded, licence, lowering, Departure, Effects};
    use gravity_exec::admit::Refused;
    use gravity_exec::runner::{or_plan_order, relowers};
    use types_error::PgResult;
    let shape = |n: u32| Refused { stage: "lower", what: format!("lowerv4: Shape: a keyed groups emit re-read under a rerun set whose owner runs inside a CTE's pulled plan (node {n})") };
    // a family whose departure needs a licence, the licensed form refused by its lowering; `plan` answers its PostgreSQL form
    let family = |runs: &mut Vec<(bool, bool)>, plan: Result<u32, Refused>| -> PgResult<Result<u32, Refused>> {
        runs.push((denying(), denying_bounded()));
        Ok(match licence(Effects::RAISES, Departure::Ahead) {
            Ok(_) => Err(shape(1)),
            Err(_) => plan,
        })
    };
    let mut runs = Vec::new();
    let (r, again) = or_plan_order(true, || family(&mut runs, Ok(7))).unwrap();
    assert!(again && matches!(r, Ok(7)), "served in plan order");
    assert_eq!(runs, [(false, false), (true, false)], "the licensed run, then the plan-order run");
    assert!(!denying(), "restored");
    runs.clear();
    let (r, again) = or_plan_order(false, || family(&mut runs, Ok(7))).unwrap();
    assert!(again && matches!(r, Ok(7)) && runs == [(false, false), (true, true)], "a counted read: the bounded run");
    runs.clear();
    let (r, again) = or_plan_order(true, || family(&mut runs, Err(shape(2)))).unwrap();
    assert!(again && runs.len() == 2 && r.is_err_and(|w| w.what.ends_with("(node 1)")), "both refuse: the first reason");
    // no licence taken: refused once
    let mut n = 0;
    let (r, again) = or_plan_order(true, || {
        n += 1;
        Ok::<Result<u32, Refused>, _>(Err(shape(3)))
    })
    .unwrap();
    assert!(!again && n == 1 && r.is_err());
    // inside the plan-order run (the errorhold re-run): never again
    assert!(!deny_all(|| relowers(&shape(4), true, false)));
    assert!(relowers(&shape(4), true, false) && !relowers(&shape(4), false, false));
    assert!(!relowers(&shape(4), true, true), "a statement that acts: no plan-order form (D3540)");
    assert!(!relowers(&Refused { stage: "lower", what: "lowerv4: NotBuilt M9: hook Variant (node 5)".into() }, true, false), "not a Shape");
    assert!(!relowers(&Refused { stage: "lower", what: "an InitPlan without its exec word".into() }, true, false));
    assert!(!relowers(&Refused { stage: "decompose", what: "lowerv4: Shape: x".into() }, true, false), "the lowering's only");
    // a statement that acts (its lowering entered with `acts`): the licensed form's refusal stands, lowered once
    runs.clear();
    let (r, again) = or_plan_order(true, || lowering(true, false, || family(&mut runs, Ok(7)))).unwrap();
    assert!(!again && runs.len() == 1 && r.is_err(), "a statement that acts is not lowered again");
    let (r, again) = or_plan_order(true, || lowering(false, false, || family(&mut runs, Ok(7)))).unwrap();
    assert!(again && matches!(r, Ok(7)), "one that does not act is: the flag was taken");
    // the graph path's admissions counter is the backend's
    assert_eq!(Backend::new_for_test().relowered, 0);
}
