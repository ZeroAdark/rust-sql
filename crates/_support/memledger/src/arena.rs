//! The allocator's own arenas, as the ledger sees them (ALLOC-LADDER (c), LADDER-DESIGN sections 5 and 6).
//!
//! pgrust hands mimalloc its address space (`allocarena`): general arenas every heap uses, and one exclusive arena for
//! the cache images (D4321 step 1). mimalloc calls `allocarena`'s commit callback the moment it takes a slice range of
//! those arenas into use and the moment it purges one, so the bytes it has taken are counted here exactly: `ALLOC_TAKEN`
//! (general arenas) and `CACHE_TAKEN` (the cache arena). The callback runs inside the allocator on the allocating
//! thread: everything it calls here is a relaxed atomic, and once past the ladder's line one futex wake. Nothing here
//! allocates, locks or blocks.
//!
//! The cache scope: a thread inside `cache_scope_for` takes its global-allocator blocks from the cache arena's heap (the
//! global allocator reads `in_cache_scope`), so a cache image shares its pages only with other cache images and an
//! evicted image empties whole pages the arena can purge.

use core::sync::atomic::{AtomicBool, AtomicI64, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed};

use crate::charge::{Charge, Door, Owner};
use crate::Kind;

/// Bytes of the general arenas mimalloc has taken into use (committed slices), less what it purged.
static ALLOC_TAKEN: AtomicI64 = AtomicI64::new(0);
/// The same for the exclusive cache arena.
static CACHE_TAKEN: AtomicI64 = AtomicI64::new(0);
/// Whether pgrust's arenas are installed (the counters mean something).
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// Whether the cache arena's heap is installed (a cache scope routes somewhere).
static CACHE_HEAP: AtomicBool = AtomicBool::new(false);
/// The line `ALLOC_TAKEN + CACHE_TAKEN` may not pass without waking the ladder (0: off).
static LINE: AtomicU64 = AtomicU64::new(0);
/// Set when a take crossed the line; the ladder (the memory watchdog's thread) clears it when it has sampled.
static LADDER_DUE: AtomicBool = AtomicBool::new(false);
/// The word the ladder thread parks on between samples (bumped at each wake).
static LADDER_WORD: AtomicU32 = AtomicU32::new(0);
/// Counted crossings (information).
static DUE_COUNT: AtomicU64 = AtomicU64::new(0);
/// An extra wake (tests): called after the futex wake.
static WAKE_HOOK: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// `allocarena` installed its arenas.
pub fn set_active() {
    ACTIVE.store(true, Relaxed);
}

/// Whether pgrust's arenas are installed.
pub fn active() -> bool {
    ACTIVE.load(Relaxed)
}

/// `allocarena` installed the cache arena's heap.
pub fn set_cache_heap_ready() {
    CACHE_HEAP.store(true, Relaxed);
}

/// Whether a cache scope takes its blocks from the cache arena.
pub fn cache_heap_ready() -> bool {
    CACHE_HEAP.load(Relaxed)
}

/// The commit callback's count: `delta` bytes taken (+) or purged (-) in a general (`cache` false) or the cache arena.
/// Past the line, the first take wakes the ladder once (until it clears `LADDER_DUE`). Allocator context: atomics and
/// one futex wake only.
#[inline]
pub fn note_taken(cache: bool, delta: i64) {
    let (mine, other) = if cache { (&CACHE_TAKEN, &ALLOC_TAKEN) } else { (&ALLOC_TAKEN, &CACHE_TAKEN) };
    let now = mine.fetch_add(delta, Relaxed).wrapping_add(delta);
    if delta <= 0 {
        return;
    }
    let line = LINE.load(Relaxed);
    if line != 0 && now.saturating_add(other.load(Relaxed)).max(0) as u64 > line && !LADDER_DUE.swap(true, Relaxed) {
        DUE_COUNT.fetch_add(1, Relaxed);
        wake_ladder();
    }
}

/// D4812 (2): mcx's block gate wakes the ladder now, as a take past the line does (`LADDER_DUE`, then one futex wake;
/// none while the ladder has not cleared an earlier one). Allocator context: atomics and one futex wake only.
pub fn note_due() {
    if !LADDER_DUE.swap(true, Relaxed) {
        wake_ladder();
    }
}

/// (general, cache) bytes taken now; `None` before the arenas are installed. REUSE BEFORE RELEASE: the cache arena's
/// committed slices a general heap holds (`set_reused_probe`) count as the general arenas' (their blocks are no cache's).
pub fn taken() -> Option<(u64, u64)> {
    active().then(|| {
        let (general, cache) = (ALLOC_TAKEN.load(Relaxed).max(0) as u64, CACHE_TAKEN.load(Relaxed).max(0) as u64);
        let reused = reused().min(cache);
        (general.saturating_add(reused), cache - reused)
    })
}

/// REUSE BEFORE RELEASE: the bytes of the cache arena general heaps hold now (allocarena's probe,
/// installed with the switch); 0 when none is installed.
static REUSED_PROBE: core::sync::atomic::AtomicPtr<()> = core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

pub fn set_reused_probe(f: fn() -> u64) {
    REUSED_PROBE.store(f as *mut (), core::sync::atomic::Ordering::Release);
}

/// The cache arena's bytes general heaps hold now (0 without the probe).
pub fn reused() -> u64 {
    let p = REUSED_PROBE.load(core::sync::atomic::Ordering::Acquire);
    if p.is_null() {
        return 0;
    }
    // SAFETY: only set_reused_probe stores here, always from fn() -> u64.
    let f: fn() -> u64 = unsafe { core::mem::transmute(p) };
    f()
}

/// The ladder's line for the allocator's takes (bytes of `ALLOC_TAKEN + CACHE_TAKEN`); 0 turns the wake off.
pub fn set_line(bytes: u64) {
    LINE.store(bytes, Relaxed);
}

/// The line now (0: off).
pub fn line() -> u64 {
    LINE.load(Relaxed)
}

/// Whether a take crossed the line since the ladder last cleared it; clears it.
pub fn take_ladder_due() -> bool {
    LADDER_DUE.swap(false, Relaxed)
}

/// Crossings so far.
pub fn due_count() -> u64 {
    DUE_COUNT.load(Relaxed)
}

/// An extra wake after the futex's (tests: a ladder stand-in); never allocates.
pub fn set_wake_hook(f: fn()) {
    WAKE_HOOK.store(f as *mut (), Relaxed);
}

fn wake_ladder() {
    LADDER_WORD.fetch_add(1, Relaxed);
    #[cfg(target_os = "linux")]
    // SAFETY: FUTEX_WAKE on a static word; no memory is touched past the word.
    unsafe {
        libc::syscall(libc::SYS_futex, LADDER_WORD.as_ptr(), libc::FUTEX_WAKE | libc::FUTEX_PRIVATE_FLAG, i32::MAX, core::ptr::null::<libc::timespec>());
    }
    let p = WAKE_HOOK.load(Relaxed);
    if !p.is_null() {
        // SAFETY: only set_wake_hook stores here, always from fn().
        let f: fn() = unsafe { core::mem::transmute(p) };
        f();
    }
}

/// The ladder thread's park between samples: up to `ms`, or until a take crosses the line (then `LADDER_DUE` is set).
/// Elsewhere than Linux a plain sleep.
pub fn park_ladder(ms: u64) {
    // D4781 (`fold_on_park`): the memory watchdog's sample loop parks here
    crate::park_fold();
    #[cfg(target_os = "linux")]
    {
        let seen = LADDER_WORD.load(Relaxed);
        if LADDER_DUE.load(Relaxed) {
            return;
        }
        let ts = libc::timespec { tv_sec: (ms / 1000) as libc::time_t, tv_nsec: ((ms % 1000) * 1_000_000) as libc::c_long };
        // SAFETY: FUTEX_WAIT on a static word with a relative timeout; returns at once if the word moved.
        unsafe {
            libc::syscall(libc::SYS_futex, LADDER_WORD.as_ptr(), libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG, seen, &ts as *const libc::timespec);
        }
    }
    #[cfg(not(target_os = "linux"))]
    std::thread::sleep(std::time::Duration::from_millis(ms));
}

// ---- ALLOC-LADDER (a): the allocator's retained bytes, counted as used until they are returned ------------------------

/// The last `retained_of` reading (bytes), refreshed by the memory watchdog's thread (`memruntime`).
static RETAINED: AtomicU64 = AtomicU64::new(0);
/// The last three readings (u64::MAX: none yet), for the fragmented part's floor.
static RETAINED_LAST: [AtomicU64; 3] = [const { AtomicU64::new(u64::MAX) }; 3];
static RETAINED_AT: AtomicU32 = AtomicU32::new(0);

/// The allocator's retained bytes from its parts: its resident bytes (the present pages of its mappings) less what the
/// accounts hold of it (`charged_live`: its live blocks, less the given-back pages inside them, as the view counts them;
/// `not_resident_heap`: door 1's bytes the residency pass found not resident). Never below 0. ONE formula for the view's
/// `allocator_retained` row and the snapshot's `retained` term.
pub fn retained_of(alloc_resident: i64, charged_live: i64, not_resident_heap: i64) -> i64 {
    alloc_resident.wrapping_sub(charged_live.wrapping_sub(not_resident_heap)).max(0)
}

/// A fresh reading of the retained bytes.
pub fn note_retained(bytes: u64) {
    RETAINED.store(bytes, Relaxed);
    let i = RETAINED_AT.fetch_add(1, Relaxed) as usize % RETAINED_LAST.len();
    RETAINED_LAST[i].store(bytes, Relaxed);
}

/// The last reading (0 before one): what the snapshot counts as used beside the accounts.
pub fn retained() -> u64 {
    RETAINED.load(Relaxed)
}

/// The fragmented part: the least of the last three readings (0 before one). It swings less than the reading (a cache's
/// give-back raises the reading for a while), and a minimum never over-cuts the one-owner wall that subtracts it.
pub fn retained_floor() -> u64 {
    match RETAINED_LAST.iter().map(|r| r.load(Relaxed)).min() {
        Some(u64::MAX) | None => 0,
        Some(m) => m,
    }
}

/// While alive, the global allocator on this thread takes its blocks from the cache arena's heap (when installed).
/// Nests; not `Send` (the count is the thread's). CACHE-CHARGE: a scope that names its holder (`cache_scope_for`)
/// charges the holder the cache-road blocks taken while it is open (`cache_took`), as they are taken.
pub struct CacheScope {
    /// CACHE-CHARGE: its owner's record (`cache_scope_for`); `None` once the scope ended.
    rec: Option<Box<ScopeRec>>,
    /// Still open (`finish` ends it before the drop).
    open: bool,
    _not_send: core::marker::PhantomData<*const ()>,
}

/// CACHE-CHARGE: an open holder scope: its door-1 charge on the holder and the bytes taken (+) or given back (-) since
/// its last fold. Boxed so the thread-local link to it stays valid while the `CacheScope` moves.
struct ScopeRec {
    charge: Charge,
    pending: i64,
    /// The holder scope open on this thread when this one opened (restored when it ends).
    prev: *mut ScopeRec,
}

std::thread_local! {
    /// CACHE-CHARGE: this thread's innermost open holder scope (null: none). Plain data: no destructor, so the global
    /// allocator may read it at any time.
    static HOLDER: core::cell::Cell<*mut ScopeRec> = const { core::cell::Cell::new(core::ptr::null_mut()) };
}

/// CACHE-CHARGE: the cache arena's address range [base, base + bytes) (`allocarena::install`): a block in it is a cache
/// image's (or its build's) block.
static CACHE_LO: AtomicUsize = AtomicUsize::new(0);
static CACHE_SPAN: AtomicUsize = AtomicUsize::new(0);
/// CACHE-CHARGE: bytes freed inside a holder scope past what it had taken (a block it did not take: an older image
/// freed inside a build), clamped off its charge.
static SCOPE_FOREIGN: AtomicU64 = AtomicU64::new(0);

/// CACHE-CHARGE: `allocarena::install`'s cache arena range.
pub fn set_cache_span(base: usize, bytes: usize) {
    CACHE_LO.store(base, Relaxed);
    CACHE_SPAN.store(bytes, Relaxed);
}

/// CACHE-CHARGE: whether `p` lies in the cache arena (one range compare: the global allocator's test at a free).
#[inline(always)]
pub fn in_cache_span(p: *const u8) -> bool {
    (p as usize).wrapping_sub(CACHE_LO.load(Relaxed)) < CACHE_SPAN.load(Relaxed)
}

/// CACHE-CHARGE: bytes freed inside holder scopes that those scopes had not taken (information; 0 by the sites' contract:
/// no eviction runs inside a build's scope).
pub fn scope_foreign_frees() -> u64 {
    SCOPE_FOREIGN.load(Relaxed)
}

/// The cache road's reading (`road`): bytes of the live blocks in the cache arena and in the own range, on any thread.
static ARENA_LIVE: AtomicI64 = AtomicI64::new(0);
static OWN_LIVE: AtomicI64 = AtomicI64::new(0);
/// Bytes taken inside a cache scope that names no owner, less those freed inside one, since start. Every scope names its
/// owner (`cache_scope_for`), so this stays 0: a guard.
static PLAIN: AtomicI64 = AtomicI64::new(0);

/// The global allocator's step for the reading: `delta` bytes of cache-road blocks taken (+) or freed (-) in the own
/// range (`own`) or the cache arena, on any thread, inside a cache scope or not. Allocator context: one relaxed atomic.
#[inline]
pub fn road_live(own: bool, delta: isize) {
    if own { &OWN_LIVE } else { &ARENA_LIVE }.fetch_add(delta as i64, Relaxed);
}

/// The cache road now, process-wide (`road`). Read between statements, `unheld` is a process-level check: a statement's
/// own cache-road images (an image no cache keeps, charged to its statement or operator) are live only while it runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Road {
    /// The cache arena's committed bytes (`taken().1`; `None` before the arenas are installed): its live blocks and the
    /// free pages it has not purged.
    pub committed: Option<u64>,
    /// Bytes of the live blocks in the cache arena.
    pub arena_live: i64,
    /// Bytes of the live blocks in the own range.
    pub own_live: i64,
    /// Bytes scopes that name no owner took and kept, since start (`PLAIN`): 0, as every scope names its owner.
    pub plain: i64,
    /// The cache holders' door-1 bytes (`charge::cache_holders_heap`).
    pub holders_heap: i64,
}

impl Road {
    /// Live cache-road bytes no cache holder's charge covers. Below 0 when a holder charges bytes it keeps off the
    /// cache road.
    pub fn unheld(&self) -> i64 {
        self.arena_live.wrapping_add(self.own_live).wrapping_sub(self.holders_heap)
    }

    /// The cache arena's committed bytes no live block uses (free pages not purged yet, and block rounding).
    pub fn retention(&self) -> Option<i64> {
        self.committed.map(|c| (c as i64).wrapping_sub(self.arena_live))
    }
}

/// The cache road's reading now (relaxed loads and one pass over the cache holders' cells; allocates nothing).
pub fn road() -> Road {
    Road {
        committed: taken().map(|t| t.1),
        arena_live: ARENA_LIVE.load(Relaxed),
        own_live: OWN_LIVE.load(Relaxed),
        plain: PLAIN.load(Relaxed),
        holders_heap: crate::charge::cache_holders_heap(),
    }
}

/// CACHE-CHARGE: open a cache scope on this thread (around a cache image's fill: the extent image, the fp / render tables'
/// final buffers, a condition-cache entry), whose cache-road blocks are charged to `owner` (the holder that keeps the
/// image: an `Owner::Cache`) on kind lane `kind` as they are taken, within `DOOR_FOLD_BYTES` per thread (`cache_took`); a
/// free of a cache-road block while it is open gives its bytes back. `finish` hands the charge to the image; a scope
/// dropped without it gives the bytes back to this thread, where its build's blocks are freed. Nested, the innermost
/// scope's owner is charged. Every cache scope names its owner: there is no scope that charges no one.
/// CONVERSION WAVE 3 (cvpread Q4): `owner` may be any owner, not only a holder: an image built on the cache road that no
/// cache keeps (pgrc_read's uncached extents, a reader's) is charged to its statement's or operator's owner as it is
/// allocated. The charge's release is the owner's (`charge::release_of`: `Evict` for a holder, `Statement` for a
/// statement / operator, `Portal` for a portal).
#[must_use]
pub fn cache_scope_for(owner: Owner, kind: Kind) -> CacheScope {
    // the record first: its block is the general road's (no scope is open yet), never the holder's
    let mut rec = Box::new(ScopeRec { charge: Charge::open(owner, crate::charge::release_of(owner), kind, Door::Heap, 0), pending: 0, prev: core::ptr::null_mut() });
    let at: *mut ScopeRec = &mut *rec;
    HOLDER.with(|h| rec.prev = h.replace(at));
    crate::door_cache_step(1);
    CacheScope { rec: Some(rec), open: true, _not_send: core::marker::PhantomData }
}

impl CacheScope {
    /// CACHE-CHARGE: the scope ended; its charge (the bytes its blocks hold on the holder, `None` for a scope that charges
    /// no one) for the image that keeps them (the holder's insert takes it over: `Charge::absorb`).
    pub fn finish(mut self) -> Option<Charge> {
        self.end()
    }

    /// D4536: `c`'s committed bytes onto this scope's charge (`Charge::absorb`): a block the image reuses already
    /// allocated moves from its old owner to the scope's in one step, never through this thread's account.
    pub fn absorb(&mut self, c: Charge) {
        if let Some(rec) = self.rec.as_mut() {
            rec.charge.absorb(c);
        }
    }

    fn end(&mut self) -> Option<Charge> {
        if !core::mem::replace(&mut self.open, false) {
            return None;
        }
        crate::door_cache_step(-1);
        let mut rec = self.rec.take()?;
        fold(&mut rec);
        HOLDER.with(|h| {
            debug_assert!(core::ptr::eq(h.get(), &*rec), "cache scopes end in the order they opened");
            h.set(rec.prev)
        });
        let ScopeRec { charge, .. } = *rec;
        Some(charge)
    }
}

impl Drop for CacheScope {
    fn drop(&mut self) {
        // a holder scope not finished: its charge drops here, its bytes back on this thread before its blocks' frees
        drop(self.end());
    }
}

/// CACHE-CHARGE, the global allocator's step (after `door_heap`): `delta` bytes of cache-road blocks (the cache arena or
/// the own range) taken (+) or freed (-) on this thread while a cache scope is open. The innermost holder scope's count
/// moves; at `DOOR_FOLD_BYTES` either way it folds onto its charge. Allocator context: thread-locals and atomics only.
#[inline]
pub fn cache_took(delta: isize) {
    let _ = HOLDER.try_with(|h| {
        let r = h.get();
        if r.is_null() {
            // no holder scope open (none can be: every scope names its owner): counted, the guard's reading
            PLAIN.fetch_add(delta as i64, Relaxed);
            return;
        }
        // SAFETY: the innermost open holder scope of this thread: its box lives until its `CacheScope` ends, which
        // unlinks it first; only this thread reaches it, and never re-entrantly (a fold allocates nothing).
        let rec = unsafe { &mut *r };
        rec.pending = rec.pending.wrapping_add(delta as i64);
        if rec.pending.unsigned_abs() >= crate::DOOR_FOLD_BYTES as u64 {
            fold(rec);
        }
    });
}

/// A holder scope's count onto its charge: taken bytes committed (they leave this thread's account for the holder's),
/// given-back bytes freed, never past what it holds (the rest is a block it did not take: counted, clamped).
fn fold(rec: &mut ScopeRec) {
    let p = core::mem::take(&mut rec.pending);
    if p > 0 {
        rec.charge.commit(p as u64);
    } else if p < 0 {
        let (n, held) = (p.unsigned_abs(), rec.charge.committed());
        if n > held {
            SCOPE_FOREIGN.fetch_add(n - held, Relaxed);
        }
        rec.charge.free(n.min(held));
    }
}

/// Whether this thread is inside a cache scope and the cache heap is installed (the global allocator's test: one
/// relaxed load, then one thread-local read).
#[inline(always)]
pub fn in_cache_scope() -> bool {
    cache_heap_ready() && crate::door_cache_depth() != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_is_resident_less_the_charged_live_and_its_floor_is_the_least_of_three() {
        assert_eq!(retained_of(10 << 20, 8 << 20, 0), 2 << 20);
        assert_eq!(retained_of(10 << 20, 8 << 20, 1 << 20), 3 << 20, "not-resident heap is no live page");
        assert_eq!(retained_of(6 << 20, 8 << 20, 0), 0, "accounts above resident: none, never below 0");
        assert_eq!(retained_floor(), 0, "no reading yet");
        note_retained(500);
        assert_eq!((retained(), retained_floor()), (500, 500));
        note_retained(900);
        note_retained(700);
        assert_eq!((retained(), retained_floor()), (700, 500));
        note_retained(800);
        assert_eq!(retained_floor(), 700, "the oldest of the three (500) left");
    }

    #[test]
    fn scopes_nest_and_close() {
        set_cache_heap_ready();
        let scope = || cache_scope_for(Owner::Cache(crate::Holder::Of(crate::Consumer::ZoneStore)), Kind::Region);
        assert!(!in_cache_scope());
        {
            let _a = scope();
            assert!(in_cache_scope());
            {
                let _b = scope();
                assert!(in_cache_scope());
            }
            assert!(in_cache_scope(), "the outer scope is still open");
        }
        assert!(!in_cache_scope());
        let t = std::thread::spawn(|| in_cache_scope());
        let _c = scope();
        assert!(!t.join().unwrap(), "a scope is the thread's own");
    }

    #[test]
    fn d4536_a_scope_absorbs_a_charge_onto_its_owner() {
        let (pool, fp) = (Owner::Cache(crate::Holder::Of(crate::Consumer::BuildPool)), Owner::Cache(crate::Holder::Of(crate::Consumer::FpTables)));
        let kept = Charge::open(pool, crate::charge::release_of(pool), Kind::Region, Door::Heap, 1 << 20);
        let mut s = cache_scope_for(fp, Kind::Region);
        s.absorb(kept);
        let c = s.finish().expect("its charge");
        assert_eq!((c.owner(), c.committed()), (fp, 1 << 20), "the pool's bytes are the scope owner's");
    }

    #[test]
    fn a_take_past_the_line_wakes_the_ladder_once() {
        static WOKE: AtomicU64 = AtomicU64::new(0);
        set_wake_hook(|| {
            WOKE.fetch_add(1, Relaxed);
        });
        let base = ALLOC_TAKEN.load(Relaxed) + CACHE_TAKEN.load(Relaxed);
        set_line((base + (8 << 20)) as u64);
        note_taken(false, 4 << 20);
        assert_eq!(WOKE.load(Relaxed), 0, "under the line");
        note_taken(true, 8 << 20);
        assert_eq!(WOKE.load(Relaxed), 1, "past it: one wake");
        note_taken(false, 1 << 20);
        assert_eq!(WOKE.load(Relaxed), 1, "still due: no second wake");
        assert!(take_ladder_due());
        note_taken(false, 1 << 20);
        assert_eq!(WOKE.load(Relaxed), 2, "cleared and still past: wakes again");
        assert!(take_ladder_due());
        note_taken(false, -(14 << 20));
        note_taken(true, 0);
        assert!(!take_ladder_due());
        set_line(0);
    }
}
