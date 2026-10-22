//! pppress card 13 (3): with PGRUST_POOL_GRAVITY=1 a region's account is the page pool's in-use count for it, by
//! construction. `pool_alloc` charges the slice the pool hands out (the ask's bucket, its runs, a span's commit), not the
//! ask; a reservation (`pool_reserved`) opens its charge at the span's commit and gives what the pool takes dead; and every
//! later move of a pool region's pages (a fit, a park and an unpark, a commit, the give-backs, an uncommit) moves the
//! charge by what the pool's count moved (whole 64 KiB runs), never by the range's length. So at every step REGION's
//! in-use bytes (pagepool Tag(2)) moved exactly as the owners' pool charges did (`memledger::pagepool_total`): the
//! view's `pool_owner_rounding` has no REGION term. Its own binary: it sets the switch before the pool's first read; the
//! pool's counters are process-wide, so its tests run one at a time.
#![cfg(target_os = "linux")]

use gravity_driver_rig as common;

use common::*;
use gravity_driver::mem::region::{alloc_witness, charge_witness, region_move_witness, reservation_witness, RegionMove};
use gravity_driver::{POOL, REGIONS};

const KIB: usize = 1 << 10;
const MIB: usize = 1 << 20;
const RUN: usize = 64 << 10;

/// One test at a time (the pool's counters are process-wide).
static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The switch on and the pool armed, once, before any test reads them.
fn on() -> std::sync::MutexGuard<'static, ()> {
    let one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    static SET: std::sync::Once = std::sync::Once::new();
    SET.call_once(|| {
        // SAFETY: under the Once, before the pool reads its switches (once, `arm_pool`); libtest reads no variable after
        unsafe { std::env::set_var("PGRUST_POOL_GRAVITY", "1") };
        arm_pool();
    });
    assert!(pool_armed(), "the pool serves the regions");
    one
}

/// REGION's in-use bytes and the owners' pool charges, read together: this thread's pending posts folded first (a charge
/// on the thread's own account waits there until `DOOR_FOLD_BYTES`).
fn read() -> (i64, i64) {
    memledger::fold();
    let in_use = pagepool::stats().in_use[pagepool::Tag::REGION.0 as usize] as i64;
    (in_use, memledger::pagepool_total())
}

/// The witness: since `base`, REGION's in-use bytes moved exactly as the pool charges did (`what` names the step).
#[track_caller]
fn exact(base: (i64, i64), what: &str) -> i64 {
    let (u, c) = read();
    assert_eq!(u - base.0, c - base.1, "{what}: REGION in use moved {} B, the pool charges {} B", u - base.0, c - base.1);
    u - base.0
}

/// `pool_alloc`: each region's charge is its slice, at least its ask, and REGION in use moved by it. Asks off every grain:
/// a bucket's, the runs', a span's (2 MiB with SPAN_FIT / FIT_TAIL off, a page with them), a lazy span's.
#[test]
fn an_allocation_charges_the_slice_the_pool_counts() {
    let _one = on();
    REGIONS.trim();
    let base = read();
    let mut held = Vec::new();
    let mut sum = 0u64;
    for ask in [5 * KIB + 1, 100 * KIB + 3, 300 * KIB + 5, MIB + 1, 3 * MIB + 4 * KIB + 7, 40 * MIB + 9] {
        let r = alloc_witness(ask).expect("a pool region");
        let c = charge_witness(&r);
        assert!(c >= ask as u64, "ask {ask}: its charge {c} holds at least the ask");
        sum += c;
        let moved = exact(base, &format!("ask {ask} taken"));
        assert_eq!(moved, sum as i64, "ask {ask}: REGION in use is the sum of the regions' charges");
        held.push(r);
    }
    assert!(sum > held.iter().map(|r| r.len() as u64).sum::<u64>(), "some ask was rounded up: the round is charged");
    drop(held);
    assert_eq!(exact(base, "all dropped"), 0, "back to the start");
}

/// An allocation's fit (a shrink past an eighth gives whole runs dead, a growth takes them back) and a park (`park_free`:
/// its pages dead) and unpark: the charge moves by the pool's count at each, the ask's length never read.
#[test]
fn an_allocations_fit_and_park_move_its_charge_by_the_pools_count() {
    let _one = on();
    REGIONS.trim();
    POOL.arm(1024 * MIB as u64);
    let base = read();
    let mut r = alloc_witness(8 * MIB).expect("a pool region");
    let c0 = charge_witness(&r);
    exact(base, "taken");
    // 8 MiB fitted to 3 MiB + 5 KiB (page-rounded): the tail is past an eighth, its whole runs go dead; the part-run past
    // the new length stays live and charged
    let (c1, done) = region_move_witness(&mut r, RegionMove::Fit(3 * MIB + 5 * KIB));
    assert!(done, "the fit is taken");
    assert!(c1 < c0 && c1 >= (3 * MIB + 5 * KIB) as u64, "the charge gave the dead runs: {c0} -> {c1}");
    assert_eq!(c1 % RUN as u64, c0 % RUN as u64, "the charge moved by whole runs: {c0} -> {c1}");
    exact(base, "fitted down");
    let (c2, done) = region_move_witness(&mut r, RegionMove::Fit(6 * MIB));
    assert!(done, "the growth is taken");
    assert!(c2 > c1, "the charge took back the revived runs: {c1} -> {c2}");
    exact(base, "fitted up");
    let (c3, _) = region_move_witness(&mut r, RegionMove::ParkFree);
    assert!(c3 < c2, "a park's dead pages left the charge: {c2} -> {c3}");
    exact(base, "parked dead");
    let (c4, _) = region_move_witness(&mut r, RegionMove::Unpark);
    assert_eq!(c4, c2, "an unpark takes them back");
    exact(base, "unparked");
    drop(r);
    assert_eq!(exact(base, "dropped"), 0, "back to the start");
}

/// A reservation (`pool_reserved`): a pool span and a warm lazy span, their ask off the run grain; then a commit, the
/// give-backs and an uncommit. The charge opens at the span's commit less what goes dead, and follows the pool after.
#[test]
fn a_reservations_charge_follows_the_pools_count() {
    let _one = on();
    REGIONS.trim();
    let base = read();
    let ask = RUN + 12 * KIB;
    let (mut r, rw) = reservation_witness(ask, 8 * MIB).expect("a pool reservation");
    assert!(rw >= ask, "its ask's pages writable");
    let c0 = charge_witness(&r);
    assert!(c0 >= rw as u64 && c0 <= rw.next_multiple_of(RUN) as u64, "the ask's pages and their part-run: {c0} (rw {rw})");
    exact(base, "reserved");
    let (c1, done) = region_move_witness(&mut r, RegionMove::Commit(3 * MIB));
    assert!(done, "a commit inside the reservation");
    assert!(c1 >= c0 + 3 * MIB as u64 - RUN as u64, "the commit is charged: {c0} -> {c1}");
    exact(base, "committed");
    let (c2, _) = region_move_witness(&mut r, RegionMove::GiveBackCommitted);
    assert!(c2 < c1, "the given-back commit left the charge: {c1} -> {c2}");
    exact(base, "commit given back");
    let (_, done) = region_move_witness(&mut r, RegionMove::Commit(2 * MIB + 3 * KIB));
    assert!(done, "a second commit");
    exact(base, "committed again");
    region_move_witness(&mut r, RegionMove::Uncommit);
    exact(base, "uncommitted");
    region_move_witness(&mut r, RegionMove::GiveBackPast(RUN));
    exact(base, "given back past a run");
    drop(r);
    assert_eq!(exact(base, "dropped"), 0, "back to the start");

    // a lazy span (512 MiB): its first owner commits 256 MiB + 20 KiB, the next asks a part-run of a span of that size
    // (warm: its last commit is the pool's count until the dead runs leave it)
    let (first, _) = reservation_witness(256 * MIB + 20 * KIB, 512 * MIB).expect("a lazy pool reservation");
    exact(base, "lazy reserved");
    drop(first);
    let (mut next, _) = reservation_witness(RUN + 4 * KIB, 512 * MIB).expect("a lazy pool reservation");
    exact(base, "warm lazy reserved");
    region_move_witness(&mut next, RegionMove::Commit(5 * MIB));
    exact(base, "warm lazy committed");
    region_move_witness(&mut next, RegionMove::Uncommit);
    exact(base, "warm lazy uncommitted");
    drop(next);
    assert_eq!(exact(base, "lazy dropped"), 0, "back to the start");
}
