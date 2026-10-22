//! What a cache's bytes are worth: each process cache's lookups counted at its door (hits, misses, the bytes a miss
//! brought back, the measured price of a miss), so a reclaim can weigh a cache's bytes by what they save. It counts;
//! it decides nothing (the choice of what to free is `mempolicy`'s).
//!
//! - **hit** / **miss**: a lookup the cache served / one it did not. The consumer's first miss and every
//!   [`SAMPLE`]th after it are timed on the missing thread, from the miss to the **fill** of the same key (the rebuilt
//!   value put back), wall nanoseconds: `cost_ns` over `cost_samples`. A fill adds its bytes whether or not its miss
//!   was timed. A timed miss whose fill never comes (a build that failed, a racing publisher that won) is overwritten
//!   by a later timed miss, never charged to another key's fill.
//! - **fault**: a miss and its fill at once, for a door that knows the bytes at the miss (the part cache's reads). A door
//!   whose misses are counted at the fault or in a tally [`arm`]s the timer where the miss starts: its first arm and every
//!   [`SAMPLE`]th open one.
//! - **re-miss** (the part cache): bytes faulted back into a part after an eviction freed bytes of it, up to the bytes
//!   freed: the price an eviction paid.
//!
//! Relaxed atomics on the consumer's own line. A hit is a thread-local add, folded onto the line every [`FOLD`] hits, at
//! the backend's statement end and at an `enter`ed account's end ([`flush`]): a thread holds back fewer than [`FOLD`] a
//! consumer between those. The timer is a
//! thread-local of [`TIMERS`] keyed slots a consumer, so a build that misses other keys inside it (a catalog entry's build
//! reading the catalog) keeps its own timer.

use core::cell::Cell;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::{Consumer, CONSUMERS};

/// A consumer's first miss and every `SAMPLE`th after it are timed.
pub const SAMPLE: u64 = 32;

/// The timed misses a thread keeps open a consumer (nested builds).
pub const TIMERS: usize = 4;

/// A thread's hits a consumer are folded onto the consumer's line every `FOLD`.
pub const FOLD: u32 = 64;

// `Local::held` has a bit per consumer
const _: () = assert!(CONSUMERS <= 32);

#[repr(align(64))]
struct Door {
    hits: AtomicU64,
    misses: AtomicU64,
    miss_bytes: AtomicU64,
    cost_ns: AtomicU64,
    cost_samples: AtomicU64,
    re_misses: AtomicU64,
    re_miss_bytes: AtomicU64,
    /// [`arm`]s so far: the first and every [`SAMPLE`]th open a timer.
    arms: AtomicU64,
}

#[allow(clippy::declare_interior_mutable_const)]
const DOOR0: Door = Door {
    hits: AtomicU64::new(0),
    misses: AtomicU64::new(0),
    miss_bytes: AtomicU64::new(0),
    cost_ns: AtomicU64::new(0),
    cost_samples: AtomicU64::new(0),
    re_misses: AtomicU64::new(0),
    re_miss_bytes: AtomicU64::new(0),
    arms: AtomicU64::new(0),
};

static DOORS: [Door; CONSUMERS] = [DOOR0; CONSUMERS];

/// One consumer's counters (a read of each, not one instant's).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub hits: u64,
    pub misses: u64,
    /// The bytes the fills brought back.
    pub miss_bytes: u64,
    /// The timed misses' wall nanoseconds, miss to fill, summed.
    pub cost_ns: u64,
    pub cost_samples: u64,
    pub re_misses: u64,
    pub re_miss_bytes: u64,
}

/// `(key, started)`: `started` the miss's `pg_clock::mono_ns`, 0 for no timer.
type Timer = Cell<(u64, u64)>;

#[allow(clippy::declare_interior_mutable_const)]
const NO_TIMER: Timer = Cell::new((0, 0));
#[allow(clippy::declare_interior_mutable_const)]
const NO_TIMERS: [Timer; TIMERS] = [NO_TIMER; TIMERS];
#[allow(clippy::declare_interior_mutable_const)]
const NEXT0: Cell<u8> = Cell::new(0);
#[allow(clippy::declare_interior_mutable_const)]
const HITS0: Cell<u32> = Cell::new(0);

struct Local {
    open: [[Timer; TIMERS]; CONSUMERS],
    next: [Cell<u8>; CONSUMERS],
    hits: [Cell<u32>; CONSUMERS],
    /// The consumers with hits held back (bit `c`): a flush with none held is one load, as every statement end runs one.
    held: Cell<u32>,
}

thread_local! {
    static LOCAL: Local = const { Local { open: [NO_TIMERS; CONSUMERS], next: [NEXT0; CONSUMERS], hits: [HITS0; CONSUMERS], held: Cell::new(0) } };
}

#[inline]
fn door(c: Consumer) -> &'static Door {
    &DOORS[c as usize]
}

/// A lookup the cache served.
#[inline]
pub fn hit(c: Consumer) {
    LOCAL.with(|l| {
        let h = &l.hits[c as usize];
        let n = h.get() + 1;
        if n >= FOLD {
            door(c).hits.fetch_add(u64::from(n), Ordering::Relaxed);
            h.set(0);
        } else {
            h.set(n);
            l.held.set(l.held.get() | 1 << c as u32);
        }
    });
}

/// Fold this thread's held-back hits onto the consumers' lines (the backend's statement end, `crate::statement_end`; an
/// `enter`ed account's end, as a worker's served statement's).
pub fn flush() {
    track_flush();
    LOCAL.with(|l| {
        let mut held = l.held.replace(0);
        while held != 0 {
            let k = held.trailing_zeros() as usize;
            held &= held - 1;
            let n = l.hits[k].replace(0);
            if n != 0 {
                DOORS[k].hits.fetch_add(u64::from(n), Ordering::Relaxed);
            }
        }
    });
}

/// `hits` and `misses` a door tallied itself (one add a flush, not a lookup); none of them timed.
#[inline]
pub fn tally(c: Consumer, hits: u64, misses: u64) {
    let d = door(c);
    if hits != 0 {
        d.hits.fetch_add(hits, Ordering::Relaxed);
    }
    if misses != 0 {
        d.misses.fetch_add(misses, Ordering::Relaxed);
    }
}

/// A lookup of `key` the cache did not serve; the consumer's first miss and every [`SAMPLE`]th open a timer for `key`
/// on this thread, which `key`'s [`fill`] ends.
#[inline]
pub fn miss(c: Consumer, key: u64) {
    let priced = door(c).misses.fetch_add(1, Ordering::Relaxed) % SAMPLE == 0;
    if priced || track_miss(c, key) {
        open(c, key, priced);
    }
}

/// `key`'s missed value back in the cache, `bytes` of it: ends `key`'s timer on this thread when its miss was timed.
#[inline]
pub fn fill(c: Consumer, key: u64, bytes: u64) {
    let d = door(c);
    d.miss_bytes.fetch_add(bytes, Ordering::Relaxed);
    if let Some((ns, priced)) = close(c, key) {
        if priced {
            d.cost_ns.fetch_add(ns, Ordering::Relaxed);
            d.cost_samples.fetch_add(1, Ordering::Relaxed);
        }
        track_fill(c, key, bytes, ns);
    }
}

/// A miss of `key` counted elsewhere (a [`fault`], a [`tally`]) starts: the consumer's first arm and every [`SAMPLE`]th
/// open `key`'s timer on this thread, which `key`'s fill ends.
#[inline]
pub fn arm(c: Consumer, key: u64) {
    let priced = door(c).arms.fetch_add(1, Ordering::Relaxed) % SAMPLE == 0;
    if priced || track_miss(c, key) {
        open(c, key, priced);
    }
}

/// A miss of `key` and its fill at once, `bytes` brought back (timed when [`arm`]ed).
#[inline]
pub fn fault(c: Consumer, key: u64, bytes: u64) {
    door(c).misses.fetch_add(1, Ordering::Relaxed);
    fill(c, key, bytes);
}

/// A miss its door does not time (its price is its `bytes` alone): counted with them.
#[inline]
pub fn missed(c: Consumer, bytes: u64) {
    let d = door(c);
    d.misses.fetch_add(1, Ordering::Relaxed);
    d.miss_bytes.fetch_add(bytes, Ordering::Relaxed);
}

/// `bytes` faulted back after an eviction freed them.
#[inline]
pub fn re_miss(c: Consumer, bytes: u64) {
    let d = door(c);
    d.re_misses.fetch_add(1, Ordering::Relaxed);
    d.re_miss_bytes.fetch_add(bytes, Ordering::Relaxed);
}

/// A timer's start with this bit set was opened for cache_track alone: its fill goes to the drain, not into the price
/// (`cost_ns` / `cost_samples` stay the periodic timers' unclustered mean, with the knob on or off).
const TRACK_ONLY: u64 = 1 << 63;

#[cold]
fn open(c: Consumer, key: u64, priced: bool) {
    let now = pg_clock::mono_ns().max(1) & !TRACK_ONLY;
    LOCAL.with(|t| {
        let k = c as usize;
        let slots = &t.open[k];
        // the key's own slot when it has one (a miss again before its fill), else the next in turn
        let i = slots.iter().position(|s| s.get().1 != 0 && s.get().0 == key).unwrap_or_else(|| {
            let i = t.next[k].get() as usize % TIMERS;
            t.next[k].set((i + 1) as u8);
            i
        });
        let (was_key, was_at) = slots[i].get();
        let priced = priced || (was_at != 0 && was_key == key && was_at & TRACK_ONLY == 0);
        slots[i].set((key, if priced { now } else { now | TRACK_ONLY }));
    });
}

/// `key`'s timer on this thread ended: its wall, and whether it prices (not [`TRACK_ONLY`]).
#[inline]
fn close(c: Consumer, key: u64) -> Option<(u64, bool)> {
    LOCAL.with(|t| {
        let s = t.open[c as usize].iter().find(|s| s.get().1 != 0 && s.get().0 == key)?;
        let (_, at) = s.replace((0, 0));
        Some((pg_clock::mono_ns().saturating_sub(at & !TRACK_ONLY), at & TRACK_ONLY == 0))
    })
}

/// Consumer `c`'s counters.
pub fn stats(c: Consumer) -> Stats {
    let d = door(c);
    let r = |a: &AtomicU64| a.load(Ordering::Relaxed);
    Stats {
        hits: r(&d.hits),
        misses: r(&d.misses),
        miss_bytes: r(&d.miss_bytes),
        cost_ns: r(&d.cost_ns),
        cost_samples: r(&d.cost_samples),
        re_misses: r(&d.re_misses),
        re_miss_bytes: r(&d.re_miss_bytes),
    }
}

/// Whether consumer `c` counts at a door (the views' NULL for the others).
pub const fn counted(c: Consumer) -> bool {
    matches!(c, Consumer::PartCache | Consumer::FpTables | Consumer::BuildPool | Consumer::ZoneStore | Consumer::CondCache | Consumer::CatalogL2)
}

/// Whether consumer `c`'s door records tail hits and ghosts under [`ext`] (the views' NULL for the others).
pub const fn ext_door(c: Consumer) -> bool {
    matches!(c, Consumer::PartCache | Consumer::CondCache | Consumer::FpTables | Consumer::BuildPool)
}

/// Tail-hit and ghost counters (the value-per-byte broker's loss and gain, `PGRUST_MEMPOLICY=broker_value=1`): off, no
/// door records any of them.
static EXT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Refault-distance buckets: bucket `k` counts ghost hits at a distance of `[2^k, 2^(k+1))` evicted bytes (0 in bucket 0).
pub const BUCKETS: usize = 40;

#[repr(align(64))]
struct Ext {
    /// The consumer's byte clock: the bytes it has evicted.
    clock: AtomicU64,
    ghost_hits: AtomicU64,
    tail_hits: AtomicU64,
    /// The bytes the published tail mark covers (the coldest tenth at the broker's last plan).
    tail_bytes: AtomicU64,
    hist: [AtomicU64; BUCKETS],
}

#[allow(clippy::declare_interior_mutable_const)]
const ZERO: AtomicU64 = AtomicU64::new(0);
#[allow(clippy::declare_interior_mutable_const)]
const EXT0: Ext = Ext { clock: ZERO, ghost_hits: ZERO, tail_hits: ZERO, tail_bytes: ZERO, hist: [ZERO; BUCKETS] };

static EXTS: [Ext; CONSUMERS] = [EXT0; CONSUMERS];

/// Switch the tail-hit and ghost counters (the server's start, from the policy's `broker_value` knob).
pub fn set_ext(on: bool) {
    EXT.store(on, Ordering::Relaxed);
}

/// Whether the doors record tail hits and ghosts.
#[inline]
pub fn ext() -> bool {
    EXT.load(Ordering::Relaxed)
}

/// `bytes` evicted by consumer `c`: its byte clock after them (the time stamp of the evicted entries' ghosts).
#[inline]
pub fn evicted(c: Consumer, bytes: u64) -> u64 {
    EXTS[c as usize].clock.fetch_add(bytes, Ordering::Relaxed) + bytes
}

/// Consumer `c`'s byte clock now.
#[inline]
pub fn clock(c: Consumer) -> u64 {
    EXTS[c as usize].clock.load(Ordering::Relaxed)
}

/// `n` ghost hits (a miss on what `c` evicted at clock `at`; `n` > 1 for a hash-sampled ghost table): the refault distance is
/// the bytes evicted since.
#[inline]
pub fn ghost(c: Consumer, at: u64, n: u64) {
    let e = &EXTS[c as usize];
    let d = e.clock.load(Ordering::Relaxed).saturating_sub(at);
    e.ghost_hits.fetch_add(n, Ordering::Relaxed);
    e.hist[bucket(d)].fetch_add(n, Ordering::Relaxed);
}

/// The refault-distance bucket of `d` bytes.
#[inline]
pub const fn bucket(d: u64) -> usize {
    let k = if d == 0 { 0 } else { (63 - d.leading_zeros()) as usize };
    if k < BUCKETS { k } else { BUCKETS - 1 }
}

/// A hit on an entry in `c`'s published tail (its coldest tenth of bytes).
#[inline]
pub fn tail_hit(c: Consumer) {
    EXTS[c as usize].tail_hits.fetch_add(1, Ordering::Relaxed);
}

/// The bytes consumer `c`'s newly published tail mark covers.
pub fn set_tail_bytes(c: Consumer, bytes: u64) {
    EXTS[c as usize].tail_bytes.store(bytes, Ordering::Relaxed);
}

/// Consumer `c`'s tail-hit and ghost counters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtStats {
    pub evicted_bytes: u64,
    pub ghost_hits: u64,
    pub tail_hits: u64,
    pub tail_bytes: u64,
    pub refault_log2: [u64; BUCKETS],
}

pub fn ext_stats(c: Consumer) -> ExtStats {
    let e = &EXTS[c as usize];
    let r = |a: &AtomicU64| a.load(Ordering::Relaxed);
    ExtStats {
        evicted_bytes: r(&e.clock),
        ghost_hits: r(&e.ghost_hits),
        tail_hits: r(&e.tail_hits),
        tail_bytes: r(&e.tail_bytes),
        refault_log2: core::array::from_fn(|k| r(&e.hist[k])),
    }
}

/// The entries [`coldest_tenth`] ranks at most (past them it keeps every `n / TAIL_CAP`th entry, rounded up, and scales
/// the bytes back): a fixed buffer, 64 KiB of zero pages until a refresh writes them.
pub const TAIL_CAP: usize = 4096;

/// [`coldest_tenth`]'s one buffer (each broker plan's tail refresh, on the memory watchdog's thread).
static TAIL_BUF: pgsync::Mutex<[(u64, u64); TAIL_CAP]> = pgsync::Mutex::new([(0, 0); TAIL_CAP]);

/// A cache's entries fed to [`coldest_tenth`]: [`TailFeed::push`] each one's `(order, bytes)`.
pub struct TailFeed<'a> {
    buf: &'a mut [(u64, u64)],
    len: usize,
    stride: usize,
    seen: usize,
}

impl TailFeed<'_> {
    /// An entry at `order` in its cache's eviction order (ascending: the lowest goes first) holding `bytes`.
    #[inline]
    pub fn push(&mut self, order: u64, bytes: u64) {
        if self.seen % self.stride == 0 && self.len < self.buf.len() {
            self.buf[self.len] = (order, bytes);
            self.len += 1;
        }
        self.seen += 1;
    }
}

/// `m` held now, or `None` when another thread holds it (poison-tolerant, as `pgsync::lock`): the tail refreshes' lock,
/// which never waits.
pub fn try_lock<T: ?Sized>(m: &pgsync::Mutex<T>) -> Option<pgsync::MutexGuard<'_, T>> {
    match m.try_lock() {
        Ok(g) => Some(g),
        Err(pgsync::TryLockError::Poisoned(e)) => Some(e.into_inner()),
        Err(pgsync::TryLockError::WouldBlock) => None,
    }
}

/// Broker (b): the coldest tenth of a cache's bytes, `(the order at it, the bytes up to it)`, over the `n` entries `feed`
/// pushes (its eviction order key and bytes each; past [`TAIL_CAP`] a stride's sample, its bytes scaled back). For the
/// tail refresh on the memory watchdog's thread: no allocation (a fixed buffer, sorted in place) and no wait (`None`
/// when another refresh holds the buffer; the caller keeps its last mark). `feed` runs with the buffer held: it takes
/// its cache's lock with `try_lock` only.
pub fn coldest_tenth(n: usize, feed: impl FnOnce(&mut TailFeed<'_>)) -> Option<(u64, u64)> {
    let mut g = try_lock(&TAIL_BUF)?;
    let stride = n.div_ceil(TAIL_CAP).max(1);
    let mut f = TailFeed { buf: &mut g[..], len: 0, stride, seen: 0 };
    feed(&mut f);
    let len = f.len;
    let kept = &mut g[..len];
    kept.sort_unstable();
    let tenth = kept.iter().map(|e| e.1).sum::<u64>() / 10;
    let (mut order, mut bytes) = (0u64, 0u64);
    for &(o, b) in kept.iter() {
        if bytes >= tenth {
            break;
        }
        (order, bytes) = (o, bytes + b);
    }
    Some((order, bytes.saturating_mul(stride as u64)))
}

/// A fixed direct-mapped ghost table of `N` slots: an evicted entry's key word and the byte clock at its eviction, no
/// allocation. A later miss of the same key takes its slot (once); a key whose slot another eviction took is not found.
pub struct Ghosts<const N: usize> {
    keys: [AtomicU64; N],
    at: [AtomicU64; N],
}

impl<const N: usize> Ghosts<N> {
    #[allow(clippy::declare_interior_mutable_const)]
    const SLOT: AtomicU64 = AtomicU64::new(0);

    pub const fn new() -> Ghosts<N> {
        Ghosts { keys: [Self::SLOT; N], at: [Self::SLOT; N] }
    }

    /// `key` evicted at clock `at` (a key word of 0 is stored as 1: 0 marks an empty slot).
    #[inline]
    pub fn record(&self, key: u64, at: u64) {
        let (k, i) = slot::<N>(key);
        self.at[i].store(at, Ordering::Relaxed);
        self.keys[i].store(k, Ordering::Relaxed);
    }

    /// The clock `key` was evicted at, when its ghost is still in its slot (taken: a ghost counts once).
    #[inline]
    pub fn take(&self, key: u64) -> Option<u64> {
        let (k, i) = slot::<N>(key);
        if self.keys[i].load(Ordering::Relaxed) != k {
            return None;
        }
        let at = self.at[i].load(Ordering::Relaxed);
        self.keys[i].compare_exchange(k, 0, Ordering::Relaxed, Ordering::Relaxed).ok().map(|_| at)
    }
}

impl<const N: usize> Default for Ghosts<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether ghost key `key` is in a one-in-`every` sample (`every` a power of two; 1: every key): its mixed top bits, so the
/// sample is one key in `every` whatever the keys' shape (a door whose ghost table holds a sample of its keys).
#[inline]
pub const fn sampled(key: u64, every: u64) -> bool {
    every <= 1 || key.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - every.trailing_zeros()) == 0
}

#[inline]
fn slot<const N: usize>(key: u64) -> (u64, usize) {
    let h = key.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    (key.max(1), (h >> 32) as usize % N)
}

// cache_track (cache plan C2): the doors' sample of each tracked cache's lookups, for its cost curve
// (`mempolicy::curve`). An unsampled lookup reads its consumer's threshold and compares the key's hash: no write. A
// sampled one goes to the thread's ring, handed over a batch at a time (statement end, a full ring) with one lock.

/// The doors' read-mostly words, on lines of their own (the drain writes them once a second): each consumer's sampling
/// threshold ([`door_pass`]; 0 = not tracked, no door samples) and its largest sampled entry (bytes: a miss, whose size
/// is not known yet, is timed when its key passes at this size).
#[repr(align(64))]
struct Track {
    t: [AtomicU64; CONSUMERS],
    max: [AtomicU64; CONSUMERS],
}

static TRACK: Track = Track { t: [ZERO; CONSUMERS], max: [ZERO; CONSUMERS] };
/// Samples a full hand-over buffer refused (the drain then lowers the thresholds).
static TRACK_DROPPED: AtomicU64 = AtomicU64::new(0);

/// The 24-bit sampling hash of a cache key.
#[inline]
pub fn hash24(key: u64) -> u32 {
    ((key ^ (key >> 31)).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 40) as u32
}

/// Whether a lookup of `bytes` with this hash is in the sample at threshold `t` (one shift, one multiply): sampled iff
/// `hash24 / bytes < t / 2^40`, so a key's rate grows with its size (byte-weighted SHARDS).
#[inline]
pub fn door_pass(h24: u32, bytes: u64, t: u64) -> bool {
    (u64::from(h24) << 16) < t.saturating_mul(bytes)
}

/// The size a door that knows no entry size at its lookups samples at (bytes 0): its keys are sampled uniformly.
pub const UNIFORM: u64 = 1 << 16;

/// A reference ([`lookup`]).
pub const REF: u8 = 0;
/// A timed fill of a sampled key: `ns` is its refill time.
pub const FILL: u8 = 1;
/// A batch's hand-over: `key` is its `pg_clock::mono_ns`; the samples after it, up to the next, are its batch.
pub const MARK: u8 = 2;
/// Bytes read back into the cache for a sampled key ([`reloaded`]): `bytes`, untimed.
pub const RELOAD: u8 = 3;

/// One sampled door event.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sample {
    pub key: u64,
    pub bytes: u32,
    pub ns: u32,
    /// The key's 24-bit sampling hash as its door computed it ([`hash24`], or the door's own: [`lookup_hashed`]).
    pub h: u32,
    pub c: u8,
    pub kind: u8,
}

/// A thread's ring: this many samples, then a hand-over.
pub const RING: usize = 64;

#[allow(clippy::declare_interior_mutable_const)]
const NO_SAMPLE: Cell<Sample> = Cell::new(Sample { key: 0, bytes: 0, ns: 0, h: 0, c: 0, kind: 0 });

struct Ring {
    n: Cell<u32>,
    s: [Cell<Sample>; RING],
}

// A thread's exit loses the < RING samples its ring still holds, uncounted: the ring has no Drop (no TLS destructor) by
// design, and a sample is an estimate's input, not an account.
thread_local! {
    static RINGS: Ring = const { Ring { n: Cell::new(0), s: [NO_SAMPLE; RING] } };
}

/// The hand-over buffer: allocated once by [`track_start`] on the drain's thread (charged to its account), swapped out
/// whole by [`track_take`]; never grows.
static SHARED: pgsync::Mutex<Vec<Sample>> = pgsync::Mutex::new(Vec::new());

#[inline]
fn sat32(x: u64) -> u32 {
    x.min(u64::from(u32::MAX)) as u32
}

/// A lookup of `key`, whose entry is `bytes` large (0: the door does not know; sampled at [`UNIFORM`]), at consumer
/// `c`'s door, hit or miss: sampled when tracked.
///
/// Off (every threshold 0, the default) this is one relaxed load and a branch inlined at the door. On, an unsampled
/// lookup adds the key's hash ([`hash24`]: a shift, a xor, a multiply) and the door's compare ([`door_pass`]), inlined,
/// with no call; only a sampled one calls out ([`push_ref`]), as at [`lookup_hashed`]. MEASURED (lane flipcost, Arm, a
/// grouping query at W0, 6,972 lookups a statement, 10-18): with a cold call on every lookup, cache_track = 1 cost it
/// +0.24M instructions a statement once the doors had settled and +0.60M in a fresh server's first second (every door
/// still samples at its start threshold until the drain's first rate cut); inlined, +0.15M and +0.40M.
#[inline(always)]
pub fn lookup(c: Consumer, key: u64, bytes: u64) {
    let t = TRACK.t[c as usize].load(Ordering::Relaxed);
    if t != 0 {
        let h = hash24(key);
        if door_pass(h, if bytes == 0 { UNIFORM } else { bytes }, t) {
            push_ref(c, key, h, bytes);
        }
    }
}

/// [`lookup`] at a door that already holds a uniform hash of its key: `h24`, its top 24 bits, a function of `key` alone
/// (shared_buffers: the buffer mapping's hash of the tag, which BufferAlloc computed for its own lookup). The key's
/// sampling hash is `h24` from the door to the tracker (the sample carries it). Off: one relaxed load and a branch; on, an
/// unsampled lookup adds a shift, a multiply and a compare, inlined, no call and no hash; only a sampled one calls out.
#[inline(always)]
pub fn lookup_hashed(c: Consumer, key: u64, h24: u32, bytes: u64) {
    let t = TRACK.t[c as usize].load(Ordering::Relaxed);
    if t != 0 && door_pass(h24, if bytes == 0 { UNIFORM } else { bytes }, t) {
        push_ref(c, key, h24, bytes);
    }
}

#[cold]
#[inline(never)]
fn push_ref(c: Consumer, key: u64, h24: u32, bytes: u64) {
    debug_assert!(h24 < 1 << 24, "a 24-bit sampling hash");
    push(Sample { key, bytes: sat32(bytes), ns: 0, h: h24, c: c as u8, kind: REF });
}

/// cache plan A6: `bytes` read back into the cache for the entry its door looks up as `key` (a part cache image, a decoded
/// dictionary block), for the tracker alone (untimed; the door's own miss counters are not touched). The bytes a miss
/// really reloads are what the statement reads back, not the entry's whole size: the tracker sums them per sampled key
/// between its references (`mempolicy::curve::Tracker::reload`). Off: one relaxed load and a branch.
#[inline(always)]
pub fn reloaded(c: Consumer, key: u64, bytes: u64) {
    let t = TRACK.t[c as usize].load(Ordering::Relaxed);
    if t != 0 {
        reloaded_tracked(c, key, bytes, t);
    }
}

#[cold]
#[inline(never)]
fn reloaded_tracked(c: Consumer, key: u64, bytes: u64, t: u64) {
    let h = hash24(key);
    // a key's lookups pass at its door's size, its reloads (a block of it) are smaller: they pass at the largest sampled
    // entry's, so every sampled key's reloads reach the tracker (which drops the keys it does not hold)
    if door_pass(h, TRACK.max[c as usize].load(Ordering::Relaxed).max(UNIFORM), t) {
        push(Sample { key, bytes: sat32(bytes), ns: 0, h, c: c as u8, kind: RELOAD });
    }
}

/// Whether a miss of `key` (its size unknown yet) is timed for the tracker.
#[inline]
fn track_miss(c: Consumer, key: u64) -> bool {
    let t = TRACK.t[c as usize].load(Ordering::Relaxed);
    t != 0 && door_pass(hash24(key), TRACK.max[c as usize].load(Ordering::Relaxed).max(UNIFORM), t)
}

#[inline]
fn track_fill(c: Consumer, key: u64, bytes: u64, ns: u64) {
    let t = TRACK.t[c as usize].load(Ordering::Relaxed);
    let h = hash24(key);
    if t != 0 && door_pass(h, bytes.max(UNIFORM), t) {
        push(Sample { key, bytes: sat32(bytes), ns: sat32(ns), h, c: c as u8, kind: FILL });
    }
}

#[cold]
fn push(s: Sample) {
    RINGS.with(|r| {
        let n = r.n.get() as usize;
        r.s[n].set(s);
        r.n.set(n as u32 + 1);
        if n + 1 == RING {
            hand_over(r);
        }
    });
}

/// The ring's samples into the hand-over buffer behind one mark: one lock a batch. A full buffer drops the batch.
#[cold]
fn hand_over(r: &Ring) {
    let n = r.n.replace(0) as usize;
    if n == 0 {
        return;
    }
    let at = pg_clock::mono_ns();
    let mut g = pgsync::lock(&SHARED);
    if g.capacity() - g.len() < n + 1 {
        TRACK_DROPPED.fetch_add(n as u64, Ordering::Relaxed);
        return;
    }
    g.push(Sample { key: at, kind: MARK, ..Sample::default() });
    g.extend(r.s[..n].iter().map(Cell::get));
}

/// This thread's ring handed over now ([`flush`]: the statement's end).
#[inline]
fn track_flush() {
    RINGS.with(|r| {
        if r.n.get() != 0 {
            hand_over(r);
        }
    });
}

/// The drain starts: a hand-over buffer of `cap` samples, allocated on the calling thread. False when it cannot be had.
/// Runs on the drain's thread only (`mempolicy::track::tick`, the memory watchdog's sampler): the buffer is charged to
/// that thread's account, and [`track_stop`] frees it there.
pub fn track_start(cap: usize) -> bool {
    let mut g = pgsync::lock(&SHARED);
    if g.capacity() >= cap {
        return true;
    }
    let mut v = Vec::new();
    if v.try_reserve_exact(cap).is_err() {
        return false;
    }
    *g = v;
    true
}

/// The drain stops: no door samples, the buffer freed (rings still holding samples hand them to no buffer: dropped).
/// Runs on the drain's thread only, where [`track_start`] allocated the buffer.
pub fn track_stop() {
    TRACK.t.iter().for_each(|t| t.store(0, Ordering::Relaxed));
    *pgsync::lock(&SHARED) = Vec::new();
}

/// Consumer `c`'s threshold (0 = not tracked) and its largest sampled entry, published by the drain.
pub fn track_set(c: Consumer, t: u64, max_bytes: u64) {
    TRACK.max[c as usize].store(max_bytes, Ordering::Relaxed);
    TRACK.t[c as usize].store(t, Ordering::Relaxed);
}

/// Consumer `c`'s threshold now.
pub fn track_of(c: Consumer) -> u64 {
    TRACK.t[c as usize].load(Ordering::Relaxed)
}

/// The samples handed over since the last take, swapped into `into` (cleared first, the same capacity as the shared
/// buffer: the drain's second buffer). No copy; the lock is held for the swap only. False (nothing taken) when the
/// capacities differ: the drain has not started, or stopped.
pub fn track_take(into: &mut Vec<Sample>) -> bool {
    into.clear();
    let mut g = pgsync::lock(&SHARED);
    if into.capacity() != g.capacity() {
        return false;
    }
    core::mem::swap(&mut *g, into);
    true
}

/// The hand-over buffer's bytes (0 while the drain is stopped).
pub fn track_buffer_bytes() -> u64 {
    (pgsync::lock(&SHARED).capacity() * core::mem::size_of::<Sample>()) as u64
}

/// The doors' batch clock (`pg_clock::mono_ns`), for the drain.
pub fn track_now() -> u64 {
    pg_clock::mono_ns()
}

/// Samples dropped so far (a full hand-over buffer).
pub fn track_dropped() -> u64 {
    TRACK_DROPPED.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    // each test its own consumer: the counters are process-wide and the tests run in parallel

    #[test]
    fn tracked_lookups_sample_by_hash_and_hand_over_in_batches() {
        let c = Consumer::CondCache;
        let mine = |got: &Vec<Sample>, kind: u8| got.iter().filter(|s| s.kind == kind && s.c == c as u8).count();
        assert!(track_start(4096));
        let mut got = Vec::with_capacity(4096);
        flush();
        track_take(&mut got);
        for k in 0..1000 {
            lookup(c, k, 4096);
        }
        flush();
        track_take(&mut got);
        assert_eq!(mine(&got, REF), 0, "not tracked: no sample");
        track_set(c, 1 << 40, 4096);
        for k in 0..100 {
            lookup(c, k, 4096);
        }
        flush();
        track_take(&mut got);
        assert_eq!(mine(&got, REF), 100, "the threshold that passes every key");
        assert_eq!(got.iter().filter(|s| s.kind == MARK).count(), 2, "a full ring, then the flush");
        track_set(c, (1 << 40) / 4096 / 16, 4096);
        for k in 0..16_000u64 {
            lookup(c, k.wrapping_mul(0x9E37) + 1, 4096);
        }
        flush();
        track_take(&mut got);
        let n = mine(&got, REF);
        assert!((800..1200).contains(&n), "one key in 16: {n}");
        track_set(c, 1 << 40, 4096);
        miss(c, 7);
        fill(c, 7, 4096);
        flush();
        track_take(&mut got);
        assert!(got.iter().any(|s| s.kind == FILL && s.key == 7 && s.c == c as u8), "a sampled miss's refill");
        // a door that holds its own hash of the key samples by it, and the sample carries it to the tracker
        let t = (1u64 << 40) / 4096 / 16;
        track_set(c, t, 4096);
        let h_of = |k: u64| ((k.wrapping_mul(4099)) & 0xFF_FFFF) as u32;
        let want = (0..4096u64).filter(|&k| door_pass(h_of(k), 4096, t)).count();
        for k in 0..4096u64 {
            lookup_hashed(c, k << 32, h_of(k), 4096);
        }
        flush();
        track_take(&mut got);
        let refs: Vec<&Sample> = got.iter().filter(|s| s.kind == REF && s.c == c as u8).collect();
        assert_eq!(refs.len(), want, "sampled by the door's hash");
        assert!((150..400).contains(&want), "about one key in 16: {want}");
        assert!(refs.iter().all(|s| s.h == h_of(s.key >> 32)), "the sample carries the door's hash");
        // A6: a reload passes at the largest sampled entry's size (here 1 MiB at 1 in 16), untimed and uncounted
        let t = 1u64 << 16;
        track_set(c, t, 1 << 20);
        let before = stats(c);
        let want = (0..4096u64).filter(|&k| door_pass(hash24(k), 1 << 20, t)).count();
        assert!((150..400).contains(&want), "about one key in 16: {want}");
        for k in 0..4096u64 {
            reloaded(c, k, 64);
        }
        flush();
        track_take(&mut got);
        let rel: Vec<&Sample> = got.iter().filter(|s| s.kind == RELOAD && s.c == c as u8).collect();
        assert_eq!(rel.len(), want, "sampled as the key's lookups at the largest size");
        assert!(rel.iter().all(|s| s.bytes == 64 && s.ns == 0), "its bytes, untimed");
        assert_eq!(stats(c).misses, before.misses, "the door's misses untouched");
        track_set(c, 0, 0);
    }

    #[test]
    fn the_door_test_weighs_by_size() {
        let t = (1u64 << 40) / 8192 / 4;
        let pass = |bytes: u64| (0..1u64 << 14).filter(|&k| door_pass(hash24(k), bytes, t)).count() as f64 / (1 << 14) as f64;
        assert!((pass(8192) - 0.25).abs() < 0.02);
        assert!((pass(16384) - 0.5).abs() < 0.02);
        assert_eq!(pass(32768), 1.0);
    }

    #[test]
    fn the_first_miss_and_every_sampleth_are_timed_to_their_fill() {
        let c = Consumer::ProgramCaches;
        let before = stats(c);
        for i in 0..2 * SAMPLE {
            miss(c, i);
            fill(c, i, 10);
        }
        let s = stats(c);
        assert_eq!(s.misses - before.misses, 2 * SAMPLE);
        assert_eq!(s.miss_bytes - before.miss_bytes, 20 * SAMPLE);
        assert_eq!(s.cost_samples - before.cost_samples, 2, "the first miss and the SAMPLEth");
    }

    /// A miss timed for cache_track alone goes to the drain, not into the price: the price stays the periodic timers'.
    #[test]
    fn a_track_only_timer_never_prices() {
        let c = Consumer::PartCache;
        let before = stats(c);
        track_set(c, 1 << 40, 64);
        for i in 0..2 * SAMPLE {
            miss(c, i);
            fill(c, i, 64);
        }
        track_set(c, 0, 0);
        flush();
        let s = stats(c);
        assert_eq!(s.misses - before.misses, 2 * SAMPLE);
        assert_eq!(s.cost_samples - before.cost_samples, 2, "every miss timed, two priced: the first and the SAMPLEth");
    }

    #[test]
    fn hits_fold_every_fold_and_at_a_flush() {
        let c = Consumer::CatalogL2;
        flush();
        let before = stats(c).hits;
        for _ in 0..FOLD - 1 {
            hit(c);
        }
        assert_eq!(stats(c).hits, before, "held back on the thread");
        hit(c);
        assert_eq!(stats(c).hits - before, u64::from(FOLD));
        hit(c);
        flush();
        assert_eq!(stats(c).hits - before, u64::from(FOLD) + 1);
    }

    #[test]
    fn a_fill_of_another_key_never_ends_a_timer() {
        let c = Consumer::WorkPool;
        let before = stats(c);
        miss(c, 7);
        fill(c, 8, 5);
        assert_eq!(stats(c).cost_samples, before.cost_samples, "key 8 had no timer");
        fill(c, 7, 5);
        let s = stats(c);
        assert_eq!(s.cost_samples - before.cost_samples, 1);
        assert_eq!(s.miss_bytes - before.miss_bytes, 10);
        fill(c, 7, 0);
        assert_eq!(stats(c).cost_samples - before.cost_samples, 1, "a timer ends once");
    }

    #[test]
    fn a_nested_timed_miss_keeps_the_outer_timer() {
        let c = Consumer::PerBackend;
        let before = stats(c);
        miss(c, 1);
        for i in 0..SAMPLE {
            miss(c, 100 + i);
            fill(c, 100 + i, 1);
        }
        fill(c, 1, 1);
        assert_eq!(stats(c).cost_samples - before.cost_samples, 2, "the outer miss and the inner SAMPLEth both timed");
    }

    #[test]
    fn faults_arm_hits_tally_and_re_misses_count() {
        let c = Consumer::SharedBuffers;
        let before = stats(c);
        arm(c, 3);
        fault(c, 3, 64);
        hit(c);
        tally(c, 2, 3);
        flush();
        re_miss(c, 16);
        let s = stats(c);
        assert_eq!((s.hits - before.hits, s.misses - before.misses), (3, 4));
        assert_eq!((s.miss_bytes - before.miss_bytes, s.cost_samples - before.cost_samples), (64, 1));
        assert_eq!((s.re_misses - before.re_misses, s.re_miss_bytes - before.re_miss_bytes), (1, 16));
    }
    #[test]
    fn refault_distances_bucket_by_log2() {
        assert_eq!([bucket(0), bucket(1), bucket(2), bucket(3), bucket(4)], [0, 0, 1, 1, 2]);
        assert_eq!(bucket((1 << 20) + 5), 20);
        assert_eq!(bucket(u64::MAX), BUCKETS - 1, "past the last bucket: the last");
    }

    #[test]
    fn a_ghost_hit_counts_the_bytes_evicted_since_its_eviction() {
        // the ext counters are their own: a consumer no other ext test uses
        let c = Consumer::ZoneStore;
        let before = ext_stats(c);
        let at = evicted(c, 100);
        assert_eq!(at, before.evicted_bytes + 100);
        evicted(c, 1000);
        ghost(c, at, 1);
        ghost(c, clock(c), 8);
        let s = ext_stats(c);
        assert_eq!(s.evicted_bytes - before.evicted_bytes, 1100);
        assert_eq!(s.ghost_hits - before.ghost_hits, 9, "a sampled ghost counts its weight");
        assert_eq!(s.refault_log2[9] - before.refault_log2[9], 1, "1000 bytes later: [512, 1024)");
        assert_eq!(s.refault_log2[0] - before.refault_log2[0], 8, "no bytes evicted since: bucket 0");
    }

    #[test]
    fn tail_hits_and_the_tail_mark_count() {
        let c = Consumer::BuildPool;
        let before = ext_stats(c).tail_hits;
        tail_hit(c);
        tail_hit(c);
        set_tail_bytes(c, 4096);
        let s = ext_stats(c);
        assert_eq!((s.tail_hits - before, s.tail_bytes), (2, 4096));
    }

    /// The tail refreshes' walk: the coldest tenth in ascending order whatever order the entries come in, a stride's
    /// sample past the buffer (its bytes scaled back), and no wait: `None` while the buffer or the lock is held. One test:
    /// the buffer is process-wide.
    #[test]
    fn the_coldest_tenth_ranks_in_a_fixed_buffer_and_never_waits() {
        fn feed(n: u64) -> impl FnOnce(&mut TailFeed<'_>) {
            move |f| (0..n).rev().for_each(|i| f.push(100 + i, 10))
        }
        assert_eq!(coldest_tenth(10, feed(10)), Some((100, 10)), "ten equal entries: the lowest order alone");
        assert_eq!(coldest_tenth(20, feed(20)), Some((101, 20)));
        assert_eq!(coldest_tenth(0, |_| {}), Some((0, 0)), "an empty cache");
        let n = 2 * TAIL_CAP as u64 + 1;
        let (order, bytes) = coldest_tenth(n as usize, feed(n)).expect("free");
        assert!((100..100 + n / 5).contains(&order), "about the tenth: {order}");
        assert!(bytes >= n * 10 / 10 && bytes <= n * 10 / 10 + 3 * 10 * 3, "a stride of 3's sample, scaled back: {bytes}");
        let held = TAIL_BUF.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(coldest_tenth(10, feed(10)), None, "the buffer busy: no wait");
        assert!(try_lock(&TAIL_BUF).is_none());
        drop(held);
        assert!(try_lock(&TAIL_BUF).is_some());
    }

    #[test]
    fn a_sample_keeps_about_one_key_in_every_whatever_their_shape() {
        // keys differing only in their high bits (a door's xor of rotated words) still sample about one in eight
        let n = (0..4096u64).filter(|&i| sampled(70 ^ i.rotate_left(17), 8)).count();
        assert!((256..768).contains(&n), "{n} of 4096");
        assert!((0..100u64).all(|k| sampled(k, 1)), "one in one: every key");
    }

    #[test]
    fn a_ghost_is_found_once_by_its_own_key() {
        static G: Ghosts<64> = Ghosts::new();
        G.record(5, 10);
        G.record(0, 3);
        assert_eq!(G.take(6), None, "another key");
        assert_eq!(G.take(5), Some(10));
        assert_eq!(G.take(5), None, "taken: a ghost counts once");
        assert_eq!(G.take(0), Some(3), "key 0 is kept as 1 in its own slot");
        G.record(9, 1);
        G.record(9, 2);
        assert_eq!(G.take(9), Some(2), "a re-eviction restamps the ghost");
    }
}
