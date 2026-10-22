use std::sync::atomic::{AtomicI64, Ordering as O};
use std::sync::Once;
use std::time::Duration;

use super::*;

static SETUP: Once = Once::new();
static NOW: AtomicI64 = AtomicI64::new(1_000_000_000);
/// Serializes the tests: `NOW` is a process-global clock shared by every
/// test in this module (the seam is process-global), so a concurrent
/// test's `advance_ms` races another's start/finish-time asserts and its
/// timer-thread post cadence (observed as a ~50%-rate
/// register_enable_fire_after failure under the default multi-threaded
/// test runner — regression adjudication, pre-existing at t35 main).
/// Each test holds the guard for its whole body via `setup_thread`.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

thread_local! {
    static FIRED: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
}

#[must_use = "hold the guard for the whole test body (global-clock serialization)"]
fn setup_thread(pid: i32) -> std::sync::MutexGuard<'static, ()> {
    let guard = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    SETUP.call_once(|| {
        timestamp_seams::get_current_timestamp::set(|| NOW.load(O::Relaxed));
    });
    globals::SetMyProcPid(pid);
    globals::SetMyLatch(Some(latch::allocate_local_latch()));
    InitializeTimeouts();
    guard
}

fn advance_ms(ms: i64) {
    NOW.fetch_add(ms * 1000, O::Relaxed);
}

fn handler_a() {
    FIRED.with(|f| f.borrow_mut().push("a"));
}

fn handler_b() {
    FIRED.with(|f| f.borrow_mut().push("b"));
}

fn drain_when_posted() {
    // Wait for the timer thread to post, then run the synchronous delivery.
    let posted = POSTED.with(|p| p.borrow().as_ref().map(std::sync::Arc::clone).unwrap());
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !posted.load(O::SeqCst) {
        assert!(std::time::Instant::now() < deadline, "timer thread never fired");
        std::thread::sleep(Duration::from_millis(1));
    }
    ProcessTimeoutInterrupt();
}

#[test]
fn register_enable_fire_after() {
    let _serial = setup_thread(9001);
    let id = RegisterTimeout(STATEMENT_TIMEOUT, handler_a);
    assert_eq!(id, STATEMENT_TIMEOUT);

    enable_timeout_after(STATEMENT_TIMEOUT, 5);
    assert!(get_timeout_active(STATEMENT_TIMEOUT));
    assert!(!get_timeout_indicator(STATEMENT_TIMEOUT, false));
    assert_eq!(get_timeout_start_time(STATEMENT_TIMEOUT), NOW.load(O::Relaxed));
    assert_eq!(
        get_timeout_finish_time(STATEMENT_TIMEOUT),
        NOW.load(O::Relaxed) + 5000
    );

    advance_ms(10);
    drain_when_posted();

    assert_eq!(FIRED.with(|f| f.borrow().clone()), vec!["a"]);
    assert!(!get_timeout_active(STATEMENT_TIMEOUT));
    assert!(get_timeout_indicator(STATEMENT_TIMEOUT, true));
    assert!(!get_timeout_indicator(STATEMENT_TIMEOUT, false));
    // The drain also set our latch, as C's handler does.
    let latch = globals::MyLatch().unwrap();
    assert_eq!(latch::latch_ref(latch).is_set.load(O::SeqCst), 1);
}

#[test]
fn priority_and_ordering() {
    let _serial = setup_thread(9002);
    RegisterTimeout(DEADLOCK_TIMEOUT, handler_a);
    RegisterTimeout(LOCK_TIMEOUT, handler_b);

    // Same fin_time: lower TimeoutId (DEADLOCK) sorts first even if enabled
    // second.
    enable_timeouts(&[
        EnableTimeoutParams::After {
            id: LOCK_TIMEOUT,
            delay_ms: 7,
        },
        EnableTimeoutParams::After {
            id: DEADLOCK_TIMEOUT,
            delay_ms: 7,
        },
    ]);
    DATA.with(|d| {
        let data = d.borrow();
        assert_eq!(data.num_active, 2);
        assert_eq!(data.active_timeouts[0], DEADLOCK_TIMEOUT);
        assert_eq!(data.active_timeouts[1], LOCK_TIMEOUT);
    });

    advance_ms(20);
    drain_when_posted();
    assert_eq!(FIRED.with(|f| f.borrow().clone()), vec!["a", "b"]);
}

#[test]
fn disable_and_reschedule() {
    let _serial = setup_thread(9003);
    RegisterTimeout(DEADLOCK_TIMEOUT, handler_a);
    RegisterTimeout(LOCK_TIMEOUT, handler_b);

    enable_timeout_after(DEADLOCK_TIMEOUT, 5);
    enable_timeout_after(LOCK_TIMEOUT, 50_000);
    disable_timeout(DEADLOCK_TIMEOUT, false);
    assert!(!get_timeout_active(DEADLOCK_TIMEOUT));
    assert!(get_timeout_active(LOCK_TIMEOUT));

    disable_timeouts(&[DisableTimeoutParams {
        id: LOCK_TIMEOUT,
        keep_indicator: true,
    }]);
    assert!(!get_timeout_active(LOCK_TIMEOUT));
    assert_eq!(DATA.with(|d| d.borrow().num_active), 0);

    // Re-enable then wipe everything; indicators kept on request.
    enable_timeout_after(DEADLOCK_TIMEOUT, 5);
    DATA.with(|d| d.borrow_mut().all_timeouts[LOCK_TIMEOUT as usize].indicator = true);
    disable_all_timeouts(true);
    assert_eq!(DATA.with(|d| d.borrow().num_active), 0);
    assert!(get_timeout_indicator(LOCK_TIMEOUT, true));

    reschedule_timeouts();

    clear_interrupt_bytes();
    advance_ms(60_000);
    assert_no_wake();
    ProcessTimeoutInterrupt();
    assert!(FIRED.with(|f| f.borrow().is_empty()));
}

#[test]
fn periodic_reenables_itself() {
    let _serial = setup_thread(9004);
    RegisterTimeout(IDLE_STATS_UPDATE_TIMEOUT, handler_b);
    let fin = NOW.load(O::Relaxed) + 5000;
    enable_timeout_every(IDLE_STATS_UPDATE_TIMEOUT, fin, 5);

    advance_ms(6);
    drain_when_posted();
    assert_eq!(FIRED.with(|f| f.borrow().clone()), vec!["b"]);
    // Re-enabled for the next interval; enable_timeout cleared the indicator.
    assert!(get_timeout_active(IDLE_STATS_UPDATE_TIMEOUT));
    assert!(!get_timeout_indicator(IDLE_STATS_UPDATE_TIMEOUT, false));
    disable_all_timeouts(false);
}

#[test]
fn reinit_keeps_long_deadlines_armable() {
    // The early-session lost-timeout shape: a pre-init phase arms a long
    // timeout (the startup-packet/auth window), disables it, and the session
    // main re-runs InitializeTimeouts — which destroys the armed slot wake.
    // A later enable whose fin_time lands at or past the stale pre-init
    // deadline must still ARM a real wake and fire (legacy behavior took the
    // setitimer-avoidance skip against the destroyed wake and never fired).
    let _serial = setup_thread(9008);
    RegisterTimeout(STATEMENT_TIMEOUT, handler_a);

    // Pre-init phase: long arm (physical deadline far in the future), then
    enable_timeout_after(STATEMENT_TIMEOUT, 6_000);
    disable_timeout(STATEMENT_TIMEOUT, false);

    // The session-main re-init (wipes the slot deadline).
    InitializeTimeouts();
    RegisterTimeout(STATEMENT_TIMEOUT, handler_a);

    // Arm so fin_time lands past the stale pre-init deadline while now is
    // still before it: fake-now T0+5.9s, fin T0+6.1s >= stale due-at T0+6s.
    advance_ms(5_900);
    enable_timeout_after(STATEMENT_TIMEOUT, 200);

    advance_ms(300);
    drain_when_posted();
    assert_eq!(FIRED.with(|f| f.borrow().clone()), vec!["a"]);
    assert!(!get_timeout_active(STATEMENT_TIMEOUT));
}

#[test]
fn timer_post_raises_interrupt_pending() {
    let _serial = setup_thread(9007);
    RegisterTimeout(STATEMENT_TIMEOUT, handler_a);
    globals::SetInterruptPending(false);

    enable_timeout_after(STATEMENT_TIMEOUT, 5);
    advance_ms(10);
    drain_when_posted();

    // The timer thread raised this backend's InterruptPending alongside the
    // post, so a CPU-bound CHECK_FOR_INTERRUPTS reaches ProcessInterrupts.
    assert!(globals::InterruptPending());
    globals::SetInterruptPending(false);
}

fn clear_interrupt_bytes() {
    globals::SetInterruptPending(false);
    globals::SetAbortPending(false);
    POSTED.with(|p| p.borrow().as_ref().unwrap().store(false, O::SeqCst));
}

fn assert_no_wake() {
    std::thread::sleep(Duration::from_millis(60));
    assert!(!POSTED.with(|p| p.borrow().as_ref().unwrap().load(O::SeqCst)), "stale wake posted");
    assert!(!globals::InterruptPending(), "stale wake raised InterruptPending");
    assert!(!globals::AbortPending(), "stale wake raised AbortPending");
}

#[test]
fn disabled_timeout_wake_is_withdrawn() {
    let _serial = setup_thread(9011);
    RegisterTimeout(STATEMENT_TIMEOUT, handler_a);
    clear_interrupt_bytes();

    enable_timeout_after(STATEMENT_TIMEOUT, 5);
    disable_timeout(STATEMENT_TIMEOUT, false);
    advance_ms(10);
    assert_no_wake();

    enable_timeout_after(STATEMENT_TIMEOUT, 5);
    disable_all_timeouts(false);
    advance_ms(10);
    assert_no_wake();
    assert!(FIRED.with(|f| f.borrow().is_empty()));
}

#[test]
fn rescheduled_later_wake_does_not_fire_early() {
    let _serial = setup_thread(9012);
    RegisterTimeout(STATEMENT_TIMEOUT, handler_a);
    clear_interrupt_bytes();

    enable_timeout_after(STATEMENT_TIMEOUT, 5);
    enable_timeout_after(STATEMENT_TIMEOUT, 500);
    advance_ms(10);
    assert_no_wake();
    assert!(get_timeout_active(STATEMENT_TIMEOUT));

    advance_ms(500);
    drain_when_posted();
    assert_eq!(FIRED.with(|f| f.borrow().clone()), vec!["a"]);
    assert!(globals::InterruptPending());
    assert!(globals::AbortPending());
    clear_interrupt_bytes();
}

#[test]
fn abort_byte_only_for_abort_class() {
    let _serial = setup_thread(9013);
    RegisterTimeout(STATEMENT_TIMEOUT, handler_a);
    RegisterTimeout(DEADLOCK_TIMEOUT, handler_b);
    clear_interrupt_bytes();

    enable_timeout_after(DEADLOCK_TIMEOUT, 5);
    advance_ms(10);
    drain_when_posted();
    assert_eq!(FIRED.with(|f| f.borrow().clone()), vec!["b"]);
    assert!(globals::InterruptPending());
    assert!(!globals::AbortPending(), "a deadlock timeout is not abort class");
    clear_interrupt_bytes();

    enable_timeout_after(STATEMENT_TIMEOUT, 5);
    advance_ms(10);
    drain_when_posted();
    assert_eq!(FIRED.with(|f| f.borrow().clone()), vec!["b", "a"]);
    assert!(globals::InterruptPending());
    assert!(globals::AbortPending());
    clear_interrupt_bytes();
}

static REARM_ID: AtomicI64 = AtomicI64::new(-1);

// The handler re-arms its own id as a one-shot 60s away.
fn handler_rearm_one_shot() {
    FIRED.with(|f| f.borrow_mut().push("rearm"));
    enable_timeout_after(REARM_ID.load(O::Relaxed) as TimeoutId, 60_000);
}

// timeout.c:417: interval_in_ms/fin_time are read after the handler, so a
// handler that re-arms as a one-shot is not overwritten by the periodic
// reschedule.
#[test]
fn handler_rearm_wins_over_periodic_reschedule() {
    let _serial = setup_thread(9009);
    RegisterTimeout(STANDBY_TIMEOUT, handler_rearm_one_shot);
    REARM_ID.store(STANDBY_TIMEOUT as i64, O::Relaxed);
    let fin = NOW.load(O::Relaxed) + 5000;
    enable_timeout_every(STANDBY_TIMEOUT, fin, 5);

    advance_ms(6);
    drain_when_posted();
    assert_eq!(FIRED.with(|f| f.borrow().clone()), vec!["rearm"]);
    assert!(get_timeout_active(STANDBY_TIMEOUT));
    assert_eq!(get_timeout_finish_time(STANDBY_TIMEOUT), NOW.load(O::Relaxed) + 60_000_000);
    disable_all_timeouts(false);
}

#[test]
fn user_timeout_slots_exhausted_is_fatal_53400() {
    let _serial = setup_thread(9010);
    for _ in USER_TIMEOUT..MAX_TIMEOUTS {
        RegisterTimeout(USER_TIMEOUT, handler_a);
    }
    let payload = std::panic::catch_unwind(|| RegisterTimeout(USER_TIMEOUT, handler_b))
        .expect_err("eleventh user timeout");
    let err = types_error::pg_error_from_panic(payload).expect("structured PgError");
    assert_eq!(err.level(), FATAL);
    assert_eq!(err.sqlstate(), ERRCODE_CONFIGURATION_LIMIT_EXCEEDED);
    assert_eq!(err.message(), "cannot add more timeout reasons");
}

fn slot_registered(pid: i32) -> bool {
    timer().slots.lock().unwrap().contains_key(&pid)
}

#[test]
fn backend_slot_released_at_task_end() {
    let _serial = setup_thread(9011);
    assert!(slot_registered(9011));
    forget_backend_timer_slot();
    assert!(!slot_registered(9011));
    // A later arm re-registers on demand, so releasing is never premature.
    RegisterTimeout(STATEMENT_TIMEOUT, handler_a);
    enable_timeout_after(STATEMENT_TIMEOUT, 60_000);
    assert!(slot_registered(9011));
    disable_all_timeouts(false);
    forget_backend_timer_slot();
    assert!(!slot_registered(9011));
}

#[test]
fn user_timeout_allocation() {
    let _serial = setup_thread(9005);
    let first = RegisterTimeout(USER_TIMEOUT, handler_a);
    assert_eq!(first, USER_TIMEOUT);
    let second = RegisterTimeout(USER_TIMEOUT, handler_b);
    assert_eq!(second, USER_TIMEOUT + 1);
}

#[test]
fn seams_installed_once() {
    // init_seams is process-global (seams are set-once); exercise the seam
    // surface from this thread.
    let _serial = setup_thread(9006);
    init_seams();
    RegisterTimeout(TRANSACTION_TIMEOUT, handler_a);
    timeout_seams::enable_timeout_after::call(TRANSACTION_TIMEOUT, 30_000).unwrap();
    assert!(timeout_seams::get_timeout_active::call(TRANSACTION_TIMEOUT));
    timeout_seams::disable_timeout::call(TRANSACTION_TIMEOUT, false).unwrap();
    assert!(!timeout_seams::get_timeout_active::call(TRANSACTION_TIMEOUT));
    timeout_seams::disable_all_timeouts::call(false).unwrap();
    timeout_seams::reschedule_timeouts::call().unwrap();
    timeout_seams::process_timeout_interrupt::call();
}

// the statement deadline passes while the shared timer thread cannot run (under load it waits on the run queue
// for a CPU; here it is held off the slots lock), and the backend reaches a CHECK_FOR_INTERRUPTS right after. As in
// PostgreSQL, whose SIGALRM is delivered to the backend itself on expiry (timeout.c handle_sig_alarm), that check must
// fire the timeout (fire_due at the run's end, then the check's drain): the handler runs, the indicator is set,
// InterruptPending and the abort byte are raised. Before the fix only the timer thread's post could fire it, so the
// check found nothing and the statement completed.
#[test]
fn statement_deadline_fires_at_the_backends_check_while_the_timer_thread_is_stalled() {
    let _serial = setup_thread(9014);
    RegisterTimeout(STATEMENT_TIMEOUT, handler_a);
    for i in 0..20 {
        clear_interrupt_bytes();
        FIRED.with(|f| f.borrow_mut().clear());

        enable_timeout_after(STATEMENT_TIMEOUT, 5);
        {
            let _stall = timer().slots.lock().unwrap();
            std::thread::sleep(Duration::from_millis(20));
            advance_ms(10);
        }
        fire_due();
        ProcessTimeoutInterrupt();

        assert_eq!(FIRED.with(|f| f.borrow().clone()), vec!["a"], "iteration {i}: the expired deadline did not fire at the check");
        assert!(get_timeout_indicator(STATEMENT_TIMEOUT, true), "iteration {i}");
        assert!(globals::InterruptPending(), "iteration {i}");
        assert!(globals::AbortPending(), "iteration {i}");
        assert!(!get_timeout_active(STATEMENT_TIMEOUT), "iteration {i}");

        // The stalled timer thread must not fire it a second time once it gets the lock.
        clear_interrupt_bytes();
        std::thread::sleep(Duration::from_millis(20));
        ProcessTimeoutInterrupt();
        assert_eq!(FIRED.with(|f| f.borrow().clone()), vec!["a"], "iteration {i}: fired twice");
    }
    clear_interrupt_bytes();
}

#[test]
fn fire_due_is_inert_after_disable() {
    let _serial = setup_thread(9015);
    RegisterTimeout(STATEMENT_TIMEOUT, handler_a);
    clear_interrupt_bytes();
    FIRED.with(|f| f.borrow_mut().clear());
    enable_timeout_after(STATEMENT_TIMEOUT, 5);
    disable_timeout(STATEMENT_TIMEOUT, false);
    advance_ms(10);
    assert!(!fire_due());
    ProcessTimeoutInterrupt();
    assert!(FIRED.with(|f| f.borrow().is_empty()));
    assert!(!globals::InterruptPending());
    assert!(!globals::AbortPending());
    clear_interrupt_bytes();
}

#[test]
fn abort_deadline_behind_a_nearer_wake_raises_the_abort_byte() {
    let _serial = setup_thread(9016);
    RegisterTimeout(IDLE_STATS_UPDATE_TIMEOUT, handler_b);
    RegisterTimeout(STATEMENT_TIMEOUT, handler_a);
    clear_interrupt_bytes();
    FIRED.with(|f| f.borrow_mut().clear());
    enable_timeout_after(IDLE_STATS_UPDATE_TIMEOUT, 5);
    enable_timeout_after(STATEMENT_TIMEOUT, 60);
    let posted = POSTED.with(|p| p.borrow().as_ref().map(std::sync::Arc::clone).unwrap());
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !posted.load(O::SeqCst) {
        assert!(std::time::Instant::now() < deadline, "the nearer wake never fired");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(globals::InterruptPending());
    assert!(!globals::AbortPending(), "the nearer wake is not of the abort class");
    while !globals::AbortPending() {
        assert!(std::time::Instant::now() < deadline, "the statement timeout behind the nearer wake never raised the abort byte");
        std::thread::sleep(Duration::from_millis(1));
    }
    advance_ms(70);
    ProcessTimeoutInterrupt();
    assert_eq!(FIRED.with(|f| f.borrow().clone()), vec!["b", "a"]);
    assert!(get_timeout_indicator(STATEMENT_TIMEOUT, true));
    assert!(!get_timeout_active(STATEMENT_TIMEOUT));
    clear_interrupt_bytes();
}

// Idle passivation's shape (ready-for-query enables a non-abort timeout, the next command disables it): one wake serves
// every later re-enable, as C's setitimer avoidance; it fires early, fires nothing, and re-arms at the real deadline. An
// abort-class timeout never rides such a wake, and its own wake is withdrawn with it.
#[test]
fn a_non_abort_wake_serves_later_reenables() {
    let _serial = setup_thread(9017);
    RegisterTimeout(IDLE_STATS_UPDATE_TIMEOUT, handler_b);
    RegisterTimeout(STATEMENT_TIMEOUT, handler_a);
    clear_interrupt_bytes();
    FIRED.with(|f| f.borrow_mut().clear());
    let slot_ns = || timer().slots.lock().unwrap().get(&globals::MyProcPid()).and_then(|s| s.deadline).map(|d| d.as_ns());
    enable_timeout_after(IDLE_STATS_UPDATE_TIMEOUT, 50);
    let first = slot_ns().expect("armed");
    for _ in 0..3 {
        disable_timeout(IDLE_STATS_UPDATE_TIMEOUT, false);
        assert_eq!(slot_ns(), Some(first), "the non-abort wake stays when the last timeout goes");
        advance_ms(10);
        enable_timeout_after(IDLE_STATS_UPDATE_TIMEOUT, 50);
        assert_eq!(slot_ns(), Some(first), "a later deadline keeps the earlier wake");
    }
    drain_when_posted();
    assert!(FIRED.with(|f| f.borrow().is_empty()), "the early wake fires nothing");
    assert!(get_timeout_active(IDLE_STATS_UPDATE_TIMEOUT));
    assert!(slot_ns().expect("re-armed") > first, "re-armed at the real deadline");
    advance_ms(60);
    drain_when_posted();
    assert_eq!(FIRED.with(|f| f.borrow().clone()), vec!["b"]);

    clear_interrupt_bytes();
    enable_timeout_after(IDLE_STATS_UPDATE_TIMEOUT, 50);
    disable_timeout(IDLE_STATS_UPDATE_TIMEOUT, false);
    enable_timeout_after(STATEMENT_TIMEOUT, 100);
    assert!(timer().slots.lock().unwrap().get(&globals::MyProcPid()).is_some_and(|s| s.abort_flag.is_some()), "re-armed with the abort byte");
    disable_timeout(STATEMENT_TIMEOUT, false);
    assert_eq!(slot_ns(), None, "an abort-class wake is withdrawn");
    assert_no_wake();
    clear_interrupt_bytes();
}

// ready-for-query enables a non-abort timeout, the next command disables it; the wake left armed expires inside
// the statement with no timeout active. C's handler then finds alarm_enabled false: it resets signal_pending and sets the
// latch, nothing else. No post, no InterruptPending (a grant queue read it as an interrupt: D3069). A re-enable after the
// expiry arms a wake of its own at the real deadline.
#[test]
fn a_wake_left_after_the_last_timeout_went_only_sets_the_latch() {
    let _serial = setup_thread(9018);
    RegisterTimeout(IDLE_STATS_UPDATE_TIMEOUT, handler_b);
    clear_interrupt_bytes();
    FIRED.with(|f| f.borrow_mut().clear());
    let slot_ns = || timer().slots.lock().unwrap().get(&globals::MyProcPid()).and_then(|s| s.deadline).map(|d| d.as_ns());
    let latch = globals::MyLatch().unwrap();
    enable_timeout_after(IDLE_STATS_UPDATE_TIMEOUT, 20);
    disable_timeout(IDLE_STATS_UPDATE_TIMEOUT, false);
    assert!(slot_ns().is_some(), "the non-abort wake is kept for a later re-enable");
    assert!(signal_pending());
    latch::latch_ref(latch).is_set.store(0, O::SeqCst);
    advance_ms(30);
    assert_no_wake();
    assert_eq!(slot_ns(), None, "the wake was delivered");
    assert!(!signal_pending(), "its delivery reset signal_pending");
    assert_eq!(latch::latch_ref(latch).is_set.load(O::SeqCst), 1, "its delivery set the latch, as C's handler does");
    ProcessTimeoutInterrupt();
    assert!(FIRED.with(|f| f.borrow().is_empty()));

    enable_timeout_after(IDLE_STATS_UPDATE_TIMEOUT, 20);
    assert!(slot_ns().is_some(), "re-armed");
    advance_ms(30);
    drain_when_posted();
    assert_eq!(FIRED.with(|f| f.borrow().clone()), vec!["b"]);
    assert!(globals::InterruptPending());
    clear_interrupt_bytes();
}

// The Dekker pair: a wake delivered while a re-enable is between disable_alarm and its ride check is seen by one side.
// Either the post found the alarm enabled (it posts; the drain fires nothing early and re-arms) or the re-enable found
// signal_pending reset (it arms its own wake). Repeated re-enables around the kept wake's expiry never lose the timeout.
#[test]
fn a_reenable_racing_the_kept_wake_never_loses_the_timeout() {
    let _serial = setup_thread(9019);
    RegisterTimeout(IDLE_STATS_UPDATE_TIMEOUT, handler_b);
    for round in 0..20 {
        clear_interrupt_bytes();
        FIRED.with(|f| f.borrow_mut().clear());
        enable_timeout_after(IDLE_STATS_UPDATE_TIMEOUT, 1);
        disable_timeout(IDLE_STATS_UPDATE_TIMEOUT, false);
        advance_ms(1);
        std::thread::sleep(Duration::from_micros(200 * (round % 8)));
        enable_timeout_after(IDLE_STATS_UPDATE_TIMEOUT, 5);
        advance_ms(10);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while FIRED.with(|f| f.borrow().is_empty()) {
            assert!(std::time::Instant::now() < deadline, "round {round}: the re-enabled timeout never fired");
            ProcessTimeoutInterrupt();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(FIRED.with(|f| f.borrow().clone()), vec!["b"]);
    }
    clear_interrupt_bytes();
}

// wio P303: the process was stopped past the deadline and the waiter woke
// before the timer thread posted; the drain still delivers the expired wake.
#[test]
fn expired_wake_is_delivered_without_timer_post() {
    let _serial = setup_thread(9012);
    RegisterTimeout(STATEMENT_TIMEOUT, handler_a);
    enable_timeout_after(STATEMENT_TIMEOUT, 50);
    if let Some(slot) = timer().slots.lock().unwrap().get_mut(&9012) {
        slot.deadline = None;
    }
    std::thread::sleep(Duration::from_millis(80));
    advance_ms(60);
    ProcessTimeoutInterrupt();
    assert_eq!(FIRED.with(|f| f.borrow_mut().drain(..).collect::<Vec<_>>()), vec!["a"]);
    assert!(get_timeout_indicator(STATEMENT_TIMEOUT, true));
    ProcessTimeoutInterrupt();
    assert!(FIRED.with(|f| f.borrow().is_empty()));
    disable_all_timeouts(false);
}
