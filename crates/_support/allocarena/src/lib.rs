//! pgrust's own address space for mimalloc v3 (ALLOC-LADDER (c); LADDER-DESIGN sections 5 and 6).
//!
//! At process start, before the first allocation (the binary's `.init_array` constructor), `install` maps address
//! space read-write and MAP_NORESERVE (no page is touched) and hands it to mimalloc with `mi_manage_memory`:
//! - general arenas, `ARENA_BYTES` each (one arena must stay under mimalloc's 16 GiB: at 16 GiB its meta needs 521
//!   slices against a cap of 512), enough for 1.25 x physical memory, which every heap uses before mimalloc reserves an
//!   OS arena of its own;
//! - one EXCLUSIVE arena for the cache images (D4321 step 1). Only the cache heap (`start_cache_heap`,
//!   `mi_heap_new_in_arena`) allocates in it, so an evicted image empties pages no other block shares, and the arena's
//!   purge returns them.
//!
//! Each arena's commit callback is the point where mimalloc takes a slice range into use (`commit`) or purges it
//! (`decommit`). It keeps a bitmap of the arena's committed 64 KiB slices (static, so the count is exact whatever range
//! mimalloc names) and counts the change into `memledger::arena` (`ALLOC_TAKEN` / `CACHE_TAKEN`), which wakes the ladder
//! past its line. It runs inside the allocator on the allocating thread: atomics, `madvise` / `mmap` on the range and at
//! most one futex wake; it never allocates, locks or blocks, and always reports success (refusing would push mimalloc to
//! an uncounted OS arena).
//!
//! A purge (`decommit`) gives the pages back the way the vendored mimalloc's own decommit does (CVE-2026-74674,
//! b08d501734b): a range holding a whole 2 MiB-aligned range is mapped over afresh (MAP_FIXED), smaller ones get
//! MADV_DONTNEED, so no MADV_DONTNEED ever covers a whole 2 MiB-aligned range. The memory stays read-write, so a
//! commit only counts.
//!
//! mimalloc's options set here: `arena_max_object_size` raised to `MAX_OBJECT` (objects past 2 GiB came straight from the
//! OS, uncounted, and the cache heap could not serve them at all).
//!
//! `PGRUST_ALLOC_ARENAS=0` keeps mimalloc's own arenas (today's behavior).

#![cfg_attr(target_family = "wasm", allow(dead_code, unused_imports))]

pub mod own;

use core::ffi::c_void;
use core::cell::Cell;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU64, AtomicUsize, Ordering::{AcqRel, Acquire, Relaxed, Release}};

/// mimalloc v3's arena slice.
pub const SLICE: usize = 64 << 10;
/// One arena's address space.
pub const ARENA_BYTES: usize = 15 << 30;
/// The largest object an arena serves (mimalloc keeps a chunk of an arena for its meta; 1 GiB below the arena is ample).
pub const MAX_OBJECT: usize = ARENA_BYTES - (1 << 30);
const SLICES: usize = ARENA_BYTES / SLICE;
const WORDS: usize = SLICES / 64;
/// General arenas at most (480 GiB of address space); past that mimalloc reserves its own OS arenas (uncounted).
pub const MAX_GENERAL: usize = 32;
const CACHE: usize = MAX_GENERAL;

struct Arena {
    base: AtomicUsize,
    /// mimalloc's id of the arena (null until `manage` hands it over); C4's purger owns the general ones by it
    id: AtomicPtr<c_void>,
    committed: [AtomicU64; WORDS],
}

static ARENAS: [Arena; MAX_GENERAL + 1] = [const {
    Arena { base: AtomicUsize::new(0), id: AtomicPtr::new(core::ptr::null_mut()), committed: [const { AtomicU64::new(0) }; WORDS] }
}; MAX_GENERAL + 1];
static GENERAL: AtomicUsize = AtomicUsize::new(0);
static CACHE_ARENA_ID: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());
static CACHE_HEAP: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());

type CommitFn = unsafe extern "C" fn(bool, *mut c_void, usize, *mut bool, *mut c_void) -> bool;

#[cfg(not(target_family = "wasm"))]
unsafe extern "C" {
    // mimalloc v3.3.2 (mimalloc.h:402, arena.c:1663)
    fn mi_manage_memory(
        start: *mut c_void,
        size: usize,
        is_committed: bool,
        is_pinned: bool,
        is_zero: bool,
        numa_node: i32,
        exclusive: bool,
        commit_fun: Option<CommitFn>,
        commit_fun_arg: *mut c_void,
        arena_id: *mut *mut c_void,
    ) -> bool;
    // mimalloc.h:341
    fn mi_heap_new_in_arena(arena_id: *mut c_void) -> *mut c_void;
    fn mi_heap_malloc_aligned(heap: *mut c_void, size: usize, alignment: usize) -> *mut c_void;
    fn mi_heap_zalloc_aligned(heap: *mut c_void, size: usize, alignment: usize) -> *mut c_void;
    fn mi_heap_realloc_aligned(heap: *mut c_void, p: *mut c_void, newsize: usize, alignment: usize) -> *mut c_void;
    fn mi_heap_collect(heap: *mut c_void, force: bool);
    // pgrust's vendored export (arena.c, C3): purge one arena's freed ranges now; a try-guard (false when another
    // thread is purging)
    fn mi_arena_purge_now(arena_id: *mut c_void, force: bool) -> bool;
    // pgrust's vendored export (arena.c, H3 step (2)): whether this thread's last mi_arena_purge_now returned at that guard
    fn mi_arena_purge_now_busy() -> bool;
    // pgrust's vendored exports (arena.c, C3 purge owned): only mi_arena_purge_now purges the owned arena (a dead-man
    // fallback after 1 s without its owner), and the fallback's purges so far
    fn mi_arena_set_purge_owned(arena_id: *mut c_void, owned: bool);
    fn mi_arena_purge_owned_fallbacks(arena_id: *mut c_void) -> usize;
    fn mi_arena_set_reuse_donor(arena_id: *mut c_void);
    fn mi_arena_reuse_stats(live_bytes: *mut usize, taken: *mut usize, taken_bytes: *mut usize, misses: *mut usize);
    fn mi_arena_reuse_reusable() -> usize;
    fn mi_arena_free_runs(id: *mut c_void, committed: *mut usize, any: *mut usize, nb: usize);
    fn mi_arena_committed_free(id: *mut c_void) -> usize;
    fn mi_arena_reuse_self_stats(taken: *mut usize, taken_bytes: *mut usize);
    fn mi_arena_reuse_cache_self_stats(taken: *mut usize, taken_bytes: *mut usize);
    // pgrust's vendored exports (arena.c, purge after idle): the owner's purge once no slice was claimed from the arena
    // for `idle_ms`, and the bytes purged by the owner vs every other caller
    fn mi_arena_purge_after_idle(arena_id: *mut c_void, idle_ms: core::ffi::c_longlong) -> bool;
    fn mi_arena_claims_noted(arena_id: *mut c_void) -> usize;
    fn mi_arena_purged_bytes(by_owner: *mut usize, by_others: *mut usize);
    // pgrust's vendored export (arena.c, pressure purge): every free and committed run of the arena decommitted now,
    // whatever the purge delay; the bytes
    fn mi_arena_purge_unused(arena_id: *mut c_void, by_owner: bool) -> usize;
    // pgrust's vendored exports (arena.c, purge timing): this thread's phase, and the purge events' time by thread class
    fn mi_arena_set_purge_phase(phase: core::ffi::c_int);
    fn mi_arena_purge_batch(begin: bool, by_owner: bool);
    fn mi_arena_purge_times(out: *mut usize);
}

/// mimalloc v3 `mi_option_arena_max_object_size` (in KiB).
#[cfg(all(target_os = "linux", not(target_family = "wasm")))]
const OPTION_ARENA_MAX_OBJECT_SIZE: libmimalloc_sys::mi_option_t = 45;

/// Install pgrust's arenas. Call once, from the constructor that runs before mimalloc's own and before the first
/// allocation (later works too, for tests: blocks already taken stay in mimalloc's arenas). Allocates nothing. Returns
/// whether the arenas (at least one general one and the cache arena) are in place.
#[cfg(all(target_os = "linux", not(target_family = "wasm")))]
pub fn install() -> bool {
    // SAFETY: getenv with a NUL-terminated literal; read before any thread exists.
    let off = unsafe {
        let v = libc::getenv(c"PGRUST_ALLOC_ARENAS".as_ptr());
        !v.is_null() && core::ffi::CStr::from_ptr(v).to_bytes() == b"0"
    };
    if off || GENERAL.load(Relaxed) != 0 {
        return false;
    }
    // SAFETY: plain sysconf reads.
    let phys = unsafe { (libc::sysconf(libc::_SC_PHYS_PAGES).max(0) as usize).saturating_mul(libc::sysconf(libc::_SC_PAGESIZE).max(0) as usize) };
    let want = phys.saturating_add(phys / 4).div_ceil(ARENA_BYTES).clamp(1, MAX_GENERAL);
    // SAFETY: a plain option store for the linked v3 (before mimalloc reads it: the option is read at each large alloc).
    unsafe { libmimalloc_sys::mi_option_set(OPTION_ARENA_MAX_OBJECT_SIZE, (MAX_OBJECT >> 10) as _) };
    let mut n = 0;
    while n < want && manage(n, false).is_some() {
        n += 1;
    }
    GENERAL.store(n, Relaxed);
    if n == 0 {
        return false;
    }
    let Some(id) = manage(CACHE, true) else { return false };
    CACHE_ARENA_ID.store(id, Relaxed);
    // CACHE-CHARGE: the global allocator's free inside a holder scope tells a cache block by this range
    memledger::arena::set_cache_span(ARENAS[CACHE].base.load(Relaxed), ARENA_BYTES);
    // LADDER-DESIGN 6a: the 64-512 KiB images' own range (without it they stay on the cache heap)
    let _ = own::install();
    memledger::arena::set_active();
    true
}

#[cfg(not(all(target_os = "linux", not(target_family = "wasm"))))]
pub fn install() -> bool {
    false
}

/// Map one arena's address space and hand it to mimalloc; its id.
#[cfg(all(target_os = "linux", not(target_family = "wasm")))]
fn manage(i: usize, exclusive: bool) -> Option<*mut c_void> {
    // SAFETY: a fresh private anonymous mapping; nothing else names it.
    let raw = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            ARENA_BYTES + SLICE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };
    if raw == libc::MAP_FAILED {
        return None;
    }
    let start = (raw as usize).next_multiple_of(SLICE);
    name(start, ARENA_BYTES);
    ARENAS[i].base.store(start, Relaxed);
    let mut id: *mut c_void = core::ptr::null_mut();
    // SAFETY: `start` is SLICE-aligned and ARENA_BYTES of it are ours for the process's life; the callback's argument is
    // the arena's index into ARENAS.
    let ok = unsafe {
        mi_manage_memory(start as *mut c_void, ARENA_BYTES, false, false, true, -1, exclusive, Some(commit), i as *mut c_void, &mut id)
    };
    if !ok {
        ARENAS[i].base.store(0, Relaxed);
        // SAFETY: the mapping above, unused.
        unsafe { libc::munmap(raw, ARENA_BYTES + SLICE) };
        return None;
    }
    ARENAS[i].id.store(id, Relaxed);
    Some(id)
}

/// The mapping's name: mimalloc's own, so the allocator's resident read (`[anon:mimalloc]`) covers these arenas too.
#[cfg(all(target_os = "linux", not(target_family = "wasm")))]
fn name(start: usize, len: usize) {
    const PR_SET_VMA: libc::c_int = 0x53564d41;
    const PR_SET_VMA_ANON_NAME: libc::c_ulong = 0;
    // SAFETY: names our own mapping; a kernel without the call refuses it harmlessly.
    unsafe { libc::prctl(PR_SET_VMA, PR_SET_VMA_ANON_NAME, start as libc::c_ulong, len as libc::c_ulong, c"mimalloc".as_ptr()) };
}

/// The arenas' commit callback (`mi_commit_fun_t`). Allocator context: see the module doc.
#[cfg(all(target_os = "linux", not(target_family = "wasm")))]
unsafe extern "C" fn commit(commit: bool, start: *mut c_void, size: usize, is_zero: *mut bool, arg: *mut c_void) -> bool {
    let i = arg as usize;
    let Some(a) = ARENAS.get(i) else { return true };
    let base = a.base.load(Relaxed);
    let lo = (start as usize).saturating_sub(base).min(ARENA_BYTES);
    let hi = lo.saturating_add(size).min(ARENA_BYTES);
    if commit {
        let (newly, all) = mark(a, lo / SLICE, hi.div_ceil(SLICE), true);
        if !is_zero.is_null() {
            // fresh, or every slice purged by `decommit` below (which zeroes): reads zero
            // SAFETY: mimalloc's out-parameter, non-null here.
            unsafe { *is_zero = newly == all };
        }
        memledger::arena::note_taken(i == CACHE, (newly * SLICE) as i64);
        FRESH[usize::from(i == CACHE)].fetch_add((newly * SLICE) as u64, Relaxed);
    } else {
        decommit(start as usize, size);
        // only the slices the range covers whole are no longer committed
        let (gone, _) = mark(a, lo.div_ceil(SLICE), hi / SLICE, false);
        memledger::arena::note_taken(i == CACHE, -((gone * SLICE) as i64));
    }
    true
}

/// REUSE (the C4 gate): bytes the commit callback committed afresh (slices not committed before: page faults to come),
/// cumulative, [general arenas, cache arena].
static FRESH: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Set (or clear) the committed bits of slices `[s0, s1)`; (bits that changed, bits in the range).
fn mark(a: &Arena, s0: usize, s1: usize, set: bool) -> (usize, usize) {
    let s1 = s1.min(SLICES);
    if s0 >= s1 {
        return (0, 0);
    }
    let mut changed = 0usize;
    let mut s = s0;
    while s < s1 {
        let w = s / 64;
        let end = ((w + 1) * 64).min(s1);
        let bits = end - s;
        let mask = if bits == 64 { u64::MAX } else { ((1u64 << bits) - 1) << (s % 64) };
        let prev = if set { a.committed[w].fetch_or(mask, Relaxed) } else { a.committed[w].fetch_and(!mask, Relaxed) };
        changed += if set { (mask & !prev).count_ones() } else { (mask & prev).count_ones() } as usize;
        s = end;
    }
    (changed, s1 - s0)
}

/// Give a purged range's pages back (the vendored decommit's way, CVE-2026-74674): a range holding a whole 2 MiB-aligned
/// range is mapped over afresh; otherwise MADV_DONTNEED, in pieces that never cover a whole 2 MiB-aligned range.
#[cfg(all(target_os = "linux", not(target_family = "wasm")))]
fn decommit(start: usize, size: usize) {
    const PMD: usize = 2 << 20;
    let end = start + size;
    let first = start.next_multiple_of(PMD);
    DECOMMITS.fetch_add(1, Relaxed);
    // H3 step (2): this thread's count (a plain thread-local: no shared atomic, no destructor, nothing allocated)
    let _ = DECOMMITS_HERE.try_with(|c| {
        let [n, b] = c.get();
        c.set([n + 1, b + size as u64]);
    });
    if first + PMD <= end {
        // SAFETY: the range is inside one of our arenas and free (mimalloc purges only free slices).
        let p = unsafe {
            libc::mmap(
                start as *mut c_void,
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if p as usize == start {
            name(start, size);
            REMAPS.fetch_add(1, Relaxed);
            return;
        }
    }
    let mut cur = start;
    while cur < end {
        let p = cur.next_multiple_of(PMD);
        let stop = if p <= end && end - p >= PMD { p + PMD / 2 } else { end };
        // the CVE rule's witness: a piece that covers a whole 2 MiB-aligned range (never, by the split above)
        if cur.next_multiple_of(PMD) + PMD <= stop {
            WHOLE_PMD_DONTNEEDS.fetch_add(1, Relaxed);
        }
        // SAFETY: as above.
        unsafe { libc::madvise(cur as *mut c_void, stop - cur, libc::MADV_DONTNEED) };
        cur = stop;
    }
}

/// The decommit's counts (tests, diagnostics): decommits, ranges mapped over afresh (MAP_FIXED), and MADV_DONTNEED
/// pieces that covered a whole 2 MiB-aligned range (the CVE-2026-74674 rule: always 0).
static DECOMMITS: AtomicU64 = AtomicU64::new(0);
static REMAPS: AtomicU64 = AtomicU64::new(0);
static WHOLE_PMD_DONTNEEDS: AtomicU64 = AtomicU64::new(0);

/// `(decommits, MAP_FIXED remaps, whole-2-MiB MADV_DONTNEED pieces)` so far.
pub fn decommit_counts() -> (u64, u64, u64) {
    (DECOMMITS.load(Relaxed), REMAPS.load(Relaxed), WHOLE_PMD_DONTNEEDS.load(Relaxed))
}

std::thread_local! {
    /// H3 step (2): this thread's decommits through the arenas' commit callback, and their bytes. Const-initialized, of a
    /// type without Drop: no TLS destructor is registered and nothing allocates inside the callback. Read on the thread
    /// itself (the born-red tests; the give-back service publishes its own from its loop, outside the allocator).
    static DECOMMITS_HERE: Cell<[u64; 2]> = const { Cell::new([0; 2]) };
}

/// H3 step (2): `(decommits, bytes)` the calling thread made through the arenas' commit callback so far.
pub fn thread_decommits() -> (u64, u64) {
    let [n, b] = DECOMMITS_HERE.try_with(Cell::get).unwrap_or([0; 2]);
    (n, b)
}

/// Create the cache arena's heap (once, before any session; after the global allocator's cache hooks point at
/// `cache_alloc` / `cache_realloc`), then turn the cache scopes on (`memledger::arena::set_cache_heap_ready`).
/// Whether the scopes now route to it.
pub fn start_cache_heap() -> bool {
    #[cfg(not(target_family = "wasm"))]
    {
        let id = CACHE_ARENA_ID.load(Relaxed);
        if id.is_null() || !CACHE_HEAP.load(Relaxed).is_null() {
            return !CACHE_HEAP.load(Relaxed).is_null();
        }
        // SAFETY: the cache arena's id from `install`.
        let heap = unsafe { mi_heap_new_in_arena(id) };
        if heap.is_null() {
            return false;
        }
        CACHE_HEAP.store(heap, Relaxed);
        memledger::arena::set_cache_heap_ready();
        true
    }
    #[cfg(target_family = "wasm")]
    false
}

/// The global allocator's cache path (`init_small::oom::set_cache_alloc`): a block of the cache heap, or null (the
/// global path serves it: no heap yet, or the cache arena is full).
pub fn cache_alloc(size: usize, align: usize, zeroed: bool) -> *mut u8 {
    #[cfg(not(target_family = "wasm"))]
    {
        // 64-512 KiB: a run of its own (already zero), else the cache heap below
        if let Some(k) = own::run_of(size, align) {
            let p = own::alloc(k, size, zeroed);
            if !p.is_null() {
                return p;
            }
        }
        let heap = CACHE_HEAP.load(Relaxed);
        if heap.is_null() {
            return core::ptr::null_mut();
        }
        // SAFETY: a live heap; mimalloc's contract (align a power of two, as Layout guarantees).
        (unsafe { if zeroed { mi_heap_zalloc_aligned(heap, size, align) } else { mi_heap_malloc_aligned(heap, size, align) } }) as *mut u8
    }
    #[cfg(target_family = "wasm")]
    {
        let _ = (size, align, zeroed);
        core::ptr::null_mut()
    }
}

/// The cache path's realloc: `p` moved into (or grown within) the cache heap, or null with `p` untouched.
pub fn cache_realloc(p: *mut u8, align: usize, new_size: usize) -> *mut u8 {
    #[cfg(not(target_family = "wasm"))]
    {
        let heap = CACHE_HEAP.load(Relaxed);
        if heap.is_null() {
            return core::ptr::null_mut();
        }
        // 64-512 KiB: moved to a run of its own (the block grew into an image's size inside the scope)
        if let Some(k) = own::run_of(new_size, align) {
            let np = own::alloc(k, new_size, false);
            if !np.is_null() {
                // SAFETY: `p` is a live mimalloc block of at least its usable size; `np` a fresh run of new_size bytes.
                unsafe {
                    let old = libmimalloc_sys::mi_usable_size(p as *const c_void);
                    core::ptr::copy_nonoverlapping(p, np, old.min(new_size));
                    libmimalloc_sys::mi_free(p as *mut c_void);
                }
                return np;
            }
        }
        // SAFETY: `p` is a live mimalloc block (the global allocator's); a null return leaves it valid.
        (unsafe { mi_heap_realloc_aligned(heap, p as *mut c_void, new_size, align) }) as *mut u8
    }
    #[cfg(target_family = "wasm")]
    {
        let _ = (p, align, new_size);
        core::ptr::null_mut()
    }
}

/// The global allocator's free of a block in the own range (`init_small::oom::set_own_pages`): `size` is its layout
/// size, which named its run.
pub fn own_free(p: *mut u8, size: usize) {
    own::free(p, size.div_ceil(SLICE));
}

/// The global allocator's in-place resize of a block in the own range: whether `new_size` keeps its run (same run
/// length, still served here). Otherwise the caller moves the block.
pub fn own_resize(_p: *mut u8, old_size: usize, new_size: usize) -> bool {
    own::run_of(new_size, 1).is_some_and(|k| k == old_size.div_ceil(SLICE))
}

/// Collect this thread's part of the cache heap and purge every arena now (R2's purge step). In mimalloc v3 a remote
/// free of an abandoned page's last block frees the page to its arena at once (free.c `mi_free_try_collect_mt`); this
/// call then purges it (`mi_arenas_try_purge` forced over every arena, the exclusive one too). A page a live thread
/// still owns is freed only by that thread's own collect.
pub fn collect_cache() {
    #[cfg(not(target_family = "wasm"))]
    {
        let heap = CACHE_HEAP.load(Relaxed);
        if !heap.is_null() {
            // SAFETY: a live heap.
            unsafe { mi_heap_collect(heap, true) };
        }
    }
}

/// This thread's part of the cache heap collected, without the arena purge (a thread answering the ladder's or a
/// give-back's collect ask: mimalloc v3 frees a page of a live thread's heap only on that thread).
pub fn collect_cache_local() {
    #[cfg(not(target_family = "wasm"))]
    {
        let heap = CACHE_HEAP.load(Relaxed);
        if !heap.is_null() {
            // SAFETY: a live heap.
            unsafe { mi_heap_collect(heap, false) };
        }
    }
}

/// A cache give-back's allocator step on the calling thread: the own range's hot runs given back and this thread's
/// part of the cache heap collected with a purge. The owners' parts follow at their next idle point
/// (`mcx::give_back_caches` asks them). The own bytes returned.
pub fn give_back() -> usize {
    let n = own::flush();
    collect_cache();
    // H3 step (2): the give-back service's arenas, which the collect skipped
    forced_purge_job();
    n
}

/// The allocator release (`mcx::release_retained`'s hook, which the binary installs): this thread's part of the cache
/// heap collected and every arena purged (`mi_collect(true)`), as before. H3 step (2) (`give_back_service`): the arenas the
/// give-back service owns, which those collects skip, get their forced purge as the service's job (`forced_purge_job`).
pub fn release() {
    collect_cache();
    #[cfg(not(target_family = "wasm"))]
    // SAFETY: mimalloc's collect on this thread.
    unsafe {
        libmimalloc_sys::mi_collect(true)
    };
    forced_purge_job();
}

/// C3: purge the cache arena's freed ranges now (`force`: also those whose purge delay has not expired), leaving every
/// general arena on mimalloc's process-wide purge delay. Through the arena's commit callback, so the decommit above
/// (the CVE rule) gives the pages back and `memledger::arena`'s CACHE_TAKEN falls. False when nothing was purged or
/// another thread was purging (mimalloc's try-guard: the caller tries again on its next tick).
pub fn purge_cache_now(force: bool) -> bool {
    #[cfg(not(target_family = "wasm"))]
    {
        let id = CACHE_ARENA_ID.load(Relaxed);
        // H3 step (2): never an arena the give-back service owns (owner 1's entry point)
        // SAFETY: the cache arena's id from `install` (null: not installed, nothing to purge).
        !id.is_null() && not_the_others(CACHE, OWNER_PURGER) && unsafe { mi_arena_purge_now(id, force) }
    }
    #[cfg(target_family = "wasm")]
    {
        let _ = force;
        false
    }
}

/// C3 (cache_purge_now, purge owned): the cache arena's purge becomes the purger's alone (`on`): every other
/// purge skips it (mi_collect(true) and mi_heap_collect(force) on query threads, the postmaster's release at a connection's
/// exit, the idle releases), unless the purger has not called `purge_cache_now` for 1 s while expired ranges wait (the
/// dead-man fallback, counted by `purge_owned_fallbacks`). `false`: today's behavior. H3 step (2): taken as owner 1 by CAS
/// (`take`) and let go only by it, so an arena the give-back service owns stays the service's (the take is refused and
/// counted); nothing else changes while the service owns nothing (give_back_service = 0).
pub fn set_cache_purge_owned(on: bool) {
    #[cfg(not(target_family = "wasm"))]
    {
        let id = CACHE_ARENA_ID.load(Relaxed);
        if !id.is_null() && (if on { take(CACHE, OWNER_PURGER) } else { give_up(CACHE, OWNER_PURGER) }) {
            // SAFETY: the cache arena's id from `install`.
            unsafe { mi_arena_set_purge_owned(id, on) };
        }
    }
    #[cfg(target_family = "wasm")]
    let _ = on;
}

/// C4 (query_purge_bg): every general arena's purge becomes the purger's (`on`), as the cache arena's
/// under C3: no other thread's collect purges them (a statement's end, a thread's exit, the postmaster's connection exit),
/// mimalloc's dead-man fallback aside. `false`: today's behavior. H3 step (2): per arena by CAS as owner 1, as the cache
/// arena's (`set_cache_purge_owned`).
pub fn set_general_purge_owned(on: bool) {
    #[cfg(not(target_family = "wasm"))]
    for (i, a) in ARENAS[..GENERAL.load(Relaxed)].iter().enumerate() {
        let id = a.id.load(Relaxed);
        if !id.is_null() && (if on { take(i, OWNER_PURGER) } else { give_up(i, OWNER_PURGER) }) {
            // SAFETY: a general arena's id from `manage`.
            unsafe { mi_arena_set_purge_owned(id, on) };
        }
    }
    #[cfg(target_family = "wasm")]
    let _ = on;
}

/// C4: the general arenas' EXPIRED ranges purged now (one `mi_arena_purge_now(id, false)` each: the purge delay kept, so
/// hot pages are not re-faulted). How many arenas purged something.
pub fn purge_general_expired() -> u32 {
    #[cfg(not(target_family = "wasm"))]
    {
        let mut n = 0;
        for (i, a) in ARENAS[..GENERAL.load(Relaxed)].iter().enumerate() {
            let id = a.id.load(Relaxed);
            // H3 step (2): never an arena the give-back service owns (owner 1's entry point)
            // SAFETY: a general arena's id from `manage`.
            if !id.is_null() && not_the_others(i, OWNER_PURGER) && unsafe { mi_arena_purge_now(id, false) } {
                n += 1;
            }
        }
        n
    }
    #[cfg(target_family = "wasm")]
    0
}

/// Purge after idle's input (a diagnostic): the page claims counted so far over every installed arena (the general arenas
/// and the cache arena).
pub fn claims_noted() -> u64 {
    #[cfg(not(target_family = "wasm"))]
    {
        let ids = ARENAS[..GENERAL.load(Relaxed)].iter().map(|a| a.id.load(Relaxed)).chain(core::iter::once(CACHE_ARENA_ID.load(Relaxed)));
        // SAFETY: installed arenas' ids from `manage` (a null id is skipped); the vendored function reads one atomic.
        ids.filter(|id| !id.is_null()).map(|id| unsafe { mi_arena_claims_noted(id) } as u64).sum()
    }
    #[cfg(target_family = "wasm")]
    0
}

/// Purge after idle (the purger's hold, `PGRUST_PURGE_AFTER_IDLE_S` above 1): the owned arenas' expired ranges purged only
/// in an arena that handed out no slice for `idle_ms` (`general`: the general arenas; `cache`: the cache arena); each call
/// also marks their owner alive (no dead-man fallback while it holds). How many arenas purged something.
pub fn purge_after_idle(general: bool, cache: bool, idle_ms: u64) -> u32 {
    #[cfg(not(target_family = "wasm"))]
    {
        let n = if general { GENERAL.load(Relaxed) } else { 0 };
        // H3 step (2): never an arena the give-back service owns (owner 1's entry point)
        let slots = (0..n).chain(cache.then_some(CACHE)).filter(|&i| not_the_others(i, OWNER_PURGER));
        let ms = core::ffi::c_longlong::try_from(idle_ms).unwrap_or(core::ffi::c_longlong::MAX);
        // SAFETY: installed arenas' ids from `manage` (a null id is skipped); the vendored function takes the purge guard.
        slots.map(arena_id).filter(|id| !id.is_null()).map(|id| u32::from(unsafe { mi_arena_purge_after_idle(id, ms) })).sum()
    }
    #[cfg(target_family = "wasm")]
    {
        let _ = (general, cache, idle_ms);
        0
    }
}

/// Ladder pressure's purge: every pgrust arena's free and committed slices decommitted now (the vendored
/// `mi_arena_purge_unused`: through the commit callback, so the ledger's taken bytes fall), whatever mimalloc's purge
/// delay, on the calling thread. `by_purger`: the bytes count as the purger thread's. The bytes decommitted.
pub fn purge_unused(by_purger: bool) -> u64 {
    #[cfg(not(target_family = "wasm"))]
    {
        // H3 step (2): the purger's call (owner 1) never purges an arena the give-back service owns; the ladder's
        // (`by_purger` false) purges every arena whatever its owner, as before (memory's kill safety), its bytes from
        // owned arenas counted apart (`purged_by_pressure`: no double purge, and nothing lost)
        let slots = (0..GENERAL.load(Relaxed)).chain(core::iter::once(CACHE)).filter(|&i| !by_purger || not_the_others(i, OWNER_PURGER));
        // SAFETY: thread-local stores; the purge timing books the whole call as one event.
        unsafe { mi_arena_purge_batch(true, by_purger) };
        let mut n = 0;
        for i in slots {
            let id = arena_id(i);
            if id.is_null() {
                continue;
            }
            // SAFETY: an installed arena's id from `manage`; the vendored function takes the purge guard.
            let b = unsafe { mi_arena_purge_unused(id, by_purger) } as u64;
            if !by_purger && OWNER[i].load(Acquire) != NO_OWNER {
                PRESSURE_OWNED.fetch_add(b, Relaxed);
            }
            n += b;
        }
        // SAFETY: as above.
        unsafe { mi_arena_purge_batch(false, by_purger) };
        n
    }
    #[cfg(target_family = "wasm")]
    {
        let _ = by_purger;
        0
    }
}

/// PRESSURE ONLY (`PGRUST_PURGE_PRESSURE_ONLY=1`, default off): mimalloc never purges on a timer or a collect (its purge
/// delay -1: no deferred purge can land on any thread), so freed pages stay committed for the next allocation and go back
/// to the OS only through `purge_unused` under ladder pressure. Call before the first statement.
pub fn set_purge_pressure_only() {
    #[cfg(not(target_family = "wasm"))]
    // SAFETY: a plain option store (v3 mi_option_purge_delay = 15); mimalloc reads it at every purge decision.
    unsafe {
        libmimalloc_sys::mi_option_set(15, -1)
    };
}

/// Bytes purged so far, `(by the arenas' owner: the purger, by every other thread: a query thread's collect, a thread's
/// exit, the dead-man fallback)`, every arena (the view's purged_by_purger / purged_by_other_threads rows).
pub fn purged_bytes() -> (u64, u64) {
    #[cfg(not(target_family = "wasm"))]
    {
        let (mut owner, mut others) = (0usize, 0usize);
        // SAFETY: two out-pointers to locals; the vendored function reads atomics only.
        unsafe { mi_arena_purged_bytes(&mut owner, &mut others) };
        (owner as u64, others as u64)
    }
    #[cfg(target_family = "wasm")]
    (0, 0)
}

/// This thread's phase for the purge timing: 1 = a session inside a statement, 2 = a session after its ReadyForQuery was
/// sent, 3 = an executor worker, 0 = anything else (a thread-local store in the vendored arena code).
pub fn set_purge_phase(phase: u8) {
    #[cfg(not(target_family = "wasm"))]
    // SAFETY: a thread-local int store.
    unsafe {
        mi_arena_set_purge_phase(core::ffi::c_int::from(phase))
    };
    #[cfg(target_family = "wasm")]
    let _ = phase;
}

/// The purge events (a delayed-purge walk, the purger's purge, a pressure purge; each that decommitted something) by the
/// thread that ran them: `[the purger, a session inside a statement, a session after its ReadyForQuery, any other
/// thread, an executor worker]`, each `[events, summed ns, longest ns, bytes decommitted]`, cumulative.
pub fn purge_times() -> [[u64; 4]; 5] {
    let mut out = [[0u64; 4]; 5];
    #[cfg(not(target_family = "wasm"))]
    {
        let mut raw = [0usize; 20];
        // SAFETY: a 20-value out-array; the vendored function reads atomics only.
        unsafe { mi_arena_purge_times(raw.as_mut_ptr()) };
        for (i, v) in raw.iter().enumerate() {
            out[i / 4][i % 4] = *v as u64;
        }
    }
    out
}

/// C4: the dead-man fallback's purges on general arena `i` (tests, the bar's per-arena row; expected 0).
pub fn general_purge_fallbacks(i: usize) -> u64 {
    #[cfg(not(target_family = "wasm"))]
    {
        let id = ARENAS.get(i).filter(|_| i < GENERAL.load(Relaxed)).map_or(core::ptr::null_mut(), |a| a.id.load(Relaxed));
        // SAFETY: a plain atomic read in the vendored arena.c (a null id would sum all, so it is skipped).
        if id.is_null() { 0 } else { unsafe { mi_arena_purge_owned_fallbacks(id) as u64 } }
    }
    #[cfg(target_family = "wasm")]
    {
        let _ = i;
        0
    }
}

/// The purges the dead-man fallback ran on owned arenas, all summed (expected 0; the view's `cache_purge_fallbacks` row).
pub fn purge_owned_fallbacks() -> u64 {
    #[cfg(not(target_family = "wasm"))]
    {
        // SAFETY: a plain atomic read in the vendored arena.c.
        unsafe { mi_arena_purge_owned_fallbacks(core::ptr::null_mut()) as u64 }
    }
    #[cfg(target_family = "wasm")]
    0
}

/// REUSE BEFORE RELEASE (PGRUST_REUSE_BEFORE_RELEASE=1, default off): a general heap that would commit fresh slices
/// first takes a general arena's own committed free run, then the cache arena's committed free slices (freed images'
/// pages still resident, waiting out their purge delay), and the cache heap's own new page (a part reload) takes the
/// cache arena's committed free run first too, so growth after an eviction reuses those pages instead of faulting fresh
/// ones and the purge gives the OS only what nothing claimed. The bytes general heaps hold in the cache arena count as
/// general bytes in `memledger::arena::taken` (the census and the cache road). `false`: today's behavior.
pub fn set_reuse(on: bool) {
    #[cfg(not(target_family = "wasm"))]
    {
        let id = if on { CACHE_ARENA_ID.load(Relaxed) } else { core::ptr::null_mut() };
        // SAFETY: the cache arena's id from `install` (or null: no donor).
        unsafe { mi_arena_set_reuse_donor(id) };
        if on && !id.is_null() {
            memledger::arena::set_reused_probe(reused_bytes);
        }
        REUSE_ON.store(on && !id.is_null(), Relaxed);
    }
    #[cfg(target_family = "wasm")]
    let _ = on;
}

static REUSE_ON: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// REUSE BEFORE RELEASE: whether the switch put the cache arena up as the donor.
pub fn reuse_on() -> bool {
    REUSE_ON.load(Relaxed)
}

/// REUSE BEFORE RELEASE: `(bytes general heaps hold of the cache arena now, claims, bytes claimed, misses)`, cumulative
/// but the first.
pub fn reuse_stats() -> (u64, u64, u64, u64) {
    #[cfg(not(target_family = "wasm"))]
    {
        let (mut live, mut taken, mut bytes, mut misses) = (0usize, 0usize, 0usize, 0usize);
        // SAFETY: four out-pointers to locals; the vendored function reads atomics only.
        unsafe { mi_arena_reuse_stats(&mut live, &mut taken, &mut bytes, &mut misses) };
        (live as u64, taken as u64, bytes as u64, misses as u64)
    }
    #[cfg(target_family = "wasm")]
    (0, 0, 0, 0)
}

fn reused_bytes() -> u64 {
    reuse_stats().0
}

/// The free-run histogram's buckets: bucket k holds runs of [2^k, 2^(k+1)) slices (64 KiB each), the last every longer one.
pub const RUN_BUCKETS: usize = 12;

/// REUSE BEFORE RELEASE sizing (the supply side): the cache arena's free runs as a log2 histogram in bytes,
/// `(committed, any)`: runs still committed (freed, waiting for their purge: reusable with no fresh fault), and every free
/// run. A walk over the arena's free and commit bitmaps (a diagnostic, `PGRUST_REUSE_RUNS=1`). Zeros without the arena.
pub fn cache_free_runs() -> ([u64; RUN_BUCKETS], [u64; RUN_BUCKETS]) {
    let (mut c, mut a) = ([0usize; RUN_BUCKETS], [0usize; RUN_BUCKETS]);
    #[cfg(not(target_family = "wasm"))]
    {
        let id = CACHE_ARENA_ID.load(Relaxed);
        if !id.is_null() {
            // SAFETY: the cache arena's id from `install`; two arrays of RUN_BUCKETS the vendored walk fills.
            unsafe { mi_arena_free_runs(id, c.as_mut_ptr(), a.as_mut_ptr(), RUN_BUCKETS) };
        }
    }
    (c.map(|v| v as u64), a.map(|v| v as u64))
}

/// REUSE (the C4 gate): the bytes a new allocation takes with no fresh fault: every pgrust arena's slices both free and
/// committed (freed pages waiting out their purge: the general arenas' that C4 holds, the cache arena's evicted images'),
/// the accounts view's reusable row (`mcx::ladder::set_reusable_probe`): information, never headroom (they are resident
/// until a purge gives them back). 0 when off.
pub fn reusable_bytes() -> u64 {
    #[cfg(not(target_family = "wasm"))]
    {
        if !reuse_on() {
            return 0;
        }
        let ids = (0..general_arenas()).map(|i| ARENAS[i].id.load(Relaxed)).chain(core::iter::once(CACHE_ARENA_ID.load(Relaxed)));
        // SAFETY: popcounts over each installed arena's free and commit bitmaps in the vendored arena.c (0 for a null id).
        ids.filter(|id| !id.is_null()).map(|id| unsafe { mi_arena_committed_free(id) } as u64).sum()
    }
    #[cfg(target_family = "wasm")]
    0
}

/// REUSE (the C4 gate): where new allocations' pages came from, cumulative bytes: `(a general arena's own committed free
/// runs, the cache arena's committed free runs taken by general heaps, committed afresh in the general arenas, committed
/// afresh in the cache arena, the cache arena's committed free runs taken by the cache heap itself)`.
pub fn alloc_sources() -> (u64, u64, u64, u64, u64) {
    #[cfg(not(target_family = "wasm"))]
    {
        let (mut n, mut bytes, mut cn, mut cbytes) = (0usize, 0usize, 0usize, 0usize);
        // SAFETY: out-pointers to locals; the vendored functions read atomics only.
        unsafe {
            mi_arena_reuse_self_stats(&mut n, &mut bytes);
            mi_arena_reuse_cache_self_stats(&mut cn, &mut cbytes);
        }
        (bytes as u64, reuse_stats().2, FRESH[0].load(Relaxed), FRESH[1].load(Relaxed), cbytes as u64)
    }
    #[cfg(target_family = "wasm")]
    (0, 0, 0, 0, 0)
}

/// Whether `p` lies in the cache arena (tests, diagnostics).
pub fn in_cache_arena(p: *const u8) -> bool {
    let base = ARENAS[CACHE].base.load(Relaxed);
    base != 0 && (p as usize).wrapping_sub(base) < ARENA_BYTES
}

/// Whether `p` lies in one of pgrust's arenas, general or cache.
pub fn in_arenas(p: *const u8) -> bool {
    ARENAS.iter().any(|a| {
        let base = a.base.load(Relaxed);
        base != 0 && (p as usize).wrapping_sub(base) < ARENA_BYTES
    })
}

/// General arenas installed.
pub fn general_arenas() -> usize {
    GENERAL.load(Relaxed)
}

// H3 step (2) (`give_back_service`, GIVEBACK-SERVICE-DESIGN step (2), memory accounting's and the broker's terms of
// 2026-10-17): one purge owner per arena, by construction. An arena's slot names its owner: none, pg:mempurge (purge (a):
// C3's cache arena, C4's general arenas, through `set_cache_purge_owned` / `set_general_purge_owned`), or the give-back
// service (`take_service_purge`). A taker wins only by CAS from none, and only the winner sets mimalloc's purge-owned flag
// (the vendored C unchanged); a take the other owner holds is refused and counted, and the refused one keeps today's
// path for that arena. Owner 1's entry points (purge_cache_now, purge_general_expired, purge_after_idle, purge_unused from
// the purger) skip the service's arenas, and the service's (`service_purge`) touches only its own, so no arena is purged by
// both owners. The ladder's pressure purge (purge_unused, by_purger false) stays owner-agnostic. While the service owns
// nothing (give_back_service = 0) every slot reads none or owner 1 and every path is as before. Nothing here writes the
// memory ledger.

/// The owners in an arena's slot: none, pg:mempurge (purge (a)), the give-back service.
pub const NO_OWNER: u8 = 0;
pub const OWNER_PURGER: u8 = 1;
pub const OWNER_SERVICE: u8 = 2;
/// The slots: the general arenas by index, then the cache arena (`CACHE`).
pub const PURGE_SLOTS: usize = MAX_GENERAL + 1;
static OWNER: [AtomicU8; PURGE_SLOTS] = [const { AtomicU8::new(NO_OWNER) }; PURGE_SLOTS];
/// Takes refused because the other owner held the arena (the view's give_back_owner_refusals row).
static OWNER_REFUSALS: AtomicU64 = AtomicU64::new(0);
/// The arenas the service owns now.
static SERVICE_ARENAS: AtomicUsize = AtomicUsize::new(0);
/// The ladder's pressure purge's bytes from arenas an owner held (M3's purged_by_pressure).
static PRESSURE_OWNED: AtomicU64 = AtomicU64::new(0);
/// The service's forced purge is due (a release asked for it), its wake, and the watchdog's cancel line read (the binary
/// installs `mcx::ladder::past_cancel_line`).
static FORCE_DUE: AtomicBool = AtomicBool::new(false);
static SERVICE_WAKE: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
/// The service's note of a busy-guard hand-off (memruntime's `service::note_handoff`: the time, for the hand-off's latency).
static SERVICE_HANDOFF: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static CANCEL_PROBE: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
/// Forced purges handed to the service, run on their callers past the watchdog's cancel line, and those of the latter
/// that met mimalloc's purge guard held by another thread.
static FORCED: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
/// Tests only (`test_guard_busy_next_inline`): the next forced purge past the cancel line meets the guard held.
static BUSY_NEXT: AtomicBool = AtomicBool::new(false);

/// Slot `i`'s arena; null when it is not installed.
fn arena_id(i: usize) -> *mut c_void {
    if i == CACHE {
        CACHE_ARENA_ID.load(Relaxed)
    } else if i < GENERAL.load(Relaxed) {
        ARENAS[i].id.load(Relaxed)
    } else {
        core::ptr::null_mut()
    }
}

/// `who` takes slot `i` by CAS: whether it holds it after (taken now, or held already). The other owner's slot: refused,
/// counted.
fn take(i: usize, who: u8) -> bool {
    match OWNER[i].compare_exchange(NO_OWNER, who, AcqRel, Acquire) {
        Ok(_) => true,
        Err(held) if held == who => true,
        Err(_) => {
            OWNER_REFUSALS.fetch_add(1, Relaxed);
            false
        }
    }
}

/// `who` lets slot `i` go: whether it held it.
fn give_up(i: usize, who: u8) -> bool {
    OWNER[i].compare_exchange(who, NO_OWNER, AcqRel, Acquire).is_ok()
}

/// Whether `who`'s own entry points may purge slot `i`: the other owner does not hold it.
fn not_the_others(i: usize, who: u8) -> bool {
    let held = OWNER[i].load(Acquire);
    held == NO_OWNER || held == who
}

/// H3 step (2): the give-back service takes every installed arena's purge as owner 2 (memruntime's pg:memrelease at its
/// start, after `set_service_wake`). `(taken, refused)`: an arena pg:mempurge holds stays its own (purge (a) starts
/// first), counted as a refusal.
pub fn take_service_purge() -> (u32, u32) {
    let (mut taken, mut refused) = (0, 0);
    for i in 0..PURGE_SLOTS {
        let id = arena_id(i);
        if id.is_null() {
            continue;
        }
        if take(i, OWNER_SERVICE) {
            #[cfg(not(target_family = "wasm"))]
            // SAFETY: an installed arena's id from `manage`.
            unsafe {
                mi_arena_set_purge_owned(id, true)
            };
            taken += 1;
        } else {
            refused += 1;
        }
    }
    SERVICE_ARENAS.store(taken as usize, Release);
    (taken, refused)
}

/// H3 step (2): the service's purge of slot `i` (`force`: the ranges inside the purge delay too), only an arena it owns;
/// each call also marks it alive for mimalloc's dead-man rule. Whether it purged.
pub fn service_purge(i: usize, force: bool) -> bool {
    if OWNER.get(i).is_none_or(|o| o.load(Acquire) != OWNER_SERVICE) {
        return false;
    }
    let id = arena_id(i);
    #[cfg(not(target_family = "wasm"))]
    {
        // SAFETY: an installed arena's id from `manage`; the vendored function takes the purge guard.
        !id.is_null() && unsafe { mi_arena_purge_now(id, force) }
    }
    #[cfg(target_family = "wasm")]
    {
        let _ = (id, force);
        false
    }
}

/// H3 step (2): the service's wake for a forced purge, and its note of a busy-guard hand-off past the cancel line (both
/// set before it takes its arenas).
pub fn set_service_wake(wake: fn(), handoff: fn()) {
    SERVICE_HANDOFF.store(handoff as *mut (), Release);
    SERVICE_WAKE.store(wake as *mut (), Release);
}

/// H3 step (2): the watchdog's cancel line read (the binary installs `mcx::ladder::past_cancel_line`).
pub fn set_cancel_line_probe(past: fn() -> bool) {
    CANCEL_PROBE.store(past as *mut (), Release);
}

/// H3 step (2): whether a forced purge is due, cleared (the service's pass).
pub fn take_force_due() -> bool {
    FORCE_DUE.swap(false, AcqRel)
}

/// H3 step (2): a release's forced purge over the service's arenas, which its collects skipped. Under the watchdog's cancel
/// line it is the service's job (one flag, and a wake); past it, run here on the caller as today (kill safety over the
/// thread, as step (1)'s exception). Nothing while the service owns no arena.
fn forced_purge_job() {
    if SERVICE_ARENAS.load(Acquire) == 0 {
        return;
    }
    let probe = CANCEL_PROBE.load(Acquire);
    // SAFETY: only set_cancel_line_probe stores here, always from fn() -> bool.
    let past = !probe.is_null() && unsafe { core::mem::transmute::<*mut (), fn() -> bool>(probe) }();
    if past {
        // mimalloc's purge guard is one try-lock over every arena: an arena whose purge met it held (the service's own
        // pass, or any collect) purged nothing here, counted
        let busy_test = BUSY_NEXT.swap(false, AcqRel);
        let mut busy = false;
        for i in 0..PURGE_SLOTS {
            if purge_owner(i) == OWNER_SERVICE && (busy_test || (!service_purge(i, true) && guard_was_busy())) {
                busy = true;
            }
        }
        FORCED[1].fetch_add(1, Relaxed);
        if !busy {
            return;
        }
        // the refused arenas' forced ranges go to the service now, not to the next release (a pass with nothing left
        // costs one batch); the service times the hand-off to the end of its next forced pass
        FORCED[2].fetch_add(1, Relaxed);
        let note = SERVICE_HANDOFF.load(Acquire);
        if !note.is_null() {
            // SAFETY: only set_service_wake stores here, always from fn().
            let note: fn() = unsafe { core::mem::transmute(note) };
            note();
        }
    }
    FORCE_DUE.store(true, Release);
    FORCED[0].fetch_add(1, Relaxed);
    let wake = SERVICE_WAKE.load(Acquire);
    if !wake.is_null() {
        // SAFETY: only set_service_wake stores here, always from fn().
        let wake: fn() = unsafe { core::mem::transmute(wake) };
        wake();
    }
}

/// H3 step (2): the purge owners' counters: `(arenas the service owns, takes refused, forced purges handed to the service,
/// forced purges run on their callers past the cancel line, the ladder's pressure purge's bytes from owned arenas)`.
pub fn purge_owner_counts() -> (u64, u64, u64, u64, u64) {
    (
        SERVICE_ARENAS.load(Relaxed) as u64,
        OWNER_REFUSALS.load(Relaxed),
        FORCED[0].load(Relaxed),
        FORCED[1].load(Relaxed),
        PRESSURE_OWNED.load(Relaxed),
    )
}

/// Whether this thread's last `mi_arena_purge_now` returned at mimalloc's purge guard (another thread held it).
fn guard_was_busy() -> bool {
    #[cfg(not(target_family = "wasm"))]
    {
        // SAFETY: a thread-local read in the vendored arena.c.
        unsafe { mi_arena_purge_now_busy() }
    }
    #[cfg(target_family = "wasm")]
    false
}

/// H3 step (2): the forced purges past the cancel line that met mimalloc's purge guard held by another thread.
pub fn forced_guard_busy() -> u64 {
    FORCED[2].load(Relaxed)
}

/// Tests only: the next forced purge past the cancel line finds mimalloc's purge guard held over the whole pass (it is one
/// try-lock over every arena), as when the service or another thread is purging just then.
#[doc(hidden)]
pub fn test_guard_busy_next_inline() {
    BUSY_NEXT.store(true, Release);
}

/// H3 step (2): slot `i`'s owner now (`NO_OWNER`, `OWNER_PURGER`, `OWNER_SERVICE`).
pub fn purge_owner(i: usize) -> u8 {
    OWNER.get(i).map_or(NO_OWNER, |o| o.load(Acquire))
}

/// The cache arena's slot.
pub const CACHE_SLOT: usize = CACHE;

#[cfg(test)]
mod tests {
    use super::*;

    /// CVE-2026-74674: the decommit (the path `purge_cache_now` reaches through the cache arena's commit callback) maps a
    /// range holding a whole 2 MiB-aligned range over afresh and splits any other range so that no MADV_DONTNEED piece
    /// covers a whole 2 MiB-aligned range.
    #[cfg(all(target_os = "linux", not(target_family = "wasm")))]
    #[test]
    fn decommit_never_dontneeds_a_whole_2mib_range() {
        const PMD: usize = 2 << 20;
        // SAFETY: a private anonymous reservation of 16 MiB, unmapped at the end.
        let base = unsafe {
            libc::mmap(core::ptr::null_mut(), 16 << 20, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE, -1, 0)
        } as usize;
        assert_ne!(base, libc::MAP_FAILED as usize);
        let a = base.next_multiple_of(PMD);
        let (_, r0, w0) = decommit_counts();
        decommit(a + 4096, 6 << 20); // holds a whole aligned 2 MiB range: mapped over
        let (_, r1, w1) = decommit_counts();
        assert_eq!(r1, r0 + 1, "a range holding a whole 2 MiB-aligned range is mapped over afresh");
        decommit(a + PMD - (1 << 20), 2 << 20); // straddles a boundary, holds no whole range: DONTNEED pieces
        decommit(a + 4096, PMD); // just short of the next boundary's whole range
        let (_, r2, w2) = decommit_counts();
        assert_eq!(r2, r1, "no remap for a range that holds no whole 2 MiB-aligned range");
        assert_eq!((w0, w1, w2), (0, 0, 0), "no MADV_DONTNEED piece covered a whole 2 MiB-aligned range");
        // SAFETY: the reservation mapped above.
        unsafe { libc::munmap(base as *mut c_void, 16 << 20) };
    }

    #[test]
    fn marks_count_each_slice_once() {
        let a = &ARENAS[MAX_GENERAL - 1];
        assert_eq!(mark(a, 3, 200, true), (197, 197));
        assert_eq!(mark(a, 0, 64, true), (3, 64), "only the three not yet set");
        assert_eq!(mark(a, 100, 300, false), (100, 200), "100..200 were set");
        assert_eq!(mark(a, 0, SLICES + 10, false), (100, SLICES), "the rest; the end is clamped");
        assert_eq!(mark(a, 5, 5, true), (0, 0));
    }
}
