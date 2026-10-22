//! D3982: a statement's part of the pool (`pgrust.gravity_statement_pool_fraction`, D2990) bounds its parallel workers'
//! draws past W and G as it bounds its leader's. A worker has no statement count: its draws show on its leader's ledger
//! account, which the cap reads.

use std::time::Duration;

use gravity_driver::mem::region::worker_draw_past_cap;
use gravity_driver::{Grant, POOL, REGIONS};

const MIB: u64 = 1 << 20;

#[test]
fn a_workers_draw_past_its_statements_part_of_the_pool_is_refused() {
    POOL.gate(0);
    REGIONS.trim();
    POOL.arm(1 << 30);
    let r = POOL.admit_need_until(Grant::of(64 * MIB), &|g| g.bytes, Duration::from_secs(5), &|| false).expect("admitted");
    r.set_pool_fraction(250_000);
    let cap = POOL.statement_cap(250_000);
    let a = memledger::current();
    let held = memledger::pool_reserved(a) as u64;
    assert!(cap > held + 128 * MIB, "a quarter of the lane is past the granted floor ({cap} vs {held})");
    assert!(!worker_draw_past_cap(cap - held), "a draw that keeps the statement within its part");
    // the workers' draws so far, as `Held::grown_worker` shows them on the leader's account
    let (owner, release) = memledger::charge::context();
    let shown = memledger::Charge::reserve(owner, release, memledger::Kind::Region, memledger::charge::Door::Heap, cap - held);
    let before = gravity_driver::mem::grant::worker_cap_refused();
    let refused = worker_draw_past_cap(MIB);
    let counted = gravity_driver::mem::grant::worker_cap_refused();
    drop(shown);
    drop(r);
    let after = worker_draw_past_cap(1 << 40);
    POOL.arm(0);
    assert!(refused, "a worker's draw past its statement's part of the pool is refused");
    assert_eq!((counted.0 - before.0, counted.1 - before.1), (1, MIB), "and counted (the grant end line's worker cap refusals)");
    assert!(!after, "the statement ended: no cap");
}
