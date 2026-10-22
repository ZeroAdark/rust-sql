//! The small tier (`PoolAlloc`) against mimalloc (the vendored 3.3.2 with the server's features: extended, no_thp,
//! override), both called through `GlobalAlloc`: single-thread alloc + free pairs, a churn mix, a cross-thread free
//! storm, and a fresh thread's faults on a warm allocator. MEASURED lines only, never judged.
//! SMALL_BENCH_ARMS=pool,mi (default both, in that order) and SMALL_BENCH_CELLS=single,churn,storm,fresh pick the work;
//! run one arm per process for the fresh cell. PAGEPOOL_BIG_TESTS=1 runs the full counts.
#![cfg(all(feature = "global", not(target_family = "wasm")))]

mod common;
use common::*;
use core::hint::black_box;
use core::sync::atomic::{AtomicUsize, Ordering::*};
use mimalloc::MiMalloc;
use pagepool::PoolAlloc;
use std::alloc::{GlobalAlloc, Layout};
use std::time::{Duration, Instant};

const DEPTH: usize = 64;

/// Each arm behind one out-of-line call, as `__rust_alloc` / `__rust_dealloc` put it in the server (PoolAlloc's fast
/// path inlines into it; mimalloc's is a further call into C).
struct Shim<A>(A);

// SAFETY: forwards.
unsafe impl<A: GlobalAlloc> GlobalAlloc for Shim<A> {
    #[inline(never)]
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        unsafe { self.0.alloc(l) }
    }
    #[inline(never)]
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { self.0.dealloc(p, l) }
    }
}
const ALIGN: usize = 8;

fn lay(size: usize) -> Layout {
    Layout::from_size_align(size, ALIGN).unwrap()
}

fn ns_per(ops: usize, f: impl FnOnce()) -> f64 {
    let t0 = Instant::now();
    f();
    t0.elapsed().as_nanos() as f64 / ops as f64
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    /// Log-uniform in [lo, hi].
    fn log_size(&mut self, lo: usize, hi: usize) -> usize {
        let u = (self.next() >> 11) as f64 / (1u64 << 53) as f64;
        let (a, b) = ((lo as f64).ln(), (hi as f64 + 1.0).ln());
        ((a + u * (b - a)).exp() as usize).clamp(lo, hi)
    }
}

fn sizes(n: usize, lo: usize, hi: usize, seed: u64) -> Vec<usize> {
    let mut r = Rng(seed);
    (0..n).map(|_| r.log_size(lo, hi)).collect()
}

fn one<A: GlobalAlloc>(a: &A, n: usize, size: usize) -> f64 {
    let l = lay(size);
    ns_per(n, || {
        for _ in 0..n {
            // SAFETY: a non-zero layout; freed with the same layout.
            unsafe {
                let p = a.alloc(l);
                a.dealloc(black_box(p), l);
            }
        }
    })
}

fn deep<A: GlobalAlloc>(a: &A, n: usize, size: usize) -> f64 {
    let l = lay(size);
    let mut held = [core::ptr::null_mut::<u8>(); DEPTH];
    ns_per(n, || {
        for _ in 0..n / DEPTH {
            for slot in held.iter_mut() {
                // SAFETY: as above.
                *slot = unsafe { a.alloc(l) };
            }
            for &p in black_box(&held) {
                // SAFETY: as above.
                unsafe { a.dealloc(p, l) };
            }
        }
    })
}

/// `slots` live blocks; each op frees a random slot's block and puts a new one of the next size there.
fn churn<A: GlobalAlloc>(a: &A, sz: &[usize], idx: &[u32], slots: usize) -> f64 {
    let mut live: Vec<(*mut u8, usize)> = vec![(core::ptr::null_mut(), 0); slots];
    let t = ns_per(sz.len(), || {
        for (k, &s) in sz.iter().enumerate() {
            let i = idx[k] as usize;
            let (p, ps) = live[i];
            // SAFETY: our blocks with their own layouts.
            unsafe {
                if !p.is_null() {
                    a.dealloc(p, lay(ps));
                }
                live[i] = (a.alloc(lay(s)), s);
            }
        }
    });
    for (p, s) in live {
        if !p.is_null() {
            // SAFETY: as above.
            unsafe { a.dealloc(p, lay(s)) };
        }
    }
    t
}

#[repr(align(128))]
struct Pad(AtomicUsize);

/// A single-producer single-consumer ring of pointers.
struct Ring {
    buf: Vec<AtomicUsize>,
    head: Pad,
    tail: Pad,
}

fn wait(spins: &mut u32) {
    *spins += 1;
    if *spins > 64 {
        std::thread::yield_now();
    } else {
        core::hint::spin_loop();
    }
}

/// `threads / 2` producer / consumer pairs; the producers allocate, the consumers free. Million pairs per second.
fn storm<A: GlobalAlloc + Sync>(a: &A, threads: usize, per: usize, sz: &[usize]) -> f64 {
    let pairs = threads / 2;
    let rings: Vec<Ring> = (0..pairs)
        .map(|_| Ring { buf: (0..4096).map(|_| AtomicUsize::new(0)).collect(), head: Pad(AtomicUsize::new(0)), tail: Pad(AtomicUsize::new(0)) })
        .collect();
    let t0 = Instant::now();
    std::thread::scope(|s| {
        for (j, ring) in rings.iter().enumerate() {
            s.spawn(move || {
                let cap = ring.buf.len();
                for k in 0..per {
                    let size = sz[(k + j * 7919) % sz.len()];
                    // SAFETY: a non-zero layout; the consumer frees it with the same.
                    let p = unsafe { a.alloc(lay(size)) };
                    let t = ring.tail.0.load(Relaxed);
                    let mut spins = 0;
                    while t - ring.head.0.load(Acquire) == cap {
                        wait(&mut spins);
                    }
                    ring.buf[t % cap].store(p as usize, Relaxed);
                    ring.tail.0.store(t + 1, Release);
                }
            });
            s.spawn(move || {
                let cap = ring.buf.len();
                for k in 0..per {
                    let h = ring.head.0.load(Relaxed);
                    let mut spins = 0;
                    while ring.tail.0.load(Acquire) == h {
                        wait(&mut spins);
                    }
                    let p = ring.buf[h % cap].load(Relaxed) as *mut u8;
                    ring.head.0.store(h + 1, Release);
                    let size = sz[(k + j * 7919) % sz.len()];
                    // SAFETY: the producer's block of this layout, handed over.
                    unsafe { a.dealloc(p, lay(size)) };
                }
            });
        }
    });
    (pairs * per) as f64 / t0.elapsed().as_secs_f64() / 1e6
}

/// A thread's first allocations: `sz.len()` blocks held (each written whole, as a new session builds its state), then
/// freed. (faults while allocating, faults in all).
fn fresh_work<A: GlobalAlloc>(a: &A, sz: &[usize]) -> (i64, i64) {
    let mut held = vec![core::ptr::null_mut::<u8>(); sz.len()];
    for p in held.iter_mut() {
        // SAFETY: our own slot: touch the vector's pages before counting.
        unsafe { core::ptr::write_volatile(p, core::ptr::null_mut()) };
    }
    let f0 = minflt();
    for (p, &s) in held.iter_mut().zip(sz) {
        // SAFETY: a non-zero layout; the block is written whole, then freed with the same layout.
        unsafe {
            *p = a.alloc(lay(s));
            core::ptr::write_bytes(*p, 0x5a, s);
        }
    }
    let f1 = minflt();
    for (&p, &s) in held.iter().zip(sz) {
        // SAFETY: as above.
        unsafe { a.dealloc(p, lay(s)) };
    }
    let f2 = minflt();
    (f1 - f0, f2 - f0)
}

fn on_thread<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|s| s.spawn(f).join().unwrap())
}

fn load() -> String {
    std::fs::read_to_string("/proc/loadavg").unwrap_or_default().split_whitespace().take(3).collect::<Vec<_>>().join(" ")
}

/// `lanes` threads each run `per` sessions in a row, a new thread per session (connection churn at that concurrency),
/// each session ending with `end` when given (the server's session-end release); faults allocating per session.
fn sessions<A: GlobalAlloc + Sync>(a: &A, sz: &[usize], lanes: usize, per: usize, end: Option<fn()>) -> Vec<i64> {
    std::thread::scope(|s| {
        let hs: Vec<_> = (0..lanes)
            .map(|_| {
                s.spawn(move || {
                    (0..per)
                        .map(|_| {
                            on_thread(|| {
                                let f = fresh_work(a, sz).0;
                                if let Some(e) = end {
                                    e();
                                }
                                f
                            })
                        })
                        .collect::<Vec<i64>>()
                })
            })
            .collect();
        hs.into_iter().flat_map(|h| h.join().unwrap()).collect()
    })
}

fn summary(v: &[i64]) -> String {
    let sum: i64 = v.iter().sum();
    format!("mean {:.1} max {} last {}", sum as f64 / v.len() as f64, v.iter().max().unwrap(), v.last().unwrap())
}

fn arm<A: GlobalAlloc + Sync>(name: &str, a: &A, cells: &str, end: fn()) {
    let big = big();
    if cells.contains("single") {
        let n = if big { 20_000_000 } else { 2_000_000 };
        for size in [16, 32, 64, 128, 256, 512, 1024] {
            one(a, n / 10, size);
            deep(a, n / 10, size);
            let (o, d) = (one(a, n, size), deep(a, n, size));
            eprintln!("MEASURED single {name} {size:>4} B: one-at-a-time {o:.2} ns/pair, {DEPTH}-deep {d:.2} ns/pair (load {})", load());
        }
    }
    if cells.contains("churn") {
        let n = if big { 20_000_000 } else { 4_000_000 };
        let slots = 1024;
        let mut r = Rng(0x2545_f491_4f6c_dd1d);
        let idx: Vec<u32> = (0..n).map(|_| (r.next() % slots as u64) as u32).collect();
        for (lo, hi) in [(16, 1024), (16, 4096)] {
            let sz = sizes(n, lo, hi, 0x9e37_79b9_7f4a_7c15);
            churn(a, &sz[..n / 10], &idx[..n / 10], slots);
            let t = churn(a, &sz, &idx, slots);
            eprintln!("MEASURED churn {name} log-uniform {lo}-{hi} B, {slots} live: {t:.2} ns/op (one free + one alloc) (load {})", load());
        }
    }
    if cells.contains("storm") {
        let per = if big { 4_000_000 } else { 1_000_000 };
        let sz = sizes(1 << 16, 16, 1024, 0x51a7_e5ee_d000_0001);
        for threads in [2, 8, 32] {
            storm(a, threads, per / 10, &sz);
            let m = storm(a, threads, per, &sz);
            eprintln!("MEASURED storm {name} {threads} threads ({} pairs): {m:.2} M alloc+free / s (load {})", threads / 2, load());
        }
    }
    if cells.contains("fresh") {
        let sz = sizes(10_000, 16, 1024, 0xf1e5_4711_0000_0002);
        let bytes: usize = sz.iter().sum();
        on_thread(|| fresh_work(a, &sz));
        on_thread(|| fresh_work(a, &sz));
        let (fa, ft) = on_thread(|| fresh_work(a, &sz));
        eprintln!("MEASURED fresh {name} warm, a new thread's first 10k (16-1024 B, {} KiB): {fa} faults allocating, {ft} in all", bytes >> 10);
        for (gap_ms, threads) in [(0u64, 100usize), (50, 20)] {
            let mut v: Vec<i64> = Vec::new();
            let t0 = Instant::now();
            for _ in 0..threads {
                v.push(on_thread(|| fresh_work(a, &sz)).0);
                std::thread::sleep(Duration::from_millis(gap_ms));
            }
            let el = t0.elapsed().as_secs_f64();
            let sum: i64 = v.iter().sum();
            eprintln!(
                "MEASURED fresh {name} churn {threads} threads in a row, gap {gap_ms} ms: faults allocating mean {:.1} max {} last {} ({el:.2} s)",
                sum as f64 / threads as f64,
                v.iter().max().unwrap(),
                v.last().unwrap()
            );
        }
        let v = sessions(a, &sz, 1, 100, Some(end));
        eprintln!("MEASURED fresh {name} churn 100 in a row, each ending with the session-end release: faults allocating {}", summary(&v));
        for (lanes, end_) in [(16, None), (16, Some(end))] {
            let v = sessions(a, &sz, lanes, 10, end_);
            eprintln!(
                "MEASURED fresh {name} {lanes} concurrent lanes x 10 sessions{}: faults allocating per session {}",
                if end_.is_some() { ", session-end release" } else { "" },
                summary(&v)
            );
        }
    }
    if name == "pool" {
        eprintln!("MEASURED pool small-tier runs: {:?}; pool {:?}", pagepool::small_counts(), pagepool::stats());
    }
}

#[test]
fn small_bench() {
    assert!(pagepool::init(&pagepool::Config::from_policy()), "pagepool::init refused");
    let arms = std::env::var("SMALL_BENCH_ARMS").unwrap_or_else(|_| "pool,mi".into());
    let cells = std::env::var("SMALL_BENCH_CELLS").unwrap_or_else(|_| "single,churn,storm,fresh".into());
    for a in arms.split(',') {
        match a {
            "pool" => arm("pool", &Shim(PoolAlloc), &cells, || {
                pagepool::release_heap();
            }),
            // trunk's session-end release (postgres.rs set_allocator_release: mi_collect(true))
            // SAFETY: no preconditions.
            "mi" => arm("mi", &Shim(MiMalloc), &cells, || unsafe { libmimalloc_sys::mi_collect(true) }),
            _ => panic!("arm {a}"),
        }
    }
}
