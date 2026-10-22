//! The frame zone's face: frames keeps every spill decision; the pool supplies address space and RAM.

use crate::reserve::zones;
use crate::superpage::{self, State, SUPER};
use crate::{os, stats, Tag};
use crate::sync::{AtomicU32, AtomicU8, AtomicUsize, Ordering::*};

const GRAIN: usize = 256 << 10;

pub struct FrameZone {
    lo: AtomicUsize,
    len: AtomicUsize,
}

impl FrameZone {
    pub fn lo(&self) -> usize {
        self.lo.load(Relaxed)
    }
    pub fn len(&self) -> usize {
        self.len.load(Relaxed)
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// How `commit` gets RAM: moved in from warm pool superpages (the default: no fault, THP kept), or faulted fresh.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Move,
    Fault,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Committed {
    pub moved: usize,
    pub faulted: usize,
}

static ZONE: FrameZone = FrameZone { lo: AtomicUsize::new(0), len: AtomicUsize::new(0) };
/// Moved pieces and committed ranges outstanding, and the mappings they may use (half the headroom under
/// vm.max_map_count at init); past it `Move` faults instead.
pub(crate) static MOVED: crate::Line<AtomicUsize> = crate::Line(AtomicUsize::new(0));
static LIVE: crate::Line<AtomicUsize> = crate::Line(AtomicUsize::new(0));
static BUDGET: AtomicUsize = AtomicUsize::new(usize::MAX);

/// pagepool P3 (d): the mappings the pool's span move-ins may have added (`span::fill_cold`: up to two per moved
/// superpage, a split of the pool zone and the moved piece), counted for the process's life (a later release by remap
/// may merge them back; this count never assumes it). Under the same budget as frames' pieces.
static SPAN_MAPS: AtomicUsize = AtomicUsize::new(0);

/// Room for `n` more mappings under the budget, taken (`span::fill_cold`); false past it.
pub(crate) fn take_maps(n: usize) -> bool {
    let budget = BUDGET.load(Relaxed);
    let mut cur = SPAN_MAPS.load(Relaxed);
    loop {
        if MOVED.load(Relaxed) + LIVE.load(Relaxed) + cur + n > budget {
            return false;
        }
        match SPAN_MAPS.compare_exchange_weak(cur, cur + n, AcqRel, Relaxed) {
            Ok(_) => return true,
            Err(now) => cur = now,
        }
    }
}

/// `take_maps`' room given back unused (the move failed or found no warm superpage).
pub(crate) fn give_maps(n: usize) {
    SPAN_MAPS.fetch_sub(n, Relaxed);
}

#[cfg_attr(any(target_family = "wasm", miri), allow(dead_code))]
pub(crate) fn set_budget(n: usize) {
    BUDGET.store(n, Relaxed);
}

pub(crate) fn init() {
    if let Some(h) = os::map_headroom() {
        BUDGET.store(h / 2, Relaxed);
    }
}

fn origin(a: usize) -> &'static AtomicU32 {
    // SAFETY: one u32 per 2 MiB of the frame zone, in the meta zone, open for the process's life.
    unsafe { &*((zones().frame_origin + (a - zones().frame.lo) / SUPER * 4) as *const AtomicU32) }
}

/// A Cold superpage to move a piece back into: the one it came from when that is still Cold, else any.
fn home_for(a: usize) -> Option<u32> {
    let o = origin(a).swap(0, Relaxed);
    if o != 0 {
        MOVED.fetch_sub(1, Relaxed);
        match superpage::take_bit(o - 1) {
            Some(State::Cold) => return Some(o - 1),
            Some(other) => superpage::put(o - 1, other),
            None => {}
        }
    }
    superpage::take_one(State::Cold)
}

/// Forget where the pieces of `[at, at + len)` came from (their pages go to the OS).
fn forget(at: usize, len: usize) {
    let mut a = at.next_multiple_of(SUPER);
    while a + SUPER <= at + len {
        if origin(a).swap(0, Relaxed) != 0 {
            MOVED.fetch_sub(1, Relaxed);
        }
        a += SUPER;
    }
}

/// The frame zone (PROT_NONE until committed); None while the pool is off.
pub fn zone() -> Option<&'static FrameZone> {
    if !crate::on() {
        return None;
    }
    let z = zones();
    if ZONE.len.load(Relaxed) == 0 {
        ZONE.lo.store(z.frame.lo, Relaxed);
        ZONE.len.store(z.frame.len, Relaxed);
    }
    Some(&ZONE)
}

fn tag_at(a: usize) -> &'static AtomicU8 {
    // SAFETY: one byte per 256 KiB of the frame zone, in the meta zone, open for the process's life.
    unsafe { &*((zones().frame_tags + ((a - zones().frame.lo) >> 18)) as *const AtomicU8) }
}

fn set_tags(at: usize, len: usize, tag: Tag) {
    let mut a = at & !(GRAIN - 1);
    while a < at + len {
        tag_at(a).store(tag.0, Relaxed);
        a += GRAIN;
    }
}

fn valid(at: usize, len: usize) -> bool {
    let z = zones();
    let page = os::page_size();
    len > 0 && at % page == 0 && len % page == 0 && z.frame.has(at) && at.checked_add(len).is_some_and(|end| end <= z.frame.lo + z.frame.len)
}

/// Open `[at, at + len)` of the frame zone with RAM: with `Move`, its whole 2 MiB pieces take warm pool superpages
/// (WarmZero first) and the rest faults; with `Fault` all of it faults. None past the cap or outside the zone.
/// # Safety
/// The range is frames' own, PROT_NONE (never committed, or released), and nothing touches it during the call.
pub unsafe fn commit(at: usize, len: usize, huge: bool, mode: Mode, tag: Tag) -> Option<Committed> {
    if !crate::on() || !valid(at, len) {
        return None;
    }
    let mut done = Committed::default();
    let (lo, hi) = (at.next_multiple_of(SUPER), (at + len) & !(SUPER - 1));
    let mut moved_to = lo;
    if mode == Mode::Move && huge && hi > lo {
        while moved_to < hi && MOVED.load(Relaxed) + LIVE.load(Relaxed) + SPAN_MAPS.load(Relaxed) < BUDGET.load(Relaxed) {
            let Some(i) = superpage::take_one(State::WarmZero).or_else(|| superpage::take_one(State::WarmDirty)) else { break };
            // SAFETY: a warm superpage we hold, moved into frames' PROT_NONE range; its pool address stays mapped.
            if !unsafe { os::move_pages(superpage::addr_of(i), moved_to, SUPER, true) } {
                superpage::put_back(i);
                break;
            }
            crate::warmres::drew(superpage::was(i));
            superpage::put(i, State::Cold);
            origin(moved_to).store(i + 1, Relaxed);
            MOVED.fetch_add(1, Relaxed);
            stats::WARM_TAKES.fetch_add(1, Relaxed);
            done.moved += SUPER;
            moved_to += SUPER;
        }
    }
    let fault = [(at, if done.moved > 0 { lo } else { at }), (if done.moved > 0 { moved_to } else { at }, at + len)];
    let need: usize = fault.iter().map(|&(a, b)| b.saturating_sub(a)).sum();
    if need > 0 && !stats::under_cap(need) {
        // SAFETY: what we moved in above, given back to the OS (the caller sees a refusal).
        unsafe { os::guard(lo, done.moved) };
        forget(lo, done.moved);
        stats::claim(-(done.moved as isize));
        return None;
    }
    for &(a, b) in &fault {
        if b <= a {
            continue;
        }
        // SAFETY: the caller's contract: frames' own PROT_NONE range.
        if !unsafe { os::open(a, b - a, huge, c"pgrust:pool") && os::populate(a, b - a) } {
            unsafe { os::guard(at, len) };
            forget(at, len);
            stats::claim(-(done.moved as isize));
            return None;
        }
        done.faulted += b - a;
    }
    stats::claim(done.faulted as isize);
    stats::in_use(tag, len as isize);
    set_tags(at, len, tag);
    LIVE.fetch_add(1, Relaxed);
    Some(done)
}

/// Close `[at, at + len)`: with `keep` its whole 2 MiB pieces move back into Cold pool superpages (warm), the rest goes
/// to the OS; the range is PROT_NONE after. The bytes kept warm.
/// # Safety
/// The range was committed by `commit` (whole), and nothing reads it after.
pub unsafe fn release(at: usize, len: usize, keep: bool) -> usize {
    if !crate::on() || !valid(at, len) {
        return 0;
    }
    let tag = Tag(tag_at(at).load(Relaxed));
    let mut kept = 0;
    if keep {
        let (lo, hi) = (at.next_multiple_of(SUPER), (at + len) & !(SUPER - 1));
        let mut a = lo;
        while a < hi {
            let Some(i) = home_for(a) else { break };
            // SAFETY: a whole committed piece of frames' range moved back to a Cold superpage we hold.
            if !unsafe { os::move_pages(a, superpage::addr_of(i), SUPER, true) } {
                superpage::put(i, State::Cold);
                break;
            }
            superpage::put(i, State::WarmDirty);
            kept += SUPER;
            a += SUPER;
        }
    }
    // SAFETY: the caller's contract; PROT_NONE again, its remaining pages to the OS.
    unsafe { os::guard(at, len) };
    forget(at, len);
    LIVE.fetch_sub(1, Relaxed);
    stats::claim(-((len - kept) as isize));
    stats::in_use(tag, -(len as isize));
    if len > kept {
        stats::RELEASES.fetch_add(1, Relaxed);
        stats::RELEASED_BYTES.fetch_add((len - kept) as u64, Relaxed);
        stats::RELEASED_UNSWEPT.fetch_add((len - kept) as u64, Relaxed);
    }
    kept
}
