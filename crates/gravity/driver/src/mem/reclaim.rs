//! Memory step 5b: the reclaimer and the free reserve inside Gravity's one memory budget B (`mem::budget`).
//!
//! - **Background** (`tick`): the hosts' reclaim thread calls it every `RECLAIM_TICK_MS` while the reclaimer applies. It
//!   refreshes B's snapshot. Below the reserve's low mark (free_B < R_low) it frees managed bytes up to the high mark, off
//!   the query path.
//! - **Inline** (`make_room`): a grant pool draw that B has no free bytes for (`GrantPool::take_leaving`) runs the same walk
//!   first, then the burst road (a managed cache under its target, never under its floor), at most `RECLAIM_INLINE_MAX`
//!   bytes in all. A draw evicts inline only for a burst the reserve did not cover. A draw B still has no room for is
//!   refused and takes its own road (a spill, a wait), as a full pool's draw does; its bytes become the background's
//!   DEMAND, which the next tick frees on the burst road, so a waiting draw (an admission, `take_wait`) finds them there.
//! - **What is freed, cheapest refill first** (`reclaim`): the policy's rungs (`mempolicy::reclaim::RUNGS`): the parked
//!   regions, the parked worker-slot sets but the newest, each managed cache past its own setting, then each (`ORDER`)
//!   down to its target.
//!
//! Off, advise, or on without the `reclaimer` actuator: nothing here runs, every draw is today's, and no cache is managed;
//! only the memory watchdog's backstop (`reclaim_backstop`) reclaims, past its cancel threshold and before it cancels.

use core::sync::atomic::{AtomicU64, Ordering};

use pg_clock::MonoStamp;

use super::budget::{self, actuator, Hooks, Mode};

pub use mempolicy::sizes::RECLAIM_TICK_MS as TICK_MS;

// The caches' order and set are the memory policy's (`mempolicy::reclaim`).
pub use mempolicy::reclaim::ORDER;
use mempolicy::reclaim::{Rung, CACHES, RUNGS};
use mempolicy::relief::{Relief, Source};

static TICKS_BELOW_LOW: AtomicU64 = AtomicU64::new(0);
static BACKGROUND_BYTES: AtomicU64 = AtomicU64::new(0);
static INLINE_RUNS: AtomicU64 = AtomicU64::new(0);
static INLINE_BYTES: AtomicU64 = AtomicU64::new(0);
static INLINE_NS_MAX: AtomicU64 = AtomicU64::new(0);
static INLINE_SHORT: AtomicU64 = AtomicU64::new(0);
static INLINE_SKIPS: AtomicU64 = AtomicU64::new(0);
/// Inline reclaims' summed time; their settles that gave back with a purge, and those that kept the freed pages for the
/// draw instead (REUSE: B had room counting them); bytes purged (any thread's purge counted in mi_arena_purge) while an
/// inline reclaim ran.
static INLINE_NS_SUM: AtomicU64 = AtomicU64::new(0);
static INLINE_GIVE_BACKS: AtomicU64 = AtomicU64::new(0);
static INLINE_REUSE_KEPT: AtomicU64 = AtomicU64::new(0);
static INLINE_PURGED: AtomicU64 = AtomicU64::new(0);
/// The largest draw B had no room for since the last tick (0: none): the next background tick frees toward it.
static DEMAND: AtomicU64 = AtomicU64::new(0);
/// One inline reclaim at a time: a second draw short of room waits for the first one's, then re-reads the free bytes.
static INLINE: pgsync::Mutex<()> = pgsync::Mutex::new(());

/// The reclaimer's counters since the process started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    /// Background ticks that found free_B under the low mark.
    pub ticks_below_low: u64,
    /// Bytes the background ticks freed.
    pub background_bytes: u64,
    /// Draws that reclaimed inline, the bytes they freed, and the longest one in nanoseconds.
    pub inline_runs: u64,
    pub inline_bytes: u64,
    pub inline_ns_max: u64,
    /// Draws B had no room for after the inline reclaim (refused: their own road).
    pub inline_short: u64,
    /// D4788: C1 draws whose R2a walk could take nothing, skipped (not runs).
    pub inline_skips: u64,
    /// The inline reclaims' summed nanoseconds; their settles with a give-back and purge, and those that kept the freed
    /// pages for the draw (REUSE); bytes purged by any thread while an inline reclaim ran.
    pub inline_ns_sum: u64,
    pub inline_give_backs: u64,
    pub inline_reuse_kept: u64,
    pub inline_purged_bytes: u64,
}

pub fn counters() -> Counters {
    Counters {
        ticks_below_low: TICKS_BELOW_LOW.load(Ordering::Relaxed),
        background_bytes: BACKGROUND_BYTES.load(Ordering::Relaxed),
        inline_runs: INLINE_RUNS.load(Ordering::Relaxed),
        inline_bytes: INLINE_BYTES.load(Ordering::Relaxed),
        inline_ns_max: INLINE_NS_MAX.load(Ordering::Relaxed),
        inline_short: INLINE_SHORT.load(Ordering::Relaxed),
        inline_skips: INLINE_SKIPS.load(Ordering::Relaxed),
        inline_ns_sum: INLINE_NS_SUM.load(Ordering::Relaxed),
        inline_give_backs: INLINE_GIVE_BACKS.load(Ordering::Relaxed),
        inline_reuse_kept: INLINE_REUSE_KEPT.load(Ordering::Relaxed),
        inline_purged_bytes: INLINE_PURGED.load(Ordering::Relaxed),
    }
}

/// The counters as the REPORT census prints them.
pub fn line() -> String {
    let c = counters();
    format!( // alloc: Cold
        "reclaimer {} below_low {} background {} MiB inline {} runs {} MiB max {} us short {}",
        if applies() { "on" } else { "off" },
        c.ticks_below_low,
        c.background_bytes >> 20,
        c.inline_runs,
        c.inline_bytes >> 20,
        c.inline_ns_max / 1000,
        c.inline_short,
    )
}

/// Whether the reclaimer applies now: the broker on with the `reclaimer` actuator.
pub fn applies() -> bool {
    budget::mode() == Mode::On && budget::actuators() & actuator::RECLAIMER != 0
}

/// The `caches` actuator's caches declared managed while the reclaimer applies (the grant pool's capacity then counts them
/// at their floor, and the reclaimer frees them past it), and undeclared otherwise: a cache no reclaimer frees counts at
/// its cap.
pub fn sync_managed() {
    let on = applies();
    for c in CACHES {
        budget::declare_managed(c, on && budget::enforcing(c));
    }
}

/// One managed cache as the walk sees it: its hooks and the broker's target for it.
#[derive(Clone, Copy)]
struct Managed {
    hooks: Hooks,
    target: Option<u64>,
}

/// The walk over the managed caches (`reclaim`'s step 3, a pure function of its inputs): each one in `caches`' order
/// freed down to max(its target, its floor), or with `burst` down to its floor, until `want` bytes are freed. A cache with
/// no target yet keeps what it holds unless `burst`. The bytes freed.
#[cfg(test)]
fn walk(want: u64, burst: bool, caches: &[Option<Managed>]) -> u64 {
    walk_noted(want, burst, caches, &mut [0; 5])
}

/// `walk`, the bytes each cache freed added into `noted` (by its index in `caches`; D4690's make_room line).
fn walk_noted(want: u64, burst: bool, caches: &[Option<Managed>], noted: &mut [u64; 5]) -> u64 {
    let mut freed = 0u64;
    for (i, m) in caches.iter().enumerate() {
        let Some(m) = m else { continue };
        if freed >= want {
            break;
        }
        let Some(trim) = m.hooks.reclaim else { continue };
        let Some((cur, floor)) = unless_skipped(|| ((m.hooks.current)(), (m.hooks.floor)())) else { continue };
        if let Some(goal) = mempolicy::reclaim::cache_goal(cur, floor, m.target, burst, want - freed) {
            let got = trim(goal);
            if let Some(n) = noted.get_mut(i) {
                *n = n.saturating_add(got);
            }
            freed = freed.saturating_add(got);
        }
    }
    freed
}

/// The cache rung's first pass: each cache in `caches`' order freed down to its own setting (`over_goal`) until `want`
/// bytes are freed, before `walk` cuts any under it. The bytes freed, each cache's added into `noted` (as `walk_noted`).
fn walk_over(want: u64, caches: &[Option<Managed>], noted: &mut [u64; 5]) -> u64 {
    let mut freed = 0u64;
    for (i, m) in caches.iter().enumerate() {
        let Some(m) = m else { continue };
        if freed >= want {
            break;
        }
        let Some(trim) = m.hooks.reclaim else { continue };
        let Some(goal) = unless_skipped(|| mempolicy::reclaim::over_goal((m.hooks.current)(), (m.hooks.floor)(), (m.hooks.cap)(), want - freed)) else { continue };
        if let Some(goal) = goal {
            let got = trim(goal);
            if let Some(n) = noted.get_mut(i) {
                *n = n.saturating_add(got);
            }
            freed = freed.saturating_add(got);
        }
    }
    freed
}

/// Frees up to `want` bytes inside B, cheapest refill first (the module's list); `burst`: a managed cache may go under its
/// target, down to its floor. The bytes freed.
pub fn reclaim(want: u64, burst: bool) -> u64 {
    reclaim_in(want, burst, false)
}

/// `reclaim`, with `backstop` (the memory watchdog's, D3992) over every registered cache, managed or not.
fn reclaim_in(want: u64, burst: bool, backstop: bool) -> u64 {
    reclaim_noted(want, burst, backstop, &mut Noted::default())
}

/// D4690: what one reclaim freed, by rung (`RUNGS`' order) and by cache (`ORDER`'s), for the make_room line.
#[derive(Clone, Copy, Default)]
struct Noted {
    rungs: [u64; 3],
    caches: [u64; 5],
}

/// `reclaim_in`, each rung's and each cache's freed bytes added into `noted`.
fn reclaim_noted(want: u64, burst: bool, backstop: bool, noted: &mut Noted) -> u64 {
    if want == 0 {
        return 0;
    }
    let (mut freed, mut unmapped) = (0u64, 0u64);
    for (r, rung) in RUNGS.into_iter().enumerate() {
        if freed >= want {
            break;
        }
        let left = want - freed;
        let got = match rung {
            Rung::Regions => match crate::park::REGIONS.parked_bytes() as u64 {
                0 => 0,
                parked => crate::park::REGIONS.trim_to(parked.saturating_sub(left) as usize) as u64,
            },
            Rung::Slots => crate::slots::DEPOT.trim_but_newest(left as usize) as u64,
            Rung::Caches => {
                reap(&mut unmapped);
                let order = mempolicy::reclaim::by_loss(ORDER, budget::loss);
                let caches = order.map(|c| budget::hooks(c).filter(|_| mempolicy::reclaim::cache_in_reach(budget::managed(c), backstop)).map(|hooks| Managed { hooks, target: budget::target(c) }));
                let mut by = [0u64; 5];
                let over = walk_over(left, &caches, &mut by);
                let got = over.saturating_add(walk_noted(left.saturating_sub(over), burst, &caches, &mut by));
                for (i, c) in order.iter().enumerate() {
                    if let Some(at) = ORDER.iter().position(|o| o == c) {
                        noted.caches[at] = noted.caches[at].saturating_add(by[i]);
                    }
                }
                got
            }
        };
        if rung != Rung::Caches {
            // pagepool P3 (C.11): a reclaim is pressure: the depots' pool bytes go back to the OS too
            crate::mem::region::pool_release(got);
            unmapped += got;
        }
        noted.rungs[r] = noted.rungs[r].saturating_add(got);
        freed = freed.saturating_add(got);
    }
    reap(&mut unmapped);
    freed
}

/// After the regions' and slots' rungs freed some: the reaper's unmaps done, so the pool and the next resident sample see them.
fn reap(unmapped: &mut u64) {
    if core::mem::take(unmapped) != 0 {
        crate::mem::reap::drain();
    }
}

/// C3: the bytes the fast rungs could free now (`reclaim`'s burst road: parked regions and slot sets, managed caches
/// above their floors); the memory watchdog reads it every 100 ms under the projection rule.
pub fn fast_bytes() -> u64 {
    let parked = crate::park::REGIONS.parked_bytes() as u64;
    let sets = crate::slots::DEPOT.trimmable_bytes() as u64;
    let caches = ORDER
        .iter()
        .filter(|c| budget::managed(**c))
        .filter_map(|c| budget::hooks(*c))
        .fold(0u64, |a, h| a.saturating_add((h.current)().saturating_sub((h.floor)())));
    parked.saturating_add(sets).saturating_add(caches)
}

/// C3: the fast rungs for `want` bytes on the memory watchdog's thread (the burst road, then retained pages): bytes freed.
pub fn reclaim_fast(want: u64) -> u64 {
    settle(reclaim(want, true))
}

/// D3992: what the memory watchdog's backstop could free now: the parked regions and slot sets, and every cache with a
/// reclaim above its floor, whatever the broker's mode.
pub fn backstop_bytes() -> u64 {
    let parked = crate::park::REGIONS.parked_bytes() as u64;
    let sets = crate::slots::DEPOT.trimmable_bytes() as u64;
    let caches = ORDER
        .iter()
        .filter_map(|c| budget::hooks(*c))
        .filter(|h| h.reclaim.is_some())
        .fold(0u64, |a, h| a.saturating_add((h.current)().saturating_sub((h.floor)())));
    parked.saturating_add(sets).saturating_add(caches)
}

/// D4353: the memory ladder's R2a count: `backstop_bytes`' rungs, each cache's bytes capped at what its reclaim can free
/// now (`mempolicy::ladder::elastic_of`). The backstop keeps its own count: its cancel's timing reads it.
/// D4353 (C), section 0b: above each cache's protected floor only.
pub fn ladder_elastic() -> u64 {
    // P3 (`parked_trim_rung`): the parked regions and slot sets are R2p's bytes (`grant::parked_bytes`, the regions by
    // their account bytes), not R2a's: R2a counts none of them, so the one number the climb reads for them is R2p's
    let (parked, sets) = if mcx::ladder::parked_rung_on() {
        (0, 0)
    } else {
        (crate::park::REGIONS.parked_bytes() as u64, crate::slots::DEPOT.trimmable_bytes() as u64)
    };
    let floors = protected_floors();
    let caches = ORDER
        .iter()
        .zip(floors)
        .filter_map(|(&c, p)| budget::hooks(c).filter(|h| h.reclaim.is_some()).and_then(|h| unless_skipped(|| mempolicy::ladder::elastic_of((h.current)(), mempolicy::ladder::r2a_floor((h.floor)(), p), budget::trimmable(c)))))
        .fold(0u64, u64::saturating_add);
    parked.saturating_add(sets).saturating_add(caches).saturating_add(tail_elastic())
}

/// D4298 (T4): the tail holders' bytes above their bytes in use (`mempolicy::ladder::TAIL`, each with a ladder-only reclaim).
fn tail_elastic() -> u64 {
    mempolicy::ladder::TAIL
        .iter()
        .filter_map(|&c| Some((budget::hooks(c)?.current, budget::ladder_reclaim_of(c)?.0)))
        .filter_map(|(current, in_use)| unless_skipped(|| mempolicy::ladder::elastic_of(current(), in_use(), None)))
        .fold(0u64, u64::saturating_add)
}

/// D4398: one cache's hook reads `f` on the memory ladder's thread: `None` when a lock one of them takes was held by
/// another thread (`memledger::ladder_pass::lock_or_skip`), so the walk leaves that cache out of this pass; off the ladder's
/// pass never `None`.
fn unless_skipped<T>(f: impl FnOnce() -> T) -> Option<T> {
    let _ = memledger::ladder_pass::take_skipped();
    let v = f();
    (!memledger::ladder_pass::take_skipped()).then_some(v)
}

/// D4353 (C), LADDER-DESIGN section 0b: each cache's protected floor in `ORDER`'s order: `mempolicy::ladder::PROTECTED_PCT`
/// of B split by the caches' caps, taken once per B so the floors do not move with the caches' own targets.
fn protected_floors() -> [u64; ORDER.len()] {
    static FLOORS: pgsync::Mutex<(u64, [u64; ORDER.len()])> = pgsync::Mutex::new((0, [0; ORDER.len()]));
    let b = budget::b_at();
    // D4398: in the ladder thread's pass, with the floors or a cap's lock held elsewhere, every cache counts as protected
    // this pass (R2a frees none of it) and nothing is stored
    let Some(mut g) = memledger::ladder_pass::lock_or_skip(&FLOORS) else { return [u64::MAX; ORDER.len()] };
    if g.0 != b {
        let Some(caps) = unless_skipped(|| ORDER.map(|c| budget::hooks(c).filter(|h| h.reclaim.is_some()).map_or(0, |h| (h.cap)()))) else {
            return [u64::MAX; ORDER.len()];
        };
        let mut out = [0; ORDER.len()];
        mempolicy::ladder::split_floors(mempolicy::ladder::protected_of(b), &caps, &mut out);
        *g = (b, out);
    }
    g.1
}

/// Ladder C1's view (the ladder views' `cachefloor` rows): per cache in `ORDER`'s order, its protected floor, the bytes
/// it holds and the bytes its trim cannot drop now (`unreachable`: in use or untrimmable, pinned); a cache with no
/// reclaim hook reads 0s.
pub fn cache_floors_view() -> [(budget::Consumer, u64, u64, u64); ORDER.len()] {
    let floors = protected_floors();
    core::array::from_fn(|i| {
        let c = ORDER[i];
        match budget::hooks(c).filter(|h| h.reclaim.is_some()) {
            Some(h) => {
                let (cur, in_use) = ((h.current)(), (h.floor)());
                (c, floors[i], cur, unreachable(i, cur, in_use))
            }
            None => (c, 0, 0, 0),
        }
    })
}

/// Ladder C1b: the caches the ladder sizes (`mcx::ladder::SIZED_*`).
fn sized_of(c: budget::Consumer) -> Option<usize> {
    match c {
        budget::Consumer::PartCache => Some(mcx::ladder::SIZED_PART),
        budget::Consumer::FpTables => Some(mcx::ladder::SIZED_FP),
        budget::Consumer::CondCache => Some(mcx::ladder::SIZED_COND),
        _ => None,
    }
}

/// Ladder C1: whether the grant pool was armed with the caches at their protected floors (exec's `arm_grant_pool`:
/// `cache_floor` with the ladder on, B known, no explicit part cache). Only then are the bytes past those floors that the
/// ladder cannot free counted as held (`c1_caches_stuck`): a pool armed as today counts every cache at its cap.
static POOL_FLOORS: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub fn set_pool_floors(on: bool) {
    POOL_FLOORS.store(on, Ordering::Relaxed);
}

/// Whether the grant pool was armed with the caches at their protected floors (`set_pool_floors`).
#[inline]
pub fn pool_floors() -> bool {
    POOL_FLOORS.load(Ordering::Relaxed)
}

/// Ladder C1: the bytes of cache `i` (in `ORDER`, holding `cur`, `in_use` in use: its hook's floor, an explicit part cache
/// setting included) its trim cannot drop now: the most of its bytes in use and its bytes past what its trim may drop
/// (`budget::trimmable`: the part cache's `trimmable_now`), never above `cur`.
fn unreachable(i: usize, cur: u64, in_use: u64) -> u64 {
    let untrimmable = budget::trimmable(ORDER[i]).map_or(0, |t| cur.saturating_sub(t));
    in_use.max(untrimmable).min(cur)
}

/// Ladder C1: the cache bytes past their protected floors that their trims cannot drop now (`unreachable` less the floor,
/// summed; 0 unless the pool was armed with the caches at those floors). The grant pool counts them as held
/// (`mcx::ladder::caches_stuck`, refreshed at the ladder's sample), so it never admits on bytes the ladder cannot free.
/// A cache another thread holds this pass counts none. C1d: the same sample sets each sized cache's floor (its
/// `unreachable` bytes: for the part cache, in use and the images Gravity's part lists hold, by `trimmable`'s honest
/// count, D4407), under which the ladder never sets its budget (`mcx::ladder::note_sized_floor`), pool armed so or not.
/// C-03 diag (D4923): the stuck samples whose cache's trimmable read skipped a lock, and the stuck bytes they gave.
static STUCK_SKIPPED: AtomicU64 = AtomicU64::new(0);
static STUCK_SKIPPED_BYTES: AtomicU64 = AtomicU64::new(0);

/// C-03 diag (the view): (samples, stuck bytes) counted from a skipped trimmable read.
pub fn stuck_skipped() -> (u64, u64) {
    (STUCK_SKIPPED.load(Ordering::Relaxed), STUCK_SKIPPED_BYTES.load(Ordering::Relaxed))
}

pub fn c1_caches_stuck() -> u64 {
    let pool = POOL_FLOORS.load(Ordering::Relaxed);
    let floors = protected_floors();
    let mut stuck = 0u64;
    for (i, &c) in ORDER.iter().enumerate() {
        let Some(h) = budget::hooks(c).filter(|h| h.reclaim.is_some()) else { continue };
        if let Some((cur, in_use)) = unless_skipped(|| ((h.current)(), (h.floor)())) {
            let pinned = unreachable(i, cur, in_use);
            // C-03 diag (D4923): `unreachable`'s trimmable read runs outside `unless_skipped`; a lock it skipped reads
            // no parts, so the whole cache counts as pinned. Count the samples and the stuck bytes they gave (the flag is
            // cleared unread by the next `unless_skipped` either way: reading it here changes nothing)
            if memledger::ladder_pass::take_skipped() {
                STUCK_SKIPPED.fetch_add(1, Ordering::Relaxed);
                STUCK_SKIPPED_BYTES.fetch_add(pinned.saturating_sub(floors[i]), Ordering::Relaxed);
            }
            if let Some(s) = sized_of(c) {
                mcx::ladder::note_sized_floor(s, pinned);
                // D4829: the protected floor a budget under it comes back up to (`mcx::ladder::restore_floors`; none
                // noted from a pass that skipped the floors)
                if floors[i] != u64::MAX {
                    mcx::ladder::note_sized_protect(s, floors[i]);
                }
            }
            stuck = stuck.saturating_add(pinned.saturating_sub(floors[i]));
        }
    }
    if pool { stuck } else { 0 }
}

/// D4353 (C): the memory ladder's R2a reclaim of `want` bytes: the parked regions and slot sets (parked working memory,
/// always elastic), then each cache down to its protected floor at most (`mempolicy::ladder::r2a_floor`), then (D4298) the
/// tail holders down to their bytes in use. The bytes freed.
pub fn ladder_reclaim(want: u64) -> u64 {
    ladder_reclaim_until(want, &|| false)
}

/// Ladder C1: `ladder_reclaim` that stops once `past` says its time is up, read between its steps (each cache, each tail
/// holder): one cache's trim may run past it. Its time from the start to its bytes given back (`settle`) is the
/// give-back rate's sample (`mcx::ladder::note_give_back`).
fn ladder_reclaim_until(want: u64, past: &dyn Fn() -> bool) -> u64 {
    if want == 0 {
        return 0;
    }
    let t0 = MonoStamp::now();
    let freed = ladder_reclaim_steps(want, past);
    mcx::ladder::note_give_back(freed, t0.elapsed_ns());
    freed
}

fn ladder_reclaim_steps(want: u64, past: &dyn Fn() -> bool) -> u64 {
    let (mut freed, mut unmapped) = (0u64, 0u64);
    // P3 (`parked_trim_rung`): the parked regions and slot sets are R2p's (`grant::trim_parked`), R2a takes the caches alone
    if !mcx::ladder::parked_rung_on() {
        crate::slots::note_released(0);
        crate::slots::drain_released();
        let parked = crate::park::REGIONS.parked_bytes() as u64;
        if parked != 0 {
            freed = crate::park::REGIONS.trim_to(parked.saturating_sub(want) as usize) as u64;
        }
        if freed < want {
            freed = freed.saturating_add(crate::slots::DEPOT.trim_but_newest((want - freed) as usize) as u64);
        }
        // pagepool P3 (C.11): the ladder's take is pressure: the depots' pool bytes go back to the OS too
        crate::mem::region::pool_release(freed);
        unmapped += freed;
        reap(&mut unmapped);
    }
    // ladder C1 (`cache_floor`, C0's cost order): the cheap caches first, the part cache last
    let floors = protected_floors();
    let order = if mcx::ladder::floors_on() { mempolicy::ladder::C1_ORDER } else { ORDER };
    for c in order {
        if freed >= want || past() {
            break;
        }
        let Some(i) = ORDER.iter().position(|&o| o == c) else { continue };
        let Some(h) = budget::hooks(c) else { continue };
        let Some(trim) = h.reclaim else { continue };
        // C1d: under `cache_floor` a cache keeps what its trim cannot drop: the part cache's images part lists hold
        // count as pinned, so the ladder's trim never reaches the evicted hook, which empties every list's warm cells
        // at once (the cancel-avoidance road, `reclaim_backstop`, still may)
        let c1 = mcx::ladder::floors_on();
        let Some((cur, keep)) = unless_skipped(|| {
            let (cur, in_use) = ((h.current)(), (h.floor)());
            let keep = mempolicy::ladder::r2a_floor(in_use, floors[i]);
            (cur, if c1 { keep.max(unreachable(i, cur, in_use)) } else { keep })
        }) else {
            continue;
        };
        if cur > keep {
            let took = trim(cur.saturating_sub(want - freed).max(keep));
            freed = freed.saturating_add(took);
            // ladder C1b: a sized cache's budget comes down to what the ladder left it; D4829 (`cache_floor_hold`): never
            // under what the take kept (a trim that overshot its target is no lower budget), unless past the cancel line
            if took != 0 && c1 {
                if let Some(s) = sized_of(c) {
                    mcx::ladder::note_take_bytes(s, took);
                    mcx::ladder::note_sized_protect(s, floors[i]);
                }
                if let (Some(s), Some(after)) = (sized_of(c), unless_skipped(|| (h.current)())) {
                    let hold = mcx::ladder::floor_hold_on() && !mcx::ladder::past_cancel_line();
                    mcx::ladder::note_taken(s, if hold { after.max(keep) } else { after });
                }
            }
        }
    }
    // D4298 (T4): then the tail holders, in TAIL's order, down to their bytes in use (no protected floor)
    for &c in &mempolicy::ladder::TAIL {
        if freed >= want || past() {
            break;
        }
        let (Some(h), Some((in_use, trim))) = (budget::hooks(c), budget::ladder_reclaim_of(c)) else { continue };
        let Some((cur, keep)) = unless_skipped(|| ((h.current)(), in_use())) else { continue };
        if cur > keep {
            freed = freed.saturating_add(trim(cur.saturating_sub(want - freed).max(keep)));
        }
    }
    settle(freed)
}

/// D4353 (C): the protected cache bytes R2b may evict now (section 0b): what R2a leaves above the bytes in use. R2b's
/// eviction is the backstop's reclaim (`reclaim_backstop`: every cache down to its bytes in use).
pub fn ladder_protected() -> u64 {
    ORDER
        .iter()
        .zip(protected_floors())
        .filter_map(|(&c, p)| budget::hooks(c).filter(|h| h.reclaim.is_some()).and_then(|h| unless_skipped(|| mempolicy::ladder::protected_left((h.current)(), (h.floor)(), p, budget::trimmable(c)))))
        .fold(0u64, u64::saturating_add)
}

/// D3992: the backstop's reclaim of `want` bytes before it cancels, on the memory watchdog's thread: the burst road over
/// every cache, then the allocator's retained pages. D3999: first every kept cursor lets its part go (D3268's release:
/// the parked slot sets now, each backend's park at its next statement), so the cache rung may evict those parts. The
/// bytes freed.
pub fn reclaim_backstop(want: u64) -> u64 {
    crate::slots::note_released(0);
    crate::slots::drain_released();
    settle(reclaim_in(want, true, true))
}

/// The background reclaimer's tick (the hosts' reclaim thread, every `TICK_MS` while it applies): the managed caches
/// declared, B's snapshot refreshed, under the low mark the managed bytes freed toward the high mark (never the burst road),
/// and a refused draw's DEMAND freed on the burst road. The bytes freed.
pub fn tick() -> u64 {
    sync_managed();
    if !applies() {
        DEMAND.store(0, Ordering::Relaxed);
        return 0;
    }
    let Some(s) = budget::snapshot() else { return 0 };
    let demand = DEMAND.load(Ordering::Relaxed);
    // no limit known (B 0): no budget to keep a reserve in
    let freed = if s.budget.b == 0 { 0 } else { tick_on(&s, demand) };
    // this tick's demand is served; a draw refused meanwhile keeps its own for the next tick
    let _ = DEMAND.compare_exchange(demand, 0, Ordering::Relaxed, Ordering::Relaxed);
    freed
}

/// `tick` over the snapshot `s` with the refused draws' `demand`. Under ladder C1 the ladder is the one evicter (the broker
/// keeps its advice): B under its low mark and a refused draw's demand are served by the ladder's R2a (the caches down to
/// their protected floors in C1's order, a sized cache's budget set by the take, the bytes noted as the ladder's: its room
/// and refill clock), not by a walk to the broker's targets; and B's free bytes bound the ladder's raises
/// (`mcx::ladder::note_b_room`), so a raise never grows a cache into the bytes B's draws claim.
fn tick_on(s: &budget::Snapshot, demand: u64) -> u64 {
    if mcx::ladder::floors_on() {
        mcx::ladder::note_b_room(s.free_b.saturating_sub(demand).saturating_sub(s.budget.r_low));
        let mut want = 0u64;
        if mempolicy::reclaim::below_low(s.free_b, s.budget.r_low) {
            mcx::memory_short_now();
            TICKS_BELOW_LOW.fetch_add(1, Ordering::Relaxed);
            want = mempolicy::reclaim::low_want(s.free_b, s.budget.r_high);
        }
        if let Some(w) = mempolicy::reclaim::burst_want(s.free_b.saturating_add(want), demand) {
            want = want.saturating_add(w);
        }
        if want == 0 {
            return 0;
        }
        let freed = {
            let _pass = memledger::ladder_pass::LadderPass::enter();
            ladder_reclaim(want)
        };
        mcx::ladder::note_reclaimer_take(freed);
        if freed != 0 {
            BACKGROUND_BYTES.fetch_add(freed, Ordering::Relaxed);
            let _ = budget::snapshot();
        }
        return freed;
    }
    let low = mempolicy::reclaim::below_low(s.free_b, s.budget.r_low);
    if !low && mempolicy::reclaim::burst_want(s.free_b, demand).is_none() {
        return 0;
    }
    let mut freed = 0u64;
    if low {
        // REACH: the backends' per-session caches (S5's plan memo) shed at their next statement (moved before the tick
        // count, which a test waits on)
        mcx::memory_short_now();
        TICKS_BELOW_LOW.fetch_add(1, Ordering::Relaxed);
        freed = reclaim(mempolicy::reclaim::low_want(s.free_b, s.budget.r_high), false);
    }
    let free = s.free_b.saturating_add(freed);
    if let Some(w) = mempolicy::reclaim::burst_want(free, demand) {
        freed = freed.saturating_add(reclaim(w, true));
    }
    if settle(freed) != 0 {
        BACKGROUND_BYTES.fetch_add(freed, Ordering::Relaxed);
        let _ = budget::snapshot();
    }
    freed
}

/// Whether a refused draw's bytes wait for the background reclaimer (its DEMAND, until the next tick serves it): a
/// waiting draw (`GrantPool::take_wait`) waits for that tick as for another holder's bytes. False whenever the reclaimer
/// does not apply.
pub fn pending() -> bool {
    applies() && DEMAND.load(Ordering::Relaxed) != 0
}

/// After a reclaim that freed `freed` bytes: the allocator's retained pages given back (B's resident bytes are the
/// process's, not the caches' sum) and the governor's sample re-read (a draw tests its bar next). `freed`.
fn settle(freed: u64) -> u64 {
    if freed != 0 {
        // LADDER-DESIGN 6a: a reclaim is a cache give-back: every holder thread's pages, then the purge
        mcx::give_back_caches();
        // pagepool: with the pool as the global allocator (or serving mcx / Gravity) the cut caches' blocks went back to
        // the pool's runs, not to the OS. This thread's pending remote frees and its shelf go home first (so those runs
        // can empty), then the warm runs are returned before the resample measures the cut. The loop thread releases
        // them measured (its own remaps); any other thread, pg:memwatchdog included, only asks the purger: those bytes
        // are landing (`reap::landing_credit`), never room. Never in give_back_caches: an idle pool stays warm.
        if pagepool::on() || pagepool::global_on() {
            pagepool::flush_shelf();
            if memledger::ladder_pass::on_ladder_thread() {
                // The synchronous release sweeps small-run handoffs before computing releasable bytes.
                let _ = pagepool::return_warm_now(usize::try_from(freed).unwrap_or(usize::MAX));
            } else {
                let _ = pagepool::flush_remote();
                let _ = pagepool::return_warm();
            }
        }
        mcx::governor::resample();
    }
    freed
}

/// `make_room`'s settle after a reclaim that freed `freed` bytes for a draw of `bytes`. REUSE (PGRUST_REUSE_BEFORE_RELEASE=1):
/// the freed images' pages go back to their arena with no purge (`mcx::keep_for_reuse`), and when B, which counts those
/// pages as used until a purge returns them, still has room for the draw, nothing is purged: the draw's allocation takes
/// them (the reuse claim) instead of fresh ones. B's free bytes are read as make_room reads them after the walk, the page
/// releaser's landing netted (`free_toward`): the walk's runs on their way back count, so a walk that covered the draw is
/// not taken for short. A draw still short gets today's give-back and purge. Switch off: `settle` as before.
fn settle_inline(freed: u64, bytes: u64) -> u64 {
    settle_inline_with(freed, bytes, || free_toward(bytes))
}

/// B's free bytes now for a draw of `bytes`, the page releaser's landing netted (`landing_toward`; knob off the landing
/// is 0: `free_b` alone). None: no snapshot.
fn free_toward(bytes: u64) -> Option<u64> {
    budget::snapshot().map(|s| Relief::of(Source::ReleaserLanding, landing_toward(bytes, &s)).ask_room(s.free_b))
}

/// `settle_inline` over a reader of B's free bytes (a fresh snapshot's).
fn settle_inline_with(freed: u64, bytes: u64, free_b: impl Fn() -> Option<u64>) -> u64 {
    if freed == 0 {
        return 0;
    }
    if mcx::keep_for_reuse() && free_b().is_some_and(|f| f >= bytes) {
        INLINE_REUSE_KEPT.fetch_add(1, Ordering::Relaxed);
        mcx::governor::resample();
        return freed;
    }
    INLINE_GIVE_BACKS.fetch_add(1, Ordering::Relaxed);
    settle(freed)
}

/// Whether B has `bytes` more free now, reclaiming inline when it has not: the walk toward the shortfall plus the reserve's
/// low mark, then the burst road for what is still short, at most `RECLAIM_INLINE_MAX` bytes in all. One draw at a time: a
/// draw that finds another reclaiming waits for it, then re-reads the free bytes. A draw still short becomes the
/// background's DEMAND. True whenever the reclaimer does not apply (today's draw).
pub fn make_room(bytes: u64) -> bool {
    if !applies() || budget::free_now() >= bytes {
        return true;
    }
    let _one = pgsync::lock(&INLINE);
    let Some(s) = budget::snapshot() else { return true };
    // no limit known (B 0): no budget to test the draw against, today's draw
    if s.free_b >= bytes || s.budget.b == 0 {
        return true;
    }
    // D4692 (`cache_release_bg`): what the page releaser is giving back now counts toward the draw (the walk's ask, the
    // burst's): no second eviction for the same bytes
    let on_way = Relief::of(Source::ReleaserLanding, landing_toward(bytes, &s));
    if on_way.ask_room(s.free_b) >= bytes {
        return true;
    }
    let t0 = MonoStamp::now();
    let purged0 = mcx::purged_bytes();
    let max = mempolicy::sizes::reclaim_inline_max();
    let mut noted = Noted::default();
    // ladder C1: the ladder is the one evicter: its R2a on this thread for the draw's shortfall only (the reserve under
    // B's low mark is the background tick's) for at most `cache_inline_us` (the same take as `ladder_room`'s, noted as the
    // ladder's inline take), no burst road past it; B had no room for the draw: none for a raise either
    let c1 = mcx::ladder::floors_on();
    let short = on_way.ask_after(bytes - s.free_b);
    let want = if c1 { short.min(max) } else { mempolicy::reclaim::inline_want(short, s.budget.r_low, max) };
    if c1 {
        mcx::ladder::note_b_room(0);
        if mempolicy::consts::get(mempolicy::consts::K::CacheInlineSkip) != 0 && r2a_has_nothing() {
            INLINE_SKIPS.fetch_add(1, Ordering::Relaxed);
            let free = free_toward(bytes).unwrap_or(u64::MAX);
            if free < bytes {
                DEMAND.fetch_max(bytes, Ordering::Relaxed);
            }
            if crate::env::make_room_log() {
                eprintln!("gravity: make_room skip: draw {} MiB B {} MiB free_B {} MiB free_after {} MiB {}", bytes >> 20, s.budget.b >> 20, s.free_b >> 20, free >> 20, if free >= bytes { "ok" } else { "short" });
            }
            return free >= bytes;
        }
    }
    let mut freed = if c1 { ladder_room_evict(want) } else { settle_inline(reclaim_noted(want, false, false, &mut noted), bytes) };
    let mut free = free_toward(bytes).unwrap_or(u64::MAX);
    let mut burst = 0;
    if let Some(w) = mempolicy::reclaim::inline_burst_want(bytes, free, max.saturating_sub(freed)).filter(|_| !c1) {
        burst = w;
        freed = freed.saturating_add(settle_inline(reclaim_noted(w, true, false, &mut noted), bytes));
        free = free_toward(bytes).unwrap_or(u64::MAX);
    }
    let purged1 = mcx::purged_bytes();
    let ns = t0.elapsed_ns();
    INLINE_RUNS.fetch_add(1, Ordering::Relaxed);
    INLINE_BYTES.fetch_add(freed, Ordering::Relaxed);
    INLINE_NS_MAX.fetch_max(ns, Ordering::Relaxed);
    INLINE_NS_SUM.fetch_add(ns, Ordering::Relaxed);
    INLINE_PURGED.fetch_add((purged1.0 + purged1.1).saturating_sub(purged0.0 + purged0.1), Ordering::Relaxed);
    let ok = free >= bytes;
    if !ok {
        INLINE_SHORT.fetch_add(1, Ordering::Relaxed);
        DEMAND.fetch_max(bytes, Ordering::Relaxed);
    }
    if crate::env::make_room_log() {
        log_make_room(bytes, &s, (want, burst), (freed, free), &noted, ns);
    }
    ok
}

/// D4692 (`cache_release_bg`): the bytes on the page releaser's landing (`allocarena::own`: cache runs a trim freed off
/// the releaser's thread, still resident, being given back now) a draw of `bytes` may count as free in `s`: all of them
/// while the draw stays under the governor's bar with them still resident, none past it, where this thread gives them
/// back itself first (today's inline release: kill-safety over the thread). Knob off: the landing is always 0, so the
/// draw reads `free_b` alone, as today. Never room for admission or the cancel line: only make_room's own ask nets it.
fn landing_toward(bytes: u64, s: &budget::Snapshot) -> u64 {
    landing_counts(bytes, allocarena::own::landing_bytes() as u64, s.free_b, s.bar, s.budget.b).unwrap_or_else(|| {
        allocarena::own::drain_landing();
        0
    })
}

/// `landing_toward`'s rule: of `landing` bytes on the releaser's landing, what a draw of `bytes` counts as free with B's
/// `free_b`, the governor's `bar` (0 = off) and B's size `b`: all while the draw stays under the bar with them resident,
/// None past it (the caller gives them back itself first). No landing: 0.
fn landing_counts(bytes: u64, landing: u64, free_b: u64, bar: u64, b: u64) -> Option<u64> {
    if landing == 0 {
        return Some(0);
    }
    (bar == 0 || bytes <= free_b.saturating_add(bar.saturating_sub(b))).then_some(landing)
}

/// D4690 (`PGRUST_MAKE_ROOM_LOG=1`): one line per inline make_room: the draw, B and its free bytes before, the walk's ask
/// and the burst's, the bytes each rung and each cache freed, B's free bytes after, the outcome and the time.
#[cold]
fn log_make_room(draw: u64, s: &budget::Snapshot, (want, burst): (u64, u64), (freed, free): (u64, u64), n: &Noted, ns: u64) {
    // each cache's freed MiB, written straight into the line (no String built)
    struct Caches<'a>(&'a [u64; 5]);
    impl core::fmt::Display for Caches<'_> {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            ORDER.iter().zip(self.0).try_for_each(|(c, b)| write!(f, " {}={}", c.name(), b >> 20))
        }
    }
    let caches = Caches(&n.caches);
    eprintln!(
        "gravity: make_room: draw {} MiB B {} MiB free_B {} MiB r_low {} MiB want {} MiB burst {} MiB freed {} MiB (regions {} slots {} caches {}:{}) free_after {} MiB {} {} us",
        draw >> 20,
        s.budget.b >> 20,
        s.free_b >> 20,
        s.budget.r_low >> 20,
        want >> 20,
        burst >> 20,
        freed >> 20,
        n.rungs[0] >> 20,
        n.rungs[1] >> 20,
        n.rungs[2] >> 20,
        caches,
        free >> 20,
        if free >= draw { "ok" } else { "short" },
        ns / 1000,
    );
}

/// Ladder C1: one draw evicts inline at a time; another draw finding it busy goes on (the bytes are on their way).
static C1_INLINE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// D4788: whether the ladder's R2a walk could take nothing now, from reads only: no parked regions, no trimmable slot sets,
/// every cache at or under its protected floor (R2a keeps at least that of it), every tail holder at its bytes in use. The
/// walk's release of the slot sets runs either way. A walk this says could take may still free nothing; it runs.
fn r2a_has_nothing() -> bool {
    crate::slots::note_released(0);
    crate::slots::drain_released();
    if crate::park::REGIONS.parked_bytes() != 0 || crate::slots::DEPOT.trimmable_bytes() != 0 {
        return false;
    }
    let floors = protected_floors();
    let caches = ORDER.iter().zip(floors).all(|(&c, f)| budget::hooks(c).filter(|h| h.reclaim.is_some()).is_none_or(|h| (h.current)() <= f));
    caches
        && mempolicy::ladder::TAIL.iter().all(|&c| match (budget::hooks(c), budget::ladder_reclaim_of(c)) {
            (Some(h), Some((in_use, _))) => (h.current)() <= in_use(),
            _ => true,
        })
}

/// D4765: a test's clock for the inline eviction's deadline (`cache_inline_us`) in place of the monotonic one (bit 63 set =
/// set, the low bits its ns): a test's slow trim moves it, so no assert on what the deadline cut depends on the box's load.
static INLINE_CLOCK_FOR_TEST: AtomicU64 = AtomicU64::new(0);

pub fn set_inline_clock_for_test(ns: Option<u64>) {
    INLINE_CLOCK_FOR_TEST.store(ns.map_or(0, |n| (1 << 63) | n), Ordering::Relaxed);
}

fn inline_now_ns() -> u64 {
    match INLINE_CLOCK_FOR_TEST.load(Ordering::Relaxed) {
        0 => MonoStamp::now().as_ns(),
        v => v & !(1 << 63),
    }
}

/// Ladder C1 (`cache_floor` with the ladder on; CACHE-IMPLEMENTATION-PLAN section 7, the on-demand path): before a draw
/// of `bytes` the pool has room for, the ladder's R2a on the asker's thread for what keeps LOW free under the ladder's
/// line after it (`mcx::ladder::inline_need`), for at most `cache_inline_us` (read between caches), inside a ladder pass
/// (no wait on a cache's lock or the reaper). The draw goes on whatever it freed: past the line the ladder's own rungs
/// act. Off, or with room: one relaxed load or two.
#[inline]
pub fn ladder_room(bytes: u64) {
    // the C1 try (`inline_on_draw`): the take moves to the statement's maps (`ladder_mapped`)
    if !mcx::ladder::floors_on() || mempolicy::ladder::inline_on_draw() {
        return;
    }
    let want = mcx::ladder::inline_need(bytes);
    if want != 0 {
        let _ = ladder_room_evict(want);
    }
}

/// Ladder C1 try (`inline_on_draw` = 1: the cache bytes go to the query trying to allocate): a
/// statement mapped `bytes` fresh (an arena chunk, a slot region: `Inner::mapped_fresh`). They count against the room
/// since the ladder's last sample, and when they leave less than LOW under its line the ladder's R2a evicts inline on
/// this thread, as `ladder_room` does at a reservation (one draw at a time, `cache_inline_us`). Off: one or two relaxed
/// loads.
#[inline]
pub fn ladder_mapped(bytes: u64) {
    if !mcx::ladder::floors_on() || !mempolicy::ladder::inline_on_draw() {
        return;
    }
    mcx::ladder::note_mapped(bytes);
    let want = mcx::ladder::inline_need_mapped();
    if want != 0 {
        let _ = ladder_room_evict(want);
    }
}

/// The ladder's inline R2a for `want` bytes on this thread, one at a time (another thread's inline take on its way:
/// nothing here, 0), for at most `cache_inline_us` read between caches. The bytes freed.
#[cold]
fn ladder_room_evict(want: u64) -> u64 {
    if C1_INLINE.swap(true, Ordering::Acquire) {
        return 0;
    }
    let t0 = inline_now_ns();
    let max_ns = mempolicy::consts::get(mempolicy::consts::K::CacheInlineUs).saturating_mul(1000);
    let freed = {
        let _pass = memledger::ladder_pass::LadderPass::enter();
        // given back inside (`settle`)
        ladder_reclaim_until(want, &|| inline_now_ns().saturating_sub(t0) >= max_ns)
    };
    mcx::ladder::note_inline(want, freed, inline_now_ns().saturating_sub(t0));
    C1_INLINE.store(false, Ordering::Release);
    freed
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::AtomicU64;

    const MIB: u64 = 1 << 20;

    static A: AtomicU64 = AtomicU64::new(0);
    static B: AtomicU64 = AtomicU64::new(0);
    static C: AtomicU64 = AtomicU64::new(0);

    fn a() -> u64 {
        A.load(Ordering::Relaxed)
    }
    fn b() -> u64 {
        B.load(Ordering::Relaxed)
    }
    fn c() -> u64 {
        C.load(Ordering::Relaxed)
    }
    fn floor_b() -> u64 {
        30 * MIB
    }
    fn zero() -> u64 {
        0
    }
    fn trim_a(to: u64) -> u64 {
        A.swap(to, Ordering::Relaxed) - to
    }
    fn trim_b(to: u64) -> u64 {
        B.swap(to, Ordering::Relaxed) - to
    }
    fn trim_c(to: u64) -> u64 {
        C.swap(to, Ordering::Relaxed) - to
    }

    fn set(x: u64, y: u64, z: u64) {
        A.store(x, Ordering::Relaxed);
        B.store(y, Ordering::Relaxed);
        C.store(z, Ordering::Relaxed);
    }

    fn caches(ta: Option<u64>, tb: Option<u64>) -> [Option<Managed>; 3] {
        [
            Some(Managed { hooks: Hooks { current: a, floor: zero, cap: a, reclaim: Some(trim_a) }, target: ta }),
            Some(Managed { hooks: Hooks { current: b, floor: floor_b, cap: b, reclaim: Some(trim_b) }, target: tb }),
            // no reclaim hook: never freed
            Some(Managed { hooks: Hooks { current: c, floor: zero, cap: c, reclaim: None }, target: Some(0) }),
        ]
    }

    /// The walk frees the cheapest cache first, each down to its target, stops once `want` is freed, and never touches a
    /// cache with no reclaim hook. One test owns the statics.
    #[test]
    fn the_walk_frees_cheapest_first_down_to_targets_and_bursts_to_floors() {
        set(100 * MIB, 100 * MIB, 100 * MIB);
        assert_eq!(walk(40 * MIB, false, &caches(Some(20 * MIB), Some(50 * MIB))), 40 * MIB, "the first cache covers it");
        assert_eq!((a(), b()), (60 * MIB, 100 * MIB));
        set(100 * MIB, 100 * MIB, 100 * MIB);
        assert_eq!(walk(150 * MIB, false, &caches(Some(20 * MIB), Some(50 * MIB))), 130 * MIB, "each down to its target");
        assert_eq!((a(), b(), c()), (20 * MIB, 50 * MIB, 100 * MIB));
        assert_eq!(walk(150 * MIB, true, &caches(Some(20 * MIB), Some(50 * MIB))), 40 * MIB, "a burst goes to the floors");
        assert_eq!((a(), b(), c()), (0, 30 * MIB, 100 * MIB));
        set(100 * MIB, 100 * MIB, 0);
        assert_eq!(walk(150 * MIB, false, &caches(None, None)), 0, "no target yet: nothing freed off the burst road");
        assert_eq!(walk(150 * MIB, true, &caches(None, None)), 150 * MIB, "a burst ignores targets");
        assert_eq!((a(), b()), (0, 50 * MIB));
        assert_eq!(walk(10 * MIB, true, &[None, None]), 0, "unmanaged slots are skipped");
        // D4690: the make_room line's bytes per cache, by the cache's index in the walk
        set(100 * MIB, 100 * MIB, 100 * MIB);
        let mut by = [0u64; 5];
        assert_eq!(walk_noted(150 * MIB, false, &caches(Some(20 * MIB), Some(50 * MIB)), &mut by), 130 * MIB);
        assert_eq!(by, [80 * MIB, 50 * MIB, 0, 0, 0], "each cache's freed bytes at its index");
    }

    /// D4404 (born red): the memory ladder's thread never waits for the reaper. R2a (`ladder_reclaim`), after the parked
    /// regions and slot sets it trims, waited in `reap::drain` until the reaper had unmapped its whole queue, with no
    /// deadline (D4098 saw that queue hold a draw 323 ms under memory pressure): a pass with no sample, no R6 and no wake for
    /// that long, exactly when the server is short. Here the reaper is held mid-batch for a second: inside the ladder's
    /// pass R2a's wait returns at once (what the reaper still owes is the pass's landing credit, re-read next pass);
    /// outside a pass (the reclaimer, a gate's own climb, the backstop) it waits as before.
    #[test]
    fn d4404_inside_the_ladder_pass_r2a_never_waits_for_the_reaper() {
        use core::time::Duration;
        let busy = crate::mem::reap::hold_busy();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(1));
            drop(busy);
        });
        let t0 = std::time::Instant::now();
        {
            let _pass = memledger::ladder_pass::LadderPass::enter();
            let mut unmapped = 1;
            reap(&mut unmapped);
        }
        let inside = t0.elapsed();
        let t1 = std::time::Instant::now();
        let mut unmapped = 1;
        reap(&mut unmapped);
        let outside = t1.elapsed();
        release.join().unwrap();
        assert!(inside < Duration::from_millis(100), "inside the ladder's pass R2a waited for the reaper: {inside:?}");
        assert!(outside >= Duration::from_millis(500), "outside a pass R2a waits for the reaper as before: {outside:?}");
    }

    static KEPT_CALLS: AtomicU64 = AtomicU64::new(0);

    fn keep() {
        KEPT_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    /// REUSE: an inline reclaim's settle keeps the freed pages for the draw (no give-back, no purge) when, with the switch's
    /// hook installed, B counting the committed free bytes has room for the draw; a draw still short gets the give-back as
    /// before; nothing freed, nothing done.
    #[test]
    fn reuse_an_inline_reclaim_keeps_the_freed_pages_when_they_cover_the_draw() {
        let c0 = counters();
        assert_eq!(settle_inline_with(0, 8 * MIB, || Some(u64::MAX)), 0, "nothing freed: nothing to settle");
        mcx::set_reuse_keep(keep);
        assert_eq!(settle_inline_with(16 * MIB, 8 * MIB, || Some(8 * MIB)), 16 * MIB);
        let c1 = counters();
        assert_eq!((c1.inline_reuse_kept - c0.inline_reuse_kept, c1.inline_give_backs - c0.inline_give_backs), (1, 0), "kept for the draw");
        assert_eq!(KEPT_CALLS.load(Ordering::Relaxed), 1, "the freed pages went back to their arena (the keep hook)");
        assert_eq!(settle_inline_with(16 * MIB, 8 * MIB, || Some(4 * MIB)), 16 * MIB);
        let c2 = counters();
        assert_eq!((c2.inline_reuse_kept - c1.inline_reuse_kept, c2.inline_give_backs - c1.inline_give_backs), (0, 1), "still short: the give-back");
        // with the page releaser (`cache_release_bg`) the settle reads B's free bytes with its landing netted, as
        // make_room does after the walk: a walk whose runs are on their way back covers the draw and the freed pages are
        // kept; past the governor's bar the landing counts for nothing (the drawing thread gives it back first). Knob off
        // the landing is 0: the read is `free_b` alone.
        let (draw, free_b, landing, b) = (8 * MIB, 4 * MIB, 6 * MIB, 1024 * MIB);
        assert_eq!(landing_counts(draw, 0, free_b, 0, b), Some(0), "knob off: no landing, free_b alone");
        assert_eq!(landing_counts(draw, 0, free_b, b + 64 * MIB, b), Some(0));
        assert_eq!(landing_counts(draw, landing, free_b, 0, b), Some(landing), "no bar: all of it counts");
        assert_eq!(landing_counts(draw, landing, free_b, b + 64 * MIB, b), Some(landing), "under the bar with it resident");
        assert_eq!(landing_counts(draw, landing, free_b, b + 2 * MIB, b), None, "past the bar: given back first, none counts");
        let c3 = counters();
        let netted = || landing_counts(draw, landing, free_b, b + 64 * MIB, b).map(|l| free_b + l);
        assert_eq!(settle_inline_with(16 * MIB, draw, netted), 16 * MIB);
        let c4 = counters();
        assert_eq!((c4.inline_reuse_kept - c3.inline_reuse_kept, c4.inline_give_backs - c3.inline_give_backs), (1, 0), "the landing covers the draw: kept");
        assert_eq!(settle_inline_with(16 * MIB, draw, || Some(free_b)), 16 * MIB);
        let c5 = counters();
        assert_eq!((c5.inline_reuse_kept - c4.inline_reuse_kept, c5.inline_give_backs - c4.inline_give_backs), (0, 1), "free_b alone reads short: the give-back");
    }

    /// Off and advise: no reclaimer, every draw is today's, no cache is declared managed.
    #[test]
    fn off_and_advise_never_reclaim_or_manage() {
        let _one = pgsync::lock(&budget::TEST_MODE);
        for mode in [Mode::Off, Mode::Advise] {
            budget::configure(budget::Cfg { mode, ..budget::Cfg::DEFAULT });
            assert!(!applies());
            assert!(make_room(u64::MAX - 1), "{mode:?}: a draw is today's");
            assert!(!pending(), "{mode:?}: no draw waits for a reclaimer");
            sync_managed();
            assert!(CACHES.iter().all(|c| !budget::managed(*c)), "{mode:?}");
            assert_eq!(tick(), 0);
        }
        budget::configure(budget::Cfg { mode: Mode::On, actuators: actuator::WORK_POOL, ..budget::Cfg::DEFAULT });
        assert!(!applies(), "on without the reclaimer actuator");
        budget::configure(budget::Cfg::DEFAULT);
    }
}
