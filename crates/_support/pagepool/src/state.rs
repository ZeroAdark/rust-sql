//! The pick order, each class's partial superpages (binned by their longest free stretch, smallest first; by used runs,
//! fullest first, with `PGRUST_POOL_HOLE_FIT=0`) and the low-warm flag. A class's
//! lists and run table entries change under its spin bit, every run bitmap and partial list under the runs bit inside it.

use crate::reserve::Zones;
use crate::run::RUN;
use crate::stats;
use crate::superpage::{self, meta, State, ALL_RUNS, NONE, RUNS_PER, SUPER};
use crate::{Class, Zero, CLASSES};
use crate::sync::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering::*};

/// A hole bin per longest free stretch (1 .. 31 runs).
const BINS: usize = RUNS_PER as usize - 1;
/// The used-run bins (`PGRUST_POOL_HOLE_FIT=0`): 1 = fullest .. 4.
const USED_BINS: usize = 4;
/// Partial-list fronts searched per bin before a fresh superpage is claimed.
const SEARCH: usize = 16;

static LOCKS: [crate::Line<AtomicU32>; CLASSES] = [const { crate::Line(AtomicU32::new(0)) }; CLASSES];
static BIN_HEADS: [crate::Line<[AtomicU32; BINS]>; CLASSES] = [const { crate::Line([const { AtomicU32::new(NONE) }; BINS]) }; CLASSES];
/// Lock order: a class's spin bit, then this one, innermost; a take never takes another class's bit.
static RUNS: crate::Line<AtomicU32> = crate::Line(AtomicU32::new(0));
static LOW: AtomicBool = AtomicBool::new(false);

struct Runs;

impl Drop for Runs {
    fn drop(&mut self) {
        superpage::unspin(&RUNS);
    }
}

fn runs() -> Runs {
    superpage::spin(&RUNS);
    Runs
}

pub(crate) struct Guard {
    class: Class,
}

impl Drop for Guard {
    fn drop(&mut self) {
        superpage::unspin(&LOCKS[self.class as usize]);
    }
}

pub(crate) fn lock(class: Class) -> Guard {
    superpage::spin(&LOCKS[class as usize]);
    Guard { class }
}

pub(crate) fn init(z: &Zones) {
    let per = superpage::per_chunk();
    let stride = superpage::stride_slots();
    let mut i = 0;
    while i < z.nsuper {
        if i % stride < per {
            superpage::put(i, State::Cold);
        }
        i += 1;
    }
}

/// A whole free superpage for `class`, by the pick order for `zero`; held (Busy) on return. A Cold one counts as claimed
/// and is refused past the cap. `supers`: the ask's superpages (0: runs), for the miss counts.
pub(crate) fn claim(zero: Zero, class: Class, supers: u32) -> Option<u32> {
    // pppress card 6 (`PGRUST_POOL_GROW_GUARD`): a zeroed take the ladder presses reuses a free warm superpage (zeroed by
    // its taker) before it claims a Cold one
    let pressed = zero != Zero::No && crate::purger::pressed(SUPER);
    let order: &[State] = match zero {
        Zero::No => &[State::WarmDirty, State::WarmZero, State::Cold],
        _ if pressed => &[State::WarmZero, State::WarmDirty, State::Cold],
        Zero::Dense => &[State::WarmZero, State::Cold, State::WarmDirty],
        Zero::Sparse => &[State::Cold, State::WarmZero, State::WarmDirty],
    };
    for &st in order {
        if st == State::Cold && !stats::under_cap(SUPER) {
            continue;
        }
        if let Some(i) = superpage::take_one(st) {
            took(st, zero, class, supers);
            if pressed && st == State::WarmDirty {
                crate::purger::note_warm_first();
            }
            if st == State::Cold {
                crate::purger::fresh(i);
            }
            return Some(i);
        }
    }
    None
}

/// Count a superpage taken for an ask with `zero`; a Cold one for an ask that wanted warm is a warm-list miss.
pub(crate) fn took(st: State, zero: Zero, class: Class, supers: u32) {
    match st {
        State::Cold => {
            stats::claim(SUPER as isize);
            stats::COLD_TAKES.fetch_add(1, Relaxed);
            crate::purger::touch();
            if zero != Zero::Sparse {
                crate::claimer::missed(class, supers);
            }
        }
        _ => {
            stats::WARM_TAKES.fetch_add(1, Relaxed);
            crate::warmres::drew(st);
            if st == State::WarmZero {
                crate::claimer::maybe_wake();
            }
        }
    }
}

/// D4872 (`PGRUST_POOL_HOLE_FIT`, default on; `=0` turns it off), read once (0: unread, 1: off, 2: on): a partial
/// superpage is listed in the bin of its longest stretch of free runs, and a take of `k` runs searches bin `k` (every
/// superpage there holds `k` free runs in a row), then the longer ones, smallest first. Listed by used runs, a take of
/// 21-31 runs saw 16 superpages a bin, none with the stretch, and claimed a fresh superpage while thousands past them had
/// it (an x86 wide-table bulk load: 50k claims, 16 GB of free runs stranded in 9.6k superpages against 7 GB owned).
static HOLE_SWITCH: AtomicU8 = AtomicU8::new(0);

pub(crate) fn hole_fit_on() -> bool {
    match HOLE_SWITCH.load(Relaxed) {
        0 => {
            let v = if crate::os::env_on_by_default(c"PGRUST_POOL_HOLE_FIT") { 2 } else { 1 };
            HOLE_SWITCH.store(v, Relaxed);
            v == 2
        }
        v => v == 2,
    }
}

pub(crate) fn force_hole_fit(on: bool) {
    HOLE_SWITCH.store(if on { 2 } else { 1 }, Relaxed);
}

/// The longest stretch of set bits in `free`.
#[inline]
pub(crate) fn longest_free(mut free: u32) -> u32 {
    let mut n = 0;
    while free != 0 {
        free &= free << 1;
        n += 1;
    }
    n
}

/// Hole bin (the longest free stretch, 1 .. 31) of a superpage with free runs `free`, 0 = none free or all.
#[inline]
fn hole_bin(free: u32) -> u32 {
    let s = longest_free(free);
    if s as usize > BINS { 0 } else { s }
}

/// The bins a take of `k` runs searches, in order (list indices). With the switch on: bin `k`, then the longer ones. A
/// stretch of `k` may still lack a start aligned to `align` > 1 runs; the per-bin SEARCH walk covers that.
#[inline]
fn take_bins(k: u32) -> core::ops::Range<usize> {
    if hole_fit_on() {
        (k as usize).clamp(1, BINS) - 1..BINS
    } else {
        0..USED_BINS
    }
}

/// Bin (1 = fullest .. 4) of a superpage with `used` runs taken, 0 = listed nowhere (empty or full).
#[inline]
fn bin_for(used: u32) -> u32 {
    match used {
        0 => 0,
        u if u >= RUNS_PER => 0,
        u if u >= 24 => 1,
        u if u >= 16 => 2,
        u if u >= 8 => 3,
        _ => 4,
    }
}

fn unlist(class: Class, i: u32) {
    let m = meta(i);
    let b = m.bin();
    if b == 0 {
        return;
    }
    let (p, n) = (m.prev.load(Relaxed), m.next.load(Relaxed));
    if p == NONE {
        BIN_HEADS[class as usize][b as usize - 1].store(n, Relaxed);
    } else {
        meta(p).next.store(n, Relaxed);
    }
    if n != NONE {
        meta(n).prev.store(p, Relaxed);
    }
    m.set_bin(0);
}

fn list(class: Class, i: u32, b: u32) {
    let m = meta(i);
    let head = &BIN_HEADS[class as usize][b as usize - 1];
    let h = head.load(Relaxed);
    m.prev.store(NONE, Relaxed);
    m.next.store(h, Relaxed);
    if h != NONE {
        meta(h).prev.store(i, Relaxed);
    }
    head.store(i, Relaxed);
    m.set_bin(b);
}

/// A run superpage's free runs given back to the OS (zero when taken again), in its span_n word. Under the runs bit.
#[inline]
fn gone(m: &superpage::SuperMeta) -> &AtomicU32 {
    &m.span_n
}

/// pagepool P3 (F1): a run superpage's free runs the last age mark saw free, none taken since (`take_mask` clears them),
/// in its dirty_at word (a free superpage's age, unused while it holds runs). Under the runs bit.
#[inline]
fn seen(m: &superpage::SuperMeta) -> &AtomicU32 {
    &m.dirty_at
}

#[inline]
fn free_runs_moved(runs: isize) {
    stats::STRANDED.fetch_add(runs * RUN as isize, Relaxed);
    stats::FREE_RUNS.fetch_add(runs * RUN as isize, Relaxed);
}

/// Put superpage `i` where its free runs say: listed Partial, Full, or a whole free superpage again (lock held). Whole
/// free with gone runs, the superpage is claimed whole again (those pages fault in on touch).
fn settle(class: Class, i: u32) {
    let m = meta(i);
    // pppress card 8: the activity bit outlives the word's rewrite below
    let active = m.word.load(Relaxed) & superpage::ACTIVE;
    let free = m.free_runs.load(Relaxed);
    let used = RUNS_PER - free.count_ones();
    let want = if hole_fit_on() { hole_bin(free) } else { bin_for(used) };
    if used == 0 {
        unlist(class, i);
        let g = gone(m).swap(0, Relaxed).count_ones() as isize;
        free_runs_moved(g - RUNS_PER as isize);
        if g != 0 {
            stats::claim(g * RUN as isize);
        }
        let st = if m.zero_runs.load(Relaxed) == ALL_RUNS { State::WarmZero } else { State::WarmDirty };
        superpage::put(i, st);
        return;
    }
    if m.bin() != want {
        unlist(class, i);
        if want != 0 {
            list(class, i, want);
        }
    }
    let st = if used == RUNS_PER { State::Full } else { State::Partial };
    m.set(st, class, want, 0, NONE);
    if active != 0 {
        m.word.fetch_or(active, Relaxed);
    }
}

/// The first `k` free runs of `free` starting at a multiple of `align`, as a mask.
#[inline]
pub(crate) fn find_runs(free: u32, k: u32, align: u32) -> Option<u32> {
    let one = ((1u64 << k) - 1) as u32;
    let mut s = 0;
    while s + k <= RUNS_PER {
        let mask = one << s;
        if free & mask == mask {
            return Some(mask);
        }
        s += align.max(1);
    }
    None
}

/// A superpage's free-run bitmap as the runs bit's kernels see it (std's atomics here, loom's in its model).
pub trait FreeRuns {
    fn get(&self) -> u32;
    fn clear(&self, mask: u32);
    fn add(&self, mask: u32) -> u32;
}

impl FreeRuns for AtomicU32 {
    fn get(&self) -> u32 {
        self.load(Relaxed)
    }
    fn clear(&self, mask: u32) {
        self.fetch_and(!mask, Relaxed);
    }
    fn add(&self, mask: u32) -> u32 {
        self.fetch_or(mask, Relaxed)
    }
}

/// Under the runs bit: the first `k` free runs of `free` at a multiple of `align`, taken.
pub fn take_free<F: FreeRuns>(free: &F, k: u32, align: u32) -> Option<u32> {
    let mask = find_runs(free.get(), k, align)?;
    free.clear(mask);
    Some(mask)
}

/// Under the runs bit: `mask` free again; false when any of it already was.
pub fn give_free<F: FreeRuns>(free: &F, mask: u32) -> bool {
    free.add(mask) & mask == 0
}

fn take_mask(class: Class, i: u32, mask: u32) -> (usize, bool) {
    let m = meta(i);
    let zero = m.zero_runs.load(Relaxed) & mask == mask;
    m.zero_runs.fetch_and(!mask, Relaxed);
    seen(m).fetch_and(!mask, Relaxed);
    let g = gone(m).fetch_and(!mask, Relaxed) & mask;
    free_runs_moved(-((mask & !g).count_ones() as isize));
    if g != 0 {
        stats::claim((g.count_ones() as usize * RUN) as isize);
    }
    settle(class, i);
    // pppress card 8: runs taken here
    active_sp(i);
    (superpage::addr_of(i) + mask.trailing_zeros() as usize * RUN, zero)
}

/// `k` runs (start aligned to `align` runs) for the guard's class: a partial superpage of its own class, smallest fitting
/// hole first (fullest first with `PGRUST_POOL_HOLE_FIT=0`), else another class's, else a fresh one. The address and
/// whether the runs are zero.
pub(crate) fn take_runs_locked(g: &Guard, k: u32, align: u32, zero: Zero) -> Option<(usize, bool)> {
    let class = g.class;
    let _r = runs();
    for c in core::iter::once(class).chain(Class::ALL.into_iter().filter(|&c| c != class)) {
        for b in take_bins(k) {
            let mut i = BIN_HEADS[c as usize][b].load(Relaxed);
            let mut n = 0;
            while i != NONE && n < SEARCH {
                let m = meta(i);
                if let Some(mask) = take_free(&m.free_runs, k, align) {
                    if regain_ok(m, mask) {
                        return Some(take_mask(c, i, mask));
                    }
                    give_free(&m.free_runs, mask);
                }
                i = m.next.load(Relaxed);
                n += 1;
            }
        }
    }
    let i = claim(zero, class, 0)?;
    let m = meta(i);
    m.free_runs.store(ALL_RUNS, Relaxed);
    m.set(State::Partial, class, 0, 0, NONE);
    gone(m).store(0, Relaxed);
    seen(m).store(0, Relaxed);
    free_runs_moved(RUNS_PER as isize);
    let mask = take_free(&m.free_runs, k, align).expect("k <= 32 runs fit an empty superpage");
    Some(take_mask(class, i, mask))
}

/// Take the `k` runs at `addr` if they are free in a partial superpage (an in-place grow).
pub(crate) fn take_runs_at_locked(_g: &Guard, addr: usize, k: u32) -> bool {
    if !crate::reserve::in_pool(addr) {
        return false;
    }
    let i = superpage::index_of(addr);
    let first = ((addr - superpage::addr_of(i)) / RUN) as u32;
    if first + k > RUNS_PER {
        return false;
    }
    let m = meta(i);
    let _r = runs();
    if !matches!(m.state(), State::Partial) {
        return false;
    }
    let mask = ((((1u64 << k) - 1) as u32) << first) as u32;
    if m.free_runs.get() & mask != mask || !regain_ok(m, mask) {
        return false;
    }
    m.free_runs.clear(mask);
    take_mask(m.class(), i, mask);
    true
}

pub(crate) fn free_runs_locked(_g: &Guard, addr: usize, k: u32, _zero: u32) {
    let i = superpage::index_of(addr);
    let first = ((addr - superpage::addr_of(i)) / RUN) as u32;
    let mask = ((((1u64 << k) - 1) as u32) << first) as u32;
    let m = meta(i);
    let _r = runs();
    let once = give_free(&m.free_runs, mask);
    debug_assert!(once, "runs freed twice");
    free_runs_moved(k as isize);
    settle(m.class(), i);
    // pppress card 8: runs given back here
    active_sp(i);
}

/// pppress card 8 (`pool_thp_partial` 3): superpage `i` was used now (a take or a give of its runs).
#[inline]
fn active_sp(i: u32) {
    if crate::thp_partial() == crate::THP_COLD {
        let w = &meta(i).word;
        if w.load(Relaxed) & superpage::ACTIVE == 0 {
            w.fetch_or(superpage::ACTIVE, Relaxed);
        }
    }
}

/// pppress card 8 (`pool_thp_partial` 3): the superpages (pool zone) and lazy span pieces `[a, a + len)` covers were used
/// now (an owner's `give_pages` / `take_pages`, a lazy commit). The age passes leave a used one's runs held until a whole
/// decay passes with no use (`clear_active` at each mark).
#[inline]
pub(crate) fn note_active(a: usize, len: usize) {
    if crate::thp_partial() != crate::THP_COLD || len == 0 {
        return;
    }
    let z = crate::reserve::zones();
    let mut p = a & !(SUPER - 1);
    while p < a + len {
        if z.pool.has(p) {
            let i = superpage::index_of(p);
            if i < z.nsuper {
                active_sp(i);
            }
        } else if z.lazy.has(p) {
            crate::span::note_active_piece(p);
        }
        p += SUPER;
    }
}

/// pppress card 8: whether the age passes leave the runs at `a` held: mode 3, the ladder not pressing, and the superpage
/// or lazy span piece used since the last mark.
#[inline]
pub(crate) fn piece_hot(a: usize) -> bool {
    if crate::thp_partial() != crate::THP_COLD || crate::purger::ladder_pressed() {
        return false;
    }
    let z = crate::reserve::zones();
    if z.pool.has(a) {
        let i = superpage::index_of(a);
        i < z.nsuper && meta(i).word.load(Relaxed) & superpage::ACTIVE != 0
    } else {
        z.lazy.has(a) && crate::span::piece_active(a)
    }
}

/// pppress card 8: every activity bit cleared, at the age mark after the passes that read them (a new decay).
pub(crate) fn clear_active() {
    if crate::thp_partial() != crate::THP_COLD {
        return;
    }
    for i in 0..crate::reserve::zones().nsuper {
        let w = &meta(i).word;
        if w.load(Relaxed) & superpage::ACTIVE != 0 {
            w.fetch_and(!superpage::ACTIVE, Relaxed);
        }
    }
    crate::span::clear_active_pieces();
}

/// Gone runs in `mask` are claimed again when taken: under the cap (PoolAlloc's own claims pass it).
fn regain_ok(m: &superpage::SuperMeta, mask: u32) -> bool {
    let g = gone(m).load(Relaxed) & mask;
    g == 0 || stats::under_cap(g.count_ones() as usize * RUN)
}

/// Pressure only (the purger, R1): the resident free runs of superpages not wholly free go back to the OS, each
/// stretch of them one `os::release` (a MADV_DONTNEED inside a superpage that keeps a run in use, never a whole one).
/// Taken out of the free bitmaps under the runs bit, released outside it, then free again and gone. The bytes released.
/// `fast` (`PGRUST_POOL_FAST_RELEASE`): the largest holes first (hole bin 31 .. 1; the emptiest superpages, used bin 4 .. 1,
/// with `PGRUST_POOL_HOLE_FIT=0`), so each folio split frees the most.
pub(crate) fn release_free_runs(want: usize, fast: bool) -> usize {
    release_free_runs_but(want, fast, NONE)
}

/// `release_free_runs`, never superpage `skip`'s runs (pppress card 6: a fresh one's, never touched).
pub(crate) fn release_free_runs_but(want: usize, fast: bool, skip: u32) -> usize {
    // D4760 (`pool_thp_partial` 1): every stretch here is part of a superpage in use, so none goes back: the runs stay
    // free, resident and reusable (counted held; `releasable` leaves them out)
    if crate::thp_partial() == crate::THP_KEEP {
        let free = stats::FREE_RUNS.load(Relaxed).max(0) as usize;
        if free > 0 {
            stats::THP_KEPT.fetch_add(1, Relaxed);
            stats::THP_KEPT_BYTES.fetch_add(free.min(want) as u64, Relaxed);
        }
        return 0;
    }
    let mut got = 0;
    let mut batch = [(NONE, 0u32); 32];
    while got < want {
        let mut n = 0;
        let mut asked = 0;
        {
            let _r = runs();
            'scan: for c in Class::ALL {
                for b in 0..BINS {
                    let b = if fast { BINS - 1 - b } else { b };
                    let mut i = BIN_HEADS[c as usize][b].load(Relaxed);
                    while i != NONE {
                        let m = meta(i);
                        let free = m.free_runs.get() & !gone(m).load(Relaxed);
                        if free != 0 && i != skip {
                            m.free_runs.clear(free);
                            batch[n] = (i, free);
                            n += 1;
                            asked += free.count_ones() as usize * RUN;
                            if n == batch.len() || got + asked >= want {
                                break 'scan;
                            }
                        }
                        i = m.next.load(Relaxed);
                    }
                }
            }
        }
        if n == 0 {
            break;
        }
        let mut done = [0u32; 32];
        for (k, &(i, free)) in batch[..n].iter().enumerate() {
            let mut rest = free;
            while rest != 0 {
                let lo = rest.trailing_zeros();
                let len = (rest >> lo).trailing_ones();
                let mask = ((((1u64 << len) - 1) as u32) << lo) as u32;
                // SAFETY: free runs out of the bitmap, handed out by no one; a run stays in use, so never a whole superpage
                if unsafe { crate::os::release(superpage::addr_of(i) + lo as usize * RUN, len as usize * RUN) } == 0 {
                    done[k] |= mask;
                }
                rest &= !mask;
            }
        }
        let before = got;
        let _r = runs();
        for (k, &(i, free)) in batch[..n].iter().enumerate() {
            let m = meta(i);
            let g = done[k];
            gone(m).fetch_or(g, Relaxed);
            m.zero_runs.fetch_or(g, Relaxed);
            give_free(&m.free_runs, free);
            let bytes = g.count_ones() as usize * RUN;
            free_runs_moved(-(g.count_ones() as isize));
            if bytes > 0 {
                stats::claim(-(bytes as isize));
                stats::RELEASES.fetch_add(1, Relaxed);
                stats::RELEASED_BYTES.fetch_add(bytes as u64, Relaxed);
            }
            got += bytes;
            settle(m.class(), i);
        }
        if got == before || (n < batch.len() && got < want) {
            break;
        }
    }
    got
}

/// pagepool P3 (F1): the purger's age mark over the free runs of superpages still in use, every `pool_lazy_decay_ms`.
/// Superpage by superpage (the runs bit taken for 64 of them at a time), a free run the last mark saw (`seen`: free a
/// whole decay or more, never taken since) and not gone is RIPE: out of the free bitmap, given back outside the bit (one
/// `os::release` a stretch: a MADV_DONTNEED inside a superpage that keeps a run in use, never a whole one; none for a
/// stretch with no page resident: `os::age_release`), then free again and gone, as `release_free_runs` does; at most
/// `budget` resident bytes given back, and none past the pass's time (the rest stay seen, RIPE at the next mark). Every
/// other free run is seen from here. Returns (bytes released as counted, resident bytes given back).
pub(crate) fn age_free_runs(budget: usize) -> (usize, usize) {
    // D4760 (`pool_thp_partial` 1): as `release_free_runs`, no part of a superpage in use goes back (no count: a pass
    // every pool_lazy_decay_ms)
    if crate::thp_partial() == crate::THP_KEEP {
        return (0, 0);
    }
    let n_super = crate::reserve::zones().nsuper;
    let (mut got, mut remapped) = (0usize, 0usize);
    let mut batch = [(NONE, 0u32); 32];
    let mut i = 0u32;
    while i < n_super {
        let end = (i + 64).min(n_super);
        let mut n = 0;
        let mut asked = 0usize;
        {
            let _r = runs();
            while i < end && n < batch.len() {
                let m = meta(i);
                if m.state() == State::Partial {
                    let free = m.free_runs.get() & !gone(m).load(Relaxed);
                    let cand = seen(m).load(Relaxed) & free;
                    // pppress card 8 (`pool_thp_partial` 3): a superpage used within the decay keeps its free runs
                    let hot = cand != 0 && piece_hot(superpage::addr_of(i));
                    if hot {
                        stats::THP_HOT_SKIPS.fetch_add(1, Relaxed);
                    }
                    let ripe = if cand != 0 && !hot && remapped + asked < budget && !crate::purger::pass_late() { cand } else { 0 };
                    if ripe != 0 {
                        m.free_runs.clear(ripe);
                        batch[n] = (i, ripe);
                        n += 1;
                        asked += ripe.count_ones() as usize * RUN;
                    }
                    seen(m).store(free & !ripe, Relaxed);
                }
                i += 1;
            }
        }
        if n == 0 {
            continue;
        }
        let mut done = [0u32; 32];
        for (k, &(s, ripe)) in batch[..n].iter().enumerate() {
            let mut rest = ripe;
            while rest != 0 {
                let lo = rest.trailing_zeros();
                let len = (rest >> lo).trailing_ones();
                let mask = (((1u64 << len) - 1) as u32) << lo;
                let (a, bytes) = (superpage::addr_of(s) + lo as usize * RUN, len as usize * RUN);
                // SAFETY: free runs out of the bitmap, handed out by no one; a run stays in use, so never a whole superpage
                if let Some(r) = unsafe { crate::os::age_release(a, bytes) } {
                    done[k] |= mask;
                    if r > 0 {
                        remapped += r;
                        stats::RELEASES.fetch_add(1, Relaxed);
                        stats::RELEASED_BYTES.fetch_add(r as u64, Relaxed);
                    }
                }
                rest &= !mask;
            }
        }
        let _r = runs();
        for (k, &(s, ripe)) in batch[..n].iter().enumerate() {
            let m = meta(s);
            let g = done[k];
            gone(m).fetch_or(g, Relaxed);
            m.zero_runs.fetch_or(g, Relaxed);
            give_free(&m.free_runs, ripe);
            let bytes = g.count_ones() as usize * RUN;
            free_runs_moved(-(g.count_ones() as isize));
            if bytes > 0 {
                stats::claim(-(bytes as isize));
            }
            got += bytes;
            settle(m.class(), s);
        }
    }
    (got, remapped)
}

/// Superpages not wholly free, by class: how many hold `n` used runs, and their free runs' bytes (a racy walk).
#[derive(Clone, Copy, Debug)]
pub struct Occupancy {
    pub used_runs: [[u32; RUNS_PER as usize + 1]; CLASSES],
    pub free_run_bytes: [usize; CLASSES],
    pub spans: u32,
    pub gone_run_bytes: [usize; CLASSES],
}

pub fn occupancy() -> Occupancy {
    let mut o = Occupancy { used_runs: [[0; RUNS_PER as usize + 1]; CLASSES], free_run_bytes: [0; CLASSES], spans: 0, gone_run_bytes: [0; CLASSES] };
    if !crate::on() {
        return o;
    }
    for i in 0..crate::reserve::zones().nsuper {
        if superpage::is_guard(i) {
            continue;
        }
        let m = meta(i);
        match m.state() {
            State::Partial | State::Full => {
                let free = m.free_runs.load(Relaxed).count_ones();
                let c = m.class() as usize;
                o.used_runs[c][(RUNS_PER - free) as usize] += 1;
                o.free_run_bytes[c] += free as usize * RUN;
                o.gone_run_bytes[c] += gone(m).load(Relaxed).count_ones() as usize * RUN;
            }
            State::Span => o.spans += 1,
            _ => {}
        }
    }
    o
}

pub(crate) fn warm_gap() -> isize {
    crate::claimer::target() as isize - (superpage::count(State::WarmZero) * SUPER) as isize
}

pub(crate) fn set_low(on: bool) {
    LOW.store(on, Relaxed);
}

#[inline]
pub(crate) fn low() -> bool {
    LOW.load(Relaxed)
}

/// Every superpage at quiescence is in exactly the bitmap its state names, or in none.
#[cfg_attr(any(target_family = "wasm", miri), allow(dead_code))]
pub(crate) fn check() -> Result<(), &'static str> {
    let z = crate::reserve::zones();
    let mut counts = [0usize; 3];
    for i in 0..z.nsuper {
        if superpage::is_guard(i) {
            continue;
        }
        let st = meta(i).state();
        let bit = |b: usize| {
            // SAFETY: the bitmap words in the meta zone.
            let w = unsafe { &*(z.bits[b] as *const AtomicU64).add(i as usize / 64) };
            w.load(Relaxed) & (1 << (i % 64)) != 0
        };
        let ins = [bit(0), bit(1), bit(2)];
        let want = match st {
            State::Cold => Some(0),
            State::WarmZero => Some(1),
            State::WarmDirty => Some(2),
            State::Busy => return Err("a superpage is still Busy at quiescence"),
            _ => None,
        };
        for (b, &on) in ins.iter().enumerate() {
            if on != (want == Some(b)) {
                return Err("a superpage's state and its free bitmaps disagree");
            }
            counts[b] += on as usize;
        }
    }
    if counts != [superpage::count(State::Cold), superpage::count(State::WarmZero), superpage::count(State::WarmDirty)] {
        return Err("a free-superpage count disagrees with its bitmap");
    }
    Ok(())
}
