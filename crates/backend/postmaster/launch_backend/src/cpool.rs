//! D4682 step 1: client backend thread reuse (the `backend_thread_reuse` knob of `PGRUST_MEMPOLICY`, default 0 = off).
//!
//! A client backend thread whose session ends in a clean proc_exit(0) (the exit-callback drain and the session-memory
//! teardown both ran whole) parks instead of exiting. It keeps its allocator heap (the exit-time `release_retained` is
//! skipped) and its thread-local block, and the postmaster hands it the next client connection. That session runs the
//! unchanged child body: InitPostmasterChild's per-session half (the once-per-thread half, the wait-event support, the
//! local latch slot and the latch wait set, survived the park), then InitPostgres cold, exactly as on a fresh thread.
//! Nothing session-level carries over: [`reset_session_state`] puts back every thread-local that outlives the
//! teardown and is session-level (the list is its body plus the sites it names), so the next session starts from a
//! fresh thread's values.
//!
//! Bounds (memory policy, idle-trim ruling 10-10): at most `backend_thread_reuse_max` threads park (a session ending
//! over the cap exits its thread as today), and a parked thread idle for `backend_thread_reuse_idle_ms` leaves the pool
//! and exits, handing its heap back through the same release a normal thread exit runs. A reload or a crash flushes the
//! pool (wpool::flush / flush_for_crash): parked threads hold no shared-memory identity (no PGPROC, no sinval slot),
//! so a flushed thread just exits.
//!
//! Reaper protocol (the wpool standby one): the park decision is made before the session's exit announce and marks
//! it (wpool::mark_parked_announce), so join_announced_child skips the join and the thread's CHILD_THREADS entry stays,
//! re-keyed to the next session's pid at its claim. A thread leaving the pool drops its own entry
//! (wpool::retire_child_thread).

use std::cell::Cell;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering::Relaxed};

use pgsync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use pgsync::Mutex;

use mempolicy::consts::{get, K};
use types_core::{init::BackendType, pid_t};
use types_startup::{ClientSocket, StartupData};

/// Everything the launcher captures for a spawn, handed to a parked thread instead.
struct ClientTask {
    child_pid: pid_t,
    child_slot: i32,
    startup_data: StartupData,
    client_sock: Option<ClientSocket>,
    inherited: super::Inherited,
    guc: GucPrelude,
}

/// The spawn path's GUC bring-up, captured on the postmaster thread at the claim (the base as of this connection).
struct GucPrelude {
    base: std::sync::Arc<guc::layers::GucBaseSnapshot>,
    snapshot: Vec<guc::store::NondefaultGuc>,
}

impl GucPrelude {
    fn capture() -> Self {
        let base = guc::layers::ensure_base_current();
        let snapshot = if guc::layers::base_share_enabled() {
            Vec::new()
        } else {
            guc::store::capture_nondefault_variables()
        };
        GucPrelude { base, snapshot }
    }

    /// A new registry from the boot values and the base, as the spawn closure builds it: every option's backing is
    /// rewritten (a boot value or the base's), so no SET of the previous session survives.
    fn apply(&self) -> types_error::PgResult<()> {
        if guc::layers::base_share_enabled() {
            guc::store::initialize_guc_options_for_child_base(&self.base)
                .and_then(|()| guc::layers::bind_base(&self.base))
        } else {
            guc::store::initialize_guc_options_for_child(&self.snapshot)
                .and_then(|()| guc::store::restore_nondefault_variables(&self.snapshot))
        }
    }
}

struct Parked {
    id: u64,
    tx: SyncSender<ClientTask>,
    /// The thread's retained heap when it parked (mcx::thread_retained; 0 without a reading): the number a RETAINED
    /// row reads (`parked_retained`).
    retained: usize,
}

// Parked threads, most recently parked last (a claim takes the warmest).
static PARKED: Mutex<Vec<Parked>> = Mutex::new(Vec::new());
// Sessions that decided to park and have not reached PARKED yet; counted against the cap under PARKED's lock.
static RESERVED: AtomicI32 = AtomicI32::new(0);
// Bumped by flush: a session that decided to park before it must not enter the drained pool.
static FLUSH_EPOCH: AtomicU64 = AtomicU64::new(0);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
// Counters for the measurement (read by stats()): claims served by a parked thread, parks, idle exits, sessions that
// ended over the cap, parks refused after the teardown (a reset or a cleanup failed), and threads over the retained
// heap bound.
static CLAIMS: AtomicU64 = AtomicU64::new(0);
static PARKS: AtomicU64 = AtomicU64::new(0);
static IDLE_EXITS: AtomicU64 = AtomicU64::new(0);
static OVER_CAP: AtomicU64 = AtomicU64::new(0);
static REFUSED: AtomicU64 = AtomicU64::new(0);
static OVER_RETAINED: AtomicU64 = AtomicU64::new(0);

thread_local! {
    // The running session is on a reused thread: run_child_task takes the warm prelude.
    static REUSED: Cell<bool> = const { Cell::new(false) };
    // run_child_task decided to park this thread: the flush epoch at the decision and the thread's retained heap then.
    static WILL_PARK: Cell<Option<(u64, usize)>> = const { Cell::new(None) };
}

fn parked() -> pgsync::MutexGuard<'static, Vec<Parked>> {
    PARKED.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn enabled() -> bool {
    get(K::BackendThreadReuse) != 0
}

/// D4682 step 2 (TRIAL, backend_thread_reuse_keep, default 0): a reused thread keeps same-database session state.
fn keep() -> bool {
    enabled() && get(K::BackendThreadReuseKeep) != 0
}

fn cap() -> usize {
    get(K::BackendThreadReuseMax) as usize
}

/// Broker flip condition 4 (backend_thread_reuse_retained_mib): the bound on a parking thread's retained heap, a bound
/// on the parked pool without a timer. A thread over it exits as today, its heap going back with the exit's release.
fn over_retained_bound(retained: Option<usize>) -> bool {
    over_bound(retained, get(K::BackendThreadReuseRetainedMib))
}

/// Over `mib` MiB (0 = no bound; no reading = within it).
fn over_bound(retained: Option<usize>, mib: u64) -> bool {
    mib != 0 && retained.is_some_and(|b| b as u64 > mib << 20)
}

/// The parked threads and the sum of their retained heaps at park, in bytes: what a RETAINED row reads (Memory
/// Accounting's parked_thread_retained; the readings were taken by each thread at its park, as no thread may walk
/// another's pages).
pub fn parked_retained() -> (usize, u64) {
    let p = parked();
    (p.len(), p.iter().map(|x| x.retained as u64).sum())
}

/// (claims, parks, idle exits, over cap, refused, over the retained bound, parked now)
pub fn stats() -> (u64, u64, u64, u64, u64, u64, usize) {
    (
        CLAIMS.load(Relaxed),
        PARKS.load(Relaxed),
        IDLE_EXITS.load(Relaxed),
        OVER_CAP.load(Relaxed),
        REFUSED.load(Relaxed),
        OVER_RETAINED.load(Relaxed),
        parked().len(),
    )
}

/// True while the running session is on a reused thread (run_child_task's prelude arm).
pub(super) fn reused() -> bool {
    REUSED.get()
}

/// Postmaster thread (postmaster_child_launch, a B_BACKEND launch): hand the connection to a parked thread. Returns
/// the session's fresh pid, or gives the startup data and socket back for the spawn path when no thread is parked.
pub(super) fn try_dispatch(
    child_slot: i32,
    mut startup_data: StartupData,
    mut client_sock: Option<ClientSocket>,
) -> Result<pid_t, (StartupData, Option<ClientSocket>)> {
    if !enabled() {
        return Err((startup_data, client_sock));
    }
    loop {
        let Some(sb) = parked().pop() else {
            return Err((startup_data, client_sock));
        };
        // A fresh pid per session: the previous session's exit announce may still be queued at the reaper.
        let Some(child_pid) = super::reserve_child_pid() else {
            parked().push(sb);
            return Err((startup_data, client_sock));
        };
        // Pre-identity signal window, as the spawn path: deliverable before the caller publishes the pid.
        procsignal::PreIdentitySignalPrepare(child_pid);
        let task = ClientTask {
            child_pid,
            child_slot,
            startup_data,
            client_sock,
            inherited: super::Inherited::capture(),
            guc: GucPrelude::capture(),
        };
        match sb.tx.send(task) {
            Ok(()) => {
                CLAIMS.fetch_add(1, Relaxed);
                return Ok(child_pid);
            }
            Err(pgsync::mpsc::SendError(task)) => {
                // The thread left (flush or idle exit raced the pop): take the next one.
                procsignal::PreIdentitySignalDiscard(child_pid);
                super::release_reserved_pid(child_pid);
                startup_data = task.startup_data;
                client_sock = task.client_sock;
            }
        }
    }
}

/// run_child_task, after the session-memory teardown of a clean proc_exit(0), before the exit announce: whether this
/// thread parks. Resets the session state first (a failure there, or a cleanup the teardown left, means exit) and
/// takes a place under the cap. True = the caller marks the announce as a park and skips the heap release.
pub(super) fn decide_park(child_type: BackendType, child_pid: pid_t) -> bool {
    if !enabled()
        || child_type != BackendType::Backend
        || miscinit::GetMyBackendType() != BackendType::Backend
    {
        return false;
    }
    // The launcher pushes our CHILD_THREADS entry right after the spawn; a session that ended before that (never in
    // practice) exits as today rather than park without the entry the claim re-keys.
    let has_entry = super::CHILD_THREADS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .any(|(p, _)| *p == child_pid);
    if !has_entry || mcxt_stats::session_cleanup_count() != 0 {
        REFUSED.fetch_add(1, Relaxed);
        return false;
    }
    let retained = mcx::thread_retained();
    if over_retained_bound(retained) {
        OVER_RETAINED.fetch_add(1, Relaxed);
        return false;
    }
    // unwind-ok: log-then-die (a reset that fails leaves the thread to exit as today)
    if std::panic::catch_unwind(reset_session_state).is_err() {
        REFUSED.fetch_add(1, Relaxed);
        return false;
    }
    let epoch = FLUSH_EPOCH.load(Relaxed);
    let p = parked();
    if p.len() + RESERVED.load(Relaxed) as usize >= cap() {
        OVER_CAP.fetch_add(1, Relaxed);
        return false;
    }
    RESERVED.fetch_add(1, Relaxed);
    drop(p);
    WILL_PARK.set(Some((epoch, retained.unwrap_or(0))));
    true
}

/// run_child_task's prelude on a reused thread, in place of the fresh InitPostmasterChild arm.
pub(super) fn warm_prelude(child_pid: pid_t) -> types_error::PgResult<()> {
    miscinit::InitReusedPostmasterChild(child_pid)?;
    // As the wretain claim: handles published for the previous session go stale.
    waiteventset_seams::rekey_wakeup_registry::call();
    procsignal::pqsignal_thread(
        procsignal::signums::SIGQUIT,
        procsignal::ThreadSignalHandler::Simple(super::default_sigquit_handler),
    );
    procsignal::pqsignal_thread(
        procsignal::signums::SIGABRT,
        procsignal::ThreadSignalHandler::Simple(super::default_sigabrt_handler),
    );
    procsignal::PreIdentitySignalAdopt(child_pid);
    Ok(())
}

/// The spawn closure, after its first session: while each session ends in a park, wait for the next connection and
/// run it. Returns when the thread leaves the pool; the closure then ends and the thread exits.
pub(super) fn serve_parked(first_pid: pid_t) {
    let mut key = first_pid;
    let mut served = false;
    while let Some((epoch, retained)) = WILL_PARK.take() {
        served = true;
        let Some((id, rx)) = park(epoch, retained) else {
            // Flushed between the decision and the park: our announce was marked a park, so drop our own entry.
            super::wpool::retire_child_thread(key);
            break;
        };
        let Some(task) = wait(id, &rx) else {
            super::wpool::retire_child_thread(key);
            break;
        };
        rekey(key, task.child_pid);
        key = task.child_pid;
        task.inherited.apply();
        if let Err(e) = task.guc.apply() {
            // Refused like a spawn whose GUC bring-up fails; its announce is a real exit, the reaper joins us.
            super::fail_child_startup(BackendType::Backend, task.child_pid, task.client_sock, &e);
            return;
        }
        REUSED.set(true);
        super::run_child_task(
            BackendType::Backend,
            task.child_pid,
            task.child_slot,
            task.startup_data,
            task.client_sock,
        );
        REUSED.set(false);
    }
    if served {
        // A thread leaving the pool hands its freed heap back, as the joiner of an exited thread does.
        mcx::release_retained_at_exit();
    }
}

fn park(epoch: u64, retained: usize) -> Option<(u64, Receiver<ClientTask>)> {
    let (tx, rx) = pgsync::mpsc::sync_channel::<ClientTask>(1);
    let id = NEXT_ID.fetch_add(1, Relaxed);
    let mut p = parked();
    RESERVED.fetch_sub(1, Relaxed);
    if FLUSH_EPOCH.load(Relaxed) != epoch || !enabled() {
        return None;
    }
    p.push(Parked { id, tx, retained });
    PARKS.fetch_add(1, Relaxed);
    Some((id, rx))
}

/// The next connection, or None when the thread leaves the pool: flushed, or idle past the bound (0 = no bound).
fn wait(id: u64, rx: &Receiver<ClientTask>) -> Option<ClientTask> {
    let idle_ms = get(K::BackendThreadReuseIdleMs);
    if idle_ms == 0 {
        return rx.recv().ok();
    }
    match rx.recv_timeout(std::time::Duration::from_millis(idle_ms)) {
        Ok(task) => Some(task),
        Err(RecvTimeoutError::Disconnected) => None,
        Err(RecvTimeoutError::Timeout) => {
            // Leave under the pool's lock; a dispatcher that already popped us has sent (or is sending) its task.
            let mut p = parked();
            if let Some(i) = p.iter().position(|s| s.id == id) {
                p.swap_remove(i);
                IDLE_EXITS.fetch_add(1, Relaxed);
                return None;
            }
            drop(p);
            rx.recv().ok()
        }
    }
}

fn rekey(old_pid: pid_t, new_pid: pid_t) {
    let mut t = super::CHILD_THREADS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(entry) = t.iter_mut().find(|(p, _)| *p == old_pid) {
        entry.0 = new_pid;
        super::release_reserved_pid(old_pid);
    }
}

/// Reload and crash (wpool::flush / flush_for_crash): every parked thread leaves (its closed channel wakes it).
pub fn flush() {
    FLUSH_EPOCH.fetch_add(1, Relaxed);
    parked().clear();
}

/// The session-level thread-locals that outlive the teardown of a clean proc_exit, put back to a fresh thread's
/// values. The exit-callback drain (ShutdownPostgres: the transaction, portals, session locks incl. advisory locks;
/// Async_UnlistenOnExit; RemoveTempRelationsCallback; ProcKill; the pgstat shutdown hook; socket_close) and
/// run_session_teardown (every session-root estate: caches, prepared statements, plans, the GUC-free estate) already
/// ran; this is what remains. Each line names the static it puts back.
fn reset_session_state() {
    // The exit flags commit_to_exit set (ipc PROC_EXIT_INPROGRESS / SHMEM_EXIT_INPROGRESS, elog's copy, the holdoff and
    // critical-section counts, statement-log suppression).
    ipc::reset_exit_state_for_retained_park();
    // miscinit lib.rs MODE, MY_BACKEND_TYPE, IGNORE_SYSTEM_INDEXES, CLIENT_AUTHN_ID / CLIENT_AUTH_METHOD; userid.rs the
    // authenticated / session / outer / current user, SECURITY_RESTRICTION_CONTEXT, SET ROLE, SYSTEM_USER.
    miscinit::ResetSessionForThreadReuse();
    // procsignal THREAD_SIGNAL_HANDLERS + MY_PROC_SIGNAL_SLOT: the session's dispositions (die, StatementCancelHandler,
    // procsignal_sigusr1_handler, ...) go; the prelude installs the fresh thread's SIGQUIT / SIGABRT.
    let mut unbound = procsignal::ThreadSignalIdentity::unbound();
    procsignal::swap_thread_signal_identity(&mut unbound);
    // init_small globals.rs: interrupt flags and counts, cancel key, proc number, database id / tablespace / path,
    // the client socket.
    init_small::globals::reset_session_scalars_for_thread_reuse();
    // pqcomm socket.rs FE_BE_WAIT_SET (its epoll set; socket_close closed the socket) and CLIENT_STATE.
    pqcomm::reset_for_thread_reuse();
    // fd vfd.rs FD (the virtual file cache, its descriptors closed) and NUM_EXTERNAL_FDS (the client socket's
    // reservation): without it every session leaked one reservation until "Too many open files" (AcquireExternalFD).
    fd::vfd::reset_for_thread_reuse();
    // pgstat lib.rs IS_INITIALIZED: the shutdown hook ran; the next InitPostgres runs pgstat_initialize again.
    pgstat::reset_for_thread_reuse();
    // mbutils lib.rs: client / database / message encodings, BACKEND_STARTUP_COMPLETE, PENDING_CLIENT_ENCODING, the
    // resolved conversion procs and CONV_PROC_LIST.
    mbutils::reset_for_thread_reuse();
    // catalog_namespace: the temp namespace (MY_TEMP_NAMESPACE / _TOAST_ / _SUB_ID) and the search path state.
    catalog_namespace::reset_for_thread_reuse();
    // Catalog-keyed caches outside any session root, so the teardown leaves them, and a parked thread (no sinval slot)
    // misses the invalidations sent while it waits: superuser LAST_ROLEID / LAST_ROLEID_IS_SUPER, attoptcache
    // ATTOPT_CACHE, cache_evtcache STATE + CACHE.
    superuser::reset_for_thread_reuse();
    attoptcache::reset_for_thread_reuse();
    cache_evtcache::reset_for_thread_reuse();
    // adt_acl membership.rs CACHE + CACHED_DB_HASH: role memberships (pg_hba `+group` and the reserved-connections
    // check read them before initialize_acl runs).
    adt_acl::reset_for_thread_reuse();
    // dfmgr FILE_LIST: the loaded libraries and the _PG_init-ran list (the next session's registry is new).
    dfmgr::reset_for_thread_reuse();
    // The latches modules registered with mcx::register_thread_reuse_hook (gravity exec run.rs DESC_GONE, D1506).
    mcx::run_thread_reuse_hooks();
    // memledger DOOR (Memory Accounting flip condition 6): the exit fold before the wait (drain check, door 1's pending,
    // kind lanes and rounding onto UNBOUND, the caches' held-back hits) and the door's account back to UNBOUND, so
    // nothing of this session folds onto the next one.
    memledger::park_thread();
}

#[cfg(test)]
mod tests {
    use super::over_bound;

    #[test]
    fn a_thread_over_the_retained_bound_exits_and_no_reading_or_no_bound_parks() {
        assert!(over_bound(Some((64 << 20) + 1), 64));
        assert!(!over_bound(Some(64 << 20), 64));
        assert!(!over_bound(Some(11 << 20), 64), "a sort-heavy statement's thread reading (~10.5 MiB) parks");
        assert!(!over_bound(None, 64), "no hook (wasm, unit rigs): no reading, the bound does not apply");
        assert!(!over_bound(Some(usize::MAX), 0), "0 = no bound");
    }
}
