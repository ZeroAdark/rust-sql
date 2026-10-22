//! pagepool P3 (c): the pool's PRESENT bytes, its pages resident now, beside `claimed`: the RAM the pool holds from the OS
//! by its own count, where a Cold superpage take and a lazy span's `commit_to` count before any page is faulted, so
//! claimed bounds the resident bytes from above (measured under load: claimed 12.54 GB at idle, at most ~10.9 GB resident).
//!
//! A SWEEP is mincore(2) over every superpage of the pool zone that is not Cold (runs of them in one call each), over each
//! live or warm lazy span's committed bytes rounded up to whole superpages (a huge-page fault makes the last 2 MiB of a
//! commit resident whole, and outside the walk it would read as unattributed), and over the pool's metadata zone ([pool-meta]: superpage and run
//! headers, bitmaps, tags), so the reading is the pool's whole resident footprint, smaps' [anon:pgrust:pool] +
//! [anon:pgrust:pool-meta]; the metadata part is also published alone (`meta_present`, a named row's reading).
//! Milliseconds over a multi-GiB pool, so no reader ever sweeps (the
//! governor's resample reads allocator_resident after every settle, on query threads and pg:memwatchdog).
//! The purger sweeps on its own cadence (`PRESENT_MS`) and PUBLISHES the bytes with the released-bytes count at the
//! sweep's start; `present()` is that value less every byte released since (`stats::RELEASED_BYTES`, exact at each
//! release), two atomic loads under a sequence count: a release shows at once, a fault since the sweep at the next one.
//! No lock, no heap, no syscall on a read. `None` with the pool off, off Linux, before the first sweep, or when mincore
//! refused it.
//!
//! pppress card 11: a page counts when it is present and does not map the shared zero page (PAGEMAP_SCAN, Linux 6.7). A
//! THP split remaps every all-zero 4 KiB subpage to the zero page; mincore counts those PTEs, smaps Rss and the memcg
//! charge do not. Without PAGEMAP_SCAN (or `PGRUST_POOL_PRESENT_MINCORE=1`) mincore reads them as before
//! (`present_zero`); kernels before 6.12 make them only on a read fault of a page never written.

use core::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use core::sync::atomic::{fence, AtomicBool, AtomicU32, AtomicU64, AtomicUsize};

/// The purger's sweep cadence, ms: a fault since the last sweep shows within it. Stretched to `DUTY` times the last
/// sweep's own cost when that is longer, so a large pool's sweep stays at most 2 % of the purger's time.
pub(crate) const PRESENT_MS: u32 = 100;
const DUTY: u64 = 50;

/// No sweep published yet (or the last one was refused).
const NONE: usize = usize::MAX;

/// The last sweep's bytes, and `stats::RELEASED_BYTES` read at its start; written under `GEN` (odd while writing).
static SWEPT: AtomicUsize = AtomicUsize::new(NONE);
static META: AtomicUsize = AtomicUsize::new(0);
/// pppress card V: the last sweep's bytes in runs holding no live block, and in CACHE-tag runs holding one.
static FREE_SW: AtomicUsize = AtomicUsize::new(0);
static CACHE_SW: AtomicUsize = AtomicUsize::new(0);
/// pppress (C, report only): the last sweep's bytes in BUILD-tag allocations holding a live block.
static BUILD_SW: AtomicUsize = AtomicUsize::new(0);
/// Card 11: the zero-page bytes the last sweep left out, and whether its reader could tell them (PAGEMAP_SCAN).
static ZERO: AtomicUsize = AtomicUsize::new(0);
static SCANNED: AtomicBool = AtomicBool::new(false);
static RELEASED_AT: AtomicU64 = AtomicU64::new(0);
/// pppress (the overshoot fix): `stats::RELEASED_LIVE` / `RELEASED_UNSWEPT` read at the sweep's start, with RELEASED_AT.
static LIVE_AT: AtomicU64 = AtomicU64::new(0);
static UNSWEPT_AT: AtomicU64 = AtomicU64::new(0);
static GEN: AtomicU32 = AtomicU32::new(0);
/// One sweeper at a time (the purger, a test's seam): 0 free, 1 sweeping.
static SWEEPING: AtomicU32 = AtomicU32::new(0);
/// `purger::tick()` of the last sweep, and its cost in ns.
static SWEPT_AT: AtomicU32 = AtomicU32::new(0);
static SWEEP_NS: AtomicU64 = AtomicU64::new(0);
/// pagepool P3 (c): the pool's activity (`stats::activity`) read at the last sweep's start, and the sweeps so far.
static SWEPT_ACT: AtomicU64 = AtomicU64::new(0);
static SWEEPS: AtomicU64 = AtomicU64::new(0);
/// pagepool P3 (c): the purger's longest skip while no count moves (ms): a quiet pool is swept this often, not every
/// `PRESENT_MS` (measured: the steady sweep was ~3.6 % of a core on an idle server). It bounds how long a first write
/// to a page of a block taken earlier, which moves no count, can go unread.
pub(crate) const QUIET_MS: u32 = 10_000;
/// pagepool P3 (c): `present_fresh` shares a sweep this young (ms) rather than sweeping again: measured on the lane
/// box (a 10 GB join suite at W8, pool at its peak: 27.6 GB claimed, 10.9 GB present), a sweep took up to 9.7 ms, and a
/// view read asks more than once.
pub(crate) const FRESH_MS: u32 = 250;

/// The pool's resident bytes as of the purger's last sweep, less the bytes released since by `less_released`'s rule
/// (`None`: off, unreadable, or not swept yet). A few atomic loads under the sequence count: never a sweep. A range
/// unmapped meanwhile reads as not resident.
pub fn present() -> Option<usize> {
    present_split().map(|s| s.0)
}

/// pppress (the overshoot fix): the sweep's (present, free, cache) less the bytes released since it. A release counts
/// WHOLE ranges (`release_run`'s n superpages, a whole dead run), not the pages of them that were resident, so taken
/// as is it overshoots: present read 0 with GBs resident, free saturated, cache capped (61's guard rows). So each
/// release comes off the part it can take:
/// - `free_gone` (released out of what the sweep classes free: free superpages and runs, dead runs, warm lazy spans)
///   comes off free and present alike, at most free;
/// - `live_gone` (`stats::RELEASED_LIVE`: a live lazy span's trimmed tail, a just-given span dropped) comes off present
///   only, at most its non-free part;
/// - `unswept` (`stats::RELEASED_UNSWEPT`: a frame's pages, outside what the sweep reads) comes off nothing.
/// cache is capped at present - free and never lowered otherwise.
pub(crate) fn less_released(p: usize, f: usize, c: usize, gone: u64, live_gone: u64, unswept: u64) -> (usize, usize, usize) {
    let to = |x: u64| usize::try_from(x).unwrap_or(usize::MAX);
    let free_gone = to(gone.saturating_sub(live_gone).saturating_sub(unswept));
    let fsub = free_gone.min(f);
    let (p, f) = (p.saturating_sub(fsub), f - fsub);
    let p = p - to(live_gone).min(p.saturating_sub(f));
    (p, f, c.min(p - f.min(p)))
}

/// The resident bytes of the pool's metadata zone ([pool-meta]) as of the last sweep (`None`: off or not swept yet): a
/// part of `present()`, published alone for a row of its own.
pub fn meta_present() -> Option<usize> {
    if !crate::on() || SWEPT.load(Acquire) == NONE {
        return None;
    }
    Some(META.load(Relaxed))
}

/// pppress card V: the last sweep's (present, free, cache), from one read of it (one sequence count), so a reader that
/// sets them side by side sees one sweep's: `present` as `present()` reads it; `free` its present bytes in runs that hold
/// NO live block; `cache` its present bytes in CACHE-tag allocations that hold one. 0 <= free, free + cache <= present.
///
/// `free`: whole free superpages (warm zero and warm dirty), free runs and dead runs inside superpages and spans, blocks
/// runs whose blocks are all home, and warm lazy spans' committed pages. Each page is classified by its run's state in
/// the same sweep that counts it present (PRESENT and not the zero page; the metadata zone never counts), on the
/// sweep's thread. A direct count, never a difference. Less the bytes released since the sweep by `less_released`'s
/// rule (a release out of free pages comes off free and present alike, at most free; one out of a live allocation off
/// present only). Not read: blocks on threads' shelves and on the central lists,
/// which sit inside runs that look allocated, and the small tier's free slots and deferred tails, which sit in runs a
/// thread owns.
///
/// `cache`: `Tag::CACHE` runs, blocks runs carved for it and spans, and pppress (B) `Tag::DICT` ones (the resident fp
/// tables' build buffers, retagged while the cache holds them: `retag`), their dead runs left out (they are in `free`).
/// Capped at `present - free` (a release since the sweep may take pages faulted after it). `None` as `present`.
pub fn present_split() -> Option<(usize, usize, usize)> {
    present_split_build().map(|(p, f, c, _)| (p, f, c))
}

/// pppress (C, report only): `present_split` and, from the same read of the same sweep, its present bytes in
/// `Tag::BUILD` allocations holding a live block (a table build's buffers: the resident fp tables', the build pool's
/// kept ones and in-flight builds'), dead runs left out. Never in `cache`. Capped at `present - free - cache` after
/// `less_released`'s rule, so free + cache + build <= present.
pub fn present_split_build() -> Option<(usize, usize, usize, usize)> {
    if !crate::on() {
        return None;
    }
    loop {
        let g = GEN.load(Acquire);
        if g & 1 != 0 {
            core::hint::spin_loop();
            continue;
        }
        let p = SWEPT.load(Relaxed);
        let f = FREE_SW.load(Relaxed);
        let c = CACHE_SW.load(Relaxed);
        let b = BUILD_SW.load(Relaxed);
        let (r, l, u) = (RELEASED_AT.load(Relaxed), LIVE_AT.load(Relaxed), UNSWEPT_AT.load(Relaxed));
        fence(Acquire);
        if GEN.load(Relaxed) != g {
            continue;
        }
        if p == NONE {
            return None;
        }
        use crate::stats::{RELEASED_BYTES, RELEASED_LIVE, RELEASED_UNSWEPT};
        let gone = RELEASED_BYTES.load(Relaxed).saturating_sub(r);
        let live_gone = RELEASED_LIVE.load(Relaxed).saturating_sub(l);
        let unswept = RELEASED_UNSWEPT.load(Relaxed).saturating_sub(u);
        let (p, f, c) = less_released(p, f, c, gone, live_gone, unswept);
        return Some((p, f, c, b.min(p - f - c)));
    }
}

/// pppress card V: `present_split`'s free bytes (runs holding no live block).
pub fn free_present() -> Option<usize> {
    present_split().map(|s| s.1)
}

/// pppress card V: `present_split`'s cache bytes (CACHE-tag allocations holding a live block).
pub fn cache_present() -> Option<usize> {
    present_split().map(|s| s.2)
}

/// pppress (C, report only): `present_split_build`'s BUILD-tag bytes.
pub fn build_present() -> Option<usize> {
    present_split_build().map(|s| s.3)
}

/// Card 11: the last sweep's reader and the zero-page bytes it left out of `present`: (true, bytes) by PAGEMAP_SCAN;
/// (false, 0) by mincore, which counts zero-page PTEs as resident. `None` as `meta_present`.
pub fn present_zero() -> Option<(bool, usize)> {
    if !crate::on() || SWEPT.load(Acquire) == NONE {
        return None;
    }
    Some((SCANNED.load(Relaxed), ZERO.load(Relaxed)))
}

/// Sweep now and publish (the purger's cadence; a test's seam). The swept bytes, `None` when another sweep is running or
/// mincore refused (then nothing is published).
pub(crate) fn refresh() -> Option<usize> {
    if SWEEPING.compare_exchange(0, 1, Acquire, Relaxed).is_err() {
        return None;
    }
    // r0 (and l0 / u0) read BEFORE the sweep: a release during the sweep may take pages the sweep has already
    // counted, and present must not read above what is resident (the views' unattributed row and the ladder's
    // retained reading set it against the process's resident bytes); one that takes pages the sweep then reads as not
    // resident comes off too, but `less_released` caps it at the sweep's free part
    let r0 = crate::stats::RELEASED_BYTES.load(Relaxed);
    let l0 = crate::stats::RELEASED_LIVE.load(Relaxed);
    let u0 = crate::stats::RELEASED_UNSWEPT.load(Relaxed);
    SWEPT_ACT.store(crate::stats::activity(), Relaxed);
    SWEEPS.fetch_add(1, Relaxed);
    let t0 = pg_clock::mono_ns();
    let swept = imp::sweep(false);
    SWEEP_NS.store(pg_clock::mono_ns().saturating_sub(t0), Relaxed);
    GEN.fetch_add(1, Acquire);
    fence(Release);
    SWEPT.store(swept.map_or(NONE, |s| s.all), Relaxed);
    META.store(swept.map_or(0, |s| s.meta), Relaxed);
    FREE_SW.store(swept.map_or(0, |s| s.free), Relaxed);
    CACHE_SW.store(swept.map_or(0, |s| s.cache), Relaxed);
    BUILD_SW.store(swept.map_or(0, |s| s.build), Relaxed);
    ZERO.store(swept.map_or(0, |s| s.zero), Relaxed);
    SCANNED.store(swept.is_some_and(|s| s.scanned), Relaxed);
    RELEASED_AT.store(r0, Relaxed);
    LIVE_AT.store(l0, Relaxed);
    UNSWEPT_AT.store(u0, Relaxed);
    GEN.fetch_add(1, Release);
    SWEPT_AT.store(crate::purger::tick(), Relaxed);
    SWEEPING.store(0, Release);
    swept.map(|s| s.all)
}

/// D4790 (pppress): ms since the sweep `present` reads from (`None`: off, or no sweep published yet): a reader that sets
/// `present` beside counts it reads now carries the skew with it (a fault since the sweep shows at the next one).
pub fn present_age_ms() -> Option<u64> {
    if !crate::on() || SWEPT.load(Acquire) == NONE {
        return None;
    }
    Some(u64::from(crate::purger::tick().wrapping_sub(SWEPT_AT.load(Relaxed))))
}

/// The purger's cadence: a sweep when `PRESENT_MS` (or `DUTY` times the last sweep's cost) passed since the last one and
/// some count moved since (`stats::activity`), else once `QUIET_MS` passed. Whether it swept.
pub(crate) fn refresh_if_due(now: u32) -> bool {
    let every = PRESENT_MS.max(u32::try_from(SWEEP_NS.load(Relaxed).saturating_mul(DUTY) / 1_000_000).unwrap_or(u32::MAX));
    let since = now.wrapping_sub(SWEPT_AT.load(Relaxed));
    if SWEPT.load(Relaxed) != NONE && (since < every || (since < QUIET_MS && crate::stats::activity() == SWEPT_ACT.load(Relaxed))) {
        return false;
    }
    refresh().is_some()
}

/// pagepool P3 (c): the pool's resident bytes NOW, for a reader that sets them against readings it takes at the same
/// moment (the memory views' unattributed row and the ladder's retained reading, beside the process's resident bytes
/// and the allocator's own mincore): `present()` lags a fault by up to a sweep period, which such a reader shows as a
/// false spike. A sweep on the calling thread, published (the purger's next one waits its period from here); with
/// another sweep running, its result once it ends; with one ended under `FRESH_MS` ago, that one's (shared, as `present`
/// reads it). Milliseconds over a multi-GiB pool (mincore, mmap_lock read a call at a time): a view's or the ladder's
/// read, never a query thread's. `None` as `present`.
pub fn present_fresh() -> Option<usize> {
    if !crate::on() {
        return None;
    }
    if SWEPT.load(Acquire) != NONE && crate::purger::tick().wrapping_sub(SWEPT_AT.load(Relaxed)) < FRESH_MS {
        return present();
    }
    if let Some(p) = refresh() {
        return Some(p);
    }
    // another sweep is running: its result is as fresh (bounded wait: a sweep is milliseconds)
    let t0 = pg_clock::mono_ns();
    while SWEEPING.load(Acquire) != 0 && pg_clock::mono_ns().saturating_sub(t0) < 50_000_000 {
        std::thread::yield_now();
    }
    present()
}

/// Test seam: the purger's sweep cadence once, now (`refresh_if_due`); whether it swept.
#[doc(hidden)]
pub fn present_due_now() -> bool {
    crate::on() && refresh_if_due(crate::purger::tick())
}

/// Test seam: the sweeps so far (the purger's, the views' and the seams').
#[doc(hidden)]
pub fn present_sweeps() -> u64 {
    SWEEPS.load(Relaxed)
}

/// The last published sweep's cost, ns (the witness's reading of the purger's duty).
#[doc(hidden)]
pub fn present_sweep_ns() -> u64 {
    SWEEP_NS.load(Relaxed)
}

/// Test seam: sweep and publish on the calling thread (a test that starts no pool thread stands in for the purger).
#[doc(hidden)]
pub fn refresh_present() -> Option<usize> {
    if !crate::on() {
        return None;
    }
    refresh()
}

/// Witness seam: a full sweep, not published (what `present()` approximates). Milliseconds over a large pool: never on a
/// reader's path.
#[doc(hidden)]
pub fn present_sweep() -> Option<usize> {
    if !crate::on() {
        return None;
    }
    imp::sweep(false).map(|s| s.all)
}

/// Card 11 witness seam: a full sweep, not published, by mincore (`mincore`) or the default reader: (present bytes,
/// zero-page bytes left out, whether PAGEMAP_SCAN read it, ns).
#[doc(hidden)]
pub fn present_sweep_by(mincore: bool) -> Option<(usize, usize, bool, u64)> {
    if !crate::on() {
        return None;
    }
    let t0 = pg_clock::mono_ns();
    imp::sweep(mincore).map(|s| (s.all, s.zero, s.scanned, pg_clock::mono_ns().saturating_sub(t0)))
}

/// Card 11: `PGRUST_POOL_PRESENT_MINCORE=1` (mincore always), read once at the pool's init.
pub(crate) fn init_reader() {
    #[cfg(all(target_os = "linux", not(miri)))]
    imp::MINCORE.store(crate::os::env_is_one(c"PGRUST_POOL_PRESENT_MINCORE"), Relaxed);
}

/// pagepool P3 (F1 / F2): the pages of `[at, at + len)` (page-aligned) now, as (resident bytes, zero-page bytes; card 11:
/// 0 by mincore, which counts them resident). The age release's check that a range has pages to give back before a
/// remap takes mmap_lock for it (a zero-page PTE is one: the remap frees its page table), and the pass budget's cost
/// (the resident bytes). Milliseconds a GiB at most: the purger's call only. `None` off Linux or when mincore refused
/// (the caller then gives the range back as if resident). A page on a swap device reads as not resident
/// (`os::age_release` clears such a range, never skips it).
#[cfg(all(target_os = "linux", not(miri)))]
pub(crate) fn residency_of(at: usize, len: usize) -> Option<(usize, usize)> {
    let mut vec = [0u8; imp::VEC];
    imp::in_ram(at, len, crate::os::page_size(), &mut vec).map(|p| (p.resident, p.zero))
}

#[cfg(not(all(target_os = "linux", not(miri))))]
pub(crate) fn residency_of(_at: usize, _len: usize) -> Option<(usize, usize)> {
    None
}

/// pppress card 10: whether a page of `[at, at + len)` is mapped by a huge page (a PMD-mapped THP, PAGEMAP_SCAN's
/// PAGE_IS_HUGE), so a give-back of part of it splits its folio first (D4760). `None`: no PAGEMAP_SCAN reading.
#[cfg(all(target_os = "linux", not(miri)))]
pub(crate) fn huge_in(at: usize, len: usize) -> Option<bool> {
    imp::huge(at, len)
}

#[cfg(not(all(target_os = "linux", not(miri))))]
pub(crate) fn huge_in(_at: usize, _len: usize) -> Option<bool> {
    None
}

/// D4877 test seam: `residency_of` (a give-back's read: bytes in RAM, a page in migration counted) over any page-aligned
/// range of the caller's own.
#[doc(hidden)]
pub fn in_ram_seam(at: usize, len: usize) -> Option<(usize, usize)> {
    residency_of(at, len)
}

/// Card 11 test seam: the sweep's reader (`range`) over any page-aligned range of the caller's own, by mincore
/// (`mincore`) or the default reader, and whether PAGEMAP_SCAN read it.
#[doc(hidden)]
pub fn residency_seam(at: usize, len: usize, mincore: bool) -> Option<(usize, usize, bool)> {
    #[cfg(all(target_os = "linux", not(miri)))]
    {
        let mut vec = [0u8; imp::VEC];
        imp::range(at, len, crate::os::page_size(), &mut vec, mincore).map(|p| (p.resident, p.zero, p.scanned))
    }
    #[cfg(not(all(target_os = "linux", not(miri))))]
    {
        let _ = (at, len, mincore);
        None
    }
}

#[cfg(all(target_os = "linux", not(miri)))]
mod imp {
    use crate::os;
    use crate::reserve::zones;
    use crate::run::{self, RUN};
    use crate::span;
    use crate::superpage::{self, State, SUPER};
    use crate::Tag;
    use core::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed};
    use core::sync::atomic::{AtomicBool, AtomicI32};

    /// Pages per mincore call (one stack vector).
    pub(super) const VEC: usize = 4096;

    #[derive(Clone, Copy)]
    pub(super) struct Sweep {
        pub(super) all: usize,
        pub(super) meta: usize,
        pub(super) zero: usize,
        pub(super) scanned: bool,
        /// pppress card V: `all`'s bytes in runs holding no live block, and in CACHE-tag runs holding one.
        pub(super) free: usize,
        pub(super) cache: usize,
        /// pppress (C): `all`'s bytes in BUILD-tag allocations holding a live block.
        pub(super) build: usize,
    }

    /// pppress card V: swept bytes by what holds them.
    #[derive(Clone, Copy, Default)]
    struct Split {
        free: usize,
        cache: usize,
        build: usize,
    }

    impl Split {
        /// Live bytes of an allocation tagged `tag`: CACHE's and DICT's (pppress (B): the resident fp tables' buffers,
        /// retagged while the cache holds them) to cache, BUILD's to build (pppress (C)), others to neither.
        fn live(&mut self, tag: Tag, n: usize) {
            if tag == Tag::CACHE || tag == Tag::DICT {
                self.cache += n;
            } else if tag == Tag::BUILD {
                self.build += n;
            }
        }
    }

    /// pppress card V: resident `[lo, hi)` of the pool zone by its superpages' states: a free superpage's whole; a
    /// Partial / Full superpage's run by run (`run::sweep_kind`, a dead run free too); a span's as a live span's.
    fn split_pool(lo: usize, hi: usize, s: &mut Split) {
        let mut a = lo;
        while a < hi {
            let i = superpage::index_of(a);
            let end = (superpage::addr_of(i) + SUPER).min(hi);
            // one load of the word: its state and a span's head from the same word (two loads may see the superpage
            // change between them, and hand the head another state's bits); a head out of the table reads untagged
            let w = superpage::meta(i).word.load(Acquire);
            match State::from_bits(w) {
                State::WarmZero | State::WarmDirty => s.free += end - a,
                State::Span => {
                    let h = (w >> 32) as u32;
                    let tag = if h < zones().nsuper { Tag(superpage::meta(h).tag()) } else { Tag::OTHER };
                    split_live(a, end, tag, s);
                }
                State::Partial | State::Full => {
                    let mut b = a;
                    while b < end {
                        let next = ((b & !(RUN - 1)) + RUN).min(end);
                        let (free, tag) = run::sweep_kind(run::run_index(b));
                        if free || crate::api::dead_at(b) {
                            s.free += next - b;
                        } else {
                            s.live(tag, next - b);
                        }
                        b = next;
                    }
                }
                _ => {}
            }
            a = end;
        }
    }

    /// pppress card V: resident `[lo, hi)` of a live span (pool or lazy) tagged `tag`: its dead runs free, the rest the
    /// tag's (CACHE's and BUILD's counted).
    fn split_live(lo: usize, hi: usize, tag: Tag, s: &mut Split) {
        if !crate::api::any_dead() {
            s.live(tag, hi - lo);
            return;
        }
        let mut b = lo;
        while b < hi {
            let next = ((b & !(RUN - 1)) + RUN).min(hi);
            if crate::api::dead_at(b) {
                s.free += next - b;
            } else {
                s.live(tag, next - b);
            }
            b = next;
        }
    }

    /// A range's resident bytes (present, not the zero page) and zero-page bytes; `scanned`: PAGEMAP_SCAN read all of it.
    #[derive(Clone, Copy)]
    pub(super) struct Pages {
        pub(super) resident: usize,
        pub(super) zero: usize,
        pub(super) scanned: bool,
    }

    impl Pages {
        fn add(&mut self, o: Pages) {
            self.resident += o.resident;
            self.zero += o.zero;
            self.scanned &= o.scanned;
        }
    }

    /// The pool's pages (all of it, metadata included) and the metadata zone's resident part.
    pub(super) fn sweep(mincore: bool) -> Option<Sweep> {
        let z = zones();
        let page = os::page_size();
        let by_scan = !mincore && fd().is_some();
        let step = if by_scan { u32::MAX } else { (VEC * page / SUPER).max(1) as u32 };
        let mut vec = [0u8; VEC];
        let mut total = Pages { resident: 0, zero: 0, scanned: by_scan };
        let mut split = Split::default();
        let mut i = 0u32;
        while i < z.nsuper {
            if superpage::meta(i).state() == State::Cold {
                i += 1;
                continue;
            }
            let mut j = i + 1;
            while j < z.nsuper && j - i < step && superpage::meta(j).state() != State::Cold {
                j += 1;
            }
            // pppress card V: each resident stretch classified as it is counted, in this one pass
            total.add(range_by(superpage::addr_of(i), (j - i) as usize * SUPER, page, &mut vec, mincore, &mut split, &split_pool)?);
            i = j;
        }
        let mut err = None;
        // a lazy span's commit rounded up to whole superpages (its slot is whole superpages): a huge-page fault past
        // the commit's last page is resident and the pool's. pppress card V: a warm span's pages hold no live block
        span::each_committed_by(|at, len, warm, tag| {
            if err.is_none() {
                let by = |lo: usize, hi: usize, s: &mut Split| {
                    if warm {
                        s.free += hi - lo;
                    } else {
                        split_live(lo, hi, tag, s);
                    }
                };
                match range_by(at, len.next_multiple_of(SUPER), page, &mut vec, mincore, &mut split, &by) {
                    Some(p) => total.add(p),
                    None => err = Some(()),
                }
            }
        });
        if err.is_some() {
            return None;
        }
        let meta_lo = z.frame.lo + z.frame.len;
        let meta = range(meta_lo, (z.all.lo + z.all.len).saturating_sub(meta_lo), page, &mut vec, mincore)?;
        total.add(meta);
        // each counted stretch went to at most one of them (a scan refused part way through had its classing undone
        // before mincore read the range again), so free + cache <= resident by construction; the mins only guard it
        let free = split.free.min(total.resident);
        let cache = split.cache.min(total.resident - free);
        let build = split.build.min(total.resident - free - cache);
        Some(Sweep { all: total.resident, meta: meta.resident, zero: total.zero, scanned: total.scanned, free, cache, build })
    }

    /// The pages of `[at, at + len)`, any length: by PAGEMAP_SCAN, else (`mincore`, or refused) by mincore, `VEC` pages
    /// a call.
    pub(super) fn range(at: usize, len: usize, page: usize, vec: &mut [u8; VEC], mincore: bool) -> Option<Pages> {
        range_by(at, len, page, vec, mincore, &mut Split::default(), &|_, _, _| {})
    }

    /// pppress card V: `range`, with `by(lo, hi, s)` classing each resident stretch it counts (present, not the zero
    /// page) into `s`, in address order. A scan refused part way through (its pages then read again by mincore) has its
    /// classing undone first, so each counted stretch is classed once.
    fn range_by(at: usize, len: usize, page: usize, vec: &mut [u8; VEC], mincore: bool, s: &mut Split, by: &dyn Fn(usize, usize, &mut Split)) -> Option<Pages> {
        if !mincore {
            if let Some(fd) = fd() {
                let keep = *s;
                if let Some(p) = scan(fd, at, len, page, None, &mut |lo, hi| by(lo, hi, s)) {
                    return Some(p);
                }
                *s = keep;
            }
        }
        let mut total = 0;
        let mut a = at;
        while a < at + len {
            let n = (at + len - a).min(VEC * page);
            total += resident(a, n, page, vec, &mut |lo, hi| by(lo, hi, s))?;
            a += n;
        }
        Some(Pages { resident: total, zero: 0, scanned: false })
    }

    /// D4877: a give-back's read of `[at, at + len)` (`residency_of`): its bytes in RAM, the zero page apart. As `range`,
    /// but a stretch PAGEMAP_SCAN reads SWAPPED is read again by mincore, which counts a page a compaction move is
    /// migrating (its PTE a migration entry, SWAPPED to the scan though in RAM) and a swapped page only while it is in the
    /// swap cache. PRESENT alone read a migrating page as not given back: base_pages counted 8384512 of 8388608 bytes.
    pub(super) fn in_ram(at: usize, len: usize, page: usize, vec: &mut [u8; VEC]) -> Option<Pages> {
        if let Some(fd) = fd() {
            if let Some(p) = scan(fd, at, len, page, Some(&mut *vec), &mut |_, _| {}) {
                return Some(p);
            }
        }
        range(at, len, page, vec, true)
    }

    /// The resident bytes of `[at, at + len)` (page-aligned, at most `VEC` pages); an unmapped range (ENOMEM) reads 0.
    /// `f` on each stretch of resident pages.
    fn resident(at: usize, len: usize, page: usize, vec: &mut [u8; VEC], f: &mut dyn FnMut(usize, usize)) -> Option<usize> {
        let n = len.div_ceil(page);
        debug_assert!(n <= VEC);
        // SAFETY: mincore only reads the page tables of `[at, at + len)` and writes `n` bytes into `vec` (n <= VEC).
        let rc = unsafe { libc::mincore(at as *mut libc::c_void, n * page, vec.as_mut_ptr().cast()) };
        if rc != 0 {
            return (std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOMEM)).then_some(0);
        }
        let (mut total, mut run) = (0, None::<usize>);
        for (k, b) in vec[..n].iter().enumerate() {
            match (b & 1 != 0, run) {
                (true, None) => run = Some(k),
                (false, Some(s)) => {
                    f(at + s * page, at + k * page);
                    total += (k - s) * page;
                    run = None;
                }
                _ => {}
            }
        }
        if let Some(s) = run {
            f(at + s * page, at + n * page);
            total += (n - s) * page;
        }
        Some(total)
    }

    /// `PGRUST_POOL_PRESENT_MINCORE=1`: mincore always.
    pub(super) static MINCORE: AtomicBool = AtomicBool::new(false);
    /// /proc/self/pagemap for PAGEMAP_SCAN (-2 not open, -1 refused: mincore from then on) and the pid that opened it (a
    /// forked child's inherited descriptor reads its parent's pages).
    static FD: AtomicI32 = AtomicI32::new(-2);
    static FD_PID: AtomicI32 = AtomicI32::new(0);

    fn fd() -> Option<i32> {
        if MINCORE.load(Relaxed) {
            return None;
        }
        let fd = FD.load(Acquire);
        if fd == -1 {
            return None;
        }
        // SAFETY: getpid has no preconditions.
        let pid = unsafe { libc::getpid() };
        if fd >= 0 && FD_PID.load(Relaxed) == pid {
            return Some(fd);
        }
        let f = os::open_ro(c"/proc/self/pagemap");
        if f < 0 {
            return None;
        }
        FD_PID.store(pid, Relaxed);
        match FD.compare_exchange(fd, f, AcqRel, Acquire) {
            Ok(_) => Some(f),
            // a racing opener's descriptor stays open (one, once), as `os::mapped_of`'s
            Err(won) => (won >= 0).then_some(won),
        }
    }

    /// <linux/fs.h>: struct pm_scan_arg and struct page_region.
    #[repr(C)]
    struct ScanArg {
        size: u64,
        flags: u64,
        start: u64,
        end: u64,
        walk_end: u64,
        vec: u64,
        vec_len: u64,
        max_pages: u64,
        category_inverted: u64,
        category_mask: u64,
        category_anyof_mask: u64,
        return_mask: u64,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Region {
        start: u64,
        end: u64,
        categories: u64,
    }

    /// _IOWR('f', 16, struct pm_scan_arg)
    const PAGEMAP_SCAN: u32 = 0xc060_6610;
    const PAGE_IS_PRESENT: u64 = 1 << 3;
    const PAGE_IS_SWAPPED: u64 = 1 << 4;
    const PAGE_IS_PFNZERO: u64 = 1 << 5;
    const PAGE_IS_HUGE: u64 = 1 << 6;
    const REGIONS: usize = 256;
    const _: () = assert!(core::mem::size_of::<ScanArg>() == 96 && core::mem::size_of::<Region>() == 24);

    /// pppress card 10: whether a page of `[at, at + len)` is present and PMD-mapped (one region at most: the first).
    pub(super) fn huge(at: usize, len: usize) -> Option<bool> {
        let fd = fd()?;
        let mut v = [Region { start: 0, end: 0, categories: 0 }; 1];
        let mut arg = ScanArg {
            size: core::mem::size_of::<ScanArg>() as u64,
            flags: 0,
            start: at as u64,
            end: (at + len) as u64,
            walk_end: 0,
            vec: v.as_mut_ptr() as u64,
            vec_len: 1,
            max_pages: 1,
            category_inverted: 0,
            category_mask: PAGE_IS_PRESENT | PAGE_IS_HUGE,
            category_anyof_mask: 0,
            return_mask: PAGE_IS_HUGE,
        };
        // SAFETY: PAGEMAP_SCAN reads `arg`, writes its walk_end and at most one region into `v`.
        let n = unsafe { libc::ioctl(fd, PAGEMAP_SCAN as _, &mut arg) };
        (n >= 0).then_some(n > 0)
    }

    /// The present pages of `[at, at + len)` by PAGEMAP_SCAN, those mapping the zero page apart; with `ram`, also the
    /// pages of its SWAPPED stretches mincore reads resident (`in_ram`). `None`: the ioctl failed (ENOTTY / EINVAL /
    /// EOPNOTSUPP: no PAGEMAP_SCAN here, mincore from then on).
    fn scan(fd: i32, at: usize, len: usize, page: usize, mut ram: Option<&mut [u8; VEC]>, f: &mut dyn FnMut(usize, usize)) -> Option<Pages> {
        let end = at + len;
        let mut v = [Region { start: 0, end: 0, categories: 0 }; REGIONS];
        let mut p = Pages { resident: 0, zero: 0, scanned: true };
        // 7.0 can set walk_end before the last region returned with the vector not full, and a split under the walk
        // can return a stretch twice: resume only on a full vector, and count each byte once (past `covered`)
        let (mut from, mut covered) = (at, at);
        while from < end {
            let mut arg = ScanArg {
                size: core::mem::size_of::<ScanArg>() as u64,
                flags: 0,
                start: from as u64,
                end: end as u64,
                walk_end: 0,
                vec: v.as_mut_ptr() as u64,
                vec_len: REGIONS as u64,
                max_pages: 0,
                category_inverted: 0,
                category_mask: if ram.is_some() { 0 } else { PAGE_IS_PRESENT },
                category_anyof_mask: if ram.is_some() { PAGE_IS_PRESENT | PAGE_IS_SWAPPED } else { 0 },
                return_mask: PAGE_IS_PRESENT | PAGE_IS_PFNZERO | if ram.is_some() { PAGE_IS_SWAPPED } else { 0 },
            };
            // SAFETY: PAGEMAP_SCAN reads `arg`, writes its walk_end and at most `vec_len` regions into `v`.
            let n = unsafe { libc::ioctl(fd, PAGEMAP_SCAN as _, &mut arg) };
            if n < 0 {
                if matches!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ENOTTY | libc::EINVAL | libc::EOPNOTSUPP)) {
                    FD.store(-1, Relaxed);
                }
                return None;
            }
            let n = (n as usize).min(REGIONS);
            for r in &v[..n] {
                let (lo, hi) = ((r.start as usize).max(covered), (r.end as usize).min(end));
                if hi > lo {
                    if r.categories & PAGE_IS_PFNZERO != 0 {
                        p.zero += hi - lo;
                    } else if r.categories & PAGE_IS_PRESENT != 0 {
                        p.resident += hi - lo;
                        f(lo, hi);
                    } else if let Some(vec) = ram.as_deref_mut() {
                        let mut a = lo;
                        while a < hi {
                            let n = (hi - a).min(VEC * page);
                            p.resident += resident(a, n, page, vec, f)?;
                            a += n;
                        }
                    }
                    covered = hi;
                }
            }
            if n < REGIONS {
                break;
            }
            let next = (arg.walk_end as usize).max(covered);
            if next <= from {
                return None;
            }
            from = next;
        }
        Some(p)
    }
}

#[cfg(not(all(target_os = "linux", not(miri))))]
mod imp {
    #[allow(dead_code)]
    #[derive(Clone, Copy)]
    pub(super) struct Sweep {
        pub(super) all: usize,
        pub(super) meta: usize,
        pub(super) zero: usize,
        pub(super) scanned: bool,
        pub(super) free: usize,
        pub(super) cache: usize,
        pub(super) build: usize,
    }

    pub(super) fn sweep(_mincore: bool) -> Option<Sweep> {
        None
    }
}
