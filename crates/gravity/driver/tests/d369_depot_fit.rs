//! D369 (main 09-24): a charged take reuses the page it parked. `take_parked`'s D225 fit (within an eighth of the ask)
//! compared a parked region against the raw ask, so a 64 B slot region's own 4 KiB page never fit it: every take mapped a
//! new page and parked it, the depot gaining an entry a take (10,234 after 600 `count(*)` statements at W16) and the bind's
//! scan under the depot lock growing with them (open_section's bind 38 -> 64 us over the loop). The fit is against the
//! page-rounded region a fresh take maps; D225's bound stands (a charged take does not take a region past it). One test:
//! it arms the process-wide pool.

use std::time::Duration;

use gravity_driver::{try_region, Grant, POOL, REGIONS, REGION_MIN};

const MIB: u64 = 1 << 20;

#[test]
fn a_charged_sub_page_take_reuses_its_parked_page_and_d225_still_bounds_the_fit() {
    POOL.gate(0);
    REGIONS.trim();
    assert_eq!(POOL.held(), 0, "a previous test left bytes held");
    POOL.arm(64 * MIB);
    let a = POOL.admit(Grant::of(8 * MIB), 1, 2, Duration::from_millis(10)).expect("admitted");
    let base = REGIONS.stats();
    // a slot's program-constant region, taken and parked back once per statement
    for ask in [64usize, REGION_MIN + 1] {
        for i in 0..64 {
            let buf = REGIONS.take(ask, Some(&a)).expect("a region");
            assert!(buf.len() >= ask);
            REGIONS.put(buf);
            let s = REGIONS.stats();
            assert!(s.entries <= base.entries + 2, "ask {ask} take {i}: the depot grew to {} entries (a new page per take)", s.entries);
        }
    }
    let s = REGIONS.stats();
    assert_eq!(s.allocs - base.allocs, 2, "one fresh region per ask size, every later take reused it");
    // D225: a charged take of two pages does not take a parked 64 KiB region (the fresh one is mapped)
    REGIONS.trim();
    REGIONS.put(try_region(64 << 10).expect("capacity"));
    let before = REGIONS.stats();
    let buf = REGIONS.take(REGION_MIN + 1, Some(&a)).expect("a region");
    assert_eq!(buf.len(), 2 * REGION_MIN, "the page-rounded fresh region, not the 64 KiB one");
    assert_eq!(REGIONS.stats().allocs, before.allocs + 1);
    drop(buf);
    REGIONS.trim();
    drop(a);
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}
