//! R4 ledger probes (refactor-ledger.md §4.5): under PGRUST_GRAVITY_LEDGER=report each reproduced finding is counted, and behaviour is unchanged.
use std::time::Duration;

use gravity_driver::grant_ledger::{counts, mode, Check, Mode};
use gravity_driver::{try_region, try_region_in, Grant, POOL, REGIONS};

const MIB: u64 = 1 << 20;

/// One test per process: the mode is read once. A draw with no statement (D187's worker grow) and a draw past a statement's
/// reservation are L1s; two reservations ended in either order (D188) give every byte and slot back and report nothing.
#[test]
fn the_probes_count_the_findings_and_nothing_else() {
    std::env::set_var("PGRUST_GRAVITY_LEDGER", "report");
    assert_eq!(mode(), Mode::Report);
    POOL.gate(0);
    REGIONS.trim();
    POOL.arm(256 * MIB);
    let want = Grant::of(8 * MIB);
    let before = counts();
    for first_in_first_out in [false, true] {
        let a = POOL.admit(want, 1, 1, Duration::from_millis(10)).unwrap();
        let b = POOL.admit(want, 1, 1, Duration::from_millis(10)).unwrap();
        assert!(a.holds_slot() && !b.holds_slot(), "one in-flight slot per thread");
        if first_in_first_out {
            drop(a);
            drop(b);
        } else {
            drop(b);
            drop(a);
        }
        assert_eq!((POOL.held(), POOL.inflight()), (0, 0), "D188: every byte and the slot back in either order");
    }
    assert_eq!(counts(), before, "leases in either order are clean");
    let l1 = counts()[Check::L1 as usize];
    let r = std::thread::spawn(|| try_region(MIB as usize).map(|_| ())).join().unwrap();
    assert!(r.is_ok(), "behaviour unchanged: the draw is taken from capacity");
    assert!(counts()[Check::L1 as usize] > l1, "D187: a draw with no statement is reported");
    let s = POOL.admit(want, 1, 1, Duration::from_millis(10)).unwrap();
    let l1 = counts()[Check::L1 as usize];
    assert!(try_region_in(&s, (s.reserved + MIB) as usize).is_err(), "S7: refused past the reservation");
    assert!(counts()[Check::L1 as usize] > l1, "a draw past the statement's reservation is reported");
    assert_eq!(s.left(), s.reserved, "the refusal took nothing");
    drop(s);
    REGIONS.trim();
    assert_eq!(POOL.held(), 0);
    POOL.arm(0);
}
