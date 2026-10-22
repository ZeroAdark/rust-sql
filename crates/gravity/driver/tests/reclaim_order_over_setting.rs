//! The cache reclaim order: every cache's bytes past its own setting go first, then the fixed list with the part cache
//! ahead of the condition cache and the fp tables. memtest's C7 LIKE-scan case: the part cache 5.7x past its 1290 MiB setting
//! beside a full condition cache, and the cache rung emptied the condition cache first (it came earlier in the list).

use std::sync::atomic::{AtomicU64, Ordering};

use gravity_driver::mem::budget::{self, Consumer, Hooks};
use gravity_driver::mem::reclaim;

const MIB: u64 = 1 << 20;

static PART: AtomicU64 = AtomicU64::new(0);
static COND: AtomicU64 = AtomicU64::new(0);

fn part() -> u64 {
    PART.load(Ordering::Relaxed)
}
fn part_floor() -> u64 {
    100 * MIB
}
fn part_cap() -> u64 {
    1290 * MIB
}
fn part_trim(to: u64) -> u64 {
    let (cur, to) = (part(), to.max(part_floor()));
    PART.store(cur.min(to), Ordering::Relaxed);
    cur.saturating_sub(to)
}
fn cond() -> u64 {
    COND.load(Ordering::Relaxed)
}
fn cond_floor() -> u64 {
    0
}
fn cond_cap() -> u64 {
    1024 * MIB
}
fn cond_trim(to: u64) -> u64 {
    let cur = cond();
    COND.store(cur.min(to), Ordering::Relaxed);
    cur.saturating_sub(to)
}

#[test]
fn a_cache_past_its_setting_goes_before_a_full_one() {
    (PART.store(7400 * MIB, Ordering::Relaxed), COND.store(1024 * MIB, Ordering::Relaxed));
    assert!(budget::register(Consumer::PartCache, Hooks { current: part, floor: part_floor, cap: part_cap, reclaim: Some(part_trim) }));
    assert!(budget::register(Consumer::CondCache, Hooks { current: cond, floor: cond_floor, cap: cond_cap, reclaim: Some(cond_trim) }));
    assert_eq!(reclaim::reclaim_backstop(3000 * MIB), 3000 * MIB);
    assert_eq!((part(), cond()), (4400 * MIB, 1024 * MIB), "the part cache's excess first, the condition cache untouched");
    assert_eq!(reclaim::reclaim_backstop(4300 * MIB), 4300 * MIB);
    assert_eq!((part(), cond()), (100 * MIB, 1024 * MIB), "past its excess the part cache still goes ahead, to its floor");
    assert_eq!(reclaim::reclaim_backstop(MIB), MIB, "then the condition cache");
    assert_eq!(cond(), 1023 * MIB);
}
