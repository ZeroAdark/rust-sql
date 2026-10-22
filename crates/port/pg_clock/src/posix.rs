//! PosixClock — the product clock: `#[inline]` single-syscall wrappers over
//! `clock_gettime`. This module is the ONLY sanctioned home of a raw
//! monotonic/wall syscall outside the P0 ledger's INFRA/BOUNDARY residue
//! (contract §0.2: no second `clock_gettime` hub survives outside this file).
//!
//! Errno-analog rule: never panics. A failed `clock_gettime` leaves the
//! zeroed timespec and the read reports 0 — exactly the retired waiter
//! `RealClock` behavior.

use crate::ClockSource;

/// Zero-sized product clock. `ActiveClock` in every non-`pgrust_sim` build.
#[derive(Clone, Copy, Debug, Default)]
pub struct PosixClock;

impl PosixClock {
    #[inline]
    pub const fn new() -> Self {
        PosixClock
    }
}

#[inline]
fn read_clock_ns(clock: libc::clockid_t) -> i64 {
    // SAFETY: clock_gettime into a zeroed timespec; on failure the zeroed
    // timespec reads as 0 (never-panic rule).
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    // SAFETY: valid pointer to ts.
    unsafe { libc::clock_gettime(clock, &mut ts) };
    ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64
}

/// This thread's CPU time in ns (`CLOCK_THREAD_CPUTIME_ID`; 0 if the read fails). Diagnostics only, never a deadline or a
/// timestamp: cancelpoll's poll-gap probe prints the on-CPU share of a stretch with it (a gap the thread spent off-CPU
/// waited; one it spent on-CPU ran).
#[cfg(not(target_family = "wasm"))]
#[inline]
pub fn thread_cpu_ns() -> u64 {
    read_clock_ns(libc::CLOCK_THREAD_CPUTIME_ID).max(0) as u64
}

/// wasm32-wasip1 has no thread CPU clock (the libc crate's wasi module defines only CLOCK_MONOTONIC / CLOCK_REALTIME):
/// always 0, the failed-read value, so the probe prints no on-CPU share there.
#[cfg(target_family = "wasm")]
#[inline]
pub fn thread_cpu_ns() -> u64 {
    0
}

/// This thread's resource use and run-queue wait, for cancelpoll's poll-gap probe (testflow, D4128 / D4129, main
/// 2026-10-12T05:53:24Z). A stretch the thread spent off-CPU either waited in code or was not running at all: a run-queue wait
/// under oversubscribed CPUs, or a major page fault blocked on disk. CPU time alone cannot tell these apart. Diagnostics only,
/// read on the probe-on path at most every half threshold, never on a hot path. Linux only: macOS has no RUSAGE_THREAD and
/// wasm has no getrusage, so [`thread_use`] is `None` there, and `None` when a read fails.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ThreadUse {
    /// User and system CPU ns (getrusage `RUSAGE_THREAD` `ru_utime` / `ru_stime`).
    pub user_ns: u64,
    pub sys_ns: u64,
    /// Minor and major page faults.
    pub minflt: u64,
    pub majflt: u64,
    /// Block input / output operations (`ru_inblock` / `ru_oublock`, 512-byte units): reads that reached the block layer and
    /// page-cache writes this thread dirtied. A blocked write under a memory limit shows here and not as a major fault.
    pub inblock: u64,
    pub oublock: u64,
    /// Voluntary (blocked) and involuntary (preempted) context switches.
    pub nvcsw: u64,
    pub nivcsw: u64,
    /// ns spent runnable but waiting for a CPU (`/proc/thread-self/schedstat`'s second field); `None` without schedstats.
    pub rq_ns: Option<u64>,
}

#[cfg(target_os = "linux")]
pub fn thread_use() -> Option<ThreadUse> {
    // SAFETY: getrusage into a zeroed rusage; RUSAGE_THREAD reads the calling thread's counters.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: valid pointer to ru.
    if unsafe { libc::getrusage(libc::RUSAGE_THREAD, &mut ru) } != 0 {
        return None;
    }
    let tv = |t: libc::timeval| (t.tv_sec.max(0) as u64) * 1_000_000_000 + (t.tv_usec.max(0) as u64) * 1_000;
    let n = |v: libc::c_long| v.max(0) as u64;
    Some(ThreadUse {
        user_ns: tv(ru.ru_utime),
        sys_ns: tv(ru.ru_stime),
        minflt: n(ru.ru_minflt),
        majflt: n(ru.ru_majflt),
        inblock: n(ru.ru_inblock),
        oublock: n(ru.ru_oublock),
        nvcsw: n(ru.ru_nvcsw),
        nivcsw: n(ru.ru_nivcsw),
        rq_ns: thread_runqueue_ns(),
    })
}

#[cfg(not(target_os = "linux"))]
#[inline]
pub fn thread_use() -> Option<ThreadUse> {
    None
}

/// `/proc/thread-self/schedstat` read into a stack buffer: no allocation on the probe path.
#[cfg(target_os = "linux")]
fn thread_runqueue_ns() -> Option<u64> {
    use std::io::Read;
    let mut b = [0u8; 96];
    let n = std::fs::File::open("/proc/thread-self/schedstat").ok()?.read(&mut b).ok()?;
    schedstat_wait_ns(&b[..n])
}

/// The run-queue wait in a schedstat line (`<on-cpu ns> <run-queue wait ns> <timeslices>`): its second field.
pub fn schedstat_wait_ns(line: &[u8]) -> Option<u64> {
    std::str::from_utf8(line).ok()?.split_ascii_whitespace().nth(1)?.parse().ok()
}

impl ClockSource for PosixClock {
    #[inline]
    fn mono_ns(&self) -> u64 {
        read_clock_ns(libc::CLOCK_MONOTONIC).max(0) as u64
    }

    #[inline]
    fn wall_ns(&self) -> i64 {
        read_clock_ns(libc::CLOCK_REALTIME)
    }
}
