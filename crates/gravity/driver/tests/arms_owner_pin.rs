//! crt-0384 (M4 capacity, 4150 hash partitions): an append.arms run's arms all produce one breaker slot and only the run's
//! last arm runs the serial owner fold (D393b, `Node::arm_continues`), so the footprint counts that fold's pin once per run.
//! Counted per arm, 4150 arms pinned 296 GB at the floor grant and the statement refused "the floor need past the pool's
//! capacity" at W0. Pipelines that produce the slot without an Arms run still pin once each, as before.
use gravity_driver_rig as common;


use common::*;
use gravity_abi::breaker::{Breaker, BreakerKind, Budget, KeyKind, RecordLayout, SlotKind, Spill};
use gravity_driver::{footprint, footprint_regions, Grant};
use gravity_program::protos::{Proto, ProtoKind, V4Descs};
use gravity_program::{Arena, Code, End, Layout, OutCol, OwnerDesc, Pipeline, Program, Reason, SiteDesc, WritePlan};

/// `arms` producers of slot 0 (a share-budgeted partitioned hash aggregate, each with its serial owner), then the emit; an
/// Arms proto over them when `run`.
fn program(arms: usize, run: bool) -> Program {
    let st = Arena::new(1 << 14);
    let sites = [SiteDesc::range_source(st.handle(), 3840, &[1, 2])];
    let b = End::Breaker(Breaker { kind: BreakerKind::HashAgg, slot: 0, bytes: 0, budget: Budget::Share, spill: Spill::Partitioned { bits: 3 } });
    let layout = RecordLayout::new(&[KeyKind::I32], &[SlotKind::Count]);
    let mut pipes: Vec<(Pipeline, Code)> = (0..arms).map(|_| (Pipeline { owner: Some(OwnerDesc::generic(layout, 0, 0)), ..Pipeline::eligible(0, b, 1000) }, Code::hand(k_range_group_spill))).collect(); // alloc: Cold
    pipes.push((Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming_one(0), Code::hand(k_emit_groups_spill)));
    let mut p = Program::with_pipelines(Layout { nsites: 1, out_natts: 3, out_rows: 64, scratch_bytes: 0, exec_bytes: 0 }, &sites, &core::array::from_fn::<_, 3, _>(|i| OutCol::new(["c0", "c1", "c2"][i], 20, -1, 8, true)), WritePlan::SELECT, pipes);
    if run {
        p.set_v4(&V4Descs::new(vec![Proto::new(ProtoKind::Arms, 0).slot(0).arg(arms as u64)])); // alloc: Cold
    }
    p
}

#[test]
fn an_arms_run_pins_its_owner_fold_once() {
    let grant = Grant::of(64 << 20);
    let one = footprint(grant, &program(1, true), 1);
    for arms in [2usize, 64, 4150] {
        let (with_run, without) = (program(arms, true), program(arms, false));
        assert_eq!(footprint_regions(grant, &with_run, 1), footprint_regions(grant, &program(1, true), 1), "{arms} arms: one slot's region");
        assert_eq!(footprint(grant, &with_run, 1), one, "{arms} arms in one run: the last arm's fold pins once");
        assert!(footprint(grant, &without, 1) > one, "{arms} producers with no run: each owner still pins");
    }
}

/// D4615: `footprint` over its scratch context returns the bytes it returned over the global heap (values taken on trunk
/// 76c757bf60e before the change).
#[test]
fn footprint_bytes_are_pinned() {
    let grant = Grant::of(64 << 20);
    let mut got = Vec::new();
    for (arms, run) in [(1usize, true), (2, true), (64, true), (64, false), (4150, true), (4150, false)] {
        for width in [1u16, 4] {
            got.push(((arms, run, width), footprint(grant, &program(arms, run), width), footprint_regions(grant, &program(arms, run), width)));
        }
    }
    let want: &[((usize, bool, u16), u64, u64)] = &[
        ((1, true, 1), 209715200, 67108864),
        ((1, true, 4), 150994944, 134217728),
        ((2, true, 1), 209715200, 67108864),
        ((2, true, 4), 150994944, 134217728),
        ((64, true, 1), 209715200, 67108864),
        ((64, true, 4), 150994944, 134217728),
        ((64, false, 1), 8929673216, 67108864),
        ((64, false, 4), 150994944, 134217728),
        ((4150, true, 1), 209715200, 67108864),
        ((4150, true, 4), 150994944, 134217728),
        ((4150, false, 1), 574481235968, 67108864),
        ((4150, false, 4), 150994944, 134217728),
    ];
    assert_eq!(got, want);
}
