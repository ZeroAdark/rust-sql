//! Ladder C1 (`PGRUST_MEMPOLICY=cache_floor=1`, with the ladder on; CACHE-IMPLEMENTATION-PLAN section C1's witness): a
//! cache holding 2 GiB above its protected floor and a statement asking 1 GiB. Trunk's pool, armed beside every cache at
//! its cap (D4345: 1152 MiB of an 8 GiB limit), cannot seat the ask beside another holder: the statement waits and takes
//! less. Under C1 the pool arms with the caches at their floors, seats the whole GiB, and the ladder evicts the cache
//! inline on the asker's thread (the room under its line kept at LOW) before the bytes land. The inline eviction's deadline
//! (`cache_inline_us`) is read between caches: one slow trim ends it, short, and the next cache is left alone; C1 takes the
//! cheap caches first (C0's cost order: the condition cache before the part cache). A part cache whose bytes are in use
//! past its protected floor: the ladder's sample counts them as held, so the pool seats only what fits beside them; off,
//! or a pool armed as today (an explicit part cache), nothing counted.
//! C1b: the ladder's take sets the part cache's ladder budget to what it holds after. C1d: the bytes part lists hold
//! count as pinned: the take stops there and the budget never goes under them.
//! The C1 try (`inline_on_draw`): the take moves from a grant's reservation to the statement's maps.
//! One test: it arms the process-wide pool, registers the process's part cache and sets the process's knobs.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Duration;

use gravity_driver::mem::budget::{self, Consumer, Hooks};
use gravity_driver::{Grant, POOL};
use mempolicy::consts::{set, K};

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

/// The fake part cache: what it holds, never under its bytes in use (`PINNED`) and (C1d) the bytes part lists hold
/// (`WARM`: its trim frees none of them; only the evicted hook would, all at once).
static HELD: AtomicU64 = AtomicU64::new(0);
static PINNED: AtomicU64 = AtomicU64::new(0);
static WARM: AtomicU64 = AtomicU64::new(0);

fn held() -> u64 {
    HELD.load(Relaxed)
}

fn in_use() -> u64 {
    PINNED.load(Relaxed)
}

fn none_in_use() -> u64 {
    0
}

fn cap() -> u64 {
    4 * GIB
}

/// What a trim could free now (`PartRegistry::trimmable_bytes`): neither the bytes in use nor the list-held ones.
fn trimmable() -> u64 {
    held().saturating_sub(in_use() + WARM.load(Relaxed))
}

fn trim_to(to: u64) -> u64 {
    let to = to.max(in_use() + WARM.load(Relaxed));
    let cur = held();
    if to >= cur {
        return 0;
    }
    HELD.store(to, Relaxed);
    cur - to
}

/// A second cache (the condition cache: before the part cache in C1's order, after it in `ORDER`), no floor of its own.
static COND: AtomicU64 = AtomicU64::new(0);

fn cond() -> u64 {
    COND.load(Relaxed)
}

/// The condition cache's trim takes 3 ms (past `cache_inline_us`) on the inline eviction's clock while this is set.
static SLOW: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static CLOCK_NS: AtomicU64 = AtomicU64::new(0);

fn cond_trim(to: u64) -> u64 {
    if SLOW.load(Relaxed) {
        gravity_driver::mem::reclaim::set_inline_clock_for_test(Some(CLOCK_NS.fetch_add(3_000_000, Relaxed) + 3_000_000));
    }
    let cur = cond();
    if to >= cur {
        return 0;
    }
    COND.store(to, Relaxed);
    cur - to
}

/// One arm: the pool armed as exec's `arm_grant_pool` arms it at an 8 GiB limit (`derived_pool_in_b`, `floors` = C1),
/// 256 MiB held by another statement, the ladder's last sample 512 MiB under its line, the part cache holding `part`,
/// then a 1 GiB admission. Returns (the bytes it reserved, 0 = refused; the bytes the part cache lost).
fn arm(floors: bool, part: u64) -> (u64, u64) {
    let limit = 8 * GIB;
    let b = mempolicy::sizes::budget_of(limit, 0, 0, 0, 0);
    let today = mempolicy::sizes::derived_pool_cap(limit, 128 * MIB);
    let cap = mempolicy::sizes::derived_pool_in_b(today, &b, 0, 128 * MIB, limit, false, 64 * MIB, floors);
    set(K::CacheFloor, u64::from(floors));
    gravity_driver::mem::reclaim::set_pool_floors(floors);
    POOL.arm(cap);
    let other = POOL.keep(256 * MIB);
    HELD.store(part, Relaxed);
    // the ladder's sample: on, its line 7 GiB, the read 512 MiB under it (LOW is 409.6 MiB)
    mcx::ladder::publish(mcx::ladder::ON, 7 * GIB);
    mcx::ladder::set_room(512 * MIB, limit);
    let before = held();
    let r = POOL.admit_need(Grant::of(GIB), &|g| g.bytes, Duration::from_millis(10));
    let reserved = r.as_ref().map_or(0, |r| r.reserved);
    let lost = before - held();
    drop(r);
    drop(other);
    mcx::ladder::publish(mcx::ladder::OFF, 0);
    (reserved, lost)
}

#[test]
fn c1_a_1gib_ask_beside_a_full_cache_is_served_after_an_inline_eviction() {
    gravity_driver::mem::reclaim::set_inline_clock_for_test(Some(0));
    assert!(budget::register(Consumer::PartCache, Hooks { current: held, floor: in_use, cap, reclaim: Some(trim_to) }));
    budget::register_trimmable(Consumer::PartCache, trimmable);
    // the protected floor: 25 % of B split by the caps; this cache alone has one: 256 MiB of a 1 GiB B
    budget::note_b(GIB);
    set(K::GateNow, 1);
    let (reserved, lost) = arm(false, 2 * GIB + 256 * MIB);
    assert!(reserved < GIB, "trunk: the pool beside every cache at its cap seats less than the GiB (reserved {} MiB)", reserved >> 20);
    assert_eq!(lost, 0, "trunk: no eviction");
    let c0 = mcx::ladder::c1_counts();
    let (reserved, lost) = arm(true, 2 * GIB + 256 * MIB);
    let c1 = mcx::ladder::c1_counts();
    assert!(reserved >= GIB, "C1: the pool with the caches at their floors seats the whole GiB (reserved {} MiB)", reserved >> 20);
    let (low, _) = mempolicy::ladder::marks(8 * GIB);
    assert_eq!(lost, GIB + low - 512 * MIB, "C1: the ladder evicted what keeps LOW free under its line after the GiB");
    assert!(held() >= 256 * MIB, "never under its protected floor");
    assert_eq!((c1.inline_runs - c0.inline_runs, c1.inline_bytes - c0.inline_bytes, c1.inline_short - c0.inline_short), (1, lost, 0), "one inline eviction, counted");
    assert!(c1.inline_ns_max < 2_000_000, "within cache_inline_us ({} ns)", c1.inline_ns_max);
    // C1b: the ladder sizes the part cache: its budget came down to what the eviction left it
    assert_eq!(mcx::ladder::sized_budgets()[mcx::ladder::SIZED_PART], held(), "C1b: the part cache's ladder budget is what it holds after the take");
    // C1's order takes the condition cache (cheap) before the part cache, and the deadline is read between caches: a
    // condition cache of 256 MiB whose trim takes 3 ms leaves the part cache alone, short
    assert!(budget::register(Consumer::CondCache, Hooks { current: cond, floor: none_in_use, cap, reclaim: Some(cond_trim) }));
    COND.store(256 * MIB, Relaxed);
    SLOW.store(true, Relaxed);
    let (reserved, lost) = arm(true, 768 * MIB);
    let c2 = mcx::ladder::c1_counts();
    assert!(reserved >= GIB, "served whatever the eviction freed");
    assert_eq!((lost, cond()), (0, 0), "the condition cache first, then the deadline: the part cache untouched");
    assert_eq!((c2.inline_runs - c1.inline_runs, c2.inline_short - c1.inline_short), (1, 1), "one eviction, short");
    assert!(c2.inline_ns_max >= 3_000_000, "the one trim ran past the deadline ({} ns)", c2.inline_ns_max);
    SLOW.store(false, Relaxed);
    // a part cache with 3.5 GiB in use (past its 256 MiB floor): the draw's eviction stops there (512 MiB of the 4 GiB);
    // the ladder's next sample counts the 3.25 GiB past the floor as held, so the next GiB is not seated beside them
    PINNED.store(3 * GIB + GIB / 2, Relaxed);
    let (_, lost) = arm(true, 4 * GIB);
    assert_eq!(lost, 512 * MIB, "the trim stops at the bytes in use");
    mcx::ladder::publish(mcx::ladder::ON, 7 * GIB);
    mcx::ladder::refresh_stuck();
    let stuck = mcx::ladder::caches_stuck();
    assert_eq!(stuck, 3 * GIB + GIB / 2 - 256 * MIB, "the bytes in use, less the cache's protected floor");
    let part = gravity_driver::mem::reclaim::cache_floors_view().into_iter().find(|v| v.0 == Consumer::PartCache).expect("part cache");
    assert_eq!((part.2, part.3), (3 * GIB + GIB / 2, 3 * GIB + GIB / 2), "the view: held, and pinned");
    // a pool armed as today (an explicit part cache: its setting is its floor): nothing counted
    gravity_driver::mem::reclaim::set_pool_floors(false);
    mcx::ladder::refresh_stuck();
    assert_eq!(mcx::ladder::caches_stuck(), 0, "a pool armed as today counts every cache at its cap already");
    gravity_driver::mem::reclaim::set_pool_floors(true);
    mcx::ladder::refresh_stuck();
    mcx::ladder::publish(mcx::ladder::OFF, 0);
    let (reserved, lost) = arm(true, 3 * GIB + GIB / 2);
    let cap = POOL.capacity();
    assert_eq!(lost, 0, "nothing more to evict");
    assert!(reserved < GIB, "C1: the GiB is not seated beside the bytes in use (reserved {} MiB)", reserved >> 20);
    assert!(reserved + 256 * MIB + stuck <= cap, "C1: what it seats fits beside the other holder and the bytes in use");
    let (base, lost) = arm(false, 3 * GIB + GIB / 2);
    assert!(base < GIB && lost == 0, "off: today's pool, no eviction (reserved {} MiB)", base >> 20);
    PINNED.store(0, Relaxed);
    // C1d: a part cache of 2.5 GiB whose part lists hold 2 GiB of it: the ladder's take stops at those bytes (it never
    // reaches the evicted hook), and the cache's ladder budget stays at that floor
    // (the ladder's sample first, so the stuck bytes the pool counts are this cache's, none, not the step above's)
    HELD.store(2 * GIB + GIB / 2, Relaxed);
    mcx::ladder::publish(mcx::ladder::ON, 7 * GIB);
    mcx::ladder::refresh_stuck();
    mcx::ladder::publish(mcx::ladder::OFF, 0);
    WARM.store(2 * GIB, Relaxed);
    // (the budget the step above's take left: under the 2 GiB this take leaves, and a take never raises it)
    let before = mcx::ladder::sized_budgets()[mcx::ladder::SIZED_PART];
    assert!(before < 2 * GIB, "the step above's take left the budget under 2 GiB ({} MiB)", before >> 20);
    let (_, lost) = arm(true, 2 * GIB + GIB / 2);
    assert_eq!((lost, held()), (GIB / 2, 2 * GIB), "C1d: the take stops at the list-held bytes");
    assert_eq!(mcx::ladder::sized_budgets()[mcx::ladder::SIZED_PART], before, "C1b: a take that leaves the cache past its budget never raises it");
    mcx::ladder::publish(mcx::ladder::ON, 7 * GIB);
    mcx::ladder::refresh_stuck();
    assert_eq!(mcx::ladder::sized_floors()[mcx::ladder::SIZED_PART], 2 * GIB, "C1d: the sample's floor: in use and list-held");
    assert_eq!(mcx::ladder::sized_budget(mcx::ladder::SIZED_PART, 4 * GIB), before, "C1d: the floor does not lift the budget");
    mcx::ladder::publish(mcx::ladder::OFF, 0);
    WARM.store(0, Relaxed);
    // the C1 try (`inline_on_draw` = 1): a grant's reservation evicts nothing; the bytes a statement maps do, what keeps
    // LOW free under the line after them (the room since the sample less the maps, the evictions counted back)
    assert_eq!(mcx::ladder::claim_low(), None, "inline_on_draw off: no pool take notes its room for the raises");
    set(K::InlineOnDraw, 1);
    let (reserved, lost) = arm(true, 2 * GIB + GIB / 4);
    assert!(reserved >= GIB, "inline_on_draw: the pool still seats the GiB (reserved {} MiB)", reserved >> 20);
    assert_eq!(lost, 0, "inline_on_draw: no eviction at the reservation");
    // and the room the admission left the pool bounds the raises: a raise never grows a cache into it
    let pool_low = mcx::ladder::claim_low().expect("inline_on_draw: the admission noted the pool's room");
    let parked = gravity_driver::REGIONS.parked_bytes() as u64;
    assert!(pool_low + GIB + 256 * MIB <= POOL.capacity() + parked, "the room left beside the GiB and the other holder ({} MiB)", pool_low >> 20);
    HELD.store(2 * GIB + GIB / 4, Relaxed);
    mcx::ladder::publish(mcx::ladder::ON, 7 * GIB);
    mcx::ladder::set_room(4 * GIB, 8 * GIB);
    gravity_driver::mem::reclaim::ladder_mapped(GIB);
    assert_eq!(held(), 2 * GIB + GIB / 4, "a 1 GiB map with 4 GiB of room: nothing evicted");
    mcx::ladder::set_room(512 * MIB, 8 * GIB);
    let c0 = mcx::ladder::c1_counts();
    gravity_driver::mem::reclaim::ladder_mapped(GIB);
    let (low, _) = mempolicy::ladder::marks(8 * GIB);
    assert_eq!(2 * GIB + GIB / 4 - held(), GIB + low - 512 * MIB, "a 1 GiB map with 512 MiB of room: what keeps LOW free after it");
    assert_eq!(mcx::ladder::c1_counts().inline_runs - c0.inline_runs, 1, "one inline eviction, on the mapping thread");
    mcx::ladder::publish(mcx::ladder::OFF, 0);
    set(K::InlineOnDraw, 0);
    // D4788 (cache_inline_skip): a draw B has no room for with every cache at its protected floor: make_room's C1 branch
    // skips the R2a walk (a skip, not a run); with the knob off the walk runs and frees nothing
    use gravity_driver::mem::reclaim::{counters, make_room};
    budget::configure(budget::Cfg { mode: budget::Mode::On, ..budget::Cfg::DEFAULT });
    mcx::governor::publish_limit(8 << 30);
    gravity_driver::REGIONS.trim();
    let _ = budget::snapshot();
    let part = gravity_driver::mem::reclaim::cache_floors_view().into_iter().find(|v| v.0 == Consumer::PartCache).expect("part cache");
    (HELD.store(part.1, Relaxed), COND.store(0, Relaxed));
    mcx::ladder::publish(mcx::ladder::ON, 7 * GIB);
    set(K::CacheInlineSkip, 1);
    let r0 = counters();
    assert!(!make_room(1 << 40), "1 TiB: B has no room for it");
    let r1 = counters();
    assert_eq!((r1.inline_runs - r0.inline_runs, r1.inline_skips - r0.inline_skips), (0, 1), "the walk could take nothing: skipped");
    set(K::CacheInlineSkip, 0);
    assert!(!make_room(1 << 40));
    let r2 = counters();
    assert_eq!((r2.inline_runs - r1.inline_runs, r2.inline_skips - r1.inline_skips, r2.inline_bytes - r1.inline_bytes), (1, 0, 0), "off: the walk runs, frees nothing");
    assert_eq!(held(), part.1, "at its floor either way");
    mcx::ladder::publish(mcx::ladder::OFF, 0);
    mcx::governor::publish_limit(0);
    budget::configure(budget::Cfg::DEFAULT);
    set(K::CacheFloor, 0);
    set(K::GateNow, 0);
    gravity_driver::mem::reclaim::set_inline_clock_for_test(None);
}
