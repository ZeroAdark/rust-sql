//! D4829 (`PGRUST_MEMPOLICY=cache_floor_hold`, with the ladder on and `cache_floor = 1`): a part cache whose trim
//! overshoots its target (it frees whole parts: asked down to its 256 MiB protected floor, it lands at 64 MiB). On, the
//! ladder's take leaves its budget at the floor, and a budget that fell under it comes back up to it at the next pass
//! with no drain to run. Off (the S1 c6 ratchet), the budget is the 64 MiB the trim left and nothing brings it back.
//! Born red with the switch off. One test: it registers the process's part cache and sets the process's knobs.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use gravity_driver::mem::budget::{self, Consumer, Hooks};
use mcx::ladder::{sized_budgets, SIZED_PART};
use mempolicy::consts::{set, K};

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

/// The fake part cache: what it holds; none of it in use.
static HELD: AtomicU64 = AtomicU64::new(0);

fn held() -> u64 {
    HELD.load(Relaxed)
}

fn in_use() -> u64 {
    0
}

fn cap() -> u64 {
    4 * GIB
}

fn trimmable() -> u64 {
    held()
}

/// A trim that frees whole parts: asked down to `to`, it lands at a quarter of it.
fn trim_over(to: u64) -> u64 {
    let cur = held();
    if to >= cur {
        return 0;
    }
    HELD.store(to / 4, Relaxed);
    cur - to / 4
}

#[test]
fn d4829_a_take_leaves_the_budget_at_the_floor_and_a_pass_restores_one_under_it() {
    assert!(budget::register(Consumer::PartCache, Hooks { current: held, floor: in_use, cap, reclaim: Some(trim_over) }));
    budget::register_trimmable(Consumer::PartCache, trimmable);
    // the protected floor: 25 % of B split by the caps; this cache alone has one: 256 MiB of a 1 GiB B
    budget::note_b(GIB);
    let floor = 256 * MIB;
    set(K::CacheFloor, 1);
    let limit = 8 * GIB;
    let line = 7 * GIB;
    mcx::ladder::publish(mcx::ladder::ON, line);
    assert_eq!(mcx::ladder::sized_budget(SIZED_PART, 4 * GIB), 4 * GIB, "nothing taken: the start");
    let none = |_: u64| 0u64;
    let (_, high) = mempolicy::ladder::marks(limit);
    let between = line - high - MIB;

    // on (the default): asked for everything, the take stops at the floor, the trim lands under it, the budget does not
    HELD.store(2 * GIB, Relaxed);
    let freed = gravity_driver::mem::reclaim::ladder_reclaim(2 * GIB);
    assert_eq!((freed, held()), (2 * GIB - floor / 4, floor / 4), "the trim overshot its floor target");
    assert_eq!(sized_budgets()[SIZED_PART], floor, "on: the take leaves the budget at the floor, not at what the trim left");

    // off: the overshoot is the budget (the ratchet), and no pass brings it back
    set(K::CacheFloorHold, 0);
    HELD.store(2 * GIB, Relaxed);
    let _ = gravity_driver::mem::reclaim::ladder_reclaim(2 * GIB);
    assert_eq!(held(), floor / 4);
    assert_eq!(sized_budgets()[SIZED_PART], floor / 4, "off: the budget is what the trim left");
    mcx::ladder::background_pass_with(&none, &|| u64::MAX, limit, line, between, false, None);
    assert_eq!(sized_budgets()[SIZED_PART], floor / 4, "off: no raise (no demand, no room past TOP), no restore");

    // on again: the next pass with no drain to run brings the budget back to the floor
    set(K::CacheFloorHold, 1);
    mcx::ladder::background_pass_with(&none, &|| u64::MAX, limit, line, between, false, None);
    assert_eq!(sized_budgets()[SIZED_PART], floor, "on: back to the floor, ungated");
    assert_eq!(mcx::ladder::sized_restores()[SIZED_PART], (1, floor - floor / 4), "one restore, counted");

    mcx::ladder::publish(mcx::ladder::OFF, 0);
    set(K::CacheFloor, 0);
}
