//! D4920: a hashed IN list's set the Bind builds before any statement exists (`in_list_set_region`) is held on the pool as
//! a draw with no statement is (D4403's small buffer first, counted with those draws) and given back when it drops, with
//! no L1: under PGRUST_GRAVITY_LEDGER=abort a statement with such a list panicked its backend at admission. Every other
//! draw with no statement (`try_region`) is still an L1 and still panics under abort. One test per process: the mode is
//! read once.
use gravity_driver::grant_ledger::{counts, mode, Check, Mode};
use gravity_driver::{in_list_set_region, try_region, POOL, REGIONS};

const MIB: u64 = 1 << 20;
const SET: usize = 408;

#[test]
fn an_in_list_set_is_no_l1_and_every_other_draw_with_no_statement_still_is() {
    std::env::set_var("PGRUST_GRAVITY_LEDGER", "abort");
    assert_eq!(mode(), Mode::Abort);
    POOL.gate(0);
    REGIONS.trim();
    POOL.arm(256 * MIB);
    POOL.set_buffer(16 * MIB);
    assert!(POOL.reserve() > 0, "the small buffer is set");
    let (l1, before) = (counts()[Check::L1 as usize], POOL.stats());
    let set = std::thread::spawn(|| in_list_set_region(SET).map(|r| (r.len(), POOL.held(), POOL.small_held()))).join().expect("no panic under abort");
    assert_eq!(set.ok(), Some((SET, before.held + SET as u64, before.small_held + SET as u64)), "held on the pool, from the small buffer");
    let after = POOL.stats();
    assert_eq!(counts()[Check::L1 as usize], l1, "no L1");
    assert_eq!((after.small_draws - before.small_draws, after.borrows - before.borrows, after.borrowed - before.borrowed), (1, 1, SET as u64), "a draw with no statement's road and counts");
    REGIONS.trim();
    assert_eq!((POOL.held(), POOL.small_held()), (before.held, before.small_held), "the set's bytes back once it dropped");
    let other = std::thread::spawn(|| try_region(SET).map(|_| ())).join();
    assert!(other.is_err(), "any other draw with no statement still panics under abort");
    assert_eq!(counts()[Check::L1 as usize], l1 + 1, "as an L1");
    REGIONS.trim();
    assert_eq!((POOL.held(), POOL.small_held()), (before.held, before.small_held), "the refused draw took nothing");
    POOL.set_buffer(0);
    POOL.arm(0);
}
