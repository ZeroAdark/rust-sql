use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use gravity_parallel::{ClaimSize, GranuleRange, Job, Pool};
use gravity_pool::{Ambient, RuntimePool};
use runtime::{Runtime, RuntimeConfig, WorkerPool};

fn pool(threads: usize) -> (WorkerPool, RuntimePool) {
    let rt = Runtime::new(RuntimeConfig::new(threads));
    let workers = WorkerPool::spawn_std(Arc::clone(&rt)).expect("pool threads");
    (workers, RuntimePool::new(Arc::clone(&rt), Ambient::HARNESS))
}

/// `parts` parts of `per` granules each, claimed whole (the runner's whole-part law).
fn whole_parts(parts: u64, per: u64) -> GranuleRange {
    let starts: Vec<u64> = (0..parts).map(|p| p * per).collect();
    let mut c = GranuleRange::over_parts(parts * per, ClaimSize::Adaptive { startup_c0: 1 }, &starts);
    c.set_whole_boundary_claims(true);
    c
}

/// Every claim of a section over `claims` at `width` on `pool`; the first `hold` claims wait until `hold` are in flight
/// (or 20 s), so the runtime's live width reaches `hold`, and each claim then sleeps `per_granule` a granule.
fn section(pool: &RuntimePool, width: u16, claims: &GranuleRange, hold: u64, per_granule: Duration) -> Vec<(u64, u64)> {
    let ranges = Mutex::new(Vec::new());
    let (held, cv, seen) = (Mutex::new(0u64), Condvar::new(), AtomicU64::new(0));
    let job = |_ordinal: u16, range: gravity_abi::MorselRange| -> gravity_driver::Result<()> {
        if seen.fetch_add(1, Ordering::SeqCst) < hold {
            let mut h = held.lock().unwrap();
            *h += 1;
            cv.notify_all();
            let deadline = Instant::now() + Duration::from_secs(20);
            while *h < hold && Instant::now() < deadline {
                h = cv.wait_timeout(h, Duration::from_millis(50)).unwrap().0;
            }
        }
        std::thread::sleep(per_granule * (range.hi - range.lo) as u32);
        ranges.lock().unwrap().push((range.lo, range.hi));
        Ok(())
    };
    let job: &Job<'_> = &job;
    pool.run(width, claims, job).expect("section");
    let mut r = ranges.into_inner().unwrap();
    r.sort_unstable();
    r
}

fn assert_tiles(ranges: &[(u64, u64)], total: u64, per: u64) {
    let mut next = 0;
    for &(lo, hi) in ranges {
        assert_eq!(lo, next, "granules {next}..{lo} not claimed, or claimed twice ({ranges:?})");
        assert!(lo < hi && lo / per == (hi - 1) / per, "claim {lo}..{hi} crosses a part");
        next = hi;
    }
    assert_eq!(next, total);
}

/// code.heads' count pass and the two-pass pass-2 cursor fill one entry of a per-part directory per claim:
/// their claims are whole parts at every width. Past a live width of 32 the runtime's end-game split cut a whole-part
/// cursor's last claims inside a part; with the split off (`set_endgame_split(false)`) every claim stays one whole part.
#[test]
fn a_whole_part_cursor_with_the_endgame_split_off_is_never_cut_at_width_48() {
    let (workers, pool) = pool(48);
    let (parts, per) = (60u64, 8u64);
    let on = section(&pool, 48, &whole_parts(parts, per), 34, Duration::from_micros(125));
    assert_tiles(&on, parts * per, per);
    assert!(on.iter().any(|&(lo, hi)| lo % per != 0 || hi - lo != per), "no end-game split at width 48 ({on:?})");
    let mut off = whole_parts(parts, per);
    off.set_endgame_split(false);
    let off = section(&pool, 48, &off, 34, Duration::from_micros(125));
    assert_tiles(&off, parts * per, per);
    assert!(off.iter().all(|&(lo, hi)| lo % per == 0 && hi - lo == per), "a cursor with the split off was cut ({off:?})");
    workers.shutdown();
}

/// A tail (`set_tail_from`): 10 parts at width 8, the first 8 claimed whole, the last 2 cut by the runtime's sizer inside
/// each part; every granule once.
#[test]
fn a_tail_cursor_cuts_only_the_parts_past_its_tail() {
    let (workers, pool) = pool(8);
    let (parts, per) = (10u64, 16u64);
    let mut c = whole_parts(parts, per);
    c.set_tail_from(8 * per);
    // 200 us a granule: a whole part runs 3.2 ms, past the runtime's 2 ms claim target, so a tail claim is under a part
    let r = section(&pool, 8, &c, 0, Duration::from_micros(200));
    assert_tiles(&r, parts * per, per);
    assert_eq!(r.iter().filter(|&&(lo, _)| lo < 8 * per).count(), 8, "{r:?}");
    assert!(r.iter().filter(|&&(lo, _)| lo < 8 * per).all(|&(lo, hi)| lo % per == 0 && hi - lo == per), "a part before the tail was cut ({r:?})");
    assert!(r.iter().filter(|&&(lo, _)| lo >= 8 * per).count() > 2, "the tail's parts were not cut ({r:?})");
    workers.shutdown();
}
