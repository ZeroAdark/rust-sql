use gravity_hosts_rig as common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use common::*;
use gravity_abi::ScanPos;
use gravity_driver::{BindingRef, CommandId, Identity, Locker, Outcome, PerTuple, Pins, Snapshot, Source};
use gravity_hosts::{IndexSource, LockHost, SiteRecipe};
use gravity_program::{LockKind, WaitPolicy, WritePlan};
use mcx::MemoryContext;
use types_rel::Relation;
use types_snapshot::SnapshotData;
use types_storage::lock::AccessShareLock;
use types_tuple::{ItemPointerGetBlockNumber, ItemPointerGetOffsetNumber};

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

static FLAG: AtomicU8 = AtomicU8::new(0);
const CYCLES: usize = 10_000;

fn binding_ref(snap: &Rc<SnapshotData<'static>>) -> BindingRef<'static> {
    BindingRef { snapshot: Snapshot(snap as *const _ as u64), command_id: CommandId(0), params: &[], abort_flag: &FLAG, pins: Pins(0), per_tuple: PerTuple(0), identity: Identity::default(), resolved: None, ordinal: None }
}

fn tracked(f: impl FnOnce()) -> usize {
    TRACKED.store(token(), Ordering::Relaxed);
    let before = ALLOCS.load(Ordering::Relaxed);
    f();
    let n = ALLOCS.load(Ordering::Relaxed) - before;
    TRACKED.store(0, Ordering::Relaxed);
    n
}

#[test]
fn ten_thousand_probe_and_lock_cycles_allocate_nothing() {
    let _g = serial();
    install();
    set_unique_index(false);
    reset_fixture();
    let ctx = MemoryContext::new("t");
    let mcx = ctx.mcx();
    let tids = load_rows(mcx, &[1, 2, 3, 4, 5]);
    let snap = static_mvcc_snapshot();
    let b = binding_ref(&snap);
    let heap = Relation::open(heap_relation_data(mcx), None);
    let index = Relation::open(index_relation_data(mcx, false), None);
    let quals = indexqual_param(mcx, INDEX_VAR, OP_INT4EQ, F_INT4EQ);
    let mut src = IndexSource::new(SiteRecipe::from_plan(&heap, &index, &quals, &types_nodes::NodeList::nil(), AccessShareLock, &[1]).unwrap());
    let mut lock = LockHost::new(&WritePlan::locking(HEAP_OID, LockKind::Exclusive, WaitPolicy::Block));
    let pos = ScanPos::new(HEAP_OID, ItemPointerGetBlockNumber(&tids[2]), ItemPointerGetOffsetNumber(&tids[2]));
    let (mut v, mut n) = ([0u64; 1], [0u8; 1]);
    for _ in 0..3 {
        src.bind(&b).unwrap();
        src.seek(&[3], &[0]).unwrap();
        assert!(src.next(&mut v, &mut n).unwrap());
        src.unbind();
        lock.bind(&b).unwrap();
        assert_eq!(lock.lock(pos).unwrap(), Outcome::Done);
        lock.unbind();
    }
    let reads = tracked(|| {
        for _ in 0..CYCLES {
            src.bind(&b).unwrap();
            src.seek(&[3], &[0]).unwrap();
            assert!(src.next(&mut v, &mut n).unwrap() && v[0] == 3);
            assert_eq!(src.position(), pos);
            assert!(!src.next(&mut v, &mut n).unwrap());
            src.unbind();
        }
    });
    assert_eq!(reads, 0, "{reads} heap allocations in {CYCLES} bind/seek/next/unbind cycles");
    let locks = tracked(|| {
        for _ in 0..CYCLES {
            lock.bind(&b).unwrap();
            assert_eq!(lock.lock(pos).unwrap(), Outcome::Done);
            lock.unbind();
        }
    });
    assert_eq!(locks, 0, "{locks} heap allocations in {CYCLES} bind/lock/unbind cycles");
    drop(src);
    drop(lock);
    quiesced();
}
