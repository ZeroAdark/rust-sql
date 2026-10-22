//! D2939 (found by D1360's debug W8 sweep, trunk c18f910671c): the EPQ substitution read its block with a combined-read length of
//! `hi - blk` while `hi` is 0 (`window_substituted` ends any claim first), an overflow panic in a debug build (m4_d655_join_epq,
//! m4_merge_join_epq, m7_epq_joins, ...) and, wrapped in a release one, a read-ahead of the relation's rest (up to
//! io_combine_limit blocks) for the one row EPQ rechecks. The substituted block is read alone.

use gravity_hosts_rig as common;

use std::rc::Rc;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering::Relaxed;

use common::*;
use gravity_abi::ScanPos;
use gravity_driver::{BindingRef, CommandId, Identity, PerTuple, Pins, Snapshot, Source};
use gravity_hosts::{HeapRangeSource, RangeRecipe};
use mcx::MemoryContext;
use types_rel::Relation;
use types_snapshot::SnapshotData;

static FLAG: AtomicU8 = AtomicU8::new(0);

fn binding_ref(snap: &Rc<SnapshotData<'static>>) -> BindingRef<'static> {
    BindingRef { snapshot: Snapshot(snap as *const _ as u64), command_id: CommandId(0), params: &[], abort_flag: &FLAG, pins: Pins(0), per_tuple: PerTuple(0), identity: Identity::default(), resolved: None, ordinal: None }
}

fn recipe(mcx: mcx::Mcx<'_>) -> RangeRecipe {
    let heap = Relation::open(heap_relation_data(mcx), None);
    RangeRecipe::from_plan(&heap, &[1]).unwrap()
}

#[test]
fn an_epq_substitution_past_the_first_block_reads_that_block_alone() {
    let _g = serial();
    install();
    reset_fixture();
    let ctx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("heap-source-test-d2939")));
    let mcx = ctx.mcx();
    load_rows(mcx, &(0..1500).collect::<Vec<i32>>());
    let snap = static_mvcc_snapshot();
    let mut src = HeapRangeSource::new(recipe(mcx));
    src.bind(&binding_ref(&snap)).unwrap();
    assert!(src.nblocks() >= 4, "{} blocks", src.nblocks());
    let pos = ScanPos::new(HEAP_OID, 2, 1);
    let calls0 = BATCHED_CALLS.load(Relaxed);
    BATCHED_HINT_MAX.store(0, Relaxed);
    src.substitute(pos);
    src.seek(&[0, u64::MAX], &[0, 0]).unwrap();
    let (mut v, mut n) = ([0u64; 1], [0u8; 1]);
    assert!(src.next(&mut v, &mut n).unwrap(), "the substituted row");
    assert_eq!((src.position(), n[0]), (pos, 0), "served at its own position");
    assert!(BATCHED_HINT_MAX.load(Relaxed) <= 1, "the EPQ read asked for {} blocks ({} combined reads)", BATCHED_HINT_MAX.load(Relaxed), BATCHED_CALLS.load(Relaxed) - calls0);
    assert!(!src.next(&mut v, &mut n).unwrap(), "the scan ends after the substituted row");
    assert_eq!(src.pins(), 0);
    src.unbind();
    assert_eq!(pins_held(), 0);
    quiesced();
}
