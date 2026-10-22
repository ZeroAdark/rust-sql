//! M2's index-only N-row windows (hosts v4/m2_index.rs, M2-inventory row 36): a batched index-only source serves the
//! row protocol's rows and positions in windows that double from one row after each seek, the RowId lane holding each
//! row's heap tid; an unbatched one (M1's form) keeps one row per window.
use gravity_hosts_rig as common;

use std::rc::Rc;
use std::sync::atomic::AtomicU8;

use common::*;
use gravity_abi::{Lane, ScanPos};
use gravity_driver::{blank_site, BindingRef, CommandId, Identity, PerTuple, Pins, Snapshot, Source, Window};
use gravity_hosts::ops::scan::index_window::IOS_MAX_ROWS;
use gravity_hosts::{IndexSource, SiteRecipe};
use gravity_kernels::ops::scan::entries::tid_of_word;
use mcx::MemoryContext;
use types_rel::Relation;
use types_snapshot::SnapshotData;
use types_storage::lock::AccessShareLock;

static FLAG: AtomicU8 = AtomicU8::new(0);
const MAX_LANES: usize = 8;

fn binding_ref(snap: &Rc<SnapshotData<'static>>) -> BindingRef<'static> {
    BindingRef { snapshot: Snapshot(snap as *const _ as u64), command_id: CommandId(0), params: &[], abort_flag: &FLAG, pins: Pins(0), per_tuple: PerTuple(0), identity: Identity::default(), resolved: None, ordinal: None }
}

/// The key-less index-only walk of the fixture index over column 1.
fn ios(mcx: mcx::Mcx<'static>, batched: bool, snap: &Rc<SnapshotData<'static>>) -> IndexSource {
    let heap = Relation::open(heap_relation_data(mcx), None);
    let index = Relation::open(index_relation_data(mcx, false), None);
    let nil = types_nodes::NodeList::nil();
    let r = SiteRecipe::from_plan(&heap, &index, &nil, &nil, AccessShareLock, &[1]).unwrap().with_only(true);
    let mut src = IndexSource::new(r).batched(batched);
    src.bind(&binding_ref(snap)).unwrap();
    src
}

/// Every window after a seek: its row count, values and RowId positions (`None` when the window has no RowId lane: the
/// window's base is its one row's position).
fn windows(src: &mut IndexSource) -> Vec<(u32, Vec<i32>, Vec<ScanPos>)> {
    src.seek(&[], &[]).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    let mut out = Vec::new();
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = src.window(&mut w).unwrap();
        if n == 0 {
            return out;
        }
        let vals = (0..n as usize).map(|i| unsafe { w.lanes()[0].datum(i) } as u32 as i32).collect();
        let poss = match w.lanes().get(1) {
            Some(t) => (0..n as usize).map(|i| tid_of_word(unsafe { t.datum(i) })).map(|(b, o)| ScanPos::row(HEAP_OID, b, o)).collect(),
            None => vec![w.base().pos],
        };
        out.push((n, vals, poss));
    }
}

#[test]
fn a_batched_index_only_walk_doubles_its_windows_and_keeps_every_rows_position() {
    let _g = serial();
    install();
    reset_fixture();
    let ctx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("m2-ios")));
    let mcx = ctx.mcx();
    let values: Vec<i32> = (0..1200).rev().collect();
    load_rows(mcx, &values);
    let snap = static_mvcc_snapshot();
    // the row protocol's rows and positions (M1's one-row windows)
    let mut one = ios(mcx, false, &snap);
    let ws1 = windows(&mut one);
    assert!(ws1.iter().all(|w| w.0 == 1), "unbatched: one row per window");
    one.unbind();
    let mut many = ios(mcx, true, &snap);
    let ws = windows(&mut many);
    let sizes: Vec<u32> = ws.iter().map(|w| w.0).collect();
    let mut want = Vec::new();
    let (mut left, mut cap) = (values.len() as u32, 1u32);
    while left > 0 {
        want.push(cap.min(left));
        left -= cap.min(left);
        cap = (cap * 2).min(IOS_MAX_ROWS as u32);
    }
    assert_eq!(sizes, want, "1, 2, 4 … up to one index page's rows");
    let flat = |ws: &[(u32, Vec<i32>, Vec<ScanPos>)]| (ws.iter().flat_map(|w| w.1.clone()).collect::<Vec<_>>(), ws.iter().flat_map(|w| w.2.clone()).collect::<Vec<_>>());
    let (v1, p1) = flat(&ws1);
    let (vn, pn) = flat(&ws);
    assert_eq!(v1, (0..1200).collect::<Vec<i32>>(), "the index's order");
    assert_eq!((vn, pn), (v1, p1), "the same rows at the same heap positions");
    // a second seek starts at one row again
    assert_eq!(windows(&mut many)[0].0, 1);
    many.unbind();
    quiesced();
}
