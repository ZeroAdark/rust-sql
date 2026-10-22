use gravity_driver_rig as common;

use std::sync::atomic::AtomicU8;
use std::sync::Arc;

use common::*;
use gravity_abi::ctx::hdr;
use gravity_driver::{Call, Identity, Node, NodeState, Ret, Snapshot};
use gravity_program::Code;

const NULLS: [u8; 4] = [0; 4];

#[test]
fn bind_writes_the_header_cells_and_hands_identity_to_the_sources() {
    let program = Arc::new(p_join(4));
    let (mut derive, stats) = FakeDerive::new([orders, order_lines, no_rows, no_rows]).with_stats();
    let mut node = Node::build(program, &mut derive);
    let flag = AtomicU8::new(0);
    let sink = FakeSink::new();
    let mut sink_ref = &sink;
    let params = [1u64, 2, 30];
    let mut b = binding(&params, &NULLS, &mut sink_ref, &flag);
    node.apply(&mut b).unwrap();
    assert_eq!(hdr_cell(&node, hdr::PARAMS), params.as_ptr() as u64);
    assert_eq!(hdr_cell(&node, hdr::CANCEL), flag.as_ptr() as u64);
    assert_eq!(node.identity(), gravity_abi::Identity { proc_number: 7, command_id: 3, resource_owner: 0xAB, latch: 0xCD, snapshot: 0x5A });
    assert_eq!(hdr_cell(&node, hdr::IDENTITY), unsafe { *gravity_abi::ctx::cell(node.ctx(), hdr::IDENTITY) });
    assert_ne!(hdr_cell(&node, hdr::IDENTITY), 0);
    assert_eq!(hdr_cell(&node, hdr::CLAIM_HI), u64::MAX);
    assert_eq!(hdr_cell(&node, hdr::FLAGS), 1 << 48, "width 1, ordinal 0, flags 0");
    let _ = &b.parts.identity as *const Identity;
    assert_eq!(stats.binds.get(), 2);
    for i in 0..2 {
        let s = node.source_mut(i).unwrap();
        let _ = s;
    }
    node.remove();
    assert_eq!((hdr_cell(&node, hdr::PARAMS), hdr_cell(&node, hdr::CANCEL), hdr_cell(&node, hdr::IDENTITY)), (0, 0, 0));
    assert_eq!(node.identity(), gravity_abi::Identity::default());
    assert_eq!(stats.unbinds.get(), 2);
    let _ = Snapshot(0);
}

#[test]
fn apply_then_remove_leaves_the_node_bit_identical() {
    let program = Arc::new(p_join(4));
    let mut derive = FakeDerive::new([orders, order_lines, no_rows, no_rows]);
    let mut node = Node::build(program, &mut derive);
    let built = node.image().to_vec();
    let flag = AtomicU8::new(0);
    for _ in 0..20 {
        let sink = FakeSink::new();
        let mut sink_ref = &sink;
        let mut b = binding(&[1, 2, 30], &NULLS, &mut sink_ref, &flag);
        node.apply(&mut b).unwrap();
        assert_eq!(node.call(Call::batch(4)).unwrap(), Ret::Done);
        assert_eq!(sink.rows(), 6);
        assert_ne!(node.image(), &built[..]);
        node.remove();
        node.reset();
        assert_eq!(node.image(), &built[..]);
        node.park();
        assert_eq!(node.state(), NodeState::Parked);
        node.unpark();
        assert_eq!(node.image(), &built[..]);
    }
}

#[test]
fn per_execution_region_resets_to_the_mark() {
    let program = Arc::new(p_code(Code::hand(k_noop), 1, 256));
    let mut derive = FakeDerive::new([no_rows; 4]);
    let mut node = Node::build(program, &mut derive);
    let used = node.arena_used();
    let p = node.exec_arena().alloc(200);
    unsafe { *p = 9 };
    assert_eq!(node.arena_used(), used + 200);
    node.reset();
    assert_eq!(node.arena_used(), used);
    assert_eq!(unsafe { *p }, 0);
}
