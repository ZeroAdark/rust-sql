//! D4812 part 2: the memory watchdog's samples, the ladder's pass among them, keep their period while the residency pass
//! (D3872) is slow. Each mincore call of that pass allocates a kernel page for its vector, and under host memory pressure
//! the allocation fell into global direct reclaim (swap writeout, writeback throttling: rq_qos_wait under submit_bio) for
//! up to 3 s, with the pass on the watchdog's thread waiting for it. Here the residency pass installed is one that takes
//! 1.5 s, a stand-in for that reclaim, and the watchdog's own clock (`memruntime::watch_ms`, one cadence a sample) must keep
//! moving with the wall clock. Its own test binary: the watchdog thread is process-wide.
#![cfg(target_os = "linux")]

use std::time::{Duration, Instant};

/// The limit the watchdog reads (`pgrust.memory_watchdog_limit`, MB), with the ladder on.
const WATCHDOG_MB: i32 = 512;
/// The slow residency pass: longer than ten cadences.
const SLOW_MS: u64 = 1500;
/// How long the test watches the watchdog's clock: the pass is due every second at this resident size.
const WATCH_FOR_MS: u64 = 4000;
/// The longest the watchdog's clock may stand still: ten cadences.
const STALL_MS: u128 = 1000;

fn slow_pass() -> bool {
    std::thread::sleep(Duration::from_millis(SLOW_MS));
    true
}

#[test]
fn the_ladder_pass_keeps_its_period_while_the_residency_pass_is_slow() {
    guc_tables::backing::set_pgrust_memory_watchdog_limit(WATCHDOG_MB);
    guc_tables::backing::set_pgrust_memory_ladder(guc_tables::consts::MEMORY_LADDER_ON);
    memruntime::set_residency_pass(slow_pass);
    memruntime::start(|| {});
    let t0 = Instant::now();
    while memruntime::watch_ms() == 0 && t0.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(5));
    }
    let (mut last, mut moved_at, mut stall) = (memruntime::watch_ms(), Instant::now(), 0u128);
    let end = Instant::now() + Duration::from_millis(WATCH_FOR_MS);
    while Instant::now() < end {
        let now = memruntime::watch_ms();
        if now != last {
            stall = stall.max(moved_at.elapsed().as_millis());
            (last, moved_at) = (now, Instant::now());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    stall = stall.max(moved_at.elapsed().as_millis());
    assert!(last > 0, "the watchdog ran");
    assert!(
        stall < STALL_MS,
        "the watchdog's samples stood still {stall} ms while a residency pass took {SLOW_MS} ms (the ladder's pass waited on \
         the residency pass's mincore: D4812)"
    );
}
