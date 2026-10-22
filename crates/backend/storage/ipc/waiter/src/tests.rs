use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::*;

// NOTE: PGRUST_TEST_DROP_WAKE_1IN / recheck-cadence env vars are read once
// per process; these tests assume the default environment (no injection).
// The cadence default (1000ms) bounds park() AND park_timeout laps
// (GL-RECWAKE-1); every sub-second timed park below is unaffected
// (min(timeout, cadence) picks the timeout), and untimed parks are bounded
// by delivering a real unpark.

#[test]
fn unpark_then_park_is_wake_before_park() {
    let h = current_handle();
    assert_eq!(unpark(h), Unparked::Pending);
    // The latched notification is consumed by the next park.
    assert_eq!(park(), ParkResult::Notified);
    // And exactly once.
    assert_eq!(
        park_timeout(Duration::from_millis(10)),
        ParkResult::TimedOut
    );
}

#[test]
fn cross_thread_unpark_wakes_parked_thread() {
    let (tx, rx) = std::sync::mpsc::channel();
    let woke = Arc::new(AtomicBool::new(false));
    let woke2 = Arc::clone(&woke);
    let t = std::thread::spawn(move || {
        tx.send(current_handle()).unwrap();
        // Cadence may fire before the unpark; loop as real callers do.
        loop {
            match park() {
                ParkResult::Notified => break,
                ParkResult::Recheck => continue,
                ParkResult::TimedOut => unreachable!(),
            }
        }
        woke2.store(true, Ordering::SeqCst);
    });
    let h = rx.recv().unwrap();
    // Deliver (or latch, if the thread has not parked yet — both are wins).
    assert_ne!(unpark(h), Unparked::Stale);
    t.join().unwrap();
    assert!(woke.load(Ordering::SeqCst));
}

#[test]
fn park_timeout_expires() {
    let start = std::time::Instant::now();
    assert_eq!(
        park_timeout(Duration::from_millis(30)),
        ParkResult::TimedOut
    );
    assert!(start.elapsed() >= Duration::from_millis(25));
}

// GL-RECWAKE-1: timed parks are ALSO bounded by the recheck cadence. A
// dropped cross-thread unpark leaves the caller's predicate flagged but the
// slot un-notified; a minutes-class caller timeout (the checkpointer main
// lap) must not be the only backstop. Driven through Slot::park_core with a
// virtual clock: deterministic, no wall-time sleeps.
#[test]
fn timed_park_lapped_by_cadence_returns_recheck() {
    static CLK: clock::virtual_time::VirtualClock = clock::virtual_time::VirtualClock::new();
    let slot: &'static Slot = Box::leak(Box::new(Slot::new()));
    slot.issue_token();
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = Arc::clone(&stop);
    let ticker = std::thread::spawn(move || {
        while !stop2.load(Ordering::SeqCst) {
            CLK.advance(100);
            std::thread::yield_now();
        }
    });
    // Caller deadline far beyond any reachable clock, cadence 250ms: the
    // cadence lap fires FIRST and reports Recheck (re-test + re-park), never
    // sleeping out the deadline. The deadline must be UNLAPPABLE, not merely
    // large: the ticker free-runs, so if this thread is descheduled while
    // parked the fake clock can advance arbitrarily far before the single
    // coalesced wakeup classifies the park — a 5000ms deadline was lapped
    // under pod contention and misreported TimedOut (CI flake, 2026-08-04).
    assert_eq!(
        slot.park_core(Some(i64::MAX / 4), Some(250), &CLK),
        ParkResult::Recheck
    );
    // Caller deadline shorter than the cadence: TimedOut, exactly as before.
    assert_eq!(
        slot.park_core(Some(200), Some(250), &CLK),
        ParkResult::TimedOut
    );
    // Untimed park with a cadence: Recheck (the pre-existing contract).
    assert_eq!(slot.park_core(None, Some(250), &CLK), ParkResult::Recheck);
    stop.store(true, Ordering::SeqCst);
    ticker.join().unwrap();
}

#[test]
fn handle_goes_stale_on_owner_death() {
    let (tx, rx) = std::sync::mpsc::channel();
    let t = std::thread::spawn(move || {
        tx.send(current_handle()).unwrap();
    });
    let h = rx.recv().unwrap();
    t.join().unwrap();
    // Owner is dead: the token was bumped at retire; the handle is poison.
    assert_eq!(unpark(h), Unparked::Stale);
}

#[test]
fn slot_reuse_cannot_hijack_new_owner() {
    // Thread A dies; thread B may reuse A's slot. A's stale handle must not
    // deliver a wake into B's incarnation.
    let (tx, rx) = std::sync::mpsc::channel();
    let t = std::thread::spawn(move || {
        tx.send(current_handle()).unwrap();
    });
    let stale = rx.recv().unwrap();
    t.join().unwrap();

    let (tx2, rx2) = std::sync::mpsc::channel();
    let t2 = std::thread::spawn(move || {
        let mine = current_handle();
        tx2.send(mine).unwrap();
        // The stale unpark below must NOT satisfy this park.
        loop {
            match park() {
                ParkResult::Notified => break,
                ParkResult::Recheck => continue,
                ParkResult::TimedOut => unreachable!(),
            }
        }
    });
    let fresh = rx2.recv().unwrap();
    assert_eq!(unpark(stale), Unparked::Stale);
    // Only the fresh handle wakes B.
    while unpark(fresh) == Unparked::Pending {
        // B may not have parked yet; Pending is already sufficient to
        // satisfy its next park.
        break;
    }
    t2.join().unwrap();
}

#[test]
fn reissue_invalidates_outstanding_handles_and_pending_notify() {
    let (tx, hrx) = std::sync::mpsc::channel();
    let (donetx, donerx) = std::sync::mpsc::channel();
    let t = std::thread::spawn(move || {
        let old = current_handle();
        tx.send(old).unwrap();
        // Latch a notification aimed at the OLD incarnation.
        donerx.recv().unwrap();
        reissue_current_token();
        let fresh = current_handle();
        assert_ne!(old, fresh);
        // The latched notify from the old incarnation was dropped.
        assert_eq!(
            park_timeout(Duration::from_millis(10)),
            ParkResult::TimedOut
        );
        fresh
    });
    let old = hrx.recv().unwrap();
    assert_eq!(unpark(old), Unparked::Pending);
    donetx.send(()).unwrap();
    let fresh = t.join().unwrap();
    // And the old handle is stale for good.
    assert_eq!(unpark(old), Unparked::Stale);
    let _ = fresh;
}

#[test]
fn fd_park_wake_writes_pipe() {
    let rfd = ensure_wake_pipe().unwrap();
    let h = current_handle();
    assert!(begin_fd_park());
    // Simulated cross-thread unpark while fd-parked (same thread here: the
    // slot is in ParkedFd, so the wake routes through the pipe).
    assert_eq!(unpark(h), Unparked::Delivered);
    // The pipe is readable now.
    let mut buf = [0u8; 8];
    // SAFETY: read from our own live pipe fd.
    let n = unsafe { libc::read(rfd, buf.as_mut_ptr().cast(), buf.len()) };
    assert_eq!(n, 1);
    end_fd_park();
    // The notification is consumed by end_fd_park; a fresh park times out.
    assert_eq!(
        park_timeout(Duration::from_millis(10)),
        ParkResult::TimedOut
    );
}

#[test]
fn begin_fd_park_reports_pending_notify() {
    ensure_wake_pipe().unwrap();
    let h = current_handle();
    assert_eq!(unpark(h), Unparked::Pending);
    // A pending notify short-circuits the fd park: poll, don't block.
    assert!(!begin_fd_park());
}

#[test]
fn describe_word_states() {
    assert_eq!(describe_word(0), "MISSING");
    let h = current_handle();
    assert!(describe_word(h.as_u64()).starts_with("waiter:slot="));
    // A stale token renders as STALE.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        tx.send(current_handle()).unwrap();
    })
    .join()
    .unwrap();
    let dead = rx.recv().unwrap();
    assert!(describe_word(dead.as_u64()).starts_with("STALE("));
}

#[test]
fn handle_word_round_trip() {
    let h = current_handle();
    assert_eq!(WakerHandle::from_u64(h.as_u64()), Some(h));
    assert_eq!(WakerHandle::from_u64(0), None);
}

// -- virtual time -----------------------------------------------------------

static VCLOCK: clock::virtual_time::VirtualClock = clock::virtual_time::VirtualClock::new();

/// One process-wide provider install: this test owns it (other tests in this
/// binary use only unpark-driven or real-short-timeout parks, which behave
/// identically under the virtual provider only if it were installed first —
/// so this test is `ignored` by default and run alone by the crate's CI
/// lane: `cargo test -p waiter -- --ignored virtual_time_drives_timeouts`.
#[test]
#[ignore]
fn virtual_time_drives_timeouts() {
    clock::install(&VCLOCK);
    let done = Arc::new(AtomicU64::new(0));
    let done2 = Arc::clone(&done);
    let t = std::thread::spawn(move || {
        // Timed parks lap at the recheck cadence (GL-RECWAKE-1): re-park for
        // the remainder as real callers do, judging the deadline on the
        // installed (virtual) clock.
        let deadline = now_ms() + 5_000;
        let r = loop {
            let rem = deadline - now_ms();
            if rem <= 0 {
                break ParkResult::TimedOut;
            }
            match park_timeout(Duration::from_millis(rem as u64)) {
                ParkResult::Recheck => continue,
                other => break other,
            }
        };
        done2.store(1 + (r == ParkResult::TimedOut) as u64, Ordering::SeqCst);
    });
    // Real time passing does nothing; virtual time expires the park.
    while done.load(Ordering::SeqCst) == 0 {
        VCLOCK.advance(1_000);
        std::thread::yield_now();
    }
    t.join().unwrap();
    assert_eq!(done.load(Ordering::SeqCst), 2, "park must report TimedOut");
    assert!(VCLOCK.now() >= 5_000);
}

// -- IoToken ------------------------------------------------------------------

#[test]
fn io_token_multi_registrant_unpark_all() {
    let token = Arc::new(io::IoToken::new(7, 42));
    assert_eq!(token.ring_id(), 7);
    assert_eq!(token.cqe_id(), 42);
    let mut threads = Vec::new();
    for _ in 0..3 {
        let token = Arc::clone(&token);
        threads.push(std::thread::spawn(move || {
            match token.register(current_handle()) {
                io::IoRegister::AlreadyCompleted => return,
                io::IoRegister::Registered => {}
            }
            while !token.is_completed() {
                let _ = park();
            }
        }));
    }
    // Completer is any thread; delivers to everyone registered so far.
    while !token.is_completed() {
        token.complete();
    }
    for t in threads {
        t.join().unwrap();
    }
    // Idempotent: a second complete delivers nothing.
    assert_eq!(token.complete(), 0);
}

#[test]
fn io_token_register_after_complete_fast_path() {
    let token = io::IoToken::new(0, 1);
    assert_eq!(token.complete_with(|_| {}), 0); // nothing registered yet
    assert!(token.is_completed());
    assert_eq!(
        token.register(current_handle()),
        io::IoRegister::AlreadyCompleted
    );
}

#[test]
fn io_token_completer_is_registrant() {
    let token = io::IoToken::new(1, 2);
    assert_eq!(token.register(current_handle()), io::IoRegister::Registered);
    // Completing our own registered token latches a self-notify.
    assert_eq!(token.complete(), 1);
    assert!(token.is_completed());
    assert_eq!(park(), ParkResult::Notified);
}

// -- wait_with (the §2.9 M1 wait protocol) -------------------------------------

#[test]
fn io_wait_with_completed_fast_path_never_parks() {
    let token = io::IoToken::new(0, 3);
    token.complete();
    let outcome = token.wait_with(
        current_handle(),
        || panic!("must not park on a completed token"),
        || panic!("must not probe state on the fast path"),
        || panic!("must not reap on the fast path"),
    );
    assert_eq!(outcome, io::IoWaitOutcome::AlreadyCompleted);
}

#[test]
fn io_wait_with_cross_thread_completion() {
    let token = Arc::new(io::IoToken::new(2, 4));
    let completer = {
        let token = Arc::clone(&token);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            token.complete();
        })
    };
    let outcome = token.wait_with(
        current_handle(),
        park,
        // A cadence recheck may race the completer; the state probe says
        // "still pending" so the protocol re-checks the token first (which
        // wins as soon as complete() lands) — never a false StateSettled.
        || false,
        || {
            // Degraded reap: legal if a recheck fired before the completer
            // ran; completion is then observed by waiting here.
            while !token.is_completed() {
                std::thread::yield_now();
            }
        },
    );
    assert!(
        matches!(outcome, io::IoWaitOutcome::Completed | io::IoWaitOutcome::Reaped),
        "unexpected outcome {outcome:?}"
    );
    completer.join().unwrap();
    assert!(token.is_completed());
}

#[test]
fn io_wait_with_spurious_notify_retests_and_reparks() {
    let token = Arc::new(io::IoToken::new(3, 5));
    let parks = AtomicU64::new(0);
    let token2 = Arc::clone(&token);
    let outcome = token.wait_with(
        current_handle(),
        || {
            match parks.fetch_add(1, Ordering::Relaxed) {
                // First park: spurious notify (no completion behind it) —
                // the loop must re-test the predicate and re-park.
                0 => ParkResult::Notified,
                // Second park: the completion lands, then the wake.
                _ => {
                    token2.complete();
                    ParkResult::Notified
                }
            }
        },
        || panic!("state probe unreachable: parks only return Notified"),
        || panic!("reap unreachable"),
    );
    assert_eq!(outcome, io::IoWaitOutcome::Completed);
    assert!(parks.load(Ordering::Relaxed) >= 2, "spurious notify must re-park");
}

#[test]
fn io_wait_with_recheck_backstop_catches_lost_completion() {
    // The lost-completion shape: IO state advanced but the token complete
    // (and so the unpark) was dropped. The cadence recheck must recover via
    // the authoritative state probe, without a blocking reap.
    let token = io::IoToken::new(4, 6);
    let state_done = AtomicBool::new(true); // state settled, wake lost
    let outcome = token.wait_with(
        current_handle(),
        || ParkResult::Recheck, // cadence fires
        || state_done.load(Ordering::Relaxed),
        || panic!("state settled: must not degrade to a reap"),
    );
    assert_eq!(outcome, io::IoWaitOutcome::StateSettled);
}

#[test]
fn io_wait_with_degrades_to_blocking_reap_when_owner_never_reaps() {
    // Genuinely pending after a full cadence (owner parked idle / wedged):
    // the waiter must drive the ring home itself.
    let token = io::IoToken::new(5, 7);
    let reaped = AtomicBool::new(false);
    let outcome = token.wait_with(
        current_handle(),
        || ParkResult::Recheck,
        || false,
        || reaped.store(true, Ordering::Relaxed),
    );
    assert_eq!(outcome, io::IoWaitOutcome::Reaped);
    assert!(reaped.load(Ordering::Relaxed));
}

// -- signal-route unpark (fp-ipc-b1#1) ---------------------------------------
//
// The postmaster's sigaction handlers reach SetLatch -> unpark; C's SetLatch
// is handler-safe (latch.c:290), so the route must never block on the slot
// mutex the interrupted thread itself may hold.

#[cfg(not(target_family = "wasm"))]
mod signal_route {
    use super::*;

    static HANDLE_WORD: AtomicU64 = AtomicU64::new(0);
    static HANDLER_RESULT: AtomicU64 = AtomicU64::new(u64::MAX);

    extern "C" fn on_signal(_sig: i32) {
        let r = match unpark_word_from_signal(HANDLE_WORD.load(Ordering::SeqCst)) {
            Unparked::Delivered => 0,
            Unparked::Pending => 1,
            Unparked::Stale => 2,
        };
        HANDLER_RESULT.store(r, Ordering::SeqCst);
    }

    fn install(handler: usize) -> libc::sigaction {
        // SAFETY: standard sigaction install/restore on a test-private signal.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = handler;
            libc::sigfillset(&mut sa.sa_mask);
            sa.sa_flags = libc::SA_RESTART;
            let mut old: libc::sigaction = std::mem::zeroed();
            libc::sigaction(libc::SIGUSR2, &sa, &mut old);
            old
        }
    }

    /// The worst point: the signal lands while THIS thread holds its own
    /// slot mutex (the begin_fd_park / issue_token window). The blocking
    /// unpark would self-deadlock here; the signal route must return, leave
    /// the wake latched, and have written the self-pipe byte.
    #[test]
    fn delivered_while_this_thread_holds_its_slot_mutex() {
        ensure_wake_pipe().expect("wake pipe");
        HANDLE_WORD.store(current_handle().as_u64(), Ordering::SeqCst);
        HANDLER_RESULT.store(u64::MAX, Ordering::SeqCst);
        let old = install(on_signal as extern "C" fn(i32) as usize);

        let guard = global::lock_current_slot_for_test();
        // SAFETY: raise(2) delivers synchronously to the calling thread.
        unsafe { libc::raise(libc::SIGUSR2) };
        drop(guard);
        // SAFETY: restore the previous disposition.
        unsafe { libc::sigaction(libc::SIGUSR2, &old, std::ptr::null_mut()) };

        assert_eq!(HANDLER_RESULT.load(Ordering::SeqCst), 0, "handler did not run to Delivered");
        // The byte is in the pipe (a kernel block would have ended)...
        let mut b = [0u8; 1];
        // SAFETY: 1-byte read from this thread's nonblocking wake pipe.
        let n = unsafe { libc::read(wake_read_fd(), b.as_mut_ptr().cast(), 1) };
        assert_eq!(n, 1, "no wake byte in the self-pipe");
        // ...and the notification is latched for the next park.
        assert!(!begin_fd_park(), "signal wake not latched");
        assert_eq!(park_timeout(Duration::from_millis(0)), ParkResult::TimedOut);
    }

    /// A peer holding the target slot's mutex (an unpark in flight) must not
    /// block the signal route either; the owner's next park sees the wake.
    #[test]
    fn does_not_block_on_a_mutex_held_by_another_thread() {
        let (tx, rx) = std::sync::mpsc::channel();
        let held = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let (held2, release2) = (Arc::clone(&held), Arc::clone(&release));
        let t = std::thread::spawn(move || {
            tx.send(current_handle()).unwrap();
            let g = global::lock_current_slot_for_test();
            held2.store(true, Ordering::SeqCst);
            while !release2.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
            drop(g);
            park()
        });
        let h = rx.recv().unwrap();
        while !held.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        let start = std::time::Instant::now();
        assert_eq!(unpark_word_from_signal(h.as_u64()), Unparked::Delivered);
        assert!(start.elapsed() < Duration::from_millis(200), "signal route blocked");
        release.store(true, Ordering::SeqCst);
        assert_eq!(t.join().unwrap(), ParkResult::Notified);
    }

    /// Uncontended, the route is the ordinary transition (Pending latches,
    /// the next park consumes it exactly once) and stale handles are no-ops.
    #[test]
    fn uncontended_is_the_ordinary_transition() {
        let h = current_handle();
        assert_eq!(unpark_word_from_signal(h.as_u64()), Unparked::Pending);
        assert_eq!(park(), ParkResult::Notified);
        assert_eq!(park_timeout(Duration::from_millis(5)), ParkResult::TimedOut);
        assert_eq!(unpark_word_from_signal(0), Unparked::Stale);
        let stale = WakerHandle::new(h.unpack().0, h.unpack().1.wrapping_add(7));
        assert_eq!(unpark_word_from_signal(stale.as_u64()), Unparked::Stale);
    }
}

// fp-ipc-b1#2: C's latch wait infrastructure (latch.c InitializeLatchWaitSet)
// is per-process with no owner cap short of MAX_BACKENDS (262143); the waiter
// slab behind it must scale with the configured backend count instead of
// failing InitializeWaitEventSupport (ensure_wake_pipe EAGAIN) for the
// 8193rd concurrent owner.
#[test]
fn waiter_slab_scales_with_configured_backends() {
    // ipci sizes this from MaxLivePostmasterChildren() + 1 at shmem init.
    WaiterSlabInit(64);
    // 32 slots stay free for the other tests running in this process.
    let want = WAITER_CAP + 32;
    let mut got = Vec::with_capacity(want);
    while got.len() < want {
        match global::try_allocate_slot() {
            Some(idx) => got.push(idx),
            None => break,
        }
    }
    let high = got.iter().copied().max().unwrap_or(0);
    let ext_idx = got.iter().copied().find(|&i| i >= WAITER_CAP);
    // Handles into the extension route exactly like boot-slab handles.
    let routed = ext_idx.map(|idx| {
        let h = WakerHandle::new(idx, 0);
        (unpark(h), describe_word(h.as_u64()))
    });
    for idx in got.iter().copied() {
        global::release_slot_for_test(idx);
    }
    assert_eq!(got.len(), want, "slab exhausted after {} slots", got.len());
    assert!(high >= WAITER_CAP, "high water {high} never left the boot slab");
    let (unparked, described) = routed.expect("no extension slot allocated");
    assert_eq!(unparked, Unparked::Pending);
    assert_eq!(
        described,
        format!("waiter:slot={} tok=0 state=notified", ext_idx.unwrap())
    );
}

/// D4484: `Slot::owner_token` (current_handle's lock-free read) is the token under the mutex through a slot's whole
/// life (issue, reissue at a pooled-thread reuse, retire, the next owner's issue), and `current_handle` publishes the
/// same handle the locked read did: an unpark through it reaches this thread, one through the pre-reissue handle is
/// Stale.
#[test]
fn owner_token_is_the_locked_token_through_a_slot_life() {
    let slot: &'static Slot = Box::leak(Box::new(Slot::new()));
    let locked = |s: &Slot| s.lock().token;
    assert_eq!(slot.owner_token(), locked(slot));
    let t = slot.issue_token();
    assert_eq!(slot.owner_token(), t);
    let t2 = slot.reissue_token();
    assert_ne!(t2, t);
    assert_eq!(slot.owner_token(), t2);
    let _ = slot.retire_token();
    assert_eq!(slot.owner_token(), locked(slot));
    let t3 = slot.issue_token();
    assert_eq!(slot.owner_token(), t3);
    assert_ne!(t3, t2);

    let before = current_handle();
    assert_eq!(current_handle(), before, "the handle is stable between reissues");
    reissue_current_token();
    let after = current_handle();
    assert_ne!(after, before, "a reissue publishes a fresh handle");
    assert_eq!(unpark(before), Unparked::Stale, "the old incarnation's handle no longer wakes this thread");
    assert_eq!(unpark(after), Unparked::Pending);
    assert_eq!(park(), ParkResult::Notified);
}
