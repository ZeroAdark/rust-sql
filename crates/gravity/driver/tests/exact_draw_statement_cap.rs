//! The owner phase's exact draw (`commit_exact`: `owners_in_place` sizes a laid leader region to the owners' counted need)
//! was the one road past G that never asked the statement's part of the pool (`pgrust.gravity_statement_pool_fraction`,
//! D2990): every other road does. It asks it once now; refused, nothing is drawn and the owner phase takes its own road
//! (its owners' mapped temp file), never an error.
//!
//! D4151: each witness's region drops inside it, and a laid one past `reap::REAP_MIN` goes to the reaper, which gives its
//! pool charge back after the unmap, off this thread; the test drains the reaper before it reads `POOL.held()`.

use std::time::Duration;

use gravity_driver::mem::grow::exact_commit_witness;
use gravity_driver::mem::reap;
use gravity_driver::{Grant, POOL, REGIONS};

const MIB: u64 = 1 << 20;

#[test]
fn an_exact_draw_past_the_statements_part_of_the_pool_is_refused() {
    POOL.gate(0);
    REGIONS.trim();
    POOL.arm(1 << 30);
    let r = POOL.admit_need_until(Grant::of(64 * MIB), &|g| g.bytes, Duration::from_secs(5), &|| false).expect("admitted");
    // 1 % of the lane, under the granted floor: the bound is G + 64 MiB
    r.set_pool_fraction(10_000);
    assert_eq!(r.grown(), 0);
    // a 2 MiB start from G, then 63 MiB past G's 62 MiB left: within the bound
    assert!(exact_commit_witness(2 * MIB as usize, 512 * MIB as usize, 65 * MIB, &r), "an exact draw within the statement's part");
    reap::drain();
    let grown = r.grown();
    assert_eq!(grown, 63 * MIB);
    let held = POOL.held();
    assert!(!exact_commit_witness(2 * MIB as usize, 512 * MIB as usize, 400 * MIB, &r), "past it: refused");
    reap::drain();
    assert_eq!((r.grown(), POOL.held()), (grown, held), "nothing drawn: the owner phase takes its own road");
    drop(r);
    POOL.arm(0);
}
