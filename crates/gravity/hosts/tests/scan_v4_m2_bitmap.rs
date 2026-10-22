//! M2's bitmap hosts (hosts v4/m2_bitmap.rs, M2-inventory row 36): a paged bitmap source serves the row protocol's rows
//! one heap page per window beside the Offset lane; a BitmapAnd / BitmapOr tree builds its bitmap from several probes.
use gravity_hosts_rig as common;

use std::rc::Rc;
use std::sync::atomic::AtomicU8;

use common::*;
use gravity_abi::{Lane, ScanPos};
use gravity_driver::{blank_site, BindingRef, CommandId, Identity, PerTuple, Pins, Snapshot, Source, Window};
use gravity_hosts::ops::scan::bitmap_window::{Shape, Tree, TreeRecipe};
use gravity_hosts::{BitmapSource, SiteRecipe};
use gravity_program::LaneId;
use mcx::MemoryContext;
use types_rel::Relation;
use types_snapshot::SnapshotData;
use types_storage::lock::AccessShareLock;

static FLAG: AtomicU8 = AtomicU8::new(0);
const MAX_LANES: usize = 8;

fn binding_ref(snap: &Rc<SnapshotData<'static>>) -> BindingRef<'static> {
    BindingRef { snapshot: Snapshot(snap as *const _ as u64), command_id: CommandId(0), params: &[], abort_flag: &FLAG, pins: Pins(0), per_tuple: PerTuple(0), identity: Identity::default(), resolved: None, ordinal: None }
}

/// The seams a bitmap heap scan asks beyond the rig's (`common::install`).
fn install_bitmap() {
    install();
    if !xact_seams::isolation_is_serializable::is_installed() {
        xact_seams::isolation_is_serializable::set(|| false);
    }
}

/// A one-key probe of the fixture index over column 1 (`col <op> $1`), as a bitmap walk's recipe.
fn leaf(mcx: mcx::Mcx<'static>, opno: u32, opfuncid: u32) -> SiteRecipe {
    let heap = Relation::open(heap_relation_data(mcx), None);
    let index = Relation::open(index_relation_data(mcx, false), None);
    let quals = indexqual_param(mcx, INDEX_VAR, opno, opfuncid);
    SiteRecipe::from_plan_any_am(&heap, &index, &quals, &types_nodes::NodeList::nil(), AccessShareLock, &[1]).unwrap()
}

/// Every row after a seek over `keys`, with its position, and each window's row count.
fn drain(src: &mut BitmapSource, keys: &[u64]) -> (Vec<(i32, ScanPos)>, Vec<u32>) {
    drain_nulls(src, keys, &vec![0; keys.len()])
}

/// `drain` over key words some of which are NULL (`nulls[i] != 0`).
fn drain_nulls(src: &mut BitmapSource, keys: &[u64], nulls: &[u8]) -> (Vec<(i32, ScanPos)>, Vec<u32>) {
    src.seek(keys, nulls).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    let (mut rows, mut sizes) = (Vec::new(), Vec::new());
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = src.window(&mut w).unwrap();
        if n == 0 {
            break;
        }
        sizes.push(n);
        let base = w.base().pos;
        for i in 0..n as usize {
            let v = unsafe { w.lanes()[0].datum(i) } as u32 as i32;
            let pos = match w.lanes().get(1) {
                Some(off) => {
                    assert_eq!(off.id, LaneId::Offset.encode());
                    ScanPos::row(HEAP_OID, base.block, unsafe { *(off.ptr as *const u16).add(i) })
                }
                None => base,
            };
            rows.push((v, pos));
        }
    }
    rows.sort_by_key(|r| r.0);
    (rows, sizes)
}

#[test]
fn a_paged_bitmap_serves_one_heap_page_per_window() {
    let _g = serial();
    install_bitmap();
    reset_fixture();
    let ctx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("m2-bitmap-page")));
    let mcx = ctx.mcx();
    load_rows(mcx, &(0..1000).collect::<Vec<i32>>());
    let snap = static_mvcc_snapshot();
    let mut one = BitmapSource::new(leaf(mcx, OP_INT4GT, F_INT4GT), None);
    one.bind(&binding_ref(&snap)).unwrap();
    let (rows1, sizes1) = drain(&mut one, &[99]);
    assert!(sizes1.iter().all(|&n| n == 1), "unpaged: one row per window");
    assert_eq!(rows1.len(), 900);
    one.unbind();
    let mut paged = BitmapSource::new(leaf(mcx, OP_INT4GT, F_INT4GT), None).paged(true);
    paged.bind(&binding_ref(&snap)).unwrap();
    let (rows, sizes) = drain(&mut paged, &[99]);
    assert_eq!(rows, rows1, "the same rows at the same positions");
    let pages: std::collections::BTreeSet<u32> = rows.iter().map(|r| r.1.block).collect();
    assert_eq!(sizes.len(), pages.len(), "one window per heap page");
    assert!(sizes.iter().any(|&n| n > 1));
    assert_eq!(drain(&mut paged, &[998]).0.len(), 1, "a rescan builds its own bitmap");
    paged.unbind();
    quiesced();
}

/// "does not apply": a tree whose AND meets an empty probe serves nothing, an OR serves the union, a NULL key its own leaf
/// nothing (D2180).
#[test]
fn a_bitmap_tree_intersects_and_unions_its_probes() {
    let _g = serial();
    install_bitmap();
    reset_fixture();
    let ctx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("m2-bitmap-tree")));
    let mcx = ctx.mcx();
    load_rows(mcx, &(0..600).collect::<Vec<i32>>());
    let snap = static_mvcc_snapshot();
    let tree = |shape: Shape, leaves: Vec<SiteRecipe>| {
        let t = TreeRecipe { leaves: leaves.clone(), shape };
        let mut s = BitmapSource::new(leaves[0].clone(), None).with_tree(Tree::new(t)).paged(true);
        s.bind(&binding_ref(&snap)).unwrap();
        s
    };
    let vals = |src: &mut BitmapSource, keys: &[u64]| drain(src, keys).0.into_iter().map(|r| r.0).collect::<Vec<i32>>();
    let (gt, eq) = (|| leaf(mcx, OP_INT4GT, F_INT4GT), || leaf(mcx, OP_INT4EQ, F_INT4EQ));
    let mut or = tree(Shape::Or(vec![Shape::Leaf(0), Shape::Leaf(1)]), vec![eq(), eq()]);
    assert_eq!(vals(&mut or, &[5, 410]), vec![5, 410]);
    assert_eq!(vals(&mut or, &[7, 7]), vec![7], "a tid in both children once");
    assert_eq!(drain_nulls(&mut or, &[5, 6], &[0, 1]).0.into_iter().map(|r| r.0).collect::<Vec<i32>>(), vec![5], "a NULL key empties its own leaf only (D2180)");
    or.unbind();
    let mut and = tree(Shape::And(vec![Shape::Leaf(0), Shape::Leaf(1)]), vec![gt(), eq()]);
    assert_eq!(vals(&mut and, &[100, 150]), vec![150]);
    assert_eq!(vals(&mut and, &[100, 50]), Vec::<i32>::new(), "50 is not above 100");
    and.unbind();
    let mut nested = tree(Shape::Or(vec![Shape::And(vec![Shape::Leaf(0), Shape::Leaf(1)]), Shape::Leaf(2)]), vec![gt(), eq(), eq()]);
    assert_eq!(vals(&mut nested, &[590, 595, 3]), vec![3, 595]);
    nested.unbind();
    quiesced();
}

/// D2180: a leaf whose key word is NULL is an empty bitmap (PG: SK_ISNULL, or a NULL array key skips that index scan). It
/// empties an And; an Or still unions its other children (MultiExecBitmapOr). A dead leaf is never probed: its keys still
/// hold the previous seek's arguments, so a rescan must not bring back the earlier key's rows.
#[test]
fn a_null_key_empties_its_own_leaf_only() {
    let _g = serial();
    install_bitmap();
    reset_fixture();
    let ctx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("m2-bitmap-null-leaf")));
    let mcx = ctx.mcx();
    load_rows(mcx, &(0..600).collect::<Vec<i32>>());
    let snap = static_mvcc_snapshot();
    let tree = |shape: Shape, leaves: Vec<SiteRecipe>| {
        let t = TreeRecipe { leaves: leaves.clone(), shape };
        let mut s = BitmapSource::new(leaves[0].clone(), None).with_tree(Tree::new(t)).paged(true);
        s.bind(&binding_ref(&snap)).unwrap();
        s
    };
    let vals = |src: &mut BitmapSource, keys: &[u64], nulls: &[u8]| drain_nulls(src, keys, nulls).0.into_iter().map(|r| r.0).collect::<Vec<i32>>();
    let (gt, eq) = (|| leaf(mcx, OP_INT4GT, F_INT4GT), || leaf(mcx, OP_INT4EQ, F_INT4EQ));
    let none: Vec<i32> = Vec::new();
    let mut or = tree(Shape::Or(vec![Shape::Leaf(0), Shape::Leaf(1)]), vec![eq(), eq()]);
    assert_eq!(vals(&mut or, &[5, 410], &[0, 1]), vec![5], "Or: the NULL second leaf is empty, the first unions");
    assert_eq!(vals(&mut or, &[5, 410], &[1, 0]), vec![410], "Or: the NULL first leaf is empty, the second unions");
    assert_eq!(vals(&mut or, &[5, 410], &[1, 1]), none, "Or: every leaf NULL, no row");
    // rescan: non-NULL, then NULL (the dead leaf keeps 5 / 410 in its keys and must not probe them), then non-NULL
    assert_eq!(vals(&mut or, &[5, 410], &[0, 0]), vec![5, 410]);
    assert_eq!(vals(&mut or, &[6, 410], &[1, 0]), vec![410], "a dead leaf never reuses the previous seek's key 5");
    assert_eq!(vals(&mut or, &[6, 411], &[0, 1]), vec![6], "a dead leaf never reuses the previous seek's key 410");
    assert_eq!(vals(&mut or, &[7, 411], &[0, 0]), vec![7, 411], "a leaf dead at the last seek probes its new key");
    or.unbind();
    let mut and = tree(Shape::And(vec![Shape::Leaf(0), Shape::Leaf(1)]), vec![gt(), eq()]);
    assert_eq!(vals(&mut and, &[100, 150], &[0, 0]), vec![150]);
    assert_eq!(vals(&mut and, &[100, 150], &[0, 1]), none, "And: a NULL leaf empties the And");
    assert_eq!(vals(&mut and, &[100, 150], &[1, 0]), none, "And: a NULL first leaf empties the And");
    assert_eq!(vals(&mut and, &[100, 160], &[0, 0]), vec![160], "the And after a NULL seek probes its new keys");
    and.unbind();
    let mut nested = tree(Shape::Or(vec![Shape::And(vec![Shape::Leaf(0), Shape::Leaf(1)]), Shape::Leaf(2)]), vec![gt(), eq(), eq()]);
    assert_eq!(vals(&mut nested, &[590, 595, 3], &[0, 0, 0]), vec![3, 595]);
    assert_eq!(vals(&mut nested, &[590, 595, 3], &[1, 0, 0]), vec![3], "Or(And(NULL, x), y): the And is empty, y unions");
    assert_eq!(vals(&mut nested, &[590, 595, 3], &[0, 1, 0]), vec![3], "Or(And(x, NULL), y): the And is empty, y unions");
    assert_eq!(vals(&mut nested, &[590, 596, 3], &[0, 0, 1]), vec![596], "Or(And(x, z), NULL): the And unions alone");
    assert_eq!(vals(&mut nested, &[590, 596, 3], &[1, 0, 1]), none, "Or(And(NULL, z), NULL): every Or child empty");
    assert_eq!(vals(&mut nested, &[590, 597, 4], &[0, 0, 0]), vec![4, 597], "every leaf probes its new key after the dead seeks");
    nested.unbind();
    quiesced();
}

/// D3195: a bitmap source bound, sought and unbound again and again (a worker slot binds every host at each section's open,
/// once per rescan of a rerun member that runs as a section) holds what one cycle holds: its scans live in the per-bind
/// context the unbind resets, never in the host's own context (rv Q2 on pgrcolumnar at W8 grew 1.9 GB per worker lane).
#[test]
fn a_rebound_bitmap_source_holds_one_binds_bytes() {
    let _g = serial();
    install_bitmap();
    reset_fixture();
    let ctx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("m2-bitmap-rebind")));
    let mcx = ctx.mcx();
    load_rows(mcx, &(0..600).collect::<Vec<i32>>());
    let snap = static_mvcc_snapshot();
    let leaves = || vec![leaf(mcx, OP_INT4GT, F_INT4GT), leaf(mcx, OP_INT4EQ, F_INT4EQ)];
    let one = BitmapSource::new(leaves()[0].clone(), None).paged(true);
    let l = leaves();
    let tree = BitmapSource::new(l[0].clone(), None).with_tree(Tree::new(TreeRecipe { leaves: l.clone(), shape: Shape::Or(vec![Shape::Leaf(0), Shape::Leaf(1)]) })).paged(true);
    for (name, mut src, nkeys) in [("one index", one, 1u64), ("a tree", tree, 2)] {
        let mut cycle = |src: &mut BitmapSource, i: u64| {
            src.bind(&binding_ref(&snap)).unwrap();
            let words: Vec<u64> = (0..nkeys).map(|j| 590 - i % 3 + j).collect();
            let n = drain(src, &words).0.len();
            src.unbind();
            n
        };
        assert!(cycle(&mut src, 0) > 0, "{name}: rows");
        let first = src.scratch_used();
        for i in 1..300 {
            cycle(&mut src, i);
        }
        let last = src.scratch_used();
        assert!(last <= first, "{name}: 300 binds hold {last} bytes where one held {first}");
    }
    quiesced();
}
