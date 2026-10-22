//! D187: an unadmitted pull's capacity draws are its own (drawn, `PoolStats::unadmitted*`), not borrows or L1s; a draw with no
//! statement still is both (FAILS before: the pull's draw was an L1 and a borrow).
use gravity_driver::grant_ledger::{counts, Check};
use gravity_driver::{try_region, try_region_in, Grant, Reservation, POOL, REGIONS};

const MIB: u64 = 1 << 20;

#[test]
fn an_unadmitted_pulls_draws_are_its_own_and_not_l1s() {
    std::env::set_var("PGRUST_GRAVITY_LEDGER", "report");
    POOL.gate(0);
    REGIONS.trim();
    POOL.arm(256 * MIB);
    let (l1, s0) = (counts()[Check::L1 as usize], POOL.stats());
    let pull = Reservation::unadmitted(Grant::of(8 * MIB));
    let r = try_region_in(&pull, MIB as usize).expect("the capacity has room");
    let s1 = POOL.stats();
    assert_eq!(counts()[Check::L1 as usize], l1, "a pull's capacity draw is no L1");
    assert_eq!((s1.unadmitted - s0.unadmitted, s1.unadmitted_bytes - s0.unadmitted_bytes), (1, r.capacity() as u64), "counted as the pull's");
    assert_eq!((s1.borrows, s1.borrowed), (s0.borrows, s0.borrowed), "not a borrow");
    assert_eq!(pull.drawn(), r.capacity() as u64, "the pull's own draws");
    drop(r);
    let r = std::thread::spawn(|| try_region(MIB as usize).map(|b| b.capacity())).join().unwrap().expect("capacity");
    assert!(counts()[Check::L1 as usize] > l1, "a draw with no statement stays an L1");
    assert_eq!(POOL.stats().borrowed - s1.borrowed, r as u64, "and a borrow");
    REGIONS.trim();
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}
