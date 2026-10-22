//! D3887: the abort poll of a long host-side loop, and the poll-gap probe.
//!
//! A Gravity kernel polls its statement's abort byte at its loop back-edges; a host call it makes (a source's window, a
//! dictionary table build, the part cache's trim) polled nothing until it returned, so a cancel, a statement_timeout and
//! the memory watchdog's cancel waited out the whole call (1.4-3.2 s measured on the stress smoke). The driver lends the
//! byte to the host code of each kernel call ([`scoped`]); a long host loop asks [`canceled`] per frame / chunk / part and
//! stops with its own error, which the host error path reports as the kernel's cancel ([`seen`]). The statement's owner
//! raises the real error (57014, 53200, the timeout's) at its next check, as for a kernel's own stop.
//!
//! The probe ([`set_probe_ms`]; Gravity's `PGRUST_GRAVITY_POLL_GAP_MS`, off by default): every stretch of a thread inside
//! a kernel call between two abort polls (a kernel's poll, a host loop's, the call's start and end) that reaches the
//! threshold is logged to stderr, one `poll-gap:` line with the host-call labels ([`phase`]) that held it. While the
//! probe is on, a statement's byte carries [`PROBE_BIT`] so that every kernel back-edge calls its poll.
//!
//! A line names its gap's site even when no labelled step held it (testflow, main 2026-10-11T09:33:11Z: the four longest
//! gaps of gate-c3x's arm A, 7.8-15.8 s, printed only 'ended by a poll'): `held by` sums EVERY labelled step that ended in
//! the gap per label (`steps` gives the count and the longest one), `unlabelled` is the part of the gap no top-level label
//! covers, `began after` is what started the stretch and where (the poll's, or the kernel call's, call site), and `on-CPU`
//! is the thread's CPU time over the stretch (a gap the thread spent off-CPU waited, one it spent on-CPU ran).
#![deny(unsafe_op_in_unsafe_fn)]

use core::cell::{Cell, RefCell};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::io::Write;

pub mod interrupts;
mod step;

pub use step::{lock_or_cancel, set_step_gap_ms, sort_by, step, step_gaps, step_on, step_static, Canceled, Each, Step, BATCH, LOCK_PARK, PIECE, SORT_RUN, STEP_GAP_MS};

/// The abort bit of the byte (gravity_abi `ctx::ABORT_BIT`, init_small's; gravity exec asserts they agree).
pub const ABORT_BIT: u8 = 1;
/// The probe's bit, set in a statement's byte while the probe is on: a kernel's back-edge test (`byte != 0`) then calls
/// its poll at every check. Every other reader of the byte masks the abort or the service bit, so it sees nothing.
pub const PROBE_BIT: u8 = 4;

thread_local! {
    /// The abort byte of the kernel call this thread runs (null: none).
    static BYTE: Cell<*const AtomicU8> = const { Cell::new(core::ptr::null()) };
    /// A host loop of that call stopped for the abort bit.
    static SEEN: Cell<bool> = const { Cell::new(false) };
    static PROBE: RefCell<Probe> = const { RefCell::new(Probe::NEW) };
}

/// Run `f` (a kernel call) with `byte` lent to its host code: [`canceled`] reads it. Nests: a host call that runs a
/// nested kernel call lends that call's byte and gets its own back, with any stop the nested call saw.
#[inline]
#[track_caller]
pub fn scoped<R>(byte: &AtomicU8, f: impl FnOnce() -> R) -> R {
    let _scope = Scope::enter(byte, core::panic::Location::caller());
    f()
}

/// [`scoped`], and whether a host loop of the call stopped for the abort bit ([`seen`] at the call's end). The stop is not
/// consumed: a nested call hands it to the outer one as `scoped` does, and the byte keeps its abort bit for the statement.
#[inline]
#[track_caller]
pub fn scoped_seen<R>(byte: &AtomicU8, f: impl FnOnce() -> R) -> (R, bool) {
    let _scope = Scope::enter(byte, core::panic::Location::caller());
    let r = f();
    (r, SEEN.get())
}

/// [`scoped`]'s guard: private, so it is always dropped before the byte it holds can go (an unwind out of `f` included).
struct Scope {
    prev: *const AtomicU8,
    prev_lent: step::Lent,
    prev_seen: bool,
    prev_last: u64,
    site: &'static core::panic::Location<'static>,
}

impl Scope {
    #[inline]
    fn enter(byte: &AtomicU8, site: &'static core::panic::Location<'static>) -> Scope {
        let prev = BYTE.replace(byte);
        let prev_lent = step::lend(byte);
        step::debug_polled();
        let prev_seen = SEEN.replace(false);
        let prev_last = if probe_on() { probe_enter(site) } else { 0 };
        if STALL_MS.load(Ordering::Relaxed) != 0 {
            injected_stall();
        }
        Scope { prev, prev_lent, prev_seen, prev_last, site }
    }
}

impl Drop for Scope {
    #[inline]
    fn drop(&mut self) {
        if probe_on() {
            probe_exit(self.prev_last, self.site);
        }
        let nested = !self.prev.is_null();
        BYTE.set(self.prev);
        step::restore(self.prev_lent);
        step::debug_polled();
        SEEN.set(self.prev_seen || (nested && SEEN.get()));
    }
}

/// A long host loop's poll (per frame / chunk / part): the abort bit of the byte the running kernel call lent; false
/// outside one. Once true, [`seen`] stays true for the rest of the call: the loop stops with its own error, which the host
/// error path reports as the kernel's cancel.
#[inline]
#[track_caller]
pub fn canceled() -> bool {
    if HOST_POLLS_OFF.load(Ordering::Relaxed) {
        return false;
    }
    if probe_on() {
        polled_slow(core::panic::Location::caller());
    }
    let b = BYTE.get();
    if b.is_null() {
        return false;
    }
    step::debug_polled();
    // SAFETY: only a live `Scope` of this thread sets `BYTE`, from a reference that outlives it (`scoped`), and its drop
    // puts the previous pointer back
    if unsafe { &*b }.load(Ordering::Acquire) & ABORT_BIT == 0 {
        return false;
    }
    SEEN.set(true);
    true
}

/// A host loop of the running kernel call stopped for the abort bit ([`canceled`], a [`Step`]'s tick): an error raised on
/// the way out is the cancel's, never the statement's.
#[inline]
pub fn seen() -> bool {
    SEEN.get()
}

fn mark_seen() {
    SEEN.set(true);
}

#[inline]
fn host_polls_off() -> bool {
    HOST_POLLS_OFF.load(Ordering::Relaxed)
}

static HOST_POLLS_OFF: AtomicBool = AtomicBool::new(false);

/// The host loops' polls on (the default) or off (`canceled` always false, no probe mark: the loops run to their end as
/// before D3887; Gravity's `PGRUST_GRAVITY_HOST_POLL=0`, the probe's before-arm).
pub fn set_host_polls(on: bool) {
    HOST_POLLS_OFF.store(!on, Ordering::Relaxed);
}

/// A kernel's abort poll (gravity exec's `poll_kernel`, on a set byte): the probe's mark; nothing when it is off.
#[inline]
#[track_caller]
pub fn polled() {
    if probe_on() {
        polled_slow(core::panic::Location::caller());
    }
}

/// K11 (D4214 / D4229): [`polled`] at a given site: a CHECK_FOR_INTERRUPTS's mark (tcop postgres mark_poll) with the
/// location its caller passed through the tracked seam (seam_core::seam_tracked!). Nothing when the probe is off.
#[inline]
pub fn polled_at(site: &'static core::panic::Location<'static>) {
    if probe_on() {
        polled_slow(site);
    }
}

/// D3962: an interruptible wait (a latch wait the cancel's signal wakes, its caller's poll at the wake) for the probe: a
/// poll at its start, and the time inside it no gap (the stretch starts again at its end). Nothing when the probe is off.
/// The caller's site ends the stretch before the wait and begins the one after it (a probe line's `ended at` / `began
/// after .. at`).
#[inline]
#[track_caller]
pub fn waiting() -> Waiting {
    step::debug_polled();
    if probe_on() {
        let site = core::panic::Location::caller();
        wait_slow(site);
        return Waiting(Some(site));
    }
    Waiting(None)
}

/// [`waiting`]'s guard: the wait ends at its drop.
#[must_use]
pub struct Waiting(Site);

impl Drop for Waiting {
    #[inline]
    fn drop(&mut self) {
        step::debug_polled();
        if let Some(site) = self.0 {
            woke_slow(site);
        }
    }
}

// ---- the pollgap gate's injected stall ----

static STALL_MS: AtomicU64 = AtomicU64::new(0);

/// The label of the injected stall's gap: the pollgap gate keys it `pollgap::cancelpoll injected stall`, a site with no row.
pub const STALL_LABEL: &str = "cancelpoll injected stall";

/// C9: the pollgap gate's born-red lever (Gravity's `PGRUST_GRAVITY_POLL_STALL_MS`, unset in production): the first kernel
/// call after it is set spins `ms` on its thread with no poll, inside the step [`STALL_LABEL`]; once per setting.
pub fn set_stall_ms(ms: u64) {
    STALL_MS.store(ms, Ordering::Relaxed);
}

#[cold]
#[inline(never)]
fn injected_stall() {
    let ms = STALL_MS.swap(0, Ordering::Relaxed);
    if ms == 0 {
        return;
    }
    let labelled = probe_on() && phase_enter(STALL_LABEL);
    let t0 = pg_clock::mono_ns();
    while pg_clock::mono_ns().saturating_sub(t0) < ms.saturating_mul(1_000_000) {
        core::hint::spin_loop();
    }
    if labelled {
        phase_exit();
    }
}

// ---- the cancel lever (witnesses) ----

/// What [`set_cancel_at`] armed: the step labels' start, and the host's delivery of a cancel to the statement whose byte it
/// gets.
struct CancelAt {
    label: &'static str,
    fire: fn(*const AtomicU8),
}

static CANCEL_AT_ON: AtomicBool = AtomicBool::new(false);
static CANCEL_AT: pgsync::OnceLock<CancelAt> = pgsync::OnceLock::new();

/// A witness lever (Gravity's `PGRUST_GRAVITY_CANCEL_AT_STEP`, unset in production): each [`step`] whose label starts with
/// `label` and that opens inside a kernel call calls `fire` with the call's byte before its first tick, and the host
/// delivers that statement's cancel there, as pg_cancel_backend would. A test then reaches a cancel inside that step with
/// no duration. Set once per process; a later call is ignored.
pub fn set_cancel_at(label: &'static str, fire: fn(*const AtomicU8)) {
    if CANCEL_AT.set(CancelAt { label, fire }).is_ok() {
        CANCEL_AT_ON.store(true, Ordering::Release);
    }
}

#[inline]
fn cancel_at_on() -> bool {
    CANCEL_AT_ON.load(Ordering::Relaxed)
}

#[cold]
#[inline(never)]
fn cancel_at(label: &'static str, byte: *const AtomicU8) {
    if let Some(c) = CANCEL_AT.get().filter(|c| !byte.is_null() && label.starts_with(c.label)) {
        (c.fire)(byte);
    }
}

// ---- the poll-gap probe ----

static GAP_NS: AtomicU64 = AtomicU64::new(0);
static GAP_SET: AtomicBool = AtomicBool::new(false);
static MAX_NS: AtomicU64 = AtomicU64::new(0);
static GAPS: AtomicU64 = AtomicU64::new(0);

/// Switch the probe on at a threshold of `ms` (0: off). Its owner (Gravity exec's knob) sets it once per process.
pub fn set_probe_ms(ms: u64) {
    GAP_NS.store(ms.saturating_mul(1_000_000), Ordering::Relaxed);
    GAP_SET.store(true, Ordering::Release);
}

/// The switch was set ([`set_probe_ms`]), on or off.
#[inline]
pub fn probe_set() -> bool {
    GAP_SET.load(Ordering::Acquire)
}

#[inline]
pub fn probe_on() -> bool {
    GAP_NS.load(Ordering::Relaxed) != 0
}

/// The longest gap the probe logged in this process (ms) and the number it logged.
pub fn probe_max() -> (u64, u64) {
    (MAX_NS.load(Ordering::Relaxed) / 1_000_000, GAPS.load(Ordering::Relaxed))
}

/// A labelled host step, for the probe's report of what held a gap (`phase("pgrc dict region build")`); nothing when the
/// probe is off or outside a kernel call.
#[inline]
pub fn phase(label: &'static str) -> Phase {
    Phase(probe_on() && phase_enter(label))
}

#[must_use]
pub struct Phase(bool);

impl Drop for Phase {
    #[inline]
    fn drop(&mut self) {
        if self.0 {
            phase_exit();
        }
    }
}

const DEPTH: usize = 8;
const HELD: usize = 6;

type Site = Option<&'static core::panic::Location<'static>>;

/// One label's steps that ended in the current stretch (per depth).
#[derive(Clone, Copy)]
struct Held {
    label: &'static str,
    depth: u8,
    /// Steps.
    n: u32,
    /// ns of the stretch they held (a step that began before the stretch counts from its start).
    ns: u64,
    /// The longest step, whole.
    max: u64,
}

struct Probe {
    /// The monotonic ns of the thread's last poll inside a kernel call; 0 in none.
    last: u64,
    /// What began the stretch since `last` and its call site (a poll's, or the kernel call's).
    from: (&'static str, Site),
    /// The open labels and their starts, outermost first (`depth` may pass `DEPTH`: the deeper ones are not kept).
    open: [(&'static str, u64); DEPTH],
    depth: usize,
    /// Every labelled step that ended since the last poll, summed per label and depth (the first HELD labels).
    held: [Held; HELD],
    nheld: usize,
    /// The steps of labels past HELD: steps, ns.
    other: (u32, u64),
    /// ns of the stretch held by depth-0 steps that ended in it (depth-0 steps never overlap).
    cov0: u64,
    /// The thread's last CPU-time sample: (monotonic ns, CPU ns); taken at a poll when older than half the threshold, so
    /// a probe that polls at every back-edge reads the CPU clock a few times a second at most.
    cpu: (u64, u64),
    /// The thread's OS reading taken with that sample (testflow, D4128 / D4129): user / system CPU, faults, context switches,
    /// run-queue wait. `None` off Linux or when the read failed: the line then has no OS part.
    os: Option<pg_clock::posix::ThreadUse>,
}

impl Probe {
    const NEW: Probe = Probe {
        last: 0,
        from: ("", None),
        open: [("", 0); DEPTH],
        depth: 0,
        held: [Held { label: "", depth: 0, n: 0, ns: 0, max: 0 }; HELD],
        nheld: 0,
        other: (0, 0),
        cov0: 0,
        cpu: (0, 0),
        os: None,
    };

    /// A poll at `now` (or a kernel call's start or end, `end` at `site`) ends the stretch since the last one; the next
    /// stretch begins after `from`.
    fn mark(&mut self, now: u64, end: &'static str, site: Site, from: &'static str) {
        let at = GAP_NS.load(Ordering::Relaxed);
        if self.last != 0 {
            let gap = now.saturating_sub(self.last);
            if at != 0 && gap >= at {
                self.report(now, gap, end, site);
            }
            self.last = now;
        }
        self.begin(now, from, site, at);
    }

    /// A new stretch from `now`.
    fn begin(&mut self, now: u64, from: &'static str, site: Site, at: u64) {
        self.nheld = 0;
        self.other = (0, 0);
        self.cov0 = 0;
        self.from = (from, site);
        if self.cpu.0 == 0 || now.saturating_sub(self.cpu.0) >= at / 2 {
            self.cpu = (now, pg_clock::posix::thread_cpu_ns());
            self.os = pg_clock::posix::thread_use();
        }
    }

    fn add(&mut self, label: &'static str, depth: usize, ns: u64, whole: u64) {
        let depth = depth.min(u8::MAX as usize) as u8;
        for h in &mut self.held[..self.nheld] {
            if h.depth == depth && (core::ptr::eq(h.label, label) || h.label == label) {
                h.n = h.n.saturating_add(1);
                h.ns += ns;
                h.max = h.max.max(whole);
                return;
            }
        }
        if self.nheld < HELD {
            self.held[self.nheld] = Held { label, depth, n: 1, ns, max: whole };
            self.nheld += 1;
        } else {
            self.other = (self.other.0.saturating_add(1), self.other.1 + ns);
        }
    }

    /// One `poll-gap:` line on stderr (the server log): the gap, the thread, what ended it, the labels still open, the
    /// labelled steps that held it (summed per label), the part no label covers, where the stretch began and ended, and
    /// the thread's CPU time over it.
    ///
    /// The CPU clock and the OS reading are taken FIRST, before the line is written (testflow, K6 / TD-179, main
    /// 2026-10-12T19:43:19Z): read after it, the line's own write (its system time and the log page it dirties, 8 blocks
    /// out on a fresh page) landed in the OS part of the gap it reports and made a sleep read as an OS stall.
    #[cold]
    fn report(&mut self, now: u64, gap: u64, end: &str, site: Site) {
        let cpu = pg_clock::posix::thread_cpu_ns();
        let os_end = if self.os.is_some() { pg_clock::posix::thread_use() } else { None };
        MAX_NS.fetch_max(gap, Ordering::Relaxed);
        GAPS.fetch_add(1, Ordering::Relaxed);
        let ms = |ns: u64| ns / 1_000_000;
        let t = std::thread::current();
        let mut e = std::io::stderr().lock();
        let _ = write!(e, "poll-gap: {} ms without an abort poll on thread {}, ended by {}", ms(gap), t.name().unwrap_or("?"), end);
        let open = self.depth.min(DEPTH);
        if open > 0 {
            let _ = write!(e, "; inside");
            for (i, (label, t0)) in self.open[..open].iter().enumerate() {
                let _ = write!(e, "{} {} ({} ms)", if i == 0 { "" } else { " >" }, label, ms(now.saturating_sub(*t0)));
            }
        }
        let held = &mut self.held[..self.nheld];
        held.sort_by_key(|h| (h.depth, u64::MAX - h.ns));
        if !held.is_empty() {
            let _ = write!(e, "; held by");
            for (i, h) in held.iter().enumerate() {
                let _ = write!(e, "{} {} {} ms (depth {})", if i == 0 { "" } else { "," }, h.label, ms(h.ns), h.depth);
            }
            let _ = write!(e, "; steps");
            for (i, h) in held.iter().enumerate() {
                let _ = write!(e, "{} {} x{} longest {} ms", if i == 0 { "" } else { "," }, h.label, h.n, ms(h.max));
            }
            if self.other.0 > 0 {
                let _ = write!(e, ", {} more step(s) {} ms", self.other.0, ms(self.other.1));
            }
        }
        let mut covered = self.cov0;
        if self.depth > 0 {
            covered += now.saturating_sub(self.open[0].1.max(self.last));
        }
        let _ = write!(e, "; unlabelled {} ms", ms(gap.saturating_sub(covered)));
        let _ = write!(e, "; began after {}", self.from.0);
        if let Some(l) = self.from.1 {
            let _ = write!(e, " at {}:{}", l.file(), l.line());
        }
        if let Some(l) = site {
            let _ = write!(e, "; ended at {}:{}", l.file(), l.line());
        }
        // a 0 start reading is a failed read or a build with no thread CPU clock (wasm): no on-CPU part, not a false 0 ms
        if self.cpu.0 != 0 && self.cpu.1 != 0 && cpu >= self.cpu.1 {
            let _ = write!(e, "; on-CPU {} ms of {} ms", ms(cpu - self.cpu.1), ms(now.saturating_sub(self.cpu.0)));
        }
        if let (Some(a), Some(b)) = (self.os, os_end) {
            let _ = write_os_part(&mut e, &a, &b);
        }
        let _ = writeln!(e);
    }
}

/// The OS part of a poll-gap line, over the same window as its on-CPU part (testflow, D4128 / D4129, main
/// 2026-10-12T05:53:24Z): '; user <u> ms, system <s> ms; faults <n> minor, <m> major; switches <v> voluntary, <i> involuntary'
/// and, with schedstats, '; run-queue wait <r> ms'. Off-CPU time with major faults or run-queue wait was the OS (memory
/// pressure, oversubscribed CPUs), not a wait in code; system time with minor faults is reclaim or page zeroing.
pub(crate) fn write_os_part(
    e: &mut impl Write,
    a: &pg_clock::posix::ThreadUse,
    b: &pg_clock::posix::ThreadUse,
) -> std::io::Result<()> {
    let ms = |x: u64, y: u64| y.saturating_sub(x) / 1_000_000;
    write!(e, "; user {} ms, system {} ms", ms(a.user_ns, b.user_ns), ms(a.sys_ns, b.sys_ns))?;
    write!(e, "; faults {} minor, {} major", b.minflt.saturating_sub(a.minflt), b.majflt.saturating_sub(a.majflt))?;
    write!(e, "; blocks {} in, {} out", b.inblock.saturating_sub(a.inblock), b.oublock.saturating_sub(a.oublock))?;
    write!(e, "; switches {} voluntary, {} involuntary", b.nvcsw.saturating_sub(a.nvcsw), b.nivcsw.saturating_sub(a.nivcsw))?;
    if let (Some(x), Some(y)) = (a.rq_ns, b.rq_ns) {
        write!(e, "; run-queue wait {} ms", ms(x, y))?;
    }
    Ok(())
}

#[cold]
fn probe_enter(site: &'static core::panic::Location<'static>) -> u64 {
    let now = pg_clock::mono_ns().max(1);
    PROBE.with(|p| {
        let mut p = p.borrow_mut();
        let prev = p.last;
        if prev != 0 {
            p.mark(now, "a nested kernel call's start", Some(site), "a nested kernel call's start");
        } else {
            p.last = now;
            let at = GAP_NS.load(Ordering::Relaxed);
            p.begin(now, "the kernel call's start", Some(site), at);
        }
        prev
    })
}

#[cold]
fn probe_exit(prev_last: u64, site: &'static core::panic::Location<'static>) {
    let now = pg_clock::mono_ns().max(1);
    PROBE.with(|p| {
        let mut p = p.borrow_mut();
        p.mark(now, "the kernel call's end", Some(site), "a nested kernel call's end");
        p.last = if prev_last != 0 { now } else { 0 };
    })
}

#[cold]
fn polled_slow(site: &'static core::panic::Location<'static>) {
    let now = pg_clock::mono_ns().max(1);
    PROBE.with(|p| p.borrow_mut().mark(now, "a poll", Some(site), "a poll"))
}

/// An interruptible wait's start (D3962): a poll, named as the wait's.
#[cold]
fn wait_slow(site: &'static core::panic::Location<'static>) {
    let now = pg_clock::mono_ns().max(1);
    PROBE.with(|p| p.borrow_mut().mark(now, "an interruptible wait's start", Some(site), "an interruptible wait's start"))
}

/// An interruptible wait's end (D3962): inside a kernel call, a new stretch from `now` (the wait is no gap), begun at the
/// wait's site.
#[cold]
fn woke_slow(site: &'static core::panic::Location<'static>) {
    let now = pg_clock::mono_ns().max(1);
    PROBE.with(|p| {
        let mut p = p.borrow_mut();
        if p.last != 0 {
            p.last = now;
            let at = GAP_NS.load(Ordering::Relaxed);
            p.begin(now, "an interruptible wait's end", Some(site), at);
        }
    })
}

#[cold]
fn phase_enter(label: &'static str) -> bool {
    let now = pg_clock::mono_ns();
    PROBE.with(|p| {
        let mut p = p.borrow_mut();
        if p.last == 0 {
            return false;
        }
        let d = p.depth;
        if d < DEPTH {
            p.open[d] = (label, now);
        }
        p.depth = d + 1;
        true
    })
}

#[cold]
fn phase_exit() {
    let now = pg_clock::mono_ns();
    PROBE.with(|p| {
        let mut p = p.borrow_mut();
        let Some(d) = p.depth.checked_sub(1) else { return };
        p.depth = d;
        if d >= DEPTH {
            return;
        }
        let (label, t0) = p.open[d];
        // the step's part in this stretch: a step that began before the last poll counts from that poll
        let ns = now.saturating_sub(t0.max(p.last));
        if d == 0 {
            p.cov0 += ns;
        }
        p.add(label, d, ns, now.saturating_sub(t0));
    })
}
