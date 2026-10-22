//! skewsplit (D3320): a running whole-part claim gives its back half to a worker that found the cursor exhausted. The job
//! stands in for the section's claim and the pgrc source: it enters the claim's offered cut, adopts it, and answers asks
//! at each granule step.
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gravity_parallel::{ClaimSize, GranuleRange, Job, Pool};
use gravity_pool::{Ambient, RuntimePool};
use runtime::{Runtime, RuntimeConfig, WorkerPool};

fn pool(threads: usize) -> (WorkerPool, RuntimePool) {
    let rt = Runtime::new(RuntimeConfig::new(threads));
    let workers = WorkerPool::spawn_std(Arc::clone(&rt)).expect("pool threads");
    (workers, RuntimePool::new(Arc::clone(&rt), Ambient::HARNESS))
}

fn whole_parts(parts: u64, per: u64, split: bool) -> GranuleRange {
    let starts: Vec<u64> = (0..parts).map(|p| p * per).collect();
    let mut c = GranuleRange::over_parts(parts * per, ClaimSize::Adaptive { startup_c0: 1 }, &starts);
    c.set_whole_boundary_claims(true);
    c.set_split_running(split);
    c
}

/// The granule ranges each claim ran (its end moved by every cut it answered), sorted; part `heavy`'s granules take
/// `slow` each, every other granule `fast`. With `hold`, the heavy part's first claim waits at its first granule until a
/// worker that ran out of parts has asked it and it has answered (at most `HOLD_MAX`).
#[allow(clippy::too_many_arguments)]
fn section(pool: &RuntimePool, width: u16, claims: &GranuleRange, per: u64, heavy: u64, slow: Duration, fast: Duration, hold: bool) -> Vec<(u64, u64)> {
    let ran = Mutex::new(Vec::new());
    let cuts = AtomicU64::new(0);
    // D4763: the cut no longer rides the box's pace. Unheld, a loaded box's oversleeping 50 us granules let the heavy claim
    // end before any worker ran dry. Held, it waits unread, and an unread claim is paced by its whole elapsed time, which
    // only grows, so the ask comes whatever the load
    let held = AtomicBool::new(hold);
    let until = Instant::now() + HOLD_MAX;
    let job = |_ordinal: u16, range: gravity_abi::MorselRange| -> gravity_driver::Result<()> {
        gravity_driver::claimcut::entered(true, |_| {
            let cut = gravity_driver::claimcut::with_current(|c| c.adopt()).flatten();
            let (mut g, mut hi) = (range.lo, range.hi);
            // bounded("test: one step per granule of the claim, and the held claim's steps for at most HOLD_MAX")
            while g < hi {
                if let Some(mid) = cut.and_then(|c| c.step(g, 0)) {
                    hi = mid;
                    cuts.fetch_add(1, Ordering::Relaxed);
                    held.store(false, Ordering::Relaxed);
                    continue;
                }
                if g / per == heavy && cut.is_some() && held.load(Ordering::Relaxed) && Instant::now() < until {
                    std::thread::sleep(fast);
                    continue;
                }
                std::thread::sleep(if g / per == heavy { slow } else { fast });
                g += 1;
            }
            ran.lock().unwrap().push((range.lo, hi));
        });
        Ok(())
    };
    let job: &Job<'_> = &job;
    pool.run(width, claims, job).expect("section");
    let mut r = ran.into_inner().unwrap();
    r.sort_unstable();
    r
}

fn assert_tiles(ranges: &[(u64, u64)], total: u64, per: u64) {
    let mut next = 0;
    for &(lo, hi) in ranges {
        assert_eq!(lo, next, "granules {next}..{lo} not run, or run twice ({ranges:?})");
        assert!(lo < hi && lo / per == (hi - 1) / per, "claim {lo}..{hi} crosses a part");
        next = hi;
    }
    assert_eq!(next, total);
}

/// The held claim's hang guard: past it the claim runs on unasked and the cut assertion fails, not the run.
const HOLD_MAX: Duration = Duration::from_secs(60);

/// 12 parts of 16 granules at width 4; part 2 takes 2 ms a granule, the rest 50 us: with the cut on, the heavy part is run
/// by several claims (its back halves taken by workers that ran out of parts), every granule once and no claim crossing a
/// part; with it off, the heavy part stays one claim.
#[test]
fn a_heavy_whole_part_is_cut_for_idle_workers_only_where_the_cursor_allows_it() {
    let (workers, pool) = pool(4);
    let (parts, per) = (12u64, 16u64);
    let (slow, fast) = (Duration::from_millis(2), Duration::from_micros(50));
    let on = section(&pool, 4, &whole_parts(parts, per, true), per, 2, slow, fast, true);
    assert_tiles(&on, parts * per, per);
    let heavy: Vec<_> = on.iter().filter(|&&(lo, _)| lo / per == 2).collect();
    assert!(heavy.len() >= 2, "the heavy part was not cut ({on:?})");
    let off = section(&pool, 4, &whole_parts(parts, per, false), per, 2, slow, fast, false);
    assert_tiles(&off, parts * per, per);
    assert!(off.iter().all(|&(lo, hi)| lo % per == 0 && hi - lo == per), "a cursor with the cut off was cut ({off:?})");
    workers.shutdown();
}
