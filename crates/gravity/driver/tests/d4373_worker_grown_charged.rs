//! D4373 (a): a parallel worker's draw past W and G (a sort run's in-place growth: kernels `sort_grow` -> driver
//! `region_grow_place` -> `grow_in_place` -> `road_held_now`) is charged on its statement's governor count, the counter
//! the leader's `Reservation::note_grown` charges and the watchdog's cancel and the ladder's R6 report as the statement's
//! charge: memtest cell e2 (work_mem 2GB, W4) held 8194 MiB of sort regions with 4116 MiB charged, and the incident's
//! victim showed 7474 MB resident against 2068 MB charged. It is given back with the hold, and a draw past the
//! statement's memory limit is refused (the sort puts its run).

use std::time::Duration;

use gravity_driver::mem::region::worker_grown_draw;
use gravity_driver::{Grant, POOL, REGIONS};

const MIB: u64 = 1 << 20;

#[test]
fn a_workers_grown_draw_is_charged_on_its_statements_count() {
    POOL.gate(0);
    REGIONS.trim();
    POOL.arm(1 << 30);
    let r = POOL.admit_need_until(Grant::of(64 * MIB), &|g| g.bytes, Duration::from_secs(5), &|| false).expect("admitted");
    mcx::governor::statement_start(0, true);
    let led = mcx::governor::current().expect("the statement's ledger");
    let before = led.bytes();
    // the worker: its claim runs under the leader's ledger (hosts worker_arm.rs `StatementMemory`)
    let (during, after) = std::thread::scope(|s| {
        s.spawn(|| {
            let _in = mcx::governor::enter(Some(led.clone()));
            let held = worker_grown_draw(256 * MIB).expect("the pool has room");
            let during = led.bytes();
            drop(held);
            (during, led.bytes())
        })
        .join()
        .unwrap()
    });
    mcx::governor::statement_end();
    drop(r);
    POOL.arm(0);
    assert_eq!(during - before, (256 * MIB) as isize, "the worker's grown bytes are on the statement's charge");
    assert_eq!(after, before, "and given back with the hold");
}

#[test]
fn a_workers_grown_draw_past_its_statements_limit_is_refused() {
    POOL.gate(0);
    REGIONS.trim();
    POOL.arm(1 << 30);
    mcx::governor::statement_start((64 * MIB) as usize, true);
    let led = mcx::governor::current().expect("the statement's ledger");
    let refused = std::thread::scope(|s| {
        s.spawn(|| {
            let _in = mcx::governor::enter(Some(led.clone()));
            worker_grown_draw(256 * MIB).is_none()
        })
        .join()
        .unwrap()
    });
    mcx::governor::statement_end();
    POOL.arm(0);
    assert!(refused, "a worker's growth past the statement's memory limit is refused: the sort puts its run");
}
