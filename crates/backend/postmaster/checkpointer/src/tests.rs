use super::*;

// Shmem is a set-once process global and the tests below mutate it:
// init through one gate and serialize the mutators.
static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

// Shared by every test that needs the sizing/alloc seams and LWLocks: the
// seams are set-once, so one gate installs them for all.
fn stub_shmem_seams_once() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        shmem_seams::mul_size::set(|a, b| Ok(a.checked_mul(b).expect("mul_size overflow")));
        shmem_seams::add_size::set(|a, b| Ok(a.checked_add(b).expect("add_size overflow")));
        shmem_seams::shmem_alloc::set(|size| {
            Ok(Box::leak(vec![0u8; size].into_boxed_slice()).as_mut_ptr())
        });
        lwlock::CreateLWLocks(false).unwrap();
    });
}

fn procsignal_seams_once() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(procsignal::init_seams);
}

fn shmem_for_tests() -> &'static CheckpointerShmemStruct {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| CheckpointerShmemInit(64).unwrap());
    shmem()
}

#[test]
fn comm_lock_offset_is_checkpointer_comm() {
    assert_eq!(
        lwlock::GetLWTrancheName(CHECKPOINTER_COMM_LOCK_OFFSET as u16),
        "CheckpointerComm"
    );
}

#[test]
fn shmem_sizing_caps_at_max_requests() {
    let one = core::mem::size_of::<types_storage::SyncCell<u8>>();
    let _ = one;
    assert!(CheckpointerShmemSize(16384) > CheckpointerShmemSize(0));
}

#[test]
fn main_fn_matches_child_main_shape() {
    let f: fn(&types_startup::StartupData) -> ! = CheckpointerMain;
    let _ = f;
}

#[test]
fn main_loop_refreshes_shared_config_on_every_entry() {
    let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    struct ConfigUpdated;
    syncrep_seams::sync_rep_update_sync_standbys_defined::set(|| {
        std::panic::panic_any(ConfigUpdated);
    });
    for _ in 0..2 {
        let result = std::panic::catch_unwind(checkpointer_main_loop);
        assert!(result.unwrap_err().is::<ConfigUpdated>());
    }
}

fn test_request(rel: u32) -> CheckpointerRequest {
    CheckpointerRequest {
        req_type: SyncRequestType::SYNC_REQUEST,
        ftag: FileTag::new(
            types_storage::sync::SyncRequestHandler::SYNC_HANDLER_MD,
            types_core::ForkNumber::MAIN_FORKNUM,
            types_storage::RelFileLocator { spcOid: 1663, dbOid: 5, relNumber: rel },
            0,
        ),
    }
}

// Control: the post-clear absorb loop passes every drained request through
// in order when nothing fails.
#[test]
fn remember_absorbed_requests_processes_all() {
    let buf = [test_request(1), test_request(2), test_request(3)];
    let seen = std::cell::RefCell::new(Vec::new());
    remember_absorbed_requests(&buf, |ftag, req_type| {
        assert_eq!(req_type, SyncRequestType::SYNC_REQUEST);
        seen.borrow_mut().push(ftag.rlocator.relNumber);
        Ok(())
    });
    assert_eq!(*seen.borrow(), [1, 2, 3]);
}

// Issue #57: once the shared queue is cleared, a remember failure must be
// fatal (C PANICs here), never a recoverable Err — and the panic payload
// must be a plain string so panic-to-error converters cannot map it back to
// a client ERROR the way they re-raise PgError payloads.
#[test]
fn remember_absorbed_requests_escalates_failure_to_panic() {
    let buf = [test_request(1), test_request(2), test_request(3)];
    let seen = std::cell::RefCell::new(Vec::new());
    let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        remember_absorbed_requests(&buf, |ftag, _| {
            if ftag.rlocator.relNumber == 2 {
                return Err(Box::new(PgError::new(ERROR, "out of memory allocating pendingOps")));
            }
            seen.borrow_mut().push(ftag.rlocator.relNumber);
            Ok(())
        });
    }))
    .unwrap_err();
    let msg = payload
        .downcast_ref::<String>()
        .expect("crash-and-restart string payload, not a re-raisable PgError");
    assert!(msg.contains("could not absorb fsync request"), "got: {msg}");
    // Requests before the failure were already remembered (kept, like C).
    assert_eq!(*seen.borrow(), [1]);
}

// Issue #57 catch-site guard: an Err reaching the recovery loop with the
// crit-section count still nonzero bypassed errstart's ERROR->PANIC
// promotion (into_error); abort_cleanup must escalate instead of laundering
// the leaked section with SetCritSectionCount(0) and recovering.
#[test]
fn abort_cleanup_escalates_leaked_crit_section() {
    let err = PgError::new(ERROR, "out of memory allocating pendingOps");
    g::StartCriticalSection();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        abort_cleanup(&err);
    }));
    g::SetCritSectionCount(0);
    let payload = result.unwrap_err();
    let msg = payload
        .downcast_ref::<&'static str>()
        .expect("crash-and-restart string payload");
    assert!(msg.contains("critical section"), "got: {msg}");
}

#[test]
fn crash_reset_restores_boot_image() {
    let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cp = shmem_for_tests();
    spin_acquire(&cp.ckpt_lck);
    cp.checkpointer_pid.store(77, Relaxed);
    cp.ckpt_started.store(3, Relaxed);
    cp.ckpt_done.store(2, Relaxed);
    cp.ckpt_failed.store(1, Relaxed);
    cp.ckpt_flags.store(CHECKPOINT_IMMEDIATE, Relaxed);
    // SAFETY: test-exclusive (TEST_LOCK held); serialized by CheckpointerCommLock
    unsafe { cp.num_requests.set(5) };
    ReqShutdownXLOG();

    CheckpointerShmemResetAfterCrash();

    assert!(cp.ckpt_lck.is_free());
    assert_eq!(cp.checkpointer_pid.load(Relaxed), 0);
    assert_eq!(cp.ckpt_started.load(Relaxed), 0);
    assert_eq!(cp.ckpt_done.load(Relaxed), 0);
    assert_eq!(cp.ckpt_failed.load(Relaxed), 0);
    assert_eq!(cp.ckpt_flags.load(Relaxed), 0);
    // SAFETY: test-exclusive (TEST_LOCK held); serialized by CheckpointerCommLock
    assert_eq!(unsafe { cp.num_requests.get() }, 0);
    assert!(!SHUTDOWN_XLOG_PENDING.load(Relaxed));
}

// Issue #56 sibling site: AbsorbSyncRequests' scratch reserve fails BEFORE
// the critical section (as C pallocs before START_CRIT_SECTION), so it must
// surface as ERROR 53200 through the PgResult — food for the main loop's
// sigsetjmp analog — with the comm lock left for abort_cleanup's
// LWLockReleaseAll, never as a thread-killing panic.
#[test]
fn absorb_oom_is_error_and_lock_release_recovers() {
    let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cp = shmem_for_tests();
    stub_shmem_seams_once();

    let prev_type = miscinit::GetMyBackendType();
    miscinit::SetMyBackendType(types_core::BackendType::Checkpointer);
    // SAFETY: test-exclusive (TEST_LOCK held); serialized by CheckpointerCommLock
    unsafe { cp.num_requests.set(4) };

    ABSORB_SCRATCH_TEST_LIMIT.store(1, Relaxed);
    let res = AbsorbSyncRequests();
    ABSORB_SCRATCH_TEST_LIMIT.store(0, Relaxed);
    // SAFETY: test-exclusive (TEST_LOCK held); serialized by CheckpointerCommLock
    unsafe { cp.num_requests.set(0) };

    let err = res.expect_err("capped absorb scratch must fail the reserve");
    assert_eq!(err.sqlstate, types_error::ERRCODE_OUT_OF_MEMORY);
    assert_eq!(err.message, "out of memory");

    // The ERROR escapes with the comm lock held, exactly like C's elog(ERROR)
    // there; the recovery loop's abort_cleanup releases it wholesale.
    assert!(lwlock::LWLockHeldByMe(checkpointer_comm_lock()));
    lwlock::LWLockReleaseAll().unwrap();
    assert!(!lwlock::LWLockHeldByMe(checkpointer_comm_lock()));

    // The path is retryable once the lock is back: the (empty) queue drains
    // to completion on the very scratch vec that just failed.
    AbsorbSyncRequests().expect("retry after recovery succeeds");
    assert!(!lwlock::LWLockHeldByMe(checkpointer_comm_lock()));

    miscinit::SetMyBackendType(prev_type);
}

// audit-18.6 b123: CheckpointerShmemInit is a ShmemInitStruct("Checkpointer
// Data", CheckpointerShmemSize()) allocation (checkpointer.c:966), so the
// block is registered in the ShmemIndex (pg_shmem_allocations lists it) and a
// re-entry finds it (found = true) instead of allocating a second one.
#[test]
fn shmem_init_registers_checkpointer_data() {
    let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cp = shmem_for_tests();
    let (raw, found) =
        shmem::ShmemInitStruct("Checkpointer Data", CheckpointerShmemSize(64)).unwrap();
    assert!(found, "Checkpointer Data is not registered in the ShmemIndex");
    assert!(
        std::ptr::eq(raw.cast::<CheckpointerShmemStruct>(), cp),
        "CheckpointerShmem does not live in the ShmemIndex block"
    );
    assert_eq!(cp.max_requests, 64);
    assert_eq!(cp.requests.len(), 64);
}

#[test]
fn signal_pended_before_reset_latch_is_handled_by_process_interrupts() {
    let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    const CKPT_PID: i32 = 4_000_017;
    std::thread::spawn(|| {
        g::SetMaxConnections(4);
        g::set_max_worker_processes(2);
        g::SetMaxBackends(4 + 3 + 2 + 2 + types_storage::storage::NUM_SPECIAL_WORKER_PROCS);
        g::SetMyProcPid(CKPT_PID);
        static SETUP: std::sync::Once = std::sync::Once::new();
        SETUP.call_once(|| {
            latch::init_seams();
            procsignal_seams_once();
            procsignal::ProcSignalShmemInit();
        });
        miscinit::InitProcessLocalLatch().expect("local latch");
        procsignal::PreIdentitySignalAdopt(CKPT_PID);
        procsignal::pqsignal_thread(
            procsignal::signums::SIGUSR2,
            procsignal::ThreadSignalHandler::Simple(interrupt::SignalHandlerForShutdownRequest),
        );
        assert!(!interrupt::ShutdownRequestPending());

        assert_eq!(procsignal::SendThreadSignal(CKPT_PID, procsignal::signums::SIGUSR2), 0);
        latch::ResetLatch(g::MyLatch().expect("MyLatch"));
        ProcessCheckpointerInterrupts().expect("interrupts");

        let seen = interrupt::ShutdownRequestPending();
        procsignal::PreIdentitySignalRelease();
        interrupt::SetShutdownRequestPending(false);
        assert!(
            seen,
            "a SIGUSR2 pended before ResetLatch must be handled by ProcessCheckpointerInterrupts \
             (else the shutdown-wait loop parks forever: D2500)"
        );
    })
    .join()
    .expect("test thread");
}

// checkpointer.c:829: a SIGHUP/SIGUSR1 pended while the checkpoint writes
// buffers is dispatched by CheckpointWriteDelay itself, so a barrier or
// reload does not wait for the write loop to finish.
static HUP_OBSERVED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
fn observe_hup() {
    HUP_OBSERVED.store(true, Relaxed);
}

#[test]
fn checkpoint_write_delay_drains_pended_thread_signals() {
    let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        stub_shmem_seams_once();
        init_small::globals::SetMaxBackends(16);
        procsignal::ProcSignalShmemInit();
        procsignal_seams_once();
    });
    const PID: i32 = 424_151;
    procsignal::PreIdentitySignalAdopt(PID);
    procsignal::pqsignal_thread(
        procsignal::signums::SIGHUP,
        procsignal::ThreadSignalHandler::Simple(observe_hup),
    );
    let prev_type = miscinit::GetMyBackendType();
    miscinit::SetMyBackendType(types_core::BackendType::Checkpointer);
    HUP_OBSERVED.store(false, Relaxed);
    assert_eq!(procsignal::SendThreadSignal(PID, procsignal::signums::SIGHUP), 0);
    assert!(!HUP_OBSERVED.load(Relaxed));
    CheckpointWriteDelay(CHECKPOINT_IMMEDIATE, 0.0).unwrap();
    assert!(HUP_OBSERVED.load(Relaxed));
    miscinit::SetMyBackendType(prev_type);
    procsignal::PreIdentitySignalRelease();
}
