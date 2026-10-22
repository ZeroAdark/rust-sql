//! D3074 / D3089: a parked region larger than a take's ask (past D225's eighth, within four times it) serves the take
//! instead of a fresh draw, fitted to the ask: the taker (and the pool's settled hold) is charged the ask, never the
//! region's size, and the rest is an uncharged tail. A later take that grows the region's length into that tail is
//! charged the grown length before the region is handed out. One test: it arms the process-wide pool.

use gravity_driver::{try_region, POOL, REGIONS};

const MIB: usize = 1 << 20;

#[test]
fn a_larger_parked_region_serves_the_ask_and_a_growth_is_charged_first() {
    POOL.gate(0);
    REGIONS.trim();
    assert_eq!(POOL.held(), 0, "a previous test left bytes held");
    POOL.arm(1024 * MIB as u64);

    REGIONS.put(try_region(64 * MIB).expect("capacity"));
    assert_eq!(POOL.held(), 64 * MIB as u64);

    // R2: 64 MiB is past an eighth over 40 MiB, within four times it: taken and fitted, charged 40 MiB
    let allocs = REGIONS.stats().allocs;
    let a = REGIONS.take(40 * MIB, None).expect("the parked region");
    assert_eq!(REGIONS.stats().allocs, allocs, "no fresh draw");
    assert_eq!((a.len(), a.whole()), (40 * MIB, 64 * MIB));
    assert_eq!(a.pool_bytes(), 40 * MIB as u64, "the region holds its ask");
    assert_eq!(POOL.held(), 40 * MIB as u64, "the pool holds the ask, not the allocation");
    let base = a.as_ptr() as usize;
    REGIONS.put(a);
    assert_eq!(REGIONS.stats().bytes, 40 * MIB, "parked at the bytes it holds");

    // R1: 60 MiB fits the 64 MiB allocation within an eighth; its length grows into the tail, charged before it is used
    let b = REGIONS.take(60 * MIB, None).expect("the parked region");
    assert_eq!(b.as_ptr() as usize, base);
    assert_eq!((b.len(), b.whole()), (60 * MIB, 64 * MIB));
    assert_eq!(b.pool_bytes(), 60 * MIB as u64, "the growth is held before the region is handed out");
    assert_eq!(POOL.held(), 60 * MIB as u64);
    drop(b);
    assert_eq!(POOL.held(), 0);

    // past four times the ask: a fresh draw, the large region stays parked
    REGIONS.put(try_region(64 * MIB).expect("capacity"));
    let small = REGIONS.take(8 * MIB, None).expect("a fresh region");
    assert_eq!((small.len(), small.whole()), (8 * MIB, 8 * MIB));
    assert_eq!(REGIONS.stats().entries, 1);
    drop(small);
    REGIONS.trim();
    assert_eq!(POOL.held(), 0);
}
