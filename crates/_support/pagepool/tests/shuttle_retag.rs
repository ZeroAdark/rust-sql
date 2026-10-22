//! Shuttle model, pppress (B): `retag` (a live block's tag moved by its owner: a CAS on the superpage words of a span
//! and a store on a runs block's head word, its in-use bytes moved between the tags' counters) racing the present
//! sweep (`refresh_present` + `present_split_build`, which reads the tags), the purger's age marks (which clear the
//! superpage words' activity bit the span's CAS must keep) and a give of the same blocks on another thread after the
//! retag (which must read the new tag). Invariants: every sweep's free + cache + build <= present; at quiescence the
//! per-tag in-use bytes (BUILD, DICT) are back where they started, so each give took its bytes off the tag the retag
//! left; `hooks::check` holds. Real crate, real remaps; every atomic of the shared state is a scheduling point
//! (src/sync.rs). Runs only with RUSTFLAGS="--cfg shuttle".
#![cfg(shuttle)]

use pagepool::{Class, Config, Hint, Tag};
use shuttle::thread;
use std::ptr::NonNull;

const RUN: usize = 64 << 10;
const SUPER: usize = 2 << 20;
const RUNS: usize = 4;

/// What the rounds exercised (std atomics: outside the model): sweeps read, retags that moved bytes.
static SWEEPS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static MOVED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn setup() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let cfg = Config {
            va: 8 << 20,
            chunk: 8 << 20,
            lazy_va: 64 << 20,
            frame_va: 0,
            shelf_max: 64 << 10,
            warm_zero: 0,
            warm_zero_max: 0,
            warm_cap: 0,
            decay_ms: 0,
            purge_batch: 2 << 20,
            lazy_min: 32 << 20,
            idle_drain_ms: 1000,
            block_min: 1024,
            release_mode: 2,
            lazy_decay_ms: 1,
            lazy_cap: 0,
            age_pass_max: 0,
            thp_partial: 0,
            host_age_ms: 0,
        };
        assert!(pagepool::init(&cfg));
    });
}

fn in_use(t: Tag) -> usize {
    pagepool::stats().in_use[t.0 as usize]
}

fn one_round() {
    setup();
    pagepool::flush_shelf();
    let (b0, d0) = (in_use(Tag::BUILD), in_use(Tag::DICT));
    let owner = thread::spawn(|| {
        let h = Hint::new(Class::Session, Tag::BUILD);
        let span = pagepool::take(SUPER, 16, h).expect("a span").cast::<u8>();
        let runs = pagepool::take(RUNS * RUN, 16, h).expect("a runs block").cast::<u8>();
        // SAFETY: the blocks we hold: one page of each written, so the sweep finds them present.
        unsafe {
            core::ptr::write_bytes(span.as_ptr(), 1, 4096);
            core::ptr::write_bytes(runs.as_ptr(), 1, 4096);
        }
        // SAFETY: our live blocks, nothing else gives or resizes them until the hand-off below.
        let moved = unsafe { [pagepool::retag(span, Tag::DICT), pagepool::retag(runs, Tag::DICT)] };
        assert_eq!(moved, [true, true], "a span and a runs block retag");
        // the runs block back to BUILD (a fp table dropped), the span given as DICT's
        // SAFETY: as above.
        assert!(unsafe { pagepool::retag(runs, Tag::BUILD) }, "and back");
        MOVED.fetch_add(3, std::sync::atomic::Ordering::Relaxed);
        let (s, r) = (span.as_ptr() as usize, runs.as_ptr() as usize);
        // the give on another thread, after the retags (the join orders them)
        let giver = thread::spawn(move || {
            // SAFETY: handed over whole by the owner, given once, by pointer (as a BuildBuf gives).
            unsafe {
                pagepool::give(NonNull::new(s as *mut u8).unwrap(), SUPER);
                pagepool::give(NonNull::new(r as *mut u8).unwrap(), RUNS * RUN);
            }
            pagepool::flush_shelf();
        });
        giver.join().unwrap();
    });
    let sweeper = thread::spawn(|| {
        for _ in 0..2 {
            if pagepool::refresh_present().is_some() {
                let (p, f, c, b) = pagepool::present_split_build().expect("a published sweep");
                assert!(f + c + b <= p, "free {f} + cache {c} + build {b} within present {p}");
                SWEEPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    });
    let purger = thread::spawn(|| {
        let _ = pagepool::lazy_pass_now();
        let _ = pagepool::lazy_pass_now();
    });
    owner.join().unwrap();
    sweeper.join().unwrap();
    purger.join().unwrap();
    pagepool::flush_shelf();
    let st = pagepool::stats();
    assert_eq!((in_use(Tag::BUILD), in_use(Tag::DICT)), (b0, d0), "per-tag in-use bytes balance: each give read the tag the retag left: {st:?}");
    if let Err(e) = pagepool::hooks::check() {
        panic!("{e}: {st:?}");
    }
}

#[test]
fn retag_races_the_sweep_the_age_marks_and_a_give() {
    let iters = std::env::var("SHUTTLE_ITERS").ok().and_then(|v| v.parse().ok()).unwrap_or(1000);
    shuttle::check_random(one_round, iters);
    shuttle::check_pct(one_round, iters, 3);
    let (s, m) = (SWEEPS.load(std::sync::atomic::Ordering::Relaxed), MOVED.load(std::sync::atomic::Ordering::Relaxed));
    eprintln!("MODEL retag: {} rounds, {s} sweeps read, {m} retags", 2 * iters);
}
