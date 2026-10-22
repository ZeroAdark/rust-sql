//! D4812 (ii): a statement the memory watchdog canceled frees its memory in its backend's error recovery, after the
//! report. Under writeback pressure the report's write(2) to server.log waited seconds, and the free waited with it. Here
//! fd 2 is a pipe nobody reads for `HOLD_MS`, full; the statement is condemned, its 53200 raised and the statement ended
//! as `run_one_iteration` ends it; and `error_recovery` must reach the steps after its report within `STALL_MS`.
//! Meanwhile another thread's line goes to the sink itself, as before: it waits for the sink (the defer is the recovering
//! thread's only). In a child process of this test binary (fd 2 is the process's); named cancel_* for the crate tests'
//! serial tail (wall-clock readings).
#![cfg(target_os = "linux")]

use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering::SeqCst};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::main_loop::{error_recovery, LoopState};

const CHILD: &str = "TCOP_D4812_CHILD";
const NAME: &str = "d4812_tests::cancel_d4812_a_condemned_statements_free_never_waits_on_the_log_sink";
/// How long nobody reads the sink.
const HOLD_MS: u64 = 1500;
/// The longest the recovery may take to reach its free while the sink blocks: on trunk it waits the whole hold.
const STALL_MS: u64 = 1000;
/// Another thread's line waits for the sink (synchronous, as before): at least this long.
const WAITED_MS: u64 = 300;

static T0: OnceLock<Instant> = OnceLock::new();
/// When the recovery reached the step after its report (ms from `T0`; `u64::MAX` = not reached).
static AFTER_REPORT_MS: AtomicU64 = AtomicU64::new(u64::MAX);
/// The recovering thread's own line after its report, and another thread's line meanwhile: how long each write took.
static OWN_LINE_MS: AtomicU64 = AtomicU64::new(u64::MAX);
static OTHER_LINE_MS: AtomicU64 = AtomicU64::new(u64::MAX);
static OTHER: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);
/// The fixtures' `disable_all_timeouts`, which the step after the report calls on.
static AFTER: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());

fn ms_since(t: Instant) -> u64 {
    t.elapsed().as_millis() as u64
}

/// `disable_all_timeouts`, the recovery's first step after its report: marks the moment, writes a line on this thread,
/// starts another thread's, then goes on to the fixtures' own.
fn after_report(keep_indicators: bool) -> types_error::PgResult<()> {
    AFTER_REPORT_MS.store(ms_since(*T0.get().unwrap()), SeqCst);
    let other = std::thread::spawn(|| {
        let t = Instant::now();
        let _ = elog::elog(types_error::LOG, "d4812 witness: another thread's line");
        OTHER_LINE_MS.store(ms_since(t), SeqCst);
    });
    *OTHER.lock().unwrap() = Some(other);
    let t = Instant::now();
    let _ = elog::elog(types_error::LOG, "d4812 witness: the condemned backend's line before its free");
    OWN_LINE_MS.store(ms_since(t), SeqCst);
    // SAFETY: only `child` stores here, from `disable_all_timeouts::wrap`'s answer.
    let f: fn(bool) -> types_error::PgResult<()> = unsafe { std::mem::transmute(AFTER.load(SeqCst)) };
    f(keep_indicators)
}

/// Fill the pipe behind fd 2 to its last byte through a non-blocking open of its own (fd 2 itself stays blocking).
fn fill_fd2() {
    // SAFETY: a fresh descriptor on the pipe fd 2 names; closed below.
    let fd = unsafe { libc::open(c"/proc/self/fd/2".as_ptr(), libc::O_WRONLY | libc::O_NONBLOCK | libc::O_CLOEXEC) };
    assert!(fd >= 0, "reopen fd 2's pipe");
    let page = [0u8; 4096];
    for len in [page.len(), 1] {
        // SAFETY: writes from a live buffer; a full pipe answers EAGAIN.
        while unsafe { libc::write(fd, page.as_ptr().cast(), len) } > 0 {}
    }
    // SAFETY: our own descriptor.
    unsafe { libc::close(fd) };
}

fn child() {
    use mcx::governor;
    crate::session_tests::install_fixtures();
    elog::config::set_where_to_send_output(types_dest::CommandDest::None);
    AFTER.store(timeout_seams::disable_all_timeouts::wrap(after_report) as *mut (), SeqCst);

    // the statement: the watchdog's cancel on it, its 53200 raised, the statement ended (run_one_iteration's order)
    governor::register(4012, 40120);
    let ctx = mcx::MemoryContext::new("d4812 condemned");
    governor::statement_start(0, true);
    ctx.mcx().alloc_uninit_bytes(core::alloc::Layout::from_size_align(64 << 20, 8).unwrap()).unwrap();
    let v = governor::cancel_largest_victim(90, 32 << 20).expect("the statement is chosen");
    assert_eq!(v.backend, Some((4012, 40120)), "{v:?}");
    let err = governor::take_watchdog_cancel().expect("its 53200");
    governor::statement_end();
    // the memory threads' log drainer, as the memory runtime starts it
    memruntime::memlog::start();

    // fd 2 becomes a pipe, full, read only after the hold
    let mut p = [0i32; 2];
    // SAFETY: plain descriptor calls; the saved fd 2 is put back before any assertion.
    let saved = unsafe {
        assert_eq!(libc::pipe2(p.as_mut_ptr(), libc::O_CLOEXEC), 0);
        let saved = libc::dup(2);
        assert!(saved >= 0);
        assert_eq!(libc::dup2(p[1], 2), 2);
        libc::close(p[1]);
        saved
    };
    fill_fd2();
    let stop = Arc::new(AtomicBool::new(false));
    let out = Arc::new(Mutex::new(Vec::new()));
    let t0 = *T0.get_or_init(Instant::now);
    let reader = {
        let (stop, out, fd) = (stop.clone(), out.clone(), p[0]);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(HOLD_MS));
            let mut buf = vec![0u8; 64 << 10];
            while !stop.load(SeqCst) {
                let mut pf = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
                // SAFETY: one pollfd on the stack; reads into a live buffer of its own length.
                if unsafe { libc::poll(&mut pf, 1, 5) } > 0 {
                    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
                    if n > 0 {
                        out.lock().unwrap().extend_from_slice(&buf[..n as usize]);
                    }
                }
            }
        })
    };

    let mut state = LoopState {
        send_ready_for_query: true,
        idle_in_transaction_timeout_enabled: false,
        idle_session_timeout_enabled: false,
        idle_passivate_timeout_enabled: false,
        log_defer_before: None,
    };
    let r = error_recovery(&err, &mut state, true);
    let took = ms_since(t0);
    // this thread's writer after the free: none again
    let writer_after = elog::set_server_log_defer(None).is_some();
    if let Some(other) = OTHER.lock().unwrap().take() {
        let _ = other.join();
    }
    let drained = memruntime::memlog::drain(Duration::from_millis(memruntime::memlog::SHUTDOWN_WAIT_MS));
    let has = |needle: &str| out.lock().unwrap().windows(needle.len()).any(|w| w == needle.as_bytes());
    let t1 = Instant::now();
    while !(has("another thread's line") && has("before its free")) && t1.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(10));
    }
    let (other_out, own_out, report_out) = (has("another thread's line"), has("before its free"), has("ERROR:"));
    // SAFETY: fd 2 back to the saved descriptor before any assertion can print.
    unsafe {
        libc::dup2(saved, 2);
        libc::close(saved);
    }
    stop.store(true, SeqCst);
    let _ = reader.join();
    drop(ctx);

    let (after, own, other) = (AFTER_REPORT_MS.load(SeqCst), OWN_LINE_MS.load(SeqCst), OTHER_LINE_MS.load(SeqCst));
    println!("d4812 child: recovery {r:?} took {took} ms, after its report at {after} ms; own line {own} ms, other line {other} ms; drained {drained}; written: report {report_out} own {own_out} other {other_out}; writer after {writer_after}");
    assert!(
        after < STALL_MS,
        "the recovery reached the step after its report at {after} ms while the sink blocked {HOLD_MS} ms (the free waited on the report's write: D4812)"
    );
    assert!(r.is_ok(), "the recovery settles: {r:?}");
    assert!(own < STALL_MS, "the condemned backend's line before its free waited {own} ms on the sink");
    assert!(other >= WAITED_MS && other != u64::MAX, "another thread's line went to the sink itself and waited for it ({other} ms)");
    assert!(report_out && own_out && other_out, "every line reached the sink: report {report_out} own {own_out} other {other_out}");
    assert!(!writer_after, "the backend's writer is put back after the free");
}

#[test]
fn cancel_d4812_a_condemned_statements_free_never_waits_on_the_log_sink() {
    if std::env::var_os(CHILD).is_some() {
        return child();
    }
    let exe = std::env::current_exe().expect("this test binary");
    let out = std::process::Command::new(exe)
        .args([NAME, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .output()
        .expect("the child runs");
    let text = format!("{}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("d4812 child:"), "the child ran its case: {text}");
    assert!(out.status.success(), "{text}");
}

/// D4812 (2): the block gate's sleep probe (tcop's, `block_gate_may_sleep`) is off with interrupts held off, as every
/// LWLock holder has them (LWLockAcquire holds them), and in a critical section; on again once both end. Its interrupt
/// probe (`block_gate_interrupted`) answers a pending InterruptPending, query cancel or die.
#[test]
fn d4812_the_block_gates_probes_see_held_interrupts_a_critical_section_and_a_pending_interrupt() {
    std::thread::spawn(|| {
        use crate::main_loop::block_gate_may_sleep;
        use init_small::globals as g;
        assert!(block_gate_may_sleep(), "a plain backend thread may wait");
        g::HoldInterrupts();
        assert!(!block_gate_may_sleep(), "interrupts held off (an LWLock held): never");
        g::ResumeInterrupts();
        g::StartCriticalSection();
        assert!(!block_gate_may_sleep(), "a critical section: never");
        g::EndCriticalSection();
        assert!(block_gate_may_sleep(), "both ended: it may wait again");
        use crate::main_loop::block_gate_interrupted;
        assert!(!block_gate_interrupted(), "nothing pending");
        g::interrupt_pending_flag().store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(block_gate_interrupted(), "InterruptPending (a cancel's or a timeout's sender raised it)");
        g::interrupt_pending_flag().store(false, std::sync::atomic::Ordering::SeqCst);
        g::SetQueryCancelPending(true);
        assert!(block_gate_interrupted(), "a query cancel pending");
        g::SetQueryCancelPending(false);
        g::SetProcDiePending(true);
        assert!(block_gate_interrupted(), "a die pending");
        g::SetProcDiePending(false);
        assert!(!block_gate_interrupted(), "all cleared");
    })
    .join()
    .unwrap();
}
