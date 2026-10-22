//! Catchable-class crash choreography end to end over REAL shared state: full
//! ipci bringup → a fake backend crashes → HandleChildCrash SIGQUITs the
//! sibling (observed quickdie-style) → the reaper drains → the reinit arm runs
//! shmem_exit(1) + the full reset walk and re-arms PM_STARTUP with a live
//! startup child (notes/crash-restart-design.md).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;

use postmaster::{with_pm, PMState, StartupStatusEnum};
use types_core::init::BackendType;

static SIGQUIT_SEEN: AtomicBool = AtomicBool::new(false);
static QUIT_REASON_SEEN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(u32::MAX);

// quickdie's observation point: the disposition reads the postmaster's reason.
fn observe_sigquit() {
    QUIT_REASON_SEEN.store(pmsignal::GetQuitSignalReason() as u32, Ordering::SeqCst);
    SIGQUIT_SEEN.store(true, Ordering::SeqCst);
}

fn write_valid_control_file(dir: &str) {
    std::fs::create_dir_all(format!("{dir}/global")).unwrap();
    // update_controlfile opens without O_CREAT (C parity).
    std::fs::write(format!("{dir}/global/pg_control"), []).unwrap();
    let mut cf = controldata_utils::ControlFileData::ZEROED;
    cf.pg_control_version = controldata_utils::PG_CONTROL_VERSION;
    cf.catalog_version_no = controldata_utils::CATALOG_VERSION_NO;
    cf.maxAlign = 8;
    cf.floatFormat = 1234567.0;
    cf.blcksz = types_core::BLCKSZ as u32;
    cf.relseg_size = types_storage::smgr::RELSEG_SIZE;
    cf.xlog_blcksz = transam_xlog::XLOG_BLCKSZ as u32;
    cf.xlog_seg_size = 16 * 1024 * 1024;
    cf.nameDataLen = types_core::NAMEDATALEN as u32;
    cf.indexMaxKeys = types_core::INDEX_MAX_KEYS as u32;
    cf.toast_max_chunk_size = 1996;
    cf.loblksize = types_storage::large_object::LOBLKSIZE as u32;
    cf.float8ByVal = true;
    controldata_utils::update_controlfile(dir, &mut cf, false).unwrap();
}

const VICTIM_PID: i32 = 9001;
const SIBLING_PID: i32 = 9002;

fn data_dir_of(pid: u32) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("pgrust-crash-restart-{pid}"))
}

/// The scratch data directory is the process's (SetDataDir), and the
/// postmaster's threads may still use it after the test returns, so it goes
/// at the process's exit.
fn remove_data_dir_at_exit() {
    extern "C" fn gone() {
        let _ = std::fs::remove_dir_all(data_dir_of(std::process::id()));
    }
    extern "C" {
        fn atexit(f: extern "C" fn()) -> core::ffi::c_int;
    }
    // SAFETY: the C library's atexit, given a function that cannot unwind
    // (it ignores the removal's error).
    unsafe { atexit(gone) };
}

/// The test, run alone in a child process, leaves no data directory behind
/// once the child exits.
#[test]
fn a_run_leaves_nothing_behind() {
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "crash_fans_out_sigquit_and_reinit_completes", "--nocapture"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let dir = data_dir_of(child.id());
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "child failed: {stdout}");
    assert!(
        stdout.contains(&format!("data dir {}", dir.display())),
        "the child never made its data dir: {stdout}"
    );
    assert!(!dir.exists(), "{} left behind", dir.display());
}

#[test]
fn crash_fans_out_sigquit_and_reinit_completes() {
    guc_tables::init_seams();
    init_small::init_seams();
    transam_xlog::init_seams();
    shmem::init_seams();
    ipc::init_seams();
    ipci::init_seams();
    pmchild::init_seams();
    postmaster::init_seams();
    pgstat::init_seams();
    pg_prng::init_seams();
    s_lock_seams::perform_spin_delay::set(|_| std::thread::yield_now());
    s_lock_seams::finish_spin_delay::set(|_| {});
    pg_sema_seams::pg_semaphore_create::set(|_| {});
    xact_seams::is_in_parallel_mode::set(|| false);
    xact_seams::get_current_transaction_nest_level::set(|| 1);
    scalar_seams::parse_bool::set(|value| match value {
        "on" | "true" | "yes" | "1" => Some(true),
        "off" | "false" | "no" | "0" => Some(false),
        _ => None,
    });
    aclchk_seams::pg_parameter_aclcheck_set::set(|_, _| Ok(true));
    mbutils_seams::get_database_encoding::set(|| 6);
    // AsyncShmemInit scans pg_notify/ during reinit; no datadir in this test.
    file_seams::with_allocated_dir::set(|dirname, cb| {
        let mut ret = false;
        let Ok(entries) = std::fs::read_dir(dirname) else { return Ok(false) };
        for entry in entries {
            ret = cb(entry.unwrap().file_name().to_str().unwrap())?;
            if ret {
                break;
            }
        }
        Ok(ret)
    });
    backend_status_seams::backend_status_shmem_size::set(|| Ok(4096));
    backend_status_seams::backend_status_shmem_init::set(|| Ok(()));
    static BE_STATUS_RESETS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);
    backend_status_seams::backend_status_shmem_reset_after_crash::set(|| {
        BE_STATUS_RESETS.fetch_add(1, Ordering::Relaxed);
    });
    {
        use std::sync::atomic::AtomicI32;
        use std::sync::atomic::Ordering::Relaxed;
        static AV_SLOTS: AtomicI32 = AtomicI32::new(16);
        static WAL_SENDERS: AtomicI32 = AtomicI32::new(10);
        static MAX_PREPARED: AtomicI32 = AtomicI32::new(0);
        static MAX_LOCKS: AtomicI32 = AtomicI32::new(64);
        guc_tables::vars::autovacuum_worker_slots.install(guc_tables::GucVarAccessors {
            get: || AV_SLOTS.load(Relaxed),
            set: |v| AV_SLOTS.store(v, Relaxed),
        });
        guc_tables::vars::max_wal_senders.install(guc_tables::GucVarAccessors {
            get: || WAL_SENDERS.load(Relaxed),
            set: |v| WAL_SENDERS.store(v, Relaxed),
        });
        guc_tables::vars::max_prepared_xacts.install(guc_tables::GucVarAccessors {
            get: || MAX_PREPARED.load(Relaxed),
            set: |v| MAX_PREPARED.store(v, Relaxed),
        });
        guc_tables::vars::max_locks_per_xact.install(guc_tables::GucVarAccessors {
            get: || MAX_LOCKS.load(Relaxed),
            set: |v| MAX_LOCKS.store(v, Relaxed),
        });
    }
    aio_core::init_seams();
    // commands_variable owns this accessor in production seams_init; this
    // harness inits GUCs piecemeal (AioShmemSize reads it).
    guc_tables::vars::io_max_combine_limit.install_if_absent(guc_tables::GucVarAccessors {
        get: || 16,
        set: |_| {},
    });

    guc::store::initialize_guc_options().unwrap();
    pg_prng::global_prng(|prng| prng.seed(42));

    init_small::globals::SetIsPostmasterEnvironment(true);
    init_small::globals::SetMaxConnections(10);
    init_small::globals::set_max_worker_processes(8);
    init_small::globals::SetNBuffers(16);
    // MaxConnections + av_slots(16) + workers + wal_senders(10) + specials.
    init_small::globals::SetMaxBackends(
        10 + 16 + 8 + 10 + types_storage::storage::NUM_SPECIAL_WORKER_PROCS,
    );
    pmchild_seams::init_postmaster_child_slots::call();
    bgworker::BackgroundWorkerShmemInit().expect("bgworker shmem init");

    let dir = data_dir_of(std::process::id());
    let _ = std::fs::remove_dir_all(&dir);
    remove_data_dir_at_exit();
    println!("data dir {}", dir.display());
    let dir: &'static str = Box::leak(dir.to_str().unwrap().to_string().into_boxed_str());
    write_valid_control_file(dir);
    init_small::globals::SetDataDir(dir);

    ipci_seams::create_shared_memory_and_semaphores::call(1).unwrap();

    // Regression pin (CI cluster crash-smoke job -47a8): per-child GUC stamping can
    // rewrite io_max_concurrency to its -1 boot value after the boot auto-tune;
    // the AIO crash-reset arm must derive geometry from stored counts, not the
    // live GUC.
    guc_tables::vars::io_max_concurrency.write(-1);

    guc_tables::vars::remove_temp_files_after_crash.write(false);

    waiteventset::InitializeWaitEventSupport().unwrap();
    miscinit::InitProcessLocalLatch().expect("local latch");

    let victim_slot =
        pmchild_seams::assign_postmaster_child_slot::call(BackendType::Backend).unwrap();
    pmchild_seams::set_child_pid::call(victim_slot, VICTIM_PID);
    let sibling_slot =
        pmchild_seams::assign_postmaster_child_slot::call(BackendType::Backend).unwrap();
    pmchild_seams::set_child_pid::call(sibling_slot, SIBLING_PID);

    let (ready_tx, ready_rx) = channel();
    let (announce_tx, announce_rx) = channel::<()>();
    let sibling = std::thread::spawn(move || {
        init_small::globals::SetIsUnderPostmaster(true);
        init_small::globals::SetMyProcNumber(1);
        init_small::globals::SetMyProcPid(SIBLING_PID);
        procsignal::ProcSignalInit(&[]).unwrap();
        procsignal::pqsignal_thread(
            libc::SIGQUIT,
            procsignal::ThreadSignalHandler::Simple(observe_sigquit),
        );
        ready_tx.send(()).unwrap();
        while !SIGQUIT_SEEN.load(Ordering::SeqCst) {
            procsignal::DrainThreadSignals().unwrap();
            std::thread::yield_now();
        }
        // quickdie's thread rendering: exit code 2, announced to the reaper —
        // gated so the first reap below stays deterministic.
        announce_rx.recv().unwrap();
        postmaster_seams::announce_child_exit::call(SIBLING_PID, 2 << 8);
    });
    ready_rx.recv().unwrap();

    with_pm(|pm| {
        pm.pm_state = PMState::PM_RUN;
        pm.conns_allowed = true;
    });

    postmaster_seams::announce_child_exit::call(VICTIM_PID, libc::SIGABRT);
    postmaster::process_pm_child_exit().unwrap();

    assert!(with_pm(|pm| pm.fatal_error), "HandleFatalError must set fatal_error");
    assert_eq!(with_pm(|pm| pm.pm_state), PMState::PM_WAIT_BACKENDS);

    announce_tx.send(()).unwrap();
    sibling.join().unwrap();
    assert!(SIGQUIT_SEEN.load(Ordering::SeqCst), "sibling must observe SIGQUIT");
    assert_eq!(
        QUIT_REASON_SEEN.load(Ordering::SeqCst),
        pmsignal::QuitSignalReason::PMQUIT_FOR_CRASH as u32,
        "sibling must see PMQUIT_FOR_CRASH at its quickdie point"
    );

    varsup::TransamVariables().nextOid.store(777, Ordering::Relaxed);
    let lock0 = lwlock::main_lock(0);
    lock0
        .state
        .store(lwlock::LW_FLAG_RELEASE_OK | 5, Ordering::Relaxed);

    postmaster::process_pm_child_exit().unwrap();

    assert_eq!(
        varsup::TransamVariables().nextOid.load(Ordering::Relaxed),
        0,
        "VarsupShmemReset must restore the boot image"
    );
    assert_eq!(
        lock0.state.load(Ordering::Relaxed),
        lwlock::LW_FLAG_RELEASE_OK,
        "LWLockResetAfterCrash must re-arm locks"
    );
    assert_eq!(BE_STATUS_RESETS.load(Ordering::Relaxed), 1);

    assert_eq!(
        with_pm(|pm| pm.pm_state),
        PMState::PM_STARTUP,
        "reinit arm must re-enter PM_STARTUP"
    );
    assert!(with_pm(|pm| pm.startup.is_some()), "a fresh startup child must be launched");
    assert_eq!(with_pm(|pm| pm.startup_status), StartupStatusEnum::Running);
    assert_eq!(with_pm(|pm| pm.abort_start_time), 0);
    assert!(
        with_pm(|pm| pm.fatal_error),
        "fatal_error clears only at PMSIGNAL_RECOVERY_STARTED, as in C"
    );
    // The real startup thread recovers against the scratch datadir in the
    // background; its eventual exit stays queued and unreaped here.

    // D3049 (upstream 12f866cd5f): a fast shutdown during this early crash
    // restart (PM_STARTUP, fatal_error still set) must not wait forever on a
    // relaunched checkpointer, which ignores SIGTERM (C checkpointer.c
    // `pqsignal(SIGTERM, SIG_IGN)`, ours alike). Before the fix the
    // PM_STOP_BACKENDS arm sent it SIGTERM only and PM_WAIT_BACKENDS never
    // drained; now it gets SIGQUIT, and the abort timer is armed.
    let ckpt_slot =
        pmchild_seams::assign_postmaster_child_slot::call(BackendType::Checkpointer).unwrap();
    pmchild_seams::set_child_pid::call(ckpt_slot, CHECKPOINTER_PID);
    let (ckpt_ready_tx, ckpt_ready_rx) = channel();
    let checkpointer = std::thread::spawn(move || {
        init_small::globals::SetIsUnderPostmaster(true);
        init_small::globals::SetMyProcNumber(2);
        init_small::globals::SetMyProcPid(CHECKPOINTER_PID);
        procsignal::ProcSignalInit(&[]).unwrap();
        procsignal::pqsignal_thread(libc::SIGTERM, procsignal::ThreadSignalHandler::Ignore);
        procsignal::pqsignal_thread(
            libc::SIGQUIT,
            procsignal::ThreadSignalHandler::Simple(observe_checkpointer_sigquit),
        );
        ckpt_ready_tx.send(()).unwrap();
        let start = std::time::Instant::now();
        while !CKPT_SIGQUIT_SEEN.load(Ordering::SeqCst) {
            if start.elapsed() > std::time::Duration::from_secs(5) {
                return false;
            }
            procsignal::DrainThreadSignals().unwrap();
            std::thread::yield_now();
        }
        true
    });
    ckpt_ready_rx.recv().unwrap();

    postmaster::handle_pm_shutdown_request_signal(libc::SIGINT);
    postmaster::statemachine::process_pm_shutdown_request().unwrap();

    assert!(
        checkpointer.join().unwrap(),
        "a fast shutdown during an early crash restart must SIGQUIT the relaunched checkpointer within 5 s"
    );
    assert_eq!(with_pm(|pm| pm.pm_state), PMState::PM_WAIT_BACKENDS);
    assert!(with_pm(|pm| pm.abort_start_time) != 0, "the SIGKILL floor must be armed");
    assert_eq!(
        with_pm(|pm| pm.startup_status),
        StartupStatusEnum::Signaled,
        "the startup child's exit must not read as a startup failure"
    );
}

static CKPT_SIGQUIT_SEEN: AtomicBool = AtomicBool::new(false);
const CHECKPOINTER_PID: i32 = 9003;

fn observe_checkpointer_sigquit() {
    CKPT_SIGQUIT_SEEN.store(true, Ordering::SeqCst);
}
