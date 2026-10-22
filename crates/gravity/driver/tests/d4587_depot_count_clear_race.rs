//! D4587 (planprod 2026-10-15, lowerv4_suite): the region depot's byte count wrapped below zero in 1 of 3 runs (park.rs
//! `put`'s add overflowed in a debug build; a release build wraps silently). A take subtracted its region's bytes after it
//! let the depot's lock go (`take_parked` after `unparked`, `take_reserved`, `take_laid`), and `clear` (the tests' reset)
//! stored 0 under the lock in between: the late subtraction took the count below zero, and the next put's add overflowed.
//! Every change to the count now happens under the lock that parks or takes the region, so the count is the parked
//! regions' bytes whenever the lock is free. One test (it drives the process-wide depot): four threads take and put back
//! while one clears, for 3 s; the count never reads wrapped and no thread panics.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gravity_driver::{REGIONS, REGION_MIN};

/// A count past this is one that went below zero (no depot parks 4 EiB).
const WRAPPED: usize = 1 << 62;

#[test]
fn the_depot_count_never_wraps_under_takes_puts_and_clears() {
    REGIONS.trim();
    let stop = Arc::new(AtomicBool::new(false));
    let ops = Arc::new(AtomicU64::new(0));
    let mut threads = Vec::new();
    for t in 0..4usize {
        let (stop, ops) = (stop.clone(), ops.clone());
        threads.push(std::thread::spawn(move || {
            let mut i = 0usize;
            while !stop.load(Ordering::Relaxed) {
                let buf = REGIONS.take(REGION_MIN * (1 + (i + t) % 4), None).expect("a region");
                REGIONS.put(buf);
                i += 1;
            }
            ops.fetch_add(i as u64, Ordering::Relaxed);
        }));
    }
    let clears = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut n = 0u64;
            while !stop.load(Ordering::Relaxed) {
                REGIONS.clear();
                n += 1;
                std::thread::yield_now();
            }
            n
        })
    };
    let (t0, mut wrapped, mut reads) = (Instant::now(), 0u64, 0u64);
    while t0.elapsed() < Duration::from_secs(3) {
        if REGIONS.parked_bytes() > WRAPPED {
            wrapped += 1;
        }
        reads += 1;
    }
    stop.store(true, Ordering::Relaxed);
    let panicked = threads.into_iter().map(|h| h.join().is_err()).filter(|&p| p).count();
    let cleared = clears.join().expect("the clearing thread");
    let s = REGIONS.stats();
    eprintln!("d4587: {} takes + puts, {cleared} clears, {reads} reads: {wrapped} wrapped, {panicked} thread(s) panicked; at the end {} entries, {} B", ops.load(Ordering::Relaxed), s.entries, s.bytes);
    assert_eq!((wrapped, panicked), (0, 0), "the depot's count went below zero ({wrapped} reads past 2^62; a put's add overflowed in {panicked} thread(s))");
    assert!(s.bytes < WRAPPED && (s.entries == 0) == (s.bytes == 0), "at rest the count is the parked regions' bytes: {s:?}");
    REGIONS.trim();
    assert_eq!((REGIONS.stats().entries, REGIONS.stats().bytes), (0, 0), "trimmed: nothing parked, nothing counted");
}
