//! D4587 (step 3, the production question): with no `clear` (the tests' reset; nothing in a server calls it), takes,
//! puts and trims (the ladder's `trim_to`, the pool's and the idle release's `trim`) never take the depot's byte count
//! below zero: each region's bytes are added under the lock that parks it before any thread can take it out, and each
//! is subtracted once. Four threads take and put back while one trims, for 3 s; the count never reads wrapped, and at
//! rest it is the parked regions' bytes (a trim empties both). Green before and after the D4587 fix.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gravity_driver::{REGIONS, REGION_MIN};

const WRAPPED: usize = 1 << 62;

#[test]
fn takes_puts_and_trims_never_wrap_the_depot_count() {
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
    let trims = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut n = 0u64;
            while !stop.load(Ordering::Relaxed) {
                if n % 2 == 0 { REGIONS.trim_to(REGION_MIN * 4) } else { REGIONS.trim() };
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
    let trimmed = trims.join().expect("the trimming thread");
    let s = REGIONS.stats();
    eprintln!("d4587 production ops: {} takes + puts, {trimmed} trims, {reads} reads: {wrapped} wrapped, {panicked} thread(s) panicked; at the end {} entries, {} B", ops.load(Ordering::Relaxed), s.entries, s.bytes);
    assert_eq!((wrapped, panicked), (0, 0), "the depot's count went below zero without a clear");
    REGIONS.trim();
    assert_eq!((REGIONS.stats().entries, REGIONS.stats().bytes), (0, 0), "trimmed: nothing parked, nothing counted");
}
