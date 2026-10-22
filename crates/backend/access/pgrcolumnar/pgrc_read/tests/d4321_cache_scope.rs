//! D4321 step 1 (LADDER-DESIGN section 6, user-approved into class 2): the allocations that become part-cache images are
//! made inside a cache scope (`memledger::arena::cache_scope`), so the global allocator puts them on the cache arena's
//! heap and an evicted image frees pages that hold cache images only. Two roads make an image: `SegBuf::build` (an
//! extent read, a raw copy) and a wrapped extent's rebuild adopted as the image (`OpenPart::unwrapped_extent_bytes`).
//! The probe allocator below notes, for each allocation of the watched size, whether this thread was inside a cache scope
//! (the global allocator's own test). Without the scopes (the arena's tip alone) every image is made outside one.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;

use pgrc_read::SegBuf;

/// The size the probe watches (0: none), and the watched allocations made inside and outside a cache scope.
static WATCH: AtomicU64 = AtomicU64::new(0);
static INSIDE: AtomicU64 = AtomicU64::new(0);
static OUTSIDE: AtomicU64 = AtomicU64::new(0);

struct Probe;

// SAFETY: every call goes to `System` unchanged; the probe only counts (atomics and the scope's thread-local depth).
unsafe impl GlobalAlloc for Probe {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let w = WATCH.load(Relaxed);
        if w != 0 && l.size() as u64 == w {
            if memledger::arena::in_cache_scope() {
                INSIDE.fetch_add(1, Relaxed);
            } else {
                OUTSIDE.fetch_add(1, Relaxed);
            }
        }
        // SAFETY: the caller's layout, passed through.
        unsafe { System.alloc(l) }
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: a block `alloc` above took from `System` with this layout.
        unsafe { System.dealloc(p, l) }
    }
}

#[global_allocator]
static GLOBAL: Probe = Probe;

/// The probe is process-wide: the tests take turns.
static ONE: Mutex<()> = Mutex::new(());

/// Watch `size` while `f` runs: (inside, outside) a cache scope.
fn watched(size: u64, f: impl FnOnce()) -> (u64, u64) {
    // as the server is once allocarena installed the heap: a scope's test reads true inside one
    memledger::arena::set_cache_heap_ready();
    let (i0, o0) = (INSIDE.load(Relaxed), OUTSIDE.load(Relaxed));
    WATCH.store(size, Relaxed);
    f();
    WATCH.store(0, Relaxed);
    (INSIDE.load(Relaxed) - i0, OUTSIDE.load(Relaxed) - o0)
}

#[test]
fn a_built_image_is_allocated_inside_a_cache_scope() {
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    // an odd size of whole words no other allocation of the call shares
    let n = 3_000_008usize;
    let v: Vec<u8> = (0..n).map(|i| (i % 251) as u8).collect();
    let mut img = None;
    let (inside, outside) = watched(n as u64, || img = Some(SegBuf::from_bytes(&v).expect("built")));
    assert_eq!(img.expect("built").bytes(), &v[..]);
    assert_eq!((inside, outside), (1, 0), "the image's one allocation is the cache arena's");
    assert!(!memledger::arena::in_cache_scope(), "the scope ends with the build");
}

/// Every cache scope names its owner: an image no reader names (`SegBuf::from_bytes`) is charged to the thread's context
/// (its running statement here) from its allocation, carried by the image and given back at its drop. A plain scope
/// charged no one: the image's bytes stayed on the thread's account.
#[test]
fn an_image_no_reader_names_is_charged_to_the_threads_context() {
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    std::thread::spawn(|| {
        memledger::arena::set_cache_heap_ready();
        memledger::bind_backend(3962, 1);
        memledger::statement_begin();
        let (o, _) = memledger::charge::context();
        let committed = || memledger::charge::owner_row(o).committed;
        let before = committed();
        let n = 1_000_008usize;
        let v: Vec<u8> = (0..n).map(|i| (i % 251) as u8).collect();
        let img = SegBuf::from_bytes(&v).expect("built");
        assert!(committed() - before >= n as i64, "the image is its statement's: {} of {n}", committed() - before);
        drop(img);
        assert_eq!(committed(), before, "its drop gives the charge back");
        memledger::statement_end();
        memledger::unbind_backend();
    })
    .join()
    .unwrap();
}
