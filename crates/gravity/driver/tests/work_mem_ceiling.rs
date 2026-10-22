//! Each breaker's ceiling is PostgreSQL's per-node budget (work_mem, x hash_mem_multiplier for a hash node): a spilling
//! breaker grows past its share up to its ceiling whatever its grant holds (the pool's room now pays for it), and spills
//! past it even with the pool free, as PostgreSQL's node does; a grant without ceilings keeps the W-then-G road.
//! `k_range_grow_in`'s breaker, 32 MiB of rows at W0, under a 2 MiB hash ceiling and under none; and its share opened at
//! the pool's room when the full pool refuses it, inside its ceiling's reservation (D3226 (c1)'s road, no 53200).

use gravity_driver_rig as common;

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use gravity_driver::{Call, Entry, Grant, Reservation, Step, POOL, REGIONS, SHARE_FLOOR};

const NULLS: [u8; 1] = [0];
const PER_BLOCK: u64 = 64;
/// 512x the 64 KiB share floor in records (16 bytes a row): 32 MiB of rows.
const BLOCKS: u64 = (512 * SHARE_FLOOR as u64 / 16) / PER_BLOCK;
const MIB: u64 = 1 << 20;
/// The pool is one per process: the tests take turns.
static LOCK: Mutex<()> = Mutex::new(());

fn run(program: &Arc<gravity_program::Program>, stmt: std::rc::Rc<Reservation>) -> Vec<Vec<u64>> {
    let mut derive = FakeDerive::ranges(RangeTable::new(BLOCKS, PER_BLOCK));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let mut b = binding(&[0], &NULLS, &mut sink, &flag);
    let mut exec = entry.start(None, program, &mut derive, &mut b, stmt).unwrap();
    assert_eq!(entry.run_pipeline(&mut exec, &mut b, Call::batch(64)).unwrap(), Step::Done);
    assert_eq!(entry.run(&mut exec, &mut b, Call::batch(64)).unwrap(), Step::Done);
    entry.end(exec, true);
    sink.sorted()
}

#[test]
fn a_spilling_breaker_grows_to_its_ceiling_and_spills_past_it() {
    let _l = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let program = Arc::new(p_grow_in(4));
    let ceil = 2 * MIB;
    // a grant with nothing past its share for the breaker: the ceiling alone decides how far it grows
    let capped = Grant::of(SHARE_FLOOR as u64).with_ceilings(ceil, ceil);
    let (refused, at_ceiling) = (GROW_IN_REFUSED.load(Ordering::Relaxed), gravity_driver::mem::grow::AT_CEILING.load(Ordering::Relaxed));
    GROW_IN_LIMIT.store(0, Ordering::Relaxed);
    let got = run(&program, lease(capped));
    let kept = got[0][0];
    let limit = GROW_IN_LIMIT.load(Ordering::Relaxed);
    assert!(limit > SHARE_FLOOR as u64 && limit <= ceil, "grew past its share up to the 2 MiB ceiling, never past it: limit {limit}");
    assert!(kept * 16 + GROW_HDR > ceil / 2 && kept * 16 + GROW_HDR <= ceil, "the ceiling's rows kept: {kept}");
    assert!(GROW_IN_REFUSED.load(Ordering::Relaxed) > refused, "past the ceiling the breaker spills");
    assert!(gravity_driver::mem::grow::AT_CEILING.load(Ordering::Relaxed) > at_ceiling, "the spills at the ceiling are counted");
    // no ceilings (a fixed grant): the old road, W then G; this unadmitted lease has no G, so it spills at its share
    let refused = GROW_IN_REFUSED.load(Ordering::Relaxed);
    GROW_IN_LIMIT.store(0, Ordering::Relaxed);
    let got = run(&program, lease(Grant::of(SHARE_FLOOR as u64)));
    assert!(got[0][0] < kept, "without a ceiling the breaker keeps less: {} vs {kept}", got[0][0]);
    assert!(GROW_IN_REFUSED.load(Ordering::Relaxed) > refused);
}

/// D4080: a mapping a larger ceiling's breaker parked never grows a breaker past its own ceiling. The 2 MiB breaker's floor
/// is taken from the 4 MiB one's parked reservation, and its in-place grows stop at its ceiling, where it spills.
#[test]
fn a_larger_parked_mapping_never_grows_a_breaker_past_its_ceiling() {
    let _l = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    REGIONS.trim();
    let program = Arc::new(p_grow_in(4));
    GROW_IN_LIMIT.store(0, Ordering::Relaxed);
    run(&program, lease(Grant::of(SHARE_FLOOR as u64).with_ceilings(4 * MIB, 4 * MIB)));
    assert!(GROW_IN_LIMIT.load(Ordering::Relaxed) > 2 * MIB, "the 4 MiB breaker grew past 2 MiB");
    let ceil = 2 * MIB;
    let at_ceiling = gravity_driver::mem::grow::AT_CEILING.load(Ordering::Relaxed);
    GROW_IN_LIMIT.store(0, Ordering::Relaxed);
    run(&program, lease(Grant::of(SHARE_FLOOR as u64).with_ceilings(ceil, ceil)));
    let limit = GROW_IN_LIMIT.load(Ordering::Relaxed);
    assert!(limit > SHARE_FLOOR as u64 && limit <= ceil, "grew past its share up to its 2 MiB ceiling, never into the parked 4 MiB: limit {limit}");
    assert!(gravity_driver::mem::grow::AT_CEILING.load(Ordering::Relaxed) > at_ceiling, "it spilled at its ceiling");
    REGIONS.trim();
}

#[test]
fn a_ceiling_reservation_the_full_pool_refuses_opens_its_share_at_the_room_left() {
    let _l = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    POOL.gate(0);
    REGIONS.trim();
    assert_eq!(POOL.held(), 0);
    POOL.arm(256 * MIB);
    let program = Arc::new(p_grow_in(4));
    // admitted at 4 MiB of a 128 MiB grant whose hash ceiling (512 MiB) passes the share: the slot opens as a reservation of
    // the ceiling, its share past G drawn on the pool, which has 16 MiB left
    let grant = Grant::of(128 * MIB).with_ceilings(256 * MIB, 512 * MIB);
    let a = std::rc::Rc::new(POOL.admit_need(grant, &|_| 4 * MIB, Duration::from_millis(10)).unwrap());
    let fill = POOL.keep(POOL.capacity() - POOL.held() - 16 * MIB);
    let opened = gravity_driver::mem::region::OPENED_SMALLER.load(Ordering::Relaxed);
    let refused = GROW_IN_REFUSED.load(Ordering::Relaxed);
    let got = run(&program, a.clone());
    assert!(got[0][0] > 0, "rows kept in the share it opened at");
    assert_eq!(gravity_driver::mem::region::OPENED_SMALLER.load(Ordering::Relaxed) - opened, 1, "opened at the room left, not out of memory");
    assert!(a.grown() <= 16 * MIB, "the share past G is the room left: {} grown", a.grown());
    assert!(GROW_IN_REFUSED.load(Ordering::Relaxed) > refused, "past the share it opened at the breaker spills");
    assert_eq!(a.refused(), 0, "the refused open is served: no named refusal kept");
    drop(fill);
    drop(a);
    REGIONS.trim();
    gravity_driver::DEPOT.trim();
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}

/// The forced-spill test fault (`Grant::spill_every`, `pgrust.gravity_test_spill_every`): under a ceiling the breaker
/// grows to, every Nth room ask of its spilling road is refused, so the breaker spills there as past its ceiling, each
/// refusal counted in `FORCED`; 0 leaves its grows alone. At every ask it never grows past its share; at every second ask
/// the grants between the refusals still grow it.
#[test]
fn the_forced_spill_fault_refuses_every_nth_room_ask() {
    use gravity_driver::mem::grow::FORCED;
    let _l = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let program = Arc::new(p_grow_in(4));
    let ceil = 4 * MIB;
    let grant = Grant::of(SHARE_FLOOR as u64).with_ceilings(ceil, ceil);
    let at = || (GROW_IN_REFUSED.load(Ordering::Relaxed), FORCED.load(Ordering::Relaxed));
    // off: the breaker grows past its share toward its ceiling, and no refusal is forced
    let (refused, forced) = at();
    GROW_IN_LIMIT.store(0, Ordering::Relaxed);
    let off = run(&program, lease(grant))[0][0];
    assert!(GROW_IN_LIMIT.load(Ordering::Relaxed) > SHARE_FLOOR as u64, "off: grew past its share");
    assert_eq!(FORCED.load(Ordering::Relaxed), forced, "off: nothing forced");
    let off_refused = GROW_IN_REFUSED.load(Ordering::Relaxed) - refused;
    // every ask: each one refused and counted, the breaker never grows and keeps its share's rows
    let (refused, forced) = at();
    GROW_IN_LIMIT.store(0, Ordering::Relaxed);
    let every = run(&program, lease(grant.with_spill_every(1)))[0][0];
    assert_eq!(GROW_IN_LIMIT.load(Ordering::Relaxed), 0, "every ask refused: never grew");
    let (r1, f1) = (GROW_IN_REFUSED.load(Ordering::Relaxed) - refused, FORCED.load(Ordering::Relaxed) - forced);
    assert!(r1 > off_refused && f1 == r1, "every refusal forced and spilled: {f1} forced, {r1} refused, {off_refused} off");
    assert!(every < off, "kept its share's rows only: {every} vs {off}");
    // every second ask: refusals between grants, so it grows and spills too; a trim keeps the fault
    let (refused, forced) = at();
    GROW_IN_LIMIT.store(0, Ordering::Relaxed);
    let second = run(&program, lease(grant.with_spill_every(2).trimmed(SHARE_FLOOR as u64)))[0][0];
    assert!(GROW_IN_LIMIT.load(Ordering::Relaxed) > SHARE_FLOOR as u64, "the grants between refusals grew it");
    let (r2, f2) = (GROW_IN_REFUSED.load(Ordering::Relaxed) - refused, FORCED.load(Ordering::Relaxed) - forced);
    assert!(f2 > 0 && r2 >= f2, "forced refusals spilled: {f2} forced, {r2} refused");
    assert!(second > every, "kept more than at every ask: {second} vs {every}");
}
