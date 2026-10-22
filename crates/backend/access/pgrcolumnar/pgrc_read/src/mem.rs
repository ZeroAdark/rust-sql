//! Bounded decoded-column memory (C1 L5; the user's ruling 2026-09-26 #5).
//!
//! Every image a [`SegBuf`](crate::SegBuf) owns is a *chunk*, counted in exactly one class from its build to its
//! `Drop`:
//!
//! - **held**: in no part map. A Private read, an assembled region, a caller-scoped slice, a Cached read the cap
//!   could not admit, or an image a map let go of while someone still holds it.
//! - **cached**: admitted to a part map by a [`Residency::Cached`](crate::Residency) face. The CLOCK ring below
//!   evicts it once nothing but the map holds it.
//! - **sticky**: handed out or inserted by a [`Residency::Shared`](crate::Residency) face, and kept for the part's
//!   life, exactly as before L5 (the Gravity / SQE posture until G1).
//!
//! The cap ([`set_cap`], `pgrust.pgrc_cache_size`; 0 = no caching) bounds cached + held; sticky is reported beside
//! it. Pinning more than the cap is allowed and witnessed (`over_cap`), never an error.
//!
//! **Pin law.** A cached chunk is pinned iff its `Arc` count is above 1. The ring never holds a chunk, only a
//! [`Victim`] that reaches it through its part's state mutex, and it reads the count under that mutex. Every new
//! clone comes from the map (which needs the mutex) or from an existing holder (so the count is already above 1),
//! so a count of 1 under the mutex means no holder can appear before the removal. Lock order: ring, then part.
//!
//! **Datum lifetime** (binding on every reader): a pointer-class datum from a Cached or Private read is valid only
//! while the cursor that produced it lives, or a [`Guard`] that received its `hold_datums` has not been released.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::openpart::SegBuf;

pub(crate) const HELD: u8 = 0;
pub(crate) const CACHED: u8 = 1;
pub(crate) const STICKY: u8 = 2;

/// The cap's default (`pgrust.pgrc_cache_size`'s boot value).
pub const DEFAULT_CAP: u64 = 256 << 20;
/// Admissions that skip the sweep after a sweep that freed nothing (a cap pinned full costs O(1) per fault).
pub const FUTILE_SKIP: u32 = 64;

// Plain atomics, not pgsync's: they are statistics and the cap's input, touched on every chunk build and drop,
// including outside a loom model. The ring (below) is the modeled state.
//
// C1-HARDEN: the cap's operand, held + cached, is ONE location (`USED`), so a room check reads it whole: a class shift
// between held and cached never touches it, and one in flight cannot be half seen (the per-class pair it replaced
// could lose the moving bytes: read held, a concurrent unpublish moves them to held and out of cached, read cached).
// Held is `USED - CACHED`, a statistic. Bytes leave `CACHED` before they leave `USED` (`Release`), and a room check
// reads `USED` with `Acquire`: an admission that saw bytes go from the operand adds to cached after they left it.
static USED: AtomicU64 = AtomicU64::new(0);
static CACHED_BYTES: AtomicU64 = AtomicU64::new(0);
static STICKY_BYTES: AtomicU64 = AtomicU64::new(0);
static CAP: AtomicU64 = AtomicU64::new(DEFAULT_CAP);

pgsync::process_global! {
    /// The process's CLOCK ring over every Cached admission (and, at L5c, the colcache images).
    static RING: pgsync::Mutex<Clock> = pgsync::Mutex::new(Clock::new());
}

pub(crate) fn charge(len: usize) {
    USED.fetch_add(len as u64, Ordering::Relaxed);
}

pub(crate) fn credit(class: u8, len: usize) {
    let len = len as u64;
    match class {
        STICKY => {
            STICKY_BYTES.fetch_sub(len, Ordering::Relaxed);
        }
        _ => {
            if class == CACHED {
                CACHED_BYTES.fetch_sub(len, Ordering::Relaxed);
            }
            USED.fetch_sub(len, Ordering::Release);
        }
    }
}

/// Move `len` bytes from class `from` to `to`. Callers serialize the shifts of one chunk (a part's state mutex, a
/// [`Charge`]'s class mutex), so the counter moves of a chunk's flips land in the flips' order. Bytes join their new
/// class before they leave the old one, and join `USED` first / leave it last: a sample over-counts, never under.
pub(crate) fn shift(from: u8, to: u8, len: usize) {
    let len = len as u64;
    if from == STICKY && to != STICKY {
        USED.fetch_add(len, Ordering::Relaxed);
    }
    match to {
        CACHED => CACHED_BYTES.fetch_add(len, Ordering::Relaxed),
        STICKY => STICKY_BYTES.fetch_add(len, Ordering::Relaxed),
        _ => 0,
    };
    match from {
        CACHED => CACHED_BYTES.fetch_sub(len, Ordering::Relaxed),
        STICKY => STICKY_BYTES.fetch_sub(len, Ordering::Relaxed),
        _ => 0,
    };
    if to == STICKY && from != STICKY {
        USED.fetch_sub(len, Ordering::Release);
    }
}

/// Set the cap in bytes (`pgrust.pgrc_cache_size` at boot; 0 turns Cached into Private). A lower cap is enforced
/// at the next admission.
pub fn set_cap(bytes: u64) {
    CAP.store(bytes, Ordering::Relaxed);
}

pub fn cap() -> u64 {
    CAP.load(Ordering::Relaxed)
}

fn used() -> u64 {
    USED.load(Ordering::Acquire)
}

/// Admission's room check (§4 steps 2 and 5), for bytes already charged as held and with no part mutex held. True:
/// admit. False: the caller keeps its image held and uninserted (cap 0, or the sweep could not get under the cap).
pub fn make_room() -> bool {
    let cap = cap();
    if cap == 0 {
        return false;
    }
    if used() <= cap {
        return true;
    }
    let mut ring = pgsync::lock(&RING);
    let ok = ring.sweep(&|| used() > cap);
    ring.over_cap += u64::from(!ok);
    ok
}

/// Register an admitted entry's slot (§4 step 4), after its part mutex is released.
pub fn register(v: Box<dyn Victim>) {
    pgsync::lock(&RING).register(v);
}

/// One CLOCK visit's verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sweep {
    /// The entry is gone (part dropped, key removed or replaced): free the slot.
    Dead,
    /// A Shared face touched it: it never comes back, free the slot.
    Sticky,
    /// Its ref bit was set, and is now clear.
    Referenced,
    /// Someone besides its map holds it.
    Pinned,
    /// Removed from its map; this many bytes go when the last holder drops.
    Evicted(u64),
}

/// An evictable entry behind a ring slot.
pub trait Victim: Send {
    /// One CLOCK visit, under the owner's own lock (taken after the ring's).
    fn visit(&self) -> Sweep;
    /// The janitor's probe: true iff the slot can be freed (what `visit` would call `Dead` or `Sticky`).
    fn dead(&self) -> bool;
}

/// The CLOCK ring: one slot per admitted entry, a hand, a janitor cursor and the futility backoff. Always used
/// under the ring mutex; public for the colcache images (L5c) and the tests.
pub struct Clock {
    slots: Vec<Option<Box<dyn Victim>>>,
    free: Vec<usize>,
    hand: usize,
    jan: usize,
    skip: u32,
    pub admitted: u64,
    pub evicted: u64,
    pub evicted_bytes: u64,
    pub over_cap: u64,
    pub futile: u64,
}

impl Default for Clock {
    fn default() -> Clock {
        Clock::new()
    }
}

impl Clock {
    pub const fn new() -> Clock {
        Clock {
            slots: Vec::new(),
            free: Vec::new(),
            hand: 0,
            jan: 0,
            skip: 0,
            admitted: 0,
            evicted: 0,
            evicted_bytes: 0,
            over_cap: 0,
            futile: 0,
        }
    }

    /// Live slots.
    pub fn slots(&self) -> usize {
        self.slots.len() - self.free.len()
    }

    fn free_slot(&mut self, i: usize) {
        if self.slots[i].take().is_some() {
            self.free.push(i);
        }
    }

    /// Add a slot; first the janitor frees up to two dead ones, so parts that drop leave the ring O(live entries).
    pub fn register(&mut self, v: Box<dyn Victim>) {
        for _ in 0..2 {
            if self.slots.is_empty() {
                break;
            }
            let i = self.jan % self.slots.len();
            self.jan = i + 1;
            if self.slots[i].as_ref().is_some_and(|s| s.dead()) {
                self.free_slot(i);
            }
        }
        match self.free.pop() {
            Some(i) => self.slots[i] = Some(v),
            None => self.slots.push(Some(v)),
        }
        self.admitted += 1;
    }

    /// Advance the hand, at most two turns, while `over()`. True once not over. A sweep that frees nothing arms
    /// the backoff: the next [`FUTILE_SKIP`] calls that find themselves over return false without sweeping.
    pub fn sweep(&mut self, over: &dyn Fn() -> bool) -> bool {
        if !over() {
            return true;
        }
        if self.skip > 0 {
            self.skip -= 1;
            return false;
        }
        let n = self.slots.len();
        let mut freed = 0u64;
        // bounded("other: at most two turns of the clock hand over the cache's slots, in memory, no I/O")
        for _ in 0..2 * n {
            if !over() {
                return true;
            }
            let i = self.hand;
            self.hand = (i + 1) % n;
            match self.slots[i].as_ref().map(|v| v.visit()) {
                None | Some(Sweep::Referenced) | Some(Sweep::Pinned) => {}
                Some(Sweep::Dead) | Some(Sweep::Sticky) => self.free_slot(i),
                Some(Sweep::Evicted(b)) => {
                    self.free_slot(i);
                    self.evicted += 1;
                    self.evicted_bytes += b;
                    freed += b;
                }
            }
        }
        let ok = !over();
        if !ok && freed == 0 {
            self.skip = FUTILE_SKIP;
            self.futile += 1;
        }
        ok
    }
}

/// Holds chunks so the datums that alias them stay valid. Owned, not shared: parallel workers keep their own and
/// [`absorb`](Guard::absorb) into the leader's. What it holds counts against the cap (cached if admitted, else held).
#[derive(Debug, Default)]
pub struct Guard {
    held: Vec<SegBuf>,
}

impl Guard {
    pub const fn new() -> Guard {
        Guard { held: Vec::new() }
    }

    /// Hold `b` (one `Arc` clone; a chunk already held is not held twice).
    pub fn hold(&mut self, b: &SegBuf) {
        if !self.held.iter().any(|h| h.same(b)) {
            self.held.push(b.clone());
        }
    }

    /// Take over everything `other` holds (worker to leader, morsel to statement).
    pub fn absorb(&mut self, other: Guard) {
        // bounded("other: guard hand-over of one reader's held guards; must run to the end, a stop midway would drop holds")
        for b in other.held {
            self.hold(&b);
        }
    }

    /// The owner's safe point: every datum this guard kept valid dies here.
    pub fn release(&mut self) {
        self.held.clear();
    }

    pub fn bytes(&self) -> u64 {
        self.held.iter().map(|b| b.len() as u64).sum()
    }
}

/// The bytes of an image built outside `pgrc_read` (the colcache images and their settled keyings, L5c), in the
/// same classes as a chunk: held from its build, cached while its owner's map publishes it (the owner's ring slot is a
/// [`Victim`] of its own), credited at `Drop`.
#[derive(Debug)]
pub struct Charge {
    len: u64,
    /// C1-HARDEN: a mutex, not an atomic: a flip and its counter moves are one step, so an unpublish racing a
    /// re-publish of a shared chunk (an unmap against a refresh that reuses the chunk) cannot land its bytes' move
    /// after the re-publish's, which counted them cached twice until it did.
    class: pgsync::Mutex<u8>,
}

impl Charge {
    pub fn new(len: usize) -> Charge {
        charge(len);
        EXT_BYTES.fetch_add(len as u64, Ordering::Relaxed);
        Charge { len: len as u64, class: pgsync::Mutex::new(HELD) }
    }

    /// Its map published it: held → cached (a no-op when already cached).
    pub fn publish(&self) {
        self.flip(HELD, CACHED);
    }

    /// Its map let go of it while a holder may remain: cached → held.
    pub fn unpublish(&self) {
        self.flip(CACHED, HELD);
    }

    fn flip(&self, from: u8, to: u8) {
        let mut class = pgsync::lock(&self.class);
        if *class == from {
            shift(from, to, self.len as usize);
            *class = to;
        }
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        credit(*pgsync::lock(&self.class), self.len as usize);
        EXT_BYTES.fetch_sub(self.len, Ordering::Relaxed);
    }
}

static EXT_BYTES: AtomicU64 = AtomicU64::new(0);

/// ZONEPRUNE F2: the bytes of every part's decoded zone records (`OpenPart::zone_records`), counted sticky (kept for
/// the part's life, as a Shared-face section is) from their build to their drop, which is their part's.
static ZONE_BYTES: AtomicU64 = AtomicU64::new(0);

pub(crate) fn zone_charge(len: usize) {
    STICKY_BYTES.fetch_add(len as u64, Ordering::Relaxed);
    ZONE_BYTES.fetch_add(len as u64, Ordering::Relaxed);
}

pub(crate) fn zone_credit(len: usize) {
    ZONE_BYTES.fetch_sub(len as u64, Ordering::Relaxed);
    STICKY_BYTES.fetch_sub(len as u64, Ordering::Relaxed);
}

/// Live decoded zone-record bytes, every part's (of [`Stats::sticky`]).
pub fn zone_bytes() -> u64 {
    ZONE_BYTES.load(Ordering::Relaxed)
}

/// The residency plane's counters (`pgrc_cache_stats()` at L5b).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub cap: u64,
    pub cached: u64,
    pub held: u64,
    pub sticky: u64,
    /// Of `cached + held`, the [`Charge`]d bytes (the colcache images).
    pub image_bytes: u64,
    pub admitted: u64,
    pub evicted: u64,
    pub evicted_bytes: u64,
    pub over_cap: u64,
    pub futile: u64,
    pub slots: u64,
}

pub fn stats() -> Stats {
    let ring = pgsync::lock(&RING);
    let cached = CACHED_BYTES.load(Ordering::Relaxed);
    Stats {
        cap: cap(),
        cached,
        // Two loads: exact when quiescent, a statistic under concurrency.
        held: USED.load(Ordering::Relaxed).saturating_sub(cached),
        sticky: STICKY_BYTES.load(Ordering::Relaxed),
        image_bytes: EXT_BYTES.load(Ordering::Relaxed),
        admitted: ring.admitted,
        evicted: ring.evicted,
        evicted_bytes: ring.evicted_bytes,
        over_cap: ring.over_cap,
        futile: ring.futile,
        slots: ring.slots() as u64,
    }
}

/// Tests (this crate's and its readers'): sweep the process ring as if the cap were 0 (two turns: every unpinned
/// cached entry goes).
#[doc(hidden)]
pub fn sweep_all_for_tests() {
    let mut ring = pgsync::lock(&RING);
    ring.skip = 0;
    ring.sweep(&|| true);
    ring.skip = 0;
}
