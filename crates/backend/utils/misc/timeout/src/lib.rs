#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]

// Thread-model SIGALRM: a shared timer thread posts + wakes the backend,
// which runs handle_sig_alarm synchronously at drain points; same-thread
// delivery removes C's volatile races (notes/timeout-threads.md).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

// pgsync by crate law (permit-s5, census row 16): the slots registry is
// locked by the (now door-registered) timer thread AND by arming backends;
// a raw std lock shared with a registered thread is the
// permit-holder-blocks-raw watchdog wedge shape (s2 AVAILABLE precedent).
// Native arm = identical std re-exports (zero cost).
use pgsync::{Mutex, OnceLock};

use pg_clock::Deadline;

use init_small::globals;
use types_core::TimestampTz;
use types_error::{PgError, ERRCODE_CONFIGURATION_LIMIT_EXCEEDED, FATAL};
use types_storage::latch::LatchHandle;

pub use timeout_seams::{
    DisableTimeoutParams, TimeoutId, CLIENT_CONNECTION_CHECK_TIMEOUT, DEADLOCK_TIMEOUT,
    IDLE_IN_TRANSACTION_SESSION_TIMEOUT, IDLE_SESSION_TIMEOUT, IDLE_STATS_UPDATE_TIMEOUT,
    LOCK_TIMEOUT, MAX_TIMEOUTS, STANDBY_DEADLOCK_TIMEOUT, STANDBY_LOCK_TIMEOUT, STANDBY_TIMEOUT,
    STARTUP_PACKET_TIMEOUT, STARTUP_PROGRESS_TIMEOUT, STATEMENT_TIMEOUT, TRANSACTION_TIMEOUT,
    USER_TIMEOUT,
};

pub type TimeoutHandlerProc = fn();

const N_TIMEOUTS: usize = MAX_TIMEOUTS as usize;

#[derive(Clone, Copy)]
pub enum EnableTimeoutParams {
    After { id: TimeoutId, delay_ms: i32 },
    At { id: TimeoutId, fin_time: TimestampTz },
    Every { id: TimeoutId, delay_ms: i32 },
}

#[derive(Clone, Copy)]
struct TimeoutParams {
    index: TimeoutId,
    active: bool,
    indicator: bool,
    timeout_handler: Option<TimeoutHandlerProc>,
    start_time: TimestampTz,
    fin_time: TimestampTz,
    interval_in_ms: i32,
}

struct TimeoutData {
    all_timeouts: [TimeoutParams; N_TIMEOUTS],
    // Active list ordered by fin_time then priority; ids index all_timeouts.
    num_active: usize,
    active_timeouts: [TimeoutId; N_TIMEOUTS],
}

impl TimeoutData {
    const fn new() -> Self {
        TimeoutData {
            all_timeouts: [TimeoutParams {
                index: 0,
                active: false,
                indicator: false,
                timeout_handler: None,
                start_time: 0,
                fin_time: 0,
                interval_in_ms: 0,
            }; N_TIMEOUTS],
            num_active: 0,
            active_timeouts: [0; N_TIMEOUTS],
        }
    }
}

thread_local! {
    static DATA: RefCell<TimeoutData> = const { RefCell::new(TimeoutData::new()) };
    static ALL_TIMEOUTS_INITIALIZED: Cell<bool> = const { Cell::new(false) };
    static ALARM: Cell<Option<&'static AlarmState>> = const { Cell::new(None) };
    static SIGNAL_DUE_AT: Cell<TimestampTz> = const { Cell::new(0) };
    // This backend's slot flag, shared with the timer thread.
    static POSTED: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
    // this backend's own copy of its slot's deadline (pg_clock mono ns) while that wake is of the abort class;
    // 0 = none. Read by fire_due at the end of a run: the backend fires an expired deadline itself instead of relying
    // on the timer thread getting a CPU (C's SIGALRM is delivered to the backend on expiry, no third thread).
    static DUE_NS: Cell<u64> = const { Cell::new(0) };
    // The deadline of this backend's armed wake, until it is delivered.
    static ARMED_DEADLINE: Cell<Option<Deadline>> = const { Cell::new(None) };
}

struct TimerSlot {
    // DST P2 (contract §1.3): deadlines live in pg_clock's monotonic domain
    // (law §0.3 keeps them coherent with the backend's wall-domain fin_time
    // under sim); the timer thread parks on the Waiter instead of a raw
    // Condvar, so statement/deadlock timeouts are virtual-time-driven.
    deadline: Option<Deadline>,
    latch: Option<LatchHandle>,
    posted: Arc<AtomicBool>,
    // The owner's InterruptPending: raised with `posted` so a CPU-bound
    // CHECK_FOR_INTERRUPTS reaches the drain, as C's SIGALRM preempts it.
    interrupt_flag: &'static AtomicBool,
    abort_flag: Option<&'static std::sync::atomic::AtomicU8>,
    // the nearest abort-class timeout's deadline while a nearer timeout of another class holds `deadline`
    // (idle_stats_update_timeout, armed at idle, ahead of statement_timeout). A served kernel checks only the abort
    // byte, so the nearer wake's InterruptPending is never drained there and its handler never re-arms the timer for
    // the statement timeout behind it; this second deadline raises the abort byte on time instead (C's SIGALRM
    // handler runs at delivery and reschedules at once). The owner's AbortPending byte rides with it.
    abort_at: Option<(Deadline, &'static std::sync::atomic::AtomicU8)>,
    alarm: &'static AlarmState,
}

// C's alarm_enabled and signal_pending, which its handle_sig_alarm reads and resets at delivery: here the delivery is
// the timer thread's post, so they are shared with it (D3099: a wake left armed after the last timeout went must only
// set the latch, never raise InterruptPending). Dekker pair: the owner stores `enabled` before it loads `pending` to
// ride a wake; the post stores `pending` before it loads `enabled`; both SeqCst.
struct AlarmState {
    enabled: AtomicBool,
    pending: AtomicBool,
}

#[inline]
fn alarm() -> &'static AlarmState {
    ALARM.get().unwrap_or_else(claim_alarm)
}

#[cold]
#[inline(never)]
fn claim_alarm() -> &'static AlarmState {
    let a: &'static AlarmState = Box::leak(Box::new(AlarmState { enabled: AtomicBool::new(false), pending: AtomicBool::new(false) }));
    ALARM.set(Some(a));
    a
}

fn signal_pending() -> bool {
    alarm().pending.load(Ordering::SeqCst)
}

fn set_signal_pending(v: bool) {
    alarm().pending.store(v, Ordering::SeqCst);
}

struct TimerShared {
    slots: Mutex<HashMap<i32, TimerSlot>>,
    // The timer thread's packed waiter::WakerHandle word (0 until the thread
    // publishes it). Armers unpark through this instead of a Condvar notify.
    timer_waker: AtomicU64,
}

fn timer() -> &'static TimerShared {
    static TIMER: OnceLock<&'static TimerShared> = OnceLock::new();
    TIMER.get_or_init(|| {
        let shared: &'static TimerShared = Box::leak(Box::new(TimerShared {
            slots: Mutex::new(HashMap::new()),
            timer_waker: AtomicU64::new(0),
        }));
        // PERMIT-S5 (P3 census row 16): the timer thread spawns through the
        // pgsync::thread child-registering wrapper — under the permit
        // scheduler it registers a slot (synthetic vpid, spawn-fenced) and
        // gates on its first grant, so its deadline parks drive virtual
        // time and its fire-wakes are scheduled ops instead of External
        // (OS-timed) wakes. Native arm = the std re-export, byte-identical.
        // wasm32: wasm32-wasip1 has no threads, so no timer can fire
        // asynchronously (and no SIGALRM exists either): the backend polls
        // its armed wake's deadline instead (`set_armed`), and a timed wait
        // fires an expired abort-class one (waiteventset's wasm backend).
        #[cfg(not(target_family = "wasm"))]
        pgsync::thread::Builder::new()
            .name("pg-timeout-timer".into())
            .spawn(pagegive::pinned(move || timer_thread(shared)))
            .expect("could not spawn timeout timer thread");
        shared
    })
}

fn timer_thread(shared: &'static TimerShared) -> ! {
    loop {
        // Dekker discipline (waiter::current_handle doc): publish the waker
        // handle BEFORE scanning, then scan, then park. An armer inserts its
        // slot and THEN unparks — so either its insert is seen by this scan,
        // or its unpark hits the just-published handle and the park below
        // returns immediately (unparks are latched). No arm can be lost.
        shared
            .timer_waker
            .store(waiter::current_handle().as_u64(), Ordering::SeqCst);

        let mut slots = shared.slots.lock().unwrap();
        let mut nearest: Option<Deadline> = None;
        let mut fired = false;
        for slot in slots.values_mut() {
            if let Some((dl, _)) = slot.abort_at {
                if dl.expired() {
                    post_abort(slot);
                    fired = true;
                } else if nearest.map_or(true, |n| dl < n) {
                    nearest = Some(dl);
                }
            }
            if let Some(dl) = slot.deadline {
                if dl.expired() {
                    // The kill(pid, SIGALRM) edge: post, then wake the
                    // backend — under the slots lock, so a disarm_timer /
                    // arm_timer on the owner is strictly before (no fire)
                    // or after (a spurious wake the drain tolerates); a
                    // disabled timeout can never raise the abort byte.
                    let _ = post(slot);
                    fired = true;
                } else if nearest.map_or(true, |n| dl < n) {
                    nearest = Some(dl);
                }
            }
        }
        drop(slots);
        if fired {
            continue;
        }
        match nearest {
            Some(dl) => {
                let rem = dl.remaining_ms();
                if rem > 0 {
                    // Timed park: deadline judged by the installed
                    // WaiterClock, whose now_ms delegates to pg_clock (§1.1
                    // invariant) — one timeline with `dl`.
                    let _ = waiter::park_timeout(Duration::from_millis(rem as u64));
                }
            }
            // Untimed park until the next arm (recheck cadence backstop
            // applies, as for every untimed waiter park).
            None => {
                let _ = waiter::park();
            }
        }
    }
}

/// The fire of an expired slot (the timer thread's scan, or the owner itself in fire_due), under the slots lock. false =
/// the owner's alarm was disabled: the latch only, as C's handler.
fn post(slot: &mut TimerSlot) -> bool {
    slot.deadline = None;
    slot.alarm.pending.store(false, Ordering::SeqCst);
    if !slot.alarm.enabled.load(Ordering::SeqCst) {
        slot.abort_flag = None;
        if let Some(latch) = slot.latch {
            latch::SetLatch(latch);
        }
        return false;
    }
    slot.posted.store(true, Ordering::SeqCst);
    slot.interrupt_flag.store(true, Ordering::SeqCst);
    if let Some(abort) = slot.abort_flag.take() {
        abort.fetch_or(globals::ABORT_BIT, Ordering::SeqCst);
    }
    if let Some(latch) = slot.latch {
        latch::SetLatch(latch);
    }
    true
}

fn post_abort(slot: &mut TimerSlot) {
    if let Some((_, abort)) = slot.abort_at.take() {
        slot.posted.store(true, Ordering::SeqCst);
        slot.interrupt_flag.store(true, Ordering::SeqCst);
        abort.fetch_or(globals::ABORT_BIT, Ordering::SeqCst);
        if let Some(latch) = slot.latch {
            latch::SetLatch(latch);
        }
    }
}

pub fn fire_due() -> bool {
    let due = DUE_NS.with(Cell::get);
    if due == 0 || !Deadline::at_ns(due).expired() {
        return false;
    }
    DUE_NS.with(|c| c.set(0));
    let mut slots = timer().slots.lock().unwrap();
    match slots.get_mut(&globals::MyProcPid()) {
        Some(slot) if slot.abort_flag.is_some() && slot.deadline.is_some_and(|dl| dl.expired()) => {
            post(slot)
        }
        Some(slot) if slot.abort_at.is_some_and(|(dl, _)| dl.expired()) => {
            post_abort(slot);
            true
        }
        _ => false,
    }
}

// setitimer(ITIMER_REAL, {delay, no interval}): one relative one-shot timer
// per backend, a new arm overwriting the previous.
fn abort_class(id: TimeoutId) -> bool {
    matches!(id, STATEMENT_TIMEOUT | LOCK_TIMEOUT | TRANSACTION_TIMEOUT | IDLE_IN_TRANSACTION_SESSION_TIMEOUT | IDLE_SESSION_TIMEOUT)
}

fn arm_timer(delay: Duration, abort: bool, abort_after: Option<Duration>) {
    let shared = timer();
    let posted = POSTED.with(|p| {
        p.borrow()
            .as_ref()
            .map(Arc::clone)
            .expect("timeout: InitializeTimeouts has not run in this backend")
    });
    let deadline = Deadline::after(delay);
    let abort_at = abort_after.map(Deadline::after);
    DUE_NS.with(|c| c.set(if abort { deadline.as_ns().max(1) } else { abort_at.map_or(0, |dl| dl.as_ns().max(1)) }));
    set_armed(Some(deadline));
    let mut slots = shared.slots.lock().unwrap();
    slots.insert(
        globals::MyProcPid(),
        TimerSlot {
            deadline: Some(deadline),
            latch: globals::MyLatch(),
            posted,
            interrupt_flag: globals::interrupt_pending_flag(),
            abort_flag: abort.then(globals::abort_pending_flag),
            abort_at: abort_at.map(|dl| (dl, globals::abort_pending_flag())),
            alarm: alarm(),
        },
    );
    drop(slots);
    // Insert-then-unpark (see the timer_thread Dekker note). A 0 word
    // (thread not yet published) is a Stale no-op — the thread's first scan
    // runs after publish and sees this slot.
    let _ = waiter::unpark_word(shared.timer_waker.load(Ordering::SeqCst));
}

/// The deadline of the backend's armed wake (`None`: none is). wasm32 has no timer thread to deliver it (`timer`): the
/// backend polls it there (init_small `SetDuePoll`), and ProcessTimeoutInterrupt fires it once it passed.
fn set_armed(deadline: Option<Deadline>) {
    ARMED_DEADLINE.with(|c| c.set(deadline));
    #[cfg(target_family = "wasm")]
    globals::SetDuePoll(deadline.map(|_| armed_passed as fn() -> bool));
}

#[cfg(target_family = "wasm")]
fn armed_passed() -> bool {
    ARMED_DEADLINE.with(Cell::get).is_some_and(|d| d.expired())
}

fn disable_alarm() {
    alarm().enabled.store(false, Ordering::SeqCst);
}

// A pending wake outlives the timeout it was armed for (C's spurious
// SIGALRM, which the drain tolerates); its abort class must not: retagged
// whenever the nearest timeout changes or none is left (docs/gravity/driver.md §6).
fn retag_timer(abort: bool, abort_after: Option<Duration>) {
    let shared = timer();
    if let Some(slot) = shared.slots.lock().unwrap().get_mut(&globals::MyProcPid()) {
        slot.abort_flag = abort.then(globals::abort_pending_flag);
        slot.abort_at = abort_after.map(|d| (Deadline::after(d), globals::abort_pending_flag()));
        let due = if abort { slot.deadline } else { slot.abort_at.map(|(dl, _)| dl) };
        DUE_NS.with(|c| c.set(due.map_or(0, |dl| dl.as_ns().max(1))));
    }
}

// The last active timeout went: the wake is withdrawn (C leaves the itimer
// and its handler finds nothing to do; here the wake would also raise
// InterruptPending and possibly the abort byte, so it must not arrive).
fn disarm_timer() {
    set_signal_pending(false);
    DUE_NS.with(|c| c.set(0));
    set_armed(None);
    let shared = timer();
    if let Some(slot) = shared.slots.lock().unwrap().get_mut(&globals::MyProcPid()) {
        slot.deadline = None;
        slot.abort_flag = None;
        slot.abort_at = None;
    }
}

// disable_timeout(s) left nothing active: a pending wake of the non-abort class stays, as C leaves the itimer; it finds
// the alarm disabled at its post and only sets the latch (C's handler with alarm_enabled false). An abort-class wake is
// withdrawn.
fn last_timeout_gone() {
    if !(signal_pending() && DUE_NS.with(|c| c.get()) == 0) {
        disarm_timer();
    }
}

fn enable_alarm() {
    alarm().enabled.store(true, Ordering::SeqCst);
}

fn timestamptz_plus_ms(tz: TimestampTz, ms: i32) -> TimestampTz {
    tz + ms as TimestampTz * 1000
}

fn find_active_timeout(data: &TimeoutData, id: TimeoutId) -> Option<usize> {
    (0..data.num_active).find(|&i| data.active_timeouts[i] == id)
}

fn insert_timeout(data: &mut TimeoutData, id: TimeoutId, index: usize) {
    // C elog(FATAL)s on an out-of-range index; it is an internal invariant.
    assert!(
        index <= data.num_active,
        "timeout index {index} out of range 0..{}",
        data.num_active
    );
    debug_assert!(!data.all_timeouts[id as usize].active);
    data.all_timeouts[id as usize].active = true;

    let mut i = data.num_active;
    while i > index {
        data.active_timeouts[i] = data.active_timeouts[i - 1];
        i -= 1;
    }
    data.active_timeouts[index] = id;
    data.num_active += 1;
}

fn remove_timeout_index(data: &mut TimeoutData, index: usize) {
    assert!(
        index < data.num_active,
        "timeout index {index} out of range 0..{}",
        data.num_active as isize - 1
    );
    let id = data.active_timeouts[index];
    debug_assert!(data.all_timeouts[id as usize].active);
    data.all_timeouts[id as usize].active = false;

    for i in (index + 1)..data.num_active {
        data.active_timeouts[i - 1] = data.active_timeouts[i];
    }
    data.num_active -= 1;
}

fn enable_timeout(
    data: &mut TimeoutData,
    id: TimeoutId,
    now: TimestampTz,
    fin_time: TimestampTz,
    interval_in_ms: i32,
) {
    debug_assert!(ALL_TIMEOUTS_INITIALIZED.with(|c| c.get()));
    debug_assert!(data.all_timeouts[id as usize].timeout_handler.is_some());

    if data.all_timeouts[id as usize].active {
        let pos = find_active_timeout(data, id).unwrap();
        remove_timeout_index(data, pos);
    }

    // Sort by fin_time, then priority (lower id first).
    let mut i = 0;
    while i < data.num_active {
        let old = &data.all_timeouts[data.active_timeouts[i] as usize];
        if fin_time < old.fin_time || (fin_time == old.fin_time && id < old.index) {
            break;
        }
        i += 1;
    }

    let t = &mut data.all_timeouts[id as usize];
    t.indicator = false;
    t.start_time = now;
    t.fin_time = fin_time;
    t.interval_in_ms = interval_in_ms;

    insert_timeout(data, id, i);
}

// TimestampDifference (timestamp.c): non-negative (secs, usecs).
fn timestamp_difference(start: TimestampTz, stop: TimestampTz) -> (i64, i64) {
    let diff = stop - start;
    if diff <= 0 {
        (0, 0)
    } else {
        (diff / 1_000_000, diff % 1_000_000)
    }
}

fn schedule_alarm(data: &TimeoutData, now: TimestampTz) {
    if data.num_active == 0 {
        return;
    }
    let secs;
    let mut usecs;

    // A pending wake more than 10ms overdue is assumed lost.
    if signal_pending() && now > SIGNAL_DUE_AT.with(|c| c.get()) + 10 * 1000 {
        set_signal_pending(false);
    }

    let nearest_id = data.active_timeouts[0];
    let nearest_timeout = data.all_timeouts[nearest_id as usize].fin_time;
    if now > nearest_timeout {
        set_signal_pending(false);
        secs = 0;
        usecs = 1;
    } else {
        (secs, usecs) = timestamp_difference(now, nearest_timeout);
        if secs == 0 && usecs == 0 {
            usecs = 1;
        }
    }

    enable_alarm();

    let abort_after = if abort_class(nearest_id) {
        None
    } else {
        data.active_timeouts[1..data.num_active].iter().find(|&&id| abort_class(id)).map(|&id| {
            let (s, us) = timestamp_difference(now, data.all_timeouts[id as usize].fin_time);
            Duration::new(s as u64, (us.max(1) * 1000) as u32)
        })
    };

    // C's setitimer avoidance where its early SIGALRM stays harmless: a pending wake of the non-abort class (DUE_NS 0:
    // it raises InterruptPending alone) at or before the needed time suffices while the nearest timeout is of that
    // class and no abort-class timeout waits behind it; its early drain fires nothing and re-arms at the real deadline.
    // Idle passivation re-enables at every ready-for-query: without this each statement re-armed and woke the timer thread.
    if signal_pending()
        && DUE_NS.with(|c| c.get()) == 0
        && !abort_class(nearest_id)
        && abort_after.is_none()
        && nearest_timeout >= SIGNAL_DUE_AT.with(|c| c.get())
    {
        return;
    }

    // A wake already due at exactly the needed time suffices (C keeps any
    // EARLIER wake too — its early SIGALRM is harmless there; here it would
    // raise InterruptPending and the abort byte for a timeout not yet due,
    // so an earlier wake is re-armed at the new deadline instead).
    if signal_pending() && nearest_timeout == SIGNAL_DUE_AT.with(|c| c.get()) {
        // The kept wake may now serve a different nearest timeout: its
        // abort class must follow.
        retag_timer(abort_class(nearest_id), abort_after);
        return;
    }

    SIGNAL_DUE_AT.with(|c| c.set(nearest_timeout));
    set_signal_pending(true);

    // C's setitimer FATAL branch has no analog: arming cannot fail.
    arm_timer(Duration::new(secs as u64, (usecs * 1000) as u32), abort_class(nearest_id), abort_after);
}

fn handle_sig_alarm() {
    globals::HoldInterrupts();

    latch::SetLatch(globals::MyLatch().expect("timeout drain without MyLatch"));

    set_signal_pending(false);

    if alarm().enabled.load(Ordering::SeqCst) {
        disable_alarm();

        if DATA.with(|d| d.borrow().num_active) > 0 {
            let mut now = timestamp_seams::get_current_timestamp::call();

            loop {
                // Handler runs with no borrow held (handlers re-enter).
                let fired = DATA.with(|d| {
                    let data = &mut *d.borrow_mut();
                    if data.num_active == 0
                        || now < data.all_timeouts[data.active_timeouts[0] as usize].fin_time
                    {
                        return None;
                    }
                    let id = data.active_timeouts[0];
                    remove_timeout_index(data, 0);
                    let t = &mut data.all_timeouts[id as usize];
                    t.indicator = true;
                    Some((id, t.timeout_handler))
                });
                let Some((id, handler)) = fired else {
                    break;
                };

                (handler.expect("fired timeout has a handler"))();

                // timeout.c:417 reads the params after the handler, which
                // may have re-armed this id (one-shot or a new interval).
                let (interval, fin_time) = DATA.with(|d| {
                    let t = &d.borrow().all_timeouts[id as usize];
                    (t.interval_in_ms, t.fin_time)
                });
                if interval > 0 {
                    // Anti-drift: schedule off the intended firing time.
                    let mut new_fin = timestamptz_plus_ms(fin_time, interval);
                    if new_fin < now {
                        new_fin = timestamptz_plus_ms(now, interval);
                    }
                    DATA.with(|d| enable_timeout(&mut d.borrow_mut(), id, now, new_fin, interval));
                }

                now = timestamp_seams::get_current_timestamp::call();
            }

            DATA.with(|d| schedule_alarm(&d.borrow(), now));
        }
    }

    globals::ResumeInterrupts();
}

/// Synchronous SIGALRM delivery: fires iff the timer thread posted a wake.
pub fn ProcessTimeoutInterrupt() {
    // Load-first fast path: this now also runs on every client-IO interrupt
    // check (the ProcessClientRead/WriteInterrupt SIGALRM rendering), so the
    // no-wake case must not clone the Arc.
    let posted = POSTED.with(|p| {
        p.borrow().as_ref().filter(|a| a.load(Ordering::SeqCst)).map(Arc::clone)
    });
    if let Some(posted) = posted {
        // the timer thread raises `posted`, InterruptPending and the abort byte under the slots lock; taking it
        // once here waits out a fire still storing, so the abort byte of the wake drained below is already set when
        // ProcessInterrupts consumes its cancel (and clears the byte with it) — never raised after, into the next
        // statement.
        drop(timer().slots.lock());
        if posted.swap(false, Ordering::SeqCst) {
            set_armed(None);
            handle_sig_alarm();
            return;
        }
    }
    // A kernel delivers an itimer that expired while the process was stopped
    // before the process runs again; a waiter that wakes past its deadline
    // must not get ahead of the timer thread's post.
    if ARMED_DEADLINE.with(|c| c.get()).is_some_and(|d| d.expired()) {
        take_own_expired_wake();
        handle_sig_alarm();
    }
}

fn take_own_expired_wake() {
    set_armed(None);
    let mut slots = timer().slots.lock().unwrap();
    if let Some(slot) = slots.get_mut(&globals::MyProcPid()) {
        if slot.deadline.is_some_and(|d| d.expired()) {
            slot.deadline = None;
        }
        slot.posted.store(false, Ordering::SeqCst);
    }
}

// BUG FIX (DEFAULT ON; PGRUST_TIMEOUT_INIT_RESET=0|off restores the legacy
// lost-timeout behavior for ONE TRAIN as the bisection escape, then the knob
// is removed — this is a correctness fix, not a perf arm):
// re-running InitializeTimeouts on a live connection thread (the session
// main re-init after the startup-packet/auth phase) replaces this thread's
// timer slot with `deadline: None`, destroying a still-armed wake — while
// later schedule_alarm whose nearest fin_time lands at or past the stale
// SIGNAL_DUE_AT then takes the setitimer-avoidance skip and trusts a wake
// that can no longer arrive: the timeout silently never fires until a
// schedule_alarm runs after SIGNAL_DUE_AT has passed (the "assumed lost"
// recovery). Net effect: any timeout armed within the first
// authentication_timeout seconds of a session whose deadline extends past
// that window is lost (C-divergent — C is immune because its kernel itimer
// survives InitializeTimeouts, keeping signal_pending truthful). The fix
// resets the pending-wake bookkeeping to match the destroyed slot, so the
// next schedule_alarm always arms a real wake. Witnesses: the
// reinit_keeps_long_deadlines_armable unit below (default world) + the
// timeout-e2e early-session long-timeout scenario (both engines).
fn timeout_init_reset_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("PGRUST_TIMEOUT_INIT_RESET").map_or(true, |v| {
            !(v == "0" || v.eq_ignore_ascii_case("off") || v.eq_ignore_ascii_case("false"))
        })
    })
}

pub fn InitializeTimeouts() {
    disable_alarm();
    DUE_NS.with(|c| c.set(0));
    set_armed(None);

    if timeout_init_reset_enabled() {
        set_signal_pending(false);
        SIGNAL_DUE_AT.with(|c| c.set(0));
    }

    DATA.with(|d| {
        let data = &mut *d.borrow_mut();
        data.num_active = 0;
        for (i, t) in data.all_timeouts.iter_mut().enumerate() {
            t.index = i as TimeoutId;
            t.active = false;
            t.indicator = false;
            t.timeout_handler = None;
            t.start_time = 0;
            t.fin_time = 0;
            t.interval_in_ms = 0;
        }
    });

    ALL_TIMEOUTS_INITIALIZED.with(|c| c.set(true));

    // C: pqsignal(SIGALRM, handle_sig_alarm).
    let posted = POSTED.with(|p| {
        let mut p = p.borrow_mut();
        if p.is_none() {
            *p = Some(Arc::new(AtomicBool::new(false)));
        }
        Arc::clone(p.as_ref().unwrap())
    });
    posted.store(false, Ordering::SeqCst);
    let shared = timer();
    shared.slots.lock().unwrap().insert(
        globals::MyProcPid(),
        TimerSlot {
            deadline: None,
            latch: globals::MyLatch(),
            posted,
            interrupt_flag: globals::interrupt_pending_flag(),
            abort_flag: None,
            abort_at: None,
            alarm: alarm(),
        },
    );
}

/// pgrust-only: C reclaims timeout state with the process; a thread-model
/// backend releases its timer-registry slot at task end.
pub fn forget_backend_timer_slot() {
    set_armed(None);
    DUE_NS.with(|c| c.set(0));
    timer().slots.lock().unwrap().remove(&globals::MyProcPid());
}

pub fn RegisterTimeout(id: TimeoutId, handler: TimeoutHandlerProc) -> TimeoutId {
    debug_assert!(ALL_TIMEOUTS_INITIALIZED.with(|c| c.get()));

    DATA.with(|d| {
        let data = &mut *d.borrow_mut();
        let mut id = id;
        if id >= USER_TIMEOUT {
            id = (USER_TIMEOUT..MAX_TIMEOUTS)
                .find(|&i| data.all_timeouts[i as usize].timeout_handler.is_none())
                .unwrap_or_else(|| {
                    std::panic::panic_any(Box::new(
                        PgError::new(FATAL, "cannot add more timeout reasons")
                            .with_sqlstate(ERRCODE_CONFIGURATION_LIMIT_EXCEEDED),
                    ))
                });
        }
        debug_assert!(data.all_timeouts[id as usize].timeout_handler.is_none());
        data.all_timeouts[id as usize].timeout_handler = Some(handler);
        id
    })
}

pub fn reschedule_timeouts() {
    if !ALL_TIMEOUTS_INITIALIZED.with(|c| c.get()) {
        return;
    }
    disable_alarm();
    if DATA.with(|d| d.borrow().num_active) > 0 {
        let now = timestamp_seams::get_current_timestamp::call();
        DATA.with(|d| schedule_alarm(&d.borrow(), now));
    }
}

pub fn enable_timeout_after(id: TimeoutId, delay_ms: i32) {
    disable_alarm();
    let now = timestamp_seams::get_current_timestamp::call();
    let fin_time = timestamptz_plus_ms(now, delay_ms);
    DATA.with(|d| {
        let data = &mut *d.borrow_mut();
        enable_timeout(data, id, now, fin_time, 0);
        schedule_alarm(data, now);
    });
}

pub fn enable_timeout_every(id: TimeoutId, fin_time: TimestampTz, delay_ms: i32) {
    disable_alarm();
    let now = timestamp_seams::get_current_timestamp::call();
    DATA.with(|d| {
        let data = &mut *d.borrow_mut();
        enable_timeout(data, id, now, fin_time, delay_ms);
        schedule_alarm(data, now);
    });
}

pub fn enable_timeout_at(id: TimeoutId, fin_time: TimestampTz) {
    disable_alarm();
    let now = timestamp_seams::get_current_timestamp::call();
    DATA.with(|d| {
        let data = &mut *d.borrow_mut();
        enable_timeout(data, id, now, fin_time, 0);
        schedule_alarm(data, now);
    });
}

pub fn enable_timeouts(timeouts: &[EnableTimeoutParams]) {
    disable_alarm();
    let now = timestamp_seams::get_current_timestamp::call();
    DATA.with(|d| {
        let data = &mut *d.borrow_mut();
        for t in timeouts {
            match *t {
                EnableTimeoutParams::After { id, delay_ms } => {
                    enable_timeout(data, id, now, timestamptz_plus_ms(now, delay_ms), 0);
                }
                EnableTimeoutParams::At { id, fin_time } => {
                    enable_timeout(data, id, now, fin_time, 0);
                }
                EnableTimeoutParams::Every { id, delay_ms } => {
                    enable_timeout(data, id, now, timestamptz_plus_ms(now, delay_ms), delay_ms);
                }
            }
        }
        schedule_alarm(data, now);
    });
}

pub fn disable_timeout(id: TimeoutId, keep_indicator: bool) {
    debug_assert!(ALL_TIMEOUTS_INITIALIZED.with(|c| c.get()));
    debug_assert!(DATA.with(|d| d.borrow().all_timeouts[id as usize].timeout_handler.is_some()));

    disable_alarm();
    DATA.with(|d| {
        let data = &mut *d.borrow_mut();
        if data.all_timeouts[id as usize].active {
            let pos = find_active_timeout(data, id).unwrap();
            remove_timeout_index(data, pos);
        }
        if !keep_indicator {
            data.all_timeouts[id as usize].indicator = false;
        }
    });
    if DATA.with(|d| d.borrow().num_active) > 0 {
        let now = timestamp_seams::get_current_timestamp::call();
        DATA.with(|d| schedule_alarm(&d.borrow(), now));
    } else {
        last_timeout_gone();
    }
}

pub fn disable_timeouts(timeouts: &[DisableTimeoutParams]) {
    debug_assert!(ALL_TIMEOUTS_INITIALIZED.with(|c| c.get()));

    disable_alarm();
    DATA.with(|d| {
        let data = &mut *d.borrow_mut();
        for t in timeouts {
            debug_assert!(data.all_timeouts[t.id as usize].timeout_handler.is_some());
            if data.all_timeouts[t.id as usize].active {
                let pos = find_active_timeout(data, t.id).unwrap();
                remove_timeout_index(data, pos);
            }
            if !t.keep_indicator {
                data.all_timeouts[t.id as usize].indicator = false;
            }
        }
    });
    if DATA.with(|d| d.borrow().num_active) > 0 {
        let now = timestamp_seams::get_current_timestamp::call();
        DATA.with(|d| schedule_alarm(&d.borrow(), now));
    } else {
        last_timeout_gone();
    }
}

pub fn disable_all_timeouts(keep_indicators: bool) {
    disable_alarm();
    disarm_timer();

    DATA.with(|d| {
        let data = &mut *d.borrow_mut();
        data.num_active = 0;
        for t in data.all_timeouts.iter_mut() {
            t.active = false;
            if !keep_indicators {
                t.indicator = false;
            }
        }
    });
}

pub fn get_timeout_active(id: TimeoutId) -> bool {
    DATA.with(|d| d.borrow().all_timeouts[id as usize].active)
}

pub fn get_timeout_indicator(id: TimeoutId, reset_indicator: bool) -> bool {
    DATA.with(|d| {
        let data = &mut *d.borrow_mut();
        if data.all_timeouts[id as usize].indicator {
            if reset_indicator {
                data.all_timeouts[id as usize].indicator = false;
            }
            return true;
        }
        false
    })
}

pub fn get_timeout_start_time(id: TimeoutId) -> TimestampTz {
    DATA.with(|d| d.borrow().all_timeouts[id as usize].start_time)
}

pub fn get_timeout_finish_time(id: TimeoutId) -> TimestampTz {
    DATA.with(|d| d.borrow().all_timeouts[id as usize].fin_time)
}

pub fn init_seams() {
    use timeout_seams as s;
    s::initialize_timeouts::set(InitializeTimeouts);
    s::register_timeout::set(RegisterTimeout);
    s::reschedule_timeouts::set(|| {
        reschedule_timeouts();
        Ok(())
    });
    s::enable_timeout_after::set(|id, delay_ms| {
        enable_timeout_after(id, delay_ms);
        Ok(())
    });
    s::enable_timeouts::set(|timeouts| {
        let params: Vec<EnableTimeoutParams> = timeouts
            .iter()
            .map(|t| EnableTimeoutParams::After {
                id: t.id,
                delay_ms: t.delay_ms,
            })
            .collect();
        enable_timeouts(&params);
        Ok(())
    });
    s::disable_timeout::set(|id, keep_indicator| {
        disable_timeout(id, keep_indicator);
        Ok(())
    });
    s::disable_timeouts::set(disable_timeouts);
    s::disable_all_timeouts::set(|keep_indicators| {
        disable_all_timeouts(keep_indicators);
        Ok(())
    });
    s::get_timeout_active::set(get_timeout_active);
    s::get_timeout_indicator::set(get_timeout_indicator);
    s::get_timeout_start_time::set(get_timeout_start_time);
    s::get_timeout_finish_time::set(get_timeout_finish_time);
    s::process_timeout_interrupt::set(ProcessTimeoutInterrupt);
    s::fire_due::set(fire_due);
    s::forget_backend_timer_slot::set(forget_backend_timer_slot);
}

#[cfg(test)]
mod tests;
