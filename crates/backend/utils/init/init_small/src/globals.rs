#![allow(non_snake_case)]

use std::cell::{Cell, RefCell};
use std::mem::ManuallyDrop;

use types_core::{
    pg_time_t, pid_t, uint32, uint8, InvalidOid, Oid, ProcNumber, ProtocolVersion, TimestampTz,
    DATEORDER_MDY, INTSTYLE_POSTGRES, INVALID_PROC_NUMBER, MAXPGPATH, MAX_CANCEL_KEY_LENGTH,
    PG_DIR_MODE_OWNER, USE_ISO_DATES,
};
use types_storage::latch::LatchHandle;

pub fn TopMemoryContext() -> &'static mcx::MemoryContext {
    thread_local! {
        static CTX: Cell<Option<&'static mcx::MemoryContext>> = const { Cell::new(None) };
    }
    CTX.with(|c| match c.get() {
        Some(m) => m,
        None => {
            let m = mcx::session_root("TopMemoryContext");
            c.set(Some(m));
            // D4682: the teardown retires (poisons) the root; a reused thread's next session makes a new one.
            mcx::register_session_cleanup(Box::new(|| {
                let _ = CTX.try_with(|c| c.set(None));
            }));
            m
        }
    })
}

pub use types_error::severity::{
    exit_on_any_error as ExitOnAnyError, set_exit_on_any_error as SetExitOnAnyError,
};

// One backend = one thread; every globals.c variable is a per-backend
// `thread_local!`, const-init, !needs_drop (AGENTS.md rule 10).
macro_rules! scalar_global {
    ($($cell:ident, $get:ident, $set:ident, $ty:ty, $init:expr;)+) => {
        $(
            thread_local! {
                static $cell: Cell<$ty> = const {
                    assert!(!core::mem::needs_drop::<$ty>());
                    Cell::new($init)
                };
            }

            #[inline]
            pub fn $get() -> $ty {
                $cell.get()
            }

            #[inline]
            pub fn $set(value: $ty) {
                $cell.set(value);
            }
        )+
    };
}

// C `int data_directory_mode` (globals.c): set once by checkDataDir() in the
// postmaster before any children exist, constant afterwards, fork-inherited.
// PROCESS-GLOBAL here — a thread-local (even one copied at child launch) can
// go stale for worker-pool threads spawned before checkDataDir ran, leaving
// `SHOW data_directory_mode` at 0700 on a group-access (0750) cluster
// (pg_basebackup/010 group-permission leg).
static DATA_DIRECTORY_MODE: std::sync::atomic::AtomicI32 =
    std::sync::atomic::AtomicI32::new(PG_DIR_MODE_OWNER as i32);

#[inline]
pub fn data_directory_mode() -> i32 {
    DATA_DIRECTORY_MODE.load(std::sync::atomic::Ordering::Relaxed)
}

#[inline]
pub fn set_data_directory_mode(mode: i32) {
    DATA_DIRECTORY_MODE.store(mode, std::sync::atomic::Ordering::Relaxed);
}

// Crash-test fault point (armed only under PGRUST_CRASH_TEST): the backend
// vpid whose next heap_insert parks, page changed and content lock held,
// before its WAL insert, until a kill ends it. 0 = none.
static CRASH_TEST_PARK_PID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

#[inline]
pub fn crash_test_park_pid() -> i32 {
    CRASH_TEST_PARK_PID.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn set_crash_test_park_pid(pid: i32) {
    CRASH_TEST_PARK_PID.store(pid, std::sync::atomic::Ordering::Relaxed);
}

// PGC_INTERNAL: checkDataDir sets the value (miscinit.c:428); GUC
// initialization publishing the 0700 boot value must not clobber it.
pub fn guc_ignores_data_directory_mode(_mode: i32) {}

// C backends setsid() at start, so a popen'd COPY PROGRAM child sits in the
// backend's process group and kill(-pid, sig) from pg_signal_backend /
// signal_child reaches it. One process here: each backend's live children
// are recorded under its (synthetic) MyProcPid so thread-signal delivery can
// fan the group signals out to them.
pgsync::process_global! { static BACKEND_CHILDREN: pgsync::Mutex<Vec<(i32, u32)>> =
    pgsync::Mutex::new(Vec::new()); }

pub fn register_backend_child(child_pid: u32) {
    BACKEND_CHILDREN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((MyProcPid(), child_pid));
}

pub fn unregister_backend_child(child_pid: u32) {
    BACKEND_CHILDREN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|&(_, c)| c != child_pid);
}

pub fn backend_children(pid: i32) -> Vec<u32> {
    BACKEND_CHILDREN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|&&(p, _)| p == pid)
        .map(|&(_, c)| c)
        .collect()
}

scalar_global! {
    FRONTEND_PROTOCOL, FrontendProtocol, SetFrontendProtocol, ProtocolVersion, 0;

    QUERY_CANCEL_PENDING, QueryCancelPending, SetQueryCancelPending, bool, false;
    PROC_DIE_PENDING, ProcDiePending, SetProcDiePending, bool, false;
    CHECK_CLIENT_CONNECTION_PENDING, CheckClientConnectionPending,
        SetCheckClientConnectionPending, bool, false;
    CLIENT_CONNECTION_LOST, ClientConnectionLost, SetClientConnectionLost, bool, false;
    IDLE_IN_TRANSACTION_SESSION_TIMEOUT_PENDING, IdleInTransactionSessionTimeoutPending,
        SetIdleInTransactionSessionTimeoutPending, bool, false;
    TRANSACTION_TIMEOUT_PENDING, TransactionTimeoutPending, SetTransactionTimeoutPending,
        bool, false;
    IDLE_SESSION_TIMEOUT_PENDING, IdleSessionTimeoutPending, SetIdleSessionTimeoutPending,
        bool, false;
    PROC_SIGNAL_BARRIER_PENDING, ProcSignalBarrierPending, SetProcSignalBarrierPending,
        bool, false;
    LOG_MEMORY_CONTEXT_PENDING, LogMemoryContextPending, SetLogMemoryContextPending,
        bool, false;
    PARALLEL_MESSAGE_PENDING, ParallelMessagePending, SetParallelMessagePending, bool, false;
    // upstream 58c1188a3eaa (18.4): Fix slotsync worker blocking promotion when stuck in wait
    // (slotsync.c's SlotSyncShutdownPending; read by ProcessInterrupts).
    SLOT_SYNC_SHUTDOWN_PENDING, SlotSyncShutdownPending, SetSlotSyncShutdownPending, bool, false;
    IDLE_STATS_UPDATE_TIMEOUT_PENDING, IdleStatsUpdateTimeoutPending,
        SetIdleStatsUpdateTimeoutPending, bool, false;
    // pgrust-only (docs/design/connection-scaling.md D3.4): idle passivation.
    IDLE_PASSIVATE_TIMEOUT_PENDING, IdlePassivateTimeoutPending,
        SetIdlePassivateTimeoutPending, bool, false;

    INTERRUPT_HOLDOFF_COUNT, InterruptHoldoffCount, SetInterruptHoldoffCount, uint32, 0;
    QUERY_CANCEL_HOLDOFF_COUNT, QueryCancelHoldoffCount, SetQueryCancelHoldoffCount, uint32, 0;
    CRIT_SECTION_COUNT, CritSectionCount, SetCritSectionCount, uint32, 0;

    MY_PROC_PID, MyProcPid, SetMyProcPid, i32, 0;
    // `Latch *MyLatch`: None is C's NULL; miscinit points it at the process-
    // local latch or PGPROC's procLatch. Storage lives in the latch unit.
    MY_LATCH, MyLatch, SetMyLatch, Option<LatchHandle>, None;
    MY_START_TIME, MyStartTime, SetMyStartTime, pg_time_t, 0;
    MY_START_TIMESTAMP, MyStartTimestamp, SetMyStartTimestamp, TimestampTz, 0;
    MY_CANCEL_KEY, MyCancelKey, SetMyCancelKey, [uint8; MAX_CANCEL_KEY_LENGTH],
        [0; MAX_CANCEL_KEY_LENGTH];
    MY_CANCEL_KEY_LENGTH, MyCancelKeyLength, SetMyCancelKeyLength, i32, 0;
    MY_PM_CHILD_SLOT, MyPMChildSlot, SetMyPMChildSlot, i32, 0;

    OUTPUT_FILE_NAME, OutputFileName, SetOutputFileName, [u8; MAXPGPATH], [0; MAXPGPATH];
    MY_EXEC_PATH, my_exec_path, set_my_exec_path, [u8; MAXPGPATH], [0; MAXPGPATH];
    PKGLIB_PATH, pkglib_path, set_pkglib_path, [u8; MAXPGPATH], [0; MAXPGPATH];

    MY_PROC_NUMBER, MyProcNumber, SetMyProcNumber, ProcNumber, INVALID_PROC_NUMBER;
    PARALLEL_LEADER_PROC_NUMBER, ParallelLeaderProcNumber, SetParallelLeaderProcNumber,
        ProcNumber, INVALID_PROC_NUMBER;

    MY_DATABASE_ID, MyDatabaseId, SetMyDatabaseId, Oid, InvalidOid;
    // pgstat.c's `bool pgstat_report_fixed`: written directly by the xlog
    // insert path, read/cleared by pgstat_report_stat.
    PGSTAT_REPORT_FIXED, PgStatReportFixed, SetPgStatReportFixed, bool, false;
    MY_DATABASE_TABLE_SPACE, MyDatabaseTableSpace, SetMyDatabaseTableSpace, Oid, InvalidOid;
    MY_DATABASE_HAS_LOGIN_EVENT_TRIGGERS, MyDatabaseHasLoginEventTriggers,
        SetMyDatabaseHasLoginEventTriggers, bool, false;

    POSTMASTER_PID, PostmasterPid, SetPostmasterPid, pid_t, 0;

    IS_POSTMASTER_ENVIRONMENT, IsPostmasterEnvironment, SetIsPostmasterEnvironment, bool, false;
    IS_UNDER_POSTMASTER, IsUnderPostmaster, SetIsUnderPostmaster, bool, false;
    IS_BINARY_UPGRADE, IsBinaryUpgrade, SetIsBinaryUpgrade, bool, false;

    DATE_STYLE, DateStyle, SetDateStyle, i32, USE_ISO_DATES;
    DATE_ORDER, DateOrder, SetDateOrder, i32, DATEORDER_MDY;
    INTERVAL_STYLE, IntervalStyle, SetIntervalStyle, i32, INTSTYLE_POSTGRES;

    ENABLE_FSYNC, enableFsync, set_enableFsync, bool, true;
    ALLOW_SYSTEM_TABLE_MODS, allowSystemTableMods, set_allowSystemTableMods, bool, false;
    WORK_MEM, work_mem, set_work_mem, i32, 4096;
    HASH_MEM_MULTIPLIER, hash_mem_multiplier, set_hash_mem_multiplier, f64, 2.0;
    MAINTENANCE_WORK_MEM, maintenance_work_mem, set_maintenance_work_mem, i32, 65536;
    MAX_PARALLEL_MAINTENANCE_WORKERS, max_parallel_maintenance_workers,
        set_max_parallel_maintenance_workers, i32, 2;

    N_BUFFERS, NBuffers, SetNBuffers, i32, 16384;
    MAX_CONNECTIONS, MaxConnections, SetMaxConnections, i32, 100;
    // 8->16, matching guc_tables::tables boot_vals (see jit-parallel-defaults).
    MAX_WORKER_PROCESSES, max_worker_processes, set_max_worker_processes, i32, 16;
    MAX_PARALLEL_WORKERS, max_parallel_workers, set_max_parallel_workers, i32, 16;
    MAX_BACKENDS, MaxBackends, SetMaxBackends, i32, 0;
    // M2 pool-binding: PGPROCs boot-reserved for the STANDING runtime
    // executor gang (parallel::standing) — an extra MaxBackends term and a
    // widened bgworker freelist segment (postinit::InitializeMaxBackends,
    // lmgr_proc::InitProcGlobal), so the gang never competes with the
    // bgworker/parallel-worker budgets. Set by the postmaster BEFORE
    // InitializeMaxBackends, only under PGRUST_RUNTIME=1; default 0 =
    // byte-identical sizing.
    RUNTIME_GANG_PROCS, RuntimeGangProcs, SetRuntimeGangProcs, i32, 0;

    VACUUM_BUFFER_USAGE_LIMIT, VacuumBufferUsageLimit, SetVacuumBufferUsageLimit, i32, 2048;
    VACUUM_COST_PAGE_HIT, VacuumCostPageHit, SetVacuumCostPageHit, i32, 1;
    VACUUM_COST_PAGE_MISS, VacuumCostPageMiss, SetVacuumCostPageMiss, i32, 2;
    VACUUM_COST_PAGE_DIRTY, VacuumCostPageDirty, SetVacuumCostPageDirty, i32, 20;
    VACUUM_COST_LIMIT, VacuumCostLimit, SetVacuumCostLimit, i32, 200;
    VACUUM_COST_DELAY, VacuumCostDelay, SetVacuumCostDelay, f64, 0.0;
    VACUUM_COST_BALANCE, VacuumCostBalance, SetVacuumCostBalance, i32, 0;
    VACUUM_COST_ACTIVE, VacuumCostActive, SetVacuumCostActive, bool, false;

    AUTOVACUUM_FREEZE_MAX_AGE, autovacuum_freeze_max_age, set_autovacuum_freeze_max_age,
        i32, 200000000;

    COMMIT_TIMESTAMP_BUFFERS, commit_timestamp_buffers, set_commit_timestamp_buffers, i32, 0;
    MULTIXACT_MEMBER_BUFFERS, multixact_member_buffers, set_multixact_member_buffers, i32, 32;
    MULTIXACT_OFFSET_BUFFERS, multixact_offset_buffers, set_multixact_offset_buffers, i32, 16;
    NOTIFY_BUFFERS, notify_buffers, set_notify_buffers, i32, 16;
    SERIALIZABLE_BUFFERS, serializable_buffers, set_serializable_buffers, i32, 32;
    SUBTRANSACTION_BUFFERS, subtransaction_buffers, set_subtransaction_buffers, i32, 0;
    TRANSACTION_BUFFERS, transaction_buffers, set_transaction_buffers, i32, 0;
}

// Cross-thread-writable, CFI-visible async wakes; leaked (notes/timeout-threads.md).
thread_local! {
    static INTERRUPT_PENDING: Cell<*const std::sync::atomic::AtomicBool> =
        const { Cell::new(std::ptr::null()) };
}

#[inline]
pub fn interrupt_pending_flag() -> &'static std::sync::atomic::AtomicBool {
    let p = INTERRUPT_PENDING.get();
    if p.is_null() {
        claim_interrupt_pending_flag()
    } else {
        // SAFETY: only ever null or Box::leak'd.
        unsafe { &*p }
    }
}

#[cold]
#[inline(never)]
fn claim_interrupt_pending_flag() -> &'static std::sync::atomic::AtomicBool {
    let flag: &'static _ = Box::leak(Box::new(std::sync::atomic::AtomicBool::new(false)));
    INTERRUPT_PENDING.set(flag);
    flag
}

// The Gravity kernels' interrupt byte (docs/gravity/driver.md §6), two bits. `ABORT_BIT`: the abort class, raised
// beside InterruptPending by the senders whose processing always ends the statement — a SIGINT / SIGTERM thread
// signal, a statement / lock / transaction / idle timeout — and never by the deferrable ones (config reload, NOTIFY,
// parallel messages, memory context dumps, barriers, the client connection check); cleared by ProcessAbortInterrupts /
// ProcessInterrupts. `SERVICE_BIT` (D3406): a deferrable thread signal is pending (procsignal raises it beside
// InterruptPending); cleared by ProcessInterrupts. A Gravity kernel loads this byte at its loop back-edge (its address
// travels in the Binding) instead of InterruptPending: a set byte calls the kernel poll (gravity_abi `ctx::poll`), which
// on the owner's thread runs CHECK_FOR_INTERRUPTS and goes on unless it raised (C's CFI: a busy statement absorbs a
// ProcSignalBarrier, logs its memory contexts) and on another thread (a pool worker reading its leader's byte) stops
// on the abort bit only. Every other reader (`AbortPending`, the Gravity driver's and pool's abort polls) reads the
// abort bit alone.
pub const ABORT_BIT: u8 = 1;
pub const SERVICE_BIT: u8 = 2;
/// C5 (revoke): another statement's draw or the memory ladder asked this backend's running statement to spill
/// (mcx::revoke, the same value); the kernels' poll cuts the region each participant fills and clears it. (Bit 4 is the
/// D3887 poll-gap probe's, cancelpoll `PROBE_BIT`.)
pub const REVOKE_BIT: u8 = 8;

thread_local! {
    static ABORT_PENDING: Cell<*const std::sync::atomic::AtomicU8> =
        const { Cell::new(std::ptr::null()) };
}

#[inline]
pub fn abort_pending_flag() -> &'static std::sync::atomic::AtomicU8 {
    let p = ABORT_PENDING.get();
    if p.is_null() {
        claim_abort_pending_flag()
    } else {
        // SAFETY: only ever null or Box::leak'd.
        unsafe { &*p }
    }
}

#[cold]
#[inline(never)]
fn claim_abort_pending_flag() -> &'static std::sync::atomic::AtomicU8 {
    let flag: &'static _ = Box::leak(Box::new(std::sync::atomic::AtomicU8::new(0)));
    ABORT_PENDING.set(flag);
    flag
}

#[inline]
pub fn AbortPending() -> bool {
    let p = ABORT_PENDING.get();
    // SAFETY: only ever null or Box::leak'd.
    !p.is_null() && unsafe { &*p }.load(std::sync::atomic::Ordering::Relaxed) & ABORT_BIT != 0
}

#[inline]
pub fn SetAbortPending(value: bool) {
    if value {
        abort_pending_flag().fetch_or(ABORT_BIT, std::sync::atomic::Ordering::SeqCst);
    } else {
        abort_pending_flag().fetch_and(!ABORT_BIT, std::sync::atomic::Ordering::SeqCst);
    }
}

/// D3406: a deferrable thread signal is pending for the kernels' poll (`SERVICE_BIT`).
#[inline]
pub fn ServicePending() -> bool {
    let p = ABORT_PENDING.get();
    // SAFETY: only ever null or Box::leak'd.
    !p.is_null() && unsafe { &*p }.load(std::sync::atomic::Ordering::Relaxed) & SERVICE_BIT != 0
}

/// D3406: the deferrable wake is being serviced (ProcessInterrupts, the kernels' poll): cleared before the drain, so a
/// delivery after it raises the bit again.
#[inline]
pub fn ClearServicePending() {
    // wasm32: the bit stays while a deadline or the embedder's cancel is polled (`SetDuePoll`, `SetHostCancel`), so the
    // kernels keep polling it.
    #[cfg(target_family = "wasm")]
    if DUE_POLL.get().is_some() || HOST_CANCEL.get() {
        return;
    }
    let p = ABORT_PENDING.get();
    if !p.is_null() {
        // SAFETY: only ever null or Box::leak'd.
        unsafe { &*p }.fetch_and(!SERVICE_BIT, std::sync::atomic::Ordering::SeqCst);
    }
}

/// A query cancel consumed or discarded outside ProcessInterrupts (C `QueryCancelPending = false`), with the abort-class
/// byte it raised beside InterruptPending (D334): left set, the byte outlived its cancel into the next statement, whose
/// Gravity kernels read it at their first window and ended that statement with 57014. A pending die keeps its byte (the
/// terminate still ends the statement it reaches). ProcessInterrupts clears the byte itself.
#[inline]
pub fn ClearQueryCancelPending() {
    SetQueryCancelPending(false);
    if !ProcDiePending() {
        SetAbortPending(false);
    }
}

// Per-tuple hot (C's `if (INTERRUPT_PENDING)` shape): leaf, no claim edge —
// unclaimed reads false; every setter/publisher claims via
// interrupt_pending_flag() first, so null never hides a raised interrupt.
#[inline(always)]
pub fn InterruptPending() -> bool {
    let p = INTERRUPT_PENDING.get();
    // SAFETY: only ever null or Box::leak'd.
    let set = !p.is_null() && unsafe { &*p }.load(std::sync::atomic::Ordering::Relaxed);
    #[cfg(target_family = "wasm")]
    if !set && (DUE_POLL.get().is_some() || HOST_CANCEL.get()) {
        return due_passed();
    }
    set
}

// wasm32 has one thread and no signals: no timer thread can deliver a timeout's wake (the timeout crate's `timer`), so
// the backend polls the deadline of its armed wake itself. While one is armed, `DUE_POLL` holds the timeout crate's
// check (true = its deadline passed): InterruptPending reads true from then on, so the next CHECK_FOR_INTERRUPTS reaches
// ProcessInterrupts, whose timeout drain fires the wake, as SIGALRM's handler would have. The kernels' service bit stays
// set meanwhile, so a running Gravity program polls it too (its poll runs CHECK_FOR_INTERRUPTS when InterruptPending).
#[cfg(target_family = "wasm")]
thread_local! {
    static DUE_POLL: Cell<Option<fn() -> bool>> = const { Cell::new(None) };
    static DUE_SKIP: Cell<u32> = const { Cell::new(0) };
    static HOST_CANCEL: Cell<bool> = const { Cell::new(false) };
    static HOST_CANCEL_SEEN: Cell<bool> = const { Cell::new(false) };
}

// wasm32: a browser page cancels the running statement (its Stop button, psql's Ctrl-C) through a word it shares with the
// server's worker, which the server reads through this import; nothing else can reach a busy single-threaded instance.
#[cfg(target_family = "wasm")]
mod embedder {
    #[link(wasm_import_module = "env")]
    extern "C" {
        /// 1: a cancel was requested since the last call (the call clears it); 0: none; -1: the embedder has no
        /// cancel channel (a page that is not cross-origin isolated, a wasmtime CLI).
        pub(super) fn pgrust_cancel_poll() -> i32;
    }
}

/// wasm32: poll the embedder's cancel word beside the deadline (`InterruptPending`, a sleep's slice: `PollHostCancel`);
/// false when the embedder has none. A cancel read is taken by the next ProcessInterrupts (`TakeHostCancel`) as a SIGINT
/// drain is: StatementCancelHandler there.
#[cfg(target_family = "wasm")]
pub fn SetHostCancel() -> bool {
    // SAFETY: the import takes nothing and answers a word.
    if unsafe { embedder::pgrust_cancel_poll() } < 0 {
        return false;
    }
    HOST_CANCEL.set(true);
    abort_pending_flag().fetch_or(SERVICE_BIT, std::sync::atomic::Ordering::SeqCst);
    true
}

/// wasm32: read the embedder's cancel word now; a cancel raises InterruptPending and the abort bit, as a SIGINT's
/// delivery does, and is kept for `TakeHostCancel`. Whether one was read.
#[cfg(target_family = "wasm")]
#[cold]
#[inline(never)]
pub fn PollHostCancel() -> bool {
    // SAFETY: as in SetHostCancel.
    if !HOST_CANCEL.get() || unsafe { embedder::pgrust_cancel_poll() } <= 0 {
        return false;
    }
    HOST_CANCEL_SEEN.set(true);
    abort_pending_flag().fetch_or(ABORT_BIT, std::sync::atomic::Ordering::SeqCst);
    interrupt_pending_flag().store(true, std::sync::atomic::Ordering::SeqCst);
    true
}

/// wasm32: a cancel the embedder requested since the last call (`PollHostCancel`).
#[cfg(target_family = "wasm")]
pub fn TakeHostCancel() -> bool {
    HOST_CANCEL_SEEN.replace(false)
}

/// wasm32: the clock is read at every `DUE_EVERY`th InterruptPending only (a host call per row is too dear); a
/// Gravity kernel polls once per window, so its deadline is seen within a few windows. With no deadline armed, the
/// embedder's cancel word alone is read at every `CANCEL_EVERY`th.
#[cfg(target_family = "wasm")]
const DUE_EVERY: u32 = 8;
#[cfg(target_family = "wasm")]
const CANCEL_EVERY: u32 = 64;

#[cfg(target_family = "wasm")]
#[cold]
#[inline(never)]
fn due_passed() -> bool {
    let skip = DUE_SKIP.get();
    if skip > 0 {
        DUE_SKIP.set(skip - 1);
        return false;
    }
    let due = DUE_POLL.get();
    DUE_SKIP.set((if due.is_some() { DUE_EVERY } else { CANCEL_EVERY }) - 1);
    if PollHostCancel() {
        return true;
    }
    if !due.is_some_and(|passed| passed()) {
        return false;
    }
    // Seen passed: InterruptPending itself is raised, so it stays pending until ProcessInterrupts takes it (a caller
    // that finds nothing raised and checks again must not find it gone between two clock reads).
    interrupt_pending_flag().store(true, std::sync::atomic::Ordering::SeqCst);
    true
}

/// wasm32: poll `passed` for the armed wake's deadline (`None`: no wake is armed). See `DUE_POLL`.
#[cfg(target_family = "wasm")]
pub fn SetDuePoll(passed: Option<fn() -> bool>) {
    DUE_POLL.set(passed);
    DUE_SKIP.set(0);
    if passed.is_some() {
        abort_pending_flag().fetch_or(SERVICE_BIT, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Raise this thread's InterruptPending only if the thread already claimed its flag: the global allocator's
/// failure path (oom.rs) must not allocate, and a thread without a flag has no CHECK_FOR_INTERRUPTS to reach.
#[inline]
pub fn RaiseInterruptPendingNoAlloc() {
    let p = INTERRUPT_PENDING.get();
    if !p.is_null() {
        // SAFETY: only ever null or Box::leak'd.
        unsafe { &*p }.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[inline]
pub fn SetInterruptPending(value: bool) {
    interrupt_pending_flag().store(value, std::sync::atomic::Ordering::SeqCst);
}

// No C counterpart (D4024): the errors ProcessInterrupts raised on this thread, counted, and the last one until a
// (sub)transaction abort or the next statement forgets it. A reader that answers a failed catalog read as unknown
// (Gravity's relcache facts) drops the error and the interrupt with it, which C's longjmp cannot. Inside a Gravity
// admission (REDELIVER > 0) each later CHECK_FOR_INTERRUPTS raises the noted error again, and the admission raises
// it at its end. A PL handler that catches one rolls its subtransaction back first, so a caught one is forgotten.
// ManuallyDrop: const-init, no TLS destructor.
thread_local! {
    static INTERRUPTS_RAISED: Cell<u64> = const { Cell::new(0) };
    static LAST_INTERRUPT: RefCell<Option<ManuallyDrop<Box<types_error::PgError>>>> = const { RefCell::new(None) };
    static REDELIVER: Cell<u32> = const { Cell::new(0) };
}

fn put_last_interrupt(e: Option<ManuallyDrop<Box<types_error::PgError>>>) -> Option<Box<types_error::PgError>> {
    LAST_INTERRUPT.with(|l| core::mem::replace(&mut *l.borrow_mut(), e)).map(ManuallyDrop::into_inner)
}

/// True when an admission raises it again: InterruptPending is set again (the caller sets the latch).
#[cold]
pub fn NoteInterruptRaised(e: &types_error::PgError) -> bool {
    INTERRUPTS_RAISED.set(INTERRUPTS_RAISED.get().wrapping_add(1));
    drop(put_last_interrupt(Some(ManuallyDrop::new(Box::new(e.clone())))));
    let again = REDELIVER.get() > 0;
    if again {
        SetInterruptPending(true);
    }
    again
}

#[inline]
pub fn ForgetInterruptRaised() {
    if LAST_INTERRUPT.with(|l| l.borrow().is_some()) {
        drop(put_last_interrupt(None));
    }
}

#[inline]
pub fn InterruptsRaised() -> u64 {
    INTERRUPTS_RAISED.get()
}

#[cold]
pub fn TakeInterruptRaised() -> Option<Box<types_error::PgError>> {
    put_last_interrupt(None)
}

#[inline]
pub fn SetRedeliverInterrupts(depth: u32) -> u32 {
    REDELIVER.replace(depth)
}

#[inline]
pub fn RedeliverInterrupts() -> u32 {
    REDELIVER.get()
}

#[cold]
pub fn RedeliveredInterrupt() -> Option<Box<types_error::PgError>> {
    if REDELIVER.get() == 0 {
        return None;
    }
    LAST_INTERRUPT.with(|l| l.borrow().as_ref().map(|e| Box::new((***e).clone())))
}

// `ClientSocket *MyClientSocket` / `struct Port *MyProcPort` (globals.c).
// Port is droppy: ManuallyDrop keeps the TLS const-init with no dtor state
// machine; the Port leaks at thread exit as C's TopMemoryContext copy does at
// process exit (per-connection reclaim is thread-model debt, see M5 note in
// docs/strategy.md — session state must not assume thread identity).
thread_local! {
    static MY_CLIENT_SOCKET: Cell<Option<types_startup::ClientSocket>> = const { Cell::new(None) };
    static MY_PROC_PORT: RefCell<Option<ManuallyDrop<Box<types_startup::Port>>>> =
        const { RefCell::new(None) };
    static PARSED_AUTH_LINES: RefCell<Option<types_startup::ParsedAuthLines>> =
        const { RefCell::new(None) };
    static PORT_LOG_VIEW: RefCell<Option<types_startup::PortLogView>> = const { RefCell::new(None) };
}

/// `WithMyProcPort` for long mutable sections that report (C's
/// ClientAuthentication): log_line_prefix keeps reading the port's user,
/// database and host through `TryWithPortLogView` meanwhile.
pub fn WithMyProcPortLogged<R>(f: impl FnOnce(&mut types_startup::Port) -> R) -> R {
    struct ClearView;
    impl Drop for ClearView {
        fn drop(&mut self) {
            PORT_LOG_VIEW.with(|v| *v.borrow_mut() = None);
        }
    }
    let view = TryWithMyProcPort(|p| p.log_view());
    PORT_LOG_VIEW.with(|v| *v.borrow_mut() = view);
    let _clear = ClearView;
    WithMyProcPortMut(f)
}

// The Port itself when readable, else the view a WithMyProcPortLogged
// section published.
pub fn TryWithPortLogView<R>(f: impl FnOnce(&types_startup::PortLogView) -> R) -> Option<R> {
    if let Some(r) = TryWithMyProcPort(|p| p.log_view()) {
        return Some(f(&r));
    }
    PORT_LOG_VIEW.with(|v| v.try_borrow().ok()?.as_ref().map(f))
}

// hba.c's parsed_hba_lines / parsed_ident_lines as this thread inherited
// them from the postmaster at launch (a child's fork copy); None on the
// postmaster thread, which owns the live ones.
pub fn InheritedParsedAuthLines() -> Option<types_startup::ParsedAuthLines> {
    PARSED_AUTH_LINES.with(|p| p.borrow().clone())
}

pub fn InheritParsedAuthLines(lines: types_startup::ParsedAuthLines) {
    PARSED_AUTH_LINES.with(|p| *p.borrow_mut() = Some(lines));
}

pub fn MyClientSocket() -> Option<types_startup::ClientSocket> {
    MY_CLIENT_SOCKET.get()
}

pub fn SetMyClientSocket(sock: types_startup::ClientSocket) {
    MY_CLIENT_SOCKET.set(Some(sock));
}

pub fn HaveMyProcPort() -> bool {
    // A borrow in flight (ClientAuthentication holds the port while its
    // FATAL formats log_line_prefix) means the port is installed.
    MY_PROC_PORT.with(|p| p.try_borrow().map_or(true, |slot| slot.is_some()))
}

// None when unset or while a WithMyProcPort borrow is in flight.
pub fn TryWithMyProcPort<R>(f: impl FnOnce(&types_startup::Port) -> R) -> Option<R> {
    MY_PROC_PORT.with(|p| {
        let slot = p.try_borrow().ok()?;
        slot.as_ref().map(|port| f(port))
    })
}

/// Session-memory teardown (FPBUDGET-1): free the Port copy at clean task
/// end (C's TopMemoryContext copy dies with the backend process). Nothing
/// reads MyProcPort after teardown; the thread is exiting.
pub fn SessionMemTeardownPort() {
    MY_PROC_PORT.with(|p| {
        if let Some(port) = p.borrow_mut().take() {
            drop(ManuallyDrop::into_inner(port));
        }
    });
}

pub fn SetMyProcPort(port: types_startup::Port) {
    MY_PROC_PORT.with(|p| *p.borrow_mut() = Some(ManuallyDrop::new(Box::new(port))));
}

// The log sink reads the Port inside every ereport (has_client_port, %u %d
// %a %h): a mutable borrow spans a field store only, never a call that logs.
pub fn WithMyProcPort<R>(f: impl FnOnce(&types_startup::Port) -> R) -> R {
    MY_PROC_PORT.with(|p| {
        let slot = p.borrow();
        let port = slot.as_ref().expect("MyProcPort is not set");
        f(port)
    })
}

pub fn WithMyProcPortMut<R>(f: impl FnOnce(&mut types_startup::Port) -> R) -> R {
    MY_PROC_PORT.with(|p| {
        let mut slot = p.borrow_mut();
        let port = slot.as_mut().expect("MyProcPort is not set");
        f(port)
    })
}

// `char *DataDir` / `char *DatabasePath`: set once per backend, never freed in
// C; the leaked &'static str keeps reads a plain load with no per-read clone.
thread_local! {
    static DATA_DIR: Cell<Option<&'static str>> = const { Cell::new(None) };
    static DATABASE_PATH: Cell<Option<&'static str>> = const { Cell::new(None) };
}

pub fn DataDir() -> Option<&'static str> {
    DATA_DIR.get()
}

pub fn SetDataDir(value: &str) {
    DATA_DIR.set(Some(String::from(value).leak()));
}

pub fn InheritDataDir(value: &'static str) {
    DATA_DIR.set(Some(value));
}

pub fn DatabasePath() -> Option<&'static str> {
    DATABASE_PATH.get()
}

pub fn SetDatabasePath(value: &str) {
    DATABASE_PATH.set(Some(Box::leak(Box::<str>::from(value))));
}

// `pfree(DatabasePath); DatabasePath = NULL` (inval.c:1163, the recovery
// poke via miscinit, once per replayed init-file invalidation).
pub fn ClearDatabasePath() {
    drop(TakeDatabasePath());
}

pub fn TakeDatabasePath() -> Option<Box<str>> {
    let path = DATABASE_PATH.take()?;
    // SAFETY: the only writer is SetDatabasePath, which stores a
    // Box::<str>::leak'd allocation, and take() removed the only handle.
    Some(unsafe { Box::from_raw(path as *const str as *mut str) })
}

// miscadmin.h / c.h interrupt macros over the counters above. Per-query hot
// family (frontend reads, WAL critical sections): keep as inline Cell ops.

/// `HOLD_INTERRUPTS()`
#[inline]
pub fn HoldInterrupts() {
    SetInterruptHoldoffCount(InterruptHoldoffCount() + 1);
}

/// `RESUME_INTERRUPTS()`
#[inline]
pub fn ResumeInterrupts() {
    let count = InterruptHoldoffCount();
    // C's Assert: debug-only. This rides every LWLockRelease (per-snapshot
    // hot); a release-mode panic branch here is cost C does not pay.
    debug_assert!(count > 0, "InterruptHoldoffCount underflow");
    SetInterruptHoldoffCount(count.saturating_sub(1));
}

/// `HOLD_INTERRUPTS()` until the guard drops (`RESUME_INTERRUPTS()`), for code that cannot return an error: a Drop that
/// reaches CHECK_FOR_INTERRUPTS (FileSet's temp-directory walk, fd walkdir) would otherwise have ProcessInterrupts
/// clear the pending cancel / timeout / terminate and raise it into a `let _ =` (D376). Held, the check leaves every
/// flag pending for the caller's next check, as PostgreSQL's cleanup runs under HOLD_INTERRUPTS.
#[must_use = "the interrupts are held only while the guard lives"]
pub struct InterruptsHeld(());

/// `HOLD_INTERRUPTS()` now, `RESUME_INTERRUPTS()` when the returned guard drops ([`InterruptsHeld`]).
#[inline]
pub fn hold_interrupts() -> InterruptsHeld {
    HoldInterrupts();
    InterruptsHeld(())
}

impl Drop for InterruptsHeld {
    #[inline]
    fn drop(&mut self) {
        ResumeInterrupts();
    }
}

/// `HOLD_CANCEL_INTERRUPTS()`
#[inline]
pub fn HoldCancelInterrupts() {
    SetQueryCancelHoldoffCount(QueryCancelHoldoffCount() + 1);
}

/// `RESUME_CANCEL_INTERRUPTS()`
#[inline]
pub fn ResumeCancelInterrupts() {
    let count = QueryCancelHoldoffCount();
    assert!(count > 0, "QueryCancelHoldoffCount underflow");
    SetQueryCancelHoldoffCount(count - 1);
}

/// `START_CRIT_SECTION()`
#[inline]
pub fn StartCriticalSection() {
    SetCritSectionCount(CritSectionCount() + 1);
}

/// `END_CRIT_SECTION()`
#[inline]
pub fn EndCriticalSection() {
    let count = CritSectionCount();
    assert!(count > 0, "CritSectionCount underflow");
    SetCritSectionCount(count - 1);
}

/// `INTERRUPTS_CAN_BE_PROCESSED()`
#[inline]
pub fn InterruptsCanBeProcessed() -> bool {
    InterruptHoldoffCount() == 0 && CritSectionCount() == 0 && QueryCancelHoldoffCount() == 0
}

// The pid that owns the postmaster lock files: the process pg_ctl launched.
// Under the crash supervisor that is the supervisor, which outlives every
// server generation as C's postmaster outlives its children; 0 = this one.
static LAUNCHED_PID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

pub fn launched_pid() -> i32 {
    match LAUNCHED_PID.load(std::sync::atomic::Ordering::Relaxed) {
        0 => process_id() as i32,
        pid => pid,
    }
}

pub fn set_launched_pid(pid: i32) {
    LAUNCHED_PID.store(pid, std::sync::atomic::Ordering::Relaxed);
}

/// `getpid()` / `std::process::id()` with wasm and sim arms. WASI has no
/// process ids (std::process::id PANICS there); the instance is the only
/// process that can exist, so a fixed synthetic pid stands in. 1 is C's
/// classic "the only process" number; nonzero matters (0/negative are
/// sentinel words in the lockfile/procsignal vocabularies).
///
/// Under `--cfg pgrust_sim` the OS pid is ambient entropy: it reaches the
/// wire (BackendKeyData) and file names (xlogtemp), and the P4 sim-net
/// determinism gate byte-compares both across runs — the sim node is one
/// process, so the same fixed synthetic pid stands in (P2 seeded-RNG law
/// applied to the one non-RNG identity source).
#[inline]
pub fn process_id() -> u32 {
    #[cfg(not(any(target_family = "wasm", pgrust_sim)))]
    {
        std::process::id()
    }
    #[cfg(any(target_family = "wasm", pgrust_sim))]
    {
        1
    }
}

/// The tag for a temp file name that C builds from `getpid()` alone
/// (`xlogtemp.%d`): `<pid>.<n>`, with `n` from a process-wide counter, so
/// every call gets a name no other thread holds. C runs one backend per
/// process, so the pid is unique among concurrent creators; here every
/// backend and auxiliary process is a thread of one process, and a pid-only
/// name let two threads unlink or rename each other's temp file (P9). Not
/// `MyProcPid` / `MyProcNumber`: threads without a PGPROC (and test
/// harnesses) share those. Callers keep C's prefix, so the startup sweeps
/// that match it still remove leftovers.
pub fn temp_file_tag() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{}.{n}", process_id())
}

/// C's `ProcNumberForTempRelations()` macro (procnumber.h:52-54), verbatim:
///
/// ```c
/// (ParallelLeaderProcNumber == INVALID_PROC_NUMBER ? MyProcNumber : ParallelLeaderProcNumber)
/// ```
///
/// The proc number to use for OUR SESSION's temp relations is normally our own,
/// but a parallel worker must use its LEADER's — a temp relation's storage is
/// keyed by proc number (`t<procno>_<relnode>`) and lives in the leader's local
/// buffers, so a worker resolving against its own slot would look in the wrong
/// place entirely.
///
/// Today this returns `MyProcNumber()` on every call, because
/// `SetParallelLeaderProcNumber` has no callers: the global is never populated.
/// That is *why* the two relcache sites that open-coded `MyProcNumber()` were
/// not a live defect — but the equivalence rested on that accident plus four
/// independent guards keeping workers away from temp relations, not on the
/// expression being right. Spelled out here so the sites stop asserting a macro
/// they did not implement, and so wiring the setter is the only remaining step
/// rather than one of two. See notes/GL-VACGUARD-1-letter.md §10.
#[inline]
pub fn ProcNumberForTempRelations() -> ProcNumber {
    let leader = ParallelLeaderProcNumber();
    if leader == INVALID_PROC_NUMBER {
        MyProcNumber()
    } else {
        leader
    }
}

/// NUM_AUXILIARY_PROCS, plus one while the instance aux is enabled.
#[inline]
pub fn NumAuxiliaryProcs() -> i32 {
    types_storage::NUM_AUXILIARY_PROCS + crate::instaux::instance_aux_count()
}

/// D4682 (client backend thread reuse, launch_backend cpool): this file's per-session thread-locals back to a fresh
/// thread's values once a session's teardown parked its thread. Kept: the values every launch hands the thread again
/// (Inherited: paths, POSTMASTER settings, the parsed auth lines), the thread's own machinery (MyLatch, the local latch
/// again since ProcKill; the leaked interrupt / abort flag cells, cleared here) and TopMemoryContext's slot (its own
/// session cleanup clears it). MY_PROC_PORT went with the session teardown (SessionMemTeardownPort).
pub fn reset_session_scalars_for_thread_reuse() {
    SetInterruptPending(false);
    SetAbortPending(false);
    ClearServicePending();
    SetQueryCancelPending(false);
    SetProcDiePending(false);
    SetCheckClientConnectionPending(false);
    SetClientConnectionLost(false);
    SetIdleInTransactionSessionTimeoutPending(false);
    SetTransactionTimeoutPending(false);
    SetIdleSessionTimeoutPending(false);
    SetProcSignalBarrierPending(false);
    SetLogMemoryContextPending(false);
    SetParallelMessagePending(false);
    SetSlotSyncShutdownPending(false);
    SetIdleStatsUpdateTimeoutPending(false);
    SetIdlePassivateTimeoutPending(false);
    SetInterruptHoldoffCount(0);
    SetQueryCancelHoldoffCount(0);
    SetCritSectionCount(0);
    ForgetInterruptRaised();
    drop(TakeInterruptRaised());
    SetRedeliverInterrupts(0);
    SetFrontendProtocol(0);
    SetMyCancelKey([0; MAX_CANCEL_KEY_LENGTH]);
    SetMyCancelKeyLength(0);
    SetMyProcNumber(INVALID_PROC_NUMBER);
    SetParallelLeaderProcNumber(INVALID_PROC_NUMBER);
    SetMyDatabaseId(InvalidOid);
    SetPgStatReportFixed(false);
    SetMyDatabaseTableSpace(InvalidOid);
    SetMyDatabaseHasLoginEventTriggers(false);
    SetVacuumCostBalance(0);
    SetVacuumCostActive(false);
    MY_CLIENT_SOCKET.set(None);
    PORT_LOG_VIEW.with(|v| *v.borrow_mut() = None);
    // SetDatabasePath expects none (InitPostgres sets it once per session).
    let _ = DATABASE_PATH.take();
}
