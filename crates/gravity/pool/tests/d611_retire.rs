//! D611: a pool thread whose identity died across the crash fence (a crash-restart under the server) retires RAW at the
//! pool gate. Before D611 it retired inside a claim: the section failed ("worker panicked in a claim", the statement
//! ERROR where PostgreSQL answers) and the unwind was swallowed, so the same thread failed every later section. Now the
//! gate runs before the thread claims (`TaskSetWork::before_claims`): it dies holding no range and the other threads
//! claim every morsel exactly once; a fence that moves between that gate and a claim fails the section and the thread
//! still dies, and either way the thread's execution permit goes back to the pool. The gate here is scripted per thread
//! (`install_pool_gate`, this test binary's own process).
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;

use gravity_driver::{Error, Result};
use gravity_parallel::{BlockRange, ClaimSize, Job, Pool};
use gravity_pool::{Ambient, RuntimePool};
use runtime::{Runtime, RuntimeConfig, WorkerPool};

const GRANULES: u64 = 1000;

/// Threads still to retire, and where: at their first gate call (the pre-claim gate) or at their second (the claim's).
static RETIRE: AtomicU32 = AtomicU32::new(0);
static AT_CALL: AtomicU8 = AtomicU8::new(1);
static RETIRED: AtomicU32 = AtomicU32::new(0);
/// The script's generation: a thread's call count restarts with it.
static GEN: AtomicU32 = AtomicU32::new(0);

/// Per thread: the script generation it last counted in and its gate calls in that generation (a retired thread is dead).
static CALLS: Mutex<Vec<(ThreadId, u32, u32)>> = Mutex::new(Vec::new());

/// The scripted pool gate: a thread picked to retire unwinds PoolRetireRaw at its `AT_CALL`th gate call of the script,
/// as launch_backend's `pool_gate` does when the crash fence moved under the thread's identity; every other call is a
/// live identity.
fn gate() -> bool {
    let me = std::thread::current().id();
    let gen = GEN.load(Ordering::Acquire);
    let n = {
        let mut calls = CALLS.lock().unwrap_or_else(|p| p.into_inner());
        match calls.iter_mut().find(|c| c.0 == me) {
            Some(c) => {
                if c.1 != gen {
                    *c = (me, gen, 0);
                }
                c.2 += 1;
                c.2
            }
            None => {
                calls.push((me, gen, 1));
                1
            }
        }
    };
    if n == AT_CALL.load(Ordering::Acquire) as u32 && RETIRE.fetch_update(Ordering::AcqRel, Ordering::Acquire, |r| r.checked_sub(1)).is_ok() {
        RETIRED.fetch_add(1, Ordering::AcqRel);
        std::panic::panic_any(parallel::standing::PoolRetireRaw);
    }
    true
}

/// Every granule's run count over one section of one-granule claims.
fn section(pool: &RuntimePool, width: u16, runs: &[AtomicU64]) -> std::result::Result<(), Error> {
    let job = |_ordinal: u16, r: gravity_abi::MorselRange| -> Result<()> {
        for g in r.lo..r.hi {
            runs[g as usize].fetch_add(1, Ordering::AcqRel);
        }
        std::thread::sleep(std::time::Duration::from_micros(20));
        Ok(())
    };
    let job: &Job<'_> = &job;
    let claims = BlockRange::new(GRANULES, ClaimSize::Fixed(1));
    pool.run(width, &claims, job).map_err(|e| e.error)
}

fn counts() -> Vec<AtomicU64> {
    (0..GRANULES).map(|_| AtomicU64::new(0)).collect()
}

#[test]
fn a_thread_retiring_across_the_crash_fence_loses_no_morsel_and_runs_none_twice() {
    parallel::standing::install_pool_gate(gate);
    // the dying threads' PoolRetireRaw panics print nothing: they are the script
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if !info.payload().is::<parallel::standing::PoolRetireRaw>() {
            hook(info)
        }
    }));
    let rt = Runtime::new(RuntimeConfig::new(8));
    let threads = WorkerPool::spawn_std(Arc::clone(&rt)).expect("pool threads");
    let pool = RuntimePool::new(Arc::clone(&rt), Ambient::HARNESS);

    // a crash reset opened the window (no engage path runs in a Gravity section): the section's leader closes it, so a
    // respawned thread can complete its identity
    parallel::standing::flush_for_crash();
    assert!(parallel::standing::pool_crash_pending());
    // 3 of the 8 threads retire at the pre-claim gate, mid-section: the section answers, every granule exactly once
    GEN.fetch_add(1, Ordering::AcqRel);
    RETIRE.store(3, Ordering::Release);
    AT_CALL.store(1, Ordering::Release);
    let runs = counts();
    assert_eq!(section(&pool, 8, &runs), Ok(()), "a retire before the claim never fails the section");
    assert!(!parallel::standing::pool_crash_pending(), "the section's leader closed the crash window");
    assert_eq!(RETIRED.load(Ordering::Acquire), 3, "the three scripted threads retired");
    let bad: Vec<(usize, u64)> = runs.iter().map(|c| c.load(Ordering::Acquire)).enumerate().filter(|&(_, n)| n != 1).collect();
    assert!(bad.is_empty(), "granules run other than once: {:?}", &bad[..bad.len().min(8)]);

    // the fence moving between a thread's pre-claim gate and its claim (a new crash reset mid-section): the section fails
    // and the thread dies (never swallowed to retire again at its next claim); no granule ran twice
    GEN.fetch_add(1, Ordering::AcqRel);
    RETIRE.store(1, Ordering::Release);
    AT_CALL.store(2, Ordering::Release);
    let runs = counts();
    let r = section(&pool, 5, &runs);
    let retired_mid = RETIRED.load(Ordering::Acquire) - 3;
    if retired_mid == 1 {
        assert_eq!(r, Err(Error::host("gravity: worker panicked in a claim")), "a retire inside the claim fails the section");
    } else {
        assert_eq!(r, Ok(()), "no thread reached a second gate call in this section");
    }
    assert!(runs.iter().all(|c| c.load(Ordering::Acquire) <= 1), "no granule ran twice");
    RETIRE.store(0, Ordering::Release);

    // the surviving threads serve a whole section again
    let runs = counts();
    assert_eq!(section(&pool, 4, &runs), Ok(()), "the pool serves after the retires");
    assert!(runs.iter().all(|c| c.load(Ordering::Acquire) == 1), "every granule once after the retires");

    // every retiring thread gave its step's execution permit back (a leaked permit per retire starved the respawned
    // pool: a server whose every thread retired after a crash hung its next section): once the survivors park, all
    // eight are free. D2574: the assert reads the value the wait saw; a parked thread can take a permit for a moment
    // after it, so a second read could see 7 with nothing leaked (a leaked permit keeps every read below 8).
    let t0 = std::time::Instant::now();
    let mut free = rt.execution_permits().available();
    while free < 8 && t0.elapsed() < std::time::Duration::from_secs(10) {
        std::thread::yield_now();
        free = rt.execution_permits().available();
    }
    assert_eq!(free, 8, "the retired threads' permits are back");
    threads.shutdown();
}
