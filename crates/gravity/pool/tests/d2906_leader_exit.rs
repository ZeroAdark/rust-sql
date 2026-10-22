//! D2906: a section's leader that leaves `run_led` by an unwind (under the server: a crash restart's quickdie, a FATAL,
//! unwinding out of the interrupt check while the pool runs its W8 section) waits for its section to stop first. The
//! claims run the leader's job over the leader's state through raw pointers; before D2906 the leader's frames went at
//! once and the pool threads ran on over freed memory (a SIGSEGV in `Open::run_claim`, mimalloc's page lists corrupted:
//! the whole server down instead of the crash restart). Here the interrupt check unwinds a few quanta into a section of
//! slow claims over a buffer the leader's frame owns: when the unwind reaches the caller no claim is in flight and none
//! starts after it.
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use gravity_driver::Result;
use gravity_parallel::{BlockRange, ClaimSize, Job, Pool};
use gravity_pool::{Ambient, RuntimePool};
use runtime::{Runtime, RuntimeConfig, WorkerPool};
use types_error::PgResult;

const GRANULES: u64 = 4000;

static CHECKS: AtomicU64 = AtomicU64::new(0);
static IN_FLIGHT: AtomicU64 = AtomicU64::new(0);
static STARTED: AtomicU64 = AtomicU64::new(0);
static STARTED_AFTER: AtomicU64 = AtomicU64::new(0);
static LEADER_GONE: AtomicBool = AtomicBool::new(false);

/// The leader's exit unwind (the server's `ProcExitThread` / `KilledBySignal`): a payload the pool never catches.
struct LeaderExit;

/// The interrupt check: its third call once the pool threads run claims (a few park quanta into the section) unwinds, as
/// quickdie does at a crash restart.
fn dying_interrupts() -> PgResult<()> {
    if STARTED.load(Ordering::Acquire) >= 8 && CHECKS.fetch_add(1, Ordering::AcqRel) == 2 {
        std::panic::panic_any(LeaderExit);
    }
    Ok(())
}

/// One section whose claims sleep and then write the leader-owned `state` (its frame's, freed by the unwind).
fn section(pool: &RuntimePool) {
    let state: Vec<AtomicU64> = (0..GRANULES).map(|_| AtomicU64::new(0)).collect();
    let job = |_ordinal: u16, r: gravity_abi::MorselRange| -> Result<()> {
        if LEADER_GONE.load(Ordering::Acquire) {
            STARTED_AFTER.fetch_add(1, Ordering::AcqRel);
        }
        STARTED.fetch_add(1, Ordering::AcqRel);
        IN_FLIGHT.fetch_add(1, Ordering::AcqRel);
        std::thread::sleep(std::time::Duration::from_millis(2));
        for g in r.lo..r.hi {
            state[g as usize].fetch_add(1, Ordering::AcqRel);
        }
        IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
        Ok(())
    };
    let job: &Job<'_> = &job;
    let claims = BlockRange::new(GRANULES, ClaimSize::Fixed(1));
    let _ = pool.run(8, &claims, job);
    unreachable!("the interrupt check unwinds out of the section");
}

#[test]
fn a_leader_unwinding_out_of_its_section_waits_for_the_claims_to_stop() {
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if !info.payload().is::<LeaderExit>() {
            hook(info)
        }
    }));
    let rt = Runtime::new(RuntimeConfig::new(8));
    let threads = WorkerPool::spawn_std(Arc::clone(&rt)).expect("pool threads");
    let pool = RuntimePool::new(Arc::clone(&rt), Ambient { interrupts: dying_interrupts, ..Ambient::HARNESS });

    let r = catch_unwind(AssertUnwindSafe(|| section(&pool)));
    let in_flight = IN_FLIGHT.load(Ordering::Acquire);
    LEADER_GONE.store(true, Ordering::Release);
    assert!(r.err().is_some_and(|p| p.is::<LeaderExit>()), "the leader's exit unwind reached its caller");
    assert!(CHECKS.load(Ordering::Acquire) >= 3, "the leader unwound from its interrupt check");
    let started = STARTED.load(Ordering::Acquire);
    assert!(started > 0 && started < GRANULES, "the section was mid-run when the leader left ({started} claims started)");
    assert_eq!(in_flight, 0, "no claim over the leader's state was in flight when its frames went");
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(STARTED_AFTER.load(Ordering::Acquire), 0, "no claim started after the leader left");
    assert_eq!(IN_FLIGHT.load(Ordering::Acquire), 0, "none ran on after it");

    // the pool serves the next leader's section whole (its interrupt check never unwinds again)
    CHECKS.store(u64::MAX / 2, Ordering::Release);
    let runs: Vec<AtomicU64> = (0..64).map(|_| AtomicU64::new(0)).collect();
    let job = |_o: u16, r: gravity_abi::MorselRange| -> Result<()> {
        for g in r.lo..r.hi {
            runs[g as usize].fetch_add(1, Ordering::AcqRel);
        }
        Ok(())
    };
    let job: &Job<'_> = &job;
    assert!(pool.run(4, &BlockRange::new(64, ClaimSize::Fixed(1)), job).is_ok(), "the next section answers");
    assert!(runs.iter().all(|c| c.load(Ordering::Acquire) == 1), "every granule once");
    threads.shutdown();
}
