//! planner-62: the phase-2 partition count is capped by the route's partition bound for every record width, so a wide
//! record's partitions stay within a slice as long as a narrow record's do.

use gravity_abi::breaker::T1_MAX_PARTS;
use gravity_driver::partition_plan;
use gravity_driver::sink::partition_bytes;

#[test]
fn wide_records_reach_the_route_bound() {
    let slice = partition_bytes();
    if slice == 0 {
        return; // unpartitioned by override
    }
    let max_bits = T1_MAX_PARTS.trailing_zeros() as u8;
    for record in [8u32, 16, 40, 41, 64, 128, 512] {
        // enough groups that 2 x bytes / slice is far past the bound
        let groups = (slice / record as u64) * (T1_MAX_PARTS as u64) * 4;
        let p = partition_plan(groups, record).expect("past the gate");
        assert_eq!(p.pbits, max_bits, "record {record} B: capped at T1_MAX_PARTS, not a width-picked 1024");
        // below the bound the count is the slice formula for every width
        let groups = (slice / record as u64) * 8;
        let p = partition_plan(groups, record).expect("past the gate");
        assert_eq!(1u64 << p.pbits, 16, "record {record} B: 8 slices of groups -> 16 partitions");
    }
}

/// D08: the route bound is the one format fact behind the owner's run directory and the build sort's partition count,
/// and the one-round owner's layout starts its records at the run directory's end.
#[test]
fn the_route_bound_sizes_the_run_directory_and_the_build_sort() {
    use gravity_abi::breaker::{HASHAGG_HDR, MAX_BUILD_PARTS, OWNERTOP_HDR, OWNER_MAX_RUNS, OWNER_RUN_OFF};
    assert_eq!(OWNER_MAX_RUNS, T1_MAX_PARTS as usize);
    assert_eq!(MAX_BUILD_PARTS, T1_MAX_PARTS as usize);
    assert_eq!(OWNER_RUN_OFF, OWNERTOP_HDR + 8 * T1_MAX_PARTS as usize);
    for rec in [16usize, 40, 128] {
        let half = (HASHAGG_HDR + 2 * rec).next_multiple_of(8);
        assert_eq!(gravity_driver::one_owner_bytes(0, rec), OWNER_RUN_OFF.next_multiple_of(8) + 2 * half, "record {rec} B");
    }
}
