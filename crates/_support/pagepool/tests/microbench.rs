//! take / give per op against mimalloc's malloc / free, warm (every take a shelf hit): one block at a time, and 64
//! taken then 64 given. ns/op is printed (MEASURED), never judged; PAGEPOOL_BIG_TESTS=1 runs 20 M ops per cell.
#![cfg(not(target_family = "wasm"))]

mod common;
use common::*;
use core::hint::black_box;
use core::ptr::NonNull;
use mimalloc::MiMalloc;
use pagepool::*;
use std::alloc::{GlobalAlloc, Layout};
use std::time::Instant;

const DEPTH: usize = 64;

fn ns_per_op(ops: usize, f: impl FnOnce()) -> f64 {
    let t0 = Instant::now();
    f();
    t0.elapsed().as_nanos() as f64 / ops as f64
}

fn pool_one(n: usize, size: usize, h: Hint) -> f64 {
    ns_per_op(n, || {
        for _ in 0..n {
            let p = take(size, 16, h).unwrap();
            // SAFETY: taken above, not used again.
            unsafe { give(black_box(p).cast(), size) };
        }
    })
}

fn pool_one_hinted(n: usize, size: usize, h: Hint) -> f64 {
    ns_per_op(n, || {
        for _ in 0..n {
            let p = take(size, 16, h).unwrap();
            // SAFETY: taken above with these three, not used again.
            unsafe { give_hinted(black_box(p).cast(), size, 16, h) };
        }
    })
}

fn pool_batch(n: usize, size: usize, h: Hint) -> f64 {
    let mut held = [NonNull::<u8>::dangling(); DEPTH];
    ns_per_op(n, || {
        for _ in 0..n / DEPTH {
            for slot in held.iter_mut() {
                *slot = take(size, 16, h).unwrap().cast();
            }
            for &p in black_box(&held) {
                // SAFETY: taken above, not used again.
                unsafe { give(p, size) };
            }
        }
    })
}

fn mi_one(n: usize, size: usize) -> f64 {
    let l = Layout::from_size_align(size, 16).unwrap();
    ns_per_op(n, || {
        for _ in 0..n {
            // SAFETY: a non-zero layout; freed with the same layout.
            unsafe {
                let p = MiMalloc.alloc(l);
                MiMalloc.dealloc(black_box(p), l);
            }
        }
    })
}

fn mi_batch(n: usize, size: usize) -> f64 {
    let l = Layout::from_size_align(size, 16).unwrap();
    let mut held = [core::ptr::null_mut::<u8>(); DEPTH];
    ns_per_op(n, || {
        for _ in 0..n / DEPTH {
            for slot in held.iter_mut() {
                // SAFETY: as above.
                *slot = unsafe { MiMalloc.alloc(l) };
            }
            for &p in black_box(&held) {
                // SAFETY: as above.
                unsafe { MiMalloc.dealloc(p, l) };
            }
        }
    })
}

#[test]
fn microbench() {
    setup();
    let n = if big() { 20_000_000 } else { 200_000 };
    let h = Hint::new(Class::Task, Tag::MCX);
    let only: Option<usize> = std::env::var("PAGEPOOL_BENCH_KIB").ok().and_then(|v| v.parse().ok());
    for size in [1 << 10, 8 << 10, 64 << 10, 256 << 10].into_iter().filter(|s| only.is_none_or(|k| k << 10 == *s)) {
        pool_batch(DEPTH * 16, size, h);
        mi_batch(DEPTH * 16, size);
        let (p1, ph, m1) = (pool_one(n, size, h), pool_one_hinted(n, size, h), mi_one(n, size));
        let (pb, mb) = (pool_batch(n, size, h), mi_batch(n, size));
        eprintln!(
            "MEASURED take+give {:>3} KiB: pool {p1:.1} ns, give_hinted {ph:.1} ns, mimalloc {m1:.1} ns ({:.2}x / {:.2}x) one at a time; pool {pb:.1} vs {mb:.1} ns ({:.2}x) {DEPTH} deep",
            size >> 10,
            p1 / m1,
            ph / m1,
            pb / mb
        );
    }
    let threads = if big() { 256 } else { 16 };
    let firsts: Vec<f64> = (0..threads)
        .map(|_| {
            std::thread::spawn(move || {
                let t0 = Instant::now();
                let p = take(64 << 10, 16, h).unwrap();
                let first = t0.elapsed().as_nanos() as f64;
                // SAFETY: taken above, not used again.
                unsafe { give_hinted(p.cast(), 64 << 10, 16, h) };
                first
            })
            .join()
            .unwrap()
        })
        .collect();
    let mut sorted = firsts.clone();
    sorted.sort_by(f64::total_cmp);
    eprintln!(
        "MEASURED a fresh thread's first 64 KiB take: median {:.0} ns, max {:.0} ns over {threads} threads",
        sorted[threads / 2],
        sorted[threads - 1]
    );
    flush_shelf();
    balanced();
}
