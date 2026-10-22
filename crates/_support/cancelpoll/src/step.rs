//! C9: a polled host step. [`step`] opens one on the byte the running kernel call lent ([`crate::scoped`]); every unit
//! of its work ticks it ([`Step::tick`], [`Step::each`], [`Step::pieces`], [`Step::batch`], [`sort_by`]), and a growing
//! buffer moves through it ([`Step::reserve`]). A tick is one load of the byte and a branch: the host-poll switch and
//! the probe were folded into the byte and its mask when the kernel call began. A set abort bit stops the step with
//! [`Canceled`], which the host's error path reports as the kernel's cancel ([`crate::seen`]); the owner raises
//! PostgreSQL's error at its next check.
use core::cell::Cell;
use core::cmp::Ordering as CmpOrdering;
use core::marker::PhantomData;
use core::ops::Range;
use core::panic::Location;
use core::sync::atomic::{AtomicU8, Ordering};

use crate::{ABORT_BIT, PROBE_BIT};

/// The largest single unit `pieces` hands out: an I/O read, a zero fill, an encode (D4016: 1 MiB bounds a read's stall on
/// a slow device).
pub const PIECE: usize = 1 << 20;

/// Units between two ticks of `batch`: only for a loop of about 100 ns or less per unit, or one that vectorizes (a tick
/// per unit costs 3 instructions and stops the vectorizer; c8g, lanes/keyrep/c9-design.md).
pub const BATCH: usize = 1 << 12;

/// [`sort_by`] sorts runs of this many elements, then merges them pairwise.
pub const SORT_RUN: usize = 1 << 16;

/// Checker 2 (debug builds only): a live step's stretch without a poll (a tick, the kernel call's start or end, an
/// interruptible wait's start or end) at least this long logs one `step-gap:` line, once per label per process. Debug
/// builds run 10-50x slower than release: 2000 ms is about 40-200 ms of release time (`set_step_gap_ms`, Gravity's
/// `PGRUST_GRAVITY_STEP_GAP_MS`, calibrates it).
pub const STEP_GAP_MS: u64 = 2000;

static NEVER: AtomicU8 = AtomicU8::new(0);
static PROBING: AtomicU8 = AtomicU8::new(PROBE_BIT);

/// What a step reads: `test` is loaded at every tick and `mask` picks the bits that leave the fast path; `real` is the
/// statement's byte. Probe on: `test` always carries the probe bit, so every tick marks the probe and then reads `real`.
#[derive(Clone, Copy)]
pub(crate) struct Lent {
    test: *const AtomicU8,
    real: *const AtomicU8,
    mask: u8,
}

impl Lent {
    pub(crate) const OFF: Lent = Lent { test: core::ptr::null(), real: core::ptr::null(), mask: 0 };

    /// A kernel call's lent byte: off under the host-poll switch (`set_host_polls`, which covers its host code's loops).
    #[inline]
    pub(crate) fn of(byte: *const AtomicU8) -> Lent {
        if crate::host_polls_off() {
            return Lent::OFF;
        }
        Lent::held(byte)
    }

    /// A byte its caller holds outside a kernel call ([`step_on`]: the leader's drain, the merge owner's fold), which the
    /// host-poll switch never covered.
    #[inline]
    fn held(byte: *const AtomicU8) -> Lent {
        if byte.is_null() {
            return Lent::OFF;
        }
        if crate::probe_on() {
            return Lent { test: &PROBING, real: byte, mask: PROBE_BIT };
        }
        Lent { test: byte, real: byte, mask: ABORT_BIT }
    }
}

thread_local! {
    static LENT: Cell<Lent> = const { Cell::new(Lent::OFF) };
}

#[cfg(debug_assertions)]
mod gap {
    use core::cell::Cell;
    use core::panic::Location;
    use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
    use std::io::Write;

    static GAP_MS: AtomicU64 = AtomicU64::new(super::STEP_GAP_MS);
    static GAPS: AtomicU64 = AtomicU64::new(0);
    /// The labels logged so far, by address (lock-free; past the first 64 labels every gap is logged).
    static SAID: [AtomicPtr<u8>; 64] = [const { AtomicPtr::new(core::ptr::null_mut()) }; 64];

    /// `label`'s first gap of the process.
    fn first(label: &'static str) -> bool {
        let p = label.as_ptr() as *mut u8;
        for s in &SAID {
            let mut cur = s.load(Ordering::Acquire);
            if cur.is_null() {
                cur = match s.compare_exchange(core::ptr::null_mut(), p, Ordering::AcqRel, Ordering::Acquire) {
                    Ok(_) => return true,
                    Err(other) => other,
                };
            }
            if cur == p {
                return false;
            }
        }
        true
    }

    thread_local! {
        /// The monotonic ns of this thread's last poll; 0: none yet.
        static LAST: Cell<u64> = const { Cell::new(0) };
    }

    pub(crate) fn set_ms(ms: u64) {
        GAP_MS.store(ms, Ordering::Relaxed);
    }

    pub(crate) fn count() -> u64 {
        GAPS.load(Ordering::Relaxed)
    }

    pub(crate) fn now() -> u64 {
        pg_clock::mono_ns().max(1)
    }

    /// A poll on this thread at `now`.
    pub(crate) fn polled_at(now: u64) {
        LAST.set(now);
    }

    /// A poll of the step `label` (open since `since`) at `site`: the stretch since this thread's last poll (no earlier
    /// than the step's open), logged when it reaches the threshold.
    pub(crate) fn poll(label: &'static str, since: u64, end: &str, site: &'static Location<'static>) {
        let now = now();
        let from = LAST.replace(now).max(since);
        let gap = now.saturating_sub(from);
        let at = GAP_MS.load(Ordering::Relaxed).saturating_mul(1_000_000);
        if at != 0 && gap >= at {
            report(label, gap, end, site);
        }
    }

    #[cold]
    #[inline(never)]
    fn report(label: &'static str, gap: u64, end: &str, site: &'static Location<'static>) {
        GAPS.fetch_add(1, Ordering::Relaxed);
        if !first(label) {
            return;
        }
        let t = std::thread::current();
        let _ = writeln!(
            std::io::stderr().lock(),
            "step-gap: {} ms without a poll on thread {}, ended by {} at {}:{}; step: {}",
            gap / 1_000_000,
            t.name().unwrap_or("?"),
            end,
            site.file(),
            site.line(),
            label
        );
    }
}

/// Checker 2's threshold in ms (0: off; debug builds only, a no-op in release).
pub fn set_step_gap_ms(ms: u64) {
    #[cfg(debug_assertions)]
    gap::set_ms(ms);
    #[cfg(not(debug_assertions))]
    let _ = ms;
}

/// The step gaps checker 2 found in this process (every one, the logged and the repeated labels); 0 in release.
pub fn step_gaps() -> u64 {
    #[cfg(debug_assertions)]
    return gap::count();
    #[cfg(not(debug_assertions))]
    0
}

/// A poll outside a Step (a kernel call's start or end, `canceled`, an interruptible wait): checker 2's stretch restarts.
#[inline(always)]
pub(crate) fn debug_polled() {
    #[cfg(debug_assertions)]
    gap::polled_at(gap::now());
}

/// The step reading of the kernel call [`crate::scoped`] enters (its previous one back at the call's end).
#[inline]
pub(crate) fn lend(byte: *const AtomicU8) -> Lent {
    LENT.replace(Lent::of(byte))
}

#[inline]
pub(crate) fn restore(prev: Lent) {
    LENT.set(prev);
}

/// The cancel a tick saw: the step stops and passes it up (`?`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Canceled;

/// A labelled host step that polls its statement's abort byte. Opened on the stack inside the kernel call whose byte it
/// reads, never kept past it (RQ fails a `Step` in a field, a static or a return type).
#[must_use]
pub struct Step {
    test: *const AtomicU8,
    real: *const AtomicU8,
    mask: u8,
    label: bool,
    /// On the byte the running kernel call lent: its stop is that call's ([`crate::seen`]). A step on a held byte
    /// (`step_on`, `step_static`) leaves the call's stop alone: its caller holds the byte and raises the stop itself.
    lent: bool,
    #[cfg(debug_assertions)]
    scope: *const AtomicU8,
    #[cfg(debug_assertions)]
    debug: GapState,
    _not_send: PhantomData<*const ()>,
}

/// A step's checker-2 state (debug builds only): its label, where and when it opened.
#[cfg(debug_assertions)]
struct GapState {
    name: &'static str,
    site: &'static Location<'static>,
    since: u64,
}

/// A step on the byte the running kernel call lent; outside one (or with host polls off) a step that never stops.
#[inline]
#[cfg_attr(debug_assertions, track_caller)]
pub fn step(label: &'static str) -> Step {
    let l = LENT.get();
    if crate::cancel_at_on() {
        crate::cancel_at(label, l.real);
    }
    Step::open(l, label, true)
}

/// A step on `byte`, a statement's abort byte the caller holds outside [`crate::scoped`] (the leader's drain, the
/// parallel merge's owner phase). The host-poll switch ([`crate::set_host_polls`]) does not cover it: those stops polled
/// their byte before the switch existed. Its stop is the caller's to raise and never the running kernel call's: it
/// leaves [`crate::seen`] as it is (set outside a call, `seen` stayed set on the thread, and the host error path then
/// dropped the statement's own error for a cancel).
/// # Safety — `byte` is null or a live abort byte that outlives the step.
#[inline]
#[cfg_attr(debug_assertions, track_caller)]
pub unsafe fn step_on(byte: *const u8, label: &'static str) -> Step {
    Step::open(Lent::held(byte as *const AtomicU8), label, false)
}

/// [`step_on`] over a byte that lives for the process (a backend's own abort byte): safe, as the byte outlives any step.
#[inline]
pub fn step_static(byte: &'static AtomicU8, label: &'static str) -> Step {
    Step::open(Lent::held(byte), label, false)
}

impl Step {
    #[inline]
    #[cfg_attr(debug_assertions, track_caller)]
    fn open(l: Lent, label: &'static str, lent: bool) -> Step {
        let test = if l.mask == 0 { &NEVER as *const AtomicU8 } else { l.test };
        Step {
            test,
            real: l.real,
            mask: l.mask,
            label: l.mask & PROBE_BIT != 0 && crate::phase_enter(label),
            lent,
            #[cfg(debug_assertions)]
            scope: if lent { l.real } else { core::ptr::null() },
            #[cfg(debug_assertions)]
            debug: GapState { name: label, site: Location::caller(), since: gap::now() },
            _not_send: PhantomData,
        }
    }

    /// One unit done: `Err` when the statement's abort bit is set.
    #[inline(always)]
    #[track_caller]
    pub fn tick(&mut self) -> Result<(), Canceled> {
        self.tick_at(Location::caller())
    }

    #[inline(always)]
    fn tick_at(&mut self, site: &'static Location<'static>) -> Result<(), Canceled> {
        #[cfg(debug_assertions)]
        self.check_scope();
        #[cfg(debug_assertions)]
        if self.mask != 0 {
            gap::poll(self.debug.name, self.debug.since, "a tick", site);
        }
        // SAFETY: `test` is a static or the byte of the kernel call this step was opened in, which outlives it
        let v = unsafe { &*self.test }.load(Ordering::Acquire);
        if v & self.mask == 0 {
            return Ok(());
        }
        self.slow(v, site)
    }

    #[cold]
    #[inline(never)]
    fn slow(&mut self, v: u8, site: &'static Location<'static>) -> Result<(), Canceled> {
        let v = if v & PROBE_BIT != 0 {
            crate::polled_slow(site);
            // SAFETY: as `tick_at`; the probe's `test` is a static, `real` the statement's byte
            unsafe { &*self.real }.load(Ordering::Acquire)
        } else {
            v
        };
        if v & ABORT_BIT == 0 {
            return Ok(());
        }
        if self.lent {
            crate::mark_seen();
        }
        Err(Canceled)
    }

    #[cfg(debug_assertions)]
    fn check_scope(&self) {
        if !self.scope.is_null() {
            let now = LENT.get().real;
            assert!(core::ptr::eq(now, self.scope), "cancelpoll: a Step ticked outside the kernel call it was opened in");
        }
    }

    /// `it`'s items, a tick before each: a cancel is an `Err` item (and the end), never a silent early end.
    #[inline]
    #[track_caller]
    pub fn each<I: IntoIterator>(&mut self, it: I) -> Each<'_, I::IntoIter> {
        Each { step: self, it: it.into_iter(), site: Location::caller(), done: false }
    }

    /// `f` over `0..len` in pieces of at most `piece` (`PIECE` unless the site says why), a tick before each.
    #[inline]
    #[track_caller]
    pub fn pieces<E: From<Canceled>>(&mut self, len: usize, piece: usize, mut f: impl FnMut(Range<usize>) -> Result<(), E>) -> Result<(), E> {
        let site = Location::caller();
        let piece = piece.max(1);
        let mut at = 0;
        while at < len {
            self.tick_at(site)?;
            let end = at + piece.min(len - at);
            f(at..end)?;
            at = end;
        }
        Ok(())
    }

    /// Unit `i` of a loop of about 100 ns or less per unit: a tick every `BATCH` units.
    #[inline(always)]
    #[track_caller]
    pub fn batch(&mut self, i: usize) -> Result<(), Canceled> {
        if i & (BATCH - 1) == BATCH - 1 {
            return self.tick_at(Location::caller());
        }
        Ok(())
    }

    /// `v` with room for `more` elements past its length. A growth moves `v`'s elements into the new buffer `PIECE` bytes
    /// a tick: a reallocation copies the whole run and faults in its new pages in one call, hundreds of ms past a few
    /// hundred MB under memory pressure. Nothing moves while the capacity holds; a stop leaves `v` as it was.
    #[track_caller]
    pub fn reserve<T: Copy>(&mut self, v: &mut Vec<T>, more: usize) -> Result<(), Canceled> {
        if v.capacity() - v.len() >= more {
            return Ok(());
        }
        let mut grown = Vec::with_capacity((v.len() + more).max(v.capacity().saturating_mul(2)));
        let piece = (PIECE / core::mem::size_of::<T>().max(1)).max(1);
        self.pieces(v.len(), piece, |r| {
            grown.extend_from_slice(&v[r]);
            Ok::<(), Canceled>(())
        })?;
        *v = grown;
        Ok(())
    }
}

/// The longest park between two tries of a held lock ([`lock_or_cancel`]): a cancel ends the wait within one park and a
/// tick, far under the poll-gap gate's 250 ms.
pub const LOCK_PARK: core::time::Duration = core::time::Duration::from_millis(1);

/// F8 / D4210 (main 2026-10-12T19:09:08Z): `m` locked, its wait polled through `step`. A free lock is taken at once (one
/// `try_lock`); a held one is tried again after a tick and a short park (50 us, doubling to [`LOCK_PARK`]), so the
/// statement's cancel ends the wait (`Err`, the step's stop) instead of waiting out the holder. A poisoned lock is taken as
/// it is (the callers' rule: a panicked holder's state is theirs to judge). The wait form for a lock whose holder can sit
/// in a long write: `Mutex::lock` there polled nothing (D4210: a spill stage's stored-stream lock waited 0.8-2.5 s
/// off-CPU behind another worker's stalled epoch write). The park is pgsync's sleep (a TimedPark under the DST scheduler).
#[track_caller]
pub fn lock_or_cancel<'m, T>(step: &mut Step, m: &'m pgsync::Mutex<T>) -> Result<pgsync::MutexGuard<'m, T>, Canceled> {
    let mut park = core::time::Duration::from_micros(50);
    loop {
        match m.try_lock() {
            Ok(g) => return Ok(g),
            Err(pgsync::TryLockError::Poisoned(p)) => return Ok(p.into_inner()),
            Err(pgsync::TryLockError::WouldBlock) => {}
        }
        step.tick()?;
        pgsync::thread::sleep(park);
        park = (park * 2).min(LOCK_PARK);
    }
}

impl Drop for Step {
    #[inline]
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        if self.mask != 0 {
            gap::poll(self.debug.name, self.debug.since, "the end of the step opened", self.debug.site);
        }
        if self.label {
            crate::phase_exit();
        }
    }
}

/// [`Step::each`]'s iterator.
pub struct Each<'s, I> {
    step: &'s mut Step,
    it: I,
    site: &'static Location<'static>,
    done: bool,
}

impl<I: Iterator> Iterator for Each<'_, I> {
    type Item = Result<I::Item, Canceled>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        if let Err(e) = self.step.tick_at(self.site) {
            self.done = true;
            return Some(Err(e));
        }
        self.it.next().map(Ok)
    }
}

/// `v` sorted by `cmp` under `step`: runs of `SORT_RUN` sorted with a tick before each, then merged pairwise into
/// `scratch` and back with a tick per `BATCH` elements. `scratch` holds at least `v.len()` when `v.len() > SORT_RUN`
/// (the caller allocates it). `Err`: `v` holds its elements in no order.
#[track_caller]
pub fn sort_by<T: Copy>(step: &mut Step, v: &mut [T], scratch: &mut [T], cmp: impl Fn(&T, &T) -> CmpOrdering) -> Result<(), Canceled> {
    let site = Location::caller();
    for run in v.chunks_mut(SORT_RUN) {
        step.tick_at(site)?;
        run.sort_unstable_by(&cmp);
    }
    let n = v.len();
    if n <= SORT_RUN {
        return Ok(());
    }
    let scratch = &mut scratch[..n];
    let (mut in_v, mut width, mut r) = (true, SORT_RUN, Ok(()));
    'passes: while width < n {
        let (src, dst): (&[T], &mut [T]) = if in_v { (&*v, &mut *scratch) } else { (&*scratch, &mut *v) };
        let mut k = 0usize;
        for start in (0..n).step_by(2 * width) {
            let (mid, end) = ((start + width).min(n), (start + 2 * width).min(n));
            let (mut i, mut j) = (start, mid);
            for slot in &mut dst[start..end] {
                if let Err(e) = step.batch(k) {
                    r = Err(e);
                    break 'passes;
                }
                k += 1;
                *slot = if j >= end || i < mid && cmp(&src[i], &src[j]) != CmpOrdering::Greater {
                    i += 1;
                    src[i - 1]
                } else {
                    j += 1;
                    src[j - 1]
                };
            }
        }
        in_v = !in_v;
        width *= 2;
    }
    // a canceled pass leaves its source whole: `v` gets it back
    if !in_v {
        v.copy_from_slice(scratch);
    }
    r
}
