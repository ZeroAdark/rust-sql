//! The emergency allocation reserve (D2395). Rust's global allocator has no error path: a failed allocation in any
//! std collection calls `handle_alloc_error`, which aborts the process, and under thread-per-backend that ends every
//! session (one failed 512-byte allocation during CREATE / DROP / NOTIFY churn under load took the whole server
//! down). PostgreSQL's palloc raises ERROR 53200 in the one backend instead. Until allocation is fallible at the
//! sites (the fallible-allocation ruling), the server keeps a touched reserve of [`RESERVE_BYTES`]: the global
//! allocator, when the system allocator fails, releases it and retries once; a retry that succeeds is recorded on
//! the allocating thread, which raises PostgreSQL's out-of-memory ERROR at its next CHECK_FOR_INTERRUPTS
//! ([`take_oom_pending`]); the backend main loop re-arms the reserve when it goes idle ([`arm_reserve`]). A failure
//! with the reserve already spent, or one larger than the reserve frees, still aborts.

use std::alloc::{GlobalAlloc, Layout};
use std::alloc::System;
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicPtr, AtomicU64, AtomicUsize, Ordering};

/// The reserve's size: two of mimalloc's 32 MiB segments, so a small allocation that needs a fresh segment succeeds
/// once the reserve is returned to the system.
pub const RESERVE_BYTES: usize = 64 << 20;

static RESERVE: AtomicPtr<u8> = AtomicPtr::new(std::ptr::null_mut());
static RESCUES: AtomicU64 = AtomicU64::new(0);
/// P6 (`pool-global`): whether mimalloc serves the global allocator after all (pphatch, `allow_reserve`), so the reserve
/// is held as without the feature.
#[cfg(feature = "pool-global")]
static RESERVE_ALLOWED: AtomicBool = AtomicBool::new(false);

thread_local! {
    // the size of the allocation the reserve rescued on this thread, 0 = none pending (const: no destructor, so
    // the allocator may read it at any point of a thread's life)
    static OOM_PENDING: Cell<usize> = const { Cell::new(0) };
    // D3243: this thread's LIVE shard (`count_delta`), u8::MAX = not picked yet (const, no destructor, as above)
    static SHARD: Cell<u8> = const { Cell::new(u8::MAX) };
    // pppress: this thread's last `allocated_bytes` reading (`take_last_allocated`; const, no destructor, as above)
    static LAST_ALLOCATED: Cell<Option<usize>> = const { Cell::new(None) };
}

fn layout() -> Layout {
    Layout::from_size_align(RESERVE_BYTES, 4096).expect("reserve layout")
}

/// Take the reserve from the system allocator (glibc: a mapping of its own, unmapped again at release) and touch
/// every page, so that releasing it returns real memory. Cheap when armed. False when the system has no room.
/// P6 (`pool-global`): no reserve is held unless `allow_reserve` ran: PoolAlloc's own claims pass the pool's cap
/// (D4718), so its failure is the address space's, which no lift frees.
pub fn arm_reserve() -> bool {
    #[cfg(feature = "pool-global")]
    if !RESERVE_ALLOWED.load(Ordering::Relaxed) {
        return true;
    }
    if !RESERVE.load(Ordering::Acquire).is_null() {
        return true;
    }
    // SAFETY: a non-zero-size layout; the block is owned by RESERVE until released with the same layout.
    let p = unsafe { System.alloc(layout()) };
    if p.is_null() {
        return false;
    }
    // SAFETY: p is a fresh RESERVE_BYTES block.
    unsafe { std::ptr::write_bytes(p, 0xA5, RESERVE_BYTES) };
    if RESERVE.compare_exchange(std::ptr::null_mut(), p, Ordering::AcqRel, Ordering::Acquire).is_err() {
        // SAFETY: allocated just above with the same layout; another thread armed first.
        unsafe { System.dealloc(p, layout()) };
    }
    true
}

/// pphatch (`pool-global`): mimalloc serves the global allocator (the hatch): `arm_reserve` holds the
/// reserve as it does without the feature. Before the first `arm_reserve`.
#[cfg(feature = "pool-global")]
pub fn allow_reserve() {
    RESERVE_ALLOWED.store(true, Ordering::Relaxed);
}

/// Give the reserve back to the system (the global allocator's failure path). False when it is already spent (or,
/// under `pool-global` without `allow_reserve`, never held). Allocates nothing.
pub fn release_reserve() -> bool {
    let p = RESERVE.swap(std::ptr::null_mut(), Ordering::AcqRel);
    if p.is_null() {
        return false;
    }
    // SAFETY: p came from arm_reserve with this layout and is no longer reachable.
    unsafe { System.dealloc(p, layout()) };
    RESCUES.fetch_add(1, Ordering::Relaxed);
    true
}

/// The global allocator's note that the reserve rescued an allocation of `size` bytes on this thread: the thread's
/// next CHECK_FOR_INTERRUPTS raises out-of-memory. Allocates nothing.
pub fn note_rescued(size: usize) {
    OOM_PENDING.with(|c| c.set(size.max(1)));
    crate::globals::RaiseInterruptPendingNoAlloc();
}

/// The size of a rescued allocation pending on this thread (0 = none), cleared.
pub fn take_oom_pending() -> usize {
    OOM_PENDING.with(|c| c.replace(0))
}

/// How many times the reserve was spent since the server started (a report face).
pub fn rescues() -> u64 {
    RESCUES.load(Ordering::Relaxed)
}

/// D3143 P2 (memory plan step 4): the size at and past which a block's pages are advised huge, 0 = never (the server
/// sets it at start through [`set_huge_advise`]; main_main `heap_thp_policy`). mimalloc advises nothing itself (built
/// `no_thp`: its arena-wide MADV_HUGEPAGE made each idle session's first 64 KiB pages fault whole 2 MiB pages, wio
/// P273), so a large hash table, dictionary image or segment takes huge pages here and a small heap never does.
static HUGE_MIN: AtomicUsize = AtomicUsize::new(0);
/// The huge page (the kernel's PMD) size the advice rounds to, 0 = never.
static HUGE_PMD: AtomicUsize = AtomicUsize::new(0);

/// D3143 P2: blocks of `min` bytes or more advise MADV_HUGEPAGE over their whole `pmd`-aligned pages at allocation
/// and MADV_NOHUGEPAGE at their free (the allocator's later reuse of the range for small blocks stays on base pages);
/// `min` = 0 or a `pmd` that is no huge page turns it off. Set once at start, before the sessions.
pub fn set_huge_advise(min: usize, pmd: usize) {
    let on = min != 0 && pmd.is_power_of_two() && pmd > 4096;
    HUGE_PMD.store(if on { pmd } else { 0 }, Ordering::Relaxed);
    HUGE_MIN.store(if on { min.max(pmd) } else { 0 }, Ordering::Relaxed);
}

/// The size at and past which a block is advised huge (0 = off).
pub fn huge_advise_min() -> usize {
    HUGE_MIN.load(Ordering::Relaxed)
}

#[inline]
fn is_huge(size: usize) -> bool {
    let m = HUGE_MIN.load(Ordering::Relaxed);
    m != 0 && size >= m
}

/// The whole huge pages inside `[p, p + size)` advised huge (`on`) or back to base pages. A syscall, no allocation;
/// errno is kept (a free after the allocator unmapped a moved block's range answers ENOMEM, harmlessly).
#[cold]
#[inline(never)]
fn advise_huge_range(p: *mut u8, size: usize, on: bool) {
    #[cfg(target_os = "linux")]
    {
        extern "C" {
            fn madvise(addr: *mut core::ffi::c_void, len: usize, advice: core::ffi::c_int) -> core::ffi::c_int;
            fn __errno_location() -> *mut core::ffi::c_int;
        }
        const MADV_HUGEPAGE: core::ffi::c_int = 14;
        const MADV_NOHUGEPAGE: core::ffi::c_int = 15;
        let pmd = HUGE_PMD.load(Ordering::Relaxed);
        if pmd == 0 {
            return;
        }
        let lo = (p as usize).next_multiple_of(pmd);
        let hi = (p as usize).saturating_add(size) / pmd * pmd;
        if hi > lo {
            // SAFETY: `[lo, hi)` lies inside the block (page-aligned); the advice never changes its bytes, and errno is
            // the calling thread's own word, restored after.
            unsafe {
                let e = *__errno_location();
                madvise(lo as *mut core::ffi::c_void, hi - lo, if on { MADV_HUGEPAGE } else { MADV_NOHUGEPAGE });
                *__errno_location() = e;
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (p, size, on);
}

// ---- D3243 diagnostic: the bytes the global allocator holds in live blocks ----------------------------------------------
//
// The memory governor and the watchdog's cancel measure RSS, which also holds the allocator's freed-but-kept pages; the
// mimalloc release build keeps no in-use statistic (MI_STAT 0) and its heap walk assumes no other thread runs. So the
// wrapper counts: each block's usable size (the allocator's own, through `count_allocations`' probe; else the layout's)
// added at its allocation and taken at its free, in one of `LIVE_SHARDS` cache-line counters a thread picks once. Off
// unless the binary armed it before the first allocation (PGRUST_ALLOC_COUNT=1): then each call costs one relaxed load.

static COUNT_ON: AtomicBool = AtomicBool::new(false);
static USABLE: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());
const LIVE_SHARDS: usize = 64;

#[repr(align(128))]
struct LiveShard(AtomicIsize);

static LIVE: [LiveShard; LIVE_SHARDS] = [const { LiveShard(AtomicIsize::new(0)) }; LIVE_SHARDS];
static NEXT_SHARD: AtomicUsize = AtomicUsize::new(0);

/// Arm (or disarm) the live-byte count. Arm it before the global allocator's first block (an `.init_array` constructor):
/// a block allocated before and freed after counts negative. `usable` = the allocator's usable size of a live block
/// (mimalloc's `mi_usable_size`), so the count matches what the allocator holds; `None` counts layout sizes.
pub fn count_allocations(on: bool, usable: Option<fn(*mut u8) -> usize>) {
    USABLE.store(usable.map_or(std::ptr::null_mut(), |f| f as *mut ()), Ordering::Relaxed);
    COUNT_ON.store(on, Ordering::Relaxed);
}

/// The bytes in live blocks of the global allocator, `None` when the count is off. Every block the
/// `#[global_allocator]` wrapper (`Rescue`) hands out counts, whichever allocator it wraps: `Rescue<PoolAlloc>` on a
/// pagepool-global bin, so PoolAlloc's blocks are in it too (at their good size).
pub fn allocated_bytes() -> Option<usize> {
    let r = if COUNT_ON.load(Ordering::Relaxed) {
        let sum: isize = LIVE.iter().map(|s| s.0.load(Ordering::Relaxed)).sum();
        Some(sum.max(0) as usize)
    } else {
        None
    };
    LAST_ALLOCATED.with(|c| c.set(r));
    r
}

/// pppress (the memory view's 'mimalloc_live'): this thread's last `allocated_bytes` reading, taken (None after), so a
/// reader that called a sum of it (the governor's `allocator_in_use` probe) shows its front term from the SAME read.
pub fn take_last_allocated() -> Option<usize> {
    LAST_ALLOCATED.with(|c| c.take())
}

#[inline]
fn counting() -> bool {
    COUNT_ON.load(Ordering::Relaxed)
}

/// D4556: door 1 charges each block's rounding (the allocator's usable size less the layout's: the size class's, ~3.4 % of
/// live bytes) to the process's rounding account (`memledger::door_round`), beside the layout bytes on the owner's, so the
/// accounts hold what the allocator holds. Armed with the count's rule (before the first block).
static CHARGE_USABLE: AtomicBool = AtomicBool::new(false);

pub fn charge_usable(on: bool, usable: fn(*mut u8) -> usize) {
    USABLE.store(usable as *mut (), Ordering::Relaxed);
    CHARGE_USABLE.store(on, Ordering::Relaxed);
}

#[inline]
fn charging_usable() -> bool {
    CHARGE_USABLE.load(Ordering::Relaxed)
}

/// P6 (`pagepool::PoolAlloc`, which hands out exactly its size for a layout): door 1's rounding from the layout, set
/// instead of `charge_usable`'s block probe; no block is read. Feature `pool-global` (main_main's `pagepool-global`).
#[cfg(feature = "pool-global")]
static USABLE_LAYOUT: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());

#[cfg(feature = "pool-global")]
pub fn charge_usable_layout(on: bool, usable: fn(usize, usize) -> usize) {
    USABLE_LAYOUT.store(usable as *mut (), Ordering::Relaxed);
    CHARGE_USABLE.store(on, Ordering::Relaxed);
}

/// A live block's rounding (its usable size less the layout's where `charge_usable` armed it, else 0) and the size the
/// count takes (0 while it is off): the block is read once.
#[cfg(not(feature = "pool-global"))]
#[inline]
fn sizes(p: *mut u8, layout_size: usize) -> (isize, usize) {
    match (charging_usable(), counting()) {
        (false, false) => (0, 0),
        (false, true) => (0, block_size(p, layout_size)),
        (true, _) => {
            let u = block_size(p, layout_size);
            (u as isize - layout_size as isize, u)
        }
    }
}

/// P6 (`pool-global`): `sizes` for a layout: the block's size from the layout where `charge_usable_layout` armed it
/// (PoolAlloc hands out exactly its size for a layout, no block read), else the block probe.
#[cfg(feature = "pool-global")]
#[inline]
fn sizes_of(p: *mut u8, layout_size: usize, align: usize) -> (isize, usize) {
    match (charging_usable(), counting()) {
        (false, false) => (0, 0),
        (false, true) => (0, layout_block_size(p, layout_size, align)),
        (true, _) => {
            let u = layout_block_size(p, layout_size, align);
            (u as isize - layout_size as isize, u)
        }
    }
}

/// P6 (`pool-global`): the size a live block of a layout counts for (no allocation, no lock).
#[cfg(feature = "pool-global")]
#[inline]
fn layout_block_size(p: *mut u8, layout_size: usize, align: usize) -> usize {
    let g = USABLE_LAYOUT.load(Ordering::Relaxed);
    if !g.is_null() && !own(p) {
        // SAFETY: only charge_usable_layout stores here, always from fn(usize, usize) -> usize.
        let g: fn(usize, usize) -> usize = unsafe { std::mem::transmute(g) };
        return g(layout_size, align);
    }
    block_size(p, layout_size)
}

/// The size a live block counts for (no allocation, no lock).
#[inline(never)]
fn block_size(p: *mut u8, layout_size: usize) -> usize {
    let f = USABLE.load(Ordering::Relaxed);
    // a cache image's own run is no mimalloc block: its layout size
    if f.is_null() || own(p) {
        return layout_size;
    }
    // SAFETY: only count_allocations stores here, always from fn(*mut u8) -> usize.
    let f: fn(*mut u8) -> usize = unsafe { std::mem::transmute(f) };
    f(p)
}

#[inline(never)]
fn count_delta(delta: isize) {
    let i = SHARD
        .try_with(|s| {
            let v = s.get();
            if v != u8::MAX {
                v
            } else {
                let v = (NEXT_SHARD.fetch_add(1, Ordering::Relaxed) % LIVE_SHARDS) as u8;
                s.set(v);
                v
            }
        })
        .unwrap_or(0);
    LIVE[i as usize].0.fetch_add(delta, Ordering::Relaxed);
}

/// D4321 step 1: the cache arena's heap, installed by the binary that owns the global allocator (`allocarena`): a block
/// asked inside a `memledger::arena::cache_scope` comes from it. (size, align, zeroed) -> block, null when the heap
/// cannot serve it (the global path then serves it).
static CACHE_ALLOC: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());
/// (block, align, new size) -> block, the cache heap's realloc; null leaves the block as it was.
static CACHE_REALLOC: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());

/// Install the cache arena's heap (once, before the cache scopes route: `memledger::arena::set_cache_heap_ready`).
pub fn set_cache_alloc(alloc: fn(usize, usize, bool) -> *mut u8, realloc: fn(*mut u8, usize, usize) -> *mut u8) {
    CACHE_ALLOC.store(alloc as *mut (), Ordering::Release);
    CACHE_REALLOC.store(realloc as *mut (), Ordering::Release);
}

/// LADDER-DESIGN 6a: the cache images' own range (`allocarena::own`): [OWN_LO, OWN_LO + OWN_SPAN), and its free and
/// in-place resize. A block in it is recognized by one range compare before anything else (it is no mimalloc block).
static OWN_LO: AtomicUsize = AtomicUsize::new(0);
static OWN_SPAN: AtomicUsize = AtomicUsize::new(0);
static OWN_FREE: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());
static OWN_RESIZE: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());

/// Install the own range (with `set_cache_alloc`, before the cache scopes route): (base, bytes), the free of one of its
/// blocks (block, layout size) and its in-place resize (block, old size, new size) -> kept in place.
pub fn set_own_pages(range: (usize, usize), free: fn(*mut u8, usize), resize: fn(*mut u8, usize, usize) -> bool) {
    OWN_FREE.store(free as *mut (), Ordering::Release);
    OWN_RESIZE.store(resize as *mut (), Ordering::Release);
    OWN_LO.store(range.0, Ordering::Relaxed);
    OWN_SPAN.store(range.1, Ordering::Release);
}

/// P6 (`pool-global`): whether a live block of a layout is the cache class's, in place of the cache arena's span.
#[cfg(feature = "pool-global")]
static CACHE_TEST: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());

#[cfg(feature = "pool-global")]
pub fn set_cache_test(f: fn(*const u8, usize, usize) -> bool) {
    CACHE_TEST.store(f as *mut (), Ordering::Release);
}

/// P6 (`pool-global`), CACHE-CHARGE: whether a live block is the cache road's (the cache class's test once set, else the
/// cache arena's span).
#[cfg(feature = "pool-global")]
#[inline]
fn in_cache(p: *mut u8, size: usize, align: usize) -> bool {
    let f = CACHE_TEST.load(Ordering::Acquire);
    if !f.is_null() {
        // SAFETY: only set_cache_test stores here, always from this fn type.
        return unsafe { std::mem::transmute::<*mut (), fn(*const u8, usize, usize) -> bool>(f) }(p, size, align);
    }
    memledger::arena::in_cache_span(p)
}

#[inline(always)]
fn own(p: *mut u8) -> bool {
    (p as usize).wrapping_sub(OWN_LO.load(Ordering::Relaxed)) < OWN_SPAN.load(Ordering::Relaxed)
}

#[cold]
#[inline(never)]
fn own_free(p: *mut u8, size: usize) {
    // SAFETY: only set_own_pages stores here (before any block is in the range), always from this fn type.
    let f: fn(*mut u8, usize) = unsafe { std::mem::transmute(OWN_FREE.load(Ordering::Acquire)) };
    f(p, size)
}

#[cold]
#[inline(never)]
fn own_resize(p: *mut u8, old: usize, new: usize) -> bool {
    // SAFETY: as own_free.
    let f: fn(*mut u8, usize, usize) -> bool = unsafe { std::mem::transmute(OWN_RESIZE.load(Ordering::Acquire)) };
    f(p, old, new)
}

#[cold]
#[inline(never)]
fn cache_alloc(l: Layout, zeroed: bool) -> *mut u8 {
    let p = CACHE_ALLOC.load(Ordering::Acquire);
    if p.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: only set_cache_alloc stores here, always from this fn type.
    let f: fn(usize, usize, bool) -> *mut u8 = unsafe { std::mem::transmute(p) };
    f(l.size(), l.align(), zeroed)
}

#[cold]
#[inline(never)]
fn cache_realloc(p: *mut u8, l: Layout, new_size: usize) -> *mut u8 {
    let f = CACHE_REALLOC.load(Ordering::Acquire);
    if f.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: only set_cache_alloc stores here, always from this fn type.
    let f: fn(*mut u8, usize, usize) -> *mut u8 = unsafe { std::mem::transmute(f) };
    f(p, l.align(), new_size)
}

/// The global allocator wrapper (main_main's `#[global_allocator]` sits over mimalloc through it): when the inner
/// allocator fails, the reserve goes back to the system and the allocation is retried once; a retry that succeeds is
/// noted on the allocating thread ([`note_rescued`]). Nothing on that path allocates. G1-A: every block's layout size is
/// door 1 of the memory ledger (`memledger::door_heap`: a thread-local count, folded onto the thread's account). D3143 P2: a block of
/// [`huge_advise_min`] bytes or more is advised huge at its allocation and back to base pages at its free.
pub struct Rescue<A>(pub A);

#[cold]
#[inline(never)]
fn rescued(retry: impl FnOnce() -> *mut u8, size: usize) -> *mut u8 {
    if !release_reserve() {
        return std::ptr::null_mut();
    }
    let p = retry();
    if !p.is_null() {
        note_rescued(size);
    }
    p
}

// SAFETY: every method forwards to the inner allocator under the caller's contract; a failed alloc / realloc
// leaves the caller's state as it was, so the one retry is a fresh call under the same contract.
unsafe impl<A: Inner> GlobalAlloc for Rescue<A> {
    #[inline]
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        // D4321 step 1: inside a cache scope the block comes from the cache arena's heap (else, or when it cannot, as before)
        let mut p = if memledger::arena::in_cache_scope() { cache_alloc(l, false) } else { std::ptr::null_mut() };
        // CACHE-CHARGE: a block of the cache road, taken inside the scope (its holder's, when the scope names one)
        let cached = !p.is_null();
        if p.is_null() {
            // SAFETY: forwarded under the caller's contract.
            p = unsafe { self.0.alloc(l) };
        }
        if p.is_null() {
            // pphatch (pool-global): the rescue out of line (`rescue_take`), so the hit's road holds none of its values
            #[cfg(feature = "pool-global")]
            // SAFETY: as above.
            return unsafe { self.rescue_take(l, false) };
            #[cfg(not(feature = "pool-global"))]
            {
                // SAFETY: as above.
                p = rescued(move || unsafe { self.0.alloc(l) }, l.size());
                // a miss the reserve does not lift ends here, so every road on has its block (no null test after a hit)
                if p.is_null() {
                    return p;
                }
            }
        }
        // pphatch (pool-global): the posts in one place for the hit and the rescue's hit (`took`)
        #[cfg(feature = "pool-global")]
        took(p, l, cached);
        #[cfg(not(feature = "pool-global"))]
        {
            if is_huge(l.size()) {
                advise_huge_range(p, l.size(), true);
            }
            let (round, counted) = sizes(p, l.size());
            memledger::door_heap(l.size() as isize);
            if round != 0 {
                memledger::door_round(round);
            }
            if cached {
                memledger::arena::cache_took(l.size() as isize);
                memledger::arena::road_live(own(p), l.size() as isize);
            }
            if counting() {
                count_delta(counted as isize);
            }
        }
        p
    }
    #[inline]
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let mut p = if memledger::arena::in_cache_scope() { cache_alloc(l, true) } else { std::ptr::null_mut() };
        // CACHE-CHARGE: a block of the cache road, taken inside the scope (its holder's, when the scope names one)
        let cached = !p.is_null();
        if p.is_null() {
            // SAFETY: forwarded under the caller's contract.
            p = unsafe { self.0.alloc_zeroed(l) };
        }
        if p.is_null() {
            // pphatch (pool-global): the rescue out of line (`rescue_take`), so the hit's road holds none of its values
            #[cfg(feature = "pool-global")]
            // SAFETY: as above.
            return unsafe { self.rescue_take(l, true) };
            #[cfg(not(feature = "pool-global"))]
            {
                // SAFETY: as above.
                p = rescued(move || unsafe { self.0.alloc_zeroed(l) }, l.size());
                // a miss the reserve does not lift ends here, so every road on has its block (no null test after a hit)
                if p.is_null() {
                    return p;
                }
            }
        }
        // pphatch (pool-global): the posts in one place for the hit and the rescue's hit (`took`)
        #[cfg(feature = "pool-global")]
        took(p, l, cached);
        #[cfg(not(feature = "pool-global"))]
        {
            if is_huge(l.size()) {
                advise_huge_range(p, l.size(), true);
            }
            let (round, counted) = sizes(p, l.size());
            memledger::door_heap(l.size() as isize);
            if round != 0 {
                memledger::door_round(round);
            }
            if cached {
                memledger::arena::cache_took(l.size() as isize);
                memledger::arena::road_live(own(p), l.size() as isize);
            }
            if counting() {
                count_delta(counted as isize);
            }
        }
        p
    }
    #[inline]
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // LADDER-DESIGN 6a: a cache image's own run (no mimalloc block: no usable-size read, no huge advice)
        if own(p) {
            memledger::door_heap(-(l.size() as isize));
            // CACHE-CHARGE: a cache-road block freed inside a scope (one its build took and lets go)
            if memledger::arena::in_cache_scope() {
                memledger::arena::cache_took(-(l.size() as isize));
            }
            memledger::arena::road_live(true, -(l.size() as isize));
            if counting() {
                count_delta(-(l.size() as isize));
            }
            own_free(p, l.size());
            return;
        }
        if is_huge(l.size()) {
            advise_huge_range(p, l.size(), false);
        }
        #[cfg(not(feature = "pool-global"))]
        let (round, counted) = sizes(p, l.size());
        #[cfg(feature = "pool-global")]
        let (round, counted) = sizes_of(p, l.size(), l.align());
        memledger::door_heap(-(l.size() as isize));
        if round != 0 {
            memledger::door_round(-round);
        }
        // CACHE-CHARGE: as the own range's (the range compare first: most frees are of no cache block)
        #[cfg(not(feature = "pool-global"))]
        let in_road = memledger::arena::in_cache_span(p);
        // pphatch: the same test tells whether the page pool serves (`served`: the free below need not ask again)
        #[cfg(feature = "pool-global")]
        let (in_road, served) = in_cache_road(p, l.size(), l.align());
        if in_road {
            if memledger::arena::in_cache_scope() {
                memledger::arena::cache_took(-(l.size() as isize));
            }
            memledger::arena::road_live(false, -(l.size() as isize));
        }
        if counting() {
            count_delta(-(counted as isize));
        }
        #[cfg(feature = "pool-global")]
        if served {
            // SAFETY: forwarded under the caller's contract; the page pool serves (the cache-class test is installed).
            return unsafe { dealloc_served(&self.0, p, l) };
        }
        // SAFETY: forwarded under the caller's contract.
        unsafe { self.0.dealloc(p, l) }
    }
    #[inline]
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new_size: usize) -> *mut u8 {
        if own(p) {
            // SAFETY: the caller's contract, forwarded.
            return unsafe { self.own_realloc(p, l, new_size) };
        }
        // D3143 P2: the old range back to base pages first (the advice keeps its bytes): moved, the allocator owns it
        // after the call; in place, the new size re-advises below
        if is_huge(l.size()) {
            advise_huge_range(p, l.size(), false);
        }
        // D3243: the old block's size read while it is still live (a moved block is the allocator's after the call)
        #[cfg(not(feature = "pool-global"))]
        let (old_round, old) = sizes(p, l.size());
        #[cfg(feature = "pool-global")]
        let (old_round, old) = sizes_of(p, l.size(), l.align());
        // D4321 step 1: grown inside a cache scope, the block moves to (or stays in) the cache arena's heap
        let scoped = memledger::arena::in_cache_scope();
        // CACHE-CHARGE: whether the old block was the cache road's (read while it is still live; out of a scope too: a
        // cache block grown there may leave the road)
        #[cfg(not(feature = "pool-global"))]
        let was_cached = memledger::arena::in_cache_span(p);
        // pphatch: the same test tells whether the page pool serves (`served`: the resize below need not ask again)
        #[cfg(feature = "pool-global")]
        let (was_cached, served) = in_cache_road(p, l.size(), l.align());
        let mut np = if scoped { cache_realloc(p, l, new_size) } else { std::ptr::null_mut() };
        // pphatch (pool-global): the page pool serving, its resize without the mode test; the rescue out of line
        // (`rescue_resize`: its hit's posts and the failed arm's re-advice there), so the hit's road holds none of its
        // values
        #[cfg(feature = "pool-global")]
        if np.is_null() {
            // SAFETY: forwarded under the caller's contract (`realloc_served` only while the page pool serves: the
            // cache-class test is installed); a failed realloc leaves `p` valid for the retry.
            np = unsafe { if served { realloc_served(&self.0, p, l, new_size) } else { self.0.realloc(p, l, new_size) } };
            if np.is_null() {
                // SAFETY: as above.
                return unsafe { self.rescue_resize(p, l, new_size, Was { round: old_round, counted: old, scoped, cached: was_cached }) };
            }
        }
        #[cfg(not(feature = "pool-global"))]
        if np.is_null() {
            // SAFETY: forwarded under the caller's contract; a failed realloc leaves `p` valid for the retry.
            np = unsafe { self.0.realloc(p, l, new_size) };
        }
        #[cfg(not(feature = "pool-global"))]
        if np.is_null() {
            // SAFETY: as above.
            np = rescued(move || unsafe { self.0.realloc(p, l, new_size) }, new_size);
            if np.is_null() {
                // the realloc failed: `p` is still the caller's block, advised as before (and every road on has its
                // block: no null test after a hit)
                if is_huge(l.size()) {
                    advise_huge_range(p, l.size(), true);
                }
                return np;
            }
        }
        #[cfg(feature = "pool-global")]
        resized(np, l, new_size, Was { round: old_round, counted: old, scoped, cached: was_cached });
        #[cfg(not(feature = "pool-global"))]
        {
            let (new_round, new_counted) = sizes(np, new_size);
            memledger::door_heap(new_size as isize - l.size() as isize);
            if new_round != old_round {
                memledger::door_round(new_round - old_round);
            }
            if scoped || was_cached {
                let now_own = own(np);
                let now = if now_own || memledger::arena::in_cache_span(np) { new_size as isize } else { 0 };
                let was = if was_cached { l.size() as isize } else { 0 };
                if scoped && now != was {
                    memledger::arena::cache_took(now - was);
                }
                if was != 0 {
                    memledger::arena::road_live(false, -was);
                }
                if now != 0 {
                    memledger::arena::road_live(now_own, now);
                }
            }
            if is_huge(new_size) {
                advise_huge_range(np, new_size, true);
            }
            if counting() {
                count_delta(new_counted as isize - old as isize);
            }
        }
        np
    }
}

impl<A: Inner> Rescue<A> {
    /// LADDER-DESIGN 6a: a realloc of a cache image's own run: in place while the run length holds, else a fresh block
    /// (through `alloc`, so a cache scope still routes it), the bytes copied, the run freed.
    #[cold]
    #[inline(never)]
    unsafe fn own_realloc(&self, p: *mut u8, l: Layout, new_size: usize) -> *mut u8 {
        if own_resize(p, l.size(), new_size) {
            memledger::door_heap(new_size as isize - l.size() as isize);
            if memledger::arena::in_cache_scope() {
                memledger::arena::cache_took(new_size as isize - l.size() as isize);
            }
            memledger::arena::road_live(true, new_size as isize - l.size() as isize);
            if counting() {
                count_delta(new_size as isize - l.size() as isize);
            }
            return p;
        }
        // SAFETY: Layout's contract: new_size with the old align is valid (the caller's realloc contract).
        let nl = unsafe { Layout::from_size_align_unchecked(new_size, l.align()) };
        // SAFETY: as GlobalAlloc::alloc.
        let np = unsafe { GlobalAlloc::alloc(self, nl) };
        if !np.is_null() {
            // SAFETY: both blocks live, distinct, at least min(old, new) bytes.
            unsafe { std::ptr::copy_nonoverlapping(p, np, l.size().min(new_size)) };
            // SAFETY: p is the caller's block with layout l.
            unsafe { GlobalAlloc::dealloc(self, p, l) };
        }
        np
    }
}

/// pphatch (pool-global): a take's posts, one copy for the hit and the rescue's hit: the huge advice, door 1's bytes and
/// rounding, the cache road's, the count (`p` a live block of `l`, just taken).
#[cfg(feature = "pool-global")]
#[inline(always)]
fn took(p: *mut u8, l: Layout, cached: bool) {
    if is_huge(l.size()) {
        advise_huge_range(p, l.size(), true);
    }
    let (round, counted) = sizes_of(p, l.size(), l.align());
    memledger::door_heap(l.size() as isize);
    if round != 0 {
        memledger::door_round(round);
    }
    if cached {
        memledger::arena::cache_took(l.size() as isize);
        memledger::arena::road_live(own(p), l.size() as isize);
    }
    if counting() {
        count_delta(counted as isize);
    }
}

/// pphatch (pool-global): what a resize read off the old block while it was live (its rounding and counted size, whether
/// a cache scope asked, whether it was the cache road's).
#[cfg(feature = "pool-global")]
#[derive(Clone, Copy)]
struct Was {
    round: isize,
    counted: usize,
    scoped: bool,
    cached: bool,
}

/// pphatch (pool-global): a resize's posts, one copy for the hit and the rescue's hit (`np` the live block of `new_size`
/// bytes that replaced the caller's block of `l`).
#[cfg(feature = "pool-global")]
#[inline(always)]
fn resized(np: *mut u8, l: Layout, new_size: usize, was: Was) {
    let (new_round, new_counted) = sizes_of(np, new_size, l.align());
    memledger::door_heap(new_size as isize - l.size() as isize);
    if new_round != was.round {
        memledger::door_round(new_round - was.round);
    }
    if was.scoped || was.cached {
        let now_own = own(np);
        let now = if now_own || in_cache(np, new_size, l.align()) { new_size as isize } else { 0 };
        let before = if was.cached { l.size() as isize } else { 0 };
        if was.scoped && now != before {
            memledger::arena::cache_took(now - before);
        }
        if before != 0 {
            memledger::arena::road_live(false, -before);
        }
        if now != 0 {
            memledger::arena::road_live(now_own, now);
        }
    }
    if is_huge(new_size) {
        advise_huge_range(np, new_size, true);
    }
    if counting() {
        count_delta(new_counted as isize - was.counted as isize);
    }
}

/// pphatch (pool-global): the reserve's one retry, out of line; a hit posts as the hit road does.
#[cfg(feature = "pool-global")]
impl<A: GlobalAlloc> Rescue<A> {
    #[cold]
    #[inline(never)]
    unsafe fn rescue_take(&self, l: Layout, zeroed: bool) -> *mut u8 {
        // SAFETY: the caller's layout; a failed take left nothing to undo.
        let p = rescued(move || unsafe { if zeroed { self.0.alloc_zeroed(l) } else { self.0.alloc(l) } }, l.size());
        if !p.is_null() {
            took(p, l, false);
        }
        p
    }

    #[cold]
    #[inline(never)]
    unsafe fn rescue_resize(&self, p: *mut u8, l: Layout, new_size: usize, was: Was) -> *mut u8 {
        // SAFETY: the caller's: `p` is still its live block of `l` (a failed realloc leaves it so).
        let np = rescued(move || unsafe { self.0.realloc(p, l, new_size) }, new_size);
        if np.is_null() {
            // the realloc failed: `p` is still the caller's block, advised as before
            if is_huge(l.size()) {
                advise_huge_range(p, l.size(), true);
            }
        } else {
            resized(np, l, new_size, was);
        }
        np
    }
}

/// The allocator `Rescue` wraps: any `GlobalAlloc` in the default build.
#[cfg(not(feature = "pool-global"))]
pub use std::alloc::GlobalAlloc as Inner;

/// The allocator `Rescue` wraps (pool-global): any `GlobalAlloc` (`'static`, so `as_pool` can tell the page pool's).
/// A blanket impl, so a test that wraps mimalloc builds with this feature on too (main_main's default `pagepool-global`
/// turns it on in every cargo build that includes main_main).
#[cfg(feature = "pool-global")]
pub trait Inner: GlobalAlloc + 'static {}

#[cfg(feature = "pool-global")]
impl<A: GlobalAlloc + 'static> Inner for A {}

/// pphatch: the wrapped allocator as the page pool's, when it is (a type test on constants: it folds away).
#[cfg(all(feature = "pool-global", not(target_family = "wasm")))]
#[inline(always)]
fn as_pool<A: Inner>(a: &A) -> Option<&pagepool::PoolAlloc> {
    if std::any::TypeId::of::<A>() == std::any::TypeId::of::<pagepool::PoolAlloc>() {
        // SAFETY: A is PoolAlloc (the same type).
        Some(unsafe { &*(a as *const A).cast::<pagepool::PoolAlloc>() })
    } else {
        None
    }
}

/// pphatch (pool-global): a free of a block known to be the page pool's: its free without the test of which allocator
/// serves (PGRUST_ALLOCATOR) when `a` is PoolAlloc, else the plain free. Rescue calls it only when the cache-class test
/// is installed (`set_cache_test`, which the server installs only while PoolAlloc serves).
///
/// # Safety
/// As `GlobalAlloc::dealloc`, and the page pool serves this process.
#[cfg(feature = "pool-global")]
#[inline(always)]
unsafe fn dealloc_served<A: Inner>(a: &A, p: *mut u8, l: Layout) {
    #[cfg(not(target_family = "wasm"))]
    if let Some(pool) = as_pool(a) {
        // SAFETY: the caller's, the page pool serving.
        return unsafe { pool.dealloc_served(p, l) };
    }
    // SAFETY: the caller's.
    unsafe { a.dealloc(p, l) }
}

/// pphatch (pool-global): as `dealloc_served`, the resize.
///
/// # Safety
/// As `GlobalAlloc::realloc`, and the page pool serves this process.
#[cfg(feature = "pool-global")]
#[inline(always)]
unsafe fn realloc_served<A: Inner>(a: &A, p: *mut u8, l: Layout, new_size: usize) -> *mut u8 {
    #[cfg(not(target_family = "wasm"))]
    if let Some(pool) = as_pool(a) {
        // SAFETY: the caller's, the page pool serving.
        return unsafe { pool.realloc_served(p, l, new_size) };
    }
    // SAFETY: the caller's.
    unsafe { a.realloc(p, l, new_size) }
}

/// pphatch (pool-global): `in_cache`, and whether the page pool serves (the cache-class test installed).
#[cfg(feature = "pool-global")]
#[inline]
fn in_cache_road(p: *mut u8, size: usize, align: usize) -> (bool, bool) {
    let f = CACHE_TEST.load(Ordering::Acquire);
    if !f.is_null() {
        // SAFETY: only set_cache_test stores here, always from this fn type.
        return (unsafe { std::mem::transmute::<*mut (), fn(*const u8, usize, usize) -> bool>(f) }(p, size, align), true);
    }
    (memledger::arena::in_cache_span(p), false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D3143: the VmFlags of the mapping holding `addr` (/proc/self/smaps), empty when none does.
    #[cfg(target_os = "linux")]
    fn vm_flags(addr: usize) -> String {
        let smaps = std::fs::read_to_string("/proc/self/smaps").unwrap_or_default();
        let mut inside = false;
        for line in smaps.lines() {
            let range = line.split_whitespace().next().unwrap_or("");
            if let Some((lo, hi)) = range.split_once('-').and_then(|(a, b)| Some((usize::from_str_radix(a, 16).ok()?, usize::from_str_radix(b, 16).ok()?))) {
                inside = lo <= addr && addr < hi;
            } else if inside && line.starts_with("VmFlags:") {
                return line.to_string();
            }
        }
        String::new()
    }

    /// D3143 P2: a block at the threshold is advised huge over its whole huge pages and back to base pages at its free;
    /// one under it takes no advice. The test owns the process-wide threshold for its run (no other test sets it).
    #[cfg(target_os = "linux")]
    #[test]
    fn large_blocks_take_huge_page_advice_and_small_ones_none() {
        let pmd = std::fs::read_to_string("/sys/kernel/mm/transparent_hugepage/hpage_pmd_size").ok().and_then(|t| t.trim().parse::<usize>().ok());
        let Some(pmd) = pmd.filter(|p| p.is_power_of_two() && *p > 4096) else { return };
        set_huge_advise(2 * pmd, pmd);
        assert_eq!(huge_advise_min(), 2 * pmd);
        let a = Rescue(System);
        let big = Layout::from_size_align(4 * pmd, 4096).unwrap();
        // SAFETY: a non-zero layout; freed below with the same layout.
        let p = unsafe { a.alloc(big) };
        assert!(!p.is_null());
        let mid = (p as usize).next_multiple_of(pmd);
        assert!(vm_flags(mid).split_whitespace().any(|f| f == "hg"), "advised huge: {}", vm_flags(mid));
        advise_huge_range(p, big.size(), false);
        let f = vm_flags(mid);
        assert!(f.split_whitespace().any(|f| f == "nh") && !f.split_whitespace().any(|f| f == "hg"), "back to base pages: {f}");
        // SAFETY: allocated above with `big`.
        unsafe { a.dealloc(p, big) };
        let small = Layout::from_size_align(64 << 10, 16).unwrap();
        // SAFETY: as above.
        let q = unsafe { a.alloc(small) };
        assert!(!q.is_null());
        assert!(!vm_flags(q as usize).split_whitespace().any(|f| f == "hg"), "a small block takes no advice: {}", vm_flags(q as usize));
        // SAFETY: allocated above with `small`.
        unsafe { a.dealloc(q, small) };
        set_huge_advise(0, pmd);
        assert_eq!(huge_advise_min(), 0);
    }

    /// The system allocator, failing while `fail` says so (as the system does under memory pressure).
    struct Failing {
        fail: std::sync::atomic::AtomicU32,
    }

    // SAFETY: forwards to System under the caller's contract, or fails with null.
    unsafe impl GlobalAlloc for Failing {
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            if self.fail.load(Ordering::Relaxed) > 0 {
                self.fail.fetch_sub(1, Ordering::Relaxed);
                return std::ptr::null_mut();
            }
            // SAFETY: the caller's contract.
            unsafe { System.alloc(l) }
        }
        unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
            // SAFETY: the caller's contract.
            unsafe { System.dealloc(p, l) }
        }
    }

    /// The reserve is process-wide, so one test walks it: arm / release once / re-arm, then the D2395 witness: a
    /// failed allocation under the wrapper spends the reserve, succeeds on the retry and leaves out-of-memory pending
    /// on its thread; with the reserve spent the next failure returns null (the abort the reserve cannot prevent).
    #[test]
    #[cfg(not(feature = "pool-global"))]
    fn the_reserve_rescues_one_failed_allocation_per_arming() {
        assert!(arm_reserve());
        assert!(arm_reserve(), "armed twice is still one reserve");
        assert!(release_reserve());
        assert!(!release_reserve(), "a spent reserve releases nothing");
        assert!(arm_reserve());
        let _ = take_oom_pending();
        let a = Rescue(Failing { fail: std::sync::atomic::AtomicU32::new(1) });
        let l = Layout::from_size_align(512, 8).unwrap();
        let before = rescues();
        // SAFETY: a non-zero layout; freed below with it.
        let p = unsafe { a.alloc(l) };
        assert!(!p.is_null(), "the retry after the release succeeds");
        assert_eq!((take_oom_pending(), rescues() - before), (512, 1), "53200 pending on this thread, the reserve spent");
        // SAFETY: allocated above with l.
        unsafe { a.dealloc(p, l) };
        a.0.fail.store(1, Ordering::Relaxed);
        // SAFETY: a non-zero layout.
        assert!(unsafe { a.alloc(l) }.is_null(), "no reserve left: the failure stands");
        assert_eq!(take_oom_pending(), 0);
        assert!(arm_reserve(), "re-armed when the backend goes idle");
        assert!(release_reserve());
    }

    /// P6 (`pool-global`, D4718): no reserve and no lift, so a failure under the wrapper stands with nothing pending.
    #[test]
    #[cfg(feature = "pool-global")]
    fn under_the_pool_a_failure_stands() {
        assert!(arm_reserve());
        assert!(!release_reserve(), "nothing to release");
        let _ = take_oom_pending();
        let a = Rescue(Failing { fail: std::sync::atomic::AtomicU32::new(1) });
        let l = Layout::from_size_align(512, 8).unwrap();
        // SAFETY: a non-zero layout.
        assert!(unsafe { a.alloc(l) }.is_null());
        assert_eq!(take_oom_pending(), 0);
    }

    /// D3243: armed, the wrapper counts each live block at its usable size (the probe's), a realloc by the difference,
    /// a free back to nothing. Read from the counting thread's own shard: a fresh thread picks one no other test thread
    /// holds (fewer than `LIVE_SHARDS` threads count in this binary), so the other tests' blocks never move it.
    #[test]
    fn armed_the_wrapper_counts_live_blocks_at_their_usable_size() {
        fn usable(_: *mut u8) -> usize {
            USABLE_SEEN.fetch_add(1, Ordering::Relaxed);
            3 << 20
        }
        static USABLE_SEEN: AtomicUsize = AtomicUsize::new(0);
        let _armed = ARMED.lock().unwrap_or_else(|e| e.into_inner());
        count_allocations(true, Some(usable));
        let (own, total) = std::thread::spawn(|| {
            count_delta(0); // picks this thread's shard
            let own = || SHARD.with(|s| LIVE[s.get() as usize].0.load(Ordering::Relaxed));
            let base = own();
            let a = Rescue(System);
            let l = Layout::from_size_align(1 << 20, 16).unwrap();
            // SAFETY: a non-zero layout; reallocated and freed below with it.
            let p = unsafe { a.alloc(l) };
            assert!(!p.is_null());
            let after_alloc = own() - base;
            // SAFETY: `p` is live with `l`; the probe says 3 MiB before and after, so the difference is 0.
            let q = unsafe { a.realloc(p, l, 2 << 20) };
            assert!(!q.is_null());
            let after_realloc = own() - base;
            // SAFETY: `q` is live with the new size.
            unsafe { a.dealloc(q, Layout::from_size_align(2 << 20, 16).unwrap()) };
            ((after_alloc, after_realloc, own() - base), allocated_bytes())
        })
        .join()
        .unwrap();
        count_allocations(false, None);
        assert_eq!(own, (3 << 20, 3 << 20, 0), "alloc, realloc, free");
        assert!(total.is_some(), "armed: the total reads");
        assert!(USABLE_SEEN.load(Ordering::Relaxed) >= 4, "alloc + realloc (old, new) + free asked the probe");
        assert_eq!(allocated_bytes(), None, "disarmed: no figure");
    }

    /// The tests that arm the process-wide probe (`USABLE`) take turns.
    static ARMED: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// D4556: armed, door 1 charges a block's layout bytes to the thread's account as before and its rounding (the probe's
    /// usable size less the layout's) to the thread's rounding count: an alloc by its rounding, a realloc by the new less
    /// the old, a free back to nothing; disarmed, no rounding. On a backend account of its own, folded and read.
    #[cfg(target_os = "linux")]
    #[test]
    fn armed_door_one_charges_the_rounding_apart() {
        extern "C" {
            fn malloc_usable_size(p: *mut core::ffi::c_void) -> usize;
        }
        fn usable(p: *mut u8) -> usize {
            // SAFETY: a live block of the System allocator (glibc malloc / posix_memalign).
            unsafe { malloc_usable_size(p.cast()) }
        }
        let _armed = ARMED.lock().unwrap_or_else(|e| e.into_inner());
        let run = |arm: bool| {
            std::thread::spawn(move || {
                charge_usable(arm, usable);
                memledger::bind_backend(4071, 94071);
                let acct = memledger::Acct::backend(4071);
                let net = || {
                    memledger::fold();
                    memledger::account_row(acct).account_bytes
                };
                let base = net();
                let a = Rescue(System);
                let l = Layout::from_size_align(1000, 16).unwrap();
                let r0 = memledger::round_pending();
                // SAFETY: a non-zero layout; reallocated and freed below with it.
                let p = unsafe { a.alloc(l) };
                assert!(!p.is_null());
                let (u0, rounded_alloc) = (usable(p), memledger::round_pending() - r0);
                let after_alloc = net() - base;
                let r1 = memledger::round_pending();
                // SAFETY: `p` is live with `l`.
                let q = unsafe { a.realloc(p, l, 70_000) };
                assert!(!q.is_null());
                let (u1, rounded_realloc) = (usable(q), memledger::round_pending() - r1);
                let after_realloc = net() - base;
                let r2 = memledger::round_pending();
                // SAFETY: `q` is live with the new size.
                unsafe { a.dealloc(q, Layout::from_size_align(70_000, 16).unwrap()) };
                let rounded_free = memledger::round_pending() - r2;
                let after_free = net() - base;
                charge_usable(false, usable);
                memledger::unbind_backend();
                ((after_alloc, after_realloc, after_free), (rounded_alloc, rounded_realloc, rounded_free), (u0 as i64, u1 as i64))
            })
            .join()
            .unwrap()
        };
        let (acct, rounded, (u0, u1)) = run(true);
        assert!(u0 >= 1000 && u1 >= 70_000, "glibc's usable sizes cover the layouts ({u0}, {u1})");
        assert_eq!(acct, (1000, 70_000, 0), "armed: the owner's account at the layout's size");
        assert_eq!(rounded, (u0 - 1000, (u1 - 70_000) - (u0 - 1000), -(u1 - 70_000)), "armed: the rounding apart");
        let (plain, none, _) = run(false);
        assert_eq!((plain, none), ((1000, 70_000, 0), (0, 0, 0)), "disarmed: no rounding");
    }

    #[test]
    fn a_rescue_is_pending_on_its_thread_until_taken() {
        assert_eq!(take_oom_pending(), 0);
        note_rescued(512);
        assert_eq!(std::thread::spawn(take_oom_pending).join().unwrap(), 0, "another thread has none");
        assert_eq!(take_oom_pending(), 512);
        assert_eq!(take_oom_pending(), 0);
    }
}
