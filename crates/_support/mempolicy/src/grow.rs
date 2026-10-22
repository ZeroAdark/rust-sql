/// CHOSEN, not tuned: growth doubles from it in place (2 MiB to 2 GiB in ten commits).
pub const START_BYTES: u64 = 2 << 20;

/// A T1's table, side words and chain words must fit the start past its header, so the start holds four `t1_bytes`.
#[inline]
pub fn start_bytes(share: u32, t1_bytes: u64) -> u32 {
    let start = START_BYTES.max(4 * t1_bytes);
    ((share as u64).min(start) as u32) & !7
}

#[inline]
pub fn grow_to(len: u64, want: u64, max: u64) -> u64 {
    len.saturating_mul(2).max(want).min(max)
}

/// D3644 (b): a group sort's array of `bytes` comes from the statement's grant (`SPILL_WORK_TAKE`), not the allocator.
#[inline]
pub fn work_from_grant(bytes: u64) -> bool {
    bytes >= crate::consts::get(crate::consts::K::WorkFloor)
}

/// Grow mode (`pgrust.gravity_grow`): a statement's grant is its estimate start, smaller than today's ask:
/// the estimate x `scale`, at least one region start (`START_BYTES`), no grant floor; at most `cap` (the GUCs'
/// grant). Its breakers grow on demand past it.
#[inline]
pub fn grow_want(estimate: u64, scale: u64, cap: u64) -> u64 {
    estimate.saturating_mul(scale.max(1)).max(START_BYTES).min(cap)
}

/// Grow mode with work_mem unset (its source the default): no work_mem ceiling. A breaker's ceiling is the most a region
/// can hold (`max`, the region format's wall), held to the pool's big lane `lane` (0 = unarmed: `max`), never 0 (0 reads
/// "no ceiling, no spill road" everywhere): past it, or where the pool has no room now, the breaker spills.
#[inline]
pub fn grow_ceiling(lane: u64, max: u64) -> u64 {
    if lane == 0 { max } else { lane.min(max).max(START_BYTES) }
}

#[inline]
pub fn spills_at_ceiling(want: u64, cap: u64) -> bool {
    want > cap
}

/// `budget` is work_mem (x hash_mem_multiplier for a hash node; 0 = none).
#[inline]
pub fn ceiling(budget: u64, share_cap: u64, max: u64) -> u64 {
    if budget == 0 {
        0
    } else {
        budget.min(share_cap).min(max)
    }
}

/// As PostgreSQL budgets the node: one final table (`one_table`) is one budget `c`; a parallel hash build's shared
/// table or a merge of the participants' runs is (width + 1) x `c` at width >= 2, at most `max`. Serially `c`.
#[inline]
pub fn leader_ceiling(c: u64, width: u16, one_table: bool, max: u64) -> u64 {
    if one_table || width < 2 {
        c
    } else {
        c.saturating_mul(width as u64 + 1).min(max)
    }
}

#[cfg(test)]
mod tests {
    use super::{ceiling, grow_to, leader_ceiling, spills_at_ceiling, start_bytes, work_from_grant, START_BYTES};

    const MIB: u64 = 1 << 20;

    #[test]
    fn todays_region_sizes() {
        assert_eq!(start_bytes(64 << 20, 4096), START_BYTES as u32);
        assert_eq!(start_bytes(1 << 20, 4096), 1 << 20, "never past the share");
        assert_eq!(start_bytes(64 << 20, MIB), 4 << 20, "four phase-1 tables at least");
        assert_eq!(start_bytes(1001, 0), 1000, "whole words");
        assert_eq!((grow_to(8, 3, 100), grow_to(8, 30, 100), grow_to(80, 30, 100)), (16, 30, 100));
        assert!(spills_at_ceiling(9, 8) && !spills_at_ceiling(8, 8));
        assert_eq!((ceiling(0, 1, 1), ceiling(8 * MIB, 4 * MIB, u64::MAX), ceiling(8 * MIB, u64::MAX, 2 * MIB)), (0, 4 * MIB, 2 * MIB));
        assert_eq!(leader_ceiling(8 * MIB, 8, false, u64::MAX), 72 * MIB);
        assert_eq!(leader_ceiling(8 * MIB, 8, true, u64::MAX), 8 * MIB);
        assert_eq!(leader_ceiling(8 * MIB, 1, false, u64::MAX), 8 * MIB);
        assert_eq!(leader_ceiling(8 * MIB, 8, false, 16 * MIB), 16 * MIB);
    }

    /// Grow mode: the start is the estimate (no 64 MiB grant floor), at least one region start, at most the GUCs' grant;
    /// the ceiling with work_mem unset is the region wall held to the pool's big lane, never 0.
    #[test]
    fn grow_mode_starts_at_the_estimate_and_has_no_work_mem_ceiling() {
        use super::{grow_ceiling, grow_want};
        assert_eq!(grow_want(10 * MIB, 1, 1 << 40), 10 * MIB, "the estimate, no 64 MiB floor");
        assert_eq!(grow_want(10 * MIB, 2, 1 << 40), 20 * MIB, "scaled");
        assert_eq!(grow_want(0, 1, 1 << 40), START_BYTES, "at least one region start");
        assert_eq!(grow_want(1 << 30, 1, 64 * MIB), 64 * MIB, "at most the GUCs' grant");
        let wall = u32::MAX as u64;
        assert_eq!(grow_ceiling(0, wall), wall, "unarmed: the region wall");
        assert_eq!(grow_ceiling(24 << 30, wall), wall, "a pool past the wall: the wall");
        assert_eq!(grow_ceiling(1 << 30, wall), 1 << 30, "a small pool: its big lane");
        assert_eq!(grow_ceiling(1, wall), START_BYTES, "never under one start (never 0)");
    }

    #[test]
    fn a_sort_array_from_the_floor_on_is_the_grants() {
        let _one = pgsync::lock(&crate::consts::TEST_LOCK);
        crate::consts::reset();
        assert!(work_from_grant(64 << 10) && !work_from_grant((64 << 10) - 1));
        crate::consts::set(crate::consts::K::WorkFloor, 1 << 20);
        assert!(!work_from_grant(64 << 10) && work_from_grant(1 << 20));
        crate::consts::reset();
    }
}
