//! `PoolAlloc`, a global allocator on the pool: up to 1 KiB from the small tier, bigger asks as class-General blocks,
//! runs and spans. The pool starts at the first allocation when the binary has not started it. Where it cannot start
//! (and for the allocations its own start makes) the small tier lays its runs in the system allocator's memory and the
//! bigger asks are the system's, which `owns` tells apart; a small block is always in a run, found by its address.
//! pphatch: `PGRUST_ALLOCATOR=mimalloc` in the environment at the process's first allocation sends every call to
//! mimalloc instead, for the process's life (an environment variable: a setting is read long after the first block).
//! Naming no allocator, the pool serves, except in a binary that says `by_arch_default!()` (the server; the user
//! 2026-10-18, x86_64 with D4872): the pool on Linux aarch64 and x86_64, mimalloc elsewhere (`crate::HEARTH_BY_ARCH`);
//! `PGRUST_ALLOCATOR=hearth` chooses
//! the pool there.

use core::alloc::{GlobalAlloc, Layout};
use std::alloc::System;

#[cfg(not(target_family = "wasm"))]
pub use imp::*;

/// The global allocator. Wrap it as the binary's `#[global_allocator]` (under `init_small::oom::Rescue` in the server).
pub struct PoolAlloc;

#[cfg(target_family = "wasm")]
// SAFETY: forwards to the system allocator.
unsafe impl GlobalAlloc for PoolAlloc {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        unsafe { System.alloc_zeroed(l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        unsafe { System.realloc(p, l, n) }
    }
}

#[cfg(not(target_family = "wasm"))]
mod imp {
    use super::*;
    use crate::run::{self, MAX_RUNS, RUN};
    use crate::small::{self, ALIGN_MAX, SMALL_MAX};
    use crate::{api, Class, Hint, Tag, Zero};
    use core::cell::Cell;
    use core::ptr::NonNull;
    use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering::*};
    // the hatch's allocator: `Hatch`, at the end of this module

    const BIG: Hint = Hint::new(Class::General, Tag::HEAP);
    const CACHE_BIG: Hint = Hint::new(Class::General, Tag::CACHE);
    /// A cache block was ever taken: frees past the small tier then read the block's tag (HEAP or CACHE).
    static CACHE_USED: AtomicBool = AtomicBool::new(false);

    #[inline(always)]
    fn is_small(size: usize, align: usize) -> bool {
        size <= SMALL_MAX && align <= ALIGN_MAX
    }

    const POOL: u8 = 0;
    const MIMALLOC: u8 = 1;
    const UNSET: u8 = 2;
    /// pphatch: who serves `PoolAlloc`'s calls, fixed at the first one and never changed after, so a block always goes
    /// back to the allocator that made it (a forked child inherits it with the blocks).
    static MODE: AtomicU8 = AtomicU8::new(UNSET);
    static DECIDES: AtomicU32 = AtomicU32::new(0);

    /// The pool serves, decided: a free's and a resize's one test (a byte load and a branch: the small tier reads a
    /// block's run header, which a mimalloc block lacks); the other case (the hatch) goes out of line (`off_dealloc` ..),
    /// so the pool's road inlines as it did before the hatch.
    #[inline(always)]
    fn served() -> bool {
        MODE.load(Relaxed) == POOL
    }

    /// The pool serves, deciding first when no call has.
    #[inline(always)]
    fn pool_mode() -> bool {
        let m = MODE.load(Relaxed);
        m == POOL || (m == UNSET && decide() == POOL)
    }

    /// A take under the hatch, from the pool's misses (`small::alloc_slow`, `big_slow`, `base_big`).
    #[cold]
    #[inline(never)]
    pub(crate) fn hatch_alloc(size: usize, align: usize) -> *mut u8 {
        // SAFETY: the caller's valid layout (a GlobalAlloc take's).
        unsafe { Hatch.alloc(Layout::from_size_align_unchecked(size, align)) }
    }

    /// A free or resize `served` sent here: one under the hatch (a block exists, so the mode is decided).
    #[cold]
    #[inline(never)]
    unsafe fn off_dealloc(p: *mut u8, l: Layout) {
        // SAFETY: the caller's: `p` is a live block of this layout, made on the road decided before it.
        unsafe { if pool_mode() { pool_dealloc(p, l) } else { Hatch.dealloc(p, l) } }
    }

    #[cold]
    #[inline(never)]
    unsafe fn off_realloc(p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        // SAFETY: as `off_dealloc`, and `new` the caller's valid size.
        unsafe { if pool_mode() { pool_realloc(p, l, new) } else { Hatch.realloc(p, l, new) } }
    }

    /// `PGRUST_ALLOCATOR` read once (getenv: no allocation), else this process's default (`crate::hearth_default`).
    /// Racing first calls read the same environment; the first store stands.
    #[cold]
    #[inline(never)]
    fn decide() -> u8 {
        DECIDES.fetch_add(1, Relaxed);
        let want = if crate::hearth_chosen(crate::os::env_str(c"PGRUST_ALLOCATOR"), crate::hearth_default()) { POOL } else { MIMALLOC };
        match MODE.compare_exchange(UNSET, want, Relaxed, Relaxed) {
            Ok(_) => want,
            Err(m) => m,
        }
    }

    #[inline]
    pub(crate) fn hatch() -> bool {
        !pool_mode()
    }

    pub(crate) fn decides() -> u32 {
        DECIDES.load(Relaxed)
    }

    static TRIED: AtomicU8 = AtomicU8::new(0);

    thread_local! {
        static STARTING: Cell<bool> = const { Cell::new(false) };
        static UNCAPPED: Cell<u32> = const { Cell::new(0) };
        static BASE_ASK: Cell<bool> = const { Cell::new(false) };
    }

    pub(crate) fn with_base<T>(f: impl FnOnce() -> T) -> T {
        struct Done;
        impl Drop for Done {
            fn drop(&mut self) {
                BASE_ASK.with(|b| b.set(false));
            }
        }
        BASE_ASK.with(|b| b.set(true));
        let _d = Done;
        f()
    }

    /// PoolAlloc's own claims go past the pool's claim cap: a global allocation has no refusal to take (mimalloc has no
    /// cap either), so its bytes reach the ledger's pool door and the ladder acts on them. Other takes keep the cap.
    pub(crate) struct Uncapped;

    impl Uncapped {
        #[inline]
        pub(crate) fn new() -> Self {
            UNCAPPED.with(|c| c.set(c.get() + 1));
            Uncapped
        }
    }

    impl Drop for Uncapped {
        #[inline]
        fn drop(&mut self) {
            UNCAPPED.with(|c| c.set(c.get() - 1));
        }
    }

    pub(crate) fn uncapped() -> bool {
        UNCAPPED.with(|c| c.get() > 0)
    }

    /// A block in the reservation that `PoolAlloc` handed out: a small-tier block, or a bigger one under its tags.
    pub(crate) fn is_global_block(p: *const u8) -> bool {
        small::is_small(p) || matches!(run::lookup(p), Some(i) if i.tag == Tag::HEAP || i.tag == Tag::CACHE)
    }

    /// The pool is up: on already, else started here once with the memory policy's sizes. False on the thread that is
    /// starting it (its own allocations meanwhile are the system's) and where it cannot start.
    #[inline]
    pub(crate) fn ready() -> bool {
        crate::on() || start()
    }

    #[cold]
    fn start() -> bool {
        crate::GLOBAL_ON.store(true, Relaxed);
        if TRIED.load(Acquire) == 2 || STARTING.with(Cell::get) {
            return crate::on();
        }
        STARTING.with(|s| s.set(true));
        // the server applies PGRUST_MEMPOLICY after its first block: the pool's knobs from it first (the same values then)
        if let Some(p) = crate::os::env_str(c"PGRUST_MEMPOLICY") {
            #[cfg(not(loom))]
            let _ = mempolicy::consts::apply(p);
        }
        let ok = crate::init(&crate::Config::from_policy());
        STARTING.with(|s| s.set(false));
        TRIED.store(2, Release);
        ok
    }

    /// The road a size takes off the small tier: a bucket, a run of 64 KiB runs, or a span.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Road {
        Bucket(u8),
        Runs,
        Span,
    }

    #[inline]
    fn road(size: usize, align: usize) -> Road {
        match run::bucket_of(size, align) {
            Some(b) => Road::Bucket(b),
            None if size.div_ceil(RUN) <= MAX_RUNS as usize => Road::Runs,
            None => Road::Span,
        }
    }

    /// A block past the small tier back: the pool's to its shelf, the system's to the system.
    #[inline(never)]
    unsafe fn big_free(p: *mut u8, l: Layout) {
        // SAFETY: the caller's: a live big block of this layout.
        unsafe {
            if crate::owns(p) {
                if CACHE_USED.load(Relaxed) {
                    api::give_unsized(NonNull::new_unchecked(p));
                } else {
                    api::give_hinted(NonNull::new_unchecked(p), l.size(), l.align(), BIG);
                }
            } else {
                System.dealloc(p, l);
            }
        }
    }

    #[inline(never)]
    fn big(l: Layout, zero: bool) -> *mut u8 {
        if zero && l.size() >= crate::superpage::SUPER && BASE_ASK.with(Cell::get) {
            return base_big(l, BIG);
        }
        let h = match (zero, l.size() >= crate::superpage::SUPER) {
            (false, _) => BIG,
            (true, true) => BIG.zero(Zero::Sparse),
            (true, false) => BIG.zero(Zero::Dense),
        };
        let _u = Uncapped::new();
        if let Some(s) = api::take(l.size(), l.align(), h) {
            return s.as_ptr() as *mut u8;
        }
        big_slow(l, h)
    }

    /// C.23: a base span's block, else (no address space for one) the zeroed span with its whole pages given back here.
    #[cold]
    fn base_big(l: Layout, h: Hint) -> *mut u8 {
        if hatch() {
            // SAFETY: the caller's layout.
            return unsafe { Hatch.alloc_zeroed(l) };
        }
        let h = h.zero(Zero::Sparse);
        let _u = Uncapped::new();
        if let Some(s) = api::take_base(l.size(), l.align(), h) {
            return s.as_ptr() as *mut u8;
        }
        let Some(s) = api::take(l.size(), l.align(), h) else { return os_big(l, true) };
        let page = crate::os::page_size();
        // SAFETY: whole pages of the zeroed span just taken, nothing in them yet.
        unsafe { crate::os::release_base(s.as_ptr() as *mut u8 as usize, l.size() / page * page) };
        s.as_ptr() as *mut u8
    }

    /// D4880: a big block the ready pool refused comes from the system, as mimalloc maps a huge block; null only when the
    /// system refuses too. A global take is uncapped, so the pool refuses one only for want of address space: its lazy zone
    /// full of spans (Gravity's regions reserve their growth there, 4 GiB at a time, while little memory is claimed) or
    /// the OS's map. `big_free` and `pool_realloc` send a block `owns` refuses back to the system; door 1 charges it by its
    /// layout as every block. Counted (`os_big`).
    #[cold]
    #[inline(never)]
    fn os_big(l: Layout, zero: bool) -> *mut u8 {
        // SAFETY: the caller's layout, not zero-sized (a big one).
        let p = unsafe { if zero { System.alloc_zeroed(l) } else { System.alloc(l) } };
        if !p.is_null() {
            crate::stats::note_os_big(l.size());
        }
        p
    }

    #[cold]
    fn big_slow(l: Layout, h: Hint) -> *mut u8 {
        // pphatch: decides on the process's first big take (api::take misses while no pool is up)
        if hatch() {
            // SAFETY: the caller's layout.
            return unsafe { if h.zero == Zero::No { Hatch.alloc(l) } else { Hatch.alloc_zeroed(l) } };
        }
        if ready() {
            if let Some(s) = api::take(l.size(), l.align(), h) {
                return s.as_ptr() as *mut u8;
            }
            if l.align() <= crate::superpage::SUPER {
                return os_big(l, h.zero != Zero::No);
            }
        }
        // SAFETY: the caller's layout; `dealloc` sends a block `owns` refuses back here.
        unsafe { if h.zero == Zero::Dense { System.alloc_zeroed(l) } else { System.alloc(l) } }
    }

    // SAFETY: every block handed out is the small tier's, the pool's or the system's, and each goes back where it came
    // from: `owns` separates the system's, and the layout (the same at alloc and dealloc) separates the tier's.
    // pphatch: or every block mimalloc's, `MODE` fixed before the first.
    unsafe impl GlobalAlloc for PoolAlloc {
        // pphatch: no mode test on a take: under the hatch the pool's own misses (a thread heap left NEW, no pool for
        // api::take) end in the small tier's / big_slow's / base_big's forward to mimalloc
        #[inline]
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            pool_alloc(l)
        }

        #[inline]
        unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
            // SAFETY: the caller's: `p` is a live block of this layout.
            unsafe { if served() { pool_dealloc(p, l) } else { off_dealloc(p, l) } }
        }

        #[inline]
        unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
            pool_alloc_zeroed(l)
        }

        unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
            // SAFETY: the caller's: a live block of this layout, `new` a valid size.
            unsafe { if served() { pool_realloc(p, l, new) } else { off_realloc(p, l, new) } }
        }
    }

    impl PoolAlloc {
        /// pphatch: `dealloc` without the mode test, for a caller that knows the pool serves (`init_small`'s Rescue: it
        /// installs the cache-class test only then).
        ///
        /// # Safety
        /// As `GlobalAlloc::dealloc`, and the pool serves this process.
        #[inline]
        pub unsafe fn dealloc_served(&self, p: *mut u8, l: Layout) {
            debug_assert!(served());
            // SAFETY: the caller's: `p` is a live block of this layout, the pool's road's.
            unsafe { pool_dealloc(p, l) }
        }

        /// pphatch: `realloc` without the mode test (as `dealloc_served`).
        ///
        /// # Safety
        /// As `GlobalAlloc::realloc`, and the pool serves this process.
        pub unsafe fn realloc_served(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
            debug_assert!(served());
            // SAFETY: the caller's: a live block of this layout, the pool's road's, `new` a valid size.
            unsafe { pool_realloc(p, l, new) }
        }
    }

    /// The pool's road for each method (the methods' bodies before the hatch).
    #[inline(always)]
    fn pool_alloc(l: Layout) -> *mut u8 {
        if is_small(l.size(), l.align()) { small::alloc(l.size(), l.align()) } else { big(l, false) }
    }

    #[inline(always)]
    unsafe fn pool_dealloc(p: *mut u8, l: Layout) {
        // SAFETY: the caller's: `p` is a live pool / system block of this layout.
        unsafe {
            if is_small(l.size(), l.align()) {
                small::free(p);
            } else {
                big_free(p, l);
            }
        }
    }

    #[inline(always)]
    fn pool_alloc_zeroed(l: Layout) -> *mut u8 {
        if is_small(l.size(), l.align()) {
            let p = small::alloc(l.size(), l.align());
            if !p.is_null() {
                // SAFETY: a fresh block of at least `l.size()` bytes.
                unsafe { core::ptr::write_bytes(p, 0, l.size()) };
            }
            p
        } else {
            big(l, true)
        }
    }

    #[inline(always)]
    unsafe fn pool_realloc(p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let (old, align) = (l.size(), l.align());
        if !is_small(old, align) && !is_small(new, align) && !crate::owns(p) {
            // SAFETY: a big system block stays one (a small layout never names a system block).
            return unsafe { System.realloc(p, l, new) };
        }
        if is_small(old, align) {
            // in place within the class only, so `good_size` of the layout stays the block's size (door 1 exact)
            if is_small(new, align) && small::usable(new, align) == small::usable(old, align) {
                return p;
            }
        } else if !is_small(new, align) && crate::owns(p) {
            let (a, b) = (road(old, align), road(new, align));
            if a == b && api::usable(old, align) == api::usable(new, align) {
                return p;
            }
            if a == b && !matches!(a, Road::Bucket(_)) {
                let _u = Uncapped::new();
                // SAFETY: a live pool block of `old` bytes, ours; on Some `new` bytes, maybe moved (a lazy span, D4722)
                if let Some(q) = unsafe { api::resize(NonNull::new_unchecked(p), old, new, align) } {
                    return q.as_ptr() as *mut u8;
                }
            }
        }
        // SAFETY: a valid layout of `new` bytes (the caller's contract), then the copy and the old block's give.
        unsafe {
            let q = pool_alloc(Layout::from_size_align_unchecked(new, align));
            if !q.is_null() {
                core::ptr::copy_nonoverlapping(p, q, old.min(new));
                pool_dealloc(p, l);
            }
            q
        }
    }

    /// A cache-class block (`set_cache_alloc`'s shape): cache-only runs up to 1 KiB, else tagged CACHE; `good_size`d.
    pub fn cache_alloc(size: usize, align: usize, zeroed: bool) -> *mut u8 {
        if !pool_mode() {
            return core::ptr::null_mut();
        }
        if !CACHE_USED.load(Relaxed) {
            CACHE_USED.store(true, Relaxed);
        }
        if is_small(size, align) {
            let p = small::cache_alloc(size, align);
            if zeroed && !p.is_null() {
                // SAFETY: a fresh block of at least `size` bytes.
                unsafe { core::ptr::write_bytes(p, 0, size) };
            }
            return p;
        }
        if !ready() {
            return core::ptr::null_mut();
        }
        if zeroed && size >= crate::superpage::SUPER && BASE_ASK.with(Cell::get) {
            if let Ok(l) = Layout::from_size_align(size, align) {
                return base_big(l, CACHE_BIG);
            }
        }
        let h = if zeroed { CACHE_BIG.zero(Zero::Dense) } else { CACHE_BIG };
        let _u = Uncapped::new();
        api::take(size, align, h).map_or(core::ptr::null_mut(), |s| s.as_ptr() as *mut u8)
    }

    /// `set_cache_alloc`'s realloc: a cache block of `new` bytes, in place at the same size. Null: left as it was.
    pub fn cache_realloc(p: *mut u8, align: usize, new: usize) -> *mut u8 {
        if !crate::owns(p) || new == 0 {
            return core::ptr::null_mut();
        }
        let old = usable_size(p);
        let small_now = small::is_small(p);
        if old == good_size(new, align) && p as usize % align == 0 {
            // SAFETY: a live block of the tier when `small_now`.
            let cache_now = if small_now { unsafe { small::is_cache_block(p) } } else { matches!(run::lookup(p), Some(i) if i.tag == Tag::CACHE) };
            if cache_now {
                return p;
            }
        }
        let q = cache_alloc(new, align, false);
        if !q.is_null() {
            // SAFETY: both live, `old` and `new` bytes; the old block goes back the way its class takes it.
            unsafe {
                core::ptr::copy_nonoverlapping(p, q, old.min(new));
                if small_now {
                    small::free(p);
                } else {
                    api::give_unsized(NonNull::new_unchecked(p));
                }
            }
        }
        q
    }

    /// Whether a live block of this layout is the cache class's (the cache charge's free side).
    #[inline]
    pub fn is_cache(p: *const u8, size: usize, align: usize) -> bool {
        if !CACHE_USED.load(Relaxed) {
            return false;
        }
        if is_small(size, align) {
            // SAFETY: the caller's: a live block of this layout, so of the tier.
            unsafe { small::is_cache_block(p) }
        } else {
            crate::owns(p) && matches!(run::lookup(p), Some(i) if i.tag == Tag::CACHE)
        }
    }

    /// Bytes `PoolAlloc` has handed out and not taken back at `good_size` per block (door 1's bytes plus its rounding).
    pub fn global_live() -> usize {
        let s = crate::stats();
        let big = s.in_use[Tag::HEAP.0 as usize] as isize + s.in_use[Tag::CACHE.0 as usize] as isize - small::cache_run_bytes();
        (small::live_bytes() + big).max(0) as usize
    }

    /// What `PoolAlloc` hands out for this layout (memledger's door 1 charges it): the small class, else the pool's.
    #[inline(always)]
    pub fn good_size(size: usize, align: usize) -> usize {
        if is_small(size, align) { small::usable(size, align) } else { api::usable(size, align) }
    }

    /// A live `PoolAlloc` block's usable size, by pointer (C free, `malloc_usable_size`); 0 for one the pool does not own.
    pub fn usable_size(p: *const u8) -> usize {
        if !crate::owns(p) {
            return 0;
        }
        if small::is_small(p) {
            // SAFETY: a block of the tier (its run carries the SMALL tag).
            return unsafe { small::block_size(p) };
        }
        match run::lookup(p) {
            // ppround (`PGRUST_POOL_FIT_TAIL`): a span's own start, what it handed out (`good_size`): a fitted base span's
            // commit, a missed span less its excess, not its superpages (a copy that long would touch the uncommitted tail)
            Some(i) if p as usize == i.base && matches!(i.kind, run::Kind::Span { .. }) => api::span_handed(i.base, i.bytes),
            Some(i) => i.bytes - (p as usize - i.base),
            None => 0,
        }
    }

    /// Give back what this thread holds idle (runs with every block home, its spare). The runs given back. Dead threads'
    /// runs are the purger's (`flush_remote`), off a session's end.
    pub fn release_heap() -> usize {
        if !crate::on() {
            return 0;
        }
        small::trim()
    }

    /// This thread's retained heap: its small runs' bytes past their live blocks and its shelf's free bytes (for the
    /// broker's park-or-exit test at a session's end; walks the runs, never per statement).
    pub fn thread_retained() -> usize {
        small::thread_retained() + crate::shelf::mine()
    }

    pub use crate::small::SmallCounts;

    /// The small tier's run counts.
    pub fn small_counts() -> SmallCounts {
        small::counts()
    }

    // pphatch: the hatch's allocator: mimalloc, the D bin's; System under cfg(pagepool_tsan) (scripts/pagepool-tsan.sh),
    // where pagepool links no mimalloc (its malloc override would take malloc from TSan's interceptor)
    #[cfg(not(pagepool_tsan))]
    use mimalloc::MiMalloc as Hatch;
    #[cfg(pagepool_tsan)]
    use std::alloc::System as Hatch;
}
