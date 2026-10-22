//! D4692 (ladder L4, `PGRUST_MEMPOLICY=cache_release_bg=1`; default 0): the page releaser, one thread beside the
//! watchdog. A cache image's own run (64-512 KiB, `allocarena::own`) freed past the hot pool used to go back to the
//! kernel (MADV_DONTNEED) on the freeing thread: a part-cache trim on a statement's or an executor worker's thread (its
//! arm, a load's room, a scan's stage, and C1's inline take inside a LadderPass on the asker's thread) spent up to 28 ms
//! in one such call under load (mmap lock and TLB shootdowns), 249 times in a 1320 s stress. With the knob on, only the
//! threads marked `allocarena::own::mark_inline_release_ok` (this one and the idle loop) give runs back themselves: every
//! other thread leaves the run on the landing stack, its bytes on the REAPING row and in the ladder's landing credit,
//! and the free wakes this thread, which gives the runs back at once, the same calls of at most 512 KiB (never a whole
//! 2 MiB range: CVE-2026-74674), in batches of at most `BATCH_BYTES` with a pause between them. Only the thread changes,
//! not when: when to give back is the purge policy's.
//! H3 step (1) (`give_back_service`, default 0): the same thread runs as the give-back service (`service.rs`): the knob
//! starts it too and moves the own runs over (cache_release_bg's knob and default untouched), and its loop is this one
//! over the service's lanes, its passes stamped for the dead-man rule and its fallbacks logged. H3 step (2): each pass
//! purges the arenas the service owns (`service::purge_pass`), every 50 ms while it owns any, after its lanes.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use core::time::Duration;

/// The backstop tick (a wake is the normal start: the free that leaves the landing count past 0 sends one).
const TICK_MS: u64 = 100;
/// The pause when a pass found nothing while the count says a run is waiting: its push is a few instructions behind
/// the count (the count goes first, so the stack never holds a run the count has not).
const BEHIND_MS: u64 = 1;
const STACK: usize = 256 << 10;
/// The batch cap: at most this many bytes given back in one go (a run is never split). Each DONTNEED takes the address
/// space's mmap_lock shared; a long run of them keeps a waiting writer out, and every query thread's fault or madvise
/// queues behind that writer (stress: 19-26 query-thread waits >= 2 ms per run overlapped a releaser burst; bursts up to
/// 126 ms). CHOSEN, not tuned: 4 MiB = 8 of the largest runs, about 0.35 ms of calls at the ~11 GiB/s measured.
const BATCH_BYTES: usize = 4 << 20;
/// The pause between two batches while runs still wait: the lock's waiters get in. CHOSEN, not tuned: 250 us, which still
/// drains ~6 GiB/s, two orders over the stress's mean landing rate (~60 MiB/s).
pub(crate) const BATCH_PAUSE_US: u64 = 250;

static WAKE: AtomicBool = AtomicBool::new(false);
static THREAD: pgsync::OnceLock<pgsync::thread::Thread> = pgsync::OnceLock::new();
/// Counters (tests, the report line): passes that gave bytes back, and the bytes.
static PASSES: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);

/// Whether the knob is on (`cache_release_bg`, read at start after `PGRUST_MEMPOLICY` is applied).
pub fn enabled() -> bool {
    mempolicy::consts::get(mempolicy::consts::K::CacheReleaseBg) != 0
}

/// `(passes that gave bytes back, bytes given back)`.
pub fn counts() -> (u64, u64) {
    (PASSES.load(Relaxed), BYTES.load(Relaxed))
}

/// The free's wake (`allocarena::own::set_release_wake`): runs inside the global allocator, so a flag and an unpark only.
fn wake() {
    WAKE.store(true, Relaxed);
    if let Some(t) = THREAD.get() {
        t.unpark();
    }
}

/// The ladder's landing credit for the runs waiting here, until the driver's start installs its own
/// (`reap::landing_credit`, which counts them too).
fn landing() -> u64 {
    allocarena::own::landing_bytes() as u64
}

/// Start the releaser when the knob is on, or the give-back service's knob (memruntime's `start`, once, before any
/// backend). Whether it started.
pub fn start() -> bool {
    let service = crate::service::enabled();
    if !(enabled() || service) || THREAD.get().is_some() {
        return false;
    }
    // H3 step (1): the service's log lines carry the postmaster's prefix and time zone, as the watchdog's do
    let (prefix, tz) = (elog::config::log_line_prefix_format(), localtime::globals::log_timezone());
    // at the spawning thread's priority (nice 0): a nice-10 purger starved past its dead-man stretch (purge.rs)
    let spawned = std::thread::Builder::new().name("pg:memrelease".into()).stack_size(STACK).spawn(pagegive::pinned(move || {
        allocarena::own::mark_inline_release_ok();
        // D4812: a memory thread: its lines never wait on the server log's sink
        crate::memlog::defer_here();
        if service {
            elog::config::set_log_line_prefix(prefix);
            localtime::globals::set_log_timezone(tz);
        }
        let body: fn() = if service { run_service } else { run };
        // unwind-ok: log-then-die
        if std::panic::catch_unwind(body).is_err() && service {
            crate::service::log_stopped();
        }
    }));
    match spawned {
        Ok(h) => {
            let _ = THREAD.set(h.thread().clone());
            if service {
                // H3 step (1): the one landing account and the admission (backlog cap, dead-man rule) before the
                // first run is left for the service
                crate::service::begin(wake);
                mcx::ladder::set_unmap_landing(crate::service::landing);
                // P3 (`parked_trim_rung`): the ladder's R2p trims become this thread's jobs
                mcx::ladder::set_parked_service(wake);
            } else {
                mcx::ladder::set_unmap_landing(landing);
            }
            // from now on a free past the hot pool on any other thread leaves its run here
            allocarena::own::set_release_wake(wake);
            true
        }
        Err(_) => false,
    }
}

fn run() {
    loop {
        wait_wake(TICK_MS);
        drain_until_empty(&mut allocarena::own::drain_landing_upto, &|| allocarena::own::landing_bytes(), &mut nap);
    }
}

/// H3 step (1) (`give_back_service`): the loop as the give-back service: the same wake, tick and paced batches over the
/// service's lanes, each pass and batch stamped (the dead-man rule) and the fallbacks logged after each drain.
fn run_service() {
    let mut log = crate::service::Log::default();
    crate::service::log_owner_refusals();
    loop {
        crate::service::alive();
        wait_wake(crate::service::tick_ms(TICK_MS));
        crate::service::alive();
        // P3: the ladder's R2p job, if one waits (the parked regions and slot sets trimmed on this thread)
        if mcx::ladder::run_parked_job() != 0 {
            crate::service::alive();
        }
        service_pass(
            &mut || {
                crate::service::purge_pass(&mut |d: Duration| {
                    crate::service::alive();
                    nap(d);
                })
            },
            &mut || {
                drain_until_empty(&mut crate::service::drain_upto, &|| crate::service::landing() as usize, &mut |d: Duration| {
                    crate::service::alive();
                    nap(d);
                });
            },
        );
        crate::service::log(&mut log);
    }
}

/// One pass of the service after a wake: its lanes until nothing waits first (take-back TB3: what a lane holds is the
/// pool's room a draw goes without, so it never lands behind an arena's purge), then H3 step (2)'s purge of its arenas (a
/// forced job, else the expired ranges), one arena a batch.
fn service_pass(purge: &mut dyn FnMut(), drain: &mut dyn FnMut()) {
    drain();
    purge();
}

/// Until a wake, or the backstop tick (`tick_ms`).
fn wait_wake(tick_ms: u64) {
    if !WAKE.swap(false, Relaxed) {
        pgsync::thread::park_timeout(Duration::from_millis(tick_ms));
        WAKE.store(false, Relaxed);
    }
}

/// The releaser's pause: a park, cut short by an unpark (a wake).
fn nap(d: Duration) {
    pgsync::thread::park_timeout(d);
}

/// One drain after a wake, until nothing waits: a free while the count is past 0 sends no wake, so a run that lands during
/// a drain is given back by this one, not after the tick. Batches of at most `BATCH_BYTES`, `BATCH_PAUSE_US` apart.
/// `drain(max)` gives back waiting runs (at most `max` bytes, a run never split) and returns the bytes; `left()` is the
/// landing count; `nap` pauses. The bytes given back.
fn drain_until_empty(drain: &mut dyn FnMut(usize) -> usize, left: &dyn Fn() -> usize, nap: &mut dyn FnMut(Duration)) -> usize {
    let mut all = 0;
    loop {
        let n = drain(BATCH_BYTES);
        if n > 0 {
            PASSES.fetch_add(1, Relaxed);
            BYTES.fetch_add(n as u64, Relaxed);
            all += n;
        }
        if left() == 0 {
            return all;
        }
        // a batch given back: the lock's waiters first; nothing found: the counted run is still on its way to the stack
        nap(if n > 0 { Duration::from_micros(BATCH_PAUSE_US) } else { Duration::from_millis(BEHIND_MS) });
    }
}

#[cfg(test)]
mod tests {
    use core::cell::{Cell, RefCell};
    use core::time::Duration;

    /// Take-back TB3 (born red at the witness commit: the arenas' purge ran first): a service pass gives the job lanes
    /// back before it purges its arenas, so what a lane holds never lands behind an arena's purge.
    #[test]
    fn a_service_pass_drains_the_lanes_before_it_purges_the_arenas() {
        let log = RefCell::new(Vec::new());
        super::service_pass(&mut || log.borrow_mut().push("purge"), &mut || log.borrow_mut().push("drain"));
        assert_eq!(log.into_inner(), ["drain", "purge"], "the lanes first");
    }

    #[test]
    fn off_by_default_the_releaser_does_not_start() {
        assert!(!super::enabled(), "cache_release_bg defaults to 0");
        assert!(!super::start());
    }

    /// D4692 batch cap (born red at the witness commit: the drain gave back everything waiting in one burst). A drain of
    /// 10 MiB of 512 KiB runs gives back at most `BATCH_BYTES` per batch and pauses `BATCH_PAUSE_US` before each next
    /// batch, so the releaser's DONTNEED calls (each takes the mmap_lock shared) never run back to back past one batch
    /// while query threads wait for that lock; the drain still ends with nothing waiting.
    #[test]
    fn a_drain_gives_back_one_batch_at_a_time_with_a_pause_between_batches() {
        const RUN: usize = 512 << 10;
        let waiting = Cell::new(20usize);
        let log = RefCell::new(Vec::new());
        let mut drain = |max: usize| {
            let mut n = 0;
            while n < max && waiting.get() > 0 {
                waiting.set(waiting.get() - 1);
                n += RUN;
            }
            log.borrow_mut().push(format!("drain {n}"));
            n
        };
        let left = || waiting.get() * RUN;
        let mut nap = |d: Duration| log.borrow_mut().push(format!("nap {}", d.as_micros()));
        let all = super::drain_until_empty(&mut drain, &left, &mut nap);
        assert_eq!((all, waiting.get()), (20 * RUN, 0), "the drain ends with nothing waiting");
        let log = log.into_inner();
        let batch = super::BATCH_BYTES;
        for e in &log {
            if let Some(n) = e.strip_prefix("drain ") {
                let n: usize = n.parse().unwrap();
                assert!(n <= batch, "a batch gave back {} MiB in one burst, at most {} MiB: {log:?}", n >> 20, batch >> 20);
            }
        }
        let drains: Vec<usize> = log.iter().enumerate().filter(|(_, e)| e.starts_with("drain ")).map(|(i, _)| i).collect();
        for w in drains.windows(2) {
            let pause = format!("nap {}", super::BATCH_PAUSE_US);
            assert!(log[w[0] + 1..w[1]].contains(&pause), "no pause of {} us between two batches: {log:?}", super::BATCH_PAUSE_US);
        }
    }
}
