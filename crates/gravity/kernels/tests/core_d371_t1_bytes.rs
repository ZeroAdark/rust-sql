//! D371: `t1_bytes` sizes the T1 at `groups × ADAPT_FACTOR` rounded up to a power of two. Past 2^60 groups the product is
//! above 2^63 and has no next power of two: `next_power_of_two` panicked in debug builds (inside `agg_open`, an extern "C"
//! host) and wrapped to 0 in release, where the 1024-slot floor opened the smallest table for the largest estimate.
use gravity_abi::breaker::t1_default_bytes;
use gravity_kernels::host_abi::t1_bytes;

#[test]
fn an_estimate_past_2_60_groups_sizes_the_default_table() {
    for groups in [(1u64 << 60) + 1, 1 << 61, (1 << 61) - 1, u64::MAX] {
        assert_eq!(t1_bytes(groups, 16), t1_default_bytes(), "groups {groups}");
    }
    assert_eq!(t1_bytes(1 << 60, 16), t1_default_bytes(), "2^60 groups: 2^63 slots, capped at the default's bytes");
}
