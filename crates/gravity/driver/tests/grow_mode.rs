//! Grow mode (`pgrust.gravity_grow`; the exec admission sets `Grant::grow`): the admission claims the
//! statement's estimate start, never past today's reservation and under it where today's 64 MiB grant floor dominates
//! (no grant floor, each share at most its PostgreSQL per-node budget, no learned raise, hash breakers small beside
//! others), and what a statement needs past it grows from the pool's room now. D4559 (TPC-H q20 W16):
//! a hash join's admitted-key set held against the grant was refused whenever the statement's other regions had drawn G's
//! room to 0 (every key admitted, 308-321 ms vs 50); under grow mode it grows past G and is held, counted as grown, and a
//! pool with no room refuses it at once (no wait, nothing mapped without room). Flag off: the hold is bounded by G as
//! before. One test: it arms the process-wide pool.

use gravity_driver_rig as common;

use std::time::{Duration, Instant};

use common::*;
use gravity_driver::{footprint, Grant, StartSize, POOL, REGIONS};

const MIB: u64 = 1 << 20;

/// Today's Estimate ask at the GUCs' grant `cap` (exec `ask`): the estimate, floored at the 64 MiB grant floor, at most the
/// cap, with the cap's PostgreSQL floors; beside another statement its hash breakers start at their shares.
fn todays(cap: Grant, estimate: u64) -> Grant {
    cap.trimmed(estimate.max(64 * MIB).min(cap.bytes)).with_floors(cap.sort_floor, cap.hash_floor).with_start(StartSize::Share)
}

/// Grow mode's ask at the same cap: the estimate start (`mempolicy::grow::grow_want`), the floors the PostgreSQL budgets
/// (the caps of its shares), small beside others, the ceilings the region wall (work_mem unset).
fn grows(cap: Grant, estimate: u64) -> Grant {
    let c = mempolicy::grow::grow_ceiling(0, gravity_driver::mem::grow::MAX_REGION);
    let cap = cap.with_grow(true).with_ceilings(c, c).with_start(mempolicy::admit::decide_start_in(1, None, true));
    cap.trimmed(mempolicy::grow::grow_want(estimate, 1, cap.bytes)).with_floors(cap.sort_floor, cap.hash_floor)
}

#[test]
fn grow_mode_claims_less_and_grows_its_holds_past_g() {
    POOL.gate(0);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();

    // (1) the start, for a hash aggregate the plan models at 100k rows: never past today's reservation at W0 and W8 at
    // the default GUCs' cap (hash_mem a participant, the PostgreSQL floors), though its ceilings are the region wall; under
    // it where today's 64 MiB grant floor is past the estimate (W0 under a pgrust.gravity_grant_mb cap: no floors)
    let program = p_group_spill_rows(4, 100_000, 3, None);
    let pg = |width: u16| Grant::of(8 * MIB * width as u64).with_floors(4 * MIB, 8 * MIB).with_ceilings(4 * MIB, 8 * MIB);
    let mb = Grant::of(1024 * MIB).with_ceilings(64 * MIB, 128 * MIB);
    for (width, cap, under) in [(1u16, pg(1), false), (8, pg(8), false), (1, mb, true)] {
        let estimate = gravity_program::cost::grant_need_of_at(&program.pipelines, program.slots.max_live, width, false, false).unwrap_or(0);
        let (today, grow) = (footprint(todays(cap, estimate), &program, width), footprint(grows(cap, estimate), &program, width));
        assert!(grow <= today, "W{width} cap {} B: grow mode's reservation {grow} B is past today's {today} B (estimate {estimate} B)", cap.bytes);
        assert!(!under || grow < today, "W{width} cap {} B: grow mode's reservation {grow} B is under today's {today} B (estimate {estimate} B)", cap.bytes);
    }

    // (2) D4559: a hold past G's room
    POOL.arm(256 * MIB);
    let admit = |g: Grant| POOL.admit_need(g, &|g| g.bytes, Duration::from_millis(10)).expect("admitted");
    let on = admit(Grant::of(8 * MIB).with_grow(true));
    let off = admit(Grant::of(8 * MIB));
    let (g_on, g_off) = (on.hold(on.left()).expect("G's room held"), off.hold(off.left()).expect("G's room held"));
    assert_eq!((on.left(), off.left()), (0, 0), "G's room drawn to 0 by the statement's other regions");
    assert!(on.hold(4 * MIB).is_none() && off.hold(4 * MIB).is_none(), "Reservation::hold stays bounded by G");
    assert!(off.hold_growing(4 * MIB).is_none(), "flag off: refused at G, every key admitted, as before");
    let grown = on.grown();
    let set = on.hold_growing(4 * MIB).expect("grow mode: the set grows past G and is held");
    assert!(on.grown() >= grown + 4 * MIB, "counted as grown past G ({} B)", on.grown() - grown);

    // (3) no room now: refused at once, nothing mapped past the pool, no timer fallback
    let rest = POOL.keep(POOL.capacity().saturating_sub(POOL.held()));
    let t0 = Instant::now();
    assert!(on.hold_growing(4 * MIB).is_none(), "a full pool refuses the grow");
    assert!(t0.elapsed() < Duration::from_millis(500), "refused without a wait: {:?}", t0.elapsed());

    drop((rest, set, g_on, g_off));
    drop((on, off));
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    assert_eq!(POOL.held(), 0, "every byte back at the statements' end");
    POOL.arm(0);
}
