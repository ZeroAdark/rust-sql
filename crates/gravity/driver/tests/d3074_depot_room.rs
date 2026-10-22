//! D3074: a take no parked region serves maps its region fresh, and the parked regions stay: a region under the ask
//! is the one the next run of the same plan takes (a repeated group-by's 922 MB worker shares, given back before each run's 1.8 GB
//! slot draw, cost every run its workers' faults: +12 ms). A parked region covering the ask is fitted to it (R1 to R3).
//! One test: the depot is process-wide.

use gravity_driver::{try_region, REGIONS};

const MIB: usize = 1 << 20;

fn put_new(bytes: usize) {
    REGIONS.put(try_region(bytes).expect("unarmed pool"));
}

#[test]
fn a_fresh_draw_keeps_the_parked_regions() {
    REGIONS.trim();

    // two 64 MiB regions parked; a 96 MiB take fits neither, maps fresh and both stay parked
    put_new(64 * MIB);
    put_new(64 * MIB);
    let before = REGIONS.stats().evictions;
    let fresh = REGIONS.take(96 * MIB, None).expect("a fresh region");
    let s = REGIONS.stats();
    assert_eq!((s.entries, s.bytes), (2, 128 * MIB), "a fresh draw gives nothing back");
    assert_eq!(s.evictions, before);
    drop(fresh);
    REGIONS.trim();

    // a 256 MiB region parked; a 1 MiB take maps fresh and leaves it parked (past four times the ask)
    put_new(256 * MIB);
    let small = REGIONS.take(MIB, None).expect("a fresh region");
    let s = REGIONS.stats();
    assert_eq!((s.entries, s.bytes), (1, 256 * MIB), "a small draw keeps a large parked region");
    drop(small);

    // a take the parked region covers is served by it, fitted to the ask
    let hit = REGIONS.take(250 * MIB, None).expect("the parked region");
    assert_eq!((hit.len(), hit.whole()), (250 * MIB, 256 * MIB), "fitted to the ask, its tail kept");
    assert_eq!(REGIONS.stats().entries, 0);
    drop(hit);

    REGIONS.trim();
}
