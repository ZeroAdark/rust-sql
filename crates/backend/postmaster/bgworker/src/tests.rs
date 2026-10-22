use std::sync::{Mutex, MutexGuard, Once};

use super::*;

static TEST_LOCK: Mutex<()> = Mutex::new(());
static POOL_PID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// Server-log capture: (sqlstate, message, detail) of every report that
/// reaches the emit-log hook on this thread.
static LOGS: Mutex<Vec<(types_error::SqlState, String, Option<String>)>> = Mutex::new(Vec::new());

fn capture_log(e: &PgError, _output_to_server: &mut bool) {
    LOGS.lock().unwrap_or_else(|e| e.into_inner()).push((
        e.sqlstate(),
        e.message().to_string(),
        e.detail().map(str::to_string),
    ));
}

/// The process title the worker asked for (init_ps_display seam capture).
static PS_TITLE: Mutex<Option<String>> = Mutex::new(None);

fn capture_ps_title(fixed_part: Option<&str>) {
    *PS_TITLE.lock().unwrap_or_else(|e| e.into_inner()) = fixed_part.map(str::to_string);
}

fn bringup() -> MutexGuard<'static, ()> {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        postmaster_seams::signal_postmaster_sigusr1::set(|| {});
        ps_status_seams::init_ps_display::set(capture_ps_title);
        postmaster_seams::parallel_pool_dispatch::set(|_, _, _| {
            POOL_PID.load(std::sync::atomic::Ordering::Relaxed)
        });
    });
    let guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    g::set_max_worker_processes(4);
    g::set_max_parallel_workers(2);
    g::SetIsUnderPostmaster(true);
    g::SetIsPostmasterEnvironment(true);
    g::SetMaxBackends(16);
    pmsignal::PMSignalShmemInit(8).unwrap();
    procsignal::ProcSignalShmemInit();
    *REGISTRY.lock().unwrap_or_else(|e| e.into_inner()) = None;
    // A fresh postmaster has no pending static registrations. The pending
    // list is process-global and only drained by BackgroundWorkerShmemInit,
    // so a sibling that registered static workers without running shmem
    // init (the over-limit LOG tests) would otherwise leak them into this
    // test's slots and `registered` entries (scheduling-dependent).
    STATIC_PENDING.lock().unwrap_or_else(|e| e.into_inner()).clear();
    BackgroundWorkerShmemInit().expect("shmem init");
    guard
}

/// Postmaster-side static registration state: no registry yet, empty
/// pending list, IsUnderPostmaster false (bgworker.c:949 admits us).
fn static_registration_setup() {
    *REGISTRY.lock().unwrap_or_else(|e| e.into_inner()) = None;
    STATIC_PENDING.lock().unwrap_or_else(|e| e.into_inner()).clear();
    g::SetIsUnderPostmaster(false);
    g::SetIsPostmasterEnvironment(true);
}

fn pending_names() -> Vec<String> {
    STATIC_PENDING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|w| w.bgw_name.clone())
        .collect()
}

fn with_log_capture<R>(f: impl FnOnce() -> R) -> (R, Vec<(types_error::SqlState, String, Option<String>)>) {
    LOGS.lock().unwrap_or_else(|e| e.into_inner()).clear();
    let prior = elog::sink::set_emit_log_hook(Some(capture_log));
    let r = f();
    elog::sink::set_emit_log_hook(prior);
    let logs = LOGS.lock().unwrap_or_else(|e| e.into_inner()).clone();
    (r, logs)
}

fn wmain(_arg: u64) -> PgResult<()> {
    Ok(())
}

fn mk_worker(name: &str, flags: i32) -> BackgroundWorker {
    BackgroundWorker {
        bgw_name: name.to_string(),
        bgw_type: String::new(),
        bgw_flags: flags,
        bgw_start_time: BgWorkerStartTime::ConsistentState,
        bgw_restart_time: BGW_NEVER_RESTART,
        bgw_main: wmain,
        bgw_main_arg: 0,
        bgw_extra: [0; BGW_EXTRALEN],
        bgw_notify_pid: 0,
    }
}

fn register(name: &str, flags: i32) -> Option<BackgroundWorkerHandle> {
    RegisterDynamicBackgroundWorker(mk_worker(name, flags)).expect("register")
}

fn registered_idx_for(handle: &BackgroundWorkerHandle) -> usize {
    with_registry(|reg| find_rw_by_slot(reg, handle.slot)).expect("registered")
}

#[test]
fn register_to_exhaustion_of_max_worker_processes() {
    let _g = bringup();
    let mut handles = Vec::new();
    for i in 0..4 {
        let h = register(&format!("w{i}"), BGWORKER_SHMEM_ACCESS).expect("free slot");
        handles.push(h);
    }
    assert!(register("w4", BGWORKER_SHMEM_ACCESS).is_none());

    BackgroundWorkerStateChange(true);
    let idx = registered_idx_for(&handles[0]);
    ForgetBackgroundWorker(idx);
    let h = register("w5", BGWORKER_SHMEM_ACCESS).expect("freed slot reusable");
    assert_eq!(h.slot, handles[0].slot);
    assert_eq!(h.generation, handles[0].generation + 1);
}

#[test]
fn parallel_class_capped_by_max_parallel_workers() {
    let _g = bringup();
    let flags = BGWORKER_SHMEM_ACCESS | BGWORKER_CLASS_PARALLEL;
    let p0 = register("p0", flags).expect("under cap");
    let _p1 = register("p1", flags).expect("under cap");
    assert!(register("p2", flags).is_none());
    assert!(register("np", BGWORKER_SHMEM_ACCESS).is_some());

    BackgroundWorkerStateChange(true);
    let idx = registered_idx_for(&p0);
    ForgetBackgroundWorker(idx);
    with_registry(|reg| {
        assert_eq!(reg.parallel_register_count, 2);
        assert_eq!(reg.parallel_terminate_count, 1);
    });
    assert!(register("p3", flags).is_some());
}

#[test]
fn parallel_admit_shares_the_max_parallel_workers_counter() {
    let _g = bringup();
    let flags = BGWORKER_SHMEM_ACCESS | BGWORKER_CLASS_PARALLEL;
    let _p0 = register("p0", flags).expect("under cap");
    assert_eq!(parallel_admit(2), 1);
    assert_eq!(parallel_admit(1), 0);
    assert!(register("p1", flags).is_none());
    parallel_release(1);
    assert!(register("p1", flags).is_some());
    assert_eq!(parallel_admit(3), 0);
    with_registry(|reg| {
        assert_eq!(reg.parallel_register_count, 3);
        assert_eq!(reg.parallel_terminate_count, 1);
    });
}

#[test]
fn handle_status_transitions() {
    let _g = bringup();
    let h = register("t0", BGWORKER_SHMEM_ACCESS).expect("slot");
    assert_eq!(GetBackgroundWorkerPid(&h), (BgwHandleStatus::BGWH_NOT_YET_STARTED, 0));

    BackgroundWorkerStateChange(true);
    let idx = registered_idx_for(&h);
    assert_eq!(rw_type(idx), "t0"); // bgw_type defaulted from bgw_name

    set_rw_pid(idx, 1234);
    ReportBackgroundWorkerPID(idx);
    assert_eq!(GetBackgroundWorkerPid(&h), (BgwHandleStatus::BGWH_STARTED, 1234));
    assert_eq!(GetBackgroundWorkerTypeByPid(1234).as_deref(), Some("t0"));

    set_rw_pid(idx, 0);
    ReportBackgroundWorkerExit(idx);
    assert_eq!(GetBackgroundWorkerPid(&h), (BgwHandleStatus::BGWH_STOPPED, 0));
    with_registry(|reg| {
        assert!(!reg.slots[h.slot as usize].in_use);
        assert!(reg.registered.iter().all(|rw| rw.is_none()));
    });

    let stale = BackgroundWorkerHandle { slot: h.slot, generation: h.generation + 7 };
    assert_eq!(GetBackgroundWorkerPid(&stale), (BgwHandleStatus::BGWH_STOPPED, 0));
}

#[test]
fn terminate_before_start_frees_slot_and_counts_parallel() {
    let _g = bringup();
    let flags = BGWORKER_SHMEM_ACCESS | BGWORKER_CLASS_PARALLEL;
    let h = register("pt", flags).expect("slot");
    TerminateBackgroundWorker(&h);

    BackgroundWorkerStateChange(true);
    with_registry(|reg| {
        assert!(!reg.slots[h.slot as usize].in_use);
        assert_eq!(reg.parallel_register_count, 1);
        assert_eq!(reg.parallel_terminate_count, 1);
        assert!(reg.registered.iter().all(|rw| rw.is_none()));
    });
    assert_eq!(GetBackgroundWorkerPid(&h), (BgwHandleStatus::BGWH_STOPPED, 0));
}

#[test]
fn state_change_disallowing_new_workers_terminates_pending() {
    let _g = bringup();
    let h = register("dn", BGWORKER_SHMEM_ACCESS).expect("slot");
    BackgroundWorkerStateChange(false);
    with_registry(|reg| assert!(!reg.slots[h.slot as usize].in_use));
    assert_eq!(GetBackgroundWorkerPid(&h), (BgwHandleStatus::BGWH_STOPPED, 0));
}

#[test]
fn running_terminated_worker_marks_rw_terminate() {
    let _g = bringup();
    let h = register("rt", BGWORKER_SHMEM_ACCESS).expect("slot");
    BackgroundWorkerStateChange(true);
    let idx = registered_idx_for(&h);
    set_rw_pid(idx, 4321);
    ReportBackgroundWorkerPID(idx);

    TerminateBackgroundWorker(&h);
    BackgroundWorkerStateChange(true);
    assert!(rw_terminate(idx));

    set_rw_pid(idx, 0);
    ReportBackgroundWorkerExit(idx);
    assert_eq!(GetBackgroundWorkerPid(&h), (BgwHandleStatus::BGWH_STOPPED, 0));
}

#[test]
fn forget_unstarted_only_zaps_waited_on_workers() {
    let _g = bringup();
    let mut w = mk_worker("fu", BGWORKER_SHMEM_ACCESS);
    w.bgw_notify_pid = 7777;
    let h = RegisterDynamicBackgroundWorker(w).expect("register").expect("slot");
    let h2 = register("fu2", BGWORKER_SHMEM_ACCESS).expect("slot");

    // notify-pid validation needs the pmchild seam; simulate the postmaster
    // knowing pid 7777 by patching after registration instead.
    with_registry(|reg| {
        reg.slots[h.slot as usize].worker.as_mut().unwrap().bgw_notify_pid = 0;
    });
    BackgroundWorkerStateChange(true);
    let idx = registered_idx_for(&h);
    with_registry(|reg| rw_mut(reg, idx).worker.bgw_notify_pid = 7777);

    ForgetUnstartedBackgroundWorkers();
    assert_eq!(GetBackgroundWorkerPid(&h), (BgwHandleStatus::BGWH_STOPPED, 0));
    assert_eq!(GetBackgroundWorkerPid(&h2), (BgwHandleStatus::BGWH_NOT_YET_STARTED, 0));
}

#[test]
fn shmem_reinit_zeroes_parallel_counts() {
    let _g = bringup();
    let flags = BGWORKER_SHMEM_ACCESS | BGWORKER_CLASS_PARALLEL;
    let _ = register("z0", flags).expect("slot");
    with_registry(|reg| assert_eq!(reg.parallel_register_count, 1));
    ResetBackgroundWorkerCrashTimes();
    BackgroundWorkerShmemInit().expect("shmem init");
    with_registry(|reg| {
        assert_eq!(reg.parallel_register_count, 0);
        assert_eq!(reg.parallel_terminate_count, 0);
        assert!(reg.slots.iter().all(|s| !s.in_use));
    });
}

#[test]
fn name_and_type_truncate_at_bgw_maxlen() {
    let _g = bringup();
    let long = "x".repeat(200);
    let h = register(&long, BGWORKER_SHMEM_ACCESS).expect("slot");
    with_registry(|reg| {
        let w = reg.slots[h.slot as usize].worker.as_ref().unwrap();
        assert_eq!(w.bgw_name.len(), BGW_MAXLEN - 1);
    });
}

#[test]
fn get_background_worker_type_by_pid() {
    let _g = bringup();
    let mut w = mk_worker("typed", BGWORKER_SHMEM_ACCESS);
    w.bgw_type = "test worker".to_string();
    let h = RegisterDynamicBackgroundWorker(w).expect("register").expect("slot");
    BackgroundWorkerStateChange(true);
    let idx = registered_idx_for(&h);
    with_registry(|reg| rw_mut(reg, idx).pid = 4242);
    ReportBackgroundWorkerPID(idx);
    assert_eq!(GetBackgroundWorkerTypeByPid(4242).as_deref(), Some("test worker"));
    assert_eq!(GetBackgroundWorkerTypeByPid(4243), None);
}

#[test]
fn static_registration_takes_slot_at_shmem_init() {
    let _g = bringup();
    // Static registration happens in the postmaster, before shmem init.
    static_registration_setup();
    let mut w = mk_worker("static launcher", BGWORKER_SHMEM_ACCESS);
    w.bgw_restart_time = 5;
    RegisterBackgroundWorker(&w).expect("register");
    // Too many: silently rejected at LOG (max_worker_processes = 4).
    for i in 0..5 {
        RegisterBackgroundWorker(&mk_worker(&format!("s{i}"), BGWORKER_SHMEM_ACCESS))
            .expect("register");
    }
    BackgroundWorkerShmemInit().expect("shmem init");
    g::SetIsUnderPostmaster(true);

    with_registry(|reg| {
        let n_used = reg.slots.iter().filter(|s| s.in_use).count();
        assert_eq!(n_used, 4, "4 slots (cap), 5th static registration rejected");
        let s0 = &reg.slots[0];
        assert_eq!(s0.worker.as_ref().unwrap().bgw_name, "static launcher");
        assert_eq!(s0.pid, InvalidPid);
        let idx = find_rw_by_slot(reg, 0).expect("registered entry");
        assert_eq!(rw_ref(reg, idx).worker.bgw_restart_time, 5);
    });

    // Registration after shmem init in a backend is refused at LOG, not panic.
    RegisterBackgroundWorker(&mk_worker("late", BGWORKER_SHMEM_ACCESS)).expect("register");
    with_registry(|reg| {
        assert_eq!(reg.slots.iter().filter(|s| s.in_use).count(), 4);
    });
}

// bgworker.c:1046-1060: a non-parallel dynamic worker may ask for a restart
// interval; the postmaster relaunches it after a crash (maybe_start_bgworkers)
// instead of forgetting it, so its registered entry survives the exit report.
#[test]
fn dynamic_worker_with_restart_interval_registers_and_survives_crash() {
    let _g = bringup();
    let mut w = mk_worker("restarting", BGWORKER_SHMEM_ACCESS);
    w.bgw_restart_time = 60;
    let h = RegisterDynamicBackgroundWorker(w).expect("register").expect("slot");
    assert_eq!(GetBackgroundWorkerPid(&h), (BgwHandleStatus::BGWH_NOT_YET_STARTED, 0));

    BackgroundWorkerStateChange(true);
    let idx = registered_idx_for(&h);
    assert_eq!(rw_restart_time(idx), 60);
    set_rw_pid(idx, 5150);
    ReportBackgroundWorkerPID(idx);
    assert_eq!(GetBackgroundWorkerPid(&h), (BgwHandleStatus::BGWH_STARTED, 5150));

    // Crash exit (CleanupBackend: rw_pid = 0, crashed_at set, not terminated).
    set_rw_pid(idx, 0);
    set_rw_crashed_at(idx, 1);
    ReportBackgroundWorkerExit(idx);
    assert_eq!(GetBackgroundWorkerPid(&h), (BgwHandleStatus::BGWH_STOPPED, 0));
    with_registry(|reg| {
        assert!(reg.slots[h.slot as usize].in_use, "restartable worker keeps its slot");
        assert_eq!(find_rw_by_slot(reg, h.slot), Some(idx), "registered entry survives for the restart");
    });
    assert_eq!(rw_crashed_at(idx), 1);
}

// bgworker.c:976: registering a static worker after BackgroundWorkerShmemInit
// is elog(ERROR) (XX000) -- an error value, never a panic.
#[test]
fn static_registration_after_shmem_init_is_an_error_not_a_panic() {
    let _g = bringup();
    g::SetIsUnderPostmaster(false);
    let e = RegisterBackgroundWorker(&mk_worker("late static", BGWORKER_SHMEM_ACCESS))
        .expect_err("registration after shmem init must fail");
    assert_eq!(e.level(), ERROR);
    assert_eq!(e.sqlstate(), types_error::ERRCODE_INTERNAL_ERROR);
    assert_eq!(e.message(), "cannot register background worker \"late static\" after shmem init");
}

// bgworker.c:1003-1008: the over-limit LOG carries ERRCODE_CONFIGURATION_LIMIT_
// EXCEEDED and errdetail_plural -- singular "worker" at max_worker_processes = 1.
#[test]
fn too_many_static_workers_log_is_singular_at_one_with_c_sqlstate() {
    let _g = bringup();
    static_registration_setup();
    g::set_max_worker_processes(1);
    let (_, logs) = with_log_capture(|| {
        RegisterBackgroundWorker(&mk_worker("first", BGWORKER_SHMEM_ACCESS)).expect("register");
        RegisterBackgroundWorker(&mk_worker("second", BGWORKER_SHMEM_ACCESS)).expect("register");
    });
    assert_eq!(pending_names(), vec!["first".to_string()]);
    let over = logs
        .iter()
        .find(|(_, m, _)| m == "too many background workers")
        .unwrap_or_else(|| panic!("no over-limit LOG: {logs:?}"));
    assert_eq!(over.0, types_error::ERRCODE_CONFIGURATION_LIMIT_EXCEEDED);
    assert_eq!(
        over.2.as_deref(),
        Some("Up to 1 background worker can be registered with the current settings.")
    );

    g::set_max_worker_processes(2);
    let (_, logs) = with_log_capture(|| {
        RegisterBackgroundWorker(&mk_worker("second", BGWORKER_SHMEM_ACCESS)).expect("register");
        RegisterBackgroundWorker(&mk_worker("third", BGWORKER_SHMEM_ACCESS)).expect("register");
    });
    let over = logs
        .iter()
        .find(|(_, m, _)| m == "too many background workers")
        .unwrap_or_else(|| panic!("no over-limit LOG: {logs:?}"));
    assert_eq!(
        over.2.as_deref(),
        Some("Up to 2 background workers can be registered with the current settings.")
    );
}

// bgworker.c:949-969: outside the postmaster environment (single-user mode)
// the registration is refused at LOG with ERRCODE_FEATURE_NOT_SUPPORTED --
// silently while shared_preload_libraries is being processed; the notify-pid
// refusal (bgworker.c:988) carries the same SQLSTATE.
#[test]
fn static_registration_outside_postmaster_environment_is_refused_at_log() {
    let _g = bringup();
    static_registration_setup();
    g::SetIsPostmasterEnvironment(false);

    let (_, logs) = with_log_capture(|| {
        RegisterBackgroundWorker(&mk_worker("single-user", BGWORKER_SHMEM_ACCESS)).expect("register");
    });
    assert!(pending_names().is_empty(), "registered outside the postmaster: {:?}", pending_names());
    let refused = logs
        .iter()
        .find(|(_, m, _)| m.contains("must be registered in \"shared_preload_libraries\""))
        .unwrap_or_else(|| panic!("no refusal LOG: {logs:?}"));
    assert_eq!(refused.0, types_error::ERRCODE_FEATURE_NOT_SUPPORTED);
    assert_eq!(
        refused.1,
        "background worker \"single-user\": must be registered in \"shared_preload_libraries\""
    );

    miscinit::set_process_shared_preload_libraries_in_progress(true);
    let (_, logs) = with_log_capture(|| {
        RegisterBackgroundWorker(&mk_worker("preloading", BGWORKER_SHMEM_ACCESS)).expect("register");
    });
    miscinit::set_process_shared_preload_libraries_in_progress(false);
    assert!(pending_names().is_empty());
    assert!(logs.is_empty(), "shared_preload_libraries-time registration must be silent: {logs:?}");

    g::SetIsPostmasterEnvironment(true);
    let mut w = mk_worker("notifier", BGWORKER_SHMEM_ACCESS);
    w.bgw_notify_pid = 42;
    let (_, logs) = with_log_capture(|| {
        RegisterBackgroundWorker(&w).expect("register");
    });
    assert!(pending_names().is_empty());
    let refused = logs
        .iter()
        .find(|(_, m, _)| m.contains("only dynamic background workers can request notification"))
        .unwrap_or_else(|| panic!("no notify refusal LOG: {logs:?}"));
    assert_eq!(refused.0, types_error::ERRCODE_FEATURE_NOT_SUPPORTED);
}

// bgworker.c:742: the worker's process title is its bgw_name.
#[test]
fn worker_identity_sets_process_title_to_bgw_name() {
    let _g = bringup();
    *PS_TITLE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    let w = mk_worker("titled worker", BGWORKER_SHMEM_ACCESS);
    adopt_worker_identity(&w);
    assert_eq!(miscinit::GetMyBackendType(), BackendType::BgWorker);
    assert_eq!(MyBgworkerEntry().map(|w| w.bgw_name), Some("titled worker".to_string()));
    assert_eq!(
        PS_TITLE.lock().unwrap_or_else(|e| e.into_inner()).as_deref(),
        Some("titled worker")
    );
}

// bgworker.c:1032 / :455: BackgroundWorkerList is head-inserted, so the
// postmaster walks registrations newest-first (static and dynamic alike).
#[test]
fn registrations_are_traversed_newest_first() {
    let _g = bringup();
    static_registration_setup();
    RegisterBackgroundWorker(&mk_worker("first static", BGWORKER_SHMEM_ACCESS)).unwrap();
    RegisterBackgroundWorker(&mk_worker("second static", BGWORKER_SHMEM_ACCESS)).unwrap();
    g::SetIsUnderPostmaster(true);
    BackgroundWorkerShmemInit().expect("shmem init");
    let names = |idxs: Vec<usize>| -> Vec<String> {
        with_registry(|reg| idxs.iter().map(|&i| rw_ref(reg, i).worker.bgw_name.clone()).collect())
    };
    assert_eq!(names(registered_indexes()), vec!["second static", "first static"]);
    register("dynamic", BGWORKER_SHMEM_ACCESS).expect("slot");
    BackgroundWorkerStateChange(true);
    assert_eq!(names(registered_indexes()), vec!["dynamic", "second static", "first static"]);
}

/// bgworker.c:982: a static worker failing SanityCheckBackgroundWorker is
/// refused at LOG and keeps ERRCODE_INVALID_PARAMETER_VALUE.
#[test]
fn static_sanity_failure_logs_with_invalid_parameter_value() {
    let _g = bringup();
    static_registration_setup();
    let w = mk_worker("no shmem", 0);
    let (_, logs) = with_log_capture(|| RegisterBackgroundWorker(&w).expect("register"));
    let hit = logs
        .iter()
        .find(|(_, m, _)| m.ends_with("background workers without shared memory access are not supported"))
        .unwrap_or_else(|| panic!("no sanity LOG in {logs:?}"));
    assert_eq!(hit.0, types_error::ERRCODE_INVALID_PARAMETER_VALUE);
    assert!(pending_names().is_empty());
}

#[test]
fn pooled_worker_startup_logs_use_postmaster_scope() {
    let _g = bringup();
    let prior_level = elog::config::log_min_messages();
    elog::config::set_log_min_messages(DEBUG1);
    POOL_PID.store(4242, std::sync::atomic::Ordering::Relaxed);
    let (handle, logs) = with_log_capture(|| {
        register("pooled startup", BGWORKER_SHMEM_ACCESS | BGWORKER_CLASS_PARALLEL).unwrap()
    });
    POOL_PID.store(0, std::sync::atomic::Ordering::Relaxed);
    assert!(logs.is_empty(), "leader must not log postmaster diagnostics: {logs:?}");
    elog::config::set_log_min_messages(types_error::WARNING);
    let (_, logs) = with_log_capture(|| BackgroundWorkerStateChange(true));
    assert!(logs.is_empty(), "postmaster threshold must suppress DEBUG1: {logs:?}");
    elog::config::set_log_min_messages(DEBUG1);
    let (_, logs) = with_log_capture(|| BackgroundWorkerStateChange(true));
    assert!(logs.is_empty(), "suppressed reports must not be replayed");
    ForgetBackgroundWorker(registered_idx_for(&handle));

    POOL_PID.store(4242, std::sync::atomic::Ordering::Relaxed);
    let handle = register("pooled startup", BGWORKER_SHMEM_ACCESS | BGWORKER_CLASS_PARALLEL).unwrap();
    POOL_PID.store(0, std::sync::atomic::Ordering::Relaxed);
    let idx = registered_idx_for(&handle);
    let (_, logs) = with_log_capture(|| {
        ReportPendingBackgroundWorkerStartup(idx);
        BackgroundWorkerStateChange(true);
        ReportPendingBackgroundWorkerStartup(idx);
    });
    assert_eq!(logs.iter().map(|l| l.1.as_str()).collect::<Vec<_>>(), vec![
        "registering background worker \"pooled startup\"",
        "starting background worker process \"pooled startup\"",
    ]);
    ForgetBackgroundWorker(idx);
    elog::config::set_log_min_messages(prior_level);
}

#[test]
fn parallel_admit_holds_worker_slots() {
    let _g = bringup();
    g::set_max_parallel_workers(8);
    let plain = BGWORKER_SHMEM_ACCESS;
    let _a = register("a", plain).expect("slot");
    let _b = register("b", plain).expect("slot");
    assert_eq!(parallel_admit(10), 2);
    assert!(register("c", plain).is_none());
    parallel_release(1);
    assert!(register("c", plain).is_some());
    assert_eq!(parallel_admit(1), 0);
    parallel_release(1);
    assert_eq!(parallel_admit(3), 1);
}
