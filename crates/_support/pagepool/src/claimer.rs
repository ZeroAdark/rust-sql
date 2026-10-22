//! Prefaulting ahead of demand, never on a query thread: the `pg:poolclaim` thread keeps the warm-zero target met.

use crate::reserve::zones;
use crate::superpage::{self, State, SUPER};
use crate::{os, stats};
use crate::sync::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering::*};

static THREAD: AtomicPtr<std::thread::Thread> = AtomicPtr::new(core::ptr::null_mut());
static STOP: AtomicBool = AtomicBool::new(false);
static PENDING: crate::Line<AtomicBool> = crate::Line(AtomicBool::new(false));
/// pool_warm_zero, doubled by a miss after each met target (to pool_warm_zero_max, half the cap), halved at idle.
static TARGET: crate::Line<AtomicUsize> = crate::Line(AtomicUsize::new(0));
static MET: crate::Line<AtomicBool> = crate::Line(AtomicBool::new(false));
static LAST_MISS: crate::Line<AtomicU32> = crate::Line(AtomicU32::new(0));
/// Tests: the claimer fills nothing while held (`hooks::claimer_hold`), so a burst's misses do not race its refill.
pub(crate) static HOLD: AtomicBool = AtomicBool::new(false);
/// The ladder's room less the claimer's fills since, in the low `ROOM_BITS` (`NO_GATE`: no gate), under the generation of
/// the sample that set it: a failed fill's refund lands only in the sample it took the room from, never on a later one.
static ROOM: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(NO_GATE));
const ROOM_BITS: u32 = 48;
const NO_GATE: u64 = (1 << ROOM_BITS) - 1;

/// The claimer's prefault follows the memory ladder's raise rules: the server's ladder sets, at each sample, the bytes it
/// may prefault until the next (`claim_room`: 0 while pressed or while a release is landing). usize::MAX = no gate.
pub fn set_claim_room(bytes: usize) {
    let r = (bytes as u64).min(NO_GATE);
    let _ = ROOM.fetch_update(Relaxed, Relaxed, |w| Some(((w >> ROOM_BITS).wrapping_add(1) << ROOM_BITS) | r));
    if bytes >= SUPER && crate::state::warm_gap() > 0 {
        unpark();
    }
}

/// The claimer's raise rule at a sample: nothing while a release is `landing` or the `read` is pressed (past LOW under
/// the `line`, past the line itself included), else the room under the line (the window's lowest) past `top`.
pub fn claim_room(line: u64, read: u64, window_low: Option<u64>, top: u64, landing: u64) -> usize {
    let room = line.saturating_sub(read);
    if landing != 0 {
        return 0;
    }
    usize::try_from(room.min(window_low.unwrap_or(room)).saturating_sub(top)).unwrap_or(usize::MAX - 1)
}

/// The room the ladder's last sample left under its line (`set_claim_room`; usize::MAX = no gate), less the claimer's
/// fills since (pppress card 6: `purger::pressed`).
#[inline]
pub(crate) fn room() -> usize {
    match ROOM.load(Relaxed) & NO_GATE {
        NO_GATE => usize::MAX,
        r => r as usize,
    }
}

/// A superpage of room taken: the word it came from (its sample's generation), None when there is none to take.
fn take_room() -> Option<u64> {
    // D4872: nothing while the host is pressed (the page cache pays for a prefault)
    if crate::purger::landing_any() || crate::warmres::shed_pending() || crate::purger::host_pressed() {
        return None;
    }
    ROOM.fetch_update(Relaxed, Relaxed, |w| match w & NO_GATE {
        NO_GATE => Some(w),
        r => r.checked_sub(SUPER as u64).map(|r| (w & !NO_GATE) | r),
    })
    .ok()
}

/// A failed fill's room back into the sample `taken` came from; a later sample's room stands as the ladder set it.
fn give_room(taken: u64) {
    let _ = ROOM.fetch_update(Relaxed, Relaxed, |w| {
        let r = w & NO_GATE;
        (w >> ROOM_BITS == taken >> ROOM_BITS && r != NO_GATE).then(|| (w & !NO_GATE) | (r + SUPER as u64).min(NO_GATE - 1))
    });
}

/// A fill may run: no release landing, the host not pressed (D4872) and a superpage of room (none taken: a recycle adds no
/// memory).
fn room_open() -> bool {
    !crate::purger::landing_any() && !crate::warmres::shed_pending() && !crate::purger::host_pressed() && room() >= SUPER
}

pub(crate) fn init() {
    TARGET.store(zones().cfg.warm_zero, Relaxed);
}

#[inline]
pub(crate) fn target() -> usize {
    TARGET.load(Relaxed)
}

/// `PGRUST_POOL_RESERVE=1`: the reserve sets the target (`warmres`), E1d's doubling and halving do not run.
pub(crate) fn set_target(bytes: usize) {
    TARGET.store(bytes, Relaxed);
}

#[cold]
pub(crate) fn missed(class: crate::Class, supers: u32) {
    stats::WARM_MISSES.fetch_add(1, Relaxed);
    LAST_MISS.store(crate::purger::tick(), Relaxed);
    crate::warmres::missed(class, supers);
    if crate::warmres::on() {
        maybe_wake();
        return;
    }
    let cfg = zones().cfg;
    if cfg.warm_zero == 0 || !MET.swap(false, Relaxed) {
        return;
    }
    let mut max = cfg.warm_zero_max.max(cfg.warm_zero);
    let cap = stats::cap();
    if cap != 0 {
        max = max.min(cap / 2).max(cfg.warm_zero);
    }
    let t = TARGET.load(Relaxed);
    TARGET.store(t.saturating_mul(2).min(max).max(t), Relaxed);
    maybe_wake();
    crate::purger::wake();
}

/// No miss and no halving for `window_ms`: halve the target, down to its floor.
pub(crate) fn decay(now: u32, window_ms: u32, last: &AtomicU32) -> bool {
    if crate::warmres::on() {
        return false;
    }
    let floor = zones().cfg.warm_zero;
    let t = TARGET.load(Relaxed);
    if t <= floor || now.wrapping_sub(LAST_MISS.load(Relaxed)) < window_ms || now.wrapping_sub(last.load(Relaxed)) < window_ms {
        return false;
    }
    TARGET.store((t / 2).next_multiple_of(SUPER).max(floor).min(t), Relaxed);
    last.store(now, Relaxed);
    true
}

/// pagepool P3 (F1): `PGRUST_POOL_TOUCH` (default ON on this branch, user 10-17; `=0` turns it off), read once
/// (`pagepool::init`; 0: unread, 1: off, 2: on): the claimer faults a
/// Cold superpage in by a store per page instead of MADV_POPULATE_WRITE. A populate holds mmap_lock for read through the
/// whole fault, and a THP fault in a MADV_HUGEPAGE range compacts and reclaims on the calling thread (lane box, a 10 GB
/// join suite at W8 / W32, THP defrag madvise: 18-20 % of populates did, read holds up to 66 ms, every mmap writer of
/// the process queued behind them). A store's fault takes only its VMA's lock (per-VMA locks), so the compaction blocks
/// nobody else's mmap / munmap.
static TOUCH: AtomicU8 = AtomicU8::new(0);

pub(crate) fn touch_on() -> bool {
    match TOUCH.load(Relaxed) {
        0 => {
            let v = if os::env_on_by_default(c"PGRUST_POOL_TOUCH") { 2 } else { 1 };
            TOUCH.store(v, Relaxed);
            v == 2
        }
        v => v == 2,
    }
}

/// The touch switch as if the environment said `on` (before the claimer starts).
pub(crate) fn force_touch(on: bool) {
    TOUCH.store(if on { 2 } else { 1 }, Relaxed);
}

/// Fault one Cold superpage in (`touch_on`: a zero store per page; else MADV_POPULATE_WRITE). False: the populate failed.
/// # Safety
/// `at` is a Cold superpage the caller holds: open read-write, its pages zero, nobody else touching it.
unsafe fn prefault(at: usize) -> bool {
    if !touch_on() {
        // SAFETY: the caller's contract.
        return unsafe { os::populate(at, SUPER) };
    }
    let page = os::page_size();
    let mut p = at;
    while p < at + SUPER {
        // SAFETY: inside the caller's superpage of zero pages: the zero stored is the byte there. A store, not a load
        // first (a read fault would map the zero page and the store fault again).
        unsafe { core::ptr::write_volatile(p as *mut u8, 0) };
        p += page;
    }
    true
}

/// Claim one Cold superpage, fault it in and make it WarmZero (`PGRUST_POOL_RESERVE=1`: a free WarmDirty one zeroed in
/// place first, never a Cold one while one is free). False: none left, the cap, or the fault failed.
pub(crate) fn fill_one() -> bool {
    if crate::warmres::on() && superpage::count(State::WarmDirty) > 0 {
        if !room_open() {
            stats::CLAIM_HELD.fetch_add(1, Relaxed);
            return false;
        }
        if crate::warmres::recycle_one() {
            return true;
        }
    }
    if !stats::under_cap(SUPER) {
        return false;
    }
    let Some(taken) = take_room() else {
        stats::CLAIM_HELD.fetch_add(1, Relaxed);
        return false;
    };
    let Some(i) = superpage::take_one(State::Cold) else {
        give_room(taken);
        return false;
    };
    stats::claim(SUPER as isize);
    let t0 = pg_clock::mono_ns();
    // SAFETY: a Cold superpage we hold: open read-write, its pages zero.
    if !unsafe { prefault(superpage::addr_of(i)) } {
        stats::claim(-(SUPER as isize));
        superpage::put(i, State::Cold);
        give_room(taken);
        return false;
    }
    crate::warmres::populated(pg_clock::mono_ns().saturating_sub(t0));
    superpage::put(i, State::WarmZero);
    stats::PREFAULTED.fetch_add(SUPER as u64, Relaxed);
    true
}

fn run() {
    let mut pinned = false;
    while !STOP.load(Acquire) {
        // before a prefault: it places the pages on this thread's node
        crate::affinity::follow(&mut pinned);
        PENDING.store(false, Relaxed);
        crate::warmres::tick(crate::purger::tick());
        while !STOP.load(Relaxed) && !HOLD.load(Relaxed) && crate::state::warm_gap() > 0 && fill_one() {}
        if !HOLD.load(Relaxed) && crate::state::warm_gap() <= 0 {
            MET.store(true, Relaxed);
            crate::warmres::refilled();
            crate::warmres::trim_spare(crate::purger::tick());
        }
        std::thread::park_timeout(core::time::Duration::from_secs(1));
    }
}

pub(crate) fn start() -> bool {
    if zones().cfg.warm_zero == 0 || !THREAD.load(Acquire).is_null() {
        return true;
    }
    spawn()
}

#[cfg(target_os = "linux")]
fn spawn() -> bool {
    STOP.store(false, Relaxed);
    let Ok(h) = std::thread::Builder::new().name("pg:poolclaim".into()).spawn(pagegive::pinned(run)) else { return false };
    let t = Box::into_raw(Box::new(h.thread().clone()));
    if THREAD.compare_exchange(core::ptr::null_mut(), t, AcqRel, Acquire).is_err() {
        // SAFETY: ours, never published.
        drop(unsafe { Box::from_raw(t) });
    }
    true
}

/// macOS / others: no pool threads (the plan's decision 11); prefill and purge_now run inline.
#[cfg(not(target_os = "linux"))]
fn spawn() -> bool {
    true
}

pub(crate) fn stop() {
    STOP.store(true, Release);
    unpark();
}

pub(crate) fn met() -> bool {
    MET.load(Relaxed)
}

pub(crate) fn unpark() {
    let t = THREAD.load(Acquire);
    if !t.is_null() {
        // SAFETY: published once by `spawn`, never freed.
        unsafe { (*t).unpark() };
    }
}

/// A take found warm-zero under half its target: wake the claimer (at most once until it runs).
#[inline]
pub(crate) fn maybe_wake() {
    let z = zones();
    if z.cfg.warm_zero > 0 && superpage::count(State::WarmZero) * SUPER < target() / 2 && !PENDING.swap(true, Relaxed) {
        unpark();
    }
}

static PREFILLED: AtomicUsize = AtomicUsize::new(0);
static FILLED: AtomicUsize = AtomicUsize::new(0);

/// Claim and fault in `bytes` now (a startup claim; off by default), on `threads` threads. The bytes claimed.
pub fn prefill(bytes: usize, threads: u32) -> usize {
    if !crate::on() {
        return 0;
    }
    let n = bytes / SUPER;
    PREFILLED.store(0, Relaxed);
    FILLED.store(0, Relaxed);
    let work = move || {
        while PREFILLED.fetch_add(1, Relaxed) < n {
            if fill_one() {
                FILLED.fetch_add(1, Relaxed);
            } else {
                PREFILLED.store(n, Relaxed);
                break;
            }
        }
    };
    #[cfg(target_os = "linux")]
    {
        let done = std::sync::Arc::new(AtomicUsize::new(0));
        let me = std::thread::current();
        let mut spawned = 0;
        for _ in 1..threads.max(1) {
            let (done, me) = (done.clone(), me.clone());
            let ok = std::thread::Builder::new().name("pg:poolfill".into()).spawn(pagegive::pinned(move || {
                crate::affinity::follow(&mut false);
                work();
                done.fetch_add(1, Release);
                me.unpark();
            }));
            spawned += ok.is_ok() as usize;
        }
        work();
        while done.load(Acquire) < spawned {
            std::thread::park_timeout(core::time::Duration::from_millis(10));
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = threads;
        work();
    }
    FILLED.load(Relaxed) * SUPER
}
