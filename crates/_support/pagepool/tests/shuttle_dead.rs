//! Shuttle model, card 2 (c): a live block's dead runs (give_pages, DEAD -> SEEN -> RIPE by the purger's age marks)
//! revived by their owner (take_pages) while the purger's lazy pass (marks, the RIPE release) and a pressure release
//! (release_dead: any dead run) give them back. The owner's live runs are never released; a revived run reads its
//! bytes as left (Zeroed::No) or zero throughout (Yes), never part of each; at quiescence every count holds
//! (`hooks::check`, no dead bytes left). Real crate, real remaps; every atomic of the shared state is a scheduling
//! point (src/sync.rs). Runs only with RUSTFLAGS="--cfg shuttle".
#![cfg(shuttle)]

use pagepool::{Class, Config, Hint, Tag, Zeroed};
use shuttle::thread;
use std::ptr::NonNull;

const RUN: usize = 64 << 10;
const RUNS: usize = 4;
const PAT: u8 = 0xa5;

/// What the rounds exercised (std atomics: outside the model): revivals that found a run released, bytes the purger gave.
static REVIVED_ZERO: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static PURGED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

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

/// Run `r` of the block reads all `PAT` (Some(true)), all zero (Some(false)), or a mix (None).
fn run_bytes(p: usize, r: usize) -> Option<bool> {
    // SAFETY: the owner's block, read by its owner.
    let b = unsafe { core::slice::from_raw_parts((p + r * RUN) as *const u8, RUN) };
    if b.iter().all(|&x| x == PAT) {
        Some(true)
    } else if b.iter().all(|&x| x == 0) {
        Some(false)
    } else {
        None
    }
}

fn one_round() {
    setup();
    let owner = thread::spawn(|| {
        let s = pagepool::take(RUNS * RUN, 16, Hint::new(Class::Task, Tag::OTHER)).expect("take");
        let blk = s.cast::<u8>();
        let p = blk.as_ptr() as usize;
        // SAFETY: the block we hold.
        unsafe { core::ptr::write_bytes(p as *mut u8, PAT, RUNS * RUN) };
        for _ in 0..2 {
            // SAFETY: runs 1 and 2 of our live block, unread until take_pages.
            unsafe { pagepool::give_pages(blk, RUN, 2 * RUN) };
            assert_eq!(run_bytes(p, 0), Some(true), "a live run was released");
            assert_eq!(run_bytes(p, 3), Some(true), "a live run was released");
            // SAFETY: as above.
            let z = unsafe { pagepool::take_pages(blk, RUN, 2 * RUN) };
            if z != Zeroed::No {
                REVIVED_ZERO.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            for r in 1..3 {
                let got = run_bytes(p, r).unwrap_or_else(|| panic!("run {r} part released, part not, after take_pages"));
                if z == Zeroed::No {
                    assert!(got, "run {r} reads zero but take_pages said nothing was released");
                }
            }
            // SAFETY: the block we hold again.
            unsafe { core::ptr::write_bytes((p + RUN) as *mut u8, PAT, 2 * RUN) };
        }
        assert_eq!(run_bytes(p, 0), Some(true));
        assert_eq!(run_bytes(p, 3), Some(true));
        // SAFETY: taken with these bytes, not used again.
        unsafe { pagepool::give(NonNull::new(p as *mut u8).unwrap(), RUNS * RUN) };
        pagepool::flush_shelf();
    });
    let purger = thread::spawn(|| {
        let mut got = pagepool::lazy_pass_now();
        got += pagepool::lazy_pass_now();
        got += pagepool::purge_now(2 * RUN, None);
        got += pagepool::lazy_pass_now();
        PURGED.fetch_add(got, std::sync::atomic::Ordering::Relaxed);
    });
    owner.join().unwrap();
    purger.join().unwrap();
    let st = pagepool::stats();
    assert_eq!(st.dead, 0, "dead bytes left after every run was revived: {st:?}");
    if let Err(e) = pagepool::hooks::check() {
        panic!("{e}: {st:?}");
    }
}

#[test]
fn dead_runs_race_the_purger() {
    let iters = std::env::var("SHUTTLE_ITERS").ok().and_then(|v| v.parse().ok()).unwrap_or(1000);
    shuttle::check_random(one_round, iters);
    shuttle::check_pct(one_round, iters, 3);
    let (z, b) = (REVIVED_ZERO.load(std::sync::atomic::Ordering::Relaxed), PURGED.load(std::sync::atomic::Ordering::Relaxed));
    eprintln!("MODEL dead runs: {} rounds, {z} revivals found a run released, the purger gave {b} bytes", 2 * iters);
}
