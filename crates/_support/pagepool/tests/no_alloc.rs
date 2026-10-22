//! No heap use on take / give / give_many / flush_shelf after init (P6: the pool is the heap).

mod common;
use common::*;
use core::ptr::NonNull;
use pagepool::*;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::alloc::{GlobalAlloc, Layout, System};

/// The armed thread (pthread_self; 0 = none) and its heap calls.
static ARMED: AtomicUsize = AtomicUsize::new(0);
static COUNT: AtomicU64 = AtomicU64::new(0);

struct Counting;

fn me() -> usize {
    // SAFETY: no preconditions.
    unsafe { libc::pthread_self() as usize }
}

fn note() {
    if ARMED.load(Relaxed) == me() {
        COUNT.fetch_add(1, Relaxed);
    }
}

// SAFETY: forwards to System; counts on the armed thread only.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc(l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc_zeroed(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        note();
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        note();
        unsafe { System.realloc(p, l, n) }
    }
}

#[global_allocator]
static A: Counting = Counting;

const SIZES: [usize; 10] = [1024, 3000, 8192, 65536, 131072, 200_000, 1 << 20, 4 * MIB, 6 * MIB + 1, 40 * MIB];

fn round() {
    let h = Hint::new(Class::Session, Tag::MCX);
    let mut held = [(NonNull::<u8>::dangling(), 0usize); SIZES.len()];
    for (k, &s) in SIZES.iter().enumerate() {
        held[k] = (take(s, 16, h).unwrap().cast(), s);
    }
    for &(p, s) in &held[..4] {
        // SAFETY: taken above, not used again.
        unsafe { give(p, s) };
    }
    // SAFETY: as above.
    unsafe { give_many(held[4..].iter().copied()) };
    for _ in 0..64 {
        let p = take(2048, 16, h).unwrap();
        // SAFETY: as above.
        unsafe { give(p.cast(), 2048) };
    }
    flush_shelf();
}

#[test]
fn take_and_give_use_no_heap() {
    setup();
    round();
    round();
    ARMED.store(me(), Relaxed);
    for _ in 0..200 {
        round();
    }
    ARMED.store(0, Relaxed);
    assert_eq!(COUNT.load(Relaxed), 0, "heap calls on the take / give / flush paths");
    balanced();
}
