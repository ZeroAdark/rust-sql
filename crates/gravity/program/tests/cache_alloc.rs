use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use gravity_abi::Ctx;
use gravity_program::{Code, Generation, Handle, Layout as L, PlanKey, Program, ProgramCache, SiteDesc, WritePlan};

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

unsafe extern "C-unwind" fn noop(_ctx: *mut Ctx) -> i64 {
    0
}

#[test]
fn hits_do_not_allocate() {
    let _g = track();
    let layout = L { nsites: 1, out_natts: 0, out_rows: 1, scratch_bytes: 0, exec_bytes: 0 };
    let mut c = ProgramCache::new(1 << 20, 16);
    let k = PlanKey { handle: Handle(1), generation: Generation(1) };
    let mut keys = Vec::new();
    let st = gravity_program::Arena::new(1 << 12);
    let a = st.handle();
    for i in 0..8u32 {
        let p = Arc::new(Program::new(layout, &[SiteDesc::index_source(a, i, i + 1, &[1], &[0], &[])], &[], WritePlan::SELECT, Code::hand(noop)));
        keys.push(p.key);
        c.insert(Some(PlanKey { handle: Handle(i as u64), generation: Generation(1) }), p);
    }
    let missing = Program::new(layout, &[SiteDesc::index_source(a, 99, 100, &[1], &[0], &[])], &[], WritePlan::SELECT, Code::hand(noop)).key;
    let before = ALLOCS.load(Ordering::Relaxed);
    for _ in 0..1000 {
        let p = c.l1(k).unwrap();
        assert_eq!(p.key, keys[1]);
        let q = c.l2(keys[5]).unwrap();
        assert_eq!(q.key, keys[5]);
        assert!(c.l1(PlanKey { handle: Handle(99), generation: Generation(1) }).is_none());
        assert!(c.l2(missing).is_none());
    }
    assert_eq!(ALLOCS.load(Ordering::Relaxed) - before, 0);
}
