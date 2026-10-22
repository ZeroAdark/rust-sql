use gravity_hosts_rig as common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use common::*;
use gravity_abi::ScanPos;
/// The tests' scratch site lanes (a test bound, not the ABI's: sites size their lane arrays per program).
const MAX_LANES: usize = 32;
use gravity_abi::Lane;
use gravity_driver::{blank_site, BindingRef, CommandId, Identity, PerTuple, Pins, Snapshot, Source, Window};
use gravity_hosts::{IndexSource, SiteRecipe};
use mcx::MemoryContext;
use types_rel::Relation;
use types_snapshot::SnapshotData;
use types_storage::lock::AccessShareLock;

struct Counting;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static TRACKED: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static TOKEN: Cell<u8> = const { Cell::new(0) };
}

fn token() -> usize {
    TOKEN.with(|t| t.as_ptr() as usize)
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if token() == TRACKED.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        System.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l)
    }
}

#[global_allocator]
static A: Counting = Counting;

fn allocs() -> usize {
    ALLOCS.load(Ordering::Relaxed)
}

static FLAG: AtomicU8 = AtomicU8::new(0);

fn binding_ref(snap: &Rc<SnapshotData<'static>>) -> BindingRef<'static> {
    BindingRef { snapshot: Snapshot(snap as *const _ as u64), command_id: CommandId(0), params: &[], abort_flag: &FLAG, pins: Pins(0), per_tuple: PerTuple(0), identity: Identity::default(), resolved: None, ordinal: None }
}

fn recipe(mcx: mcx::Mcx<'_>, opno: u32, opfuncid: u32) -> SiteRecipe {
    let heap = Relation::open(heap_relation_data(mcx), None);
    let index = Relation::open(index_relation_data(mcx, false), None);
    let quals = indexqual_param(mcx, INDEX_VAR, opno, opfuncid);
    SiteRecipe::from_plan(&heap, &index, &quals, &types_nodes::NodeList::nil(), AccessShareLock, &[1]).unwrap()
}

fn probe(src: &mut IndexSource, key: i64, null: u8) -> Vec<i64> {
    src.seek(&[key as u64], &[null]).unwrap();
    let mut out = Vec::new();
    let (mut v, mut n) = ([0u64; 1], [0u8; 1]);
    while src.next(&mut v, &mut n).unwrap() {
        assert_eq!(n[0], 0);
        out.push(v[0] as i32 as i64);
    }
    out
}

#[test]
fn bind_seek_next_unbind_finds_rows_records_positions_and_allocates_nothing_when_warm() {
    let _g = serial();
    install();
    reset_fixture();
    let ctx = MemoryContext::new("t");
    let mcx = ctx.mcx();
    let tids = load_rows(mcx, &[30, 10, 20, 50, 40]);
    let snap = static_mvcc_snapshot();
    let b = binding_ref(&snap);

    let mut eq = IndexSource::new(recipe(mcx, OP_INT4EQ, F_INT4EQ));
    eq.bind(&b).unwrap();
    assert_eq!(probe(&mut eq, 20, 0), vec![20]);
    assert_eq!(eq.position(), ScanPos::new(HEAP_OID, 0, tids[2].ip_posid), "the position of the last row served");
    assert_eq!(probe(&mut eq, 20, 1), Vec::<i64>::new(), "a NULL key is EOF without a probe");
    assert_eq!(probe(&mut eq, 99, 0), Vec::<i64>::new());
    let (mut v, mut n) = ([0u64; 1], [0u8; 1]);
    assert_eq!(eq.position(), ScanPos::NONE, "a fresh seek clears the position");
    assert!(!eq.refetch(&mut v, &mut n).unwrap());
    probe(&mut eq, 40, 0);
    assert!(eq.refetch(&mut v, &mut n).unwrap(), "refetch by position after the scan moved on");
    assert_eq!((v[0] as i32, n[0]), (40, 0));
    let pos = eq.position();
    eq.substitute(pos);
    assert_eq!(probe(&mut eq, 40, 0), vec![40], "a substituted position yields that version, re-checked against the keys");
    assert_eq!(eq.position(), pos, "and the site's position is the substituted version");
    assert_eq!(probe(&mut eq, 40, 0), vec![40], "handed again at the next open while it passes the keys (ExecScanFetch's test tuple)");
    assert_eq!(probe(&mut eq, 20, 0), Vec::<i64>::new(), "a substituted version that no longer matches the keys is skipped");
    assert_eq!(probe(&mut eq, 40, 0), Vec::<i64>::new(), "and cleared: no later open yields it");
    assert_eq!(probe(&mut eq, 20, 0), Vec::<i64>::new(), "nor walks the index (a recheck's source)");
    eq.unbind();
    eq.bind(&b).unwrap();
    assert_eq!(probe(&mut eq, 20, 0), vec![20], "the next binding walks the index again");
    eq.unbind();

    let mut gt = IndexSource::new(recipe(mcx, OP_INT4GT, F_INT4GT));
    gt.bind(&b).unwrap();
    assert_eq!(probe(&mut gt, 15, 0), vec![20, 30, 40, 50]);
    gt.unbind();
    quiesced();

    for _ in 0..3 {
        eq.bind(&b).unwrap();
        assert_eq!(probe(&mut eq, 40, 0), vec![40]);
        eq.unbind();
    }
    let used = eq.scratch_used();
    let opens = RELATION_OPENS.load(Ordering::Relaxed);
    TRACKED.store(token(), Ordering::Relaxed);
    let before = allocs();
    for _ in 0..200 {
        eq.bind(&b).unwrap();
        eq.seek(&[40], &[0]).unwrap();
        assert!(eq.next(&mut v, &mut n).unwrap() && v[0] == 40 && n[0] == 0);
        assert!(eq.position().is_valid());
        assert!(!eq.next(&mut v, &mut n).unwrap());
        eq.unbind();
    }
    let steady = allocs() - before;
    TRACKED.store(0, Ordering::Relaxed);
    assert_eq!(eq.scratch_used(), used, "the scratch context must not grow across warm cycles");
    assert_eq!(steady, 0, "{steady} heap allocations in 200 warm bind/seek/next/unbind cycles");
    assert_eq!(RELATION_OPENS.load(Ordering::Relaxed) - opens, 0, "a warm bind re-pins its relcache entries without a relcache lookup");
    quiesced();

    rebuild_relcache();
    eq.bind(&b).unwrap();
    assert_eq!(probe(&mut eq, 40, 0), vec![40]);
    eq.unbind();
    assert_eq!(RELATION_OPENS.load(Ordering::Relaxed) - opens, 2, "rebuilt entries are re-opened, not re-pinned");
    quiesced();
    drop(eq);
    drop(gt);
    quiesced();
}

#[test]
fn the_probe_row_is_a_one_row_window() {
    let _g = serial();
    install();
    reset_fixture();
    let ctx = MemoryContext::new("t-window");
    let mcx = ctx.mcx();
    let tids = load_rows(mcx, &[30, 10, 20]);
    let snap = static_mvcc_snapshot();
    let mut src = IndexSource::new(recipe(mcx, OP_INT4GT, F_INT4GT));
    src.bind(&binding_ref(&snap)).unwrap();
    src.seek(&[15], &[0]).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    let mut seen = Vec::new();
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = src.window(&mut w).unwrap();
        if n == 0 {
            break;
        }
        assert_eq!((n, w.nlanes, w.lanes()[0].width), (1, 1, 8));
        assert!(w.lanes()[0].all_valid() && unsafe { w.lanes()[0].is_valid(0) });
        seen.push((unsafe { w.lanes()[0].datum(0) } as i32, w.base().pos));
    }
    assert_eq!(seen.iter().map(|s| s.0).collect::<Vec<_>>(), vec![20, 30]);
    assert_eq!(seen[0].1, ScanPos::row(HEAP_OID, 0, tids[2].ip_posid), "the base of a probe window is the row's own position (POS_ROW)");
    assert_eq!(src.position(), seen[1].1);
    src.unbind();
    quiesced();
}

#[test]
fn the_key_less_walk_reads_the_whole_index_in_order_forward_and_backward() {
    let _g = serial();
    install();
    reset_fixture();
    let ctx = MemoryContext::new("t-walk");
    let mcx = ctx.mcx();
    load_rows(mcx, &[30, 10, 20, 50, 40]);
    let snap = static_mvcc_snapshot();
    let b = binding_ref(&snap);
    let heap = Relation::open(heap_relation_data(mcx), None);
    let index = Relation::open(index_relation_data(mcx, false), None);
    let nil = types_nodes::NodeList::nil();
    let recipe = SiteRecipe::from_plan(&heap, &index, &nil, &nil, AccessShareLock, &[1]).unwrap();
    assert_eq!(recipe.nkeys, 0);
    let walk = |src: &mut IndexSource| {
        src.seek(&[], &[]).unwrap();
        let mut out = Vec::new();
        let (mut v, mut n) = ([0u64; 1], [0u8; 1]);
        while src.next(&mut v, &mut n).unwrap() {
            out.push(v[0] as i32);
        }
        out
    };
    let mut fwd = IndexSource::new(recipe.clone());
    fwd.bind(&b).unwrap();
    assert_eq!(walk(&mut fwd), vec![10, 20, 30, 40, 50], "forward: the index's own order");
    assert_eq!(walk(&mut fwd), vec![10, 20, 30, 40, 50], "a rescan with no keys restarts the walk");
    fwd.unbind();
    let mut bwd = IndexSource::new(recipe.with_backward(true));
    bwd.bind(&b).unwrap();
    assert_eq!(walk(&mut bwd), vec![50, 40, 30, 20, 10], "backward: the plan's indexorderdir");
    bwd.unbind();
    quiesced();
}

/// D4046: a walk polls its statement's cancel every 64 rows it visits, rows that pass included (one host `next` fills a
/// whole window); with the byte clear it reads every row.
#[test]
fn a_canceled_walk_stops_within_64_passing_rows_in_rows_and_in_windows() {
    let _g = serial();
    install();
    reset_fixture();
    let ctx = MemoryContext::new("t-d4046");
    let mcx = ctx.mcx();
    let vals: Vec<i32> = (1..=200).map(|i| i * 37 % 211).collect();
    load_rows(mcx, &vals);
    let snap = static_mvcc_snapshot();
    let b = binding_ref(&snap);
    let heap = Relation::open(heap_relation_data(mcx), None);
    let index = Relation::open(index_relation_data(mcx, false), None);
    let nil = types_nodes::NodeList::nil();
    let recipe = SiteRecipe::from_plan(&heap, &index, &nil, &nil, AccessShareLock, &[1]).unwrap();
    let byte = AtomicU8::new(0);
    let rows = |src: &mut IndexSource| {
        src.seek(&[], &[]).unwrap();
        let (mut v, mut n, mut got) = ([0u64; 1], [0u8; 1], 0usize);
        let end = cancelpoll::scoped(&byte, || loop {
            match src.next(&mut v, &mut n) {
                Ok(true) => got += 1,
                other => break other,
            }
        });
        (got, end)
    };
    let mut src = IndexSource::new(recipe.clone());
    src.bind(&b).unwrap();
    assert_eq!(rows(&mut src), (200, Ok(false)), "the byte clear: every row");
    byte.store(cancelpoll::ABORT_BIT, Ordering::Relaxed);
    let (got, end) = rows(&mut src);
    assert_eq!(end, Err(gravity_driver::Error::Cancelled), "a set byte stops the walk");
    assert!(got < 64, "within 64 visited rows: {got} passed first");
    src.unbind();
    byte.store(0, Ordering::Relaxed);
    let mut src = IndexSource::new(recipe).batched(true);
    src.bind(&b).unwrap();
    src.seek(&[], &[]).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    let mut window = |src: &mut IndexSource| {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        cancelpoll::scoped(&byte, || src.window(&mut w))
    };
    let first = window(&mut src).unwrap();
    assert!(first > 0);
    byte.store(cancelpoll::ABORT_BIT, Ordering::Relaxed);
    let mut before = first as usize;
    let stopped = loop {
        match window(&mut src) {
            Ok(0) => break Ok(0),
            Ok(n) => before += n as usize,
            e => break e,
        }
    };
    assert_eq!(stopped, Err(gravity_driver::Error::Cancelled), "a window walk stops for the cancel");
    assert!(before < 64 + first as usize, "within 64 visited rows of the cancel: {before} rows came first");
    src.unbind();
    quiesced();
}

/// D4213: a walk over TIDs whose rows its snapshot cannot see polls per TID. index_getnext_slot looped over such a run
/// (dead versions after an update or delete storm, each a heap fetch, maybe a buffer read and a dirty victim's write)
/// without a poll, so a cancel raised before the walk waited for its next visible row or its end.
#[test]
fn a_canceled_walk_over_invisible_rows_stops_between_tids() {
    struct Hidden;
    impl Drop for Hidden {
        fn drop(&mut self) {
            HIDE_ROWS.store(false, Ordering::Relaxed);
        }
    }
    let _g = serial();
    install();
    reset_fixture();
    let ctx = MemoryContext::new("t-d4213");
    let mcx = ctx.mcx();
    load_rows(mcx, &(1..=200).collect::<Vec<i32>>());
    let snap = static_mvcc_snapshot();
    let b = binding_ref(&snap);
    let heap = Relation::open(heap_relation_data(mcx), None);
    let index = Relation::open(index_relation_data(mcx, false), None);
    let nil = types_nodes::NodeList::nil();
    let recipe = SiteRecipe::from_plan(&heap, &index, &nil, &nil, AccessShareLock, &[1]).unwrap();
    let byte = AtomicU8::new(0);
    let walk = |src: &mut IndexSource| {
        src.seek(&[], &[]).unwrap();
        let (mut v, mut n, mut got) = ([0u64; 1], [0u8; 1], 0usize);
        let end = cancelpoll::scoped(&byte, || loop {
            match src.next(&mut v, &mut n) {
                Ok(true) => got += 1,
                other => break other,
            }
        });
        (got, end)
    };
    let mut src = IndexSource::new(recipe);
    src.bind(&b).unwrap();
    assert_eq!(walk(&mut src), (200, Ok(false)), "every row visible: all of them");
    let hidden = Hidden;
    HIDE_ROWS.store(true, Ordering::Relaxed);
    assert_eq!(walk(&mut src), (0, Ok(false)), "no row visible: the walk ends with none");
    byte.store(cancelpoll::ABORT_BIT, Ordering::Relaxed);
    assert_eq!(walk(&mut src), (0, Err(gravity_driver::Error::Cancelled)), "the cancel stops the run of invisible TIDs");
    drop(hidden);
    src.unbind();
    quiesced();
}

/// V4R-36: bind's stale check compares the index's access method with the one the recipe was built over, not with btree:
/// a recipe built over a hash index (M2's any-AM walk) whose relation now opens as a btree is stale, and a btree recipe
/// over the btree still binds (M1's check unchanged).
#[test]
fn a_rebind_whose_index_access_method_changed_is_stale() {
    let _g = serial();
    install();
    reset_fixture();
    let ctx = MemoryContext::new("t");
    let mcx = ctx.mcx();
    load_rows(mcx, &[1, 2]);
    let snap = static_mvcc_snapshot();
    let b = binding_ref(&snap);
    let heap = Relation::open(heap_relation_data(mcx), None);
    let mut hash = index_relation_data(mcx, false);
    hash.rd_rel.relam = types_core::HASH_AM_OID;
    let hash = Relation::open(hash, None);
    let nil = types_nodes::NodeList::nil();
    let r = SiteRecipe::from_plan_any_am(&heap, &hash, &nil, &nil, AccessShareLock, &[1]).unwrap();
    assert!(IndexSource::new(r).bind(&b).is_err(), "the index opens as a btree now: stale");
    let msg = gravity_hosts::take_error().map(|e| e.message().to_string()).unwrap_or_default();
    assert!(msg.contains("relation changed under the parked plan"), "{msg}");
    let mut bt = IndexSource::new(recipe(mcx, OP_INT4EQ, F_INT4EQ));
    bt.bind(&b).unwrap();
    assert_eq!(probe(&mut bt, 2, 0), vec![2]);
    bt.unbind();
}

/// V4R-44: a btree `= ANY ($1)` key over a parameter's array is bound from its key word at every seek (the array's NULL
/// elements match nothing, an empty array and a NULL array no row), and a substituted row rechecks against the array's
/// elements (`column = ANY (array)`) instead of failing. At the parent `from_plan` refuses the key ("a search-array key
/// whose array is not a constant").
#[test]
fn a_parameter_search_array_binds_at_every_seek_and_rechecks_a_substituted_row() {
    let _g = serial();
    install();
    reset_fixture();
    let ctx = MemoryContext::new("t");
    let mcx = ctx.mcx();
    load_rows(mcx, &[30, 10, 20, 50, 40]);
    let snap = static_mvcc_snapshot();
    let b = binding_ref(&snap);
    let heap = Relation::open(heap_relation_data(mcx), None);
    let index = Relation::open(index_relation_data(mcx, false), None);
    let var = types_nodes::Node::mk_var(mcx, INDEX_VAR, 1, types_core::INT4OID, -1, 0, 0).unwrap();
    let p = types_nodes::Node::mk(mcx, types_nodes::primnodes::Param { paramkind: types_nodes::primnodes::ParamKind::PARAM_EXTERN, paramid: 1, paramtype: 1007, paramtypmod: -1, paramcollid: 0, location: -1 }).unwrap();
    let args = types_nodes::NodeList::make2(mcx, var, p).unwrap();
    let sa = types_nodes::Node::mk(mcx, types_nodes::primnodes::ScalarArrayOpExpr { opno: OP_INT4EQ, opfuncid: F_INT4EQ, hashfuncid: 0, negfuncid: 0, useOr: true, inputcollid: 0, args, location: -1 }).unwrap();
    let quals = types_nodes::NodeList::make1(mcx, sa).unwrap();
    let r = SiteRecipe::from_plan(&heap, &index, &quals, &types_nodes::NodeList::nil(), AccessShareLock, &[1]).expect("a parameter search array builds (V4R-44)");
    let arr = |vals: &[Option<i32>]| {
        let elems: Vec<datum::Datum> = vals.iter().map(|v| datum::Datum::from_i32(v.unwrap_or(0))).collect();
        let nulls: Vec<bool> = vals.iter().map(|v| v.is_none()).collect();
        let img = arrayfuncs::construct_md_array(mcx, &elems, Some(&nulls), 1, &[vals.len() as i32], &[1], types_core::INT4OID, 4, true, b'i').unwrap();
        img.leak().as_ptr() as u64
    };
    let empty = arrayfuncs::construct_md_array(mcx, &[], None, 0, &[], &[], types_core::INT4OID, 4, true, b'i').unwrap().leak().as_ptr() as u64;
    let mut src = IndexSource::new(r);
    src.bind(&b).unwrap();
    let rows = |src: &mut IndexSource, word: u64, null: u8| {
        src.seek(&[word], &[null]).unwrap();
        let (mut v, mut n, mut out) = ([0u64; 1], [0u8; 1], Vec::new());
        while src.next(&mut v, &mut n).unwrap() {
            out.push(v[0] as i32);
        }
        out
    };
    assert_eq!(rows(&mut src, arr(&[Some(40), Some(10), Some(99), None, Some(40)]), 0), vec![10, 40], "elements in index order, deduplicated, NULL skipped");
    assert_eq!(rows(&mut src, arr(&[Some(20)]), 0), vec![20], "a second seek binds the new array");
    assert_eq!(rows(&mut src, empty, 0), Vec::<i32>::new(), "an empty array");
    assert_eq!(rows(&mut src, arr(&[None]), 0), Vec::<i32>::new(), "only NULL elements");
    assert_eq!(rows(&mut src, 0, 1), Vec::<i32>::new(), "a NULL array");
    assert_eq!(rows(&mut src, arr(&[Some(50)]), 0), vec![50]);
    let pos = src.position();
    src.substitute(pos);
    assert_eq!(rows(&mut src, arr(&[Some(1), Some(50)]), 0), vec![50], "a substituted row some element matches");
    src.substitute(pos);
    assert_eq!(rows(&mut src, arr(&[Some(20), None]), 0), Vec::<i32>::new(), "a substituted row no element matches");
    src.unbind();
    quiesced();
}

/// PERF-OLTP: a btree `= ANY ('{…}')` key over a CONSTANT array. `from_plan` bakes the image into the recipe (a host key:
/// the seek ignores the word), so a filed program served one literal array only and the simple protocol lowered again per
/// value; `from_plan_words` (the v4 walks) binds it from its key word at every seek as a parameter's (V4R-44): the recipe
/// holds no image and a second seek with another array reads that array.
#[test]
fn a_constant_search_array_is_a_word_under_from_plan_words() {
    let _g = serial();
    install();
    reset_fixture();
    let ctx = MemoryContext::new("t");
    let mcx = ctx.mcx();
    load_rows(mcx, &[30, 10, 20, 50, 40]);
    let snap = static_mvcc_snapshot();
    let b = binding_ref(&snap);
    let heap = Relation::open(heap_relation_data(mcx), None);
    let index = Relation::open(index_relation_data(mcx, false), None);
    let arr = |vals: &[i32]| {
        let elems: Vec<datum::Datum> = vals.iter().map(|&v| datum::Datum::from_i32(v)).collect();
        let img = arrayfuncs::construct_md_array(mcx, &elems, None, 1, &[vals.len() as i32], &[1], types_core::INT4OID, 4, true, b'i').unwrap();
        img.leak().as_ptr() as u64
    };
    let var = types_nodes::Node::mk_var(mcx, INDEX_VAR, 1, types_core::INT4OID, -1, 0, 0).unwrap();
    let c = types_nodes::Node::mk_const(mcx, 1007, -1, 0, -1, datum::Datum::from_u64(arr(&[20, 40])), false, false).unwrap();
    let args = types_nodes::NodeList::make2(mcx, var, c).unwrap();
    let sa = types_nodes::Node::mk(mcx, types_nodes::primnodes::ScalarArrayOpExpr { opno: OP_INT4EQ, opfuncid: F_INT4EQ, hashfuncid: 0, negfuncid: 0, useOr: true, inputcollid: 0, args, location: -1 }).unwrap();
    let quals = types_nodes::NodeList::make1(mcx, sa).unwrap();
    let baked = SiteRecipe::from_plan(&heap, &index, &quals, &types_nodes::NodeList::nil(), AccessShareLock, &[1]).expect("a constant search array builds");
    assert!(baked.host_keys[0] && baked.host_args[0].is_some(), "from_plan bakes the constant array (the shape table's recipe)");
    let r = SiteRecipe::from_plan_words(&heap, &index, &quals, AccessShareLock, &[1]).expect("a constant search array builds as a word");
    assert!(!r.host_keys[0] && r.host_args[0].is_none(), "from_plan_words holds no image");
    assert_eq!(r.keys[0].sk_flags & types_scan::scankey::SK_SEARCHARRAY, types_scan::scankey::SK_SEARCHARRAY);
    let mut src = IndexSource::new(r);
    src.bind(&b).unwrap();
    let rows = |src: &mut IndexSource, word: u64, null: u8| {
        src.seek(&[word], &[null]).unwrap();
        let (mut v, mut n, mut out) = ([0u64; 1], [0u8; 1], Vec::new());
        while src.next(&mut v, &mut n).unwrap() {
            out.push(v[0] as i32);
        }
        out
    };
    assert_eq!(rows(&mut src, arr(&[40, 20]), 0), vec![20, 40], "the word's array, in index order");
    assert_eq!(rows(&mut src, arr(&[50, 99, 10]), 0), vec![10, 50], "a second seek reads the second statement's array, not the first's");
    assert_eq!(rows(&mut src, 0, 1), Vec::<i32>::new(), "a NULL array word matches nothing");
    src.unbind();
    quiesced();
}

/// V4R-55 (the index N-row windows card): a batched heap walk fills N rows per window, the window doubling from one row
/// after the seek (1, 2, 4, 8, ...), each row's heap tid in the RowId lane after the attributes (hosts `tid_word`) and
/// the window's base its first row; an EvalPlanQual row is a one-row window whose RowId lane is that row's tid.
#[test]
fn a_batched_heap_walk_fills_doubling_windows_with_each_rows_tid() {
    let _g = serial();
    install();
    reset_fixture();
    let ctx = MemoryContext::new("t-nrow");
    let mcx = ctx.mcx();
    let vals: Vec<i32> = (1..=20).map(|i| i * 37 % 101).collect();
    let tids = load_rows(mcx, &vals);
    let tid_of = |v: i32| tids[vals.iter().position(|&x| x == v).unwrap()];
    let snap = static_mvcc_snapshot();
    let mut src = IndexSource::new(recipe(mcx, OP_INT4GT, F_INT4GT)).batched(true);
    src.bind(&binding_ref(&snap)).unwrap();
    src.seek(&[0], &[0]).unwrap();
    let mut lanes = [Lane::NONE; MAX_LANES];
    let (mut sizes, mut seen) = (Vec::new(), Vec::new());
    loop {
        let mut site = blank_site(lanes.as_mut_ptr());
        let mut w = Window::over(&mut site);
        let n = src.window(&mut w).unwrap();
        if n == 0 {
            break;
        }
        sizes.push(n);
        for r in 0..n as usize {
            let v = unsafe { w.lanes()[0].datum(r) } as i32;
            let word = unsafe { w.lanes()[1].datum(r) };
            let t = tid_of(v);
            assert_eq!(word, (types_tuple::ItemPointerGetBlockNumber(&t) as u64) << 16 | t.ip_posid as u64, "row {v}'s RowId word is its heap tid");
            if r == 0 {
                assert_eq!(w.base().pos, ScanPos::row(HEAP_OID, types_tuple::ItemPointerGetBlockNumber(&t), t.ip_posid), "the base is the first row");
            }
            seen.push(v);
        }
    }
    let mut want = vals.clone();
    want.sort();
    assert_eq!(seen, want, "every row once, in index order");
    assert_eq!(sizes, vec![1, 2, 4, 8, 5], "one row after the seek, doubling");
    let last = src.position();
    assert_eq!(last, ScanPos::row(HEAP_OID, 0, tid_of(*want.last().unwrap()).ip_posid));
    // a second seek starts over at one row
    src.seek(&[90], &[0]).unwrap();
    let mut site = blank_site(lanes.as_mut_ptr());
    let mut w = Window::over(&mut site);
    assert_eq!(src.window(&mut w).unwrap(), 1);
    // an EvalPlanQual row: one row, its RowId lane set
    src.substitute(last);
    src.seek(&[0], &[0]).unwrap();
    let mut site = blank_site(lanes.as_mut_ptr());
    let mut w = Window::over(&mut site);
    assert_eq!(src.window(&mut w).unwrap(), 1);
    assert_eq!(unsafe { w.lanes()[1].datum(0) }, last.off as u64, "the substituted row's tid (block 0)");
    src.unbind();
    quiesced();
}

/// D409: a btree walk's `column IS [NOT] NULL` index qual is PostgreSQL's SK_SEARCHNULL / SK_SEARCHNOTNULL key
/// (`ExecIndexBuildScanKeys`: `SK_ISNULL | SK_SEARCHNULL`, no argument, so no key word), and a substituted row rechecks
/// the column's nullness (`IndexRecheck`).
#[test]
fn a_null_test_index_qual_is_a_search_null_key_without_a_word() {
    use types_scan::scankey::{SK_ISNULL, SK_SEARCHNOTNULL, SK_SEARCHNULL};
    let _g = serial();
    install();
    reset_fixture();
    let ctx = MemoryContext::new("t");
    let mcx = ctx.mcx();
    load_rows(mcx, &[30, 10, 20]);
    let snap = static_mvcc_snapshot();
    let b = binding_ref(&snap);
    let recipe = |t: types_nodes::primnodes::NullTestType| {
        let heap = Relation::open(heap_relation_data(mcx), None);
        let index = Relation::open(index_relation_data(mcx, false), None);
        let var = types_nodes::Node::mk_var(mcx, INDEX_VAR, 1, types_core::INT4OID, -1, 0, 0).unwrap();
        let nt = types_nodes::Node::mk(mcx, types_nodes::primnodes::NullTest { arg: Some(var), nulltesttype: t, argisrow: false, location: -1 }).unwrap();
        SiteRecipe::from_plan(&heap, &index, &types_nodes::NodeList::make1(mcx, nt).unwrap(), &types_nodes::NodeList::nil(), AccessShareLock, &[1]).expect("a btree null test builds (D409)")
    };
    let rows = |src: &mut IndexSource| {
        src.seek(&[], &[]).unwrap();
        let (mut v, mut n, mut out) = ([0u64; 1], [0u8; 1], Vec::new());
        while src.next(&mut v, &mut n).unwrap() {
            out.push(v[0] as i32);
        }
        out
    };
    let r = recipe(types_nodes::primnodes::NullTestType::IS_NOT_NULL);
    assert_eq!((r.keys[0].sk_flags, r.keys[0].sk_attno, r.words.len(), r.host_keys[0]), (SK_ISNULL | SK_SEARCHNOTNULL, 1, 0, true));
    let mut src = IndexSource::new(r);
    src.bind(&b).unwrap();
    assert_eq!(rows(&mut src), vec![10, 20, 30], "IS NOT NULL: every non-null key, in index order");
    let pos = src.position();
    src.substitute(pos);
    assert_eq!(rows(&mut src), vec![30], "a substituted non-null row passes IS NOT NULL");
    src.unbind();
    let r = recipe(types_nodes::primnodes::NullTestType::IS_NULL);
    assert_eq!((r.keys[0].sk_flags, r.words.len()), (SK_ISNULL | SK_SEARCHNULL, 0));
    let mut src = IndexSource::new(r);
    src.bind(&b).unwrap();
    assert_eq!(rows(&mut src), Vec::<i32>::new(), "IS NULL over non-null keys finds nothing");
    src.substitute(pos);
    assert_eq!(rows(&mut src), Vec::<i32>::new(), "a substituted non-null row fails IS NULL");
    src.unbind();
    quiesced();
}
