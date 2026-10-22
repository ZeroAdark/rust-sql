//! D848: a hosted function's result region was 8 MiB, zeroed, per hosted function per lowering (hir.rs `Hosted::build`)
//! and per armed lane: under memory pressure each such lowering faulted 4 huge pages through direct reclaim and swap
//! writeout (D841: sind/erf/IS JSON lowering 0.5 ms -> 200-500 ms on box 12). The region now starts at 64 KiB,
//! uninitialized, and grows through the D641 spill path; no lowering of a hosted call allocates 4 MiB at once (the
//! rig fixture's own arena is 1 MiB).
use gravity_lowerv4_it as common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct Largest;
static ON: AtomicBool = AtomicBool::new(false);
static MAX: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Largest {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if ON.load(Ordering::Relaxed) {
            MAX.fetch_max(l.size(), Ordering::Relaxed);
        }
        System.alloc(l)
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        if ON.load(Ordering::Relaxed) {
            MAX.fetch_max(l.size(), Ordering::Relaxed);
        }
        System.alloc_zeroed(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l)
    }
}

#[global_allocator]
static A: Largest = Largest;

#[test]
fn a_hosted_lowering_allocates_no_large_region() {
    // SELECT x, sind(x), sind(x) IN (...) FROM (VALUES ...): sind is a hosted call
    let text = std::fs::read_to_string(common::plans_dir().join("regress-full/float8-010.plan")).expect("plan fixture");
    common::lower_fixture(&text).expect("decompose takes it").expect("float8-010 lowers"); // warm: seams, caches
    MAX.store(0, Ordering::Relaxed);
    ON.store(true, Ordering::Relaxed);
    let r = common::lower_fixture(&text).expect("decompose takes it").map(|_| ());
    ON.store(false, Ordering::Relaxed);
    assert!(r.is_ok(), "float8-010 lowers");
    let max = MAX.load(Ordering::Relaxed);
    assert!(max < 4 << 20, "a hosted lowering's largest allocation is {max} bytes (D848: was an 8 MiB zeroed region)");
}
