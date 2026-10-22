//! `PoolAlloc` as this binary's global allocator (the pool starts at the first allocation, from the memory policy),
//! under a guard that counts re-entry: std collections, threads that exit holding blocks, cross-thread frees, every
//! alignment, realloc across the small / bucket / runs / span roads, zeroed blocks, usable sizes. Nothing recurses
//! (the allocator never allocates inside itself) and, once the pool is on, every block is the pool's.
#![cfg(all(feature = "global", not(target_family = "wasm")))]

use core::alloc::{GlobalAlloc, Layout};
use core::cell::Cell;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering::*};
use pagepool::PoolAlloc;
use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc;

static NESTED: AtomicU64 = AtomicU64::new(0);
static FOREIGN: AtomicU64 = AtomicU64::new(0);
static CALLS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static DEPTH: Cell<u32> = const { Cell::new(0) };
}

struct Guard(PoolAlloc);

impl Guard {
    #[inline]
    fn enter(&self) {
        CALLS.fetch_add(1, Relaxed);
        DEPTH.with(|d| {
            if d.get() > 0 {
                NESTED.fetch_add(1, Relaxed);
            }
            d.set(d.get() + 1);
        });
    }
    #[inline]
    fn leave(&self) {
        DEPTH.with(|d| d.set(d.get() - 1));
    }
    #[inline]
    fn check(&self, p: *mut u8) -> *mut u8 {
        if pagepool::on() && !p.is_null() && !pagepool::owns(p) {
            FOREIGN.fetch_add(1, Relaxed);
        }
        p
    }
}

// SAFETY: forwards to PoolAlloc.
unsafe impl GlobalAlloc for Guard {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        self.enter();
        let p = self.check(unsafe { self.0.alloc(l) });
        self.leave();
        p
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        self.enter();
        let p = self.check(unsafe { self.0.alloc_zeroed(l) });
        self.leave();
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        self.enter();
        unsafe { self.0.dealloc(p, l) };
        self.leave();
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        self.enter();
        let q = self.check(unsafe { self.0.realloc(p, l, n) });
        self.leave();
        q
    }
}

#[global_allocator]
static A: Guard = Guard(PoolAlloc);

/// One test at a time: several read process-wide counters (the pool's in-use, the small tier's counts).
static ONE: AtomicBool = AtomicBool::new(false);

struct One;

impl Drop for One {
    fn drop(&mut self) {
        ONE.store(false, Release);
    }
}

fn one() -> One {
    while ONE.compare_exchange(false, true, Acquire, Relaxed).is_err() {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    One
}

fn rng(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

fn collections(seed: u64) {
    let mut x = seed | 1;
    let mut m: HashMap<u64, String> = HashMap::new();
    let mut b: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    for i in 0..20_000u64 {
        let k = rng(&mut x) % 5000;
        m.insert(k, format!("value {i} {k}"));
        b.entry(k % 977).or_default().extend_from_slice(&i.to_le_bytes());
        if i % 7 == 0 {
            m.remove(&(rng(&mut x) % 5000));
        }
    }
    for (k, v) in &m {
        assert!(v.ends_with(&format!(" {k}")));
    }
    let total: usize = b.values().map(Vec::len).sum();
    assert_eq!(total, 20_000 * 8);
}

#[test]
fn std_under_the_pool() {
    let _one = one();
    std_under();
}

fn std_under() {
    collections(1);
    let hs: Vec<_> = (0..16).map(|t| std::thread::spawn(move || collections(t as u64 + 2))).collect();
    for h in hs {
        h.join().unwrap();
    }
    assert!(pagepool::on(), "the pool started at the first allocation");
    assert!(pagepool::global_on());
    std::thread::sleep(std::time::Duration::from_millis(10));
}

#[test]
fn threads_exit_holding_blocks_and_others_free_them() {
    let _one = one();
    let (tx, rx) = mpsc::channel::<Vec<Box<[u8]>>>();
    for round in 0..32 {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut x = round as u64 + 11;
            let v: Vec<Box<[u8]>> = (0..2000).map(|_| vec![round as u8; 1 + (rng(&mut x) % 1024) as usize].into_boxed_slice()).collect();
            tx.send(v).unwrap();
        })
        .join()
        .unwrap();
    }
    drop(tx);
    for (round, v) in rx.into_iter().enumerate() {
        for b in &v {
            assert!(b.iter().all(|&c| c == round as u8), "a block was reused while live");
        }
    }
    pagepool::release_heap();
}

#[test]
fn cross_thread_storm() {
    let _one = one();
    let (tx, rx) = mpsc::sync_channel::<Box<[u64]>>(256);
    let prod = std::thread::spawn(move || {
        let mut x = 7u64;
        for i in 0..200_000u64 {
            let n = 1 + (rng(&mut x) % 128) as usize;
            tx.send(vec![i; n].into_boxed_slice()).unwrap();
        }
    });
    let cons = std::thread::spawn(move || {
        let mut i = 0u64;
        for b in rx {
            assert!(b.iter().all(|&v| v == i));
            i += 1;
        }
        i
    });
    prod.join().unwrap();
    assert_eq!(cons.join().unwrap(), 200_000);
}

#[test]
fn alignments_sizes_and_usable() {
    let _one = one();
    for align in [1usize, 2, 4, 8, 16, 32, 64, 128, 256, 4096, 1 << 16, 1 << 21] {
        for size in [1usize, 7, 8, 9, 24, 100, 1000, 1024, 1025, 3000, 70_000, 200_000, 3 << 20] {
            let l = Layout::from_size_align(size, align).unwrap();
            // SAFETY: a non-zero layout, freed with the same.
            unsafe {
                let p = A.alloc(l);
                assert!(!p.is_null() && p as usize % align == 0, "{size} @ {align}");
                assert!(pagepool::usable_size(p) >= size, "{size} @ {align}: usable {}", pagepool::usable_size(p));
                assert!(pagepool::good_size(size, align) >= size);
                if pagepool::owns(p) {
                    assert_eq!(pagepool::usable_size(p), pagepool::good_size(size, align), "door 1's size is the block's: {size} @ {align}");
                }
                core::ptr::write_bytes(p, 0xa5, size);
                A.dealloc(p, l);
                let z = A.alloc_zeroed(l);
                assert!(core::slice::from_raw_parts(z, size).iter().all(|&b| b == 0), "zeroed {size} @ {align}");
                A.dealloc(z, l);
            }
        }
    }
}

#[test]
fn realloc_across_roads_keeps_bytes() {
    let _one = one();
    for align in [1usize, 8, 16, 64] {
        let mut size = 3usize;
        let l = Layout::from_size_align(size, align).unwrap();
        // SAFETY: as above; each realloc's result replaces the block.
        unsafe {
            let mut p = A.alloc(l);
            for i in 0..size {
                *p.add(i) = i as u8;
            }
            for new in [5usize, 17, 600, 1000, 1024, 1500, 9000, 140_000, 900_000, 5 << 20, 700_000, 2000, 900, 40, 1] {
                p = A.realloc(p, Layout::from_size_align(size, align).unwrap(), new);
                assert!(!p.is_null() && p as usize % align == 0);
                for i in 0..size.min(new) {
                    assert_eq!(*p.add(i), i as u8, "byte {i} after {size} -> {new} @ {align}");
                }
                for i in size.min(new)..new {
                    *p.add(i) = i as u8;
                }
                if pagepool::owns(p) && new < 2 << 20 {
                    assert_eq!(pagepool::usable_size(p), pagepool::good_size(new, align), "after {size} -> {new} @ {align}");
                }
                size = new;
            }
            A.dealloc(p, Layout::from_size_align(size, align).unwrap());
        }
    }
}

#[test]
fn thread_retained_reads_the_heap() {
    let _one = one();
    // relative readings only: this thread may adopt runs other tests' threads left with live blocks
    std::thread::spawn(|| {
        let mut v: Vec<Option<Box<[u8; 64]>>> = Vec::with_capacity(4000);
        v.extend((0..4000).map(|_| Some(Box::new([1u8; 64]))));
        let held = pagepool::thread_retained();
        for b in v.iter_mut().skip(1).step_by(2) {
            *b = None;
        }
        let half = pagepool::thread_retained();
        assert!((2000 * 64 - (64 << 10)..=2000 * 64).contains(&(half - held)), "held {held} half {half}");
        drop(v);
        let all = pagepool::thread_retained();
        pagepool::release_heap();
        pagepool::flush_shelf();
        let after = pagepool::thread_retained();
        assert!(after <= all, "all {all} after {after}");
    })
    .join()
    .unwrap();
}

const CACHE_SIZES: [usize; 10] = [24, 64, 200, 512, 1000, 3000, 60_000, 65_536, 70_000, 300_000];

/// One session's cache images: `n` blocks over CACHE_SIZES, each written whole.
fn cache_blocks(seed: u64, n: usize) -> Vec<(usize, Layout)> {
    let mut x = seed | 1;
    (0..n)
        .map(|i| {
            let l = Layout::from_size_align(CACHE_SIZES[(rng(&mut x) as usize + i) % CACHE_SIZES.len()], 8).unwrap();
            let p = pagepool::cache_alloc(l.size(), l.align(), i % 5 == 0);
            assert!(!p.is_null());
            // SAFETY: a fresh block of `l.size()` bytes.
            unsafe { core::ptr::write_bytes(p, seed as u8, l.size()) };
            (p as usize, l)
        })
        .collect()
}

fn cache_in_use() -> usize {
    pagepool::stats().in_use[pagepool::Tag::CACHE.0 as usize]
}

#[test]
fn cache_class_keeps_its_own_runs() {
    let _one = one();
    let general: Vec<Box<[u8]>> = (0..2000).map(|i| vec![1u8; 1 + i % 1024].into_boxed_slice()).collect();
    let cache = cache_blocks(3, 2000);
    let run = |p: usize| p & !((64 << 10) - 1);
    let general_runs: std::collections::HashSet<usize> = general.iter().map(|b| run(b.as_ptr() as usize)).collect();
    for &(p, l) in &cache {
        assert!(pagepool::is_cache(p as *const u8, l.size(), l.align()), "{} B", l.size());
        assert_eq!(pagepool::usable_size(p as *const u8), pagepool::good_size(l.size(), l.align()));
        if l.size() <= 1024 {
            assert!(!general_runs.contains(&run(p)), "a cache block in a general run");
        }
    }
    for b in &general {
        assert!(!pagepool::is_cache(b.as_ptr(), b.len(), 1));
    }
    // realloc on the cache class keeps the bytes and the class
    let (p, l) = cache[1];
    let q = pagepool::cache_realloc(p as *mut u8, l.align(), 5000);
    assert!(!q.is_null() && pagepool::is_cache(q, 5000, l.align()));
    // SAFETY: the realloc's block of 5000 bytes, the old block's bytes copied.
    unsafe {
        assert!(core::slice::from_raw_parts(q, l.size().min(5000)).iter().all(|&b| b == 3));
        A.dealloc(q, Layout::from_size_align(5000, l.align()).unwrap());
    }
    // grown the way a Vec grows (a 64 KiB block of the run bucket is a big block, not a run of the tier)
    let mut q = pagepool::cache_alloc(1, 1, true);
    let mut size = 1;
    for new in [700, 4096, 30_000, 60_000, 65_536, 131_072, 1 << 20, 3 << 20, 65_536, 1000] {
        q = pagepool::cache_realloc(q, 1, new);
        assert!(!q.is_null() && pagepool::is_cache(q, new, 1), "{size} -> {new}");
        if new < 2 << 20 {
            assert_eq!(pagepool::usable_size(q), pagepool::good_size(new, 1), "{size} -> {new}");
        }
        // SAFETY: a live block of `new` bytes, its first `size.min(new)` copied.
        unsafe {
            assert!((0..size.min(new)).all(|i| *q.add(i) == i as u8), "{size} -> {new}");
            (0..new).for_each(|i| *q.add(i) = i as u8);
        }
        size = new;
    }
    // SAFETY: ours, `size` bytes.
    unsafe { A.dealloc(q, Layout::from_size_align(size, 1).unwrap()) };
    for &(p, l) in cache.iter().filter(|&&(p0, _)| p0 != p) {
        // SAFETY: ours, freed once with its layout.
        unsafe { A.dealloc(p as *mut u8, l) };
    }
    pagepool::idle_point();
    pagepool::flush_remote();
    assert_eq!(cache_in_use(), 0, "every cache run and block came home");
}

#[test]
fn parked_builders_cache_runs_come_home_without_them() {
    let _one = one();
    // 200 sessions each build cache images over several classes, reach their idle point and park (blocked, alive); the
    // cutter frees every image, flushes its remote frees and shelf: no cache byte stays with a parked builder
    let base = cache_in_use();
    let (tx, rx) = mpsc::channel::<Vec<(usize, Layout)>>();
    let mut wake = Vec::new();
    let mut joins = Vec::new();
    for t in 0..200u64 {
        let tx = tx.clone();
        let (wtx, wrx) = mpsc::channel::<()>();
        wake.push(wtx);
        joins.push(std::thread::spawn(move || {
            let blocks = cache_blocks(t + 10, 24);
            let general: Vec<u64> = (0..100).collect();
            pagepool::idle_point();
            tx.send(blocks).unwrap();
            wrx.recv().unwrap();
            drop(general);
        }));
    }
    drop(tx);
    let all: Vec<_> = rx.iter().take(200).flatten().collect();
    assert_eq!(all.len(), 200 * 24);
    assert!(cache_in_use() > base);
    let free = |s: pagepool::Stats| (s.central + s.shelved + s.stranded + s.warm_free()) as isize;
    let (runs, before, free0) = (pagepool::small_counts(), pagepool::stats().in_use, free(pagepool::stats()));
    for &(p, l) in &all {
        // SAFETY: the builders' blocks, handed over, freed once.
        unsafe { A.dealloc(p as *mut u8, l) };
    }
    pagepool::flush_remote();
    pagepool::flush_shelf();
    assert_eq!(cache_in_use(), base, "a parked builder still holds cache bytes (in use before the cut {:?})", before);
    assert!(pagepool::small_counts().retired > runs.retired, "the swept runs went back to the pool");
    let cut = (before[pagepool::Tag::CACHE.0 as usize] - base) as isize;
    assert!(free(pagepool::stats()) - free0 >= cut, "the cut's {cut} bytes are the pool's free bytes now");
    let runs_now = || {
        let o = pagepool::occupancy();
        let (f, g): (usize, usize) = (o.free_run_bytes.iter().sum(), o.gone_run_bytes.iter().sum());
        (f - g, g)
    };
    let (resident0, gone0) = runs_now();
    let released = pagepool::purge_now(usize::MAX, None);
    let gone1 = runs_now().1;
    assert!(released >= resident0 && gone1 > gone0, "the purger releases what came home, inside partly used superpages too: {released} of {resident0}, gone {gone0} -> {gone1}");
    for w in wake {
        w.send(()).unwrap();
    }
    for j in joins {
        j.join().unwrap();
    }
}

static LATE: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

extern "C" fn late_destructor(_: *mut libc::c_void) {
    // runs after the tier's exit destructor (a later key): takes and frees through the dead heap, and leaves one block
    let v: Vec<u64> = (0..50).collect();
    assert_eq!(v.iter().sum::<u64>(), 1225);
    drop(v);
    LATE.store(Box::into_raw(Box::new([7u64; 3])) as usize, Relaxed);
}

#[test]
fn allocation_after_the_exit_destructor() {
    let _one = one();
    let before = pagepool::small_counts();
    std::thread::spawn(|| {
        let warm = vec![1u8; 100];
        let mut key: libc::pthread_key_t = 0;
        // SAFETY: a valid out pointer; the destructor lives for the process.
        assert_eq!(unsafe { libc::pthread_key_create(&mut key, Some(late_destructor)) }, 0);
        // SAFETY: a live key; a non-null value arms its destructor.
        unsafe { libc::pthread_setspecific(key, 1usize as *const libc::c_void) };
        drop(warm);
    })
    .join()
    .unwrap();
    let p = LATE.load(Relaxed) as *mut [u64; 3];
    assert!(!p.is_null(), "the late destructor ran");
    // SAFETY: the dead thread's block, handed over.
    let b = unsafe { Box::from_raw(p) };
    assert_eq!(*b, [7, 7, 7]);
    drop(b);
    assert!(pagepool::small_counts().abandoned > before.abandoned, "the dead heap kept no run (its take's run went to the abandoned list)");
    pagepool::release_heap();
}

#[test]
fn zz_nothing_recursed_or_went_to_the_system() {
    let _one = one();
    // runs last by name (the default test order is alphabetical within the binary when threads finish; the counters
    // are process-wide, so a re-entry anywhere above shows here)
    std_under();
    assert_eq!(NESTED.load(Relaxed), 0, "the allocator allocated inside itself");
    assert_eq!(FOREIGN.load(Relaxed), 0, "blocks from the system after the pool was on");
    assert!(CALLS.load(Relaxed) > 100_000);
}
