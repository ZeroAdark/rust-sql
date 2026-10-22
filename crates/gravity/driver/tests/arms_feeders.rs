//! M3-MERGE (b): an append.arms run whose arms are interleaved with feeders, `P0 E1 P2`: the `Arms` proto spans the three
//! pipelines (`arg` counts pipelines), the first and last produce the slot. The feeder E1 produces another slot and is no
//! arm (it neither continues nor folds on), each producer is one; a run whose last pipeline is a feeder is no run.
use gravity_driver_rig as common;

use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use common::*;
use gravity_driver::{install_spill_factory, Entry, Grant};
use gravity_abi::breaker::{Breaker, BreakerKind, Budget, Spill};
use gravity_program::protos::{Proto, ProtoKind, V4Descs};
use gravity_program::{Arena, Code, End, Generation, Handle, Layout, OutCol, Pipeline, PlanKey, Program, Reason, SiteDesc, WritePlan};

fn interleaved(arg: u64) -> Program {
    let st = Arena::new(1 << 14);
    let sites = [SiteDesc::range_source(st.handle(), 2001, &[1, 2])];
    let b = |slot| End::Breaker(Breaker { kind: BreakerKind::HashAgg, slot, bytes: 0, budget: Budget::Share, spill: Spill::Partitioned { bits: 3 } });
    let pipes = [
        (Pipeline::eligible(0, b(0), 1000), Code::hand(k_range_group_spill)),
        (Pipeline::eligible(0, b(1), 1000), Code::hand(k_range_group_spill)),
        (Pipeline::eligible(0, b(0), 1000).consuming_one(1), Code::hand(k_range_group_spill)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming_one(0), Code::hand(k_emit_groups_spill)),
    ];
    let mut p = Program::with_pipelines(Layout { nsites: 1, out_natts: 3, out_rows: 64, scratch_bytes: 0, exec_bytes: 0 }, &sites, &core::array::from_fn::<_, 3, _>(|i| OutCol::new(["c0", "c1", "c2"][i], 20, -1, 8, true)), WritePlan::SELECT, pipes);
    p.set_v4(&V4Descs::new(vec![Proto::new(ProtoKind::Arms, 0).slot(0).arg(arg)])); // alloc: Cold
    p
}

fn classes(arg: u64) -> Vec<(bool, bool)> {
    install_spill_factory(&MEM_SPILL);
    let program = Arc::new(interleaved(arg));
    let mut derive = FakeDerive::ranges(RangeTable::new(4, 4));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let params = [7u64, 3];
    let mut b = binding(&params, &[0, 0], &mut sink, &flag);
    let mut exec = entry.start(Some(PlanKey { handle: Handle(12), generation: Generation(arg as u32) }), &program, &mut derive, &mut b, lease(Grant::of(64 << 20))).unwrap();
    let node = exec.node_mut();
    let out = (0..4).map(|p| (node.in_arm_run(p), node.arm_continues(p))).collect(); // alloc: Cold
    entry.end(exec, false);
    out
}

#[test]
fn a_feeder_between_two_arms_runs_untouched() {
    assert_eq!(classes(3), [(true, true), (false, false), (true, false), (false, false)]);
}

#[test]
fn a_run_ending_in_a_feeder_is_no_run() {
    assert_eq!(classes(2), [(false, false); 4]);
}

/// D3918: a probe's run `P1 P2` into slot 0, `P2` its pair pass (its claims the spill partitions of the build's slot 1).
fn paired(kind: BreakerKind) -> Program {
    let st = Arena::new(1 << 14);
    let sites = [SiteDesc::range_source(st.handle(), 2001, &[1, 2])];
    let b = |kind, slot| End::Breaker(Breaker { kind, slot, bytes: 0, budget: Budget::Share, spill: Spill::Partitioned { bits: 3 } });
    let pipes = [
        (Pipeline::eligible(0, b(BreakerKind::HashBuild, 1), 1000), Code::hand(k_range_group_spill)),
        (Pipeline::eligible(0, b(kind, 0), 1000).consuming_one(1), Code::hand(k_range_group_spill)),
        (Pipeline::eligible(0, b(kind, 0), 1000).consuming_one(1).over_spill_partitions(1), Code::hand(k_range_group_spill)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming_one(0), Code::hand(k_emit_groups_spill)),
    ];
    let mut p = Program::with_pipelines(Layout { nsites: 1, out_natts: 3, out_rows: 64, scratch_bytes: 0, exec_bytes: 0 }, &sites, &core::array::from_fn::<_, 3, _>(|i| OutCol::new(["c0", "c1", "c2"][i], 20, -1, 8, true)), WritePlan::SELECT, pipes);
    p.set_v4(&V4Descs::new(vec![Proto::new(ProtoKind::Arms, 1).slot(0).arg(2)])); // alloc: Cold
    p
}

/// Each pipeline's (continues, ended) for `paired(kind)`, its build's slot never spilled.
fn paired_classes(kind: BreakerKind) -> Vec<(bool, bool)> {
    paired_node(kind, |node| (0..4).map(|p| (node.arm_continues(p), node.arm_ended(p))).collect()) // alloc: Cold
}

/// D4054: for each pipeline `of` of `paired(kind)`, the pipelines that finish its production (`Node::finishes`).
fn paired_finishers(kind: BreakerKind) -> Vec<Vec<usize>> {
    paired_node(kind, |node| (0..4).map(|of| (0..4).filter(|&p| node.finishes(of, p)).collect()).collect()) // alloc: Cold
}

/// `f` over a started `paired(kind)`'s node, its build's slot never spilled.
fn paired_node<T>(kind: BreakerKind, f: impl FnOnce(&gravity_driver::Node) -> T) -> T {
    install_spill_factory(&MEM_SPILL);
    let program = Arc::new(paired(kind));
    let mut derive = FakeDerive::ranges(RangeTable::new(4, 4));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let params = [7u64, 3];
    let mut b = binding(&params, &[0, 0], &mut sink, &flag);
    let mut exec = entry.start(Some(PlanKey { handle: Handle(13), generation: Generation(kind as u32) }), &program, &mut derive, &mut b, lease(Grant::of(64 << 20))).unwrap();
    let out = f(exec.node());
    entry.end(exec, false);
    out
}

/// D3918 / D4072 / D4054: under a sort, a hash aggregate or a hash build the run ends at the probe's arm while its pair pass
/// has no partition (the probe owns at its section's width, finishes and seals the run; the pair pass is stepped past). Any
/// other end's run is untouched (its pair arm runs, the probe's arm continues).
#[test]
fn a_sort_run_ends_at_its_first_arm_when_the_pair_pass_has_no_partition() {
    let (ended, untouched) = ([(false, false), (false, false), (false, true), (false, false)], [(false, false), (true, false), (false, false), (false, false)]);
    assert_eq!(paired_classes(BreakerKind::Sort), ended);
    assert_eq!(paired_classes(BreakerKind::HashAgg), ended, "D4072: a keyed end's empty pair pass no longer holds its run");
    assert_eq!(paired_classes(BreakerKind::HashBuild), ended, "D4054: a build's empty pair pass no longer holds its run");
    assert_eq!(paired_classes(BreakerKind::PlainAgg), untouched);
}

/// D4054: one pipeline finishes a production, whichever of its pipelines is asked for: a lone producer itself; of a run
/// its first arm when it ended there (the stepped-past pair pass finishes nothing), else its last. The emit produces no
/// slot and finishes only itself.
#[test]
fn one_arm_finishes_a_run_its_first_when_it_ended_there_else_its_last() {
    let at = |arm: usize| vec![vec![0], vec![arm], vec![arm], vec![3]]; // alloc: Cold
    assert_eq!(paired_finishers(BreakerKind::HashBuild), at(1), "the run ended at its first arm");
    assert_eq!(paired_finishers(BreakerKind::Sort), at(1));
    assert_eq!(paired_finishers(BreakerKind::PlainAgg), at(2), "a run that does not end early: its last arm");
}
