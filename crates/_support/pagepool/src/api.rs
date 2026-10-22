//! The composer: routing by size, zero handling, frees from any thread, dead pages, and the debug arm.

use crate::central::{self, Key};
use crate::reserve::zones;
use crate::run::{self, Kind, LIST_RUNS, MAX_RUNS, RUN};
use crate::superpage::{self, State, SUPER};
use crate::{os, shelf, span, stats, Hint, Tag, Zero};
use core::ptr::NonNull;
use crate::sync::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering::*};

/// Whether pages a dead range held were released meanwhile (and so read zero).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Zeroed {
    No,
    Yes,
}

const POISON: u8 = 0xdb;

#[inline]
fn slice(p: usize, len: usize) -> Option<NonNull<[u8]>> {
    Some(NonNull::slice_from_raw_parts(NonNull::new(p as *mut u8)?, len))
}

/// `bytes` at `align` (a power of two, 2 MiB at most) for `hint`; the slice's length is what may be used.
#[inline]
pub fn take(bytes: usize, align: usize, hint: Hint) -> Option<NonNull<[u8]>> {
    if hint.zero == Zero::No && crate::on() && !debug() && align.is_power_of_two() {
        let size = match run::bucket_of(bytes, align) {
            Some(b) => Some(central::bucket_slot(b)),
            None if bytes <= LIST_RUNS as usize * RUN && align <= RUN => Some(central::runs_slot(bytes.div_ceil(RUN) as u32)),
            None => None,
        };
        if let Some(size) = size {
            let k = Key::new(hint.class, size);
            if let Some(p) = shelf::pop_use(k, hint.tag) {
                if k.is_runs() {
                    run::set_run_tag(p, hint.tag);
                } else {
                    run::tag_slot(p).store(hint.tag.0, Relaxed);
                }
                return slice(p, central::slot_bytes(size));
            }
        }
    }
    take_slow(bytes, align, hint)
}

#[inline(never)]
fn take_slow(bytes: usize, align: usize, hint: Hint) -> Option<NonNull<[u8]>> {
    if !crate::on() || bytes == 0 || !align.is_power_of_two() || align > SUPER {
        return None;
    }
    if let Some(b) = run::bucket_of(bytes, align) {
        return take_block(b, hint);
    }
    let k = bytes.div_ceil(RUN);
    if k <= MAX_RUNS as usize {
        return take_runs(k as u32, (align / RUN).max(1) as u32, hint);
    }
    // pppress (`PGRUST_POOL_SPAN_FIT`): a span whose last superpage the ask fills only in part, on base pages
    let fit = fits(bytes);
    let mut s = if fit { span::base_span(bytes, hint).inspect(|_| fitted(bytes)).or_else(|| span::span(bytes, hint))? } else { span::span(bytes, hint)? };
    // ONE RULE (ppround's FIT_TAIL, pppress card 13 (2)): a lazy span commits, and so counts in use, what `usable`
    // reports for the ask, the size door 1 charges. A fitted base span: its ask to the page with FIT_TAIL. A warm lazy
    // span taken again, which may be up to twice the ask (`span::lazy_span_locked`): the ask's superpages, the slots
    // past them uncommitted and handed out to no one. A pool span is its ask's superpages already. A FIT_TAIL miss is
    // held whole, its excess named below.
    let miss = fit && !(s.is_lazy() && span::is_base(&s)) && fit_tail_on();
    let want = if miss { s.len() } else { usable(bytes, align) };
    let len = handed(&mut s, want);
    if miss {
        // a miss: held whole, handed out (and so charged by door 1) to the page, the rest named
        let want = usable(bytes, align);
        missed(s.as_ptr() as usize, len - want, hint.tag);
        return slice(s.as_ptr() as usize, want);
    }
    slice(s.as_ptr() as usize, len)
}

/// The ONE rule's commit: a lazy span (its owner's alone, fresh from `span::span` / `base_span` or a resize) committed
/// up to `want` (`usable` of its size, capped at the span's length), the bytes it hands out. A commit already past
/// `want` stays (a warm span taken again with `PGRUST_POOL_SPAN_TRIM` off keeps its warm pages counted, as before;
/// D4787's trim is what gives them back). A pool span: all of it.
fn handed(s: &mut span::Span, want: usize) -> usize {
    if !s.is_lazy() {
        return s.len();
    }
    let want = want.min(s.len());
    // SAFETY: the caller's: the span is its own.
    unsafe { span::commit_to(s, want) };
    span::set_past(s, s.committed().saturating_sub(want));
    s.committed()
}

/// A span ask `PGRUST_POOL_SPAN_FIT` serves on base pages: under `pool_lazy_min`, its last superpage filled only in part.
#[inline]
fn fits(bytes: usize) -> bool {
    fit_on() && bytes < zones().cfg.lazy_min && bytes.next_multiple_of(SUPER) - bytes >= FIT_TAIL
}

/// ppround (card 12): `PGRUST_POOL_FIT_TAIL=1`, read once (0: unread, 1: off, 2: on).
static FIT_TAIL_SWITCH: AtomicU8 = AtomicU8::new(0);
/// Fitted asks that got a huge-paged pool span instead (no base address space): charged to the page, held whole.
static FIT_TAIL_MISSES: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));

/// ppround (card 12): `PGRUST_POOL_FIT_TAIL` (default ON on this branch, user 10-17; `=0` turns it off), read ONCE
/// (`pagepool::init`), acting with `PGRUST_POOL_SPAN_FIT` only: a
/// span that SPAN_FIT serves on base pages commits (the pool's in_use and claim) and reports (`usable`, so door 1's
/// `good_size`) its ask rounded to the page, not its whole superpages. The tail past the ask is base pages its owner
/// never touches (a 16-worker scan suite's census: 0.7 % of those tails resident at their free), so it is no memory,
/// and door 1 no longer charges it (~1.05 GB of `allocator_rounding` at the peak, 2-32 MiB asks ~1 MiB over each). The
/// span keeps its whole superpages of address space; a realloc grows it in place up to them (committing the new size),
/// and a shrink under its commit moves (the caller copies), so the commit never stays above what the owner holds.
pub(crate) fn fit_tail_on() -> bool {
    match FIT_TAIL_SWITCH.load(Relaxed) {
        0 => {
            let v = if os::env_on_by_default(c"PGRUST_POOL_FIT_TAIL") { 2 } else { 1 };
            FIT_TAIL_SWITCH.store(v, Relaxed);
            v == 2
        }
        v => v == 2,
    }
}

pub(crate) fn force_fit_tail(on: bool) {
    FIT_TAIL_SWITCH.store(if on { 2 } else { 1 }, Relaxed);
}

/// ppround: (`PGRUST_POOL_FIT_TAIL` on, fitted asks that fell back to a whole pool span, their excess bytes live now, the
/// misses the table had no room for).
pub fn fit_tail() -> (bool, u64, usize, u64) {
    (fit_tail_on(), FIT_TAIL_MISSES.load(Relaxed), MISS_BYTES.load(Relaxed).max(0) as usize, MISS_UNNAMED.load(Relaxed))
}

/// ppround (`PGRUST_POOL_FIT_TAIL`, memacct's condition (a)): a fitted ask that missed the base sub-zone holds a whole
/// span (a huge-paged pool span: no base address space left) while `usable`, so door 1, says the ask to the page. The take
/// hands out that much, and the span's excess (its length less it) moves from the tag's in-use bytes to `Stats::waste`
/// (the reconcile's `pool_run_tails` row), so in-use stays what door 1 charged and no byte lands unattributed. One entry
/// per live missed span: its start (2 MiB aligned) with its tag (bits 9..17) and its excess in pages (bits 0..9: under
/// 512, a 2 MiB superpage's pages) in the low bits; the give puts it back. With the table full the excess stays on in-use
/// (the reconcile's `pool_small_slack`), counted in `MISS_UNNAMED`.
const MISS_SLOTS: usize = 64;
static MISS: [AtomicUsize; MISS_SLOTS] = [const { AtomicUsize::new(0) }; MISS_SLOTS];
static MISS_LIVE: AtomicU32 = AtomicU32::new(0);
static MISS_BYTES: AtomicIsize = AtomicIsize::new(0);
static MISS_UNNAMED: AtomicU64 = AtomicU64::new(0);

fn missed(a: usize, excess: usize, tag: Tag) {
    FIT_TAIL_MISSES.fetch_add(1, Relaxed);
    let page = os::page_size();
    debug_assert!(a % SUPER == 0 && excess % page == 0 && excess / page < 512 && SUPER / page <= 512);
    let w = a | (tag.0 as usize) << 9 | excess / page;
    if !MISS.iter().any(|e| e.compare_exchange(0, w, Relaxed, Relaxed).is_ok()) {
        MISS_UNNAMED.fetch_add(1, Relaxed);
        return;
    }
    MISS_LIVE.fetch_add(1, Relaxed);
    MISS_BYTES.fetch_add(excess as isize, Relaxed);
    stats::in_use(tag, -(excess as isize));
    stats::WASTE.fetch_add(excess as isize, Relaxed);
}

/// ppround (`PGRUST_POOL_FIT_TAIL`) and the ONE rule (pppress card 13 (2)): what the live span starting at `a` (`len`:
/// its superpages) handed out, as `usable` said at its take or last resize: a lazy span (base or not) its commit, a
/// missed span its length less the excess, a pool span its length. `pagepool::usable_size` by pointer reads it, so a
/// copy of that length (C realloc, `cache_realloc`) never touches the uncommitted tail.
#[cfg_attr(not(all(feature = "global", not(target_family = "wasm"))), allow(dead_code))]
pub(crate) fn span_handed(a: usize, len: usize) -> usize {
    if fit_tail_on() {
        if let Some(excess) = miss_of(a, false) {
            return len - excess;
        }
    }
    // SAFETY: the caller's: `a` is a live span's start (`run::lookup` named it a span starting there).
    match NonNull::new(a as *mut u8).and_then(|p| unsafe { span::span_at(p) }) {
        Some(s) if s.is_lazy() => s.committed(),
        _ => len,
    }
}

/// The excess of the live missed span starting at `a`, if it is one: `take` it out of the table (and back onto in-use).
fn miss_of(a: usize, take: bool) -> Option<usize> {
    if MISS_LIVE.load(Relaxed) == 0 {
        return None;
    }
    let e = MISS.iter().find(|e| e.load(Relaxed) & !(SUPER - 1) == a)?;
    let w = e.load(Relaxed);
    let excess = (w & 511) * os::page_size();
    if take {
        e.store(0, Relaxed);
        MISS_LIVE.fetch_sub(1, Relaxed);
        MISS_BYTES.fetch_sub(excess as isize, Relaxed);
        stats::WASTE.fetch_sub(excess as isize, Relaxed);
        stats::in_use(Tag((w >> 9 & 255) as u8), excess as isize);
    }
    Some(excess)
}

/// pppress (charter: cut the rounding): `PGRUST_POOL_SPAN_FIT=1`, read once (0: unread, 1: off, 2: on).
static FIT_SWITCH: AtomicU8 = AtomicU8::new(0);
/// The least unused tail (the span's superpages past the ask) that sends a span to base pages.
const FIT_TAIL: usize = SUPER / 8;
static FITS: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));
static FIT_TAIL_BYTES: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));

/// pppress: `PGRUST_POOL_SPAN_FIT` (default OFF, user 10-18; `=1` turns it on), read ONCE
/// (`pagepool::init`): a take of a span under `pool_lazy_min` whose last
/// superpage the ask fills only in part (at least `FIT_TAIL` unused past it) is served from the base-page sub-zone
/// (C.23: huge pages advised off), so only the pages its owner touches are resident. A pool span is huge-paged: the
/// first touch of its last superpage faults all 2 MiB, and a warm one is resident whole, so the tail past the ask is
/// charged as memory (the door-1 census at the 8 GiB pressure cell's peak: 2-16 MiB asks rounded up by ~470 MB against
/// mimalloc's ~130 MB). The span still reads its whole superpages long (`usable`); a grow by remap is refused (base
/// spans do not move) and the caller copies, as mimalloc does.
/// OFF by default (user 10-18, on the A/B below): a base page faults on its own, 4 KiB at a time. Arm c8g.4xlarge, bin
/// 199da3c54cd, one take each: a wide-table bulk load 73.7-76.4 s on vs 58.4-60.7 s
/// off (mimalloc ~56 s), minor faults 44.7M vs 2.87M;
/// a 10 GB join suite's geomean 62.1 vs 62.1 ms, a scan suite within noise; peak RssAnon 19.52 vs 20.13 GiB, RssAnon
/// after each phase +1.6..+1.8 GiB off. FIT_TAIL acts only with this on, so door 1 charges a span's whole superpages
/// again.
pub(crate) fn fit_on() -> bool {
    match FIT_SWITCH.load(Relaxed) {
        0 => {
            let v = if os::env_is_one(c"PGRUST_POOL_SPAN_FIT") { 2 } else { 1 };
            FIT_SWITCH.store(v, Relaxed);
            v == 2
        }
        v => v == 2,
    }
}

pub(crate) fn force_fit(on: bool) {
    FIT_SWITCH.store(if on { 2 } else { 1 }, Relaxed);
}

fn fitted(bytes: usize) {
    FITS.fetch_add(1, Relaxed);
    FIT_TAIL_BYTES.fetch_add((bytes.next_multiple_of(SUPER) - bytes) as u64, Relaxed);
}

/// pppress: (`PGRUST_POOL_SPAN_FIT` on, spans served on base pages, the unused tails they kept off huge pages).
pub fn span_fits() -> (bool, u64, u64) {
    (fit_on(), FITS.load(Relaxed), FIT_TAIL_BYTES.load(Relaxed))
}

#[cfg(all(feature = "global", not(target_family = "wasm")))]
pub(crate) fn take_base(bytes: usize, align: usize, hint: Hint) -> Option<NonNull<[u8]>> {
    if !align.is_power_of_two() || align > SUPER {
        return None;
    }
    let mut s = span::base_span(bytes, hint)?;
    // the ONE rule, as `take_slow`'s: the commit is what `usable` says
    let len = handed(&mut s, usable(bytes, align));
    slice(s.as_ptr() as usize, len)
}

#[inline]
fn take_block(b: u8, hint: Hint) -> Option<NonNull<[u8]>> {
    let key = Key::new(hint.class, central::bucket_slot(b));
    let s = run::bucket_bytes(b);
    let (p, zero) = match shelf::pop_use(key, hint.tag) {
        Some(p) => (p, false),
        None => refill_block(key, b, s, hint)?,
    };
    run::tag_slot(p).store(hint.tag.0, Relaxed);
    if debug() && !zero {
        check_poison(p, s);
    }
    fill(p, s, hint.zero, zero);
    slice(p, s)
}

/// One block of bucket `b` for `hint` (dirty ok), and whether its bytes read zero (a fresh carve): the small tier's run
/// source, which lays out a zero run's free list a page at a time.
#[cfg(all(feature = "global", not(target_family = "wasm")))]
#[inline]
pub(crate) fn take_block_zero(b: u8, hint: Hint) -> Option<(usize, bool)> {
    if !crate::on() {
        return None;
    }
    let key = Key::new(hint.class, central::bucket_slot(b));
    let (p, zero) = match shelf::pop_use(key, hint.tag) {
        Some(p) => (p, false),
        None => refill_block(key, b, run::bucket_bytes(b), hint)?,
    };
    run::tag_slot(p).store(hint.tag.0, Relaxed);
    Some((p, zero))
}

#[inline(never)]
fn refill_block(key: Key, b: u8, s: usize, hint: Hint) -> Option<(usize, bool)> {
    let batch = match central::pop(key) {
        Some(batch) => batch,
        None => {
            let batch = run::carve(b, hint).or_else(|| crate::purger::reclaim(hint.class, || run::carve(b, hint)));
            // pppress card 6: what the carve claimed while the ladder pressed goes back now, no lock held
            crate::purger::settle();
            let mut batch = batch?;
            if debug() {
                poison_chain(batch.head, batch.n, s);
                // D4868: the carve read zero, the poison does not; a zeroed take must zero it (`fill` trusts this flag)
                batch.zero = false;
            }
            batch
        }
    };
    stats::in_use(hint.tag, s as isize);
    Some((shelf::refill(key, batch), batch.zero))
}

fn take_runs(k: u32, align_runs: u32, hint: Hint) -> Option<NonNull<[u8]>> {
    let bytes = k as usize * RUN;
    if k <= LIST_RUNS && align_runs == 1 {
        let key = Key::new(hint.class, central::runs_slot(k));
        let hit = match shelf::pop_use(key, hint.tag) {
            Some(p) => Some(p),
            None => central::pop(key).map(|batch| {
                stats::in_use(hint.tag, bytes as isize);
                shelf::refill(key, batch)
            }),
        };
        if let Some(p) = hit {
            run::set_run_tag(p, hint.tag);
            fill(p, bytes, hint.zero, false);
            return slice(p, bytes);
        }
    }
    let got = run::take_run(k, align_runs, hint).or_else(|| crate::purger::reclaim(hint.class, || run::take_run(k, align_runs, hint)));
    // pppress card 6: as refill_block's
    crate::purger::settle();
    let (p, zero) = got?;
    stats::in_use(hint.tag, bytes as isize);
    fill(p, bytes, hint.zero, zero);
    slice(p, bytes)
}

pub(crate) static ZERO_FILLS: crate::Line<AtomicU64> = crate::Line(AtomicU64::new(0));

#[inline]
pub(crate) fn zero_fill(p: usize, len: usize) {
    ZERO_FILLS.fetch_add(1, Relaxed);
    // SAFETY: the caller's: memory it holds, open read-write.
    unsafe { core::ptr::write_bytes(p as *mut u8, 0, len) };
}

/// Zero what the ask needs: nothing for `No`; the link bytes of a known-zero block; else all of it.
#[inline]
fn fill(p: usize, len: usize, want: Zero, zero: bool) {
    if want == Zero::No {
        return;
    }
    if zero {
        // SAFETY: the block we hand out, open read-write.
        unsafe { core::ptr::write_bytes(p as *mut u8, 0, core::mem::size_of::<central::Link>().min(len)) };
    } else {
        zero_fill(p, len);
    }
}

/// Give back what `take` handed out (any thread).
/// # Safety
/// `p` came from `take` (or `resize`), is not used again, and `bytes` is at most its usable size.
pub unsafe fn give(p: NonNull<u8>, bytes: usize) {
    let _ = bytes;
    unsafe { give_unsized(p) };
}

/// An emptied small-tier run (a run-bucket block) straight back to its superpage, for any class's next take.
#[cfg_attr(not(feature = "global"), allow(dead_code))]
pub(crate) fn give_run_home(a: usize, hint: Hint) {
    let b = run::bucket_of(RUN, RUN).expect("the run bucket");
    if debug() || DEAD_RUNS.load(Relaxed) > 0 {
        // SAFETY: the caller's: a live block of the run bucket taken with `hint`.
        unsafe { give_hinted(NonNull::new_unchecked(a as *mut u8), RUN, RUN, hint) };
        return;
    }
    stats::in_use(hint.tag, -(RUN as isize));
    central::set_link_next(a, central::NONE);
    central::home(Key::new(hint.class, central::bucket_slot(b)), central::Batch { head: central::idx_of(a), n: 1, bytes: RUN as u32, zero: false });
}

/// Give back what `take(bytes, align, hint)` handed out, with the take's own three: a block or a short run goes onto
/// this thread's shelf without the run lookup `give` does.
/// # Safety
/// As `give`, and `bytes`, `align` and `hint`'s class and tag are the take's.
#[inline]
pub unsafe fn give_hinted(p: NonNull<u8>, bytes: usize, align: usize, hint: Hint) {
    let a = p.as_ptr() as usize;
    if !debug() && DEAD_RUNS.load(Relaxed) <= 0 {
        let size = match run::bucket_of(bytes, align) {
            Some(b) => Some(central::bucket_slot(b)),
            None => {
                let k = bytes.div_ceil(RUN);
                (k <= LIST_RUNS as usize).then(|| central::runs_slot(k as u32))
            }
        };
        if let Some(size) = size {
            #[cfg(debug_assertions)]
            check_hint(a, size, hint);
            shelf::push_free(Key::new(hint.class, size), hint.tag, a);
            return;
        }
    }
    // SAFETY: the caller's contract.
    unsafe { give_unsized(p) };
}

#[cfg(debug_assertions)]
fn check_hint(a: usize, size: u8, hint: Hint) {
    let i = run::lookup(a as *const u8).expect("pagepool: give_hinted of an address the pool does not own");
    let ok = match i.kind {
        Kind::Blocks { bucket } => central::bucket_slot(bucket) == size,
        Kind::Run { runs } => runs as u32 <= LIST_RUNS && central::runs_slot(runs as u32) == size,
        _ => false,
    };
    assert!(ok && i.base == a && i.class == hint.class && i.tag == hint.tag, "pagepool: give_hinted with another take's size or hint: {i:?} {hint:?}");
}

/// Give back a block whose size is not known (C free); its usable size.
/// # Safety
/// As `give`.
pub unsafe fn give_unsized(p: NonNull<u8>) -> usize {
    let a = p.as_ptr() as usize;
    if debug() {
        if let Some(s) = quarantine(a) {
            return s;
        }
    }
    unsafe { give_now(a) }
}

unsafe fn give_now(a: usize) -> usize {
    let z = zones();
    if z.lazy.has(a) {
        // SAFETY: the caller's contract: `a` is a live span's start.
        let s = unsafe { span::span_at(NonNull::new_unchecked(a as *mut u8)) }.expect("pagepool: give of an address that is no live span");
        let len = s.len();
        // ppround (`PGRUST_POOL_FIT_TAIL`): a missed span's excess back onto in-use, which the give takes off whole
        let _ = miss_of(a, true);
        unsafe { span::give_span(s) };
        return len;
    }
    let m = superpage::meta(superpage::index_of(a));
    if m.state() == State::Span {
        // SAFETY: as above.
        let s = unsafe { span::span_at(NonNull::new_unchecked(a as *mut u8)) }.expect("pagepool: give of a span's interior");
        let len = s.len();
        let _ = miss_of(a, true);
        unsafe { span::give_span(s) };
        return len;
    }
    let info = run::lookup(a as *const u8).expect("pagepool: give of an address the pool does not own");
    match info.kind {
        Kind::Blocks { bucket } => {
            let s = run::bucket_bytes(bucket);
            if s >= RUN {
                undead(a, s, info.tag);
            }
            if debug() {
                poison(a, s);
            }
            shelf::push_free(Key::new(info.class, central::bucket_slot(bucket)), info.tag, a);
            s
        }
        Kind::Run { runs } => {
            let bytes = runs as usize * RUN;
            undead(a, bytes, info.tag);
            if runs as u32 <= LIST_RUNS {
                shelf::push_free(Key::new(info.class, central::runs_slot(runs as u32)), info.tag, a);
            } else {
                stats::in_use(info.tag, -(bytes as isize));
                run::free_run(a, runs as u32, info.class);
            }
            bytes
        }
        _ => panic!("pagepool: give of a free or dead address {a:#x}"),
    }
}

/// A context reset's blocks: shelf pushes, no atomic read-modify-write.
/// # Safety
/// Each block as `give`'s.
pub unsafe fn give_many<I: IntoIterator<Item = (NonNull<u8>, usize)>>(blocks: I) {
    for (p, n) in blocks {
        unsafe { give(p, n) };
    }
}

/// Resize: the same bucket, a run's runs, a span (a lazy span may move: use the result, never `p`). None: the caller copies.
/// # Safety
/// `p` came from `take` with `old` bytes or fewer and is its caller's; on Some the result replaces it.
pub unsafe fn resize(p: NonNull<u8>, old: usize, new: usize, align: usize) -> Option<NonNull<[u8]>> {
    let _ = old;
    let a = p.as_ptr() as usize;
    if new == 0 || !align.is_power_of_two() {
        return None;
    }
    let info = run::lookup(a as *const u8)?;
    match info.kind {
        Kind::Blocks { bucket } => {
            let s = run::bucket_bytes(bucket);
            (new <= s && a % align == 0).then(|| slice(a, s)).flatten()
        }
        Kind::Run { runs } => {
            let nk = new.div_ceil(RUN);
            if a % align != 0 || nk > MAX_RUNS as usize {
                return None;
            }
            let (k, nk) = (runs as u32, nk as u32);
            if nk < k {
                undead(a + nk as usize * RUN, (k - nk) as usize * RUN, info.tag);
                run::shrink_run(a, k, nk);
                stats::in_use(info.tag, -(((k - nk) as usize * RUN) as isize));
            } else if nk > k {
                if !run::grow_run(a, k, nk - k) {
                    return None;
                }
                stats::in_use(info.tag, ((nk - k) as usize * RUN) as isize);
            }
            slice(a, nk as usize * RUN)
        }
        Kind::Span { .. } => {
            // a size `take` serves as a bucket's block or runs (`take_slow`'s roads) never stays a span: the caller
            // copies. A span held at such a size goes back through `give_hinted`'s size-keyed shelf as a bucket's or a
            // run's block (mcx's pooled_bit with SPAN_FIT off: a 40 MiB lazy span shrunk in place to 32 KiB, then given
            // as 32 KiB; FIT_TAIL's `fits` refusal below had covered it with SPAN_FIT on)
            if run::bucket_of(new, align).is_some() || new.div_ceil(RUN) <= MAX_RUNS as usize {
                return None;
            }
            // SAFETY: the caller's contract: `a` is a live span's start.
            let mut s = unsafe { span::span_at(p) }?;
            // ppround (`PGRUST_POOL_FIT_TAIL`): a missed span, or any other non-base span to a size `usable` reports to
            // the page, moves (the caller copies): what it would hand out in place is no longer `usable(new)`
            if fit_tail_on() && !(s.is_lazy() && span::is_base(&s)) && (fits(new) || miss_of(a, false).is_some()) {
                return None;
            }
            if fit_tail_on() && s.is_lazy() && span::is_base(&s) {
                // ppround (`PGRUST_POOL_FIT_TAIL`): in place up to its superpages, committed to what `usable` says; a
                // shrink under the commit moves (its pages past the new size may be resident)
                let want = usable(new, align);
                if want > s.len() || want < s.committed() || a % align != 0 {
                    return None;
                }
                let len = handed(&mut s, want);
                return slice(a, len);
            }
            if s.is_lazy() && !span::is_base(&s) {
                // the ONE rule (pppress card 13 (2)): in place, a lazy span holds what `usable` reports for its new
                // size: grown to it (maybe moved), or its superpages past it dropped, then committed up to it
                let want = usable(new, align);
                // SAFETY: the caller's span, its owner's.
                let ok = unsafe { if want > s.len() { span::grow_span(&mut s, want) } else { span::shrink_span(&mut s, want); true } };
                if !ok {
                    return None;
                }
                let len = handed(&mut s, want);
                return slice(s.as_ptr() as usize, len);
            }
            if new > s.len() {
                if !unsafe { span::grow_span(&mut s, new) } {
                    return None;
                }
                let len = s.len();
                unsafe { span::commit_to(&mut s, len) };
            } else {
                unsafe { span::shrink_span(&mut s, new) };
            }
            slice(s.as_ptr() as usize, s.len())
        }
        _ => None,
    }
}

const LIVE: u8 = 0;
const DEAD: u8 = 1;
const RELEASING: u8 = 2;
const RELEASED: u8 = 3;
/// pagepool P3 (F1): a dead run an age mark has seen dead, and one two marks have (RIPE: dead a whole
/// `pool_lazy_decay_ms` or more, never taken live since; the age pass gives it back). DEAD to everything else.
const SEEN: u8 = 4;
const RIPE: u8 = 5;
/// pagepool P3 (F1): a mark left RIPE runs the age pass has not given back yet, and where a budget stopped the last Ripe
/// pass (the next resumes there; the purger's alone).
static RIPE_LEFT: AtomicBool = AtomicBool::new(false);
static RIPE_FROM: AtomicUsize = AtomicUsize::new(0);

static DEAD_RUNS: crate::Line<AtomicIsize> = crate::Line(AtomicIsize::new(0));

#[inline]
fn dead_slot(a: usize) -> &'static AtomicU8 {
    // SAFETY: one byte per 64 KiB of the pool and lazy zones (contiguous), in the meta zone, open for the process's life.
    unsafe { &*((zones().dead + (a - zones().pool.lo) / RUN) as *const AtomicU8) }
}

/// pppress card V: whether any run is dead now (the present sweep skips its per-run read of live spans otherwise).
#[cfg_attr(not(all(target_os = "linux", not(miri))), allow(dead_code))]
#[inline]
pub(crate) fn any_dead() -> bool {
    DEAD_RUNS.load(Relaxed) > 0
}

#[inline]
pub(crate) fn dead_at(a: usize) -> bool {
    DEAD_RUNS.load(Relaxed) > 0 && dead_slot(a).load(Relaxed) != LIVE
}

/// pppress (B): whether a whole run of `[a, a + len)` is dead now.
pub(crate) fn dead_in(a: usize, len: usize) -> bool {
    if !any_dead() {
        return false;
    }
    let (lo, hi) = whole_runs(a, len);
    // bounded("other: one read per whole run of one live block, a retag's (once per resident fp table)")
    (lo..hi).step_by(RUN).any(dead_at)
}

/// pppress (B): the live block `take` handed out at `p` tagged `tag` from now on: its in-use bytes move from its tag
/// to `tag`, and the present sweep, a give and a lookup read `tag`. For a runs block or a span (a lazy span's commit
/// past its ask moves too). False, nothing changed: a small bucket block (the sweep tags its run as a whole), a span a
/// FIT_TAIL miss holds (its excess is named under the take's tag), a block holding a dead run, or not the pool's.
/// A span's holder gives it back by pointer (`give`) after a retag: a `Span` value names its take's tag.
/// # Safety
/// `p` is the start of a live block `take` handed out, its caller's alone: no give, resize, commit or `give_pages` of
/// it meanwhile.
pub unsafe fn retag(p: NonNull<u8>, tag: Tag) -> bool {
    let a = p.as_ptr() as usize;
    if !crate::on() || miss_of(a, false).is_some() {
        return false;
    }
    if zones().lazy.has(a) || (crate::reserve::in_pool(a) && superpage::meta(superpage::index_of(a)).state() == State::Span) {
        // SAFETY: the caller's.
        return unsafe { span::retag(a, tag) };
    }
    let Some(info) = run::lookup(a as *const u8) else { return false };
    match info.kind {
        Kind::Run { runs } if info.base == a => {
            let bytes = runs as usize * RUN;
            if dead_in(a, bytes) {
                return false;
            }
            if info.tag != tag {
                run::set_run_tag(a, tag);
                stats::in_use(info.tag, -(bytes as isize));
                stats::in_use(tag, bytes as isize);
            }
            true
        }
        _ => false,
    }
}

#[inline]
fn whole_runs(at: usize, len: usize) -> (usize, usize) {
    let lo = at.next_multiple_of(RUN);
    let hi = (at + len) & !(RUN - 1);
    (lo, hi.max(lo))
}

/// `[block + at, + len)` is dead, the block kept: its whole 64 KiB runs may be released by the purger. The bytes it
/// made dead: its whole runs that were live, what left the block's tag's in-use bytes (pppress card 13 (3): an owner's
/// charge follows the pool's count by them, not by the range).
/// # Safety
/// The range lies in a live block or span of the caller's, which does not read it until `take_pages`.
pub unsafe fn give_pages(block: NonNull<u8>, at: usize, len: usize) -> usize {
    let b = block.as_ptr() as usize;
    let tag = run::lookup(b as *const u8).map_or(Tag::OTHER, |i| i.tag);
    let (lo, hi) = whole_runs(b + at, len);
    let mut n = 0isize;
    let mut r = lo;
    while r < hi {
        if dead_slot(r).compare_exchange(LIVE, DEAD, AcqRel, Relaxed).is_ok() {
            n += 1;
        }
        r += RUN;
    }
    DEAD_RUNS.fetch_add(n, Relaxed);
    stats::DEAD.fetch_add(n * RUN as isize, Relaxed);
    stats::in_use(tag, -(n * RUN as isize));
    // pppress card 8: the owner is using this memory now
    crate::state::note_active(lo, hi - lo);
    n as usize * RUN
}

/// The owner writes `[block + at, + len)` again; `Yes` when its pages were released meanwhile (they read zero).
/// # Safety
/// As `give_pages`, for a range given there.
pub unsafe fn take_pages(block: NonNull<u8>, at: usize, len: usize) -> Zeroed {
    // SAFETY: the caller's contract.
    unsafe { take_pages_counted(block, at, len) }.0
}

/// `take_pages`, with the bytes it made live again: its dead or released runs, what came back onto the block's tag's
/// in-use bytes (pppress card 13 (3): an owner's charge follows the pool's count by them, as `give_pages` returns its).
/// # Safety
/// As `take_pages`.
pub unsafe fn take_pages_counted(block: NonNull<u8>, at: usize, len: usize) -> (Zeroed, usize) {
    let b = block.as_ptr() as usize;
    let tag = run::lookup(b as *const u8).map_or(Tag::OTHER, |i| i.tag);
    let (lo, hi) = whole_runs(b + at, len);
    let (zeroed, back) = revive(lo, hi, tag);
    // pppress card 8: the owner is using this memory now
    crate::state::note_active(lo, hi.saturating_sub(lo));
    // pppress card 6: released runs live again claim; while the ladder presses, the pool's free memory goes back for them
    crate::purger::settle();
    (if zeroed { Zeroed::Yes } else { Zeroed::No }, back)
}

/// Every dead run in `[lo, hi)` live again: whether any had been released, and the bytes made live (onto in-use).
fn revive(lo: usize, hi: usize, tag: Tag) -> (bool, usize) {
    let (mut dead, mut released) = (0isize, 0isize);
    let mut r = lo;
    while r < hi {
        let d = dead_slot(r);
        loop {
            match d.load(Acquire) {
                DEAD if d.compare_exchange(DEAD, LIVE, AcqRel, Relaxed).is_ok() => dead += 1,
                RELEASED if d.compare_exchange(RELEASED, LIVE, AcqRel, Relaxed).is_ok() => released += 1,
                SEEN if d.compare_exchange(SEEN, LIVE, AcqRel, Relaxed).is_ok() => dead += 1,
                RIPE if d.compare_exchange(RIPE, LIVE, AcqRel, Relaxed).is_ok() => dead += 1,
                RELEASING => {
                    crate::sync::spin_loop();
                    continue;
                }
                LIVE => {}
                _ => continue,
            }
            break;
        }
        r += RUN;
    }
    let run = RUN as isize;
    if dead + released > 0 {
        DEAD_RUNS.fetch_sub(dead + released, Relaxed);
        stats::DEAD.fetch_sub(dead * run, Relaxed);
        if released > 0 {
            stats::claim(released * run);
        }
        stats::in_use(tag, (dead + released) * run);
    }
    (released > 0, (dead + released).max(0) as usize * RUN)
}

/// A block or span leaving its owner: its dead runs count live again first.
#[inline]
pub(crate) fn undead(a: usize, len: usize, tag: Tag) {
    if DEAD_RUNS.load(Relaxed) > 0 {
        let (lo, hi) = whole_runs(a, len);
        revive(lo, hi, tag);
    }
}

/// The purger: release dead runs, contiguous ones together, until `want` bytes are met. The bytes released. `fast`
/// (`PGRUST_POOL_FAST_RELEASE`): no scan with no dead run left to give back (`stats::DEAD`: dead, seen and ripe runs, not
/// the released ones `DEAD_RUNS` still counts), and the lazy zone scanned only where a span can be: below its bump and
/// the base sub-zone (`span::lazy_bounds`), not its whole reservation.
pub(crate) fn release_dead(want: usize, fast: bool) -> usize {
    if DEAD_RUNS.load(Relaxed) <= 0 || (fast && stats::DEAD.load(Relaxed) <= 0) {
        return 0;
    }
    let z = zones();
    let total = (z.pool.len + z.lazy.len) / RUN;
    let (used, base) = span::lazy_bounds();
    let skip = if fast { ((z.pool.len + used) / RUN, (z.pool.len + base) / RUN) } else { (total, total) };
    let max_batch = z.cfg.purge_batch / RUN;
    let mut got = 0usize;
    let mut i = 0usize;
    let keep = crate::thp_partial() == crate::THP_KEEP;
    while i < total && got < want {
        if i >= skip.0 && i < skip.1 {
            i = skip.1;
            continue;
        }
        let a = z.pool.lo + i * RUN;
        if !matches!(dead_slot(a).load(Relaxed), DEAD | SEEN | RIPE) {
            i += 1;
            continue;
        }
        let j = if keep && span::huge_at(a) {
            // D4760 (`pool_thp_partial` 1): whole dead superpages only; the rest of the dead runs stay held
            let j = if a % SUPER == 0 { grab_whole(z.pool.lo, i, total, max_batch, true) } else { i };
            if j == i {
                stats::THP_KEPT.fetch_add(1, Relaxed);
                stats::THP_KEPT_BYTES.fetch_add(RUN as u64, Relaxed);
                i += 1;
                continue;
            }
            j
        } else {
            let mut j = i;
            while j < total && j - i < max_batch && grab(dead_slot(z.pool.lo + j * RUN), true) {
                j += 1;
            }
            if j == i {
                i += 1;
                continue;
            }
            j
        };
        // SAFETY: dead runs of live blocks: their owners do not read them until take_pages, which waits out RELEASING.
        let rc = unsafe { os::release(a, (j - i) * RUN) };
        let to = if rc == 0 { RELEASED } else { DEAD };
        for k in i..j {
            dead_slot(z.pool.lo + k * RUN).store(to, Release);
        }
        if rc == 0 {
            let bytes = (j - i) * RUN;
            stats::DEAD.fetch_sub(bytes as isize, Relaxed);
            stats::claim(-(bytes as isize));
            stats::RELEASES.fetch_add(1, Relaxed);
            stats::RELEASED_BYTES.fetch_add(bytes as u64, Relaxed);
            got += bytes;
        }
        i = j;
    }
    got
}

/// D4760 (`pool_thp_partial` 1): from slot `i` (a superpage's first run, `base` the slots' address 0), whole superpages
/// of dead runs to RELEASING together (each run RIPE, or with `any` any dead one), up to `hi` and at most `max` runs (one
/// superpage at least): the end slot, `i` when the superpage at `i` is not wholly dead. A superpage grabbed in part is
/// put back as it was, so no give-back covers part of a superpage still in use.
fn grab_whole(base: usize, i: usize, hi: usize, max: usize, any: bool) -> usize {
    const PER: usize = SUPER / RUN;
    let mut j = i;
    while j + PER <= hi && j + PER - i <= max.max(PER) {
        let mut was = [DEAD; PER];
        let mut k = 0;
        while k < PER {
            let d = dead_slot(base + (j + k) * RUN);
            let st = d.load(Relaxed);
            if !(st == RIPE || (any && matches!(st, DEAD | SEEN))) || d.compare_exchange(st, RELEASING, AcqRel, Relaxed).is_err() {
                break;
            }
            was[k] = st;
            k += 1;
        }
        if k < PER {
            for (x, &st) in was[..k].iter().enumerate() {
                dead_slot(base + (j + x) * RUN).store(st, Release);
            }
            break;
        }
        j += PER;
    }
    j
}

/// A dead run to RELEASING: a RIPE one, or (`any`) any dead one.
fn grab(d: &AtomicU8, any: bool) -> bool {
    d.compare_exchange(RIPE, RELEASING, AcqRel, Relaxed).is_ok()
        || (any && (d.compare_exchange(SEEN, RELEASING, AcqRel, Relaxed).is_ok() || d.compare_exchange(DEAD, RELEASING, AcqRel, Relaxed).is_ok()))
}

/// pagepool P3 (F1 / F2): what one `dead_pass` does.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeadPass {
    /// The age mark, every `pool_lazy_decay_ms`: a SEEN run becomes RIPE, a DEAD one SEEN. Nothing released.
    Mark,
    /// ppround card 14 v3 (`PGRUST_POOL_PEAK_RELEASE` lever DEAD1): a climbing walk's mark, one mark of age: a DEAD or SEEN
    /// run becomes RIPE at once (the road without the pool gives a region's freed pages back at once). Nothing released.
    Mark1,
    /// The age release: RIPE runs go back.
    Ripe,
    /// The cap: any dead run goes back.
    Any,
}

/// pagepool P3 (F1 / F2): the purger's pass over dead runs, in address order over the pool zone and the lazy zone's used
/// part. Contiguous runs go back together, `batch` bytes a call at most (`os::age_release`: a range with no page resident
/// goes back with no remap). Releasing stops at `want` bytes released (as counted), `budget` resident bytes given back,
/// or the pass's time (`purger::pass_late`). Returns (bytes released as counted, resident bytes given back).
pub(crate) fn dead_pass(pass: DeadPass, want: usize, budget: usize, batch: usize) -> (usize, usize) {
    if DEAD_RUNS.load(Relaxed) <= 0 || (pass == DeadPass::Ripe && !RIPE_LEFT.load(Relaxed)) {
        return (0, 0);
    }
    let z = zones();
    let total = (z.pool.len + span::lazy_used().min(z.lazy.len)) / RUN;
    let max_batch = (batch / RUN).max(1);
    let (mut got, mut remapped) = (0usize, 0usize);
    let mut ripe = false;
    // a Ripe pass resumes where a budget stopped the last one, round to it
    let from = if pass == DeadPass::Ripe { RIPE_FROM.load(Relaxed).min(total) } else { 0 };
    let mut stop = None;
    let keep = crate::thp_partial() == crate::THP_KEEP;
    'scan: for (lo, hi) in [(from, total), (0, from)] {
        let mut i = lo;
        while i < hi {
            let d = dead_slot(z.pool.lo + i * RUN);
            let st = d.load(Relaxed);
            if pass == DeadPass::Mark1 {
                if st == RIPE || (matches!(st, SEEN | DEAD) && d.compare_exchange(st, RIPE, AcqRel, Relaxed).is_ok()) {
                    ripe = true;
                }
                i += 1;
                continue;
            }
            if pass == DeadPass::Mark {
                if st == RIPE || (st == SEEN && d.compare_exchange(SEEN, RIPE, AcqRel, Relaxed).is_ok()) {
                    ripe = true;
                } else if st == DEAD {
                    let _ = d.compare_exchange(DEAD, SEEN, AcqRel, Relaxed);
                }
                i += 1;
                continue;
            }
            let any = pass == DeadPass::Any;
            if !(st == RIPE || (any && matches!(st, DEAD | SEEN))) {
                i += 1;
                continue;
            }
            // the limits at a run to give back only (the clock read once a release, not once a slot: a pass scans
            // up to some millions of slots)
            if got >= want || remapped >= budget || crate::purger::pass_late() {
                // RIPE runs left for the next pass, from here
                ripe = true;
                stop = Some(i);
                break 'scan;
            }
            let a = z.pool.lo + i * RUN;
            // pppress card 8 (`pool_thp_partial` 3): the age release leaves a hot superpage's or piece's dead runs held
            // (RIPE, for a later pass); the cap (`Any`) is pressure and does not wait
            if pass == DeadPass::Ripe && crate::state::piece_hot(a) {
                stats::THP_HOT_SKIPS.fetch_add(1, Relaxed);
                ripe = true;
                i += (SUPER - a % SUPER) / RUN;
                continue;
            }
            let j = if keep && span::huge_at(a) {
                // D4760 (`pool_thp_partial` 1): whole dead superpages only; a superpage in part waits, held
                if a % SUPER != 0 {
                    i += (SUPER - a % SUPER) / RUN;
                    continue;
                }
                let j = grab_whole(z.pool.lo, i, hi, max_batch, any);
                if j == i {
                    i += SUPER / RUN;
                    continue;
                }
                j
            } else {
                let mut j = i;
                while j < hi && j - i < max_batch && grab(dead_slot(z.pool.lo + j * RUN), any) {
                    j += 1;
                }
                if j == i {
                    i += 1;
                    continue;
                }
                j
            };
            let bytes = (j - i) * RUN;
            // SAFETY: dead runs of live blocks: their owners do not read them until take_pages, which waits out RELEASING.
            let res = unsafe { os::age_release(a, bytes) };
            let to = if res.is_some() { RELEASED } else { DEAD };
            for k in i..j {
                dead_slot(z.pool.lo + k * RUN).store(to, Release);
            }
            let Some(res) = res else {
                ripe = true;
                stop = Some(j);
                break 'scan;
            };
            stats::DEAD.fetch_sub(bytes as isize, Relaxed);
            stats::claim(-(bytes as isize));
            got += bytes;
            if res > 0 {
                remapped += res;
                stats::RELEASES.fetch_add(1, Relaxed);
                stats::RELEASED_BYTES.fetch_add(res as u64, Relaxed);
            }
            i = j;
        }
    }
    if pass == DeadPass::Ripe {
        RIPE_FROM.store(stop.unwrap_or(0), Relaxed);
    }
    if pass != DeadPass::Any {
        RIPE_LEFT.store(ripe, Relaxed);
    }
    (got, remapped)
}

/// Fault in a built table's pages now (R16).
/// # Safety
/// `[p, p + len)` lies in a live block or span of the caller's, which no other thread writes during the call.
pub unsafe fn prefault(p: NonNull<u8>, len: usize) {
    let page = os::page_size();
    let a = p.as_ptr() as usize;
    let lo = a.next_multiple_of(page);
    let hi = (a + len) & !(page - 1);
    if hi > lo {
        // SAFETY: the caller's contract.
        unsafe { os::populate(lo, hi - lo) };
    }
}

/// What `take(bytes, align, ..)` hands out.
pub fn usable(bytes: usize, align: usize) -> usize {
    if let Some(b) = run::bucket_of(bytes.max(1), align) {
        return run::bucket_bytes(b);
    }
    let k = bytes.div_ceil(RUN);
    if k <= MAX_RUNS as usize {
        return k * RUN;
    }
    // ppround (`PGRUST_POOL_FIT_TAIL`): a fitted span is the ask to the page
    if fit_tail_on() && fits(bytes) {
        return bytes.next_multiple_of(os::page_size());
    }
    bytes.checked_next_multiple_of(SUPER).unwrap_or(bytes)
}

#[inline]
fn debug() -> bool {
    crate::DEBUG.load(Relaxed)
}

pub(crate) fn poison(p: usize, s: usize) {
    let skip = core::mem::size_of::<central::Link>();
    // SAFETY: a block the caller is giving back, ours now, past its link bytes.
    unsafe { core::ptr::write_bytes((p + skip) as *mut u8, POISON, s - skip) };
}

fn poison_chain(head: u32, n: u32, s: usize) {
    let mut cur = head;
    for _ in 0..n {
        let p = central::addr_of_idx(cur);
        cur = central::link_next(p);
        poison(p, s);
    }
}

pub(crate) fn check_poison(p: usize, s: usize) {
    let skip = core::mem::size_of::<central::Link>();
    // SAFETY: a free block we hold, open read-write.
    let b = unsafe { core::slice::from_raw_parts((p + skip) as *const u8, s - skip) };
    if let Some(off) = b.iter().position(|&x| x != POISON) {
        panic!("pagepool: block {p:#x} ({s} B) written at +{} after it was given back", off + skip);
    }
}

const QUARANTINE: usize = 64;
static Q_LOCK: AtomicU32 = AtomicU32::new(0);
static Q: [core::sync::atomic::AtomicUsize; QUARANTINE] = [const { core::sync::atomic::AtomicUsize::new(0) }; QUARANTINE];
static Q_NEXT: AtomicU32 = AtomicU32::new(0);

/// The debug arm: blocks wait `QUARANTINE` gives before they are reused. Some(usable): `a` was queued.
fn quarantine(a: usize) -> Option<usize> {
    let info = run::lookup(a as *const u8)?;
    let Kind::Blocks { bucket } = info.kind else { return None };
    poison(a, run::bucket_bytes(bucket));
    superpage::spin(&Q_LOCK);
    let i = Q_NEXT.fetch_add(1, Relaxed) as usize % QUARANTINE;
    let old = Q[i].swap(a, Relaxed);
    superpage::unspin(&Q_LOCK);
    if old != 0 {
        // SAFETY: a block given back QUARANTINE gives ago, poisoned since.
        unsafe { give_now(old) };
    }
    Some(run::bucket_bytes(bucket))
}
