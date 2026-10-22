//! D559: one statement can hold two append.arms runs on ONE slot number (a slot's number is reused once its life ends:
//! D555's MergeAppend over a Group's input sort). `P0 P1 | P2 | P3 P4 | P5`: run A (P0, P1) produces slot 0, P2 consumes it
//! (its life ends) and produces slot 1; run B (P3, P4) produces slot 0 again, P5 consumes both. Each run's arms are arms of
//! THEIR run: `arms_of` takes the run whose range holds the pipeline, never the first `Arms` proto naming the number (run
//! A's range holds no arm of run B, so B's arms lost D393's continue / fold-on / last-consumer handling).
use gravity_driver_rig as common;

use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use common::*;
use gravity_abi::breaker::{Breaker, BreakerKind, Budget, Spill};
use gravity_driver::{install_spill_factory, Entry, Grant};
use gravity_program::protos::{Proto, ProtoKind, V4Descs};
use gravity_program::{Arena, Code, End, Generation, Handle, Layout, OutCol, Pipeline, PlanKey, Program, Reason, SiteDesc, WritePlan};

fn two_runs() -> Program {
    let st = Arena::new(1 << 14);
    let sites = [SiteDesc::range_source(st.handle(), 2001, &[1, 2])];
    let b = |slot| End::Breaker(Breaker { kind: BreakerKind::HashAgg, slot, bytes: 0, budget: Budget::Share, spill: Spill::Partitioned { bits: 3 } });
    let pipes = [
        (Pipeline::eligible(0, b(0), 1000), Code::hand(k_range_group_spill)),
        (Pipeline::eligible(0, b(0), 1000), Code::hand(k_range_group_spill)),
        (Pipeline::eligible(0, b(1), 1000).consuming_one(0), Code::hand(k_range_group_spill)),
        (Pipeline::eligible(0, b(0), 1000), Code::hand(k_range_group_spill)),
        (Pipeline::eligible(0, b(0), 1000), Code::hand(k_range_group_spill)),
        (Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite).consuming(&[0, 1]), Code::hand(k_emit_groups_spill)),
    ];
    let mut p = Program::with_pipelines(Layout { nsites: 1, out_natts: 3, out_rows: 64, scratch_bytes: 0, exec_bytes: 0 }, &sites, &core::array::from_fn::<_, 3, _>(|i| OutCol::new(["c0", "c1", "c2"][i], 20, -1, 8, true)), WritePlan::SELECT, pipes);
    let arms = |first| Proto::new(ProtoKind::Arms, first).slot(0).arg(2);
    p.set_v4(&V4Descs::new(vec![arms(0), arms(3)])); // alloc: Cold
    p
}

#[test]
fn each_run_on_a_reused_slot_number_holds_its_own_arms() {
    install_spill_factory(&MEM_SPILL);
    let program = Arc::new(two_runs());
    let mut derive = FakeDerive::ranges(RangeTable::new(4, 4));
    let mut entry = Entry::new(2);
    let flag = AtomicU8::new(0);
    let mut sink = LegSink::new();
    let params = [7u64, 3];
    let mut b = binding(&params, &[0, 0], &mut sink, &flag);
    let mut exec = entry.start(Some(PlanKey { handle: Handle(13), generation: Generation(559) }), &program, &mut derive, &mut b, lease(Grant::of(64 << 20))).unwrap();
    let node = exec.node_mut();
    let got: Vec<(bool, bool)> = (0..6).map(|p| (node.in_arm_run(p), node.arm_continues(p))).collect(); // alloc: Cold
    entry.end(exec, false);
    let (arm, last, none) = ((true, true), (true, false), (false, false));
    assert_eq!(got, [arm, last, none, arm, last, none], "run B (P3, P4) on slot 0's reused number keeps its arms");
}
