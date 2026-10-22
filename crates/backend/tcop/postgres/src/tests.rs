use std::sync::atomic::Ordering;
use std::sync::Mutex;

use super::*;

use crate::session_tests::{LOCK_TIMEOUT_INDICATOR, STMT_TIMEOUT_INDICATOR};

// Serializes tests that reach the QueryCancel arm: the timeout-indicator
// stubs are process-global while the flags they mimic are per-backend.
static CANCEL_ARM: Mutex<()> = Mutex::new(());

fn install_test_seams() {
    crate::session_tests::install_shared_stubs();
}

fn my_latch() {
    use init_small::globals as g;
    if g::MyLatch().is_none() {
        let h = latch::allocate_local_latch();
        latch::InitLatch(h);
        g::SetMyLatch(Some(h));
    }
}

#[test]
fn xact_started_flag_roundtrip() {
    assert!(!xact_started());
    set_xact_started(true);
    assert!(xact_started());
    set_xact_started(false);
}

#[test]
fn is_transaction_exit_stmt_none_is_false() {
    assert!(!simple_query::IsTransactionExitStmt(None));
}

#[test]
fn process_interrupts_noop_when_nothing_pending() {
    init_small::globals::SetInterruptPending(false);
    assert!(check_for_interrupts().is_ok());
}

#[test]
fn process_interrupts_die_is_fatal() {
    install_test_seams();
    init_small::globals::SetInterruptPending(true);
    init_small::globals::SetProcDiePending(true);
    let err = check_for_interrupts().unwrap_err();
    assert_eq!(err.level(), types_error::FATAL);
    assert_eq!(err.sqlstate, types_error::ERRCODE_ADMIN_SHUTDOWN);
    assert!(!init_small::globals::ProcDiePending());
}

#[test]
fn process_interrupts_cancel_is_error_57014() {
    install_test_seams();
    let _serial = CANCEL_ARM.lock().unwrap();
    init_small::globals::SetInterruptPending(true);
    init_small::globals::SetQueryCancelPending(true);
    let err = check_for_interrupts().unwrap_err();
    assert_eq!(err.level(), types_error::ERROR);
    assert_eq!(err.sqlstate, types_error::ERRCODE_QUERY_CANCELED);
    assert!(err.message.contains("user request"));
}

#[test]
fn process_interrupts_lock_timeout_is_55p03() {
    install_test_seams();
    let _serial = CANCEL_ARM.lock().unwrap();
    LOCK_TIMEOUT_INDICATOR.store(true, Ordering::Relaxed);
    init_small::globals::SetInterruptPending(true);
    init_small::globals::SetQueryCancelPending(true);
    let err = check_for_interrupts().unwrap_err();
    assert_eq!(err.sqlstate, types_error::ERRCODE_LOCK_NOT_AVAILABLE);
    assert_eq!(err.message, "canceling statement due to lock timeout");
    let location = err.location().unwrap();
    assert_eq!(location.filename.as_deref(), Some("postgres.c"));
    assert_eq!(location.lineno, 3439);
    assert_eq!(location.funcname.as_deref(), Some("ProcessInterrupts"));
    assert!(!LOCK_TIMEOUT_INDICATOR.load(Ordering::Relaxed)); /* reset consumed it */
}

#[test]
fn process_interrupts_statement_timeout_is_57014() {
    install_test_seams();
    let _serial = CANCEL_ARM.lock().unwrap();
    STMT_TIMEOUT_INDICATOR.store(true, Ordering::Relaxed);
    init_small::globals::SetInterruptPending(true);
    init_small::globals::SetQueryCancelPending(true);
    let err = check_for_interrupts().unwrap_err();
    assert_eq!(err.sqlstate, types_error::ERRCODE_QUERY_CANCELED);
    assert!(err.message.contains("statement timeout"));
}

// D380: the thread whose next timeout drain delivers a late fire (session_tests' process_timeout_interrupt stub).
use crate::session_tests::LATE_FIRE;

// D380: a statement_timeout fire the drain observes while its sender is still storing: ProcessInterrupts clears the
// abort byte first, then drains; the fire's handler raises the cancel and the byte lands after the clear. The consume
// must take the byte with the cancel, or it outlives the statement and the next one's Gravity kernels read a spurious
// 57014 at their first window.
#[test]
fn process_interrupts_consumed_cancel_takes_a_late_abort_byte() {
    install_test_seams();
    let _serial = CANCEL_ARM.lock().unwrap();
    crate::session_tests::install_timeout_drain_stub();
    init_small::globals::SetAbortPending(false);
    *LATE_FIRE.lock().unwrap() = Some(std::thread::current().id());
    init_small::globals::SetInterruptPending(true);
    let err = check_for_interrupts().unwrap_err();
    *LATE_FIRE.lock().unwrap() = None;
    assert_eq!(err.sqlstate, types_error::ERRCODE_QUERY_CANCELED);
    let stale = init_small::globals::AbortPending();
    init_small::globals::SetAbortPending(false);
    assert!(!stale, "the consumed cancel's abort byte must not outlive it into the next statement");
    assert!(!init_small::globals::QueryCancelPending());
}

// timeout.c:414: the SIGALRM handler runs before ProcessInterrupts clears
// InterruptPending, never between the drain and the clear. The thread model
// clears first (D355), so a timer post landing after the drain re-raises
// InterruptPending and the next CHECK_FOR_INTERRUPTS services it.
#[test]
fn process_interrupts_timer_post_after_drain_is_serviced() {
    install_test_seams();
    let _serial = CANCEL_ARM.lock().unwrap();
    crate::session_tests::install_timeout_drain_stub();
    *crate::session_tests::TIMER_ARMED_THREAD.lock().unwrap() = Some(std::thread::current().id());
    crate::session_tests::TIMER_POST_ON_DRAIN.store(true, Ordering::SeqCst);
    init_small::globals::SetInterruptPending(true);
    let err = match check_for_interrupts() {
        Err(e) => e,
        Ok(()) => {
            assert!(init_small::globals::InterruptPending(), "the late post must re-raise InterruptPending");
            check_for_interrupts().unwrap_err()
        }
    };
    *crate::session_tests::TIMER_ARMED_THREAD.lock().unwrap() = None;
    assert_eq!(err.sqlstate, types_error::ERRCODE_QUERY_CANCELED);
    assert!(err.message.contains("statement timeout"));
    assert!(!crate::session_tests::TIMER_POSTED.load(Ordering::SeqCst));
    assert!(!init_small::globals::QueryCancelPending());
}

// postgres.c:5052: Save_r/Save_t are zero until the first ResetUsage, so a
// ShowUsage reached without one reports elapsed time since the epoch.
#[test]
fn show_usage_without_reset_subtracts_zero_baseline() {
    let report = usage_report();
    let line = report.lines().nth(1).expect("usage line");
    let elapsed = line
        .split("s system, ")
        .nth(1)
        .and_then(|s| s.split(" s elapsed").next())
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or_else(|| panic!("no elapsed in {line:?}"));
    assert!(elapsed > 1.0e9, "{line}");
}

// postgres.c:312: -E echoes through printf's %s, so an embedded NUL ends
// the echoed text.
#[test]
fn single_user_echo_writes_the_input_bytes_verbatim() {
    let mut out = Vec::new();
    crate::main_loop::write_echo(&mut out, b"select 1 as \"\xff\";\n\0").unwrap();
    assert_eq!(out, b"statement: select 1 as \"\xff\";\n\n");
}

#[test]
fn forbidden_in_wal_sender_reports_c_source_lines() {
    let line = |firstchar| {
        crate::main_loop::forbidden_in_wal_sender_error(firstchar)
            .unwrap_err()
            .location
            .as_ref()
            .map(|l| l.lineno)
    };
    assert_eq!(line(crate::main_loop::pqmsg::FUNCTION_CALL), Some(5043));
    assert_eq!(line(crate::main_loop::pqmsg::PARSE), Some(5047));
}

#[test]
fn single_user_startup_error_is_reported_as_fatal() {
    install_test_seams();
    static SEEN: Mutex<Vec<types_error::ErrorLevel>> = Mutex::new(Vec::new());
    fn capture(e: &types_error::PgError, _output_to_server: &mut bool) {
        SEEN.lock().unwrap().push(e.level);
    }
    let _serial = CANCEL_ARM.lock().unwrap();
    SEEN.lock().unwrap().clear();
    let saved = elog::config::where_to_send_output();
    elog::config::set_where_to_send_output(types_dest::CommandDest::None);
    let prev = elog::sink::set_emit_log_hook(Some(capture));
    let err: types_error::PgError = elog::ereport(types_error::ERROR)
        .errcode(types_error::ERRCODE_UNDEFINED_FILE)
        .errmsg("could not access file \"nope\": No such file or directory")
        .into_error();
    crate::single_user::report_startup_error(&err);
    elog::sink::set_emit_log_hook(prev);
    elog::config::set_where_to_send_output(saved);
    assert_eq!(*SEEN.lock().unwrap(), vec![types_error::FATAL]);
}

#[test]
fn single_user_echo_stops_at_embedded_nul() {
    assert_eq!(crate::main_loop::echoed_statement(b"select 1\n\0"), b"select 1\n");
    assert_eq!(crate::main_loop::echoed_statement(b"select 1\0 junk\n\0"), b"select 1");
}

#[test]
fn process_interrupts_held_off_is_deferred() {
    init_small::globals::HoldInterrupts();
    init_small::globals::SetInterruptPending(true);
    init_small::globals::SetQueryCancelPending(true);
    assert!(check_for_interrupts().is_ok());
    assert!(init_small::globals::QueryCancelPending());
    init_small::globals::SetQueryCancelPending(false);
    init_small::globals::SetInterruptPending(false);
    init_small::globals::ResumeInterrupts();
}

#[test]
fn process_interrupts_cancel_holdoff_rearms() {
    init_small::globals::HoldCancelInterrupts();
    init_small::globals::SetInterruptPending(true);
    init_small::globals::SetQueryCancelPending(true);
    assert!(check_for_interrupts().is_ok());
    assert!(init_small::globals::InterruptPending()); /* re-armed */
    assert!(init_small::globals::QueryCancelPending());
    init_small::globals::SetQueryCancelPending(false);
    init_small::globals::SetInterruptPending(false);
    init_small::globals::ResumeCancelInterrupts();
}

#[test]
fn recovery_conflict_arm_is_handled() {
    // PORTED (t25 car-10, recovery-t24-merge): ProcessRecoveryConflictInterrupts
    // is real — the arm SERVICES the conflict signal instead of panicking
    // loudly (the pre-port stub assertion this test used to pin). Outside
    // recovery/transaction state (this unit environment) C resolves the
    // conflict as a no-op: no panic, clean return, interrupt consumed.
    install_test_seams();
    HandleRecoveryConflictInterrupt(5);
    assert!(init_small::globals::InterruptPending());
    check_for_interrupts()
        .expect("ported recovery-conflict arm must service the interrupt cleanly");
    assert!(!init_small::globals::InterruptPending());
}

#[test]
fn recovery_conflict_snapshot_outside_transaction_is_noop() {
    use types_storage::storage::ProcSignalReason;
    install_test_seams();
    // Not in a transaction: C ignores LOCK/TABLESPACE/SNAPSHOT conflicts.
    HandleRecoveryConflictInterrupt(ProcSignalReason::PROCSIG_RECOVERY_CONFLICT_SNAPSHOT as u32);
    assert!(init_small::globals::InterruptPending());
    check_for_interrupts().expect("snapshot conflict outside a transaction is a no-op");
    assert!(!init_small::globals::InterruptPending());
    assert_eq!(RECOVERY_CONFLICT_PENDING_REASONS.with(std::cell::Cell::get), 0);
}

#[test]
fn recovery_conflict_database_is_fatal_57p04() {
    use types_storage::storage::ProcSignalReason;
    install_test_seams();
    HandleRecoveryConflictInterrupt(ProcSignalReason::PROCSIG_RECOVERY_CONFLICT_DATABASE as u32);
    let err = check_for_interrupts().unwrap_err();
    assert_eq!(err.level(), types_error::FATAL);
    assert_eq!(err.sqlstate, types_error::ERRCODE_DATABASE_DROPPED);
    assert_eq!(err.message, "terminating connection due to conflict with recovery");
    assert_eq!(
        err.detail.as_deref(),
        Some("User was connected to a database that must be dropped.")
    );
    assert_eq!(
        err.hint.as_deref(),
        Some(
            "In a moment you should be able to reconnect to the database and repeat your command."
        )
    );
    assert_eq!(RECOVERY_CONFLICT_PENDING_REASONS.with(std::cell::Cell::get), 0);
}

#[test]
fn recovery_conflict_logicalslot_is_error_40001_and_respects_holdoff() {
    use types_storage::storage::ProcSignalReason;
    install_test_seams();
    let reason = ProcSignalReason::PROCSIG_RECOVERY_CONFLICT_LOGICALSLOT as u32;

    // Mid-message read (QueryCancelHoldoffCount > 0): re-arm and defer.
    init_small::globals::HoldCancelInterrupts();
    HandleRecoveryConflictInterrupt(reason);
    check_for_interrupts().expect("conflict must defer under cancel holdoff");
    assert!(init_small::globals::InterruptPending()); /* re-armed */
    assert_ne!(RECOVERY_CONFLICT_PENDING_REASONS.with(std::cell::Cell::get) & (1 << reason), 0);
    init_small::globals::ResumeCancelInterrupts();

    // Holdoff released: the deferred conflict now cancels the statement.
    let err = check_for_interrupts().unwrap_err();
    assert_eq!(err.level(), types_error::ERROR);
    assert_eq!(err.sqlstate, types_error::ERRCODE_T_R_SERIALIZATION_FAILURE);
    assert_eq!(err.message, "canceling statement due to conflict with recovery");
    assert_eq!(
        err.detail.as_deref(),
        Some("User was using a logical replication slot that must be invalidated.")
    );
    assert_eq!(RECOVERY_CONFLICT_PENDING_REASONS.with(std::cell::Cell::get), 0);
    assert!(!init_small::globals::InterruptPending());
}

// postgres.c:3271: the RecoveryConflictPending gate drops before the walk,
// so a reason left behind by an earlier reason's ERROR is not revisited until
// HandleRecoveryConflictInterrupt raises the gate again.
#[test]
fn recovery_conflict_gate_ignores_reasons_left_by_an_error() {
    use types_storage::storage::ProcSignalReason::*;
    let lock = PROCSIG_RECOVERY_CONFLICT_LOCK as u32;
    let snapshot = PROCSIG_RECOVERY_CONFLICT_SNAPSHOT as u32;
    RECOVERY_CONFLICT_PENDING_REASONS.with(|c| c.set(0));
    HandleRecoveryConflictInterrupt(lock);
    HandleRecoveryConflictInterrupt(snapshot);
    assert!(recovery_conflict_interrupt_pending());
    RECOVERY_CONFLICT_PENDING_REASONS.with(|c| c.set(c.get() & !RECOVERY_CONFLICT_PENDING));
    assert!(take_recovery_conflict_reason(lock));
    // LOCK raised ERROR: SNAPSHOT stays recorded but gated off.
    assert!(!recovery_conflict_interrupt_pending());
    assert_ne!(RECOVERY_CONFLICT_PENDING_REASONS.with(std::cell::Cell::get) & (1 << snapshot), 0);
    HandleRecoveryConflictInterrupt(lock);
    assert!(recovery_conflict_interrupt_pending());
    RECOVERY_CONFLICT_PENDING_REASONS.with(|c| c.set(c.get() & !RECOVERY_CONFLICT_PENDING));
    assert!(take_recovery_conflict_reason(lock));
    assert!(take_recovery_conflict_reason(snapshot));
    assert!(!take_recovery_conflict_reason(lock));
    assert_eq!(RECOVERY_CONFLICT_PENDING_REASONS.with(std::cell::Cell::get), 0);
    init_small::globals::SetInterruptPending(false);
}

// walsender.c:3677: a walsender keeps SignalHandlerForCrashExit for SIGQUIT,
// so an immediate shutdown writes no WARNING onto the replication connection.
#[test]
fn walsender_sigquit_exits_without_a_client_warning() {
    install_test_seams();
    install_ipc_stubs();
    let _serial = CANCEL_ARM.lock().unwrap();

    static SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());
    fn capture(e: &types_error::PgError, _output_to_server: &mut bool) {
        SEEN.lock().unwrap().push(e.message.clone());
    }
    let run = |walsender: bool| -> i32 {
        std::thread::spawn(move || {
            use init_small::globals as g;
            g::SetMyProcNumber(2);
            g::SetMyProcPid(6262);
            g::SetIsUnderPostmaster(true);
            walsender_seams::set_am_walsender(walsender);
            procsignal::ProcSignalInit(&[]).unwrap();
            my_latch();
            install_thread_signal_handlers();
            let prev = elog::sink::set_emit_log_hook(Some(capture));
            assert_eq!(procsignal::SendThreadSignal(6262, libc::SIGQUIT), 0);
            let outcome = std::panic::catch_unwind(procsignal::DrainThreadSignals);
            elog::sink::set_emit_log_hook(prev);
            let payload = outcome.expect_err("SIGQUIT disposition exits the thread");
            payload.downcast_ref::<ipc::ProcExitThread>().expect("exit_thread_raw").code
        })
        .join()
        .unwrap()
    };

    SEEN.lock().unwrap().clear();
    assert_eq!(run(true), 2);
    assert!(
        SEEN.lock().unwrap().is_empty(),
        "walsender SIGQUIT must not ereport: {:?}",
        SEEN.lock().unwrap()
    );

    SEEN.lock().unwrap().clear();
    assert_eq!(run(false), 2);
    assert!(SEEN.lock().unwrap().iter().any(|m| m.contains("terminating connection")));
}

#[test]
fn idle_and_transaction_timeout_arms() {
    install_test_seams();

    // GUC reset to zero between firing and servicing: signal ignored.
    init_small::globals::SetInterruptPending(true);
    init_small::globals::SetIdleInTransactionSessionTimeoutPending(true);
    lmgr_proc::globals::set_IdleInTransactionSessionTimeout(0);
    assert!(check_for_interrupts().is_ok());
    assert!(!init_small::globals::IdleInTransactionSessionTimeoutPending());

    for (set_pending, set_guc, sqlstate, msg) in [
        (
            init_small::globals::SetIdleInTransactionSessionTimeoutPending as fn(bool),
            lmgr_proc::globals::set_IdleInTransactionSessionTimeout as fn(i32),
            types_error::ERRCODE_IDLE_IN_TRANSACTION_SESSION_TIMEOUT,
            "terminating connection due to idle-in-transaction timeout",
        ),
        (
            init_small::globals::SetTransactionTimeoutPending,
            lmgr_proc::globals::set_TransactionTimeout,
            types_error::ERRCODE_TRANSACTION_TIMEOUT,
            "terminating connection due to transaction timeout",
        ),
        (
            init_small::globals::SetIdleSessionTimeoutPending,
            lmgr_proc::globals::set_IdleSessionTimeout,
            types_error::ERRCODE_IDLE_SESSION_TIMEOUT,
            "terminating connection due to idle-session timeout",
        ),
    ] {
        init_small::globals::SetInterruptPending(true);
        set_pending(true);
        set_guc(100);
        let err = check_for_interrupts().unwrap_err();
        assert_eq!(err.level(), types_error::FATAL);
        assert_eq!(err.sqlstate, sqlstate);
        assert_eq!(err.message, msg);
        set_pending(false);
        set_guc(0);
    }
    init_small::globals::SetInterruptPending(false);
}

#[test]
fn escaped_fatal_counts_as_fatal_disconnect() {
    use pgstat::database::{pgstat_session_end_cause, pgstat_set_session_end_cause, SessionEndType};
    pgstat_set_session_end_cause(SessionEndType::DisconnectNormal);
    let fatal = ereport(FATAL)
        .errcode(types_error::ERRCODE_IDLE_SESSION_TIMEOUT)
        .errmsg("terminating connection due to idle-session timeout")
        .into_error();
    crate::main_loop::report_escaped_error(&fatal);
    assert_eq!(pgstat_session_end_cause(), SessionEndType::DisconnectFatal);
    pgstat_set_session_end_cause(SessionEndType::DisconnectKilled);
    crate::main_loop::report_escaped_error(&fatal);
    assert_eq!(pgstat_session_end_cause(), SessionEndType::DisconnectKilled);
    pgstat_set_session_end_cause(SessionEndType::DisconnectNormal);
}

#[test]
fn die_sets_flags_and_latch() {
    my_latch();
    init_small::globals::HoldInterrupts(); /* keep ProcessInterrupts inert */
    assert!(die().is_ok());
    assert!(init_small::globals::InterruptPending());
    assert!(init_small::globals::ProcDiePending());
    assert_eq!(
        pgstat::database::pgstat_session_end_cause(),
        pgstat::database::SessionEndType::DisconnectKilled
    );
    init_small::globals::SetProcDiePending(false);
    init_small::globals::SetInterruptPending(false);
    init_small::globals::ResumeInterrupts();
}

#[test]
fn statement_cancel_handler_sets_flags() {
    my_latch();
    StatementCancelHandler();
    assert!(init_small::globals::InterruptPending());
    assert!(init_small::globals::QueryCancelPending());
    init_small::globals::SetQueryCancelPending(false);
    init_small::globals::SetInterruptPending(false);
}

#[test]
fn float_exception_handler_is_22p01() {
    let err = FloatExceptionHandler().unwrap_err();
    assert_eq!(err.sqlstate, types_error::ERRCODE_FLOATING_POINT_EXCEPTION);
}

#[test]
fn show_usage_reports_without_reset() {
    // Without ResetUsage, ShowUsage still reports (totals leg).
    let _ = ShowUsage("TEST STATISTICS");
}

fn install_ipc_stubs() {
    crate::session_tests::install_shared_stubs();
    crate::session_tests::install_shared_proc_fixture();
}

// The shutdown/cancel delivery spine end-to-end: another thread "kills" this
// backend through the procsignal surface; the parked backend wakes, drains,
// and its CFI raises C's exact SQLSTATEs (57014 cancel, 57P01 die).
#[test]
fn thread_signal_sigint_cancels_and_sigterm_terminates() {
    install_test_seams();
    install_ipc_stubs();
    let _serial = CANCEL_ARM.lock().unwrap();

    let (err_tx, err_rx) = std::sync::mpsc::channel();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let backend = std::thread::spawn(move || {
        use init_small::globals as g;
        g::SetMyProcNumber(2);
        g::SetMyProcPid(6161);
        procsignal::ProcSignalInit(&[]).unwrap();
        let h = latch::allocate_local_latch();
        latch::InitLatch(h);
        g::SetMyLatch(Some(h));
        install_thread_signal_handlers();
        ready_tx.send(()).unwrap();

        let proc_latch = &lmgr_proc::GetPGProcByNumber(2).procLatch;
        loop {
            while !proc_latch.is_set() {
                std::thread::yield_now();
            }
            proc_latch.is_set.store(0, Ordering::SeqCst);
            // The WaitLatch wake path: drain dispositions, then CFI.
            if let Err(e) = procsignal::DrainThreadSignals().and_then(|_| check_for_interrupts())
            {
                let fatal = e.level() >= types_error::FATAL;
                err_tx.send(e).unwrap();
                if fatal {
                    return;
                }
            }
        }
    });

    ready_rx.recv().unwrap();
    let timeout = std::time::Duration::from_secs(10);

    assert_eq!(procsignal::SendThreadSignal(6161, libc::SIGINT), 0);
    let err = err_rx.recv_timeout(timeout).expect("backend must surface the cancel");
    assert_eq!(err.level(), types_error::ERROR);
    assert_eq!(err.sqlstate, types_error::ERRCODE_QUERY_CANCELED); /* 57014 */
    assert!(err.message.contains("user request"));

    assert_eq!(procsignal::SendThreadSignal(6161, libc::SIGTERM), 0);
    let err = err_rx.recv_timeout(timeout).expect("backend must surface the die");
    assert_eq!(err.level(), types_error::FATAL);
    assert_eq!(err.sqlstate, types_error::ERRCODE_ADMIN_SHUTDOWN); /* 57P01 */
    backend.join().unwrap();
}

// D355 (a D266 follow-up): a thread signal delivered while ProcessInterrupts drains (after the drain took the
// pending word, before InterruptPending is cleared) must not be stranded. In C the handler runs at delivery and raises
// InterruptPending after ProcessInterrupts cleared it, so a busy backend's next CHECK_FOR_INTERRUPTS runs it. The
// window is forced deterministically: SIGHUP's disposition, run by the drain, sends this backend SIGUSR2 (the sender
// pends the bit and raises InterruptPending), and SIGUSR2's disposition counts. After the check the SIGUSR2 bit is
// pending; the next check must reach it without the backend going idle (no latch wait, no explicit drain).
#[test]
fn a_thread_signal_delivered_during_the_drain_is_not_stranded() {
    install_test_seams();
    install_ipc_stubs();
    let backend = std::thread::spawn(|| {
        use init_small::globals as g;
        use std::sync::atomic::{AtomicBool, AtomicUsize};
        static RESENT: AtomicBool = AtomicBool::new(false);
        static SECOND: AtomicUsize = AtomicUsize::new(0);
        g::SetMyProcNumber(3);
        g::SetMyProcPid(6266);
        procsignal::ProcSignalInit(&[]).unwrap();
        let h = latch::allocate_local_latch();
        latch::InitLatch(h);
        g::SetMyLatch(Some(h));
        procsignal::pqsignal_thread(
            libc::SIGUSR2,
            procsignal::ThreadSignalHandler::Simple(|| {
                SECOND.fetch_add(1, Ordering::SeqCst);
            }),
        );
        procsignal::pqsignal_thread(
            libc::SIGHUP,
            procsignal::ThreadSignalHandler::Simple(|| {
                if !RESENT.swap(true, Ordering::SeqCst) {
                    assert_eq!(procsignal::SendThreadSignal(6266, libc::SIGUSR2), 0);
                }
            }),
        );
        g::SetInterruptPending(false);
        assert_eq!(procsignal::SendThreadSignal(6266, libc::SIGHUP), 0);
        assert!(
            g::InterruptPending(),
            "delivery raises InterruptPending (D266)"
        );
        check_for_interrupts().unwrap(); /* runs SIGHUP; SIGUSR2 lands mid-drain */
        assert!(RESENT.load(Ordering::SeqCst));
        let stranded = !g::InterruptPending() && SECOND.load(Ordering::SeqCst) == 0;
        check_for_interrupts().unwrap(); /* the busy backend's next CHECK_FOR_INTERRUPTS */
        let seen = SECOND.load(Ordering::SeqCst);
        procsignal::DrainThreadSignals().unwrap(); /* leave the slot clean either way */
        g::SetInterruptPending(false);
        (stranded, seen)
    });
    let (stranded, seen) = backend.join().unwrap();
    assert!(
        !stranded,
        "SIGUSR2 delivered during the drain is pending with InterruptPending cleared: stranded until the backend idles"
    );
    assert_eq!(
        seen, 1,
        "the next CHECK_FOR_INTERRUPTS runs the signal delivered during the drain, exactly once"
    );
}

// ---- check_log_duration sampling (GL-GUCBATCH-1; postgres.c:2427 parity) ----

fn sampling_gucs() -> simple_query::LogDurationGucs {
    simple_query::LogDurationGucs {
        log_duration: false,
        log_min: -1,
        log_min_sample: -1,
        sample_rate: 1.0,
        xact_is_sampled: false,
    }
}

#[test]
fn log_sampling_boot_defaults_return_zero_without_a_draw() {
    let g = sampling_gucs();
    let (code, msec) =
        simple_query::check_log_duration_impl(false, 5_000_000, &g, || panic!("no draw"));
    assert_eq!(code, 0);
    assert!(msec.is_empty());
}

// postgres.c:2437 TimestampDifference clamps a backwards clock step to zero:
// the logged duration is "0.000", never a negative rendering.
#[test]
fn log_duration_clamps_a_negative_elapsed_time_to_zero() {
    let mut g = sampling_gucs();
    g.log_min = 0;
    let (code, msec) =
        simple_query::check_log_duration_impl(false, -1_500, &g, || panic!("no draw"));
    assert_eq!(code, 2);
    assert_eq!(msec, "0.000");
}

#[test]
fn log_sampling_rate_zero_never_logs_and_never_draws() {
    let mut g = sampling_gucs();
    g.log_min_sample = 0; // every statement exceeds the sample threshold
    g.sample_rate = 0.0;
    let (code, _) =
        simple_query::check_log_duration_impl(false, 5_000, &g, || panic!("no draw at rate 0"));
    assert_eq!(code, 0);
}

#[test]
fn log_sampling_rate_one_always_logs_and_never_draws() {
    let mut g = sampling_gucs();
    g.log_min_sample = 0;
    g.sample_rate = 1.0;
    let (code, msec) =
        simple_query::check_log_duration_impl(false, 1_234_567, &g, || panic!("no draw at rate 1"));
    assert_eq!(code, 2);
    // C: snprintf "%ld.%03d" of (secs*1000 + msecs, usecs % 1000).
    assert_eq!(msec, "1234.567");
    // Already-logged statements report 1 (log duration only), C's return 1 arm.
    let (code, _) =
        simple_query::check_log_duration_impl(true, 1_234_567, &g, || panic!("no draw at rate 1"));
    assert_eq!(code, 1);
}

#[test]
fn log_sampling_min_duration_sample_gates_the_draw() {
    let mut g = sampling_gucs();
    g.log_min_sample = 100; // ms
    g.sample_rate = 1.0;
    // 50ms: under the sample threshold — no draw, no log.
    let (code, _) =
        simple_query::check_log_duration_impl(false, 50_000, &g, || panic!("under threshold"));
    assert_eq!(code, 0);
    // 150ms: over — rate 1 logs.
    let (code, _) =
        simple_query::check_log_duration_impl(false, 150_000, &g, || panic!("no draw at rate 1"));
    assert_eq!(code, 2);
}

#[test]
fn log_sampling_fractional_rate_statistical_n1000() {
    let mut g = sampling_gucs();
    g.log_min_sample = 0;
    g.sample_rate = 0.25;
    let prng = std::cell::RefCell::new(pg_prng::PgPrng::seeded(0x6763_6261_7463_6831));
    let mut hits = 0;
    for _ in 0..1000 {
        let (code, _) = simple_query::check_log_duration_impl(false, 5_000, &g, || {
            prng.borrow_mut().next_f64()
        });
        if code == 2 {
            hits += 1;
        }
    }
    // Seeded, hence deterministic; the band guards the decision inequality
    // (<= rate) rather than the generator's exact stream.
    assert!((200..=300).contains(&hits), "hits={hits} outside 200..=300 for rate 0.25");
}

#[test]
fn log_sampling_xact_sampled_forces_logging() {
    let mut g = sampling_gucs();
    g.xact_is_sampled = true;
    let (code, msec) =
        simple_query::check_log_duration_impl(false, 2_000, &g, || panic!("no draw"));
    assert_eq!(code, 2);
    assert_eq!(msec, "2.000");
    let (code, _) =
        simple_query::check_log_duration_impl(true, 2_000, &g, || panic!("no draw"));
    assert_eq!(code, 1);
}

#[test]
fn log_sampling_leaves_log_min_duration_statement_intact() {
    let mut g = sampling_gucs();
    g.log_min = 0; // log_min_duration_statement=0: log everything, no sampling
    g.sample_rate = 0.0;
    let (code, _) = simple_query::check_log_duration_impl(false, 10, &g, || panic!("no draw"));
    assert_eq!(code, 2);
}

// Issue #58: an Err reaching error_recovery with the critical section still
// open (a hand-built PgError that bypassed errstart's ERROR->PANIC promotion,
// e.g. XLogFlush failing after the commit record is already in WAL) must
// crash-and-restart, not settle back into the main loop. The guard sits
// before every recovery step, so no seams are needed to reach it.
#[test]
fn error_recovery_escalates_instead_of_recovering_inside_critical_section() {
    use init_small::globals as g;

    let err = types_error::PgError::new(
        types_error::ERROR,
        "xlog flush request 0/1234 is not satisfied",
    );
    let mut state = main_loop::LoopState {
        send_ready_for_query: true,
        idle_in_transaction_timeout_enabled: false,
        idle_session_timeout_enabled: false,
        idle_passivate_timeout_enabled: false,
        log_defer_before: None,
    };

    g::SetCritSectionCount(1);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = main_loop::error_recovery(&err, &mut state, true);
    }));
    g::SetCritSectionCount(0);

    assert!(result
        .expect_err("error_recovery must not recover with a critical section open")
        .is::<types_error::PanicExitThread>());
}

// D2933 (user 2026-09-29: "when crashing, the whole system should abort like
// postgres does"): the statement boundary keeps a structured error (a PgError
// payload) an ERROR, re-raises an exit payload, and turns a plain panic into C's
// abort(): the unwind goes on as PanicExitThread (the thread dies as crashed,
// the postmaster's crash restart), never ERROR XX000 to the client.
#[test]
fn d2933_plain_panic_takes_the_crash_path_not_xx000() {
    let pg = main_loop::pg_error_from_panic(Box::new(types_error::PgError::new(
        types_error::ERROR,
        "division by zero",
    )));
    assert_eq!((pg.level(), pg.message()), (types_error::ERROR, "division by zero"));
    let boxed = main_loop::pg_error_from_panic(Box::new(Box::new(types_error::PgError::new(
        types_error::ERROR,
        "boxed",
    ))));
    assert_eq!(boxed.message(), "boxed");

    for plain in [
        Box::new(String::from("index out of bounds: the len is 1 but the index is 2"))
            as Box<dyn std::any::Any + Send>,
        Box::new("called `Option::unwrap()` on a `None` value"),
        Box::new(7u32),
    ] {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            main_loop::pg_error_from_panic(plain)
        }));
        let payload = r.expect_err("a plain panic must not become an ERROR");
        assert!(payload.is::<types_error::PanicExitThread>(), "C's abort(): the crash path");
    }

    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        main_loop::pg_error_from_panic(Box::new(types_error::PanicExitThread))
    }));
    assert!(r.expect_err("a PANIC keeps unwinding").is::<types_error::PanicExitThread>());
}

// audit-18.6 b151: errdetail_abort (postgres.c:2540) — the aborted-transaction
// refusal carries "Abort reason: recovery conflict" exactly when
// MyProc->recoveryConflictPending; without a PGPROC bound (this unit
// environment) the DETAIL is absent, as for an ordinary aborted block.
#[test]
fn aborted_xact_error_carries_recovery_conflict_detail_only_when_pending() {
    assert_eq!(errdetail_abort_for(false), None);
    assert_eq!(errdetail_abort_for(true), Some("Abort reason: recovery conflict"));

    let err = extended_query::aborted_xact_error("exec_execute_message");
    assert_eq!(err.sqlstate, types_error::ERRCODE_IN_FAILED_SQL_TRANSACTION);
    assert_eq!(
        err.message,
        "current transaction is aborted, commands ignored until end of transaction block"
    );
    assert_eq!(err.detail, None);
}
