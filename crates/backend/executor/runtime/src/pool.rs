//! Worker pool driver: `cores + K` threads running the worker loop against
//! one [`Runtime`].
//!
//! Layering: this module owns only the LOOP and (for tests) std-thread
//! spawning + join. Production spawning goes through launch_backend's
//! rtpool glue (synthetic pids via the guarded reserve_child_pid counter,
//! wpool-style warm prelude, standard child stack size) which calls
//! [`WorkerPool::spawn_with`] with its own spawner. Runtime workers are
//! EXECUTORS, not sessions: no PGPROC, no bgworker slot, no reaper entry,
//! and — deliberately — NO wakeup-registry entry (they park on the
//! runtime's eventcount, not a latch), so the pid-collision/registry-hijack
//! wedge class (notes/pardistinct-contention-fix.md) has no surface here.
//! Any FUTURE latch use by pool threads must follow the rekey discipline
//! (waiteventset_seams::rekey_wakeup_registry) — documented, not needed.

use std::io;
use std::sync::Arc;
use std::thread::JoinHandle;

use crate::sched::Step;
use crate::Runtime;

/// The worker loop. Permit discipline: a permit is held across the whole
/// scheduling step (superset of task execution — satisfies "any
/// task-executing thread holds one"), released before parking so an idle
/// worker never caps a busy one; a task that blocks inside the step may
/// donate the permit to a standby through the §2.8 seams (crate::io).
/// Park discipline: the epoch is captured BEFORE the step, so a publish
/// between a failed pick and the park is never lost.
///
/// §2.9 uring duties: ring created at loop start / torn down at loop exit,
/// and a non-blocking boundary reap after EVERY step — completions run
/// (IoToken unpark-all) and issuer pins drop before the worker runs its
/// next task or parks.

/// Lane wakelit arm (iii), `PGRUST_POOL_SPIN_US=<µs>` (default 0 = off):
/// a worker that is about to park spins on the park-lot epoch for at most
/// this window first. Consecutive phases of one statement are published
/// microseconds apart, so a worker that never parks between them never
/// re-enters the kernel's wake placement — the burst that stacks two
/// wakees on one CPU (§2.2af) does not happen for phases 2..n. The spin
/// runs with the execution permit RELEASED and is bounded; an idle pool
/// costs one window per worker after its last phase and nothing after.
fn spin_us() -> u64 {
    static V: crate::sync::OnceLock<u64> = crate::sync::OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("PGRUST_POOL_SPIN_US").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0)
    })
}

/// Spin on the epoch for the configured window; true = the epoch moved
/// (new work or a wake) and the caller skips the park. The window is a
/// `pg_clock` deadline (the process monotonic clock; virtual under DST,
/// where the arm stays off: a spin has no yield point to advance time).
fn spin_for_epoch(rt: &Runtime, epoch: u64) -> bool {
    let us = spin_us();
    if us == 0 {
        return false;
    }
    let deadline = pg_clock::Deadline::after(std::time::Duration::from_micros(us));
    loop {
        if rt.park_epoch() != epoch {
            return true;
        }
        if deadline.expired() {
            return false;
        }
        std::hint::spin_loop();
    }
}

pub fn worker_loop(rt: &Arc<Runtime>, worker: usize) {
    let mut local = rt.worker_local(worker);
    // purge timing: this thread's purges count as an executor worker's
    mcx::purge_phase(3);
    // ALLOC-LADDER (b): the ladder's first rung reaches a parked worker through the park lot; the worker collects its own
    // allocator pages at its next idle park (`mcx::answer_collect`)
    mcx::set_collect_wake(|| {
        if let Some(rt) = crate::global() {
            rt.notify_source_progress();
        }
    });
    let mut collect_seen = mcx::collect_counts().0;
    let ring = crate::io::worker_enter(rt);
    rt.register_worker_ring(worker, ring);
    // Register this thread for the spill blocking-section facade (M3.5
    // §6.1): spill code inside task bodies may donate this thread's permit
    // to a standby for the duration of a blocking syscall region.
    // SAFETY: the permit semaphore lives in `rt`, which outlives this loop;
    // blocking_io_section is only reachable from task bodies inside
    // worker_step, where the permit is held.
    let _blocking_reg = unsafe { crate::blocking::PermitThreadReg::new(rt.execution_permits()) };
    // Exit discipline on every path, unwind included: a task-body panic
    // must not leak this worker's permit (the pool would shrink for good).
    let mut exit = LoopExit { rt, worker, held: false };
    if crate::sched::step_v2() {
        // STEP-V2 (agg192-contention, 48xl finding #1): permit held ACROSS
        // steps (released around parks and at exit) — the per-step global
        // semaphore acquire/release was a shared-line hot spot at high DOP —
        // and Retry parks after a bounded spin instead of spinning the whole
        // invalidated-slot window (publish/completion wake_all; the epoch is
        // captured before the failing step, so the park is lost-wakeup-free).
        // `PGRUST_RUNTIME_STEP_V2=0` restores the loop below.
        let mut retries = 0u32;
        // regression change 4 (PGRUST_POOL_WAKE_SPINNER, default OFF):
        // this worker's directed-wake slot + search-phase accounting. The
        // spinner mark spans wake→claim (cleared at worker_step's claim
        // points, consumed by the directed park); submissions elide their
        // notify while any worker is in that window (ParkLot::wake_work).
        let spinner_mode = rt.wake_spinner_armed();
        // Lane wakelit arm (iv): the wake TREE also parks on the directed
        // stack (a tree wake pops registered parkers) and each wakee
        // continues the tree before it searches.
        let tree_mode = rt.wake_tree_armed();
        let directed = spinner_mode || tree_mode;
        let parker = std::sync::Arc::new(pgsync::WorkerParker::new());
        if spinner_mode {
            rt.spin_enter_worker(&mut local);
        }
        loop {
            let epoch = rt.park_epoch();
            if !exit.held {
                // Flip fix: the loop-top acquire can BLOCK (permits held
                // by inline statements / suspended bodies) — a blocked
                // thread is not a searcher; drop the mark for the wait and
                // re-arm after (submissions during the wait then wake a
                // parked worker instead of eliding against us).
                if spinner_mode {
                    rt.spin_abandon_worker(&mut local);
                }
                rt.execution_permits().acquire();
                crate::io::note_permit(true);
                exit.held = true;
                if spinner_mode && !local.spinning() {
                    rt.spin_enter_worker(&mut local);
                }
            }
            let step = rt.worker_step(&mut local);
            // Task boundary (§2.9): drain this worker's CQEs non-blockingly.
            crate::io::boundary_reap();
            match step {
                Step::Ran => {
                    retries = 0;
                    // Post-task: the worker is searching again (its next
                    // step re-scans) — re-arm the spinner mark so the
                    // between-tasks window elides submission wakes too.
                    if spinner_mode && !local.spinning() {
                        rt.spin_enter_worker(&mut local);
                    }
                }
                Step::Retry => {
                    local.drive.steps_retry += 1;
                    retries += 1;
                    if retries >= crate::sched::RETRY_PARK_AFTER {
                        retries = 0;
                        crate::io::note_permit(false);
                        rt.execution_permits().release();
                        exit.held = false;
                        if !spin_for_epoch(rt, epoch) {
                            if directed {
                                rt.park_worker_directed(epoch, &parker, &mut local);
                            } else {
                                rt.park(epoch);
                            }
                            if tree_mode {
                                rt.wake_tree_continue();
                            }
                        }
                    } else {
                        std::thread::yield_now();
                    }
                }
                Step::Idle => {
                    retries = 0;
                    collect_seen = mcx::answer_collect(collect_seen);
                    crate::io::note_permit(false);
                    rt.execution_permits().release();
                    exit.held = false;
                    if !spin_for_epoch(rt, epoch) {
                        // D4781 (`fold_on_park`): an idle worker's pending counts reach its account before it parks
                        memledger::park_fold();
                        if directed {
                            rt.park_worker_directed(epoch, &parker, &mut local);
                        } else {
                            rt.park(epoch);
                        }
                        if tree_mode {
                            rt.wake_tree_continue();
                        }
                    }
                }
                Step::Stop => break,
            }
        }
    } else {
        loop {
            let epoch = rt.park_epoch();
            rt.execution_permits().acquire();
            crate::io::note_permit(true);
            exit.held = true;
            let step = rt.worker_step(&mut local);
            crate::io::note_permit(false);
            rt.execution_permits().release();
            exit.held = false;
            // Task boundary (§2.9): drain this worker's CQEs non-blockingly.
            crate::io::boundary_reap();
            match step {
                Step::Ran => {}
                // Retry: the coordinator is mid-invalidation (slot word
                // invalid, active bit not yet cleared) — a transient window;
                // yield rather than burn the permit path (and the loom models
                // require the no-lone-progress hint on exactly this edge).
                Step::Retry => std::thread::yield_now(),
                Step::Idle => {
                    collect_seen = mcx::answer_collect(collect_seen);
                    memledger::park_fold();
                    rt.park(epoch)
                }
                Step::Stop => break,
            }
        }
    }
}

struct LoopExit<'a> {
    rt: &'a Arc<Runtime>,
    worker: usize,
    held: bool,
}

impl Drop for LoopExit<'_> {
    fn drop(&mut self) {
        // D611: an unwind out of worker_step already gave the step permit back
        // (sched.rs PermitOnUnwind), or gave it up in an aio wait or a bound
        // serve; release only the permit this thread still holds.
        if self.held && crate::io::permit_held() {
            crate::io::note_permit(false);
            self.rt.execution_permits().release();
        }
        self.rt.register_worker_ring(self.worker, None);
        crate::io::worker_exit();
    }
}

/// Handle to a spawned pool (join-able; tests and clean shutdown).
pub struct WorkerPool {
    rt: Arc<Runtime>,
    handles: Vec<JoinHandle<()>>,
}

impl WorkerPool {
    /// Spawn `nthreads = workers + standbys` loop threads with plain std
    /// threads (tests; production uses [`WorkerPool::spawn_with`] via
    /// launch_backend::rtpool).
    pub fn spawn_std(rt: Arc<Runtime>) -> io::Result<WorkerPool> {
        Self::spawn_with(rt, |ordinal, body| {
            std::thread::Builder::new()
                .name(format!("pgrust-rtworker-{ordinal}"))
                .spawn(pagegive::pinned(body))
        })
    }

    /// Spawn through a caller-provided spawner (launch_backend's rtpool
    /// passes one that reserves synthetic pids and applies the child
    /// prelude). The spawner returns a JoinHandle so shutdown can join.
    pub fn spawn_with(
        rt: Arc<Runtime>,
        mut spawn: impl FnMut(usize, Box<dyn FnOnce() + Send>) -> io::Result<JoinHandle<()>>,
    ) -> io::Result<WorkerPool> {
        let n = rt.nthreads();
        let mut handles = Vec::with_capacity(n);
        for ordinal in 0..n {
            let rt2 = Arc::clone(&rt);
            handles.push(spawn(ordinal, Box::new(move || worker_loop(&rt2, ordinal)))?);
        }
        Ok(WorkerPool { rt, handles })
    }

    pub fn runtime(&self) -> &Arc<Runtime> {
        &self.rt
    }

    /// Stop and join all workers (tests / clean shutdown; the production
    /// pool is process-lifetime and never calls this).
    pub fn shutdown(mut self) {
        self.rt.request_stop();
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
    }
}
