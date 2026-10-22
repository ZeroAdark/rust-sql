use crate::consts::{get, K, KNOBS};

/// The process caches' shares of RAM in 64ths: together at most 18 / 64 under any grant pool (`process_caches_cap`).
pub const TABLES_RAM_64THS: u64 = 8;
pub const BUILD_POOL_RAM_64THS: u64 = 8;
pub const CONDITION_CACHE_RAM_64THS: u64 = 1;
pub const ZONE_STORE_RAM_64THS: u64 = 1;
const _: () = assert!(TABLES_RAM_64THS + BUILD_POOL_RAM_64THS + CONDITION_CACHE_RAM_64THS + ZONE_STORE_RAM_64THS <= 64 / 4 + 64 / 32);

const fn ram_64ths(ram: u64, n: u64) -> u64 {
    ram / 64 * n + ram % 64 * n / 64
}

/// 0 = unknown; nothing is kept when neither RAM nor the pool is known.
const fn pool_bound(ram: u64, pool: u64, n: u64) -> u64 {
    let r = ram_64ths(ram, n);
    match (ram, pool) {
        (0, p) => p,
        (_, 0) => r,
        (_, p) if r < p => r,
        (_, p) => p,
    }
}
pub const fn part_cache_cap(ram: u64, pool: u64) -> u64 {
    let r = ram_64ths(ram, 8);
    let p = pool / 4;
    match (ram, pool) {
        (0, 0) => u64::MAX,
        (0, _) => p,
        (_, 0) => r,
        _ if r < p => r,
        _ => p,
    }
}
/// Never more than three quarters of what shared_buffers leaves; 0 when RAM is unknown.
pub const fn derived_pool_cap(ram: u64, shared: u64) -> u64 {
    let was = ram.saturating_sub(shared) / 4 * 3;
    let fits = ram_64ths(ram, 54).saturating_sub(shared).saturating_sub(ram_64ths(ram, 8));
    if fits < was { fits } else { was }
}
pub const fn build_pool_cap(ram: u64, pool: u64) -> u64 {
    pool_bound(ram, pool, BUILD_POOL_RAM_64THS)
}
pub const fn condition_cache_cap(ram: u64, pool: u64) -> u64 {
    pool_bound(ram, pool, CONDITION_CACHE_RAM_64THS)
}
pub const fn zone_store_cap(ram: u64, pool: u64) -> u64 {
    pool_bound(ram, pool, ZONE_STORE_RAM_64THS)
}
/// Not bounded by the grant pool: an LRU under the parts' tables rebuilds them all at every read after a write.
pub const fn resident_tables_cap(ram: u64, pool: u64) -> u64 {
    if ram == 0 { pool } else { ram_64ths(ram, TABLES_RAM_64THS) }
}
pub const fn process_caches_cap(ram: u64, pool: u64) -> u64 {
    resident_tables_cap(ram, pool).saturating_add(build_pool_cap(ram, pool)).saturating_add(condition_cache_cap(ram, pool)).saturating_add(zone_store_cap(ram, pool))
}

pub fn os_reserve(limit: u64) -> u64 {
    let r = limit / get(K::OsReserveDiv).max(1);
    r.max(get(K::OsReserveMin))
}

pub fn page_cache_floor_default(limit: u64) -> u64 {
    limit / get(K::PageCacheFloorDiv).max(1)
}

pub const fn budget_auto(limit: u64, os_reserve: u64, page_cache_floor: u64) -> u64 {
    limit.saturating_sub(os_reserve).saturating_sub(page_cache_floor)
}

pub fn reserve_low(b: u64) -> u64 {
    let r = b / get(K::ReserveDiv).max(1);
    r.max(get(K::ReserveMin))
}

pub fn reserve_high(b: u64) -> u64 {
    let r = b / get(K::ReserveDiv).max(1);
    reserve_low(b).saturating_add(r.max(get(K::ReserveMin)))
}

/// B = min(B_auto, target, bar): `target` only lowers B. The free reserve lives inside B, never subtracted from it twice.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Budget {
    pub limit: u64,
    pub os_reserve: u64,
    pub page_cache_floor: u64,
    pub b_auto: u64,
    pub b: u64,
    pub r_low: u64,
    pub r_high: u64,
}

pub fn budget_of(limit: u64, target: u64, page_cache_floor: u64, reserve: u64, bar: u64) -> Budget {
    let os = os_reserve(limit);
    let pcf = if page_cache_floor != 0 { page_cache_floor } else { page_cache_floor_default(limit) };
    let b_auto = if limit == 0 { 0 } else { budget_auto(limit, os, pcf) };
    let mut b = b_auto;
    if target != 0 && target < b {
        b = target;
    }
    if bar != 0 && bar < b {
        b = bar;
    }
    let (r_low, r_high) = if reserve != 0 { (reserve, reserve.saturating_add(reserve)) } else { (reserve_low(b), reserve_high(b)) };
    Budget { limit, os_reserve: os, page_cache_floor: pcf, b_auto, b, r_low, r_high }
}

pub const WORK_POOL_MIN: u64 = KNOBS[K::WorkPoolMin as usize].default;
pub const RECLAIM_INLINE_MAX: u64 = KNOBS[K::ReclaimInlineMax as usize].default;
pub const RECLAIM_TICK_MS: u64 = KNOBS[K::ReclaimTickMs as usize].default;

#[inline]
pub fn work_pool_min() -> u64 {
    get(K::WorkPoolMin)
}

#[inline]
pub fn reclaim_inline_max() -> u64 {
    get(K::ReclaimInlineMax)
}

#[inline]
pub fn reclaim_tick_ms() -> u64 {
    get(K::ReclaimTickMs)
}

pub fn work_pool_capacity(budget: &Budget, fixed: u64, unattributed: u64, shared_buffers: u64, caches: u64) -> u64 {
    budget
        .b
        .saturating_sub(budget.r_high)
        .saturating_sub(fixed)
        .saturating_sub(unattributed)
        .saturating_sub(shared_buffers)
        .saturating_sub(caches)
        .max(work_pool_min())
}

/// B0: what a cache keeps from the pool: managed, its floor or less but never its pinned bytes; unmanaged, its cap.
pub fn cache_kept(managed: bool, current: u64, floor: u64, cap: u64, pinned: u64) -> u64 {
    if managed { floor.min(current).max(pinned) } else { cap }
}

/// B0's margin under memory.max (%): with the pool small the C7 stress spent 6.5 min (not 22.6) within 5 % of the limit and
/// pgbench stalled 27 s (not 59). CHOSEN past that zone, a tick to spare. The room under it counts clean inactive pages free.
pub const POOL_HEADROOM_PCT: u64 = 10;

pub fn pool_headroom_room(limit: u64, current: u64, clean_inactive: u64) -> Option<u64> {
    if limit == 0 || current == 0 {
        return None;
    }
    let line = limit - (limit / 100 * POOL_HEADROOM_PCT + limit % 100 * POOL_HEADROOM_PCT / 100);
    Some(line.saturating_sub(current.saturating_sub(clean_inactive)))
}

/// `grant_room_counts_cache`: the pool tick's room, plus with `on` the caches' bytes past what they keep (`evictable`).
pub fn grant_room(room: u64, evictable: u64, on: bool) -> u64 {
    if on { room.saturating_add(evictable) } else { room }
}

pub fn pool_tick_capacity(capacity: u64, held: u64, room: Option<u64>) -> u64 {
    room.map_or(capacity, |r| capacity.min(held.saturating_add(r)).max(work_pool_min()))
}

pub const fn caches_at_caps(ram: u64) -> u64 {
    part_cache_cap(ram, 0).saturating_add(process_caches_cap(ram, 0))
}

/// Never under `least`: one statement's grant still fits whole. Ladder C1 (`floors`: `cache_floor` with the ladder on):
/// the caches at their protected floors (`ladder::pool_caches`) and the HIGH mark of `limit` as the reserve in place of
/// R_high (`ladder::marks`): the ladder evicts what the caches hold above their floors.
#[allow(clippy::too_many_arguments)]
pub fn derived_pool_in_b(today: u64, budget: &Budget, fixed: u64, shared_buffers: u64, ram: u64, part_cache_explicit: bool, least: u64, floors: bool) -> u64 {
    if budget.b == 0 || part_cache_explicit {
        return today;
    }
    let caches = crate::ladder::pool_caches(caches_at_caps(ram), budget.b, floors);
    let budget = if floors { Budget { r_high: crate::ladder::marks(budget.limit).1, ..*budget } } else { *budget };
    today.min(work_pool_capacity(&budget, fixed, 0, shared_buffers, caches).max(least))
}

/// Never under what the pool holds: a capacity under the held bytes would refuse every running statement's next draw.
pub fn pool_follow(capacity: u64, target: Option<u64>, held: u64) -> u64 {
    capacity.max(target.unwrap_or(0)).max(held)
}

pub const OUTSIDE_ALLOWANCE: u64 = 3 << 30;
pub const THREAD_ARENA: u64 = 64 << 20;
pub const ARENAS_PER_CPU: u64 = 8;

pub fn railed(limit: u64, mem: u64) -> bool {
    limit != 0 && (mem == 0 || limit < mem * 2)
}

pub fn rail_reserve(slots: u64, admitting: u64, stack: u64, cpus: u64, admission_arena: u64) -> u64 {
    slots.saturating_mul(stack).saturating_add(slots.min(cpus * ARENAS_PER_CPU) * THREAD_ARENA).saturating_add(admitting.saturating_mul(admission_arena))
}

pub fn railed_pool(limit: u64, mapped: u64, reserve: u64, per_pooled: u64) -> (u64, u64) {
    let headroom = limit.saturating_sub(mapped).saturating_sub(reserve);
    let n = (headroom / (per_pooled + OUTSIDE_ALLOWANCE)).clamp(1, 64);
    (n, headroom.saturating_sub(n * OUTSIDE_ALLOWANCE).min(n * per_pooled).max(per_pooled))
}

/// A flat `arena::KEEP_BYTES` a level, the most a reset arena keeps, so a pool fitted inside B bounds the statements.
pub fn keeps_arenas(railed: bool, explicit: bool, fitted: bool, cap: u64) -> bool {
    !railed && !explicit && !fitted && cap != 0
}

pub fn fitted(railed: bool, from_budget: bool, armed: u64, derived: u64) -> bool {
    !railed && !from_budget && armed < derived
}

pub fn pool_capacity(explicit_mb: Option<u64>, from_budget: Option<u64>, today: u64) -> u64 {
    explicit_mb.map_or(from_budget.unwrap_or(today), |mb| mb << 20)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consts::{reset, set, TEST_LOCK};

    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;

    #[test]
    fn a_derived_pool_fits_b_beside_every_cache_at_its_cap_unless_the_part_cache_is_explicit() {
        let _one = pgsync::lock(&TEST_LOCK);
        reset();
        let limit = 15_741 * MIB;
        let today = derived_pool_cap(limit, 128 * MIB);
        assert_eq!(today >> 20, 11_185, "today's derived pool under the unit, as the server logged it");
        let caches = caches_at_caps(limit);
        assert_eq!(caches, part_cache_cap(limit, 0) + process_caches_cap(limit, 0));
        assert_eq!(caches >> 20, 6_394, "part cache, fp tables, build pool at RAM / 8, the two stores at RAM / 64");
        let b = budget_of(limit, 0, 0, 0, 0);
        assert_eq!(b.b >> 20, 13_733);
        let pool = derived_pool_in_b(today, &b, 0, 128 * MIB, limit, false, 64 * MIB, false);
        assert_eq!(pool, b.b - b.r_high - 128 * MIB - caches);
        assert_eq!(pool >> 20, 5_162);
        assert!(pool + caches + 128 * MIB + b.r_high <= b.b, "B fits");
        assert_eq!(derived_pool_in_b(today, &b, 0, 128 * MIB, limit, false, 8 * GIB, false), 8 * GIB, "a statement's grant still fits whole");
        assert_eq!(derived_pool_in_b(today, &b, 0, 128 * MIB, limit, false, 16 * GIB, false), today, "never above today's");
        assert_eq!(derived_pool_in_b(today, &Budget::default(), 0, 0, limit, false, 0, false), today, "no limit known");
        assert_eq!(derived_pool_in_b(today, &b, 0, 128 * MIB, limit, true, 64 * MIB, false), today, "an explicit part cache: the operator's call");
        let cb = 30 * GIB + 768 * MIB;
        let cb_today = derived_pool_cap(cb, 128 * MIB);
        assert_eq!(cb_today >> 20, 22_504);
        assert_eq!(derived_pool_in_b(cb_today, &budget_of(cb, 0, 0, 0, 0), 0, 128 * MIB, cb, true, 8 * GIB, false), cb_today);
        assert!(fitted(false, false, pool, today) && !keeps_arenas(false, false, true, pool));
        assert!(!fitted(false, false, today, today) && keeps_arenas(false, false, false, today));
        assert!(!fitted(true, false, pool, today), "a rail's pool is not R5's");
        assert!(!fitted(false, true, pool, today), "mode on re-arms from B: the holds stay");
        reset();
    }

    /// Ladder C1 (`cache_floor`) at 8 GiB (D4345's unit): the pool armed with the caches at their protected floors and
    /// the HIGH mark as its reserve, ~3.5x today's 14 % of the limit; B still fits.
    #[test]
    fn c1_a_derived_pool_counts_the_caches_at_their_floors_and_keeps_high() {
        let _one = pgsync::lock(&TEST_LOCK);
        reset();
        let limit = 8 * GIB;
        let today = derived_pool_cap(limit, 128 * MIB);
        let b = budget_of(limit, 0, 0, 0, 0);
        let off = derived_pool_in_b(today, &b, 0, 128 * MIB, limit, false, 64 * MIB, false);
        assert_eq!(off >> 20, 1_152, "D4345: 1.2 GB of 8 GiB");
        let on = derived_pool_in_b(today, &b, 0, 128 * MIB, limit, false, 64 * MIB, true);
        let (_, high) = crate::ladder::marks(limit);
        let floors = crate::ladder::protected_of(b.b);
        assert_eq!(on, b.b - high - 128 * MIB - floors);
        assert_eq!(on >> 20, 4_044);
        assert!(on + floors + 128 * MIB + high <= b.b, "B fits");
        assert_eq!(derived_pool_in_b(today, &b, 0, 128 * MIB, limit, true, 64 * MIB, true), today, "an explicit part cache: the operator's call");
        reset();
    }

    #[test]
    fn b0_a_managed_cache_keeps_its_pinned_bytes_out_of_the_pool() {
        let _one = pgsync::lock(&TEST_LOCK);
        reset();
        assert_eq!(cache_kept(true, 5 * GIB, GIB, 8 * GIB, 3 * GIB), 3 * GIB);
        assert_eq!(cache_kept(true, 5 * GIB, GIB, 8 * GIB, 0), GIB);
        assert_eq!(cache_kept(true, 512 * MIB, GIB, 8 * GIB, 0), 512 * MIB, "under its floor: what it holds");
        assert_eq!(cache_kept(false, 5 * GIB, GIB, 8 * GIB, 3 * GIB), 8 * GIB, "unmanaged: its cap");
        let b = budget_of(32 * GIB, 0, 0, 0, 0);
        let kept = cache_kept(true, 5 * GIB, GIB, 8 * GIB, 3 * GIB);
        assert_eq!(work_pool_capacity(&b, 0, 0, 0, kept), b.b - b.r_high - 3 * GIB);
    }

    #[test]
    fn b0_the_tick_grows_the_pool_only_under_the_headroom_margin() {
        let _one = pgsync::lock(&TEST_LOCK);
        reset();
        let limit = 20 * GIB;
        assert_eq!(pool_headroom_room(limit, 0, 0), None, "no cgroup reading");
        assert_eq!(pool_headroom_room(limit, 15 * GIB, GIB), Some(4 * GIB), "line at 18 GiB, 14 GiB used");
        assert_eq!(pool_headroom_room(limit, 19 * GIB, 0), Some(0), "inside the margin: no room");
        assert_eq!(pool_tick_capacity(10 * GIB, 3 * GIB, Some(4 * GIB)), 7 * GIB, "what it holds plus the room");
        assert_eq!(pool_tick_capacity(5 * GIB, 3 * GIB, Some(4 * GIB)), 5 * GIB, "never past free room in B");
        assert_eq!(pool_tick_capacity(10 * GIB, 0, Some(0)), work_pool_min(), "never under the minimum");
        assert_eq!(pool_tick_capacity(10 * GIB, 3 * GIB, None), 10 * GIB);
        // grant_room_counts_cache: 1 GiB free and 5 GiB of cache past its floor admit a 6 GiB grant whole, never past B
        assert_eq!(pool_tick_capacity(10 * GIB, 0, Some(grant_room(GIB, 5 * GIB, false))), GIB.max(work_pool_min()), "off: the free bytes only");
        assert_eq!(pool_tick_capacity(10 * GIB, 0, Some(grant_room(GIB, 5 * GIB, true))), 6 * GIB, "on: the evictable cache too");
        assert_eq!(pool_tick_capacity(4 * GIB, 0, Some(grant_room(GIB, 5 * GIB, true))), 4 * GIB, "on: never past B's capacity");
    }

    #[test]
    fn the_pool_capacity_keeps_its_minimum_and_the_knobs_move_it() {
        let _one = pgsync::lock(&TEST_LOCK);
        reset();
        let b = budget_of(32 * GIB, 0, 0, 0, 0);
        assert_eq!(b.r_high, reserve_high(b.b));
        assert_eq!(work_pool_capacity(&b, GIB, 0, 128 * MIB, 4 * GIB), b.b - b.r_high - GIB - 128 * MIB - 4 * GIB);
        assert_eq!(work_pool_capacity(&b, b.b, 0, 0, 0), WORK_POOL_MIN);
        set(K::WorkPoolMin, 2 * GIB);
        assert_eq!(work_pool_capacity(&b, b.b, 0, 0, 0), 2 * GIB);
        set(K::ReserveMin, 2 * GIB);
        assert_eq!(reserve_low(8 * GIB), 2 * GIB);
        reset();
        assert_eq!(reserve_low(8 * GIB), GIB);
        assert_eq!(os_reserve(64 * GIB), 2 * GIB);
    }

    #[test]
    fn a_rail_splits_its_headroom_and_an_explicit_capacity_wins() {
        let (n, cap) = railed_pool(64 * GIB, 4 * GIB, 0, GIB);
        assert_eq!(n, 15);
        assert_eq!(cap, 15 * GIB);
        assert_eq!(railed_pool(GIB, 0, 0, GIB), (1, GIB), "at least one statement and its pooled bytes");
        assert!(railed(8 * GIB, 32 * GIB) && !railed(0, 32 * GIB) && !railed(64 * GIB, 32 * GIB) && railed(64 * GIB, 0));
        assert_eq!(pool_capacity(Some(256), Some(GIB), 2 * GIB), 256 * MIB);
        assert_eq!(pool_capacity(None, Some(GIB), 2 * GIB), GIB);
        assert_eq!(pool_capacity(None, None, 2 * GIB), 2 * GIB);
        assert_eq!(pool_follow(GIB, Some(2 * GIB), 0), 2 * GIB, "the broker's target past B's capacity");
        assert_eq!(pool_follow(GIB, None, 3 * GIB), 3 * GIB, "never under what it holds");
        assert_eq!(pool_follow(4 * GIB, Some(2 * GIB), GIB), 4 * GIB);
    }
}
