//! M8 (RT-2e, D1430 / D1431): a rerun set keeps the slots its members produce and consume for the statement (exec
//! `Node::defer_rerun`, `free_after` skips them): a member re-runs inside its owner's pipeline, after its last consumer in
//! program order. The footprint released them there, so the owner's breaker drew past the reservation (main 22:21Z: a kept
//! VALUES build's share beside the owner's Sort share, a fresh zeroed region and a ledger L1 each statement at W8).
use gravity_driver_rig as common;


use common::*;
use gravity_abi::breaker::{Breaker, BreakerKind, Budget, Spill};
use gravity_driver::park::region_bytes;
use gravity_driver::{footprint, footprint_regions, share, Grant};
use gravity_program::protos::{Proto, ProtoKind, V4Descs};
use gravity_program::{Arena, Code, End, Layout, OutCol, Pipeline, Program, Reason, SiteDesc, WritePlan};

/// A build of slot 0; its reader (slot 1); the reader's reader (slot 2); the emit. With `rerun`, pipeline 1 is a rerun
/// member whose owner is pipeline 2.
fn program(rerun: bool) -> Program {
    let st = Arena::new(1 << 14);
    let sites = [SiteDesc::range_source(st.handle(), 3840, &[1, 2])];
    let b = |kind, slot| End::Breaker(Breaker { kind, slot, bytes: 0, budget: Budget::Share, spill: Spill::Partitioned { bits: 3 } });
    let pipes = vec![
        (Pipeline::eligible(0, b(BreakerKind::HashBuild, 0), 1000), Code::hand(k_range_group_spill)),
        (Pipeline::eligible(0, b(BreakerKind::HashAgg, 1), 1000).consuming_one(0), Code::hand(k_range_group_spill)),
        (Pipeline::eligible(0, b(BreakerKind::HashAgg, 2), 1000).consuming_one(1), Code::hand(k_range_group_spill)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming_one(2), Code::hand(k_emit_groups_spill)),
    ]; // alloc: Cold
    let mut p = Program::with_pipelines(Layout { nsites: 1, out_natts: 3, out_rows: 64, scratch_bytes: 0, exec_bytes: 0 }, &sites, &core::array::from_fn::<_, 3, _>(|i| OutCol::new(["c0", "c1", "c2"][i], 20, -1, 8, true)), WritePlan::SELECT, pipes);
    if rerun {
        p.set_v4(&V4Descs::new(vec![Proto::new(ProtoKind::Rerun, 1).peer(2).words(0, 1)])); // alloc: Cold
    }
    p
}

#[test]
fn a_rerun_member_keeps_its_slots_through_the_owner() {
    for (grant, width) in [(Grant::of(36 << 20), 1u16), (Grant::of(36 << 20), 8), (Grant::of(8 << 20), 1)] {
        let (plain, kept) = (program(false), program(true));
        // without the set, slot 0 frees after its reader (pipeline 1), so the owner's breaker opens beside slot 1 alone; the
        // member keeps slot 0, so its region (opened at its own split, as the leader's) is live there too
        let split = share(&kept, 0, grant, width);
        let build = region_bytes(split.breaker.bytes as usize) as u64 + split.images.bytes as u64;
        assert_eq!(footprint_regions(grant, &kept, width), footprint_regions(grant, &plain, width) + build, "W{width}: the kept build beside the owner's breaker");
        assert!(footprint(grant, &kept, width) >= footprint_regions(grant, &kept, width));
    }
}
