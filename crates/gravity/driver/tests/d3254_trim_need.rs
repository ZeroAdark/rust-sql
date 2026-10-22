//! D3254: an admission trims the parked regions it is short of, least recently put first, and none for an ask the pool
//! cannot seat whole (it halves, each half trimming for its own need). Before, every admission that did not fit at once
//! dropped the whole region depot: a shape run back to back under a large work_mem (two 1.92 GiB slot regions and 16
//! worker regions a statement at W16) mapped its regions afresh each run, and the allocator's purge of the dropped
//! ones ran inside the next section open. One test: the pool and the depot are process-wide.

use std::time::Duration;

use gravity_driver::{try_region, Grant, POOL, REGIONS};

const MIB: u64 = 1 << 20;

fn park(bytes: u64) {
    REGIONS.put(try_region(bytes as usize).expect("room in the pool"));
}

#[test]
fn an_admission_trims_only_the_parked_regions_it_needs() {
    POOL.gate(0);
    REGIONS.trim();
    assert_eq!(POOL.held(), 0, "a previous test left bytes held");
    POOL.arm(3072 * MIB); // the region depot keeps a sixth of the pool: 512 MiB
    assert_eq!(POOL.stats().reserve, 0);

    // (1) the last run's two regions parked (400 of 3072 MiB); the next run of its shape asks past the pool, halves to a
    // grant that fits beside them and keeps them: no trim, no eviction, and its draws of those sizes map nothing fresh
    park(200 * MIB);
    park(200 * MIB);
    let (p0, d0) = (POOL.stats(), REGIONS.stats());
    assert_eq!(d0.bytes as u64, 400 * MIB);
    assert_eq!(POOL.held_by_statements(), 0, "a parked region is no statement's");
    let a = POOL.admit_need(Grant::of(4000 * MIB), &|g| g.bytes, Duration::from_secs(5)).expect("admitted");
    assert_eq!(a.grant.bytes, 2000 * MIB, "halved once, beside the parked regions");
    let (p1, d1) = (POOL.stats(), REGIONS.stats());
    assert_eq!((d1.evictions, d1.bytes), (d0.evictions, d0.bytes), "no parked region dropped for an ask past the pool");
    assert_eq!(p1.trims - p0.trims, 1, "the ask past the pool trims the slot depot alone");
    let r1 = REGIONS.take(200 * MIB as usize, None).expect("a parked region");
    let r2 = REGIONS.take(200 * MIB as usize, None).expect("a parked region");
    assert_eq!(REGIONS.stats().allocs, d1.allocs, "both draws served by the parked regions");
    drop(a);
    REGIONS.put(r1);
    REGIONS.put(r2);

    // (2) an ask that fits whole once 128 MiB come back trims the least recently put region alone
    let (p2, d2) = (POOL.stats(), REGIONS.stats());
    assert_eq!((d2.entries, d2.bytes as u64), (2, 400 * MIB));
    let b = POOL.admit_need(Grant::of(2800 * MIB), &|g| g.bytes, Duration::from_secs(5)).expect("admitted");
    assert_eq!(b.grant.bytes, 2800 * MIB, "the whole ask");
    let (p3, d3) = (POOL.stats(), REGIONS.stats());
    assert_eq!((d3.entries, d3.bytes as u64, d3.evictions - d2.evictions), (1, 200 * MIB, 1), "one region dropped for the 128 MiB short");
    assert_eq!(p3.trims - p2.trims, 1);
    drop(b);

    REGIONS.trim();
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}
