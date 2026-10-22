//! D4812: the memory safety threads' server-log writes, made off those threads. The memory watchdog's thread
//! (pg:memwatchdog) runs the ladder's pass and R6's cancel, and it wrote its own lines through elog to fd 2. Under
//! writeback / IO pressure one such write(2) to server.log waited 0.7-3 s on the ext4 journal (jbd2), at the line crossing
//! or the R6 line, while a statement grew at ~1.15 GB/s: the pass, and the cancel with it, came seconds late.
//!
//! One path for every such thread: a thread that calls [`defer_here`] hands each server-log write elog would make
//! (`elog::set_server_log_defer`: a formatted line at any level, a syslogger pipe chunk, a write_stderr text) to [`push`].
//! The bytes are copied into one fixed ring ([`RING_BYTES`], in .bss, faulted in at the watchdog's start): no allocation
//! per line, no wait on the sink. A full ring drops the write and counts it. One drainer thread, pg:memlog, writes the
//! ring's records to fd 2 in their order and logs how many were dropped once the ring is empty again. The lock is held
//! for copies only, never across the write.
//!
//! Order: a thread's lines keep their order. Another thread's lines may now land between them, or before them, where
//! they did not before. Each line's prefix (time, pid) is still formatted when the line is made, so it shows when it
//! happened. Shutdown: the postmaster's exit callback [`drain`]s the ring (bounded) before the process exits, and the
//! crash restart's exit drains it best effort. A SIGKILL loses what the ring still holds: milliseconds of lines,
//! seconds only while the sink itself is stuck.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use core::time::Duration;

/// The ring's bytes: about 250 lines of 1 KiB (the watchdog's lines are 0.1-0.7 KiB; an episode logs a handful).
/// CHOSEN, not tuned: 256 KiB of .bss.
pub const RING_BYTES: usize = 256 << 10;
/// The longest write the ring keeps (a longer one is cut to it, marked and counted: a condemned statement's report with
/// its statement's text). CHOSEN: 16 KiB, past every watchdog line and past the syslogger's pipe chunk (PIPE_BUF).
pub const WRITE_BYTES: usize = 16 << 10;
/// The end of a cut write.
const CUT: &[u8] = b"... [memory log: cut to 16 kB]\n";
/// A record's length prefix.
const HEADER: usize = 4;
/// The drainer's backstop tick (a push wakes it).
const TICK_MS: u64 = 100;
/// A clean shutdown's wait for the drain (the postmaster's exit callback). CHOSEN: 5 s, a twelfth of pg_ctl's wait.
pub const SHUTDOWN_WAIT_MS: u64 = 5_000;
/// The crash restart's wait (best effort: the generation is ending).
pub const CRASH_WAIT_MS: u64 = 1_000;
/// The drainer's stack: one record's copy (`WRITE_BYTES`) and elog's report of the drops.
const STACK: usize = 256 << 10;

/// The ring: records of a 4-byte length and its bytes, from `head`, `len` bytes in all (wrapping).
pub(crate) struct Ring {
    buf: [u8; RING_BYTES],
    head: usize,
    len: usize,
    /// The drainer holds a record it has not written yet.
    busy: bool,
    /// Drained for the exit: a push from here on is written by its own thread.
    closed: bool,
    /// Bytes let go because the ring was out of step ([`Ring::take`]), not yet counted.
    lost: u64,
}

impl Ring {
    pub(crate) const fn new() -> Self {
        Ring { buf: [0; RING_BYTES], head: 0, len: 0, busy: false, closed: false, lost: 0 }
    }

    fn copy_in(&mut self, at: usize, src: &[u8]) {
        let at = at % RING_BYTES;
        let first = src.len().min(RING_BYTES - at);
        self.buf[at..at + first].copy_from_slice(&src[..first]);
        self.buf[..src.len() - first].copy_from_slice(&src[first..]);
    }

    fn copy_out(&self, at: usize, dst: &mut [u8]) {
        let at = at % RING_BYTES;
        let first = dst.len().min(RING_BYTES - at);
        let n = dst.len();
        dst[..first].copy_from_slice(&self.buf[at..at + first]);
        dst[first..].copy_from_slice(&self.buf[..n - first]);
    }

    /// Append one record; false when it is longer than `WRITE_BYTES` or the ring has no room for it.
    pub(crate) fn put(&mut self, bytes: &[u8]) -> bool {
        self.put2(bytes, &[])
    }

    /// Append one record of `a` then `b`.
    fn put2(&mut self, a: &[u8], b: &[u8]) -> bool {
        let n = a.len() + b.len();
        let need = HEADER + n;
        if n > WRITE_BYTES || self.len + need > RING_BYTES {
            return false;
        }
        let tail = self.head + self.len;
        self.copy_in(tail, &(n as u32).to_le_bytes());
        self.copy_in(tail + HEADER, a);
        self.copy_in(tail + HEADER + a.len(), b);
        self.len += need;
        true
    }

    /// Append `bytes`, cut to `WRITE_BYTES` with `CUT` at its end when longer; (kept, cut).
    pub(crate) fn put_cut(&mut self, bytes: &[u8]) -> (bool, bool) {
        if bytes.len() <= WRITE_BYTES {
            return (self.put(bytes), false);
        }
        (self.put2(&bytes[..WRITE_BYTES - CUT.len()], CUT), true)
    }

    /// The oldest record copied into `out` and taken off: its length (None: empty). A length no `put` wrote (past a
    /// record, or past the bytes the ring holds) means the ring is out of step: everything it holds is let go and
    /// counted ([`Self::take_lost`]), so the drainer never reads on out of step (D4887: a length taken past `len`
    /// wrapped it, and the drainer wrote the ring's stale bytes to the server log without end).
    pub(crate) fn take(&mut self, out: &mut [u8; WRITE_BYTES]) -> Option<usize> {
        if self.len == 0 {
            return None;
        }
        let mut h = [0u8; HEADER];
        self.copy_out(self.head, &mut h);
        let n = u32::from_le_bytes(h) as usize;
        if n > WRITE_BYTES || HEADER + n > self.len {
            self.lost += self.len as u64;
            (self.head, self.len) = (0, 0);
            return None;
        }
        self.copy_out(self.head + HEADER, &mut out[..n]);
        self.head = (self.head + HEADER + n) % RING_BYTES;
        self.len -= HEADER + n;
        Some(n)
    }

    /// The bytes let go since the last call because the ring was out of step ([`Self::take`]).
    pub(crate) fn take_lost(&mut self) -> u64 {
        core::mem::take(&mut self.lost)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Each page of the ring written with the byte it holds: faulted in for writing, and a record already queued keeps
    /// its bytes. D4887 (lane flipcost, 10-18): writing 0 there raced the memory threads that start before the
    /// watchdog (the give-back service's first line, pg:memrelease): a queued record's length lost its low byte, the
    /// drainer read the ring out of step, its count wrapped, and pg:memlog wrote 16 KiB of the ring's zeros to
    /// server.log without end (310 MB in 3 min and half a core on an x86 pgbench take, 92 MB on an Arm analytic one).
    fn touch_pages(&mut self) {
        for i in (0..RING_BYTES).step_by(4096) {
            self.buf[i] = std::hint::black_box(self.buf[i]);
        }
        std::hint::black_box(&mut self.buf[..]);
    }
}

static RING: pgsync::Mutex<Ring> = pgsync::Mutex::new(Ring::new());
/// The drainer found the ring empty (a drain waits on it).
static EMPTIED: pgsync::Condvar = pgsync::Condvar::new();
static THREAD: pgsync::OnceLock<pgsync::thread::Thread> = pgsync::OnceLock::new();
static WAKE: AtomicBool = AtomicBool::new(false);
/// The counts (the accounts view's memlog rows): writes queued, writes dropped (ring full, or longer than a record), the
/// drops not yet reported in the log, and the drainer's longest write to fd 2 (us).
static QUEUED: AtomicU64 = AtomicU64::new(0);
static DROPPED: AtomicU64 = AtomicU64::new(0);
static UNREPORTED: AtomicU64 = AtomicU64::new(0);
static CUT_WRITES: AtomicU64 = AtomicU64::new(0);
static WRITE_US_LONGEST: AtomicU64 = AtomicU64::new(0);

/// `(writes queued, writes dropped, the longest write to the sink in ms)` since start.
pub fn counts() -> (u64, u64, u64) {
    (QUEUED.load(Relaxed), DROPPED.load(Relaxed), WRITE_US_LONGEST.load(Relaxed) / 1000)
}

/// The writes kept cut to `WRITE_BYTES` since start.
pub fn cut_writes() -> u64 {
    CUT_WRITES.load(Relaxed)
}

/// Start the drainer (memruntime's `start`, once, before the watchdog's thread). Whether it runs.
pub fn start() -> bool {
    if THREAD.get().is_some() {
        return true;
    }
    // the drainer's own line (the drops) carries the postmaster's prefix and time zone, as the watchdog's do
    let (prefix, tz) = (elog::config::log_line_prefix_format(), localtime::globals::log_timezone());
    let spawned = std::thread::Builder::new().name("pg:memlog".into()).stack_size(STACK).spawn(pagegive::pinned(move || {
        elog::config::set_log_line_prefix(prefix);
        localtime::globals::set_log_timezone(tz);
        // unwind-ok: log-then-die
        let _ = std::panic::catch_unwind(run);
        // the drainer is gone: every push from here on is written by its own thread
        pgsync::lock(&RING).closed = true;
        EMPTIED.notify_all();
    }));
    match spawned {
        Ok(h) => THREAD.set(h.thread().clone()).is_ok(),
        Err(_) => false,
    }
}

/// This thread's server-log writes go through the ring from now on (a memory safety thread, at its start).
pub fn defer_here() {
    elog::set_server_log_defer(Some(push));
}

/// A statement the memory watchdog canceled frees its memory in its backend's error recovery, after the report: this
/// thread's server-log writes go through the ring from there to the free (tcop's `error_recovery`), so the free never
/// waits on the log's sink. This thread's only; the writer it had, which the caller puts back after the free.
pub fn defer_condemned() -> Option<elog::ServerLogDefer> {
    elog::set_server_log_defer(Some(push))
}

std::thread_local! {
    /// D4812 (2): the writer this thread had when mcx's block gate deferred its lines (`gate_defer_begin`); `None`
    /// outside the gate.
    static GATE_BEFORE: core::cell::Cell<Option<Option<elog::ServerLogDefer>>> = const { core::cell::Cell::new(None) };
}

/// D4812 (2), mcx's block gate entering its inline rungs (`mcx::set_gate_log_defer`): this thread's server-log writes go
/// through the ring, as `defer_condemned`'s, the writer it had kept for `gate_defer_end`. Allocator context: no
/// allocation (a push copies into the ring).
pub fn gate_defer_begin() {
    let before = defer_condemned();
    let _ = GATE_BEFORE.try_with(|c| c.set(Some(before)));
}

/// D4812 (2): the block gate's rungs ran: the writer this thread had is put back.
pub fn gate_defer_end() {
    if let Ok(Some(before)) = GATE_BEFORE.try_with(|c| c.take()) {
        elog::set_server_log_defer(before);
    }
}

/// The ring's pages faulted in (the watchdog's prefault), so a push under direct reclaim faults in none; the records
/// already queued keep their bytes ([`Ring::touch_pages`]).
pub fn prefault() {
    pgsync::lock(&RING).touch_pages();
}

/// elog's [`elog::ServerLogDefer`]: queue one write (cut to `WRITE_BYTES` when longer, counted), or count it dropped.
/// False (the caller writes it): no drainer, or the ring was drained for the exit.
pub fn push(bytes: &[u8]) -> bool {
    if THREAD.get().is_none() {
        return false;
    }
    let mut r = pgsync::lock(&RING);
    if r.closed {
        return false;
    }
    let (kept, cut) = r.put_cut(bytes);
    drop(r);
    if kept {
        QUEUED.fetch_add(1, Relaxed);
        if cut {
            CUT_WRITES.fetch_add(1, Relaxed);
        }
        wake();
    } else {
        DROPPED.fetch_add(1, Relaxed);
        UNREPORTED.fetch_add(1, Relaxed);
    }
    true
}

fn wake() {
    WAKE.store(true, Relaxed);
    if let Some(t) = THREAD.get() {
        t.unpark();
    }
}

/// Wait (at most `wait`) until the drainer wrote every queued record, then close the ring: later writes are made by their
/// own threads, after the ones queued. Whether it emptied in time (true with no drainer).
pub fn drain(wait: Duration) -> bool {
    if THREAD.get().is_none() {
        return true;
    }
    let end = pg_clock::mono_ns().saturating_add(wait.as_nanos() as u64);
    let mut r = pgsync::lock(&RING);
    loop {
        if r.closed || (r.is_empty() && !r.busy) {
            r.closed = true;
            return true;
        }
        let left = Duration::from_nanos(end.saturating_sub(pg_clock::mono_ns()));
        if left.is_zero() {
            r.closed = true;
            return false;
        }
        wake();
        r = match EMPTIED.wait_timeout(r, left.min(Duration::from_millis(TICK_MS))) {
            Ok((g, _)) => g,
            Err(e) => e.into_inner().0,
        };
    }
}

/// The drainer: each record written in order; once the ring is empty, the drops since the last report logged, and the
/// waiters of `drain` told.
fn run() {
    let mut out = [0u8; WRITE_BYTES];
    loop {
        let n = {
            let mut r = pgsync::lock(&RING);
            let n = r.take(&mut out);
            r.busy = n.is_some();
            if r.take_lost() != 0 {
                // the ring was out of step: what it held is lost, reported as one dropped write
                DROPPED.fetch_add(1, Relaxed);
                UNREPORTED.fetch_add(1, Relaxed);
            }
            n
        };
        match n {
            Some(n) => write_sink(&out[..n]),
            None => {
                let dropped = UNREPORTED.swap(0, Relaxed);
                if dropped != 0 {
                    let _ = elog::elog(
                        types_error::LOG,
                        format!("memory log: {dropped} log writes dropped: the queue was full while its sink was slow"), // alloc: Cold
                    );
                }
                EMPTIED.notify_all();
                if !WAKE.swap(false, Relaxed) {
                    pgsync::thread::park_timeout(Duration::from_millis(TICK_MS));
                    WAKE.store(false, Relaxed);
                }
            }
        }
    }
}

/// One record to fd 2, whole (a pipe chunk is one write, as elog's own): the time it took, the longest kept.
fn write_sink(mut b: &[u8]) {
    let at = pg_clock::mono_ns();
    #[cfg(unix)]
    while !b.is_empty() {
        // SAFETY: writes from a live buffer of its own length.
        let n = unsafe { libc::write(2, b.as_ptr().cast(), b.len()) };
        if n > 0 {
            b = &b[n as usize..];
        } else if n < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        } else {
            break;
        }
    }
    #[cfg(not(unix))]
    {
        use std::io::Write;
        let _ = std::io::stderr().write_all(b);
        b = &[];
    }
    let _ = b;
    WRITE_US_LONGEST.fetch_max(pg_clock::mono_ns().saturating_sub(at) / 1000, Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring() -> Box<Ring> {
        Box::new(Ring::new())
    }

    #[test]
    fn records_come_out_whole_and_in_order_across_the_wrap() {
        let mut r = ring();
        let mut out = [0u8; WRITE_BYTES];
        // each round leaves the head further on, so records straddle the end of the buffer
        for round in 0..40u32 {
            let lines: Vec<Vec<u8>> = (0..9u32).map(|i| format!("line {round}.{i} {}\n", "x".repeat((round * 97 + i * 31) as usize % 900)).into_bytes()).collect();
            for l in &lines {
                assert!(r.put(l));
            }
            for l in &lines {
                let n = r.take(&mut out).expect("a record");
                assert_eq!(&out[..n], &l[..]);
            }
            assert!(r.is_empty() && r.take(&mut out).is_none());
        }
    }

    #[test]
    fn a_full_ring_refuses_and_keeps_what_it_holds() {
        let mut r = ring();
        let line = [b'a'; 1000];
        let mut kept = 0;
        while r.put(&line) {
            kept += 1;
        }
        assert_eq!(kept, RING_BYTES / (HEADER + line.len()));
        assert!(!r.put(&[b'b'; WRITE_BYTES + 1]), "longer than a record");
        let mut out = [0u8; WRITE_BYTES];
        let n = r.take(&mut out).expect("the first record");
        assert_eq!(&out[..n], &line[..]);
        assert!(r.put(&line), "room again after a take");
    }

    #[test]
    fn a_write_past_a_record_is_cut_and_marked() {
        let mut r = ring();
        let mut out = [0u8; WRITE_BYTES];
        let long = [b'q'; WRITE_BYTES + 5000];
        assert_eq!(r.put_cut(&long), (true, true));
        assert_eq!(r.put_cut(b"next\n"), (true, false));
        let n = r.take(&mut out).expect("the cut record");
        assert_eq!(n, WRITE_BYTES);
        assert!(out[..n - CUT.len()].iter().all(|&b| b == b'q') && out[..n].ends_with(CUT), "its head, then the mark");
        let n = r.take(&mut out).expect("the next one");
        assert_eq!(&out[..n], b"next\n");
    }

    #[test]
    fn a_prefault_keeps_the_queued_records() {
        // D4887: the give-back service's first line, queued at offset 0 before the watchdog's prefault
        let mut r = ring();
        let mut out = [0u8; WRITE_BYTES];
        let first = b"2026-10-18 17:00:42.009 UTC [66537] LOG:  give-back service: 4 arenas' purge is pg:mempurge's\n".to_vec();
        let lines: Vec<Vec<u8>> = (0..40u32).map(|i| format!("line {i} {}\n", "y".repeat(i as usize * 211 % 3000)).into_bytes()).collect();
        assert!(r.put(&first));
        for l in &lines {
            assert!(r.put(l));
        }
        r.touch_pages();
        assert_eq!(r.take(&mut out), Some(first.len()));
        assert_eq!(&out[..first.len()], &first[..]);
        for l in &lines {
            let n = r.take(&mut out).expect("a record");
            assert_eq!(&out[..n], &l[..]);
        }
        assert!(r.is_empty() && r.take_lost() == 0);
    }

    #[test]
    fn a_ring_out_of_step_lets_go_instead_of_reading_on() {
        let mut r = ring();
        let mut out = [0u8; WRITE_BYTES];
        assert!(r.put(b"a line\n"));
        assert!(r.put(b"another line\n"));
        let held = r.len as u64;
        // a length no put wrote: past the bytes the ring holds
        r.buf[0] = 0xff;
        assert_eq!(r.take(&mut out), None);
        assert!(r.is_empty());
        assert_eq!(r.take_lost(), held);
        assert!(r.put(b"after\n"), "usable again");
        assert_eq!(r.take(&mut out), Some(6));
        assert_eq!(&out[..6], b"after\n");
    }

    #[test]
    fn put_and_take_allocate_nothing() {
        let mut r = ring();
        let mut out = [0u8; WRITE_BYTES];
        let line = [b'z'; 700];
        let (all, allocs) = crate::tests::counted(|| (0..1000).all(|_| r.put(&line) && r.take(&mut out) == Some(line.len())));
        assert!(all);
        assert_eq!(allocs, 0, "no allocation per line");
    }
}
