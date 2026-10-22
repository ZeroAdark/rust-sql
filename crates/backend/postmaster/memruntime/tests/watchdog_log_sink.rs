//! D4812: the memory watchdog's samples, the ladder's pass among them, keep their period while the server log's sink
//! blocks. Under writeback / IO pressure one write(2) of a watchdog line to server.log waited 0.7-3 s on the ext4 journal
//! while a statement grew at ~1.15 GB/s past the ladder's line, and the pass (and R6 with it) waited for the write. Here
//! the sink is fd 2 turned into a pipe that nobody reads while it is full, so every write to it blocks; a grower ramps
//! this process's anonymous memory past the ladder's line, so the episode's "opens" line is logged on the watchdog's
//! thread while the sink blocks; and the watchdog's own clock (`memruntime::watch_ms`, one cadence a sample) must keep
//! moving with the wall clock. Its own test binary: the watchdog thread and fd 2 are process-wide.
#![cfg(target_os = "linux")]

use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The limit the watchdog reads (`pgrust.memory_watchdog_limit`, MB): its ladder line sits at 90 % less the 64 MB
/// margin, about 397 MB, under what any test box holds free.
const WATCHDOG_MB: i32 = 512;
/// The grower's step, the largest block an AllocSet asks for (mcx aset).
const STEP: usize = 8 << 20;
/// The grower's pace: one step every 10 ms, ~800 MB/s, the witness's hog in kind.
const STEP_MS: u64 = 10;
/// How far past the line the grower stops (under the cancel line, about 64 MB higher).
const PAST_LINE: u64 = 24 << 20;
/// How long the sink stays blocked after the grower crossed the line: passes keep coming, and the episode's lines.
const HOLD_MS: u64 = 1500;
/// The longest the watchdog's clock may stand still while the sink blocks: ten cadences. On trunk it stands still for
/// the whole hold.
const STALL_MS: u128 = 1000;

/// The governed resident the watchdog reads without a cgroup limit: RssAnon + RssShmem.
fn resident() -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let kb = |key: &str| {
        s.lines().find_map(|l| l.strip_prefix(key)).map_or(0, |v| v.trim().trim_end_matches("kB").trim().parse::<u64>().unwrap_or(0))
    };
    (kb("RssAnon:") + kb("RssShmem:")) * 1024
}

/// One look at the watchdog's clock: when it moved, the time it stood still before (`stall`, the longest so far).
fn sample(last: &mut u64, moved_at: &mut Instant, stall: &mut u128) {
    let now = memruntime::watch_ms();
    if now != *last {
        *stall = (*stall).max(moved_at.elapsed().as_millis());
        (*last, *moved_at) = (now, Instant::now());
    }
}

/// The pipe's reader: it drains into `out` while `on`, and stops reading (`idle` set) once `on` is cleared.
fn reader(fd: i32, on: Arc<AtomicBool>, idle: Arc<AtomicBool>, stop: Arc<AtomicBool>, out: Arc<Mutex<Vec<u8>>>) {
    let mut buf = vec![0u8; 64 << 10];
    while !stop.load(SeqCst) {
        if !on.load(SeqCst) {
            idle.store(true, SeqCst);
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }
        let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        // SAFETY: one pollfd on the stack.
        if unsafe { libc::poll(&mut p, 1, 5) } <= 0 || !on.load(SeqCst) {
            continue;
        }
        // SAFETY: reads into a live buffer of its own length.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n > 0 {
            out.lock().unwrap().extend_from_slice(&buf[..n as usize]);
        }
    }
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

#[test]
fn the_ladder_pass_keeps_its_period_while_the_log_sink_blocks() {
    // fd 2 becomes a pipe this test reads
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
    let (on, idle, stop) = (Arc::new(AtomicBool::new(true)), Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
    let out = Arc::new(Mutex::new(Vec::new()));
    let r = {
        let (on, idle, stop, out) = (on.clone(), idle.clone(), stop.clone(), out.clone());
        std::thread::spawn(move || reader(p[0], on, idle, stop, out))
    };

    // the watchdog under a limit with the ladder on; its start-up lines go through while the pipe is read
    guc_tables::backing::set_pgrust_memory_watchdog_limit(WATCHDOG_MB);
    guc_tables::backing::set_pgrust_memory_ladder(guc_tables::consts::MEMORY_LADDER_ON);
    memruntime::start(|| {});
    let t0 = Instant::now();
    while mcx::ladder::line() == 0 && t0.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(10));
    }
    let line = mcx::ladder::line();
    std::thread::sleep(Duration::from_millis(1500));

    // the sink blocks: the reader stops and the pipe is full, so the next write to fd 2 waits
    idle.store(false, SeqCst);
    on.store(false, SeqCst);
    while !idle.load(SeqCst) {
        std::thread::sleep(Duration::from_millis(1));
    }
    fill_fd2();
    let blocked_at = Instant::now();

    // the grower ramps past the line while the watchdog's clock is sampled
    let mut held: Vec<Vec<u8>> = Vec::new();
    let (mut last, mut moved_at, mut stall) = (memruntime::watch_ms(), Instant::now(), 0u128);
    let mut crossed = None;
    while crossed.is_none() && held.len() < 256 {
        let mut b = vec![0u8; STEP];
        for i in (0..STEP).step_by(4096) {
            b[i] = 1;
        }
        held.push(b);
        if line != 0 && resident() >= line + PAST_LINE {
            crossed = Some(Instant::now());
        }
        let step_end = Instant::now() + Duration::from_millis(STEP_MS);
        while Instant::now() < step_end {
            sample(&mut last, &mut moved_at, &mut stall);
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    let hold_end = Instant::now() + Duration::from_millis(HOLD_MS);
    while Instant::now() < hold_end {
        sample(&mut last, &mut moved_at, &mut stall);
        std::thread::sleep(Duration::from_millis(2));
    }
    stall = stall.max(moved_at.elapsed().as_millis());
    let blocked_ms = blocked_at.elapsed().as_millis();

    // the sink drains again: the lines the watchdog logged while it blocked reach it
    on.store(true, SeqCst);
    let t1 = Instant::now();
    let has = |needle: &str| out.lock().unwrap().windows(needle.len()).any(|w| w == needle.as_bytes());
    while !has("ladder on episode") && t1.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(10));
    }
    let opened = has("ladder on episode");
    // the exit's drain: every queued write reaches the sink, then the ring closes (later writes are their threads' own)
    let drained = memruntime::memlog::drain(Duration::from_millis(memruntime::memlog::SHUTDOWN_WAIT_MS));
    let (queued, dropped, _) = memruntime::memlog::counts();
    drop(held);
    // SAFETY: fd 2 back to the saved descriptor before any assertion can print.
    unsafe {
        libc::dup2(saved, 2);
        libc::close(saved);
    }
    stop.store(true, SeqCst);
    let _ = r.join();

    assert_ne!(line, 0, "the watchdog published the ladder's line");
    assert!(crossed.is_some(), "the grower crossed the line {} MB (resident {} MB)", line >> 20, resident() >> 20);
    assert!(opened, "the episode's opening line was logged while the sink blocked");
    assert!(drained, "the drain emptied the ring");
    assert!(queued > 0, "the watchdog's lines went through the ring ({queued} queued, {dropped} dropped)");
    assert!(
        stall < STALL_MS,
        "the watchdog's samples stood still {stall} ms of the {blocked_ms} ms the log sink blocked (the ladder's pass waited \
         on the log's write: D4812)"
    );
}
