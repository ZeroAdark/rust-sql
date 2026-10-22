//! D405 (PREFAULT-ATTR 09-24): over the grant pool's park budget `RegionDepot::put` dropped the region being returned and
//! kept the older parked ones, so after other large-grant statements q28's own sizes were thrown away at every put (6,042 of
//! 16,434 puts; fresh region bytes 37.6 -> 86.6 GiB over 7 statements, the fresh THP regions faulting through direct
//! compaction). A put now evicts the least recently put regions down to the budget and parks the returned one; it still
//! drops the returned one (depot untouched) when the live bytes alone hold the pool over the budget. One test: it arms the
//! process-wide pool.

use gravity_driver::{try_region, POOL, REGIONS};

const MIB: usize = 1 << 20;

/// Every parked region's address, emptying the depot (each taken region is dropped).
fn drain() -> Vec<usize> {
    let mut out = Vec::new();
    while let Some(buf) = REGIONS.take_parked(4 * MIB, None, None) {
        out.push(buf.as_ptr() as usize);
    }
    out
}

#[test]
fn over_the_park_budget_a_put_evicts_the_least_recently_put_region_not_itself() {
    POOL.gate(0);
    REGIONS.trim();
    assert_eq!(POOL.held(), 0, "a previous test left bytes held");
    // DR-18: the depot holds a sixth of the pool: 192 MiB parks 32 MiB, eight 4 MiB regions
    POOL.arm(192 * MIB as u64);
    // an earlier statement's regions: 8 x 4 MiB parked, at the depot's cap, not over it
    let old: Vec<usize> = (0..8)
        .map(|_| {
            let buf = try_region(4 * MIB).expect("capacity");
            let p = buf.as_ptr() as usize;
            REGIONS.put(buf);
            p
        })
        .collect();
    assert_eq!(REGIONS.stats().entries, 8);
    assert!(!POOL.over_park_budget());
    // the running statement's region comes back: the depot is over its cap with it
    let fresh = try_region(4 * MIB).expect("capacity");
    let fresh_ptr = fresh.as_ptr() as usize;
    let before = REGIONS.stats();
    REGIONS.put(fresh);
    let after = REGIONS.stats();
    assert_eq!(after.puts, before.puts + 1, "the returned region is parked, not dropped");
    assert_eq!(after.entries, 8, "one older region made room");
    assert_eq!(after.evictions, before.evictions + 1);
    assert!(!POOL.over_park_budget(), "the park budget stays bounded");
    let parked = drain();
    assert!(parked.contains(&fresh_ptr), "the running statement's region is parked");
    assert!(!parked.contains(&old[0]), "the least recently put region was the one evicted");
    assert!(old[1..].iter().all(|p| parked.contains(p)), "the other older regions stay parked");
    assert_eq!(POOL.held(), 0);

    // does not apply: the live bytes alone hold the pool over the budget, so no eviction can make room; the depot is left
    // alone and the returned region drops, as before
    for _ in 0..2 {
        REGIONS.put(try_region(4 * MIB).expect("capacity"));
    }
    let live = try_region(96 * MIB).expect("capacity");
    let back = try_region(4 * MIB).expect("capacity");
    let before = REGIONS.stats();
    REGIONS.put(back);
    let after = REGIONS.stats();
    assert_eq!((after.entries, after.puts), (before.entries, before.puts), "the depot is untouched");
    assert_eq!(after.evictions, before.evictions + 1, "the returned region is the one dropped");
    drop(live);
    REGIONS.trim();
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}
