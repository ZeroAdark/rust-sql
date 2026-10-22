use gravity_driver_rig as common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;

use common::*;
use gravity_driver::{Entry, Error, Node, PullAdapter};
use gravity_program::{Generation, Handle, PlanKey, Program};

struct Counting;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static TRACKED: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static TOKEN: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
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

static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn track() -> std::sync::MutexGuard<'static, ()> {
    let g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    TRACKED.store(token(), Ordering::Relaxed);
    g
}

fn allocs() -> usize {
    ALLOCS.load(Ordering::Relaxed)
}

const KEY: PlanKey = PlanKey { handle: Handle(1), generation: Generation(1) };
const NULLS: [u8; 4] = [0; 4];
const CYCLES: usize = 10_000;

struct Shape {
    name: &'static str,
    program: fn() -> Program,
    fills: [Fill; 4],
    params: &'static [u64],
    rows: u64,
}

fn shapes() -> Vec<Shape> {
    vec![
        Shape { name: "lookup3", program: || p_lookup3(64), fills: [customer, no_rows, no_rows, no_rows], params: &[1, 2, 3], rows: 1 },
        Shape { name: "lookup4/cap2", program: || p_lookup4(2), fills: [by_last_name, no_rows, no_rows, no_rows], params: &[1, 2, 3, 4], rows: 5 },
        Shape { name: "update", program: p_update, fills: [stock, no_rows, no_rows, no_rows], params: &[1, 77], rows: 1 },
        Shape { name: "join/cap4", program: || p_join(4), fills: [orders, order_lines, no_rows, no_rows], params: &[1, 2, 30], rows: 6 },
        Shape { name: "lock", program: p_lock, fills: [five_stock, no_rows, no_rows, no_rows], params: &[1, 2], rows: 5 },
    ]
}

fn cycle(entry: &mut Entry, program: &Arc<Program>, derive: &mut FakeDerive, params: &[u64], flag: &AtomicU8) -> Result<u64, Error> {
    let sink = FakeSink::new();
    let mut sink_ref = &sink;
    let mut b = binding(params, &NULLS, &mut sink_ref, flag);
    let mut exec = entry.start(Some(KEY), program, derive, &mut b, lease(gravity_driver::Grant::of(8 << 20)))?;
    let r = entry.run_to_end(&mut exec, &mut b);
    entry.end(exec, r.is_ok());
    r.map(|_| b.rows_processed)
}

#[test]
fn steady_state_cycles_allocate_nothing_for_every_shape() {
    let _g = track();
    for s in shapes() {
        let program = Arc::new((s.program)());
        let mut derive = FakeDerive::new(s.fills);
        let mut entry = Entry::new(4);
        let flag = AtomicU8::new(0);
        assert_eq!(cycle(&mut entry, &program, &mut derive, s.params, &flag).unwrap(), s.rows, "{}", s.name);
        let warm = allocs();
        for i in 0..CYCLES {
            assert_eq!(cycle(&mut entry, &program, &mut derive, s.params, &flag).unwrap(), s.rows);
            assert_eq!(allocs() - warm, 0, "{}: cycle {i} allocated", s.name);
        }
        flag.store(1, Ordering::Release);
        assert_eq!(cycle(&mut entry, &program, &mut derive, s.params, &flag), Err(Error::Cancelled), "{}", s.name);
        assert_eq!(allocs() - warm, 0, "{}: the cancelled cycle allocated", s.name);
        flag.store(0, Ordering::Release);
        assert_eq!(cycle(&mut entry, &program, &mut derive, s.params, &flag).unwrap(), s.rows);
        assert_eq!(allocs() - warm, 0, "{}: the retry after cancel allocated", s.name);
        let st = entry.stats();
        assert_eq!((st.built, st.reused, st.ended_failed), (1, CYCLES as u64 + 2, 1), "{}", s.name);
    }
}

static PARAMS4: [u64; 4] = [1, 2, 3, 4];
static FLAG: AtomicU8 = AtomicU8::new(0);
static NULLS4: [u8; 4] = [0; 4];

fn adapter_cycle(entry: &mut Entry, adapter: &mut PullAdapter<'static>, program: &Arc<Program>, derive: &mut FakeDerive) -> usize {
    adapter.open(entry, Some(KEY), program, derive, parts(&PARAMS4, &NULLS4, &FLAG), lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    let mut n = 0;
    let mut count = |_: &[u64], _: &[u8]| {
        n += 1;
        Ok(())
    };
    adapter.fetch(entry, 3, &mut count).unwrap();
    adapter.fetch(entry, 3, &mut count).unwrap();
    adapter.fetch(entry, usize::MAX, &mut count).unwrap();
    adapter.close(entry, true);
    // the bound-Execute shape: 5, then the rest, on a fresh open
    adapter.open(entry, Some(KEY), program, derive, parts(&PARAMS4, &NULLS4, &FLAG), lease(gravity_driver::Grant::of(8 << 20))).unwrap();
    adapter.fetch(entry, 5, &mut count).unwrap();
    assert!(!adapter.is_done());
    adapter.fetch(entry, 15, &mut count).unwrap();
    assert!(adapter.is_done());
    adapter.close(entry, true);
    n
}

#[test]
fn pull_adapter_cycles_allocate_nothing() {
    let _g = track();
    let program = Arc::new(p_lookup4(4));
    let mut derive = FakeDerive::new([ten_rows, no_rows, no_rows, no_rows]);
    let mut entry = Entry::new(4);
    let before = allocs();
    let mut adapter = PullAdapter::new(2, 4);
    assert_eq!(allocs() - before, 4, "PullAdapter::new = two boxes + two buffers");
    assert_eq!(adapter_cycle(&mut entry, &mut adapter, &program, &mut derive), 20, "3+3+rest, then 5+15");
    let warm = allocs();
    for i in 0..200 {
        assert_eq!(adapter_cycle(&mut entry, &mut adapter, &program, &mut derive), 20, "3+3+rest, then 5+15");
        assert_eq!(adapter.entries(), 3);
        assert_eq!(allocs() - warm, 0, "adapter cycle {i} allocated");
    }
}

#[test]
fn error_cycles_allocate_nothing() {
    let _g = track();
    let program = Arc::new(p_lookup4(2));
    let mut derive = FakeDerive::new([by_last_name, no_rows, no_rows, no_rows]);
    derive.fail_next = Some((0, 3));
    let mut entry = Entry::new(4);
    let flag = AtomicU8::new(0);
    assert!(cycle(&mut entry, &program, &mut derive, &[1, 2, 3, 4], &flag).is_err());
    let warm = allocs();
    for _ in 0..50 {
        assert_eq!(cycle(&mut entry, &program, &mut derive, &[1, 2, 3, 4], &flag), Err(Error::host("index read failed")));
    }
    assert_eq!(allocs() - warm, 0);
}

#[test]
fn cold_costs_are_pinned() {
    let _g = track();
    // pagepool P3 (the forced-on arm, PGRUST_POOL_GRAVITY=1): the pool armed before any count, so an arena block (the
    // staging arena's, the program's, the Node's) is the pool's, no allocator call, whichever test of this binary ran first
    arm_pool();
    let pool = pool_armed() as usize;
    let mut derive = FakeDerive::new([orders, order_lines, no_rows, no_rows]);
    let before = allocs();
    let program = p_join(64);
    assert_eq!(allocs() - before, 3 - 2 * pool, "Program::new = the staging arena, the pipeline list, the program's arena block");
    let program = Arc::new(program);
    assert_eq!(allocs() - before, 4 - 2 * pool, "Arc<Program> = one");
    let node = Node::build(program.clone(), &mut derive);
    assert_eq!(allocs() - before, 5 - 3 * pool, "Node::build = one arena block");
    node.teardown();
    let entry = Entry::new(8);
    assert_eq!(allocs() - before, 5 - 3 * pool, "Entry::new = none: its park's slots grow with its puts (D3620)");
    drop(entry);
    let mut entry = Entry::new(8);
    let flag = AtomicU8::new(0);
    drop(lease(gravity_driver::Grant::of(8 << 20)));
    let before = allocs();
    cycle(&mut entry, &program, &mut derive, &[1, 2, 30], &flag).unwrap();
    assert_eq!(allocs() - before, 3 - pool, "first cycle = Node::build, its box and the park's slot table (its first put, D3620)");
}
