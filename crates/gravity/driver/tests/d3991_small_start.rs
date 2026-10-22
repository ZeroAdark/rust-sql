//! D3991: under the projection rule decide(Start) holds a top-level start while F < F* and relief is landing. Holding a
//! start whose need is within the grant floor frees nothing the projection is short of, so it is admitted at once; a
//! larger need still waits.

use std::time::{Duration, Instant};

use gravity_driver::{Grant, GRANT_FLOOR, POOL, REGIONS};
use mempolicy::decide::{Rise99, Rule};

const MIB: u64 = 1 << 20;

fn admit(bytes: u64) -> Duration {
    let t0 = Instant::now();
    let r = POOL.admit_need_until(Grant::of(bytes), &|g| g.bytes, Duration::from_secs(5), &|| false).expect("admitted");
    let took = t0.elapsed();
    drop(r);
    took
}

#[test]
fn a_start_within_the_grant_floor_does_not_wait_on_the_projection() {
    POOL.gate(0);
    REGIONS.trim();
    POOL.arm(1 << 30);
    let rise = |bytes| Rise99 { bytes, prior: false };
    mempolicy::decide::set_rule(Rule::Projection);
    mcx::governor::publish(0, 0, mcx::global_footprint::bytes());
    mcx::governor::publish_decide(&mcx::governor::DecidePublish { l: 4 << 30, b: 64 * MIB, u: MIB, fast: 0, rise_fast: rise(MIB), rise_slow: rise(16 * MIB), revoke_on: false });
    // R falls 10 MiB in a tick against F* - F = 13 MiB short: relief is landing
    mcx::governor::publish_decide_resident((70 * MIB) as usize, mcx::global_footprint::bytes());
    mcx::governor::decide_tick().expect("projection decides at Tick");
    mcx::governor::publish_decide_resident((60 * MIB) as usize, mcx::global_footprint::bytes());
    let t = mcx::governor::decide_tick().expect("projection decides at Tick");
    assert!(t.inputs.falling && mcx::governor::throttled(), "the starts are throttled");

    let small = admit(GRANT_FLOOR / 2);
    let big = admit(4 * GRANT_FLOOR);

    mempolicy::decide::set_rule(Rule::Threshold);
    assert!(mcx::governor::decide_tick().is_none() && !mcx::governor::throttled());
    mcx::governor::publish_decide(&mcx::governor::DecidePublish::default());
    mcx::governor::publish_decide_resident(0, mcx::global_footprint::bytes());
    POOL.arm(0);
    assert!(small < Duration::from_millis(500), "a need within the grant floor waited {small:?}");
    assert!(big >= Duration::from_millis(1_500), "a need past the grant floor waits for the relief: {big:?}");
}
