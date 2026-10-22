//! The only code that gives memory back to the OS: on pressure (`set_pressure`, `purge_now`), or by age past the warm
//! cap when `pool_release_mode` asks for it. Whole free superpages go back by remap (`pagegive::give_back`), contiguous
//! ones in batches of at most `purge_batch`, never a MADV_DONTNEED over a whole 2 MiB range; under pressure, the free
//! runs of superpages still in use too (`state::release_free_runs`).

use crate::central;
use crate::reserve::zones;
use crate::superpage::{self, State, SUPER};
use crate::{api, os, run, span, state, stats, Class};
use core::cell::Cell;
use crate::sync::{tls as thread_local, AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering::*};

#[derive(Clone, Copy, Debug)]
pub enum Pressure {
    None,
    Release { bytes: usize, class: Option<Class> },
    /// Release every warm byte.
    Floor,
}

static ASK: AtomicUsize = AtomicUsize::new(0);
static ASK_CLASS: AtomicU8 = AtomicU8::new(NO_CLASS);
const NO_CLASS: u8 = 0xff;
/// `PoolAlloc`'s dead threads' runs with every block home go back on this cadence (or an ask), never a session's end.
const SWEEP_MS: u32 = 1_000;
static THREAD: AtomicPtr<std::thread::Thread> = AtomicPtr::new(core::ptr::null_mut());
static STOP: AtomicBool = AtomicBool::new(false);
static PURGING: crate::Line<AtomicU32> = crate::Line(AtomicU32::new(0));
static LAST_CLAIM: crate::Line<AtomicU32> = crate::Line(AtomicU32::new(0));
static RETURN_ASK: AtomicBool = AtomicBool::new(false);
static RETURNING: AtomicBool = AtomicBool::new(false);
/// pppress card 10: pages of ranges an idle tail cut gives back whose superpage is still PMD-mapped; the purger splits
/// their folios (D4760's MADV_COLD, `os::split_partial`) off the sessions' threads, which give the range back on their
/// next slice. 0: a free slot.
static SPLIT_ASKS: [AtomicUsize; SPLIT_SLOTS] = [const { AtomicUsize::new(0) }; SPLIT_SLOTS];
const SPLIT_SLOTS: usize = 64;
static SPLIT_ASKED: AtomicU64 = AtomicU64::new(0);
static SPLIT_DONE: AtomicU64 = AtomicU64::new(0);

/// A coarse clock (ms, wrapping), read at superpage grain only.
#[inline]
pub(crate) fn tick() -> u32 {
    (pg_clock::mono_ns() / 1_000_000) as u32
}

#[inline]
pub(crate) fn touch() {
    LAST_CLAIM.store(tick(), Relaxed);
}

/// Ask the purger for memory back (the broker, the ladder, the cap).
pub fn set_pressure(p: Pressure) {
    let (bytes, class) = match p {
        Pressure::None => return,
        Pressure::Release { bytes, class } => (bytes, class),
        Pressure::Floor => (usize::MAX, None),
    };
    if !crate::on() || bytes == 0 {
        return;
    }
    ASK_CLASS.store(class.map_or(NO_CLASS, |c| c as u8), Relaxed);
    ASK.fetch_max(bytes, Release);
    let t = THREAD.load(Acquire);
    if t.is_null() {
        #[cfg(not(target_os = "linux"))]
        {
            let b = ASK.swap(0, Acquire);
            purge_now(b, class);
        }
        return;
    }
    // SAFETY: published once by `start`, never freed.
    unsafe { (*t).unpark() };
}

/// Release up to `bytes` now, on the calling thread (tests, the clock harness). The bytes released.
pub fn purge_now(bytes: usize, class: Option<Class>) -> usize {
    if !crate::on() || bytes == 0 {
        return 0;
    }
    superpage::spin(&PURGING);
    let t0 = pg_clock::mono_ns();
    let got = purge(bytes, class);
    stats::PURGE_NS.fetch_add(pg_clock::mono_ns().saturating_sub(t0), Relaxed);
    superpage::unspin(&PURGING);
    got
}

/// pagepool P3 (R1's warm return): the bytes a pressure release can give back now: whole free superpages
/// (warm lazy spans' pages among them), the dead runs of live blocks, and last the claimer's prefaulted zero superpages
/// (free memory whose only value is a saved fault), then the resident free runs of superpages not wholly free (a
/// MADV_DONTNEED inside the superpage); never parked blocks. 0 with the pool off.
pub fn releasable() -> usize {
    if !crate::on() {
        return 0;
    }
    let pos = |v: isize| v.max(0) as usize;
    // D4760 (`pool_thp_partial` 1): free runs and dead runs inside superpages in use stay held; none of the free runs
    // and (to stay under, never over, what R1 gets) none of the dead runs count, though whole dead superpages still go
    let partial = if crate::thp_partial() == crate::THP_KEEP {
        0
    } else {
        pos(stats::DEAD.load(Relaxed)).saturating_add(pos(stats::FREE_RUNS.load(Relaxed)))
    };
    (superpage::count(State::WarmDirty) * SUPER)
        .saturating_add(pos(stats::LAZY_WARM.load(Relaxed)))
        .saturating_add(superpage::count(State::WarmZero) * SUPER)
        .saturating_add(partial)
        .saturating_sub(crate::warmres::keep())
}

/// R1's ask not yet served by the purger (`return_warm`), in one word: the bytes asked (the low `ASK_BITS`) under a count
/// of the asks, so the pass that serves it (`serve_ask`, in `return_warm_now`'s order) clears it only when no ask came
/// meanwhile; and when it was asked (`tick`).
static LANDING: AtomicU64 = AtomicU64::new(0);
const ASK_BITS: u32 = 48;
const ASK_BYTES: u64 = (1 << ASK_BITS) - 1;
static LANDING_AT: AtomicU32 = AtomicU32::new(0);
/// `hooks::set_landing`: a landing the claimer holds for, with no ask for the purger to serve.
static LANDING_HELD: AtomicUsize = AtomicUsize::new(0);

/// D4760 (pppress): `PGRUST_POOL_FAST_RELEASE=1`, read once (0: unread, 1: off, 2: on).
static FAST_SWITCH: AtomicU32 = AtomicU32::new(0);

/// D4760 (pppress): `PGRUST_POOL_FAST_RELEASE=1`, read ONCE (`pagepool::init`): an R1 serve costs what it gives back.
/// The dead step skips an empty scan and walks the dead table a word at a time, only to the lazy zone's used part; the
/// free-run step takes the superpages with the most free runs first (one folio split frees the most bytes); and an R1
/// ask that comes during the purger's other passes is served between them, not after them.
pub(crate) fn fast_on() -> bool {
    match FAST_SWITCH.load(Relaxed) {
        0 => {
            let v = if os::env_is_one(c"PGRUST_POOL_FAST_RELEASE") { 2 } else { 1 };
            FAST_SWITCH.store(v, Relaxed);
            v == 2
        }
        v => v == 2,
    }
}

pub(crate) fn force_fast(on: bool) {
    FAST_SWITCH.store(if on { 2 } else { 1 }, Relaxed);
}

/// D4760 (pppress): R1's clock, counted with every switch off. `serve_ask`'s R1 serves: ask -> land, the oldest unserved
/// ask's time to the end of the pass that served it, in `R1_BIN_MS` bins (ms, upper bounds; the last bin open); and
/// every `return_warm_now` (the purger's and the ladder thread's own), by step: calls, bytes released, ns.
pub const R1_STEPS: usize = 5;
pub const R1_STEP_ROWS: [&str; R1_STEPS] = ["r1_warm_dirty", "r1_dead", "r1_lazy_warm", "r1_warm_zero", "r1_free_runs"];
pub const R1_BINS: usize = 16;
pub const R1_BIN_MS: [u64; R1_BINS - 1] = [1, 2, 3, 4, 5, 6, 8, 10, 12, 15, 20, 30, 50, 100, 200];
pub const R1_BIN_ROWS: [&str; R1_BINS] = [
    "r1_land_lt1", "r1_land_lt2", "r1_land_lt3", "r1_land_lt4", "r1_land_lt5", "r1_land_lt6", "r1_land_lt8", "r1_land_lt10",
    "r1_land_lt12", "r1_land_lt15", "r1_land_lt20", "r1_land_lt30", "r1_land_lt50", "r1_land_lt100", "r1_land_lt200", "r1_land_ge200",
];
static ASK_NS: AtomicU64 = AtomicU64::new(0);
static LANDS: AtomicU64 = AtomicU64::new(0);
static LAND_NS: AtomicU64 = AtomicU64::new(0);
static LAND_MAX_NS: AtomicU64 = AtomicU64::new(0);
static LAND_BYTES: AtomicU64 = AtomicU64::new(0);
static LAND_BINS: [AtomicU64; R1_BINS] = [const { AtomicU64::new(0) }; R1_BINS];
static CALLS: AtomicU64 = AtomicU64::new(0);
static CALL_NS: AtomicU64 = AtomicU64::new(0);
static CALL_MAX_NS: AtomicU64 = AtomicU64::new(0);
static STEP_CALLS: [AtomicU64; R1_STEPS] = [const { AtomicU64::new(0) }; R1_STEPS];
static STEP_BYTES: [AtomicU64; R1_STEPS] = [const { AtomicU64::new(0) }; R1_STEPS];
static STEP_NS: [AtomicU64; R1_STEPS] = [const { AtomicU64::new(0) }; R1_STEPS];
/// Asks served between the purger's other passes (`PGRUST_POOL_FAST_RELEASE`).
static CUT_INS: AtomicU64 = AtomicU64::new(0);
/// D4878: the statement-end hand-backs under ladder pressure (`note_end_give`): released now on the ending thread (and
/// the bytes), asked of the purger.
static END_NOW: AtomicU64 = AtomicU64::new(0);
static END_NOW_BYTES: AtomicU64 = AtomicU64::new(0);
static END_ASK: AtomicU64 = AtomicU64::new(0);

/// D4760 (pppress): R1's clock (`r1_counts`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct R1Counts {
    pub fast_on: bool,
    pub lands: u64,
    pub land_ns: u64,
    pub land_max_ns: u64,
    pub land_bytes: u64,
    pub land_bins: [u64; R1_BINS],
    pub calls: u64,
    pub call_ns: u64,
    pub call_max_ns: u64,
    pub step_calls: [u64; R1_STEPS],
    pub step_bytes: [u64; R1_STEPS],
    pub step_ns: [u64; R1_STEPS],
    pub cut_ins: u64,
    /// D4878: statement-end hand-backs under ladder pressure, released now (and the bytes) / asked of the purger.
    pub end_now: u64,
    pub end_now_bytes: u64,
    pub end_ask: u64,
}

fn loads<const N: usize>(v: &[AtomicU64; N]) -> [u64; N] {
    core::array::from_fn(|i| v[i].load(Relaxed))
}

pub fn r1_counts() -> R1Counts {
    R1Counts {
        fast_on: fast_on(),
        lands: LANDS.load(Relaxed),
        land_ns: LAND_NS.load(Relaxed),
        land_max_ns: LAND_MAX_NS.load(Relaxed),
        land_bytes: LAND_BYTES.load(Relaxed),
        land_bins: loads(&LAND_BINS),
        calls: CALLS.load(Relaxed),
        call_ns: CALL_NS.load(Relaxed),
        call_max_ns: CALL_MAX_NS.load(Relaxed),
        step_calls: loads(&STEP_CALLS),
        step_bytes: loads(&STEP_BYTES),
        step_ns: loads(&STEP_NS),
        cut_ins: CUT_INS.load(Relaxed),
        end_now: END_NOW.load(Relaxed),
        end_now_bytes: END_NOW_BYTES.load(Relaxed),
        end_ask: END_ASK.load(Relaxed),
    }
}

/// D4878: a statement end's hand-back under ladder pressure gave the pool's free memory back: `now` on its thread
/// (`bytes` released), else asked of the purger. Counted only (`r1_counts`).
pub fn note_end_give(now: bool, bytes: usize) {
    if now {
        END_NOW.fetch_add(1, Relaxed);
        END_NOW_BYTES.fetch_add(bytes as u64, Relaxed);
    } else {
        END_ASK.fetch_add(1, Relaxed);
    }
}

fn note_land(asked_ns: u64, bytes: usize) {
    let ns = pg_clock::mono_ns().saturating_sub(asked_ns);
    let ms = ns / 1_000_000;
    let bin = R1_BIN_MS.iter().position(|&b| ms < b).unwrap_or(R1_BINS - 1);
    LANDS.fetch_add(1, Relaxed);
    LAND_NS.fetch_add(ns, Relaxed);
    LAND_MAX_NS.fetch_max(ns, Relaxed);
    LAND_BYTES.fetch_add(bytes as u64, Relaxed);
    LAND_BINS[bin].fetch_add(1, Relaxed);
}

#[inline]
fn step(k: usize, f: impl FnOnce() -> usize) -> usize {
    let t0 = pg_clock::mono_ns();
    let got = f();
    STEP_CALLS[k].fetch_add(1, Relaxed);
    STEP_BYTES[k].fetch_add(got as u64, Relaxed);
    STEP_NS[k].fetch_add(pg_clock::mono_ns().saturating_sub(t0), Relaxed);
    got
}

/// pppress card 6: `PGRUST_POOL_GROW_GUARD=1`, read once (0: unread, 1: off, 2: on).
static GUARD_SWITCH: AtomicU32 = AtomicU32::new(0);

/// pppress card 6: `PGRUST_POOL_GROW_GUARD=1`, read ONCE (`pagepool::init`): RELEASE BEFORE GROW. While the ladder
/// presses the pool (its last sample left less room under its line than a grow adds, `claimer::room`), a thread about to
/// add pool charge (a span or block take past the shelves, a lazy span's commit or grow) first gives back the pool's own
/// free memory inline (`grow_guard`), and a zeroed take of whole superpages takes a free warm one before a Cold one. The
/// claimer already prefaults nothing while pressed (its room).
pub(crate) fn guard_on() -> bool {
    match GUARD_SWITCH.load(Relaxed) {
        0 => {
            let v = if os::env_is_one(c"PGRUST_POOL_GROW_GUARD") { 2 } else { 1 };
            GUARD_SWITCH.store(v, Relaxed);
            v == 2
        }
        v => v == 2,
    }
}

pub(crate) fn force_guard(on: bool) {
    GUARD_SWITCH.store(if on { 2 } else { 1 }, Relaxed);
}

/// pppress card 8: the ladder's last sample left the pool no room (`set_claim_room(0)`: pressed, or a release landing).
#[inline]
pub(crate) fn ladder_pressed() -> bool {
    crate::claimer::room() == 0
}

/// pppress card 6: the ladder presses the pool for a grow of `bytes` (`PGRUST_POOL_GROW_GUARD` on; with no gate, never).
#[inline]
pub(crate) fn pressed(bytes: usize) -> bool {
    guard_on() && crate::claimer::room() < bytes.max(1)
}

/// pppress card 6: the inline releases (`grow_guard`): count, bytes, ns, the longest ns, by `GUARD_BIN_US` (µs, upper
/// bounds; the last open), and the asks that found another releaser running (none released).
pub const GUARD_BINS: usize = 8;
pub const GUARD_BIN_US: [u64; GUARD_BINS - 1] = [10, 50, 100, 500, 1_000, 5_000, 10_000];
pub const GUARD_BIN_ROWS: [&str; GUARD_BINS] =
    ["grow_guard_us_lt10", "grow_guard_us_lt50", "grow_guard_us_lt100", "grow_guard_us_lt500", "grow_guard_us_lt1000", "grow_guard_us_lt5000", "grow_guard_us_lt10000", "grow_guard_us_ge10000"];
static GUARDS: AtomicU64 = AtomicU64::new(0);
static GUARD_BYTES: AtomicU64 = AtomicU64::new(0);
static GUARD_NS: AtomicU64 = AtomicU64::new(0);
static GUARD_MAX_NS: AtomicU64 = AtomicU64::new(0);
static GUARD_SKIPS: AtomicU64 = AtomicU64::new(0);
static GUARD_HIST: [AtomicU64; GUARD_BINS] = [const { AtomicU64::new(0) }; GUARD_BINS];
static GUARD_WARM: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GuardCounts {
    pub on: bool,
    pub releases: u64,
    pub bytes: u64,
    pub ns: u64,
    pub max_ns: u64,
    pub skips: u64,
    pub bins: [u64; GUARD_BINS],
    /// Zeroed whole-superpage takes served warm before Cold while pressed.
    pub warm_first: u64,
}

pub fn guard_counts() -> GuardCounts {
    GuardCounts {
        on: guard_on(),
        releases: GUARDS.load(Relaxed),
        bytes: GUARD_BYTES.load(Relaxed),
        ns: GUARD_NS.load(Relaxed),
        max_ns: GUARD_MAX_NS.load(Relaxed),
        skips: GUARD_SKIPS.load(Relaxed),
        bins: loads(&GUARD_HIST),
        warm_first: GUARD_WARM.load(Relaxed),
    }
}

pub(crate) fn note_warm_first() {
    GUARD_WARM.fetch_add(1, Relaxed);
}

thread_local! {
    /// pppress card 6: the pool charge this thread claimed while the ladder pressed, not yet given back (`settle`).
    static OWED: Cell<usize> = const { Cell::new(0) };
    /// pppress card 6: the superpage this thread's last Cold claim took (its free runs were never touched).
    static FRESH: Cell<u32> = const { Cell::new(superpage::NONE) };
}

/// pppress card 6: `bytes` of pool charge claimed on this thread (`stats::claim`); while pressed, owed back.
#[inline]
pub(crate) fn owe(bytes: usize) {
    if pressed(bytes) {
        OWED.with(|o| o.set(o.get().saturating_add(bytes)));
    }
}

/// pppress card 6: superpage `i`, Cold, claimed on this thread while pressed (owed with it).
#[inline]
pub(crate) fn fresh(i: u32) {
    if pressed(SUPER) {
        FRESH.with(|f| f.set(i));
    }
}

/// pppress card 6: RELEASE BEFORE GROW at a take's or grow's settle point (no pool lock held, before the caller touches
/// what it took): what this thread claimed while the ladder pressed goes back from the pool's own free memory, inline:
/// dead runs, free runs inside superpages in use (emptiest first, split first in `pool_thp_partial` 2; never the fresh
/// superpage's untouched runs), warm lazy spans, then whole free superpages past the warm reserve's spare; up to the owed
/// bytes, through `os::release` (give_back) only. One releaser at a time (the purger's lock, tried once): another
/// running, none here (counted).
#[inline]
pub(crate) fn settle() {
    if guard_on() {
        let owed = OWED.with(|o| o.replace(0));
        // read and cleared at every settle: a fresh superpage is skipped only by the release its own claim owes
        let fresh = FRESH.with(|f| f.replace(superpage::NONE));
        if owed != 0 {
            guard_release(owed, fresh);
        }
    }
}

#[cold]
#[inline(never)]
fn guard_release(want: usize, fresh: u32) -> usize {
    if PURGING.compare_exchange(0, 1, Acquire, Relaxed).is_err() {
        GUARD_SKIPS.fetch_add(1, Relaxed);
        return 0;
    }
    let t0 = pg_clock::mono_ns();
    let mut got = api::release_dead(want, true);
    if got < want {
        got += state::release_free_runs_but(want - got, true, fresh);
    }
    if got < want {
        got += span::release_warm(want - got, None);
    }
    if got < want {
        got += release_free(State::WarmDirty, (want - got).min(crate::warmres::spare_cap()), 0);
    }
    if got < want {
        got += release_free(State::WarmZero, (want - got).min(crate::warmres::spare_cap()), 0);
    }
    let ns = pg_clock::mono_ns().saturating_sub(t0);
    stats::PURGE_NS.fetch_add(ns, Relaxed);
    superpage::unspin(&PURGING);
    GUARDS.fetch_add(1, Relaxed);
    GUARD_BYTES.fetch_add(got as u64, Relaxed);
    GUARD_NS.fetch_add(ns, Relaxed);
    GUARD_MAX_NS.fetch_max(ns, Relaxed);
    let us = ns / 1_000;
    GUARD_HIST[GUARD_BIN_US.iter().position(|&b| us < b).unwrap_or(GUARD_BINS - 1)].fetch_add(1, Relaxed);
    got
}

/// D4872: `PGRUST_POOL_HOST_PRESSURE` (default ON; `=0` turns it off), read once (0: unread, 1: off, 2: on): the host's
/// memory pressure (`set_host_pressed`: the memory PSI's stall share or the file refault rate past its knob,
/// `mempolicy::hostpress`, read at the memory watchdog's samples) releases the pool's free memory and stops the claimer's
/// prefault, whatever the ladder's line says. Under the line, the pool otherwise keeps what it freed while the kernel
/// evicts the page cache around it (D4872: a wide-table bulk load on a 32 GiB x86 box, 19 % slower than under
/// mimalloc).
static HOST_SWITCH: AtomicU32 = AtomicU32::new(0);
static HOST: AtomicBool = AtomicBool::new(false);
/// D4872: the presses (a sample pressed after one that was not), the release asks made while pressed, and their bytes.
static HOST_PRESSES: AtomicU64 = AtomicU64::new(0);
static HOST_ASKS: AtomicU64 = AtomicU64::new(0);
static HOST_BYTES: AtomicU64 = AtomicU64::new(0);
/// D4872: the purger's pending ask is the host's (its bytes go to `HOST_BYTES`).
static HOST_ASKED: AtomicBool = AtomicBool::new(false);
/// D4872 (B): a host ask releases whole free memory only (`host_release`), never the free runs of superpages in use.
static HOST_WHOLE_ASK: AtomicBool = AtomicBool::new(false);

pub(crate) fn host_on() -> bool {
    match HOST_SWITCH.load(Relaxed) {
        0 => {
            let v = if os::env_on_by_default(c"PGRUST_POOL_HOST_PRESSURE") { 2 } else { 1 };
            HOST_SWITCH.store(v, Relaxed);
            v == 2
        }
        v => v == 2,
    }
}

pub(crate) fn force_host(on: bool) {
    HOST_SWITCH.store(if on { 2 } else { 1 }, Relaxed);
}

/// D4872: the host's memory pressure at this sample (memruntime's watchdog, every sample). Pressed: the pool's whole free
/// memory (`whole_releasable`: free superpages, warm lazy spans) is asked of the purger at each sample while pressed (what
/// has aged `pool_host_age_ms` since goes too: `host_release`), and the claimer prefaults nothing (`host_pressed`). With
/// the switch off or the pool off, nothing.
pub fn set_host_pressed(pressed: bool) {
    if !crate::on() || !host_on() {
        return;
    }
    let was = HOST.swap(pressed, Relaxed);
    if !pressed {
        return;
    }
    if !was {
        HOST_PRESSES.fetch_add(1, Relaxed);
    }
    if whole_releasable() != 0 {
        HOST_ASKS.fetch_add(1, Relaxed);
        HOST_ASKED.store(true, Relaxed);
        HOST_WHOLE_ASK.store(true, Release);
        let t = THREAD.load(Acquire);
        if !t.is_null() {
            // SAFETY: published once by `start`, never freed.
            unsafe { (*t).unpark() };
        }
    }
}

/// D4872 (B): the whole free memory a host ask gives back: free superpages (dirty and zero, past the warm reserve's keep)
/// and warm lazy spans; never a run of a superpage in use (round 2: releasing those under a busy load only churned).
fn whole_releasable() -> usize {
    let pos = |v: isize| v.max(0) as usize;
    ((superpage::count(State::WarmDirty) + superpage::count(State::WarmZero)) * SUPER)
        .saturating_add(pos(stats::LAZY_WARM.load(Relaxed)))
        .saturating_sub(crate::warmres::keep())
}

/// D4872 (B): the host's ask, on the purger's thread: whole free superpages, warm lazy spans, then the claimer's zero
/// superpages. The bytes released.
/// D4872 churn guard (`pool_host_age_ms`): only what has been free that long or more (a superpage by its free stamp, a
/// warm span by its give's), oldest first. A host whose page cache refaults for its own reasons (RO OLTP on a dataset
/// past RAM: the refaults never stop, so the press never lets go) otherwise had every superpage a statement freed
/// released at the next sample and faulted again by the next statement: 1,977 GB released in 300 s, THP faults x8,
/// refaults -0.7 %. What stays free that long is memory the pool is not using; what a statement frees and the next
/// takes is not the page cache's to have back.
fn host_release() -> usize {
    let cfg = zones().cfg;
    let age = cfg.host_age_ms;
    superpage::spin(&PURGING);
    let mut got = release_free(State::WarmDirty, crate::warmres::spare_cap(), age);
    got += if age == 0 { span::release_warm(usize::MAX, None) } else { span::age_warm(age, usize::MAX, usize::MAX, cfg.purge_batch).0 };
    got += release_free(State::WarmZero, crate::warmres::spare_cap(), age);
    superpage::unspin(&PURGING);
    got
}

/// D4872: the host is pressed (the claimer prefaults nothing).
#[inline]
pub(crate) fn host_pressed() -> bool {
    HOST.load(Relaxed)
}

/// D4872: the switch, whether pressed now, the presses, the asks made while pressed and the bytes they released.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HostCounts {
    pub on: bool,
    pub pressed: bool,
    pub presses: u64,
    pub asks: u64,
    pub bytes: u64,
}

pub fn host_counts() -> HostCounts {
    HostCounts {
        on: host_on(),
        pressed: host_pressed(),
        presses: HOST_PRESSES.load(Relaxed),
        asks: HOST_ASKS.load(Relaxed),
        bytes: HOST_BYTES.load(Relaxed),
    }
}

/// An R1 ask waits for the purger (`PGRUST_POOL_FAST_RELEASE`: the passes that may run long stop for it).
#[inline]
pub(crate) fn r1_waiting() -> bool {
    fast_on() && LANDING.load(Relaxed) & ASK_BYTES != 0
}

/// pagepool P3: R1's pool step OFF the memory runtime's loop thread (the watchdog's backstop past its
/// pressure line, the ladder's R1): every releasable byte asked of the purger; this thread never remaps. The bytes asked
/// are LANDING (`landing`) until the purger's pass that serves the ask has run: help on its way, never freed and never
/// room. Never from a periodic tick or an idle path. The bytes landing now.
pub fn return_warm() -> usize {
    let can = releasable();
    if can != 0 {
        let _ = ASK_NS.compare_exchange(0, pg_clock::mono_ns().max(1), Relaxed, Relaxed);
        LANDING_AT.store(tick().max(1), Relaxed);
        let can = (can as u64).min(ASK_BYTES);
        let bump = |w: u64| ((w >> ASK_BITS).wrapping_add(1) << ASK_BITS) | (w & ASK_BYTES).max(can);
        let was = LANDING.fetch_update(AcqRel, Acquire, |w| Some(bump(w))).unwrap_or_else(|w| w);
        let t = THREAD.load(Acquire);
        if !t.is_null() {
            // SAFETY: published once by `start`, never freed.
            unsafe { (*t).unpark() };
        } else if cfg!(not(target_os = "linux")) {
            // macOS / others (no pool threads): the plain release inline, as set_pressure did; served, so no longer
            // landing unless another ask came meanwhile. The bytes asked are still R1's answer.
            let _ = purge_now(can as usize, None);
            let _ = LANDING.compare_exchange(bump(was), 0, AcqRel, Relaxed);
            return (bump(was) & ASK_BYTES) as usize;
        }
    }
    (LANDING.load(Acquire) & ASK_BYTES) as usize
}

/// pagepool P3: R1's pool step on the calling thread, measured: up to `want` bytes released now, in R1's
/// order: free superpages, dead runs, warm lazy spans, the claimer's zero superpages, then free runs inside superpages
/// still in use. The bytes released.
pub fn return_warm_now(want: usize) -> usize {
    // `PGRUST_POOL_FAST_RELEASE`: what is releasable is read after the central lists went home (their free runs count)
    let fast = fast_on();
    let want = if fast { want } else { want.min(releasable()) };
    if !crate::on() || want == 0 {
        return 0;
    }
    superpage::spin(&PURGING);
    let t0 = pg_clock::mono_ns();
    state::set_low(true);
    home_lists(None);
    let want = if fast { want.min(releasable()) } else { want };
    let mut got = step(0, || release_free(State::WarmDirty, want.min(crate::warmres::spare_cap()), 0));
    if got < want {
        got += step(1, || api::release_dead(want - got, fast));
    }
    if got < want {
        got += step(2, || span::release_warm(want - got, None));
    }
    if got < want {
        got += step(3, || release_free(State::WarmZero, (want - got).min(crate::warmres::spare_cap()), 0));
    }
    if got < want {
        got += step(4, || state::release_free_runs(want - got, fast));
    }
    state::set_low(false);
    let ns = pg_clock::mono_ns().saturating_sub(t0);
    stats::PURGE_NS.fetch_add(ns, Relaxed);
    CALLS.fetch_add(1, Relaxed);
    CALL_NS.fetch_add(ns, Relaxed);
    CALL_MAX_NS.fetch_max(ns, Relaxed);
    superpage::unspin(&PURGING);
    got
}

/// The bytes an R1 ask (`return_warm`) is still landing: asked of the purger and not yet served, for at most `within_ms`
/// after the ask (then 0: what is left is present to the reader again).
pub fn landing(within_ms: u32) -> usize {
    let b = (LANDING.load(Acquire) & ASK_BYTES) as usize;
    if b == 0 || tick().wrapping_sub(LANDING_AT.load(Relaxed)) >= within_ms {
        return 0;
    }
    b
}

/// An R1 ask not yet served by the purger (no expiry: the claimer holds its prefault until the pass ran).
pub(crate) fn landing_any() -> bool {
    LANDING.load(Acquire) & ASK_BYTES != 0 || LANDING_HELD.load(Acquire) != 0
}

pub(crate) fn set_landing(bytes: usize) {
    LANDING_HELD.store(bytes, Release);
    LANDING_AT.store(tick().max(1), Relaxed);
}

/// Home `class`'s central lists (every class's with None) and free the block runs left whole; this thread's shelf of
/// them goes first. True when anything moved. No OS release.
pub(crate) fn home_lists(class: Option<Class>) -> bool {
    crate::shelf::flush_where(class);
    let mut moved = false;
    for k in central::keys().filter(|k| class.is_none_or(|c| k.class == c)) {
        if central::bytes(k) > 0 {
            central::take_all(k, |b| central::home(k, b));
            moved = true;
        }
        if !k.is_runs() {
            moved |= run::sweep(k) > 0;
        }
    }
    moved
}

/// Reuse before release on a failed claim (B1): `f` again after homing `class`'s lists, then every class's; then None
/// and the purger's return pass is asked for. The calling thread never releases to the OS here.
pub(crate) fn reclaim<T>(class: Class, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    for c in [Some(class), None] {
        if home_lists(c) {
            if let Some(v) = f() {
                return Some(v);
            }
        }
    }
    ask_return();
    None
}

/// Shelves come home (low_warm) until the purger's return pass (homing only, no release) finds a warm superpage.
fn ask_return() {
    let t = THREAD.load(Acquire);
    if t.is_null() || RETURN_ASK.swap(true, AcqRel) {
        return;
    }
    state::set_low(true);
    // SAFETY: published once by `start`, never freed.
    unsafe { (*t).unpark() };
}

#[cfg_attr(any(target_family = "wasm", miri), allow(dead_code))]
pub(crate) fn return_pending() -> bool {
    RETURN_ASK.load(SeqCst) || RETURNING.load(SeqCst)
}

fn return_pass() {
    home_lists(None);
    if superpage::count(State::WarmDirty) + superpage::count(State::WarmZero) > 0 {
        state::set_low(false);
    }
}

fn purge(want: usize, class: Option<Class>) -> usize {
    state::set_low(true);
    home_lists(class);
    let mut got = release_free(State::WarmDirty, want.min(crate::warmres::spare_cap()), 0);
    if got < want {
        got += release_free(State::WarmZero, (want - got).min(crate::warmres::spare_cap()), 0);
    }
    if got < want {
        got += api::release_dead(want - got, fast_on());
    }
    if got < want {
        got += span::release_warm(want - got, class);
    }
    if got < want {
        got += state::release_free_runs(want - got, fast_on());
    }
    state::set_low(false);
    got
}

fn take_in(i: u32, st: State) -> bool {
    let z = zones();
    let b = match st {
        State::WarmZero => 1,
        _ => 2,
    };
    let bit = 1u64 << (i % 64);
    // SAFETY: the bitmap words in the meta zone.
    let w = unsafe { &*(z.bits[b] as *const core::sync::atomic::AtomicU64).add(i as usize / 64) };
    if w.load(Relaxed) & bit == 0 {
        return false;
    }
    match superpage::take_bit(i) {
        Some(s) if s == st => true,
        Some(other) => {
            superpage::put(i, other);
            false
        }
        None => false,
    }
}

/// Release whole free superpages in `st`, oldest first (by a 32-bin histogram of their age), at least `min_age_ms` old,
/// until `want` bytes are met.
fn release_free(st: State, want: usize, min_age_ms: u32) -> usize {
    if superpage::count(st) == 0 || want == 0 {
        return 0;
    }
    let z = zones();
    let now = tick();
    let age_bin = |i: u32| -> Option<u32> {
        let age = now.wrapping_sub(superpage::meta(i).dirty_at.load(Relaxed));
        (age >= min_age_ms).then(|| 32 - (age | 1).leading_zeros())
    };
    let mut hist = [0usize; 33];
    for i in 0..z.nsuper {
        if superpage::meta(i).state() == st {
            if let Some(b) = age_bin(i) {
                hist[b as usize] += SUPER;
            }
        }
    }
    let mut cut = 32usize;
    let mut acc = hist[32];
    while cut > 0 && acc < want {
        cut -= 1;
        acc += hist[cut];
    }
    let batch = (z.cfg.purge_batch / SUPER).clamp(1, superpage::per_chunk() as usize) as u32;
    let mut got = 0;
    for c in 0..superpage::chunks() {
        let base = c * superpage::stride_slots();
        let mut i = base;
        while i < base + superpage::per_chunk() && got < want {
            let ok = |i: u32| age_bin(i).is_some_and(|b| b as usize >= cut) && superpage::meta(i).state() == st;
            if !ok(i) || !take_in(i, st) {
                i += 1;
                continue;
            }
            let mut j = i + 1;
            while j < base + superpage::per_chunk() && j - i < batch && got + ((j - i) as usize) * SUPER < want && ok(j) && take_in(j, st) {
                j += 1;
            }
            got += release_run(i, j - i, st);
            i = j;
        }
        if got >= want {
            break;
        }
    }
    got
}

/// Give superpages `first..first + n` (held) back to the OS: one remap. The bytes released.
fn release_run(first: u32, n: u32, st: State) -> usize {
    let bytes = n as usize * SUPER;
    // SAFETY: whole free superpages we hold: nothing reads them; the pool zone stays mapped (a remap, not an unmap).
    let rc = unsafe { os::release(superpage::addr_of(first), bytes) };
    for i in first..first + n {
        superpage::put(i, if rc == 0 { State::Cold } else { st });
    }
    if rc != 0 {
        return 0;
    }
    stats::claim(-(bytes as isize));
    stats::RELEASES.fetch_add(1, Relaxed);
    stats::RELEASED_BYTES.fetch_add(bytes as u64, Relaxed);
    bytes
}

/// Age release by `pool_release_mode`: (a) superpages free past `decay_ms` while warm free is past `warm_cap`; (b) the
/// same only after 60 s with no claim; (c) none.
fn age_pass() {
    let cfg = zones().cfg;
    if cfg.release_mode >= 2 || cfg.decay_ms == 0 {
        return;
    }
    if cfg.release_mode == 1 && tick().wrapping_sub(LAST_CLAIM.load(Relaxed)) < 60_000 {
        return;
    }
    let warm = (superpage::count(State::WarmDirty) + superpage::count(State::WarmZero)) * SUPER;
    if warm <= cfg.warm_cap {
        return;
    }
    superpage::spin(&PURGING);
    let over = warm - cfg.warm_cap;
    // ppres: with PGRUST_POOL_RESERVE on, WarmDirty is the reserve too (recycled in place): the pass takes only the spare.
    let got = release_free(State::WarmDirty, over.min(crate::warmres::age_cap()), cfg.decay_ms);
    // pagepool P3 (F1): WarmZero up to the claimer's target stays. Under it the claimer prefaults again what this pass
    // gives back (it fills to its target), a release and a refault a decay apart for as long as the server idles (ppsmall
    // 19:11Z: ~13 MB/s idle). Past the target (it halves at idle) there is no refill, so that much goes.
    let spare = (superpage::count(State::WarmZero) * SUPER).saturating_sub(crate::claimer::target()).min(crate::warmres::age_cap());
    if got < over && spare > 0 {
        release_free(State::WarmZero, (over - got).min(spare), cfg.decay_ms);
    }
    superpage::unspin(&PURGING);
}

static LAST_DECAY: AtomicU32 = AtomicU32::new(0);

static SEEN_POPS: [AtomicU32; central::KEYS] = [const { AtomicU32::new(0) }; central::KEYS];

/// Age release on: a central list no take popped since the last pass goes home (whole block runs freed). Homing only.
fn idle_home_pass() {
    for k in central::keys() {
        let p = central::pops(k);
        if SEEN_POPS[k.index()].swap(p, Relaxed) == p && central::bytes(k) > 0 {
            central::take_all(k, |b| central::home(k, b));
            if !k.is_runs() {
                run::sweep(k);
            }
        }
    }
}

/// pagepool P3 (F1): `tick` of the last lazy pass and of the last age mark. The purger wakes on every claimer miss (about
/// once a millisecond under a busy query), so the pass keeps its own period (`PRESENT_MS`).
static LAZY_AT: AtomicU32 = AtomicU32::new(0);
static MARK_AT: AtomicU32 = AtomicU32::new(0);
/// pagepool P3 (F2): `tick` of the last cap pass, and its period (ms).
static CAP_AT: AtomicU32 = AtomicU32::new(0);
const CAP_MS: u32 = 100;
/// pagepool P3 (F1 / F2): where the running lazy pass's time ends (mono ns; `u64::MAX` outside one, and for a forced
/// one): its gives-back stop there, the rest waits a pass, as at the budget. Bytes alone do not bound a pass: one of many
/// small or contended remaps (a TLB flush each, an mmap_lock wait each) took 0.6 s within its 256 MiB (lane box, a 10
/// GB join suite at W8, swap on, every range remapped).
static PASS_END: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(u64::MAX);
const PASS_NS: u64 = 4_000_000;

/// The running lazy pass is past its time (`PASS_END`): no further give-back this pass.
pub(crate) fn pass_late() -> bool {
    let end = PASS_END.load(Relaxed);
    end != u64::MAX && pg_clock::mono_ns() >= end
}

/// pagepool P3 (F1 / F2): the purger's pass over freed pool memory that only pressure releases otherwise
/// (`releasable`): warm lazy spans (a dropped region's), dead runs (the pages a live region gave up) and free runs of
/// superpages still in use. With `pool_lazy_decay_ms`, whatever `pool_release_mode` says (the road without the pool gives
/// a region's freed pages back at once), those untouched that long go back: a warm span by its give's stamp, dead runs
/// and free runs by two age marks a decay apart (marks every decay; RIPE dead runs left by the budget go at the next
/// passes). With `pool_lazy_cap`, the warm span and dead run bytes past it go back, oldest warm span first. Every
/// `PRESENT_MS` (the purger's loop, rate-limited: it also wakes on claimer misses); at most `pool_age_pass_max` bytes remapped a pass, `pool_purge_batch` a remap, so a query
/// thread's mmap_lock wait stays short. Through `os::release` (give_back), on the purger thread only (`force`: a test's
/// seam on its own thread, every mark due). The bytes released, as counted (committed bytes, not only resident ones).
fn lazy_pass(now: u32, force: bool, spent: usize) -> usize {
    let cfg = zones().cfg;
    if cfg.lazy_decay_ms == 0 && cfg.lazy_cap == 0 {
        return 0;
    }
    // ppround card 14: a pass every PEAK_MS while the claim climbs (`PRESENT_MS` otherwise)
    let period = if CLIMBING.load(Relaxed) { PEAK_MS } else { crate::present::PRESENT_MS };
    if !force && now.wrapping_sub(LAZY_AT.load(Relaxed)) < period {
        return 0;
    }
    LAZY_AT.store(now, Relaxed);
    let t0 = pg_clock::mono_ns();
    // ppround card 14 (`PGRUST_POOL_PEAK_RELEASE`): the claim's climb over the last `CLIMB_MS`
    let climb = climbing(now, stats::claimed());
    CLIMBING.store(climb, Relaxed);
    PASS_END.store(if force { u64::MAX } else { t0 + if climb { PEAK_PASS_NS } else { PASS_NS } }, Relaxed);
    let batch = cfg.purge_batch;
    let mut budget = if cfg.age_pass_max == 0 { usize::MAX } else { cfg.age_pass_max.saturating_sub(spent) };
    let mut got = 0;
    let mut take = |(g, r): (usize, usize), budget: &mut usize| {
        got += g;
        *budget = budget.saturating_sub(r);
    };
    if cfg.lazy_decay_ms > 0 {
        let due = force || now.wrapping_sub(MARK_AT.load(Relaxed)) >= cfg.lazy_decay_ms;
        // ppround card 14: while the claim climbs, first in the pass, the free-run and dead-run marks come every pass
        // (`PEAK_MS` apart), not every decay: a run seen free at one pass and still free, never taken, at the next goes
        // back. Cold ones only, as at a mark (`pool_thp_partial` 3's hot superpages keep theirs: the activity bits still
        // clear only at the decay's marks)
        if climb && !due {
            // ppround card 14 v3 (`PGRUST_POOL_PEAK_RELEASE` levers, `peak_levers`): AGE, the walk every `WALK_MS` (or
            // straight on after a walk its budget cut), so a run goes back only when seen free at two walks a `WALK_MS`
            // apart; SKIP, no free-run walk under `SKIP_MIN` of resident free runs and no dead walk under `SKIP_MIN` dead;
            // DEAD1, the dead walk's mark makes a dead run ripe at once (one mark, not two)
            let lev = peak_levers();
            let walk = lev & LEVER_AGE == 0 || WALK_LATE.load(Relaxed) || now.wrapping_sub(WALK_AT.load(Relaxed)) >= WALK_MS;
            if walk {
                WALK_AT.store(now, Relaxed);
                let pos = |v: isize| v.max(0) as usize;
                let runs_ok = lev & LEVER_SKIP == 0 || pos(stats::FREE_RUNS.load(Relaxed)) >= SKIP_MIN;
                let dead_ok = lev & LEVER_SKIP == 0 || pos(stats::DEAD.load(Relaxed)) >= SKIP_MIN;
                let runs = if runs_ok { state::age_free_runs(budget) } else { (0, 0) };
                take(runs, &mut budget);
                let dead = if dead_ok {
                    api::dead_pass(if lev & LEVER_DEAD1 != 0 { api::DeadPass::Mark1 } else { api::DeadPass::Mark }, 0, 0, batch);
                    api::dead_pass(api::DeadPass::Ripe, usize::MAX, budget, batch)
                } else {
                    (0, 0)
                };
                take(dead, &mut budget);
                // WARM1: a freed lazy span ages out after one walk period, not the decay
                if lev & LEVER_WARM1 != 0 {
                    take(span::age_warm(WALK_MS.min(cfg.lazy_decay_ms), usize::MAX, budget, batch), &mut budget);
                }
                WALK_LATE.store(lev & LEVER_AGE != 0 && pass_late(), Relaxed);
                PEAK_SKIPS.fetch_add(!runs_ok as u64 + !dead_ok as u64, Relaxed);
                note_peak(runs, dead);
            } else {
                PEAK_WAITS.fetch_add(1, Relaxed);
            }
        }
        take(span::age_warm(cfg.lazy_decay_ms, usize::MAX, budget, batch), &mut budget);
        take(api::dead_pass(api::DeadPass::Ripe, usize::MAX, budget, batch), &mut budget);
        if due {
            MARK_AT.store(now, Relaxed);
            take(state::age_free_runs(budget), &mut budget);
            api::dead_pass(api::DeadPass::Mark, 0, 0, batch);
            take(api::dead_pass(api::DeadPass::Ripe, usize::MAX, budget, batch), &mut budget);
            // pppress card 8 (`pool_thp_partial` 3): a new decay for the activity bits, after the passes that read them
            state::clear_active();
        }
    }
    if cfg.lazy_cap > 0 && budget > 0 && (force || now.wrapping_sub(CAP_AT.load(Relaxed)) >= CAP_MS) {
        CAP_AT.store(now, Relaxed);
        let pos = |v: isize| v.max(0) as usize;
        let over = (pos(stats::LAZY_WARM.load(Relaxed)) + pos(stats::DEAD.load(Relaxed))).saturating_sub(cfg.lazy_cap);
        if over > 0 {
            let (g, r) = span::age_warm(0, over, budget, batch);
            take((g, r), &mut budget);
            if g < over && budget > 0 {
                take(api::dead_pass(api::DeadPass::Any, over - g, budget, batch), &mut budget);
            }
        }
    }
    if climb && pass_late() {
        PEAK_LATE.fetch_add(1, Relaxed);
    }
    PASS_END.store(u64::MAX, Relaxed);
    stats::PURGE_NS.fetch_add(pg_clock::mono_ns().saturating_sub(t0), Relaxed);
    got
}

/// ppround card 14: `PGRUST_POOL_PEAK_RELEASE=1`, read once (0: unread, 1: off, 2: on).
static PEAK_SWITCH: AtomicU32 = AtomicU32::new(0);

/// ppround card 14: `PGRUST_POOL_PEAK_RELEASE`, read ONCE (`pagepool::init`): RELEASE COLD FREE RUNS WHILE THE CLAIM
/// CLIMBS. Default off; an odd value up to 31 is on and its other bits are the levers (`peak_levers`): 1 = v2 alone,
/// 23 = v2 + SKIP + AGE + WARM1 (the candidate setting; DEAD1 measured no gain), 31 = every lever. The e15 peak's
/// resident retention past the bytes in use is mostly free runs of superpages still in use (a grouping query at W16:
/// 1,606 MB of them present at the memcg peak, dead runs only 216 MB present), and the age passes take two decay marks
/// plus a 4 ms pass at a time to give them back, so they are still resident when the charge tops out. While the pool's
/// claim has grown by `CLIMB_MIN` or more over the last `CLIMB_MS` (a climb: the slope of the claim, the pool's own
/// reading, not the ladder's line, which an e15 peak never nears), the lazy pass runs every `PEAK_MS` instead of
/// `PRESENT_MS`, and first in each pass marks and releases free runs and dead runs as a decay mark does (seen at one
/// pass, still free and untouched at the next; cold superpages and pieces only), with a `PEAK_PASS_NS` time budget
/// instead of `PASS_NS`. Through `os::age_release` (give_back) on the purger thread only, part of a superpage in use,
/// never a whole 2 MiB range.
pub(crate) fn peak_on() -> bool {
    match PEAK_SWITCH.load(Relaxed) {
        0 => {
            // v3: an odd value is on, its other bits the levers (1: v2 as it was; 31: every lever)
            let v = os::env_str(c"PGRUST_POOL_PEAK_RELEASE").and_then(|s| s.trim().parse::<u32>().ok()).unwrap_or(0);
            let on = v & 1 == 1 && v <= 31;
            PEAK_LEVERS.store(if on { v & !1 } else { 0 }, Relaxed);
            PEAK_SWITCH.store(if on { 2 } else { 1 }, Relaxed);
            on
        }
        v => v == 2,
    }
}

pub(crate) fn force_peak(on: bool) {
    PEAK_SWITCH.store(if on { 2 } else { 1 }, Relaxed);
}

/// ppround card 14 v3: `PGRUST_POOL_PEAK_RELEASE`'s levers past its on bit, read with it (0: v2 as it was). Each makes the
/// climbing passes cheaper, with no change when the switch is off:
/// - `LEVER_SKIP` (2): a climbing walk skips its free-run part under `SKIP_MIN` of resident free runs in superpages in
///   use (`stats::FREE_RUNS`) and its dead part under `SKIP_MIN` dead (`stats::DEAD`): no walk of every superpage or
///   every dead slot for nothing to give back.
/// - `LEVER_AGE` (4): the climbing walk runs once per `WALK_MS`, not every `PEAK_MS` pass (straight on at the next pass
///   when its time budget cut it): a run goes back only after it was seen free at two walks `WALK_MS` apart, so no run
///   freed less than `WALK_MS` ago (the release-then-refault churn of runs a query frees and takes again), and a fifth
///   of the walks.
/// - `LEVER_DEAD1` (8): the climbing walk's dead mark makes a dead run ripe at once (`DeadPass::Mark1`): the pages a
///   live region gave up go back at the first walk, as the road without the pool gives them back at once.
/// - `LEVER_WARM1` (16): the climbing walk gives back the warm lazy spans freed `WALK_MS` ago or more (`span::age_warm`),
///   not only those past `pool_lazy_decay_ms`: a freed span ages out after one walk period, as DEAD1 for dead pages.
pub(crate) fn peak_levers() -> u32 {
    let _ = peak_on();
    PEAK_LEVERS.load(Relaxed)
}

pub(crate) fn force_peak_levers(levers: u32) {
    PEAK_LEVERS.store(levers & (LEVER_SKIP | LEVER_AGE | LEVER_DEAD1 | LEVER_WARM1), Relaxed);
}

static PEAK_LEVERS: AtomicU32 = AtomicU32::new(0);
pub const LEVER_SKIP: u32 = 2;
pub const LEVER_AGE: u32 = 4;
pub const LEVER_DEAD1: u32 = 8;
pub const LEVER_WARM1: u32 = 16;
/// ppround card 14 v3: `LEVER_SKIP`'s floor of releasable bytes, and `LEVER_AGE`'s walk period (ms).
pub const SKIP_MIN: usize = 32 << 20;
pub const WALK_MS: u32 = 100;
/// ppround card 14 v3: the tick of the last climbing walk, whether its time budget cut it, and the walks the levers saved.
static WALK_AT: AtomicU32 = AtomicU32::new(0);
static WALK_LATE: AtomicBool = AtomicBool::new(false);
static PEAK_WAITS: AtomicU64 = AtomicU64::new(0);
static PEAK_SKIPS: AtomicU64 = AtomicU64::new(0);

/// ppround card 14: the window the climb is read over (ms), and the claim growth over it that is a climb (128 MiB/s and
/// up; a query building its working set grows the claim by GB/s).
pub const CLIMB_MS: u32 = 500;
pub const CLIMB_MIN: usize = 64 << 20;
/// ppround card 14: the lazy pass period while the claim climbs (`PRESENT_MS` otherwise): a stretch freed in a burst
/// (a grouping query: 1.85 GB of free runs inside 0.5 s, the charge's top 0.8 s later) is seen within PEAK_MS and gone
/// at the next pass.
const PEAK_MS: u32 = 20;
/// ppround card 14: a climbing lazy pass's time budget (`PASS_NS` otherwise). Each give-back is a MADV_DONTNEED under
/// the mmap_lock read side (no fault waits on it) and one TLB flush.
const PEAK_PASS_NS: u64 = 12_000_000;

/// The (tick, claim) of the last `CLIMB_SLOTS` lazy passes (the purger's thread, or a test's), a ring: at `PEAK_MS`,
/// more than `CLIMB_MS` of them.
const CLIMB_SLOTS: usize = 32;
static CLIMB_T: [AtomicU32; CLIMB_SLOTS] = [const { AtomicU32::new(0) }; CLIMB_SLOTS];
static CLIMB_C: [AtomicUsize; CLIMB_SLOTS] = [const { AtomicUsize::new(0) }; CLIMB_SLOTS];
static CLIMB_AT: AtomicUsize = AtomicUsize::new(0);
/// Whether the last lazy pass climbed (the pass period and the purger's park).
static CLIMBING: AtomicBool = AtomicBool::new(false);
/// A test's seam: the next passes climb whatever the claim does (0: read the claim; 1: climbing; 2: not).
static CLIMB_FORCE: AtomicU8 = AtomicU8::new(0);

/// ppround card 14: this pass's (tick, claim) into the ring, and whether the claim climbs: the switch on, and the claim
/// grown by `CLIMB_MIN` or more since the lowest claim of the passes in the last `CLIMB_MS`.
fn climbing(now: u32, claimed: usize) -> bool {
    let at = CLIMB_AT.fetch_add(1, Relaxed);
    let low = (0..at.min(CLIMB_SLOTS))
        .filter(|&i| now.wrapping_sub(CLIMB_T[i].load(Relaxed)) <= CLIMB_MS)
        .map(|i| CLIMB_C[i].load(Relaxed))
        .min();
    CLIMB_T[at % CLIMB_SLOTS].store(now, Relaxed);
    CLIMB_C[at % CLIMB_SLOTS].store(claimed, Relaxed);
    if !peak_on() {
        return false;
    }
    match CLIMB_FORCE.load(Relaxed) {
        1 => true,
        2 => false,
        _ => low.is_some_and(|low| claimed >= low.saturating_add(CLIMB_MIN)),
    }
}

/// ppround card 14: passes that climbed, the free-run and dead-run bytes they released (as counted) and gave back
/// (resident), and climbing passes that ran out of time.
static PEAK_PASSES: AtomicU64 = AtomicU64::new(0);
static PEAK_RUN_BYTES: AtomicU64 = AtomicU64::new(0);
static PEAK_RUN_RES: AtomicU64 = AtomicU64::new(0);
static PEAK_DEAD_BYTES: AtomicU64 = AtomicU64::new(0);
static PEAK_DEAD_RES: AtomicU64 = AtomicU64::new(0);
static PEAK_LATE: AtomicU64 = AtomicU64::new(0);

fn note_peak(runs: (usize, usize), dead: (usize, usize)) {
    PEAK_PASSES.fetch_add(1, Relaxed);
    PEAK_RUN_BYTES.fetch_add(runs.0 as u64, Relaxed);
    PEAK_RUN_RES.fetch_add(runs.1 as u64, Relaxed);
    PEAK_DEAD_BYTES.fetch_add(dead.0 as u64, Relaxed);
    PEAK_DEAD_RES.fetch_add(dead.1 as u64, Relaxed);
}

/// ppround card 14 (`PGRUST_POOL_PEAK_RELEASE`): what the climbing passes did, counted with the switch off too (all 0).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PeakCounts {
    pub on: bool,
    /// Lazy passes that climbed with no decay mark due (the passes the switch added).
    pub passes: u64,
    /// Free runs released by them: as counted (claim) and resident bytes given back (RSS).
    pub run_bytes: u64,
    pub run_resident: u64,
    /// Dead runs released by them: as counted (claim) and resident bytes given back (RSS).
    pub dead_bytes: u64,
    pub dead_resident: u64,
    /// Climbing passes that ran out of time (`PEAK_PASS_NS`): the rest waited a pass.
    pub late: u64,
    /// v3: the levers on; climbing passes that waited for `WALK_MS` (`LEVER_AGE`); walk parts skipped (`LEVER_SKIP`).
    pub levers: u32,
    pub waits: u64,
    pub skips: u64,
}

pub fn peak_counts() -> PeakCounts {
    PeakCounts {
        on: peak_on(),
        passes: PEAK_PASSES.load(Relaxed),
        run_bytes: PEAK_RUN_BYTES.load(Relaxed),
        run_resident: PEAK_RUN_RES.load(Relaxed),
        dead_bytes: PEAK_DEAD_BYTES.load(Relaxed),
        dead_resident: PEAK_DEAD_RES.load(Relaxed),
        late: PEAK_LATE.load(Relaxed),
        levers: peak_levers(),
        waits: PEAK_WAITS.load(Relaxed),
        skips: PEAK_SKIPS.load(Relaxed),
    }
}

/// Test seam (ppround card 14): the next lazy passes climb (`Some(true)`), never climb (`Some(false)`), or read the
/// claim (`None`).
pub(crate) fn force_climb(c: Option<bool>) {
    CLIMB_FORCE.store(
        match c {
            None => 0,
            Some(true) => 1,
            Some(false) => 2,
        },
        Relaxed,
    );
}

/// Test seam (ppround card 14): one lazy pass now as the purger runs it (unforced: only the marks due, and with
/// `PGRUST_POOL_PEAK_RELEASE` a climb's), its period not waited, on the calling thread. The bytes released, as counted.
#[doc(hidden)]
pub fn lazy_pass_timed_now() -> usize {
    if !crate::on() {
        return 0;
    }
    let now = tick();
    LAZY_AT.store(now.wrapping_sub(crate::present::PRESENT_MS), Relaxed);
    lazy_pass(now, false, 0)
}

/// Test seam: one lazy pass now (`pool_lazy_decay_ms` / `pool_lazy_cap` as configured, the period not waited), on the
/// calling thread. The bytes released, as counted.
#[doc(hidden)]
pub fn lazy_pass_now() -> usize {
    if !crate::on() {
        return 0;
    }
    lazy_pass(tick(), true, 0)
}

/// The purger's pass over the pressure asked of it (`set_pressure`, R1's `return_warm`): released now. The bytes released.
fn serve_ask() -> usize {
    let r1 = LANDING.load(Acquire);
    let bytes = ASK.swap(0, Acquire);
    let asked = if r1 != 0 { ASK_NS.swap(0, Relaxed) } else { 0 };
    let mut got = 0;
    if r1 != 0 {
        got = return_warm_now(((r1 & ASK_BYTES) as usize).max(bytes));
    } else if bytes > 0 {
        let c = ASK_CLASS.load(Relaxed);
        got = purge_now(bytes, (c != NO_CLASS).then(|| Class::from_u8(c)));
    }
    let r1_got = got;
    // D4872 (B): the host's whole-memory ask, after R1's and the ladder's
    let host = if HOST_WHOLE_ASK.swap(false, Acquire) { host_release() } else { 0 };
    if HOST_ASKED.swap(false, Relaxed) {
        HOST_BYTES.fetch_add(host as u64, Relaxed);
    }
    got += host;
    // the warm reserve's shed (`warmres::reserve_ask`, the ladder's background pass), after R1's garbage
    got += crate::warmres::serve_shed().unwrap_or(0);
    // R1's ask served (released, or nothing left to release): no longer landing, unless another came meanwhile (the
    // next pass serves it)
    if r1 != 0 {
        let _ = LANDING.compare_exchange(r1, 0, AcqRel, Relaxed);
    }
    if asked != 0 {
        note_land(asked, r1_got);
    }
    got
}

/// `PGRUST_POOL_FAST_RELEASE`: an R1 ask that came during the purger's other passes, served now.
fn cut_in() {
    if r1_waiting() {
        CUT_INS.fetch_add(1, Relaxed);
        serve_ask();
    }
}

/// Test seam: one purger pass over the pressure asked of it, on the calling thread (a test that starts no pool thread
/// stands in for the purger's wake). The bytes released.
#[doc(hidden)]
pub fn serve_ask_now() -> usize {
    serve_ask()
}

fn run() {
    let cfg = zones().cfg;
    let wait = if cfg.release_mode < 2 && cfg.decay_ms > 0 { (cfg.decay_ms / 4).clamp(10, 1_000) } else { 3_600_000 };
    let window = cfg.idle_drain_ms.saturating_mul(4).max(1);
    // pagepool P3 (c): the purger also sweeps the pool's present bytes every PRESENT_MS (present.rs); the age pass keeps
    // its own cadence (`wait`), run on an ask's wake as before, so the shorter park changes no release (the claimer's
    // decay is rate-limited by its own window)
    let mut age_at = tick();
    let (mut home_at, mut swept_at) = (age_at, age_at);
    let mut pinned = false;
    while !STOP.load(Acquire) {
        crate::affinity::follow(&mut pinned);
        split_asked();
        RETURNING.store(true, SeqCst);
        let returned = RETURN_ASK.swap(false, SeqCst);
        if returned {
            return_pass();
        }
        RETURNING.store(false, Release);
        let asked = ASK.load(Acquire) != 0 || LANDING.load(Acquire) != 0;
        if asked || tick().wrapping_sub(swept_at) >= SWEEP_MS {
            crate::flush_remote();
            swept_at = tick();
        }
        serve_ask();
        // C.23: given base spans back first (a dirty one blocks the next carve), out of the pass's remap budget
        let spent = crate::span::clean_base(if cfg.age_pass_max == 0 { usize::MAX } else { cfg.age_pass_max });
        cut_in();
        let now = tick();
        if returned || asked || now.wrapping_sub(age_at) >= wait {
            if cfg.release_mode < 2 && cfg.decay_ms > 0 && now.wrapping_sub(home_at) >= cfg.decay_ms {
                idle_home_pass();
                home_at = now;
            }
            age_pass();
            age_at = now;
            cut_in();
        }
        // pagepool P3 (F1): the free runs of small blocks reach their superpages' bitmaps only once their lists go home:
        // with lazy release on and age release off, idle lists go home every lazy decay
        if !(cfg.release_mode < 2 && cfg.decay_ms > 0) && cfg.lazy_decay_ms > 0 && now.wrapping_sub(home_at) >= cfg.lazy_decay_ms {
            idle_home_pass();
            home_at = now;
        }
        lazy_pass(tick(), false, spent);
        cut_in();
        crate::claimer::decay(tick(), window, &LAST_DECAY);
        crate::present::refresh_if_due(tick());
        if ASK.load(Acquire) == 0 && LANDING.load(Acquire) == 0 && !RETURN_ASK.load(Acquire) && !HOST_WHOLE_ASK.load(Acquire) {
            let wait = if crate::claimer::target() > cfg.warm_zero { wait.min(window / 2).max(1) } else { wait };
            let left = wait.saturating_sub(tick().wrapping_sub(age_at)).max(1);
            // ppround card 14: PEAK_MS while the claim climbs
            let tick_ms = if CLIMBING.load(Relaxed) { PEAK_MS } else { crate::present::PRESENT_MS };
            std::thread::park_timeout(core::time::Duration::from_millis(left.min(tick_ms) as u64));
        }
    }
}

pub(crate) fn start() -> bool {
    if !THREAD.load(Acquire).is_null() {
        return true;
    }
    spawn()
}

#[cfg(target_os = "linux")]
fn spawn() -> bool {
    STOP.store(false, Relaxed);
    let Ok(h) = std::thread::Builder::new().name("pg:poolpurge".into()).spawn(pagegive::pinned(run)) else { return false };
    let t = Box::into_raw(Box::new(h.thread().clone()));
    if THREAD.compare_exchange(core::ptr::null_mut(), t, AcqRel, Acquire).is_err() {
        // SAFETY: ours, never published.
        drop(unsafe { Box::from_raw(t) });
    }
    true
}

#[cfg(not(target_os = "linux"))]
fn spawn() -> bool {
    let _ = run;
    true
}

/// pppress card 10: ask the purger to split the huge folio holding `at` (a page of a dead range its caller gives back
/// later). False when no purger runs or every slot is taken: the caller splits it itself.
pub(crate) fn ask_split(at: usize) -> bool {
    if THREAD.load(Acquire).is_null() {
        return false;
    }
    let sp = at & !(SUPER - 1);
    if SPLIT_ASKS.iter().any(|s| {
        let v = s.load(Relaxed);
        v != 0 && v & !(SUPER - 1) == sp
    }) {
        return true;
    }
    if !SPLIT_ASKS.iter().any(|s| s.compare_exchange(0, at, AcqRel, Relaxed).is_ok()) {
        return false;
    }
    SPLIT_ASKED.fetch_add(1, Relaxed);
    wake();
    true
}

/// pppress card 10: (splits asked, splits the purger made).
pub(crate) fn split_counts() -> (u64, u64) {
    (SPLIT_ASKED.load(Relaxed), SPLIT_DONE.load(Relaxed))
}

/// Test seam (pppress card 10, memacct's case): while held, the purger leaves the asked splits queued, as if it never
/// ran.
static SPLIT_HOLD: AtomicBool = AtomicBool::new(false);

pub(crate) fn hold_splits(on: bool) {
    SPLIT_HOLD.store(on, Relaxed);
    if !on {
        wake();
    }
}

/// The asked splits: one MADV_COLD page each (advice: no byte changes, whoever holds the superpage now).
fn split_asked() {
    if SPLIT_HOLD.load(Relaxed) {
        return;
    }
    for s in &SPLIT_ASKS {
        let at = s.swap(0, AcqRel);
        if at != 0 {
            #[cfg(target_os = "linux")]
            os::split_partial(at, os::page_size());
            SPLIT_DONE.fetch_add(1, Relaxed);
        }
    }
}

pub(crate) fn wake() {
    let t = THREAD.load(Acquire);
    if !t.is_null() {
        // SAFETY: published once by `start`, never freed.
        unsafe { (*t).unpark() };
    }
}

pub(crate) fn stop() {
    STOP.store(true, Release);
    let t = THREAD.load(Acquire);
    if !t.is_null() {
        // SAFETY: published once by `start`, never freed.
        unsafe { (*t).unpark() };
    }
}
