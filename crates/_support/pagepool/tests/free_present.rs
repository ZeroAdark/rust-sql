//! pppress card V: `free_present` reads the pool's present pages in runs that hold no live block, and `cache_present` its
//! present pages in CACHE-tag allocations holding one, both classified in the same sweep that counts `present`. A free
//! run's touched pages count, a live block's pages never do (touched or not), a dead run's count until released, a
//! released run reads 0, and both stay within `present` while other threads take, write and give. Its own binary: the
//! pool's sweep reads the whole pool, so the tests run one at a time, each body on a thread of its own, joined (as
//! present.rs does, D4842: a pool thread's exit moves counts).
#![cfg(target_os = "linux")]

mod common;
use common::*;
use core::ptr::NonNull;
use pagepool::*;

const RUN: usize = 64 << 10;

static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn one(body: impl FnOnce() + Send) {
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    std::thread::scope(|s| {
        if let Err(p) = s.spawn(body).join() {
            std::panic::resume_unwind(p);
        }
    });
}

/// A fresh sweep: (present, free, cache), one read of it.
fn swept() -> (usize, usize, usize) {
    refresh_present().expect("a sweep");
    let (p, f, c) = present_split().expect("present_split");
    assert!(f + c <= p, "free {f} + cache {c} within present {p}");
    assert_eq!((free_present(), cache_present()), (Some(f), Some(c)), "the one-row readers read the same sweep");
    (p, f, c)
}

/// Sets `stop` as it drops, so a failed check on the sweeping thread ends the load threads (the scope joins them)
/// and the failure is reported, not a hang.
struct StopOnDrop<'a>(&'a std::sync::atomic::AtomicBool);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Everything free released first, so a test reads only its own pages.
fn quiet() -> (usize, usize, usize) {
    flush_shelf();
    let _ = purge_now(usize::MAX >> 1, None);
    swept()
}

#[test]
fn a_free_runs_touched_pages_count_and_a_released_run_reads_zero() {
    one(|| {
        setup();
        let h = Hint::new(Class::Task, Tag::OTHER);
        let (_, f0, _) = quiet();
        // nine runs: past LIST_RUNS, so the give puts them straight back to their superpage as free runs
        let b = take(9 * RUN, RUN, h).expect("a runs block").cast::<u8>();
        let a = b.as_ptr() as usize;
        touch(a, 9 * RUN);
        let (_, f1, _) = swept();
        let own = resident(a, 9 * RUN);
        assert_eq!(own, 9 * RUN, "written: resident");
        // its superpage's other runs are free: a huge-page fault may have made them present (and free) too
        assert!(f1 <= f0 + SUPER - 9 * RUN, "a live block's pages never count: {f0} -> {f1}");
        // SAFETY: ours, given once
        unsafe { give(b, 9 * RUN) };
        let (_, f2, _) = swept();
        assert_eq!(f2 - f1, own, "given back: its touched runs are free and present ({f1} -> {f2})");
        let _ = purge_now(usize::MAX >> 1, None);
        let (_, f3, _) = swept();
        assert_eq!(resident(a, 9 * RUN), 0, "released");
        assert!(f3 + own <= f2, "a released run reads 0: {f2} -> {f3}");
        balanced();
    });
}

#[test]
fn a_live_spans_pages_never_count_and_its_dead_runs_do() {
    one(|| {
        setup();
        let h = Hint::new(Class::Task, Tag::OTHER);
        let (_, f0, c0) = quiet();
        // a pool span (under lazy_min): its superpages are wholly its own
        let s = span(4 * MIB, h).expect("a pool span");
        assert!(!s.is_lazy());
        let a = s.as_ptr() as usize;
        touch(a, MIB);
        let (_, f1, c1) = swept();
        assert_eq!(f1, f0, "a live span's touched and untouched pages never count");
        assert_eq!(c1, c0, "not a CACHE span");
        // its first MiB dead (written, so present): free until taken back
        let p = NonNull::new(a as *mut u8).expect("the span");
        // SAFETY: inside our own span; nothing reads it until take_pages
        let dead = unsafe { give_pages(p, 0, MIB) };
        assert_eq!(dead, MIB);
        let (_, f2, _) = swept();
        assert_eq!(f2 - f1, resident(a, MIB), "its dead runs' present pages count ({f1} -> {f2})");
        // SAFETY: the range we gave dead
        let _ = unsafe { take_pages(p, 0, MIB) };
        let (_, f3, _) = swept();
        assert_eq!(f3, f1, "live again: no longer free");
        // SAFETY: ours, not used again
        unsafe { give_span(s) };
        balanced();
    });
}

#[test]
fn cache_tag_allocations_count_in_cache_present() {
    one(|| {
        setup();
        let h = Hint::new(Class::Cache, Tag::CACHE);
        let (_, f0, c0) = quiet();
        let s = span(4 * MIB, h).expect("a CACHE pool span");
        let a = s.as_ptr() as usize;
        touch(a, 2 * MIB);
        let (_, f1, c1) = swept();
        assert_eq!(c1 - c0, resident(a, 4 * MIB), "a CACHE span's present pages ({c0} -> {c1})");
        assert_eq!(f1, f0, "live: not free");
        // a CACHE runs block too (past LIST_RUNS: its give puts it straight back; a short one would sit on a shelf,
        // inside a run that still reads allocated, which this count does not see)
        let b = take(9 * RUN, RUN, h).expect("a CACHE runs block").cast::<u8>();
        touch(b.as_ptr() as usize, 9 * RUN);
        let (_, _, c2) = swept();
        assert_eq!(c2 - c1, 9 * RUN, "a CACHE runs block's pages ({c1} -> {c2})");
        // SAFETY: ours, given once
        unsafe {
            give(b, 9 * RUN);
            give_span(s);
        }
        flush_shelf();
        let (_, _, c3) = swept();
        assert_eq!(c3, c0, "given back: no longer the cache's");
        balanced();
    });
}

/// pppress (C): a fresh sweep's (present, free, cache, build), `present_split_build` reading the same sweep as
/// `present_split`.
fn swept_build() -> (usize, usize, usize, usize) {
    let (p, f, c) = swept();
    let (p2, f2, c2, b) = present_split_build().expect("present_split_build");
    assert_eq!((p2, f2, c2), (p, f, c), "present_split_build reads present_split's sweep");
    assert!(f + c + b <= p, "free {f} + cache {c} + build {b} within present {p}");
    assert_eq!(build_present(), Some(b), "the one-row reader reads the same sweep");
    (p, f, c, b)
}

#[test]
fn build_tag_allocations_count_in_build_present_never_in_cache() {
    one(|| {
        setup();
        let h = Hint::new(Class::Session, Tag::BUILD);
        let _ = quiet();
        let (_, f0, c0, b0) = swept_build();
        let s = span(4 * MIB, h).expect("a BUILD pool span");
        let a = s.as_ptr() as usize;
        touch(a, 2 * MIB);
        let (_, f1, c1, b1) = swept_build();
        assert_eq!(b1 - b0, resident(a, 4 * MIB), "a BUILD span's present pages ({b0} -> {b1})");
        assert_eq!((f1, c1), (f0, c0), "live and not the cache's: neither free nor cache");
        // a BUILD runs block too (past LIST_RUNS, as the CACHE witness)
        let r = take(9 * RUN, RUN, h).expect("a BUILD runs block").cast::<u8>();
        touch(r.as_ptr() as usize, 9 * RUN);
        let (_, _, c2, b2) = swept_build();
        assert_eq!(b2 - b1, 9 * RUN, "a BUILD runs block's pages ({b1} -> {b2})");
        assert_eq!(c2, c0, "BUILD bytes never in cache");
        // SAFETY: ours, given once
        unsafe {
            give(r, 9 * RUN);
            give_span(s);
        }
        flush_shelf();
        let (_, _, c3, b3) = swept_build();
        assert_eq!((b3, c3), (b0, c0), "given back: no longer BUILD's");
        balanced();
    });
}

#[test]
fn a_retagged_build_block_moves_to_the_cache_term_and_back() {
    one(|| {
        setup();
        let h = Hint::new(Class::Session, Tag::BUILD);
        let use_of = |t: Tag| stats().in_use[t.0 as usize];
        let _ = quiet();
        let (_, f0, c0, b0) = swept_build();
        let (ub0, ud0) = (use_of(Tag::BUILD), use_of(Tag::DICT));
        // a pool span, a runs block and a committed lazy span, each BUILD's
        let s = span(4 * MIB, h).expect("a BUILD pool span");
        let sa = s.as_ptr() as usize;
        touch(sa, 2 * MIB);
        let r = take(9 * RUN, RUN, h).expect("a BUILD runs block").cast::<u8>();
        touch(r.as_ptr() as usize, 9 * RUN);
        let mut l = span(64 * MIB, h).expect("a lazy span");
        assert!(l.is_lazy());
        // SAFETY: our own span
        unsafe { commit_to(&mut l, 8 * MIB) };
        let la = l.as_ptr() as usize;
        touch(la, 8 * MIB);
        let own = resident(sa, 4 * MIB) + 9 * RUN + resident(la, 8 * MIB);
        // (free may move: a huge page faulted for the runs block makes its superpage's other runs resident and free)
        let (_, f1, c1, b1) = swept_build();
        assert_eq!((b1 - b0, c1), (own, c0), "BUILD's: build, not cache");
        let ub1 = use_of(Tag::BUILD);
        let p = |a: usize| NonNull::new(a as *mut u8).expect("a block");
        // SAFETY: our live blocks, nothing else gives or resizes them
        let all = |t: Tag| unsafe { [retag(p(sa), t), retag(r, t), retag(p(la), t)] };
        assert_eq!(all(Tag::DICT), [true; 3], "a span, a runs block and a lazy span retag");
        let (_, f2, c2, b2) = swept_build();
        assert_eq!((c2 - c0, b2, f2), (own, b0, f1), "retagged DICT: the cache term's, no longer build's, free as it was");
        assert_eq!((use_of(Tag::BUILD), use_of(Tag::DICT) - ud0), (ub0, ub1 - ub0), "their in-use bytes moved whole");
        assert_eq!(all(Tag::DICT), [true; 3], "again: no change");
        assert_eq!(use_of(Tag::DICT) - ud0, ub1 - ub0, "a second retag moves nothing");
        assert_eq!(all(Tag::BUILD), [true; 3], "and back");
        let (_, _, c3, b3) = swept_build();
        assert_eq!((c3, b3), (c0, b1), "back to BUILD's");
        assert_eq!((use_of(Tag::BUILD), use_of(Tag::DICT)), (ub1, ud0), "in-use bytes back");
        // an address inside a block is no block's start: refused, nothing moved
        // SAFETY: inside our live span (refused before anything is changed)
        assert!(!unsafe { retag(p(sa + RUN), Tag::DICT) }, "a span's interior is not retagged");
        assert_eq!((use_of(Tag::BUILD), use_of(Tag::DICT)), (ub1, ud0), "a refusal moves nothing");
        assert_eq!(all(Tag::DICT), [true; 3], "given while DICT's: the give reads the tag");
        // by pointer, as a BuildBuf gives: a `Span` value names its take's tag, so a retagged span is given by its start
        core::mem::forget((s, l));
        // SAFETY: ours, given once
        unsafe {
            give(r, 9 * RUN);
            give(p(sa), 4 * MIB);
            give(p(la), 64 * MIB);
        }
        flush_shelf();
        let (_, _, c4, b4) = swept_build();
        assert_eq!((c4, b4), (c0, b0), "given back: neither's");
        assert_eq!((use_of(Tag::BUILD), use_of(Tag::DICT)), (ub0, ud0), "in-use bytes balanced per tag");
        balanced();
    });
}

#[test]
fn a_warm_lazy_spans_pages_count_until_released() {
    one(|| {
        setup();
        let h = Hint::new(Class::Task, Tag::OTHER);
        let (_, f0, _) = quiet();
        let mut l = span(64 * MIB, h).expect("a lazy span");
        assert!(l.is_lazy());
        // SAFETY: our own span
        unsafe { commit_to(&mut l, 8 * MIB) };
        let a = l.as_ptr() as usize;
        touch(a, 8 * MIB);
        let (_, f1, _) = swept();
        assert_eq!(f1, f0, "a live lazy span's pages never count");
        let own = resident(a, 8 * MIB);
        // SAFETY: ours, not used again
        unsafe { give_span(l) };
        let (_, f2, _) = swept();
        // warm (released on pressure only in this pool): its committed pages hold no live block
        assert!(f2 >= f1 + own, "a warm lazy span's present pages count ({f1} -> {f2}, its {own})");
        let _ = purge_now(usize::MAX >> 1, None);
        let (_, f3, _) = swept();
        assert!(f3 + own <= f2, "released: {f2} -> {f3}");
        balanced();
    });
}

/// pool_free_listed's parts (the view's row: shelved + central + parked, claimed): blocks given back while their runs
/// still hold a live block stay in the sum as they move from the shelf to the central lists and home (parked); once
/// every run is wholly free they leave it.
#[test]
fn listed_free_blocks_stay_listed_until_their_runs_go_free() {
    one(|| {
        setup();
        let h = Hint::new(Class::Task, Tag::OTHER);
        let _ = quiet();
        let listed = || {
            let s = stats();
            assert!(s.parked <= s.stranded, "parked is part of stranded: {s:?}");
            s.shelved + s.central + s.parked
        };
        const B: usize = 16 << 10;
        // the vectors first: with --features global the harness's own allocations go through the pool
        let mut blocks = Vec::with_capacity(64);
        let mut keep: Vec<(usize, NonNull<u8>)> = Vec::with_capacity(64);
        let l0 = listed();
        blocks.extend((0..64).map(|_| take(B, 16, h).expect("a block").cast::<u8>()));
        // one block kept live per 64 KiB run (a blocks run is whole runs), so no run the given ones sit in goes free
        let mut given = 0;
        for b in blocks.drain(..) {
            let a = b.as_ptr() as usize;
            touch(a, B);
            if keep.iter().any(|&(r, _)| r == a & !(RUN - 1)) {
                // SAFETY: ours, given once
                unsafe { give(b, B) };
                given += B;
            } else {
                keep.push((a & !(RUN - 1), b));
            }
        }
        assert!(given > 0);
        assert_eq!(listed() - l0, given, "given back: on this thread's shelf");
        flush_shelf();
        assert_eq!(listed() - l0, given, "flushed: on the central lists");
        let _ = purge_now(usize::MAX >> 1, None);
        let s = stats();
        assert_eq!(listed() - l0, given, "homed: parked in runs that still hold a live block ({s:?})");
        assert!(s.parked >= given, "parked: {s:?}");
        for (_, b) in keep.drain(..) {
            // SAFETY: ours, given once
            unsafe { give(b, B) };
        }
        flush_shelf();
        let _ = purge_now(usize::MAX >> 1, None);
        assert_eq!(listed(), l0, "every run wholly free: none of it listed");
        balanced();
    });
}

/// pppress (the overshoot fix, 61's guard rows): a release counts WHOLE ranges, so one larger than the swept present
/// (mostly unfaulted warm superpages) must come off the sweep's free part only: present and free never go to 0 past
/// it, and cache never drops.
#[test]
fn a_release_larger_than_the_swept_present_comes_off_free_only() {
    one(|| {
        setup();
        let _ = quiet();
        let cs = span(4 * MIB, Hint::new(Class::Cache, Tag::CACHE)).expect("a CACHE pool span");
        touch(cs.as_ptr() as usize, 4 * MIB);
        // 16 pool spans of 16 MiB, one page written in each, given back: warm superpages, mostly unfaulted
        let h = Hint::new(Class::Task, Tag::OTHER);
        let mut spans = Vec::with_capacity(16);
        for _ in 0..16 {
            let s = span(16 * MIB, h).expect("a pool span");
            touch(s.as_ptr() as usize, 4096);
            spans.push(s);
        }
        for s in spans {
            // SAFETY: ours, given once
            unsafe { give_span(s) };
        }
        let (p, f, c) = swept();
        assert!(c >= 4 * MIB, "the CACHE span's pages: {c}");
        let r0 = stats().released_bytes;
        let _ = purge_now(usize::MAX >> 1, None);
        let gone = (stats().released_bytes - r0) as usize;
        assert!(gone > p, "released more than the sweep read present: {gone} > {p}");
        let (p2, f2, c2) = present_split().expect("present_split");
        assert_eq!(c2, c, "cache never drops ({p} {f} {c} -> {p2} {f2} {c2})");
        assert!(p2 >= c2 && p2 > 0, "present never to 0 ({p} -> {p2})");
        assert_eq!(p - p2, f - f2, "only free came off ({p} {f} -> {p2} {f2})");
        assert_eq!(present(), Some(p2), "present() reads the same");
        // SAFETY: ours, not used again
        unsafe { give_span(cs) };
        let _ = quiet();
        balanced();
    });
}

/// Under load (threads taking, writing and giving blocks of every road), every sweep reads 0 <= free and
/// free + cache <= present, and the published reads between sweeps stay within `present()`. Between sweeps the main thread
/// gives the dirty base spans back (`clean_base`) as the server's purger does meanwhile: no pool thread runs in a test
/// binary, and without it the threads' given 3 MiB spans stayed resident (D4855 sweep: one run 6.3 GB max RSS).
#[test]
fn free_and_cache_stay_within_present_under_load() {
    one(|| {
        setup();
        let stop = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|sc| {
            for t in 0..4usize {
                let stop = &stop;
                sc.spawn(move || {
                    let h = Hint::new(Class::Task, if t % 2 == 0 { Tag::OTHER } else { Tag::CACHE });
                    let mut held: Vec<(NonNull<u8>, usize)> = Vec::new();
                    let mut i = 0usize;
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        let bytes = [4 << 10, 48 << 10, 3 * RUN, 9 * RUN, 3 * MIB][i % 5];
                        if let Some(b) = take(bytes, 64, h) {
                            let p = b.cast::<u8>();
                            touch(p.as_ptr() as usize, bytes);
                            held.push((p, bytes));
                        }
                        if held.len() > 8 {
                            let (p, n) = held.remove(i % held.len());
                            // SAFETY: ours, given once
                            unsafe { give(p, n) };
                        }
                        i += 1;
                    }
                    for (p, n) in held {
                        // SAFETY: ours, given once
                        unsafe { give(p, n) };
                    }
                    flush_shelf();
                });
            }
            let _stop = StopOnDrop(&stop);
            let t0 = std::time::Instant::now();
            let mut n = 0;
            while t0.elapsed() < std::time::Duration::from_millis(800) {
                let _ = swept();
                let (p, f, c) = present_split().expect("present_split");
                assert!(f + c <= p, "published: free {f} + cache {c} within present {p}");
                let _ = hooks::clean_base();
                n += 1;
            }
            drop(_stop);
            assert!(n > 10, "swept {n} times");
        });
        let _ = quiet();
        balanced();
    });
}
