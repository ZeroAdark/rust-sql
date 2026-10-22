//! V4R-33 G4 (does not apply): the condition cache's survivor counts are pgrc only (the ruling of record: no heap
//! condition cache). A heap range scan over every block publishes nothing to the store: no granule, no count, no byte.

use gravity_hosts_rig as common;

use std::sync::atomic::AtomicU8;

use common::*;
use gravity_abi::Lane;
use gravity_driver::{blank_site, BindingRef, CommandId, Identity, PerTuple, Pins, Snapshot, Source, Window};
use gravity_hosts::ops::scan::pgrc::cond_cache;
use gravity_hosts::{HeapRangeSource, RangeRecipe};
use mcx::MemoryContext;
use types_rel::Relation;

static FLAG: AtomicU8 = AtomicU8::new(0);

#[test]
fn a_heap_scan_fills_no_condition_cache_count() {
    let _g = serial();
    install();
    reset_fixture();
    let ctx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("cond-counts-heap")));
    let mcx = ctx.mcx();
    let values: Vec<i32> = (0..3000).collect();
    load_rows(mcx, &values);
    let snap = static_mvcc_snapshot();
    let heap = Relation::open(heap_relation_data(mcx), None);
    let mut src = HeapRangeSource::new(RangeRecipe::from_plan(&heap, &[1]).unwrap());
    let b = BindingRef { snapshot: Snapshot(&snap as *const _ as u64), command_id: CommandId(0), params: &[], abort_flag: &FLAG, pins: Pins(0), per_tuple: PerTuple(0), identity: Identity::default(), resolved: None, ordinal: None };
    src.bind(&b).unwrap();
    let before = cond_cache::census();
    src.seek(&[0, u64::MAX], &[0, 0]).unwrap();
    let mut lanes = [Lane::NONE; 32];
    let mut rows = 0u64;
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = src.window(&mut w).unwrap();
        if n == 0 {
            break;
        }
        rows += n as u64;
    }
    src.unbind();
    assert_eq!(rows, values.len() as u64);
    let after = cond_cache::census();
    assert_eq!((after[0], after[1], after[2], after[4]), (before[0], before[1], before[2], before[4]), "no lookup, no publish, no byte held");
    quiesced();
}
