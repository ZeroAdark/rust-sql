//! Pool spans (2 MiB up to `lazy_min`): contiguous superpages in one chunk. Lazy spans: their own mapping in the lazy
//! zone, committed by their owner in steps and kept warm, pages and mapping, after `give_span`.

use crate::reserve::zones;
use crate::superpage::{self, meta, spin, unspin, Pref, State, ALL_RUNS, NONE, SUPER};
use crate::{os, state, stats, Class, Hint, Tag, Zero};
use core::ptr::NonNull;
use crate::sync::{AtomicU32, AtomicU64, Ordering::*};

pub struct Span {
    base: NonNull<u8>,
    len: usize,
    committed: usize,
    lazy: bool,
    class: Class,
    tag: Tag,
}

// SAFETY: a Span names memory the pool handed out; it may move between threads like the bytes it names.
unsafe impl Send for Span {}

impl Span {
    pub fn as_ptr(&self) -> *mut u8 {
        self.base.as_ptr()
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn committed(&self) -> usize {
        self.committed
    }
    pub fn is_lazy(&self) -> bool {
        self.lazy
    }
}

impl core::fmt::Debug for Span {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Span").field("base", &self.base).field("len", &self.len).field("committed", &self.committed).field("lazy", &self.lazy).finish()
    }
}

/// Memory of `bytes` or more: a warm span first.
pub fn span(bytes: usize, hint: Hint) -> Option<Span> {
    if !crate::on() || bytes == 0 {
        return None;
    }
    let n = bytes.div_ceil(SUPER);
    if bytes < zones().cfg.lazy_min && n <= superpage::per_chunk() as usize {
        let s = pool_span(n as u32, hint).or_else(|| crate::purger::reclaim(hint.class, || pool_span(n as u32, hint)));
        // pppress card 6: what the span claimed while the ladder pressed goes back now, no lock held
        crate::purger::settle();
        s
    } else {
        lazy_span(bytes, hint)
    }
}

static CURSOR: crate::Line<AtomicU32> = crate::Line(AtomicU32::new(0));

/// Accounting and zeroing for superpages `first..first + n` just taken, held: the cap first. False: all put back.
fn adopt(first: u32, n: u32, cold: u32, zero: Zero, class: Class) -> bool {
    if cold > 0 && !stats::under_cap(cold as usize * SUPER) {
        for i in first..first + n {
            superpage::put_back(i);
        }
        return false;
    }
    for i in first..first + n {
        state::took(superpage::was(i), zero, class, n);
        if zero != Zero::No && meta(i).zero_runs.load(Relaxed) != ALL_RUNS {
            crate::api::zero_fill(superpage::addr_of(i), SUPER);
        }
    }
    true
}

fn mark_span(first: u32, n: u32, from: u32, class: Class, tag: Tag) {
    for i in first + from..first + n {
        meta(i).set(State::Span, class, 0, tag.0, first);
    }
    meta(first).span_n.store(n, Relaxed);
}

/// pagepool P3 (d): a span's run `first..first + n` (held) has `cold` Cold superpages; each takes the pages of a free warm
/// superpage elsewhere (WarmDirty first, then WarmZero) by one remap (`os::move_pages`, the source's address kept mapped
/// and empty: Cold from here), so a span ask reuses scattered warm memory before it claims new: the claimed bytes stay
/// as they were, the warm count falls. Each move may add two mappings (a split of the pool zone and the moved piece),
/// under the frame zone's map budget (`frames::take_maps`); past it, or with no warm superpage left, the rest stays
/// Cold. The Cold superpages left.
fn fill_cold(first: u32, n: u32, mut cold: u32) -> u32 {
    let mut i = first;
    while cold > 0 && i < first + n && superpage::count(State::WarmDirty) + superpage::count(State::WarmZero) > 0 {
        if superpage::was(i) != State::Cold {
            i += 1;
            continue;
        }
        if !crate::frames::take_maps(2) {
            break;
        }
        let Some(j) = superpage::take_one(State::WarmDirty).or_else(|| superpage::take_one(State::WarmZero)) else {
            crate::frames::give_maps(2);
            break;
        };
        let st = superpage::was(j);
        crate::warmres::drew(st);
        // SAFETY: `j` is a free warm superpage we hold and `i` a Cold one of our held run, whole 2 MiB pieces of the pool
        // zone; nothing reads either during the move, and the source stays mapped (empty).
        if !unsafe { os::move_pages(superpage::addr_of(j), superpage::addr_of(i), SUPER, true) } {
            superpage::put_back(j);
            crate::frames::give_maps(2);
            break;
        }
        // `i` holds `j`'s pages now, as the free state `j` had (its zero runs with them); `j`'s address is empty: Cold
        let zero = meta(j).zero_runs.load(Relaxed);
        meta(i).zero_runs.store(zero, Relaxed);
        meta(i).set(State::Busy, Class::Task, 0, st as u8, NONE);
        superpage::put(j, State::Cold);
        stats::MOVED_IN.fetch_add(1, Relaxed);
        cold -= 1;
        i += 1;
    }
    cold
}

fn pool_span(n: u32, hint: Hint) -> Option<Span> {
    let first = if n == 1 {
        let i = state::claim(hint.zero, hint.class, 1)?;
        if hint.zero != Zero::No && meta(i).zero_runs.load(Relaxed) != ALL_RUNS {
            crate::api::zero_fill(superpage::addr_of(i), SUPER);
        }
        i
    } else {
        let chunks = superpage::chunks();
        let start = CURSOR.load(Relaxed) % chunks;
        // pppress card 6: a zeroed span the ladder presses takes free warm superpages before Cold ones (zeroed by adopt)
        let prefs = if hint.zero == Zero::Sparse && !crate::purger::pressed(n as usize * SUPER) { [Pref::Cold, Pref::Any] } else { [Pref::Warm, Pref::Any] };
        let mut got = None;
        'find: for pref in prefs {
            for c in 0..chunks {
                let c = (start + c) % chunks;
                if let Some(t) = superpage::free_contig(c, n, pref) {
                    // pagepool P3 (d): the run's Cold superpages filled from scattered warm ones first (moved in: no
                    // fault, no claim); only what no warm superpage fills is claimed
                    let cold = if t.cold > 0 && hint.zero != Zero::Sparse { fill_cold(t.first, n, t.cold) } else { 0 };
                    if !adopt(t.first, n, cold, hint.zero, hint.class) {
                        return None;
                    }
                    CURSOR.store(c, Relaxed);
                    got = Some(t.first);
                    break 'find;
                }
            }
        }
        got?
    };
    mark_span(first, n, 0, hint.class, hint.tag);
    let len = n as usize * SUPER;
    stats::in_use(hint.tag, len as isize);
    Some(Span { base: NonNull::new(superpage::addr_of(first) as *mut u8)?, len, committed: len, lazy: false, class: hint.class, tag: hint.tag })
}

/// Grow a span to `bytes`: a pool span in place when the next superpages are free; a lazy span in place when the next
/// address space is free, else moved by mremap. False: the caller copies.
/// # Safety
/// `s` is a live span of the pool's, its owner's.
pub unsafe fn grow_span(s: &mut Span, bytes: usize) -> bool {
    if bytes <= s.len {
        return true;
    }
    if s.lazy {
        return !is_base(s) && lazy_grow(s, bytes);
    }
    let first = superpage::index_of(s.base.as_ptr() as usize);
    let n = (s.len / SUPER) as u32;
    let want = bytes.div_ceil(SUPER);
    if want > superpage::per_chunk() as usize {
        return false;
    }
    let want = want as u32;
    let Some(cold) = superpage::take_after(first, n, want - n) else { return false };
    let adopted = adopt(first + n, want - n, cold, Zero::No, s.class);
    // pppress card 6: what the grow claimed while the ladder pressed goes back now
    crate::purger::settle();
    if !adopted {
        return false;
    }
    mark_span(first, want, n, s.class, s.tag);
    stats::in_use(s.tag, ((want - n) as usize * SUPER) as isize);
    s.len = want as usize * SUPER;
    s.committed = s.len;
    true
}

/// Shrink a span to `bytes` (at least one superpage); the tail goes warm.
/// # Safety
/// `s` is a live span of the pool's; nothing past `bytes` of it is used again.
pub unsafe fn shrink_span(s: &mut Span, bytes: usize) {
    let keep = bytes.div_ceil(SUPER).max(1);
    if keep * SUPER >= s.len {
        return;
    }
    if s.lazy && is_base(s) {
        return;
    }
    crate::api::undead(s.base.as_ptr() as usize + keep * SUPER, s.len - keep * SUPER, s.tag);
    if s.lazy {
        lazy_shrink(s, keep);
        return;
    }
    let first = superpage::index_of(s.base.as_ptr() as usize);
    let n = (s.len / SUPER) as u32;
    for i in first + keep as u32..first + n {
        superpage::put(i, State::WarmDirty);
    }
    meta(first).span_n.store(keep as u32, Relaxed);
    stats::in_use(s.tag, -((s.len - keep * SUPER) as isize));
    s.len = keep * SUPER;
    s.committed = s.len;
}

/// A lazy span's owner declares `bytes` of it touched (stats and the pool door; no syscall).
/// # Safety
/// `s` is a live span of the pool's.
pub unsafe fn commit_to(s: &mut Span, bytes: usize) {
    if !s.lazy {
        return;
    }
    let to = bytes.min(s.len).next_multiple_of(os::page_size()).min(s.len);
    if to <= s.committed {
        return;
    }
    let d = (to - s.committed) as isize;
    // pppress card 8: the owner commits (touches) these pages now
    crate::state::note_active(s.base.as_ptr() as usize + s.committed, to - s.committed);
    // pppress card 7 (`PGRUST_POOL_SPAN_MOVE`): the commit's whole untouched superpages taken from the pool's free warm
    // ones by remap, not faulted fresh: their pages were claimed as the pool's, so the claim grows by the rest alone
    let moved = if move_on() { move_in(s, s.committed, to) } else { 0 };
    stats::claim(d - moved as isize);
    // pppress card 6 (`PGRUST_POOL_GROW_GUARD`): the ladder pressing, the pool's own free memory goes back for it now
    crate::purger::settle();
    stats::LAZY_COMMITTED.fetch_add(d, Relaxed);
    stats::in_use(s.tag, d);
    s.committed = to;
    slot(lazy_slot(s.base.as_ptr() as usize)).committed.store(to as u64, Relaxed);
}

/// pppress card 13 (2): the lazy span `s`'s commit past its take's ask is `past` bytes now (`api::handed`; 0 at its give):
/// `stats::COMMIT_PAST_ASK` moves by the change. A pool span has none.
pub(crate) fn set_past(s: &Span, past: usize) {
    if !s.lazy {
        return;
    }
    let old = slot(lazy_slot(s.base.as_ptr() as usize)).past.swap(past as u64, Relaxed);
    stats::COMMIT_PAST_ASK[s.tag.0 as usize].fetch_add(past as isize - old as isize, Relaxed);
}

/// Give a span back: a pool span's superpages go WarmDirty; a lazy span joins the warm list, pages and mapping kept.
/// # Safety
/// `s` is a live span of the pool's; nothing of it is used again.
pub unsafe fn give_span(s: Span) {
    crate::api::undead(s.base.as_ptr() as usize, s.len, s.tag);
    if s.lazy {
        set_past(&s, 0);
        if is_base(&s) { base_give(s) } else { lazy_give(s) }
        return;
    }
    let first = superpage::index_of(s.base.as_ptr() as usize);
    stats::in_use(s.tag, -(s.len as isize));
    for i in first..first + (s.len / SUPER) as u32 {
        superpage::put(i, State::WarmDirty);
    }
}

/// pppress (B): the live span at `a` tagged `tag` from now on (`api::retag`'s span road): its in-use bytes (a lazy
/// span's commit, a pool span's whole length) and a lazy span's commit past its ask move to `tag`; the head's word (a
/// pool span's every superpage's) carries it, so the sweep, a give and a lookup read it. False where the span holds a
/// dead run (its in-use bytes are not its commit), and for no span.
/// # Safety
/// `a` is the start of a live span of the pool's, its caller's alone (no give, resize, commit or give_pages meanwhile).
pub(crate) unsafe fn retag(a: usize, tag: Tag) -> bool {
    // SAFETY: the caller's.
    let Some(s) = (unsafe { span_at(NonNull::new_unchecked(a as *mut u8)) }) else { return false };
    if s.tag == tag {
        return true;
    }
    let held = if s.lazy { s.committed } else { s.len };
    if crate::api::dead_in(a, held) {
        return false;
    }
    if s.lazy {
        let sl = slot(lazy_slot(a));
        let mut w = sl.word.load(Acquire);
        loop {
            if l_state(w) != LIVE {
                return false;
            }
            let n = (w & !(0xff << 5)) | (tag.0 as u64) << 5;
            match sl.word.compare_exchange_weak(w, n, AcqRel, Acquire) {
                Ok(_) => break,
                Err(x) => w = x,
            }
        }
        let past = sl.past.load(Relaxed) as isize;
        stats::COMMIT_PAST_ASK[s.tag.0 as usize].fetch_sub(past, Relaxed);
        stats::COMMIT_PAST_ASK[tag.0 as usize].fetch_add(past, Relaxed);
    } else {
        let first = superpage::index_of(a);
        for i in first..first + (s.len / SUPER) as u32 {
            meta(i).set_tag(tag.0);
        }
    }
    stats::in_use(s.tag, -(held as isize));
    stats::in_use(tag, held as isize);
    true
}

/// The span starting at `p` (a free by pointer).
/// # Safety
/// `p` is the start of a live span of the pool's; the result is its owner's (one Span per span).
pub unsafe fn span_at(p: NonNull<u8>) -> Option<Span> {
    let a = p.as_ptr() as usize;
    if !crate::on() {
        return None;
    }
    let z = zones();
    if z.lazy.has(a) {
        let h = lazy_slot(a);
        let w = slot(h).word.load(Acquire);
        if l_state(w) != LIVE || slot_addr(h) != a {
            return None;
        }
        let len = l_n(w) as usize * SUPER;
        let committed = slot(h).committed.load(Relaxed) as usize;
        return Some(Span { base: p, len, committed, lazy: true, class: l_class(w), tag: l_tag(w) });
    }
    if !crate::reserve::in_pool(a) {
        return None;
    }
    let i = superpage::index_of(a);
    let m = meta(i);
    if m.state() != State::Span || m.head() != i || superpage::addr_of(i) != a {
        return None;
    }
    let len = m.span_n.load(Relaxed) as usize * SUPER;
    Some(Span { base: p, len, committed: len, lazy: false, class: m.class(), tag: Tag(m.tag()) })
}

/// A lazy span: PROT_NONE past `at` (the region road's commit by mprotect). False on a pool span or a refusal.
/// # Safety
/// `s` is a live span of the pool's; nothing past `at` is touched until it is opened again.
pub unsafe fn guard_past(s: &mut Span, at: usize) -> bool {
    if !s.lazy {
        return false;
    }
    let Some(at) = at.checked_next_multiple_of(os::page_size()).filter(|&at| at < s.len) else { return true };
    let base = s.base.as_ptr() as usize;
    let h = lazy_slot(base);
    let ok = unsafe { os::set_prot(base + at, s.len - at, false) };
    if ok {
        slot(h).word.fetch_or(GUARDED, Relaxed);
    }
    ok
}

/// The lazy zone's table, one slot per 2 MiB: word = state:2 | class:3 | tag:8 | guarded:1 | n:32 (a head's length in
/// slots; a tail's distance to its head).
#[repr(C)]
pub(crate) struct LazySlot {
    word: AtomicU64,
    committed: AtomicU64,
    /// pppress card 13 (2): a live span's commit past its take's ask (`set_past`), on its head slot.
    past: AtomicU64,
}

const FREE: u64 = 0;
const LIVE: u64 = 1;
const WARM: u64 = 2;
const TAIL: u64 = 3;
const GUARDED: u64 = 1 << 13;
/// pppress card 8: a live span's piece used since the last age mark (`state::note_active`); set on live spans' slots only.
/// Bit 16: 13-15 are GUARDED / BASE / FENCE and 32-63 the slot count (bit 14 was BASE's: in mode 3 a used span read as a
/// base span, so its give went to `base_give` and no `clean_base` ever reached it, its claim held for good).
const L_ACTIVE: u64 = 1 << 16;
const _: () = assert!(L_ACTIVE & (GUARDED | BASE | FENCE | 3) == 0 && L_ACTIVE < 1 << 32, "L_ACTIVE shares a slot word bit");
const WARM_MAX: usize = 64;
/// pagepool P3 (F1): a FREE slot held PROT_NONE as the fence after the span ending just below it (`fence_on`).
const FENCE: u64 = 1 << 15;
/// `PGRUST_POOL_FENCE=1`, read once (0: unread, 1: off, 2: on).
static FENCE_SWITCH: AtomicU32 = AtomicU32::new(0);

/// pagepool P3 (F1): `PGRUST_POOL_FENCE` (default ON on this branch, user 10-17; `=0` turns it off), read ONCE
/// (`pagepool::init` reads it before the first span): each new lazy
/// span keeps the slot after it PROT_NONE (`FENCE`), so no two spans share a mapping. Without it the lazy zone's spans,
/// opened side by side with one name and the same advice, merge into one VMA (55-61 GB on the lane box), and every give-
/// back or guard there write-locks that VMA: it waits out every page fault in flight in every span while holding
/// mmap_lock for write (lane box, a join suite at W8: purger write holds up to 42.7 ms, query threads waiting behind
/// them).
pub(crate) fn fence_on() -> bool {
    match FENCE_SWITCH.load(Relaxed) {
        0 => {
            let v = if os::env_on_by_default(c"PGRUST_POOL_FENCE") { 2 } else { 1 };
            FENCE_SWITCH.store(v, Relaxed);
            v == 2
        }
        v => v == 2,
    }
}

/// The fence switch as if the environment said `on` (before the pool's first lazy span).
pub(crate) fn force_fence(on: bool) {
    FENCE_SWITCH.store(if on { 2 } else { 1 }, Relaxed);
}

/// D4787: `PGRUST_POOL_SPAN_TRIM=1`, read once (0: unread, 1: off, 2: on).
static TRIM_SWITCH: AtomicU32 = AtomicU32::new(0);

/// D4787: `PGRUST_POOL_SPAN_TRIM` (default ON on this branch, user 10-17; `=0` turns it off), read ONCE
/// (`pagepool::init` reads it before the first span): a warm lazy span taken
/// again keeps only the committed pages its new owner's ask expects (`trim_target`); the pages past them go back to the
/// OS before the owner has the span, and it is zeroed off the lazy lock. Without it a span another user grew is handed
/// over with every page it committed, zeroed under the lock: a 1 GiB graph arena that uses ~260 KB took a region's
/// 1.64-1.68 GB committed span, zeroed it on the session's path and held it for the session's life (D4784 / D4787).
pub(crate) fn trim_on() -> bool {
    match TRIM_SWITCH.load(Relaxed) {
        0 => {
            let v = if os::env_on_by_default(c"PGRUST_POOL_SPAN_TRIM") { 2 } else { 1 };
            TRIM_SWITCH.store(v, Relaxed);
            v == 2
        }
        v => v == 2,
    }
}

/// The trim switch as if the environment said `on` (before the pool's first lazy span).
pub(crate) fn force_trim(on: bool) {
    TRIM_SWITCH.store(if on { 2 } else { 1 }, Relaxed);
}

/// D4787: the committed bytes a reused warm span keeps for an ask of `bytes`: what the ask expects to touch
/// (`Hint::expect_kib`), else its whole length, in whole superpages.
fn trim_target(bytes: usize, hint: Hint) -> usize {
    let want = if hint.expect_kib > 0 { hint.expect_kib as usize * 1024 } else { bytes };
    want.next_multiple_of(SUPER)
}

/// D4787: reused warm lazy spans' counts, either switch: zeroings (count, bytes, ns, the longest ns); trims (count, bytes
/// off the commit as counted, resident bytes given back, ns).
pub(crate) static ZERO_TAKES: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));
pub(crate) static ZERO_BYTES: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));
pub(crate) static ZERO_NS: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));
pub(crate) static ZERO_MAX_NS: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));
pub(crate) static TRIMS: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));
pub(crate) static TRIMMED: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));
pub(crate) static TRIM_RESIDENT: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));
pub(crate) static TRIM_NS: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));

/// pppress card 7: `PGRUST_POOL_SPAN_MOVE=1`, read once (0: unread, 1: off, 2: on).
static MOVE_SWITCH: AtomicU32 = AtomicU32::new(0);

/// pppress card 7: `PGRUST_POOL_SPAN_MOVE=1`, read ONCE (`pagepool::init`): a lazy span's commit (`commit_to`) takes its
/// whole superpages past what the span ever touched (its commit rounded up to a superpage, as a huge fault fills one)
/// from the pool's free warm superpages, by one remap each (`os::move_pages`, 2 MiB aligned both ends: the kernel moves
/// the PMD, no copy, no fault), WarmZero first, a dirty one zeroed first (a lazy span's fresh pages read zero); fresh
/// faults only for what no free superpage covers. Without it a growing lazy span faults fresh pages, THP compaction and
/// direct reclaim included, while free pool memory waits for release. Each move may add two mappings (the frame zone's
/// map budget, `frames::take_maps`); a span with moved pieces is several mappings, which a grow by remap moves whole on
/// kernels that move several (Linux 6.17 and later; before, the grow refuses and the caller copies).
pub(crate) fn move_on() -> bool {
    match MOVE_SWITCH.load(Relaxed) {
        0 => {
            let v = if os::env_is_one(c"PGRUST_POOL_SPAN_MOVE") { 2 } else { 1 };
            MOVE_SWITCH.store(v, Relaxed);
            v == 2
        }
        v => v == 2,
    }
}

pub(crate) fn force_move(on: bool) {
    MOVE_SWITCH.store(if on { 2 } else { 1 }, Relaxed);
}

/// pppress card 7: the commits that moved superpages in (count), the bytes moved, the bytes zeroed first, ns and the
/// longest ns; moves refused (map budget or remap); lazy grows whose remap the kernel refused (either switch).
pub(crate) static MOVES: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));
pub(crate) static MOVED_BYTES: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));
pub(crate) static MOVE_ZEROED: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));
pub(crate) static MOVE_NS: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));
pub(crate) static MOVE_MAX_NS: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));
pub(crate) static MOVE_REFUSED: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));
pub(crate) static GROW_REMAP_REFUSED: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));

/// pppress card 7: the whole superpages of lazy span `s` in `[from, to)` past `from` rounded up (never touched), taken
/// from the pool's free warm superpages (`move_on`). The bytes moved in.
fn move_in(s: &Span, from: usize, to: usize) -> usize {
    let (mut a, end) = (from.next_multiple_of(SUPER), to & !(SUPER - 1));
    if a >= end || superpage::count(State::WarmZero) + superpage::count(State::WarmDirty) == 0 {
        return 0;
    }
    let base = s.base.as_ptr() as usize;
    let t0 = pg_clock::mono_ns();
    let (mut moved, mut zeroed) = (0usize, 0usize);
    while a < end {
        if !crate::frames::take_maps(2) {
            MOVE_REFUSED.fetch_add(1, Relaxed);
            break;
        }
        let Some(j) = superpage::take_one(State::WarmZero).or_else(|| superpage::take_one(State::WarmDirty)) else {
            crate::frames::give_maps(2);
            break;
        };
        let st = superpage::was(j);
        let src = superpage::addr_of(j);
        if meta(j).zero_runs.load(Relaxed) != ALL_RUNS {
            crate::api::zero_fill(src, SUPER);
            zeroed += SUPER;
        }
        // SAFETY: `j` is a free warm superpage we hold (nothing reads it), `[base + a, + SUPER)` a whole superpage of the
        // caller's live lazy span past every page it touched (its commit rounded up); both 2 MiB aligned. The source
        // stays mapped, empty: Cold from here.
        if !unsafe { os::move_pages(src, base + a, SUPER, true) } {
            superpage::put_back(j);
            crate::frames::give_maps(2);
            MOVE_REFUSED.fetch_add(1, Relaxed);
            break;
        }
        crate::warmres::drew(st);
        superpage::put(j, State::Cold);
        stats::WARM_TAKES.fetch_add(1, Relaxed);
        moved += SUPER;
        a += SUPER;
    }
    if moved > 0 {
        let ns = pg_clock::mono_ns().saturating_sub(t0);
        MOVES.fetch_add(1, Relaxed);
        MOVED_BYTES.fetch_add(moved as u64, Relaxed);
        MOVE_ZEROED.fetch_add(zeroed as u64, Relaxed);
        MOVE_NS.fetch_add(ns, Relaxed);
        MOVE_MAX_NS.fetch_max(ns, Relaxed);
    }
    moved
}

/// A reused warm span's committed pages zeroed for its new owner, timed (`ZERO_*`).
fn zero_reused(base: usize, c: usize) {
    let t0 = pg_clock::mono_ns();
    crate::api::zero_fill(base, c);
    let ns = pg_clock::mono_ns().saturating_sub(t0);
    ZERO_TAKES.fetch_add(1, Relaxed);
    ZERO_BYTES.fetch_add(c as u64, Relaxed);
    ZERO_NS.fetch_add(ns, Relaxed);
    ZERO_MAX_NS.fetch_max(ns, Relaxed);
}

/// The slots a new span takes past its own: its fence (1), or none.
#[inline]
fn fence_n() -> u32 {
    fence_on() as u32
}

/// A slot no span holds: FREE, no other span's fence, nothing committed.
#[inline]
/// pppress card 8: the lazy span piece at `a` used now.
pub(crate) fn note_active_piece(a: usize) {
    // the piece's own slot (a tail's too), not its span's head
    let i = ((a - zones().lazy.lo) / SUPER) as u32;
    if i < zones().nlazy {
        let w = &slot(i).word;
        let v = w.load(Relaxed);
        // only a live span's slot: a free or fence slot's word is compared whole elsewhere
        if v & L_ACTIVE == 0 && matches!(l_state(v), LIVE | TAIL) {
            w.fetch_or(L_ACTIVE, Relaxed);
        }
    }
}

/// pppress card 8: whether the lazy span piece at `a` was used since the last age mark.
pub(crate) fn piece_active(a: usize) -> bool {
    let i = ((a - zones().lazy.lo) / SUPER) as u32;
    i < zones().nlazy && slot(i).word.load(Relaxed) & L_ACTIVE != 0
}

/// pppress card 8: every lazy piece's activity bit cleared (the age mark), over the zone's used part.
pub(crate) fn clear_active_pieces() {
    let used = (lazy_used().div_ceil(SUPER) as u32).min(zones().nlazy);
    for i in 0..used {
        let w = &slot(i).word;
        if w.load(Relaxed) & L_ACTIVE != 0 {
            w.fetch_and(!L_ACTIVE, Relaxed);
        }
    }
}

fn free_slot(i: u32) -> bool {
    let w = slot(i).word.load(Relaxed);
    l_state(w) == FREE && w & FENCE == 0 && slot(i).committed.load(Relaxed) == 0
}

/// Slot `i` (FREE and PROT_NONE: never opened, or dropped) becomes the fence of the span ending below it (lock held).
fn set_fence(i: u32) {
    slot(i).word.store(FENCE, Relaxed);
    if i >= BUMP.load(Relaxed) {
        BUMP.store(i + 1, Relaxed);
    }
}

static LAZY_LOCK: crate::Line<AtomicU32> = crate::Line(AtomicU32::new(0));
static BUMP: crate::Line<AtomicU32> = crate::Line(AtomicU32::new(0));
static WARM_LIST: [AtomicU32; WARM_MAX] = [const { AtomicU32::new(NONE) }; WARM_MAX];
/// pagepool P3 (F1): when each `WARM_LIST` entry's span was given (`purger::tick`), for the age release.
static WARM_AT: [AtomicU32; WARM_MAX] = [const { AtomicU32::new(0) }; WARM_MAX];

#[inline]
fn l_state(w: u64) -> u64 {
    w & 3
}
#[inline]
fn l_class(w: u64) -> Class {
    Class::from_u8(((w >> 2) & 7) as u8)
}
#[inline]
fn l_tag(w: u64) -> Tag {
    Tag(((w >> 5) & 0xff) as u8)
}
#[inline]
fn l_n(w: u64) -> u32 {
    (w >> 32) as u32
}
#[inline]
fn l_word(st: u64, class: Class, tag: Tag, n: u32) -> u64 {
    st | (class as u64) << 2 | (tag.0 as u64) << 5 | (n as u64) << 32
}

#[inline]
fn slot(i: u32) -> &'static LazySlot {
    // SAFETY: `nlazy` slots in the meta zone, open for the process's life.
    unsafe { &*(zones().lazy_slots as *const LazySlot).add(i as usize) }
}

#[inline]
fn slot_addr(i: u32) -> usize {
    zones().lazy.lo + i as usize * SUPER
}

#[inline]
fn lazy_slot(a: usize) -> u32 {
    let i = ((a - zones().lazy.lo) / SUPER) as u32;
    let w = slot(i).word.load(Acquire);
    if l_state(w) == TAIL { i - l_n(w) } else { i }
}

pub(crate) fn lazy_info(a: usize) -> Option<crate::RunInfo> {
    let h = lazy_slot(a);
    let w = slot(h).word.load(Acquire);
    let kind = if l_state(w) == LIVE { crate::Kind::Span { superpages: l_n(w) } } else { crate::Kind::Free };
    let n = l_n(w).max(1) as usize;
    Some(crate::RunInfo { kind, base: slot_addr(h), bytes: n * SUPER, class: l_class(w), tag: l_tag(w) })
}

fn mark_lazy(h: u32, n: u32, from: u32, st: u64, class: Class, tag: Tag) {
    for j in from.max(1)..n {
        slot(h + j).word.store(TAIL | (j as u64) << 32, Relaxed);
    }
    slot(h).word.store(l_word(st, class, tag, n), Release);
}

fn unmark_lazy(h: u32, n: u32) {
    for j in 0..n {
        slot(h + j).word.store(FREE, Relaxed);
        slot(h + j).committed.store(0, Relaxed);
    }
}

/// `n` free slots of address space (lock held): the bump, else the first free run below it.
fn find_va(n: u32) -> Option<u32> {
    let total = base_floor();
    let b = BUMP.load(Relaxed);
    if b + n <= total {
        BUMP.store(b + n, Relaxed);
        return Some(b);
    }
    let mut i = 0;
    while i + n <= b {
        let mut j = 0;
        while j < n && free_slot(i + j) {
            j += 1;
        }
        if j == n {
            return Some(i);
        }
        i += j + 1;
    }
    None
}

/// D4885: warm spans whose address space went to a take that found none free (count, bytes of address space).
pub(crate) static WARM_VA_DROPS: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));
pub(crate) static WARM_VA_DROPPED: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));

/// D4885: `n` slots of address space when the bump and the free runs below it have none (lock held): warm spans give
/// theirs up, the least committed first (then the longest), until a run of `n` is free. A warm span keeps its address
/// space until a take of its own size (`n ..= 2n` slots) reuses it, so with every given span still warm (up to
/// `WARM_MAX`) a take of another size found the lazy zone full and went to the system (`os_big`), the zone all free.
fn find_va_dropping_warm(n: u32) -> Option<u32> {
    loop {
        let (k, h) = WARM_LIST
            .iter()
            .enumerate()
            .map(|(k, e)| (k, e.load(Relaxed)))
            .filter(|&(_, h)| h != NONE)
            .min_by_key(|&(_, h)| (slot(h).committed.load(Relaxed), core::cmp::Reverse(l_n(slot(h).word.load(Relaxed)))))?;
        WARM_LIST[k].store(NONE, Relaxed);
        let (c, len) = (slot(h).committed.load(Relaxed) as isize, l_n(slot(h).word.load(Relaxed)));
        drop_va(h, len);
        stats::LAZY_WARM.fetch_sub(c, Relaxed);
        stats::LAZY_COMMITTED.fetch_sub(c, Relaxed);
        stats::claim(-c);
        if c > 0 {
            stats::RELEASES.fetch_add(1, Relaxed);
            stats::RELEASED_BYTES.fetch_add(c as u64, Relaxed);
        }
        WARM_VA_DROPS.fetch_add(1, Relaxed);
        WARM_VA_DROPPED.fetch_add(len as u64 * SUPER as u64, Relaxed);
        if let Some(h) = find_va(n) {
            return Some(h);
        }
    }
}

/// Address space no span uses again: PROT_NONE (its pages go back) and free (lock held).
fn drop_va(h: u32, n: u32) {
    // SAFETY: our own lazy-zone range, no longer named by any span.
    unsafe { os::guard(slot_addr(h), n as usize * SUPER) };
    unmark_lazy(h, n);
    // the fence of a span whose end this was goes with it (`set_fence` again where the span now ends)
    let f = h + n;
    if n > 0 && f < zones().nlazy && slot(f).word.load(Relaxed) == FENCE {
        slot(f).word.store(FREE, Relaxed);
        if f + 1 == BUMP.load(Relaxed) {
            BUMP.store(f, Relaxed);
        }
    }
    if h + n == BUMP.load(Relaxed) {
        BUMP.store(h, Relaxed);
    }
}

fn lazy_span(bytes: usize, hint: Hint) -> Option<Span> {
    let n = bytes.div_ceil(SUPER);
    if n > zones().nlazy as usize {
        return None;
    }
    let n = n as u32;
    let trim = trim_on();
    spin(&LAZY_LOCK);
    let got = lazy_span_locked(n, bytes, hint, trim);
    unspin(&LAZY_LOCK);
    let (mut s, reused) = got?;
    // D4787: a reused span's pages past its ask go back, and what stays is zeroed, here: off the lazy lock, the span the
    // caller's alone (LIVE, on no warm list)
    if reused {
        trim_reused(&mut s, trim_target(bytes, hint));
        if hint.zero != Zero::No && s.committed > 0 {
            zero_reused(s.base.as_ptr() as usize, s.committed);
        }
    }
    Some(s)
}

/// D4787: the committed pages of a reused warm span `s` past `keep` (a superpage multiple) given back before its new
/// owner has it, from the top of its commit down, `purge_batch` bytes a remap (`os::age_release`: a piece with no page
/// resident goes with no call). Address space and fence stay; its commit, the pool's in_use for its tag and the lazy
/// count come down by what went.
fn trim_reused(s: &mut Span, keep: usize) {
    let c = s.committed;
    if keep >= c {
        return;
    }
    let t0 = pg_clock::mono_ns();
    let base = s.base.as_ptr() as usize;
    let batch = zones().cfg.purge_batch.max(SUPER).next_multiple_of(SUPER);
    let mut top = c.next_multiple_of(SUPER).min(s.len);
    let mut resident = 0usize;
    while top > keep {
        let lo = top.saturating_sub(batch).max(keep);
        // SAFETY: whole superpages of a span this thread just took off the warm list: no other owner, nothing reads them
        // (its next owner reads only what is zeroed for it, or fresh pages); `lo` and `top` are superpage multiples
        let Some(r) = (unsafe { os::age_release(base + lo, top - lo) }) else { break };
        resident += r;
        top = lo;
    }
    let left = c.min(top);
    let gone = c - left;
    if gone > 0 {
        slot(lazy_slot(base)).committed.store(left as u64, Relaxed);
        stats::in_use(s.tag, -(gone as isize));
        stats::LAZY_COMMITTED.fetch_sub(gone as isize, Relaxed);
        stats::claim(-(gone as isize));
        s.committed = left;
        TRIMS.fetch_add(1, Relaxed);
        TRIMMED.fetch_add(gone as u64, Relaxed);
    }
    if resident > 0 {
        stats::RELEASES.fetch_add(1, Relaxed);
        stats::RELEASED_BYTES.fetch_add(resident as u64, Relaxed);
        stats::RELEASED_LIVE.fetch_add(resident as u64, Relaxed);
        TRIM_RESIDENT.fetch_add(resident as u64, Relaxed);
    }
    TRIM_NS.fetch_add(pg_clock::mono_ns().saturating_sub(t0), Relaxed);
}

/// A lazy span of `n` slots for `bytes` (lock held), and whether it is a warm one taken again (`trim`: its trim and
/// zeroing are the caller's, off the lock).
fn lazy_span_locked(n: u32, bytes: usize, hint: Hint, trim: bool) -> Option<(Span, bool)> {
    // D4787 (`trim`): the span's warm pages are wanted only up to what the ask expects
    let expect = if trim { trim_target(bytes, hint) } else { (hint.expect_kib as usize * 1024).max(bytes) };
    let mut best: Option<(usize, u32, u32, usize)> = None;
    for (k, e) in WARM_LIST.iter().enumerate() {
        let h = e.load(Relaxed);
        if h == NONE {
            continue;
        }
        let w = slot(h).word.load(Relaxed);
        let (len, c) = (l_n(w), slot(h).committed.load(Relaxed) as usize);
        if len < n || len > 2 * n {
            continue;
        }
        let better = match best {
            None => true,
            // D4787 (`trim`): the expected pages warm first, then the least past them to give back, else the most warm
            // under them, then the shortest
            Some((_, _, blen, bc)) if trim => {
                use core::cmp::Reverse;
                let key = |c: usize, len: u32| (c >= expect, Reverse(c.saturating_sub(expect)), c.min(expect), Reverse(len));
                key(c, len) > key(bc, blen)
            }
            Some((_, _, blen, bc)) => (c >= expect, core::cmp::Reverse(len)) > (bc >= expect, core::cmp::Reverse(blen)),
        };
        if better {
            best = Some((k, h, len, c));
        }
    }
    let class = hint.class;
    if let Some((k, h, len, c)) = best {
        WARM_LIST[k].store(NONE, Relaxed);
        let w = slot(h).word.load(Relaxed);
        let base = slot_addr(h);
        // SAFETY: our own span's range, guarded by its last owner.
        if w & GUARDED != 0 && !unsafe { os::set_prot(base, len as usize * SUPER, true) } {
            drop_va(h, len);
            stats::LAZY_WARM.fetch_sub(c as isize, Relaxed);
            stats::LAZY_COMMITTED.fetch_sub(c as isize, Relaxed);
            stats::claim(-(c as isize));
            return None;
        }
        mark_lazy(h, len, len, LIVE, class, hint.tag);
        stats::LAZY_WARM.fetch_sub(c as isize, Relaxed);
        stats::in_use(hint.tag, c as isize);
        if !trim && hint.zero != Zero::No && c > 0 {
            zero_reused(base, c);
        }
        return Some((Span { base: NonNull::new(base as *mut u8)?, len: len as usize * SUPER, committed: c, lazy: true, class, tag: hint.tag }, trim));
    }
    let f = fence_n();
    let h = find_va(n + f).or_else(|| find_va_dropping_warm(n + f))?;
    let base = slot_addr(h);
    // SAFETY: free lazy-zone address space, PROT_NONE until now (the fence slot past it stays so).
    if !unsafe { os::open(base, n as usize * SUPER, true, c"pgrust:pool") } {
        drop_va(h, n);
        return None;
    }
    mark_lazy(h, n, 1, LIVE, class, hint.tag);
    if f > 0 {
        set_fence(h + n);
    }
    Some((Span { base: NonNull::new(base as *mut u8)?, len: n as usize * SUPER, committed: 0, lazy: true, class, tag: hint.tag }, false))
}

unsafe fn lazy_give(s: Span) {
    let h = lazy_slot(s.base.as_ptr() as usize);
    let c = s.committed as isize;
    stats::in_use(s.tag, -c);
    spin(&LAZY_LOCK);
    let n = l_n(slot(h).word.load(Relaxed));
    let guarded = slot(h).word.load(Relaxed) & GUARDED;
    slot(h).word.store(l_word(WARM, s.class, s.tag, n) | guarded, Release);
    let free = WARM_LIST.iter().position(|e| e.load(Relaxed) == NONE);
    match free {
        Some(k) => {
            WARM_AT[k].store(crate::purger::tick(), Relaxed);
            WARM_LIST[k].store(h, Relaxed);
            stats::LAZY_WARM.fetch_add(c, Relaxed);
        }
        None => {
            drop_va(h, n);
            stats::LAZY_COMMITTED.fetch_sub(c, Relaxed);
            stats::claim(-c);
            stats::RELEASES.fetch_add(1, Relaxed);
            stats::RELEASED_BYTES.fetch_add(c as u64, Relaxed);
            stats::RELEASED_LIVE.fetch_add(c as u64, Relaxed);
        }
    }
    unspin(&LAZY_LOCK);
}

unsafe fn lazy_grow(s: &mut Span, bytes: usize) -> bool {
    let base = s.base.as_ptr() as usize;
    let h = lazy_slot(base);
    let n = (s.len / SUPER) as u32;
    let want = bytes.div_ceil(SUPER);
    if want > zones().nlazy as usize {
        return false;
    }
    let want = want as u32;
    spin(&LAZY_LOCK);
    let ok = lazy_grow_locked(s, h, n, want);
    unspin(&LAZY_LOCK);
    ok
}

fn lazy_grow_locked(s: &mut Span, h: u32, n: u32, want: u32) -> bool {
    let total = base_floor();
    let b = BUMP.load(Relaxed);
    let end = h + n;
    let f = fence_n();
    // our own fence (the slot at `end`) is ours to grow over; the fence then moves to the new end
    let fenced = end < zones().nlazy && slot(end).word.load(Relaxed) == FENCE;
    let free_after =
        end + (want - n) + f <= total && (end..end + (want - n) + f).all(|i| i >= b || free_slot(i) || (i == end && fenced));
    let base = slot_addr(h);
    let w = slot(h).word.load(Relaxed);
    if free_after {
        // SAFETY: free lazy-zone address space after our own span.
        if !unsafe { os::open(slot_addr(end), (want - n) as usize * SUPER, true, c"pgrust:pool") } {
            return false;
        }
        if end + (want - n) > b {
            BUMP.store(end + (want - n), Relaxed);
        }
        mark_lazy(h, want, n, LIVE, s.class, s.tag);
        slot(h).word.fetch_or(w & GUARDED, Relaxed);
        if f > 0 {
            set_fence(h + want);
        }
        s.len = want as usize * SUPER;
        return true;
    }
    let Some(to) = find_va(want + f) else { return false };
    let dst = slot_addr(to);
    // SAFETY: free lazy-zone address space past where the span lands. The tail first: a refusal never moves the span.
    if w & GUARDED != 0 || !unsafe { os::open(dst + n as usize * SUPER, (want - n) as usize * SUPER, true, c"pgrust:pool") } {
        drop_va(to, want);
        return false;
    }
    // SAFETY: our own span moved to free lazy-zone address space; the source stays mapped. One mapping, or several where
    // `move_on` moved superpages in (a kernel that moves only one refuses: counted, the caller copies).
    if !unsafe { os::move_pages(base, dst, n as usize * SUPER, true) } {
        GROW_REMAP_REFUSED.fetch_add(1, Relaxed);
        drop_va(to, want);
        return false;
    }
    let (c, past) = (slot(h).committed.load(Relaxed), slot(h).past.swap(0, Relaxed));
    drop_va(h, n);
    mark_lazy(to, want, 1, LIVE, s.class, s.tag);
    if f > 0 {
        set_fence(to + want);
    }
    slot(to).committed.store(c, Relaxed);
    slot(to).past.store(past, Relaxed);
    s.base = NonNull::new(dst as *mut u8).expect("lazy zone address");
    s.len = want as usize * SUPER;
    true
}

unsafe fn lazy_shrink(s: &mut Span, keep: usize) {
    let base = s.base.as_ptr() as usize;
    let h = lazy_slot(base);
    let n = (s.len / SUPER) as u32;
    let keep_bytes = keep * SUPER;
    let lost = s.committed.saturating_sub(keep_bytes) as isize;
    spin(&LAZY_LOCK);
    let w = slot(h).word.load(Relaxed);
    let fenced = h + n < zones().nlazy && slot(h + n).word.load(Relaxed) == FENCE;
    drop_va(h + keep as u32, n - keep as u32);
    if fenced && (keep as u32) < n {
        set_fence(h + keep as u32);
    }
    slot(h).word.store((w & !(0xffff_ffff << 32)) | (keep as u64) << 32, Release);
    if lost > 0 {
        slot(h).committed.store(keep_bytes as u64, Relaxed);
    }
    unspin(&LAZY_LOCK);
    let past = slot(h).past.load(Relaxed) as usize;
    if lost > 0 && past > 0 {
        set_past(s, past.saturating_sub(lost as usize));
    }
    if lost > 0 {
        stats::in_use(s.tag, -lost);
        stats::LAZY_COMMITTED.fetch_sub(lost, Relaxed);
        stats::claim(-lost);
        s.committed = keep_bytes;
    }
    s.len = keep_bytes;
}

/// pagepool P3 (F1): the lazy zone's bytes ever handed out (its bump): no dead run lies past it.
pub(crate) fn lazy_used() -> usize {
    BUMP.load(Relaxed) as usize * SUPER
}

/// pppress: where in the lazy zone a span can be, as offsets: below the bump, and from the base sub-zone's floor up.
pub(crate) fn lazy_bounds() -> (usize, usize) {
    let len = zones().lazy.len;
    (lazy_used().min(len), (base_floor() as usize * SUPER).min(len))
}

/// pagepool P3 (c): `f(base, committed, warm, tag)` over every live or warm lazy span with committed bytes (`present`'s
/// read; a moment's view: a span may change under it). pppress card V: with each span's state and tag, `warm` a span no
/// one holds (its pages hold no live block). Only the slots a span can be in (`lazy_bounds`): below the bump and from
/// the base sub-zone's floor up. The whole table is the lazy zone's address space a slot per 2 MiB (pool_lazy_va_mul
/// x RAM: 3.1M slots at 16 x 384 GiB), walked at every present sweep (the purger's, spaced to at most 1 / `DUTY` of a
/// core: a longer walk only spaces the sweeps out).
#[cfg_attr(not(all(target_os = "linux", not(miri))), allow(dead_code))]
pub(crate) fn each_committed_by(mut f: impl FnMut(usize, usize, bool, Tag)) {
    let n = zones().nlazy;
    let used = BUMP.load(Relaxed).min(n);
    for h in (0..used).chain(base_floor().clamp(used, n)..n) {
        let w = slot(h).word.load(Acquire);
        let st = l_state(w);
        if matches!(st, LIVE | WARM) {
            let c = slot(h).committed.load(Relaxed) as usize;
            if c > 0 {
                f(slot_addr(h), c.min(l_n(w).max(1) as usize * SUPER), st == WARM, l_tag(w));
            }
        }
    }
}

/// The purger: release warm lazy spans' pages, highest address first, until `want` bytes are met; the spans and their
/// address space stay warm (committed 0). Returns the bytes released.
pub(crate) fn release_warm(want: usize, class: Option<Class>) -> usize {
    let mut got = 0;
    spin(&LAZY_LOCK);
    loop {
        if got >= want {
            break;
        }
        let pick = WARM_LIST
            .iter()
            .map(|e| e.load(Relaxed))
            .filter(|&h| h != NONE && slot(h).committed.load(Relaxed) > 0)
            .filter(|&h| class.is_none_or(|c| l_class(slot(h).word.load(Relaxed)) == c))
            .max();
        let Some(h) = pick else { break };
        let c = slot(h).committed.load(Relaxed) as usize;
        let len = l_n(slot(h).word.load(Relaxed)) as usize * SUPER;
        let guarded = slot(h).word.load(Relaxed) & GUARDED != 0;
        // SAFETY: a warm span's pages: no owner, nothing reads them; a guarded span's tail is PROT_NONE already.
        // Released to the commit's last whole superpage: a huge-page fault past the commit's last page left that tail
        // resident, outside every count), and nothing of a warm span is in use there.
        let rc = unsafe {
            if guarded {
                os::set_prot(slot_addr(h), len, true);
            }
            os::release(slot_addr(h), c.next_multiple_of(SUPER).min(len))
        };
        if rc != 0 {
            break;
        }
        slot(h).committed.store(0, Relaxed);
        stats::LAZY_WARM.fetch_sub(c as isize, Relaxed);
        stats::LAZY_COMMITTED.fetch_sub(c as isize, Relaxed);
        stats::claim(-(c as isize));
        stats::RELEASES.fetch_add(1, Relaxed);
        stats::RELEASED_BYTES.fetch_add(c as u64, Relaxed);
        got += c;
    }
    unspin(&LAZY_LOCK);
    got
}

/// pagepool P3 (F1 / F2): the purger's age and cap release of warm lazy spans, oldest first: each span given `min_age_ms`
/// ago or more comes off the warm list (no taker spins on the lock through its release; `present` still reads it, and the
/// ladder's `releasable` still counts it, until its pages are given back) and has its pages given back from the top of its
/// commit down, `batch` bytes a call (`os::age_release`: a piece with no page resident goes with no remap), its commit
/// lowered after each. It stops at `want` bytes released (as counted: committed), `budget` resident bytes given back, or
/// the pass's time (`purger::pass_late`), and the span goes back on the warm list with what is left of its commit and its
/// age. Returns (bytes released as counted, resident bytes given back).
pub(crate) fn age_warm(min_age_ms: u32, want: usize, budget: usize, batch: usize) -> (usize, usize) {
    let batch = batch.max(SUPER).next_multiple_of(SUPER);
    let (mut got, mut remapped) = (0usize, 0usize);
    let now = crate::purger::tick();
    while got < want && remapped < budget && !crate::purger::pass_late() {
        spin(&LAZY_LOCK);
        let mut pick: Option<(usize, u32)> = None;
        for (k, e) in WARM_LIST.iter().enumerate() {
            let h = e.load(Relaxed);
            if h == NONE || slot(h).committed.load(Relaxed) == 0 {
                continue;
            }
            let age = now.wrapping_sub(WARM_AT[k].load(Relaxed));
            if age >= min_age_ms && pick.is_none_or(|(_, a)| age > a) {
                pick = Some((k, age));
            }
        }
        let Some((k, _)) = pick else {
            unspin(&LAZY_LOCK);
            break;
        };
        let h = WARM_LIST[k].load(Relaxed);
        let at = WARM_AT[k].load(Relaxed);
        WARM_LIST[k].store(NONE, Relaxed);
        unspin(&LAZY_LOCK);
        let w = slot(h).word.load(Relaxed);
        let base = slot_addr(h);
        let len = l_n(w) as usize * SUPER;
        let c = slot(h).committed.load(Relaxed) as usize;
        // SAFETY: a warm span off the warm list: no owner, no taker; a guarded span's tail is PROT_NONE already, and the
        // remaps below map read-write as `release_warm`'s does
        if w & GUARDED != 0 {
            if !unsafe { os::set_prot(base, len, true) } {
                put_warm(h, k, at);
                break;
            }
            // open whole now: no longer guarded (a taker would open it again for nothing)
            slot(h).word.fetch_and(!GUARDED, Relaxed);
        }
        let mut top = c.next_multiple_of(SUPER).min(len);
        let (mut resident, mut failed) = (0usize, false);
        while top > 0 && got + c.saturating_sub(top) < want && remapped < budget && !crate::purger::pass_late() {
            let lo = top.saturating_sub(batch);
            // SAFETY: whole superpages of a warm span nobody owns or reads
            let Some(r) = (unsafe { os::age_release(base + lo, top - lo) }) else {
                failed = true;
                break;
            };
            remapped += r;
            resident += r;
            top = lo;
        }
        let left = c.min(top);
        let gone = c - left;
        if gone > 0 {
            slot(h).committed.store(left as u64, Relaxed);
            stats::LAZY_WARM.fetch_sub(gone as isize, Relaxed);
            stats::LAZY_COMMITTED.fetch_sub(gone as isize, Relaxed);
            stats::claim(-(gone as isize));
            got += gone;
        }
        if resident > 0 {
            stats::RELEASES.fetch_add(1, Relaxed);
            stats::RELEASED_BYTES.fetch_add(resident as u64, Relaxed);
        }
        put_warm(h, k, at);
        if failed {
            break;
        }
    }
    (got, remapped)
}

/// `age_warm`'s span back on the warm list with its age (its old entry if still free); with the list full meanwhile,
/// its address space is dropped, the rest of its commit with it.
fn put_warm(h: u32, k: usize, at: u32) {
    spin(&LAZY_LOCK);
    let free = if WARM_LIST[k].load(Relaxed) == NONE { Some(k) } else { WARM_LIST.iter().position(|e| e.load(Relaxed) == NONE) };
    match free {
        Some(j) => {
            WARM_AT[j].store(at, Relaxed);
            WARM_LIST[j].store(h, Relaxed);
        }
        None => {
            let left = slot(h).committed.load(Relaxed) as isize;
            drop_va(h, l_n(slot(h).word.load(Relaxed)));
            stats::LAZY_WARM.fetch_sub(left, Relaxed);
            stats::LAZY_COMMITTED.fetch_sub(left, Relaxed);
            stats::claim(-left);
            stats::RELEASES.fetch_add(1, Relaxed);
            stats::RELEASED_BYTES.fetch_add(left as u64, Relaxed);
        }
    }
    unspin(&LAZY_LOCK);
}

/// C.23: the base-page sub-zone, the lazy zone's top slots from `BASE_LO` up, opened a chunk at a time with huge pages
/// advised off. Its free slots (word `BASE`) are untouched since their open or last give-back: zero, committed at their
/// first write. A carve is no syscall; a given span is dirty (WARM) until the purger gives it back (`clean_base`).
const BASE: u64 = 1 << 14;
#[cfg_attr(not(all(feature = "global", not(target_family = "wasm"))), allow(dead_code))]
const BASE_CHUNK: u32 = 512;
static BASE_LO: crate::Line<AtomicU32> = crate::Line(AtomicU32::new(NONE));
static BASE_DIRTY: crate::Line<AtomicU32> = crate::Line(AtomicU32::new(0));
static CLEANING: crate::Line<AtomicU32> = crate::Line(AtomicU32::new(0));

/// The lowest slot of the base sub-zone (`nlazy` before its first chunk): the other lazy spans stay below it.
fn base_floor() -> u32 {
    let b = BASE_LO.load(Relaxed);
    if b == NONE { zones().nlazy } else { b }
}

/// D4760: whether `a` (inside the pool's reservation) lies where the pool advises huge pages: the pool zone, and the lazy
/// zone below the base sub-zone (C.23: base pages, huge advised off); the frame zone is not.
pub(crate) fn huge_at(a: usize) -> bool {
    let z = zones();
    if a.wrapping_sub(z.lazy.lo) < z.lazy.len {
        return (((a - z.lazy.lo) / SUPER) as u32) < base_floor();
    }
    a.wrapping_sub(z.pool.lo) < z.pool.len
}

pub(crate) fn base_dirty() -> u32 {
    BASE_DIRTY.load(Relaxed)
}

pub(crate) fn is_base(s: &Span) -> bool {
    slot(lazy_slot(s.base.as_ptr() as usize)).word.load(Relaxed) & BASE != 0
}

/// C.23: `base_pages`'s span: the sub-zone's first `n` free slots in a row, else a chunk opened for it.
#[cfg_attr(not(all(feature = "global", not(target_family = "wasm"))), allow(dead_code))]
pub(crate) fn base_span(bytes: usize, hint: Hint) -> Option<Span> {
    let n = bytes.div_ceil(SUPER);
    if !crate::on() || bytes == 0 || n > zones().nlazy as usize {
        return None;
    }
    let n = n as u32;
    spin(&LAZY_LOCK);
    let mut h = base_fit(n);
    if h.is_none() {
        let r = base_reserve(n);
        unspin(&LAZY_LOCK);
        let (at, lo) = r?;
        // SAFETY: lazy-zone address space past the bump, PROT_NONE and no span's (reserved above).
        let ok = unsafe { os::open(slot_addr(at), (lo - at) as usize * SUPER, false, c"pgrust:pool") };
        spin(&LAZY_LOCK);
        if ok {
            for i in at + n..lo {
                slot(i).word.store(BASE, Relaxed);
            }
            h = Some(at);
        }
    }
    if let Some(h) = h {
        mark_lazy(h, n, 1, LIVE, hint.class, hint.tag);
        slot(h).word.fetch_or(BASE, Relaxed);
    }
    unspin(&LAZY_LOCK);
    Some(Span { base: NonNull::new(slot_addr(h?) as *mut u8)?, len: n as usize * SUPER, committed: 0, lazy: true, class: hint.class, tag: hint.tag })
}

/// The first `n` free sub-zone slots in a row (lock held).
#[cfg_attr(not(all(feature = "global", not(target_family = "wasm"))), allow(dead_code))]
fn base_fit(n: u32) -> Option<u32> {
    let (mut i, end) = (BASE_LO.load(Relaxed), zones().nlazy);
    while i != NONE && i + n <= end {
        let j = (0..n).find(|&j| slot(i + j).word.load(Relaxed) != BASE).unwrap_or(n);
        if j == n {
            return Some(i);
        }
        i += j + 1;
    }
    None
}

/// The sub-zone's next chunk (`n` slots if more) past the bump (lock held): FREE until opened, for good if that fails.
#[cfg_attr(not(all(feature = "global", not(target_family = "wasm"))), allow(dead_code))]
fn base_reserve(n: u32) -> Option<(u32, u32)> {
    let lo = base_floor();
    let at = lo.checked_sub(n.max(BASE_CHUNK)).filter(|&at| at >= BUMP.load(Relaxed))?;
    BASE_LO.store(at, Relaxed);
    Some((at, lo))
}

/// A base span given: dirty until the purger's give-back; the first dirty one wakes the purger.
unsafe fn base_give(s: Span) {
    let h = lazy_slot(s.base.as_ptr() as usize);
    let c = s.committed as isize;
    stats::in_use(s.tag, -c);
    stats::LAZY_WARM.fetch_add(c, Relaxed);
    spin(&LAZY_LOCK);
    let first = BASE_DIRTY.fetch_add(1, Relaxed) == 0;
    let n = l_n(slot(h).word.load(Relaxed));
    slot(h).word.store(l_word(WARM, s.class, s.tag, n) | BASE, Release);
    unspin(&LAZY_LOCK);
    if first {
        crate::purger::wake();
    }
}

/// C.23, the purger: dirty base spans given back whole (`os::release_base`; none resident: `os::clear_nonresident`, D4869)
/// until `budget` resident bytes; the resident bytes given back (the pass budget's cost, a span's length when unknown).
pub(crate) fn clean_base(budget: usize) -> usize {
    if BASE_DIRTY.load(Relaxed) == 0 || CLEANING.swap(1, Acquire) != 0 {
        return 0;
    }
    let (mut got, mut i) = (0, BASE_LO.load(Relaxed));
    // pppress (`PGRUST_POOL_FAST_RELEASE`): an R1 ask waiting for the purger goes first (a base span's give-back took 47 ms)
    while got < budget && i != NONE && i < zones().nlazy && !crate::purger::r1_waiting() {
        let w = slot(i).word.load(Acquire);
        if l_state(w) != WARM || w & BASE == 0 {
            i += 1;
            continue;
        }
        let (n, c) = (l_n(w), slot(i).committed.load(Relaxed) as isize);
        let len = n as usize * SUPER;
        let r = crate::present::residency_of(slot_addr(i), len);
        // D4869: nothing resident is cleared by MADV_DONTNEED (no remap), never trusted to read zero: a swap device enabled
        // after `os::swap_on`'s one read can hold the span's pages
        #[cfg(all(target_os = "linux", not(miri)))]
        let none = r == Some((0, 0));
        #[cfg(not(all(target_os = "linux", not(miri))))]
        let none = { let _ = r; false };
        // SAFETY: a given base span's pages: no owner, and no carve takes a dirty slot.
        #[cfg(all(target_os = "linux", not(miri)))]
        let rc = if none { unsafe { os::clear_nonresident(slot_addr(i), len) } } else { unsafe { os::release_base(slot_addr(i), len) } };
        // SAFETY: as above.
        #[cfg(not(all(target_os = "linux", not(miri))))]
        let rc = { let _ = none; unsafe { os::release_base(slot_addr(i), len) } };
        if rc != 0 {
            break;
        }
        spin(&LAZY_LOCK);
        for j in i..i + n {
            slot(j).word.store(BASE, Relaxed);
            slot(j).committed.store(0, Relaxed);
        }
        BASE_DIRTY.fetch_sub(1, Relaxed);
        unspin(&LAZY_LOCK);
        stats::LAZY_WARM.fetch_sub(c, Relaxed);
        stats::LAZY_COMMITTED.fetch_sub(c, Relaxed);
        stats::claim(-c);
        stats::RELEASES.fetch_add(1, Relaxed);
        stats::RELEASED_BYTES.fetch_add(c as u64, Relaxed);
        got += r.map_or(len, |(resident, _)| resident);
        i += n;
    }
    CLEANING.store(0, Release);
    got
}
