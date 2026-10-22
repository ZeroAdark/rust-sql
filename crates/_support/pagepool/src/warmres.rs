//! The warm page reserve as a sized cache (`PGRUST_POOL_RESERVE=1`, read once; off, the claimer keeps E1d's target).
//! Counted whatever the switch says: foreground misses by class and ask size, warm takes by state, the claimer's
//! POPULATE time per superpage (log2 µs bins), the deepest drawdown between refills over a 60 s window, and churn (bytes
//! the pool released that the claimer populated again within the window). On: the claimer's target is min(want,
//! budget), want = 1.1 x the window's deepest drawdown (a miss counts at once), at least `pool_warm_zero`; the budget is
//! the memory ladder's (`set_reserve_budget`, which may take it to 0), else E1d's bound. A WarmDirty superpage is zeroed
//! in place before a Cold one is populated, and free superpages past the target go back by age, one superpage a remap.

use crate::superpage::{self, State, SUPER};
use crate::{os, reserve::zones, stats, Class, CLASSES};
use crate::sync::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering::*};

const WINDOW_MS: u32 = 60_000;
const HALF_MS: u32 = WINDOW_MS / 2;
/// Ask sizes a miss is binned by: runs inside a superpage, one superpage, 2-15, 16 or more (32 MiB+).
pub const SIZES: usize = 4;
/// POPULATE time bins: bin b holds [2^(b-1), 2^b) µs, bin 0 under 1 µs, the last everything past.
pub const POP_BINS: usize = 16;
/// Spare free superpages this long free go back; while churn is in the window, half the window.
const SPARE_AGE_MS: u32 = 1_000;
/// The most one claimer pass gives back of the spare, and one purger pass of a shed (one superpage a remap, the thread
/// yielding between remaps); a pass also ends past `PASS_NS`. The rest waits a pass.
const SPARE_PASS_MAX: usize = 16 << 20;
const SHED_PASS_MAX: usize = 16 << 20;
const PASS_NS: u64 = 2_000_000;

static SWITCH: AtomicU8 = AtomicU8::new(0);

static MISS_CLASS: crate::Line<[AtomicU64; CLASSES]> = crate::Line([const { AtomicU64::new(0) }; CLASSES]);
static MISS_SIZE: crate::Line<[AtomicU64; SIZES]> = crate::Line([const { AtomicU64::new(0) }; SIZES]);
static MISS_BYTES: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));
static ZERO_TAKES: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));
static DIRTY_TAKES: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));
/// The episode since the claimer last met its target: the highest free level seen, the deepest fall under it, the
/// bytes missed.
static PEAK: crate::Line<AtomicUsize> = crate::Line(AtomicUsize::new(0));
static DD: crate::Line<AtomicUsize> = crate::Line(AtomicUsize::new(0));
static MISSED: crate::Line<AtomicUsize> = crate::Line(AtomicUsize::new(0));
/// The window's deepest episode, in two halves (this one, the last one), and when this half began (`purger::tick`).
static DEEP: [AtomicUsize; 2] = [const { AtomicUsize::new(0) }; 2];
static HALF_AT: AtomicU32 = AtomicU32::new(0);
static DEEPEST_EVER: AtomicUsize = AtomicUsize::new(0);
static REFILLS: AtomicU64 = AtomicU64::new(0);
static POP_N: AtomicU64 = AtomicU64::new(0);
static POP_NS: AtomicU64 = AtomicU64::new(0);
static POP_MAX_NS: AtomicU64 = AtomicU64::new(0);
static POP_US: [AtomicU64; POP_BINS] = [const { AtomicU64::new(0) }; POP_BINS];
static RECYCLED: AtomicU64 = AtomicU64::new(0);
static RECYCLE_NS: AtomicU64 = AtomicU64::new(0);
/// Released bytes not yet matched by a populate: those released this half and the last; the released count last seen.
static CREDIT: [AtomicUsize; 2] = [const { AtomicUsize::new(0) }; 2];
static REL_SEEN: AtomicU64 = AtomicU64::new(0);
static CHURN: AtomicU64 = AtomicU64::new(0);
static CHURN_AT: AtomicU32 = AtomicU32::new(0);
static SPARE_RELEASED: AtomicU64 = AtomicU64::new(0);
static SHED: AtomicU64 = AtomicU64::new(0);
/// The memory ladder's budget for the reserve (`u64::MAX` = none: E1d's bound).
static BUDGET: AtomicU64 = AtomicU64::new(u64::MAX);
/// The bytes a shed asked of the purger (`reserve_ask`) and not yet released, and when it was asked (mono ns); the
/// sheds served, the last one's and the longest ask-to-released time; the longest single remap (its mmap_lock hold).
static SHED_LEFT: AtomicUsize = AtomicUsize::new(0);
static SHED_ASK_NS: AtomicU64 = AtomicU64::new(0);
static REMAP_MAX_NS: AtomicU64 = AtomicU64::new(0);
static SHED_RUNS: AtomicU64 = AtomicU64::new(0);
static SHED_CLOCK_NS: AtomicU64 = AtomicU64::new(0);
static SHED_CLOCK_MAX_NS: AtomicU64 = AtomicU64::new(0);

/// Whether `PGRUST_POOL_RESERVE=1`, read ONCE: `pagepool::init` reads it first thing, before the pool's first
/// `superpage::put`, so every `took` debits a superpage its `put` posted (`door`) and the account never goes below zero.
#[inline]
pub fn on() -> bool {
    match SWITCH.load(Relaxed) {
        0 => {
            let v = if !crate::hatch_on() && os::env_is_one(c"PGRUST_POOL_RESERVE") { 2 } else { 1 };
            SWITCH.store(v, Relaxed);
            v == 2
        }
        v => v == 2,
    }
}

#[cfg_attr(any(miri, target_family = "wasm"), allow(dead_code))]
pub(crate) fn force(on: bool) {
    debug_assert!(level() == 0, "the reserve switch moves only before the pool's first warm superpage");
    SWITCH.store(if on { 2 } else { 1 }, Relaxed);
}

#[inline]
fn level() -> usize {
    (superpage::count(State::WarmZero) + superpage::count(State::WarmDirty)) * SUPER
}

/// The reserve's account (`memledger::Acct::POOL_RESERVE`, switch on): a free warm superpage in (+) or out (-), as the
/// pool's pooled bytes (claimed at its door: off `pool_unowned`, never counted twice).
#[inline]
pub(crate) fn door(st: State, delta: i64) {
    #[cfg(not(loom))]
    if st != State::Cold && on() {
        memledger::door_pool_reserve(delta);
    }
    #[cfg(loom)]
    let _ = (st, delta);
}

/// Ladder-sized: on, and the memory ladder publishes the reserve's budget.
#[inline]
pub(crate) fn sized() -> bool {
    on() && BUDGET.load(Relaxed) != u64::MAX
}

/// What R1's warm return keeps (the reserve proper, left to the ladder's background pass): the free warm bytes up to
/// the target while ladder-sized, else 0.
pub(crate) fn keep() -> usize {
    if sized() { level().min(crate::claimer::target()) } else { 0 }
}

/// The most R1's and a pressure purge's first WarmDirty / WarmZero steps release: the spare past the target while
/// ladder-sized, else all.
pub(crate) fn spare_cap() -> usize {
    if sized() { level().saturating_sub(crate::claimer::target()) } else { usize::MAX }
}

/// The most `pool_release_mode` (a) / (b)'s age pass releases: the spare past the target while the switch is on (sized
/// or not: aging is no pressure, and the reserve's own spare release ages the rest), else all.
pub(crate) fn age_cap() -> usize {
    if on() { level().saturating_sub(crate::claimer::target()) } else { usize::MAX }
}

#[inline]
pub(crate) fn size_bin(supers: u32) -> usize {
    match supers {
        0 => 0,
        1 => 1,
        2..=15 => 2,
        _ => 3,
    }
}

/// A consumer took a free superpage that was `st` (per superpage, never per lookup).
#[inline]
pub(crate) fn drew(st: State) {
    (if st == State::WarmDirty { &DIRTY_TAKES } else { &ZERO_TAKES }).fetch_add(1, Relaxed);
    let after = level();
    let peak = PEAK.fetch_max(after + SUPER, Relaxed).max(after + SUPER);
    DD.fetch_max(peak - after, Relaxed);
}

/// A take for `class` asking `supers` superpages (0: runs) found no warm superpage and claimed a Cold one.
#[cold]
pub(crate) fn missed(class: Class, supers: u32) {
    MISS_CLASS[class as usize].fetch_add(1, Relaxed);
    MISS_SIZE[size_bin(supers)].fetch_add(1, Relaxed);
    MISS_BYTES.fetch_add(SUPER as u64, Relaxed);
    MISSED.fetch_add(SUPER, Relaxed);
    if on() {
        retarget();
    }
}

#[inline]
fn drawdown_now() -> usize {
    DD.load(Relaxed).saturating_add(MISSED.load(Relaxed))
}

/// The claimer met its target: the episode's depth goes into the window, and a new one starts at the level now.
pub(crate) fn refilled() {
    let depth = DD.swap(0, Relaxed).saturating_add(MISSED.swap(0, Relaxed));
    PEAK.store(level(), Relaxed);
    if depth > 0 {
        DEEP[0].fetch_max(depth, Relaxed);
        DEEPEST_EVER.fetch_max(depth, Relaxed);
        REFILLS.fetch_add(1, Relaxed);
    }
}

/// want from a depth: 1.1 x, at least `floor`, whole superpages.
#[inline]
pub(crate) fn want_of(deepest: usize, floor: usize) -> usize {
    let w = deepest.saturating_add(deepest / 10).max(floor);
    w.checked_next_multiple_of(SUPER).unwrap_or(w & !(SUPER - 1))
}

fn want() -> usize {
    let deep = DEEP[0].load(Relaxed).max(DEEP[1].load(Relaxed)).max(drawdown_now());
    want_of(deep, zones().cfg.warm_zero)
}

/// E1d's bound: `pool_warm_zero_max` (never under `pool_warm_zero`), at most half the claim cap.
#[inline]
pub(crate) fn bound_of(floor: usize, max: usize, cap: usize) -> usize {
    let max = max.max(floor);
    if cap == 0 { max } else { max.min(cap / 2).max(floor) }
}

fn bound() -> usize {
    let cfg = zones().cfg;
    bound_of(cfg.warm_zero, cfg.warm_zero_max, stats::cap())
}

#[inline]
pub(crate) fn budget_of(ladder: u64, bound: usize) -> usize {
    if ladder == u64::MAX { bound } else { usize::try_from(ladder).unwrap_or(usize::MAX) }
}

fn budget() -> usize {
    budget_of(BUDGET.load(Relaxed), bound())
}

fn retarget() {
    crate::claimer::set_target(want().min(budget()));
}

/// The memory ladder's budget for the reserve at its sample (`u64::MAX`: the ladder does not size it; E1d's bound).
/// Off: noted, nothing moves.
pub fn set_reserve_budget(bytes: u64) {
    if BUDGET.swap(bytes, Relaxed) == bytes || !crate::on() || !on() {
        return;
    }
    retarget();
    crate::claimer::maybe_wake();
}

/// The claimer's pass start: the window rolls at each half, released bytes become churn credit, and (on) the target is
/// set from want and the budget.
pub(crate) fn tick(now: u32) {
    let at = HALF_AT.load(Relaxed);
    let since = now.wrapping_sub(at);
    if at == 0 || since >= HALF_MS {
        let stale = at == 0 || since >= 2 * HALF_MS;
        let cur = DEEP[0].swap(0, Relaxed);
        DEEP[1].store(if stale { 0 } else { cur }, Relaxed);
        let c = CREDIT[0].swap(0, Relaxed);
        CREDIT[1].store(if stale { 0 } else { c }, Relaxed);
        HALF_AT.store(now.max(1), Relaxed);
    }
    let rel = stats::RELEASED_BYTES.load(Relaxed);
    let new = rel.saturating_sub(REL_SEEN.swap(rel, Relaxed));
    if new > 0 {
        CREDIT[0].fetch_add(usize::try_from(new).unwrap_or(usize::MAX), Relaxed);
    }
    if on() {
        retarget();
    }
}

/// The claimer populated a Cold superpage in `ns`: the POPULATE histogram, and churn when released bytes are unmatched.
pub(crate) fn populated(ns: u64) {
    POP_N.fetch_add(1, Relaxed);
    POP_NS.fetch_add(ns, Relaxed);
    POP_MAX_NS.fetch_max(ns, Relaxed);
    let us = ns / 1_000;
    POP_US[((u64::BITS - us.leading_zeros()) as usize).min(POP_BINS - 1)].fetch_add(1, Relaxed);
    for c in [&CREDIT[1], &CREDIT[0]] {
        if c.fetch_update(Relaxed, Relaxed, |v| v.checked_sub(SUPER)).is_ok() {
            CHURN.fetch_add(SUPER as u64, Relaxed);
            CHURN_AT.store(crate::purger::tick().max(1), Relaxed);
            return;
        }
    }
}

/// On: a free WarmDirty superpage zeroed in place and made WarmZero (no fault, no new memory). False: none free.
pub(crate) fn recycle_one() -> bool {
    let Some(i) = superpage::take_one(State::WarmDirty) else { return false };
    let t0 = pg_clock::mono_ns();
    // SAFETY: a free superpage we hold, open read-write; nothing else reads or writes it until it is put back.
    unsafe { core::ptr::write_bytes(superpage::addr_of(i) as *mut u8, 0, SUPER) };
    superpage::put(i, State::WarmZero);
    RECYCLED.fetch_add(1, Relaxed);
    RECYCLE_NS.fetch_add(pg_clock::mono_ns().saturating_sub(t0), Relaxed);
    true
}

/// On, after the claimer's pass: free superpages past the target, free `SPARE_AGE_MS` or more (half the window while
/// churn is in it), go back, at most `SPARE_PASS_MAX` a pass.
pub(crate) fn trim_spare(now: u32) {
    if !on() {
        return;
    }
    let spare = level().saturating_sub(crate::claimer::target());
    if spare < SUPER {
        return;
    }
    let churned = CHURN_AT.load(Relaxed);
    let age = if churned != 0 && now.wrapping_sub(churned) < WINDOW_MS { HALF_MS } else { SPARE_AGE_MS };
    let got = release(spare.min(SPARE_PASS_MAX), age, now);
    SPARE_RELEASED.fetch_add(got as u64, Relaxed);
}

/// The memory ladder's own passes (never an inline draw's), after R1's garbage: `want` bytes of the reserve, in whole
/// superpages, asked of the purger, which gives them back over its next passes (`SHED_PASS_MAX` a pass, one superpage a
/// remap). The budget comes down to what is left, under the floor if need be: the claimer refills nothing past it until
/// the ladder raises it. The bytes landing (0 unless ladder-sized).
pub fn reserve_ask(want: usize) -> usize {
    if !crate::on() || !sized() || want == 0 {
        return 0;
    }
    let held = level();
    let bytes = want.checked_next_multiple_of(SUPER).unwrap_or(usize::MAX).min(held);
    BUDGET.store((held - bytes) as u64, Relaxed);
    retarget();
    if bytes != 0 {
        if SHED_LEFT.fetch_add(bytes, AcqRel) == 0 {
            SHED_ASK_NS.store(pg_clock::mono_ns(), Relaxed);
        }
        crate::purger::wake();
    }
    bytes
}

/// A shed asked and not yet served: the claimer fills nothing.
#[inline]
pub(crate) fn shed_pending() -> bool {
    SHED_LEFT.load(Relaxed) != 0
}

/// The purger's pass, after R1's ask: up to `SHED_PASS_MAX` of a shed asked (WarmDirty first), one superpage a remap, the
/// purger yielding between them; what is left asks the next pass. The bytes released (None: no shed asked).
pub(crate) fn serve_shed() -> Option<usize> {
    let left = SHED_LEFT.load(Acquire);
    if left == 0 {
        return None;
    }
    let got = release(left.min(SHED_PASS_MAX), 0, crate::purger::tick());
    SHED.fetch_add(got as u64, Relaxed);
    // nothing released: nothing free is left to shed
    let _ = SHED_LEFT.fetch_update(AcqRel, Acquire, |v| Some(if got == 0 { 0 } else { v.saturating_sub(got) }));
    if SHED_LEFT.load(Acquire) == 0 {
        SHED_RUNS.fetch_add(1, Relaxed);
        let clock = pg_clock::mono_ns().saturating_sub(SHED_ASK_NS.load(Relaxed));
        SHED_CLOCK_NS.store(clock, Relaxed);
        SHED_CLOCK_MAX_NS.fetch_max(clock, Relaxed);
    } else {
        crate::purger::wake();
    }
    Some(got)
}

/// The reserve proper R1 leaves to the ladder's background pass (0 unless ladder-sized): its releasable bytes there.
pub fn reserve_kept() -> u64 {
    if crate::on() { keep() as u64 } else { 0 }
}

/// Whether `PGRUST_POOL_RESERVE=1` (the ladder sizes the reserve only then).
pub fn reserve_on() -> bool {
    on()
}

/// The reserve's start for the ladder's sizing: E1d's bound (0 with the pool off).
pub fn reserve_bound() -> u64 {
    if crate::on() { bound() as u64 } else { 0 }
}

/// The memory ladder's probe of the reserve: (bytes held: free warm superpages, the foreground miss bytes so far).
pub fn reserve_probe() -> (u64, u64) {
    if !crate::on() {
        return (0, 0);
    }
    (level() as u64, MISS_BYTES.load(Relaxed))
}

/// Free whole superpages, WarmDirty then WarmZero, `min_age_ms` free or more, back to the OS until `want` bytes or
/// `PASS_NS`: each its own `os::release` (`pagegive::give_back` of one 2 MiB superpage by remap, never a MADV_DONTNEED),
/// the thread yielding between them so a faulting thread's mmap_lock wait stays one remap. The bytes released.
fn release(want: usize, min_age_ms: u32, now: u32) -> usize {
    let n = zones().nsuper;
    let end = pg_clock::mono_ns().saturating_add(PASS_NS);
    let mut got = 0;
    for st in [State::WarmDirty, State::WarmZero] {
        let mut i = 0;
        while i < n && got < want && superpage::count(st) > 0 {
            let m = superpage::meta(i);
            if m.state() == st && now.wrapping_sub(m.dirty_at.load(Relaxed)) >= min_age_ms {
                if got != 0 {
                    if pg_clock::mono_ns() >= end {
                        return got;
                    }
                    std::thread::yield_now();
                }
                match superpage::take_bit(i) {
                    // Cold since the state read (a move took its pages: frames' commit, a span's fill or commit):
                    // never claimed, so nothing to give back and no release to count
                    Some(State::Cold) => superpage::put(i, State::Cold),
                    Some(had) => {
                        let t0 = pg_clock::mono_ns();
                        // SAFETY: a whole free superpage we hold: nothing reads it; the pool zone stays mapped (a remap).
                        let rc = unsafe { os::release(superpage::addr_of(i), SUPER) };
                        REMAP_MAX_NS.fetch_max(pg_clock::mono_ns().saturating_sub(t0), Relaxed);
                        if rc == 0 {
                            superpage::put(i, State::Cold);
                            stats::claim(-(SUPER as isize));
                            stats::RELEASES.fetch_add(1, Relaxed);
                            stats::RELEASED_BYTES.fetch_add(SUPER as u64, Relaxed);
                            got += SUPER;
                        } else {
                            superpage::put(i, had);
                            return got;
                        }
                    }
                    None => {}
                }
            }
            i += 1;
        }
    }
    got
}

/// The reserve's counters and sizes now (racy reads).
#[derive(Clone, Copy, Debug, Default)]
pub struct Reserve {
    pub on: bool,
    pub warm_zero: usize,
    pub warm_dirty: usize,
    pub target: usize,
    pub want: usize,
    /// The ladder's (`u64::MAX`: none), and E1d's bound.
    pub budget: u64,
    pub bound: usize,
    pub floor: usize,
    /// Superpages a take claimed Cold for want of a warm one, by class (`Class as usize`) and by ask size (`SIZES`).
    pub miss_class: [u64; CLASSES],
    pub miss_size: [u64; SIZES],
    pub miss_bytes: u64,
    pub zero_takes: u64,
    pub dirty_takes: u64,
    pub drawdown: usize,
    pub deepest: usize,
    pub deepest_ever: usize,
    pub refills: u64,
    pub populate_n: u64,
    pub populate_ns: u64,
    pub populate_max_ns: u64,
    pub populate_us: [u64; POP_BINS],
    pub recycled: u64,
    pub recycle_ns: u64,
    pub churn: u64,
    pub spare_released: u64,
    pub shed: u64,
    /// Sheds served, and the last one's and the longest time from the ladder's ask to the bytes released (ns).
    pub shed_runs: u64,
    pub shed_clock_ns: u64,
    pub shed_clock_max_ns: u64,
    /// The longest single remap of the reserve's releases (ns): the most a faulting thread waits on one.
    pub remap_max_ns: u64,
}

pub fn reserve_stats() -> Reserve {
    if !crate::on() {
        return Reserve { on: on(), ..Reserve::default() };
    }
    let cfg = zones().cfg;
    let ld = |a: &AtomicU64| a.load(Relaxed);
    Reserve {
        on: on(),
        warm_zero: superpage::count(State::WarmZero) * SUPER,
        warm_dirty: superpage::count(State::WarmDirty) * SUPER,
        target: crate::claimer::target(),
        want: want(),
        budget: BUDGET.load(Relaxed),
        bound: bound(),
        floor: cfg.warm_zero,
        miss_class: core::array::from_fn(|c| ld(&MISS_CLASS[c])),
        miss_size: core::array::from_fn(|s| ld(&MISS_SIZE[s])),
        miss_bytes: ld(&MISS_BYTES),
        zero_takes: ld(&ZERO_TAKES),
        dirty_takes: ld(&DIRTY_TAKES),
        drawdown: drawdown_now(),
        deepest: DEEP[0].load(Relaxed).max(DEEP[1].load(Relaxed)),
        deepest_ever: DEEPEST_EVER.load(Relaxed),
        refills: ld(&REFILLS),
        populate_n: ld(&POP_N),
        populate_ns: ld(&POP_NS),
        populate_max_ns: ld(&POP_MAX_NS),
        populate_us: core::array::from_fn(|b| ld(&POP_US[b])),
        recycled: ld(&RECYCLED),
        recycle_ns: ld(&RECYCLE_NS),
        churn: ld(&CHURN),
        spare_released: ld(&SPARE_RELEASED),
        shed: ld(&SHED),
        shed_runs: ld(&SHED_RUNS),
        shed_clock_ns: ld(&SHED_CLOCK_NS),
        shed_clock_max_ns: ld(&SHED_CLOCK_MAX_NS),
        remap_max_ns: ld(&REMAP_MAX_NS),
    }
}

#[cfg(test)]
mod t {
    use super::*;

    const MIB: usize = 1 << 20;

    #[test]
    fn want_is_a_tenth_over_the_deepest_drawdown_with_a_floor_in_whole_superpages() {
        assert_eq!(want_of(0, 16 * MIB), 16 * MIB, "no drawdown: the floor");
        assert_eq!(want_of(10 * MIB, 16 * MIB), 16 * MIB, "11 MiB is under the floor");
        assert_eq!(want_of(100 * MIB, 16 * MIB), 110 * MIB);
        assert_eq!(want_of(20 * MIB, 16 * MIB), 22 * MIB);
        assert_eq!(want_of(21 * MIB, 16 * MIB), 24 * MIB, "23.1 MiB rounds up to whole superpages");
        assert_eq!(want_of(usize::MAX - SUPER, 0) % SUPER, 0, "saturates, never wraps");
    }

    #[test]
    fn the_budget_is_the_ladders_else_e1ds_bound() {
        assert_eq!(bound_of(16 * MIB, 256 * MIB, 0), 256 * MIB, "no cap: pool_warm_zero_max");
        assert_eq!(bound_of(16 * MIB, 256 * MIB, 128 * MIB), 64 * MIB, "half the claim cap");
        assert_eq!(bound_of(16 * MIB, 256 * MIB, 8 * MIB), 16 * MIB, "never under the floor");
        assert_eq!(bound_of(16 * MIB, 0, 0), 16 * MIB, "a max under the floor is the floor");
        assert_eq!(budget_of(u64::MAX, 256 * MIB), 256 * MIB, "no ladder budget: the bound");
        assert_eq!(budget_of(0, 256 * MIB), 0, "the ladder took the reserve: 0, under the floor");
        assert_eq!(budget_of(512 << 20, 256 * MIB), 512 * MIB, "the ladder's raise passes E1d's bound");
    }

    #[test]
    fn misses_bin_by_ask_size() {
        assert_eq!([0, 1, 2, 15, 16, 4096].map(size_bin), [0, 1, 2, 2, 3, 3]);
    }
}
