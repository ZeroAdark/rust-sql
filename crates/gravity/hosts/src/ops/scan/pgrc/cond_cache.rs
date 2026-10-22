//! Immutable-part predicate reuse keyed by mode, predicate fingerprint and part identity.
//! Entries serve only their original live part owner. Granule mode keeps only no-match states; rows mode also keeps
//! row bitmaps; full mode additionally stores survivor counts, totals and sparse row lists. Visibility is applied after
//! lookup. Publication uses Release/Acquire, and the first identical predicate evaluation wins.
//! LRU or W-TinyLFU bounds resident bytes under `pgrust.condition_cache_size`.
use std::any::Any;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Weak};

use pgsync::{Mutex, OnceLock};
use guc_tables::ConditionCacheMode;

use crate::tinylfu;

/// The set bits of `words` (`count` of them) as ascending row ordinals.
fn survivors(words: &[u64], count: usize) -> Box<[u16]> {
    let mut out = Vec::with_capacity(count); // alloc: Cold (one publish per granule, freed with the entry)
    // bounded("small-fixed: the mask words of one granule, at most VALIDITY_WORDS (128)")
    for (w, &m) in words.iter().enumerate() {
        let mut m = m;
        // bounded("small-fixed: the set bits of one 64-bit word")
        while m != 0 {
            out.push((w * 64) as u16 + m.trailing_zeros() as u16);
            m &= m - 1;
        }
    }
    out.into_boxed_slice()
}

const UNKNOWN: u8 = 0;
const NONE: u8 = 1;
const ALL: u8 = 2;
const MIXED: u8 = 3;

/// A granule's cached verdict: `None` = no row passes (skipped unread), `All` = every row passes, `Mixed` = the words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit<'a> {
    Unknown,
    None,
    All,
    Mixed(&'a [u64]),
}

/// A mixed granule's cached value: its words, and its survivors as ascending row ordinals when it kept them (empty
/// otherwise). One cell, published once: the list is derived from these words and lives exactly as long.
struct Mixed {
    words: Box<[u64]>,
    rows: Box<[u16]>,
}

/// An entry's `charge` bit: it is resident (its bytes count in `HELD`).
const LIVE: u64 = 1 << 63;
/// cachepolicy (C5): an entry's `charge` bits 61..62, its segment (`tinylfu::WINDOW` / `PROBATION` / `PROTECTED`; the
/// window always under lru), in the word with its bytes and `LIVE`, so a publish racing a move counts its bytes in
/// exactly one segment's total (`SEG_HELD`).
const SEG_SHIFT: u32 = 61;
const SEG: u64 = 3 << SEG_SHIFT;
const META: u64 = LIVE | SEG;

/// The segment a `charge` word names.
#[inline]
const fn seg_of(charge: u64) -> usize {
    ((charge & SEG) >> SEG_SHIFT) as usize
}

/// One unit's granules under one fingerprint.
pub struct Entry {
    /// G1-A + C8 move 7: its bytes on the condition cache's charge (`Cache(CondCache)`, door 1), first so its drop (the
    /// last `Arc`) moves them to the dropping thread's account just before the fields below free.
    held: Mutex<memledger::Charge>,
    owner: Weak<dyn Any + Send + Sync>,
    state: Box<[AtomicU8]>,
    words: Box<[OnceLock<Mixed>]>,
    wpg: usize,
    /// V4R-33: granule `g`'s survivor count, written before its state's `Release` (read after an `Acquire` of a known state).
    counts: Option<Counts>,
    mode: ConditionCacheMode,
    /// Its bytes, `LIVE` while resident and its segment (`SEG`): one word, so a publish racing an eviction is counted in
    /// `HELD` exactly once.
    charge: AtomicU64,
    /// Its key in the store's order (`order_key`: its segment and its last use's tick; written under the store lock).
    used: AtomicU64,
}

/// G1-A: the condition cache's owner in the memory ledger (its account `Acct::of(Consumer::CondCache)`). An entry's bytes
/// are committed to its charge from the building thread's account as it counts them (`new`; `put`'s words and row list
/// are charged as they are taken, in its cache scope, and join the charge at the publish), and move back to the
/// dropping thread's at its drop (the last `Arc`), just before they free: an entry unlinked while a source still holds it
/// stays on the account while it is resident.
const OWNER: memledger::Owner = memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::CondCache));

struct Counts {
    granules: Box<[AtomicU32]>,
    known: AtomicU32,
    total: AtomicU64,
}

impl Entry {
    fn new(owner: &Arc<dyn Any + Send + Sync>, granules: usize, wpg: usize, mode: ConditionCacheMode) -> Entry {
        let counts = (mode == ConditionCacheMode::Full).then(|| Counts { granules: (0..granules).map(|_| AtomicU32::new(0)).collect(), known: AtomicU32::new(0), total: AtomicU64::new(0) }); // alloc: Cold
        let state = (0..granules).map(|_| AtomicU8::new(UNKNOWN)).collect(); // alloc: Cold
        let words = (0..if mode >= ConditionCacheMode::Rows { granules } else { 0 }).map(|_| OnceLock::new()).collect(); // alloc: Cold
        let base = Self::base_bytes(granules, mode);
        let held = Mutex::new(memledger::Charge::open(OWNER, memledger::Release::Evict, memledger::Kind::Region, memledger::charge::Door::Heap, base));
        Entry { held, owner: Arc::downgrade(owner), state, words, wpg, counts, mode, charge: AtomicU64::new(base), used: AtomicU64::new(0) }
    }

    /// Charge the Arc allocation and only the per-granule arrays present in this mode.
    fn base_bytes(granules: usize, mode: ConditionCacheMode) -> u64 {
        use core::mem::size_of;
        let per = size_of::<AtomicU8>() + if mode >= ConditionCacheMode::Rows { size_of::<OnceLock<Mixed>>() } else { 0 } + if mode == ConditionCacheMode::Full { size_of::<AtomicU32>() } else { 0 };
        (2 * size_of::<usize>() + size_of::<Entry>() + granules * per) as u64
    }

    fn bytes(&self) -> u64 {
        self.charge.load(Ordering::Relaxed) & !META
    }

    pub fn get(&self, g: usize) -> Hit<'_> {
        match self.state.get(g).map_or(UNKNOWN, |s| s.load(Ordering::Acquire)) {
            NONE => Hit::None,
            ALL => Hit::All,
            MIXED => self.words[g].get().map_or(Hit::Unknown, |m| Hit::Mixed(&m.words)),
            _ => Hit::Unknown,
        }
    }

    /// The full-mode survivor count before delete vectors, `None` in other modes or while unknown.
    pub fn count(&self, g: usize) -> Option<u32> {
        let counts = self.counts.as_ref()?;
        match self.state.get(g).map_or(UNKNOWN, |s| s.load(Ordering::Acquire)) {
            UNKNOWN => None,
            _ => Some(counts.granules[g].load(Ordering::Relaxed)),
        }
    }

    /// The full-mode survivor total, `None` in other modes or until every granule is published.
    pub fn part_total(&self) -> Option<u64> {
        let counts = self.counts.as_ref()?;
        (counts.known.load(Ordering::Acquire) as usize == self.state.len()).then(|| counts.total.load(Ordering::Relaxed))
    }

    /// Granule `g`'s survivors as ascending row ordinals when it is a mixed granule that kept them (`put`'s `list_max`):
    /// the rows its `Hit::Mixed` words set, nothing else.
    pub fn rows(&self, g: usize) -> Option<&[u16]> {
        if self.state.get(g).map_or(UNKNOWN, |s| s.load(Ordering::Acquire)) != MIXED {
            return None;
        }
        self.words[g].get().map(|m| &m.rows[..]).filter(|r| !r.is_empty())
    }

    /// Publish only the mode's permitted payload, ignoring bits past `rows`. Full mode additionally keeps sparse row
    /// lists up to `list_max`; granule mode publishes only an empty verdict.
    pub fn put(&self, g: usize, words: &[u64], rows: usize, list_max: usize) {
        if self.mode == ConditionCacheMode::Off {
            return;
        }
        let Some(s) = self.state.get(g) else { return };
        if s.load(Ordering::Acquire) != UNKNOWN || rows == 0 || words.len() < rows.div_ceil(64) || rows > self.wpg * 64 {
            return;
        }
        let mut filled = 0u64;
        let n = rows.div_ceil(64);
        let tail = if rows % 64 == 0 { !0u64 } else { (1u64 << (rows % 64)) - 1 };
        let last = |i: usize, w: u64| if i + 1 == n { w & tail } else { w };
        let any = words[..n].iter().enumerate().any(|(i, &w)| last(i, w) != 0);
        if self.mode == ConditionCacheMode::Granule && any {
            return;
        }
        let every = words[..n].iter().enumerate().all(|(i, &w)| last(i, w) == if i + 1 == n { tail } else { !0 });
        let mut survivors_n = 0u32;
        let v = if !any {
            NONE
        } else if every {
            survivors_n = rows as u32;
            ALL
        } else {
            // D4321 step 1, item 3 (LADDER-DESIGN 6): the entry's words and row list are cache bytes, on the cache arena.
            // CACHE-CHARGE: charged to the condition cache as they are taken (`cache_scope_for`); the scope's charge joins
            // the entry's at the publish
            let cache = memledger::arena::cache_scope_for(OWNER, memledger::Kind::Region);
            let mut own = vec![0u64; self.wpg].into_boxed_slice(); // alloc: Cold charged OWNER
            own[..n].iter_mut().enumerate().for_each(|(i, o)| *o = last(i, words[i]));
            let count = own[..n].iter().map(|w| w.count_ones() as usize).sum::<usize>();
            survivors_n = count as u32;
            let list: Box<[u16]> = if self.mode == ConditionCacheMode::Full && count <= list_max { survivors(&own[..n], count) } else { Box::default() };
            let taken = cache.finish();
            let add = (self.wpg * 8 + list.len().next_multiple_of(4) * 2) as u64;
            match self.words[g].set(Mixed { words: own, rows: list }) {
                Ok(()) => {
                    filled = add;
                    let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
                    // the scope's bytes are the entry's now; what it did not take (no cache arena installed: the blocks
                    // are the general road's) is committed from this thread, so the bytes count once
                    let got = taken.map_or(0, |c| held.absorb(c));
                    held.commit(add.saturating_sub(got));
                    drop(held);
                    let prev = self.charge.fetch_add(add, Ordering::AcqRel);
                    if prev & LIVE != 0 {
                        HELD.fetch_add(add as i64, Ordering::Relaxed);
                        SEG_HELD[seg_of(prev)].fetch_add(add as i64, Ordering::Relaxed);
                    }
                }
                // a racing publisher's words won: the scope's charge ends first (its bytes back on this thread), then
                // this one's words free
                Err(lost) => {
                    drop(taken);
                    drop(lost);
                }
            }
            MIXED
        };
        // identical plans over one immutable part compute identical words, so a racing publisher stores the same count
        if let Some(counts) = &self.counts {
            counts.granules[g].store(survivors_n, Ordering::Relaxed);
        }
        if s.compare_exchange(UNKNOWN, v, Ordering::Release, Ordering::Relaxed).is_ok() {
            if let Some(counts) = &self.counts {
                counts.total.fetch_add(survivors_n as u64, Ordering::Relaxed);
                counts.known.fetch_add(1, Ordering::Release);
            }
        }
        PUBLISHED.fetch_add(1, Ordering::Relaxed);
        // the granule's miss brought back: its words' bytes (none / all are its state alone), its price when it was timed
        memledger::value::fill(memledger::Consumer::CondCache, value_key(self, g), filled);
    }

    fn owned_by(&self, owner: &Arc<dyn Any + Send + Sync>) -> bool {
        core::ptr::addr_eq(self.owner.as_ptr(), Arc::as_ptr(owner))
    }
}

type Key = ([u64; 2], [u64; 2], u8);

struct Store {
    map: HashMap<Key, Arc<Entry>>,
    /// The resident entries by segment, then last use (`Entry::used`, `order_key`), oldest first: one row per `map` entry.
    /// Under lru every entry is in the window, so this is the LRU order.
    lru: BTreeMap<u64, Key>,
    tick: u64,
    /// cachepolicy (C5, W-TinyLFU): the frequency sketch of every lookup, made at the first lookup under `cache_policy` = 2.
    sketch: Option<crate::tinylfu::Sketch>,
    /// The resident count at which the next sweep of dropped owners' entries runs (doubling: amortized O(1) per insert).
    sweep_at: usize,
    /// D4607: `pgrc_read::openpart::closed_parts()` read before the last complete sweep: a count past it is a part closed
    /// since, whose entries may be dead.
    closed_seen: u64,
    /// D4607: when the last complete sweep ended (`pg_clock::mono_ns`): a sweep for closed parts waits
    /// `COND_CACHE_CLOSED_SWEEP_GAP_NS` after it.
    swept_ns: u64,
}

/// An entry's key in the store's order: its segment above its tick (ticks stay far under 2^60), so each segment is one
/// range of the order, oldest first. Under lru (the window only) it is the tick.
#[inline]
const fn order_key(seg: u8, tick: u64) -> u64 {
    ((seg as u64) << 60) | tick
}

/// The store's segments' ranges in eviction order: probation's, the window's, protected's. Under lru only the window has
/// entries, so the order is the LRU order.
const EVICT_ORDER: [u8; 3] = [tinylfu::PROBATION, tinylfu::WINDOW, tinylfu::PROTECTED];

/// A segment's place in `EVICT_ORDER`.
#[inline]
const fn evict_rank(key: u64) -> u8 {
    match (key >> 60) as u8 {
        tinylfu::PROBATION => 0,
        tinylfu::WINDOW => 1,
        _ => 2,
    }
}

/// Order `key`'s entry `e` newest in segment `seg` (a fresh tick, its `used`): the store's one order insert.
fn order(lru: &mut BTreeMap<u64, Key>, tick: &mut u64, key: Key, e: &Entry, seg: u8) {
    *tick += 1;
    let k = order_key(seg, *tick);
    e.used.store(k, Ordering::Relaxed);
    lru.insert(k, key);
}

/// cachepolicy (C5): move resident `e` into segment `to`, its bytes from its segment's total to `to`'s in the one word.
fn move_seg(e: &Entry, to: u8) {
    let bits = (to as u64) << SEG_SHIFT;
    let prev = e.charge.fetch_update(Ordering::AcqRel, Ordering::Acquire, |c| Some((c & !SEG) | bits)).unwrap_or_else(|c| c);
    if prev & LIVE != 0 {
        let b = (prev & !META) as i64;
        SEG_HELD[seg_of(prev)].fetch_sub(b, Ordering::Relaxed);
        SEG_HELD[to as usize].fetch_add(b, Ordering::Relaxed);
    }
}

impl Store {
    /// Register `e` under `key`, newest in the window: resident, its bytes in `HELD`.
    fn link(&mut self, key: Key, e: &Arc<Entry>) {
        order(&mut self.lru, &mut self.tick, key, e, tinylfu::WINDOW);
        // a new entry's segment bits are the window's (0)
        let prev = e.charge.fetch_or(LIVE, Ordering::AcqRel);
        if prev & LIVE == 0 {
            HELD.fetch_add((prev & !META) as i64, Ordering::Relaxed);
            SEG_HELD[seg_of(prev)].fetch_add((prev & !META) as i64, Ordering::Relaxed);
        }
        self.map.insert(key, Arc::clone(e)); // alloc: Cold
    }

    /// Take `key`'s entry out of the store: its order row, its bytes from `HELD` and its segment's total. A source still
    /// holding it keeps publishing into it, uncounted.
    fn unlink(&mut self, key: &Key) -> Option<Arc<Entry>> {
        let e = self.map.remove(key)?;
        self.lru.remove(&e.used.load(Ordering::Relaxed));
        let prev = e.charge.fetch_and(!META, Ordering::AcqRel);
        if prev & LIVE != 0 {
            HELD.fetch_sub((prev & !META) as i64, Ordering::Relaxed);
            SEG_HELD[seg_of(prev)].fetch_sub((prev & !META) as i64, Ordering::Relaxed);
        }
        Some(e)
    }

    /// The next entry out at the budget: the first of the order by `EVICT_ORDER` (under lru the least recently used).
    fn victim(&self) -> Option<Key> {
        EVICT_ORDER.iter().find_map(|&g| self.lru.range(order_key(g, 0)..order_key(g + 1, 0)).next().map(|(_, k)| *k))
    }

    /// Unlink `key`'s entry at the budget (an insert's or a trim's eviction): the byte clock, the ghost, the counts.
    fn evict(&mut self, key: &Key, at_cap: bool) {
        let e = self.unlink(key);
        evicted(key, e.as_deref());
        EVICTED.fetch_add(1, Ordering::Relaxed);
        if at_cap {
            CAP_EVICTED.fetch_add(1, Ordering::Relaxed);
            CAP_EVICTED_BYTES.fetch_add(e.as_deref().map_or(0, Entry::bytes), Ordering::Relaxed);
        }
    }

    /// cachepolicy (C5, W-TinyLFU): a hit on resident `e`: newest in the window or in protected; a probation entry moves to
    /// protected, and protected past its share (`tinylfu_protected`) hands its least recent back to probation (newest).
    fn touch(&mut self, key: Key, e: &Entry, cap: u64) {
        let seg = seg_of(e.charge.load(Ordering::Relaxed)) as u8;
        if seg != tinylfu::PROBATION {
            order(&mut self.lru, &mut self.tick, key, e, seg);
            return;
        }
        move_seg(e, tinylfu::PROTECTED);
        order(&mut self.lru, &mut self.tick, key, e, tinylfu::PROTECTED);
        TLFU[PROMOTED].fetch_add(1, Ordering::Relaxed);
        let share = mempolicy::cache::tinylfu_protected(cap);
        // bounded("other: protected's least recent entries back to probation until protected holds its share, one move each, at most the protected entries; under the store lock, a stop would leave protected over its share")
        while seg_held(tinylfu::PROTECTED) > share {
            let first = self.lru.range(order_key(tinylfu::PROTECTED, 0)..).next().map(|(&k, &v)| (k, v));
            let Some((k, old)) = first.filter(|&(_, old)| old != key) else { break };
            let Some(oe) = self.map.get(&old) else { break };
            self.lru.remove(&k);
            move_seg(oe, tinylfu::PROBATION);
            order(&mut self.lru, &mut self.tick, old, oe, tinylfu::PROBATION);
            TLFU[DEMOTED].fetch_add(1, Ordering::Relaxed);
        }
    }

    /// cachepolicy (C5, W-TinyLFU), after an insert into the window: the window past its share (`tinylfu_window`) hands its
    /// least recent entries to main, into probation while the cache holds its budget, else only when the sketch counts the
    /// entry more often than the main entries it would push out, summed (probation's least recent first, then
    /// protected's; `tinylfu_admit`), which then go; a refused one goes itself. Then the cache back under its budget, by
    /// `EVICT_ORDER`.
    fn admit(&mut self, cap: u64) {
        let window = mempolicy::cache::tinylfu_window(cap);
        let main = order_key(tinylfu::PROBATION, 0);
        // bounded("other: the window's least recent entries into main or out until the window holds its share, one move or eviction each, at most the window's entries; under the store lock, a stop would leave the window over its share")
        while seg_held(tinylfu::WINDOW) > window {
            let Some((&wk, &cand)) = self.lru.range(..main).next() else { break };
            let Some(ce) = self.map.get(&cand) else { break };
            let over = held().saturating_sub(cap);
            if over == 0 {
                self.lru.remove(&wk);
                move_seg(ce, tinylfu::PROBATION);
                order(&mut self.lru, &mut self.tick, cand, ce, tinylfu::PROBATION);
                continue;
            }
            // the main entries it would push out, oldest first, until their bytes cover the excess: their counts summed
            let est = |k: &Key| self.sketch.as_ref().map_or(0, |s| s.estimate(ghost_key(k)));
            let (mut bytes, mut n, mut count) = (0u64, 0usize, 0u32);
            // bounded("other: the main space's least recent entries until their bytes cover the cache's excess, at most the main entries; under the store lock")
            for (_, k) in self.lru.range(main..) {
                if bytes >= over {
                    break;
                }
                bytes += self.map.get(k).map_or(0, |v| v.bytes());
                count = count.saturating_add(est(k));
                n += 1;
            }
            if bytes >= over && mempolicy::cache::tinylfu_admit(est(&cand), count) {
                // bounded("other: the n victims counted just above, one eviction each")
                for _ in 0..n {
                    let Some((_, &v)) = self.lru.range(main..).next() else { break };
                    self.evict(&v, true);
                }
                if let Some(ce) = self.map.get(&cand) {
                    self.lru.remove(&wk);
                    move_seg(ce, tinylfu::PROBATION);
                    order(&mut self.lru, &mut self.tick, cand, ce, tinylfu::PROBATION);
                }
                TLFU[ADMITTED].fetch_add(1, Ordering::Relaxed);
            } else {
                self.evict(&cand, true);
                TLFU[REJECTED].fetch_add(1, Ordering::Relaxed);
            }
        }
        // bounded("other: the cache's budget holds: entries out by EVICT_ORDER until it does, one unlink each, at most the cache's entries; a stop would leave the cache over its budget")
        while held() > cap {
            let Some(k) = self.victim() else { break };
            self.evict(&k, true);
        }
    }

    /// Drop the entries of closed parts, when the store has doubled since the last sweep or (D4607) when a part closed
    /// since the last sweep and it ended at least `CLOSED_SWEEP_GAP_NS` ago: before, a dropped database's or table's
    /// entries stayed charged until the store doubled (up to twice the live entries). A tick per `BATCH` entries looked at
    /// and per entry unlinked: a cancel returns mid-sweep and leaves the threshold and the closed count, so the next insert
    /// sweeps again (an unswept closed part's entry is only memory until then).
    fn sweep(&mut self) {
        // read before the walk: a part that closes during it moves the count past `closed_seen` (the next insert sweeps)
        let closed = pgrc_read::openpart::closed_parts();
        if self.map.len() < self.sweep_at {
            if closed == self.closed_seen {
                return;
            }
            if self.map.is_empty() {
                self.closed_seen = closed;
                return;
            }
            if pg_clock::mono_ns().saturating_sub(self.swept_ns) < CLOSED_SWEEP_GAP_NS {
                return;
            }
        }
        let mut step = cancelpoll::step("cond_cache sweep: closed parts' entries");
        // the closed parts' keys, as many as the map holds dead: on the cache's account while the sweep lives
        let mut dead = memledger::ChargedVec::<Key>::new(Some(OWNER), memledger::Kind::Region);
        for (i, (k, v)) in self.map.iter().enumerate() {
            if step.batch(i).is_err() {
                return;
            }
            if v.owner.strong_count() == 0 {
                dead.with_vec(|d| d.push(*k)); // alloc: Exec charged OWNER
            }
        }
        for k in dead.iter() {
            if step.tick().is_err() {
                return;
            }
            self.unlink(k);
        }
        SWEEPS.fetch_add(1, Ordering::Relaxed);
        self.sweep_at = (self.map.len() * 2).max(SWEEP_FLOOR);
        self.closed_seen = closed;
        self.swept_ns = pg_clock::mono_ns();
    }
}

/// The least resident count that triggers a sweep of dropped owners' entries.
const SWEEP_FLOOR: usize = gravity_program::cost::pgrc::COND_CACHE_SWEEP_FLOOR;
/// D4607: the least time between a sweep and the next one for closed parts (ns).
const CLOSED_SWEEP_GAP_NS: u64 = gravity_program::cost::pgrc::COND_CACHE_CLOSED_SWEEP_GAP_NS;

static STORE: OnceLock<Mutex<Store>> = OnceLock::new();
/// The byte budget: every execution that takes the cache sets it first (exec's `src.verdicts`, from
/// `pgrust.condition_cache_size`); the initial value serves a host test that sets none.
static CAPACITY: AtomicU64 = AtomicU64::new(100 << 20);
/// The resident entries' bytes (`Entry::charge` of the `LIVE` ones); an i64, as a publish racing an eviction may subtract
/// before it adds.
static HELD: AtomicI64 = AtomicI64::new(0);
static SWEEPS: AtomicU64 = AtomicU64::new(0);
static PUBLISHED: AtomicU64 = AtomicU64::new(0);
static EVICTED: AtomicU64 = AtomicU64::new(0);
/// Ladder C1c: the entries an insert evicted to stay under the capacity, since start; C1d: their bytes (the ladder's demand
/// probe: a raise gives back what the cache evicted under its budget since the last one).
static CAP_EVICTED: AtomicU64 = AtomicU64::new(0);
static CAP_EVICTED_BYTES: AtomicU64 = AtomicU64::new(0);
/// cachepolicy (C5): the resident entries' bytes by segment (`SEG`; all in the window under lru), as `HELD`.
static SEG_HELD: [AtomicI64; 3] = [AtomicI64::new(0), AtomicI64::new(0), AtomicI64::new(0)];
/// cachepolicy (C5, W-TinyLFU), since start: window entries admitted into main past the sketch, refused by it (evicted),
/// probation entries promoted to protected on a hit, protected entries demoted back to probation.
static TLFU: [AtomicU64; 4] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
const ADMITTED: usize = 0;
const REJECTED: usize = 1;
const PROMOTED: usize = 2;
const DEMOTED: usize = 3;

fn seg_held(seg: u8) -> u64 {
    SEG_HELD[seg as usize].load(Ordering::Relaxed).max(0) as u64
}

/// cachepolicy (C5): `[admitted, rejected, promoted, demoted, window bytes, probation bytes, protected bytes]` (the
/// accounts view's `cachepolicy` rows).
pub fn tinylfu_census() -> [u64; 7] {
    let c = |i: usize| TLFU[i].load(Ordering::Relaxed);
    [c(ADMITTED), c(REJECTED), c(PROMOTED), c(DEMOTED), seg_held(tinylfu::WINDOW), seg_held(tinylfu::PROBATION), seg_held(tinylfu::PROTECTED)]
}

pub fn cap_evictions() -> u64 {
    CAP_EVICTED.load(Ordering::Relaxed)
}

pub fn cap_evicted_bytes() -> u64 {
    CAP_EVICTED_BYTES.load(Ordering::Relaxed)
}

/// The byte budget (`pgrust.condition_cache_size` in bytes); the next insert evicts down to it.
pub fn set_capacity(bytes: u64) {
    CAPACITY.store(bytes, Ordering::Relaxed);
    CAPACITY_SET.store(true, Ordering::Relaxed);
}

/// Memory step 5: an execution has set the budget (`set_capacity`); before that the initial value serves only a rig.
static CAPACITY_SET: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Memory step 5: the budget as the memory budget reads it (any thread, no GUC store): the one an
/// execution set, else the machine's derived share (`cost::condition_cache_cap`, `pgrust.condition_cache_size = -1`).
pub fn capacity_seen() -> u64 {
    if CAPACITY_SET.load(Ordering::Relaxed) {
        CAPACITY.load(Ordering::Relaxed)
    } else {
        crate::dict::text_hosts::reuse::store_bytes(gravity_program::cost::condition_cache_cap) as u64
    }
}

/// The bytes the resident entries hold.
pub fn held_bytes() -> u64 {
    held()
}

/// Memory step 5: the condition cache's reclaim for the memory budget (5b's reclaimer): the least recently used entries
/// unlinked until the cache holds at most `bytes`. The bytes unlinked.
pub fn trim_to(bytes: u64) -> u64 {
    let before = held();
    if before <= bytes {
        return 0;
    }
    // D4398: on the memory ladder's pass, nothing unlinked while another thread holds the store
    let Some(mut guard) = memledger::ladder_pass::lock_or_skip(store()) else { return 0 };
    let s = &mut *guard;
    // bounded("other: the memory budget's reclaim hook: least recently used entries unlinked until the cache holds the asked bytes, one unlink each, at most the cache's entries; the reclaimer owns it, and a stop would leave the budget's ask unmet")
    while held() > bytes {
        // by `EVICT_ORDER`: under lru the least recently used, under W-TinyLFU probation's first
        let Some(oldest) = s.victim() else { break };
        s.evict(&oldest, false);
    }
    drop(guard);
    before.saturating_sub(held())
}

/// Broker (b): the ghosts of evicted entries, hash-sampled one key in [`GHOST_SAMPLE`] (a ghost hit counts that many):
/// (key word, the cache's byte clock at the eviction).
static GHOSTS: memledger::value::Ghosts<4096> = memledger::value::Ghosts::new();
const GHOST_SAMPLE: u64 = 8; // a power of two (`sampled`)

/// Broker (b): the LRU tick at the cache's coldest tenth of bytes (`publish_tail`, each broker plan); a hit on an entry
/// last used at or below it is a tail hit (`in_tail`). 0: none published. Under W-TinyLFU an order key (`order_key`) in
/// eviction order.
static TAIL_TICK: AtomicU64 = AtomicU64::new(0);

/// Whether order key `used` is at or below the tail mark `tail` in eviction order (under lru: `used <= tail`).
#[inline]
const fn in_tail(used: u64, tail: u64) -> bool {
    let (a, b) = (evict_rank(used), evict_rank(tail));
    a < b || (a == b && used <= tail)
}

/// A key's word for the ghosts.
#[inline]
fn ghost_key(key: &Key) -> u64 {
    key.0[0] ^ key.0[1].rotate_left(17) ^ key.1[0].rotate_left(31) ^ key.1[1].rotate_left(47) ^ key.2 as u64
}

/// Whether ghost key `k` is in the sample (one key in [`GHOST_SAMPLE`], `memledger::value::sampled`).
#[inline]
const fn sampled(k: u64) -> bool {
    memledger::value::sampled(k, GHOST_SAMPLE)
}

/// Broker (b): `key`'s entry `e` evicted (least recently used): the byte clock moves; a sampled key keeps a ghost.
#[inline]
fn evicted(key: &Key, e: Option<&Entry>) {
    if !memledger::value::ext() {
        return;
    }
    let at = memledger::value::evicted(memledger::Consumer::CondCache, e.map_or(0, Entry::bytes));
    let k = ghost_key(key);
    if sampled(k) {
        GHOSTS.record(k, at);
    }
}

/// Broker (b): publish the cache's tail, the LRU tick at its coldest tenth of bytes, and the bytes it covers. For the
/// broker's plan on the memory watchdog's thread: no allocation and no wait (the LRU walked in order under `try_lock`;
/// busy, or the store never made, the last mark stays).
pub fn publish_tail() {
    let Some(g) = STORE.get().and_then(memledger::value::try_lock) else { return };
    let tenth = held() / 10;
    let (mut tick, mut bytes) = (0u64, 0u64);
    let order = EVICT_ORDER.iter().flat_map(|&s| g.lru.range(order_key(s, 0)..order_key(s + 1, 0)));
    // bounded("other: the memory watchdog thread's walk of the LRU's coldest tenth of bytes under try_lock, outside any statement: no statement's cancel applies")
    for (&t, key) in order {
        if bytes >= tenth {
            break;
        }
        tick = t;
        bytes += g.map.get(key).map_or(0, |e| e.bytes());
    }
    drop(g);
    TAIL_TICK.store(tick, Ordering::Relaxed);
    memledger::value::set_tail_bytes(memledger::Consumer::CondCache, bytes);
}

/// `(hits, misses, granules published, entries evicted, bytes held)`.
pub fn census() -> [u64; 5] {
    let v = memledger::value::stats(memledger::Consumer::CondCache);
    [v.hits, v.misses, PUBLISHED.load(Ordering::Relaxed), EVICTED.load(Ordering::Relaxed), held()]
}

fn held() -> u64 {
    HELD.load(Ordering::Relaxed).max(0) as u64
}

/// A source's lookups since its last flush, for the census and the cache's value (memledger::value; granule_source's
/// `Tally`: one add per claim, not per lookup).
pub fn count_n(hits: u64, misses: u64) {
    memledger::value::tally(memledger::Consumer::CondCache, hits, misses);
    reuse::note(reuse::Kind::Verdicts, hits);
}

/// The word a granule's miss is timed by (memledger::value): its entry and granule. `verdicts.rs` arms it where the
/// granule misses; `Entry::put` fills it.
#[inline]
pub fn value_key(e: &Entry, g: usize) -> u64 {
    (e as *const Entry as usize as u64) ^ (g as u64).rotate_left(48)
}

/// Drop every entry (a rig's restart stand-in): the count dropped.
pub fn drop_all() -> usize {
    let mut s = pgsync::lock(store());
    let mut keys = memledger::ChargedVec::<Key>::new(Some(OWNER), memledger::Kind::Region);
    keys.with_vec(|v| v.extend(s.map.keys().copied())); // alloc: Exec charged OWNER
    // bounded("other: a rig's restart stand-in that drops every entry, outside any statement: no statement's cancel applies")
    for k in keys.iter() {
        s.unlink(k);
    }
    s.sweep_at = SWEEP_FLOOR;
    // cachepolicy (C5): a restart starts with no lookups counted
    s.sketch = None;
    // D4607: nothing is left to sweep: the parts closed so far are seen, and the next sweep for closed parts waits its gap
    s.closed_seen = pgrc_read::openpart::closed_parts();
    s.swept_ns = pg_clock::mono_ns();
    keys.len()
}

/// DEAD_BYTES: the bytes of the resident entries nothing can hit again (their part closed: a dropped database, table or
/// part, a rewrite's old locator), which the next sweep takes (`Store::sweep`); a live entry the LRU has not reached yet
/// is not dead. Read by the accounts view beside the cache's account (`'condition_cache dead_bytes'`): a walk of the
/// store under its lock, information only, never on a scan's path.
pub fn dead_bytes() -> u64 {
    let Some(m) = STORE.get() else { return 0 };
    let s = pgsync::lock(m);
    // one pass over the resident entries for a memory view's row (an iterator chain, no loop of ours: no bounded marker),
    // an atomic load each, no allocation
    s.map.values().filter(|e| e.owner.strong_count() == 0).map(|e| e.bytes()).sum()
}

fn store() -> &'static Mutex<Store> {
    STORE.get_or_init(|| Mutex::new(Store { map: HashMap::new(), lru: BTreeMap::new(), tick: 0, sketch: None, sweep_at: SWEEP_FLOOR, closed_seen: 0, swept_ns: 0 })) // alloc: Cold
}

/// cachepolicy (C5): a sketch for the store, its words on the condition cache's charge.
fn new_sketch() -> tinylfu::Sketch {
    let charge = memledger::Charge::open(OWNER, memledger::Release::Evict, memledger::Kind::Region, memledger::charge::Door::Heap, tinylfu::Sketch::bytes());
    tinylfu::Sketch::new(vec![0u64; tinylfu::SKETCH_WORDS].into_boxed_slice(), charge) // alloc: Cold charged OWNER
}

/// The entry of `(fp, unit)` for `owner`'s `granules` granules of `rows` rows at most: the resident one when `owner` is
/// the one it was filled under, else a new one registered in its place (evicting least-recently-used entries to the
/// budget, or under W-TinyLFU through its window and admission: `Store::admit`); a new entry over the whole budget, or
/// one W-TinyLFU's admission refuses, is handed back unregistered (a scan-local memo).
pub fn entry_for(fp: [u64; 2], unit: [u64; 2], owner: &Arc<dyn Any + Send + Sync>, granules: usize, rows: usize) -> Arc<Entry> {
    entry_for_mode(fp, unit, owner, granules, rows, ConditionCacheMode::Full)
}

pub fn entry_for_mode(fp: [u64; 2], unit: [u64; 2], owner: &Arc<dyn Any + Send + Sync>, granules: usize, rows: usize, mode: ConditionCacheMode) -> Arc<Entry> {
    let wpg = rows.div_ceil(64).max(1);
    if mode == ConditionCacheMode::Off {
        return Arc::new(Entry::new(owner, granules, wpg, mode)); // alloc: Cold
    }
    // ladder C1b: under `cache_floor` the ladder sizes the condition cache (C1d: from the capacity, raised past it on
    // demand)
    let cap = mcx::ladder::sized_budget(mcx::ladder::SIZED_COND, CAPACITY.load(Ordering::Relaxed));
    let tlfu = tinylfu::on();
    let mut guard = pgsync::lock(store());
    let s = &mut *guard;
    let key = (fp, unit, mode as u8);
    if tlfu {
        // every lookup counts, a hit or a miss
        s.sketch.get_or_insert_with(new_sketch).add(ghost_key(&key));
    }
    if let Some(e) = s.map.get(&key) {
        if e.owned_by(owner) && e.state.len() == granules && e.wpg == wpg {
            let e = Arc::clone(e);
            if memledger::value::ext() {
                let tail = TAIL_TICK.load(Ordering::Relaxed);
                if tail != 0 && in_tail(e.used.load(Ordering::Relaxed), tail) {
                    memledger::value::tail_hit(memledger::Consumer::CondCache);
                }
            }
            s.lru.remove(&e.used.load(Ordering::Relaxed));
            if tlfu {
                s.touch(key, &e, cap);
            } else {
                let seg = seg_of(e.charge.load(Ordering::Relaxed)) as u8;
                order(&mut s.lru, &mut s.tick, key, &e, seg);
            }
            return e;
        }
    }
    let e = Arc::new(Entry::new(owner, granules, wpg, mode)); // alloc: Cold
    if memledger::value::ext() {
        let k = ghost_key(&key);
        if sampled(k) {
            if let Some(at) = GHOSTS.take(k) {
                memledger::value::ghost(memledger::Consumer::CondCache, at, GHOST_SAMPLE);
            }
        }
    }
    s.unlink(&key);
    s.sweep();
    let need = e.bytes();
    if need > cap {
        return e;
    }
    if tlfu {
        s.link(key, &e);
        s.admit(cap);
        return e;
    }
    // bounded("other: the cache's capacity holds: least recently used entries unlinked until the new entry fits, one unlink each, at most the cache's entries; a stop would leave the cache over its budget")
    while held() + need > cap {
        let Some((_, oldest)) = s.lru.pop_first() else { break };
        s.evict(&oldest, true);
    }
    s.link(key, &e);
    e
}

/// The fingerprint's domain; `.2` = under the one key (hash-contract.md §3.6), with the shape among the hashed bytes.
const DOMAIN: &[u8] = b"gravity.v4.cond-words.2";

/// This execution's condition fingerprint (V4R-4, M1-SOURCE's shape): the Fp128 producer's (`gravity_dict::fp`, the one
/// key, hash-contract.md §3.6) over [`DOMAIN`], the qual shape's fingerprint and the hashed vector of the covered quals'
/// bound words `covered` (`(word, varlena)` in need order: any number, any order), each as its null byte then its 8
/// bytes, or, for a varlena word, its data length and bytes. `None` (the cache stays off for the execution) before the
/// seed is installed, for a word outside the block, a compressed or external varlena, or a zero result.
pub fn fingerprint(shape: [u64; 2], words: &[u64], nulls: &[u8], covered: &[(usize, bool)]) -> Option<[u64; 2]> {
    let keys = gravity_dict::fp::keys()?;
    let mut bytes: Vec<u8> = Vec::with_capacity(DOMAIN.len() + 16 + covered.len() * 9); // alloc: Exec
    bytes.extend_from_slice(DOMAIN);
    bytes.extend_from_slice(&shape[0].to_le_bytes());
    bytes.extend_from_slice(&shape[1].to_le_bytes());
    // a tick per covered word (a varlena's bytes are copied); a cancel answers `None`: the cache stays off for the execution
    let mut step = cancelpoll::step("cond_cache fingerprint: the covered quals' bound words");
    for &(at, varlena) in covered {
        step.tick().ok()?;
        let (&w, &null) = (words.get(at)?, nulls.get(at)?);
        bytes.push(null);
        if null != 0 {
            continue;
        }
        if !varlena {
            bytes.extend_from_slice(&w.to_le_bytes());
            continue;
        }
        super::raw::push_bind_varlena(&mut bytes, w)?;
    }
    let fp = keys.fp_bytes(&bytes);
    (fp != [0, 0]).then_some(fp)
}

/// Cross-statement reuse, with predicate payloads restricted by `pgrust.condition_cache`.
/// The mode is captured on the leader at execution start and cache keys include it, so sessions cannot read a
/// stronger mode's payload. `reuse::census` counts reuse by kind.
///
/// | path | payload | control |
/// |---|---|---|
/// | condition cache | no-match granules; row bitmaps; counts and sparse row lists | `granule`; `rows`; `full`, respectively; `off` reads and publishes nothing |
/// | zero-read count answers | cached granule counts and part totals | `full` only |
/// | dictionary condition tables | a predicate verdict per dictionary code | shared only in `rows` and `full`; statement-local otherwise |
/// | run-time sizing feedback | observed group counts, row counts and key ranges | `pgrust.gravity_grant_feedback` |
/// | startup prewarm | catalog, relcache and process-image pages | `pgrust.gravity_prewarm` |
///
/// Condition entries still require an immutable part owner, the memory budget, and, for unlogged relations,
/// `pgrust.condition_cache_unlogged`. Temporary relations never enter. A new part owner cannot reuse an old entry.
/// Data, metadata and plan caches remain independent: zone maps and their derived zone store, part and dictionary
/// caches, row maps, cursors, build pools, plans, and compiled code keep their existing controls.
pub mod reuse {
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A counted kind of cross-statement reuse (a row of the registry above).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Kind {
        Verdicts,
        CodeTables,
        Feedback,
        /// Never counted: the Fp128 sidecar files are gone (the seed is drawn per process); the word keeps the
        /// `gravity: reuse` line's layout.
        Sidecars,
    }

    pub const KINDS: usize = 4;

    static USED: [AtomicU64; KINDS] = [const { AtomicU64::new(0) }; KINDS];

    /// `n` reuses of `kind`: an answer, table or file taken from an earlier statement's work.
    #[inline]
    pub fn note(kind: Kind, n: u64) {
        if n != 0 {
            USED[kind as usize].fetch_add(n, Ordering::Relaxed);
        }
    }

    /// The reuses so far in this process, by `Kind` in declaration order.
    pub fn census() -> [u64; KINDS] {
        core::array::from_fn(|i| USED[i].load(Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner() -> Arc<dyn Any + Send + Sync> {
        Arc::new(0u64)
    }

    // the budget and the counters are process-wide: the tests that touch them run one at a time
    fn serial() -> pgsync::MutexGuard<'static, ()> {
        static L: OnceLock<Mutex<()>> = OnceLock::new();
        pgsync::lock(L.get_or_init(|| Mutex::new(())))
    }

    #[test]
    fn modes_keep_only_their_allowed_payloads_and_never_share_stronger_entries() {
        let _s = serial();
        set_capacity(100 << 20);
        let o = owner();
        let entry = |mode| entry_for_mode([93, 1], [93, 2], &o, 3, 100, mode);
        let full = entry(ConditionCacheMode::Full);
        full.put(0, &[0, 0], 100, 8);
        full.put(1, &[!0, !0], 100, 8);
        full.put(2, &[5, 0], 100, 8);
        assert_eq!(full.part_total(), Some(102));
        assert_eq!(full.rows(2), Some(&[0, 2][..]));
        for mode in [ConditionCacheMode::Granule, ConditionCacheMode::Rows, ConditionCacheMode::Off] {
            let e = entry(mode);
            assert!(!Arc::ptr_eq(&e, &full));
            assert_eq!(e.get(0), Hit::Unknown);
            e.put(0, &[0, 0], 100, 8);
            e.put(1, &[!0, !0], 100, 8);
            e.put(2, &[5, 0], 100, 8);
            assert!(e.counts.is_none(), "{mode:?}: no count storage");
            assert_eq!(e.part_total(), None);
            for g in 0..3 {
                assert_eq!(e.count(g), None);
                assert_eq!(e.rows(g), None, "{mode:?}: no sparse survivor list");
            }
            match mode {
                ConditionCacheMode::Off => assert!((0..3).all(|g| e.get(g) == Hit::Unknown)),
                ConditionCacheMode::Granule => {
                    assert!(e.words.is_empty(), "granule mode allocates no bitmap cells");
                    assert_eq!((e.get(0), e.get(1), e.get(2)), (Hit::None, Hit::Unknown, Hit::Unknown));
                    assert!(Arc::ptr_eq(&e, &entry(mode)));
                }
                ConditionCacheMode::Rows => {
                    assert_eq!((e.get(0), e.get(1), e.get(2)), (Hit::None, Hit::All, Hit::Mixed(&[5, 0])));
                    assert!(Arc::ptr_eq(&e, &entry(mode)));
                }
                ConditionCacheMode::Full => unreachable!(),
            }
        }
        assert!(Arc::ptr_eq(&full, &entry(ConditionCacheMode::Full)));
        assert_eq!(full.part_total(), Some(102));
    }

    #[test]
    fn a_published_granule_reads_back_as_none_all_or_its_words() {
        let _s = serial();
        set_capacity(100 << 20);
        let o = owner();
        let e = entry_for([1, 2], [3, 4], &o, 4, 100);
        assert_eq!(e.get(0), Hit::Unknown);
        e.put(0, &[0, 0], 100, 0);
        e.put(1, &[!0, (1 << 36) - 1], 100, 0);
        e.put(2, &[5, 0], 100, 0);
        assert_eq!(e.get(0), Hit::None);
        assert_eq!(e.get(1), Hit::All);
        assert_eq!(e.get(2), Hit::Mixed(&[5, 0]));
        assert_eq!(e.get(3), Hit::Unknown);
        // the first publish wins; bits past the rows are ignored
        e.put(2, &[7, 0], 100, 0);
        e.put(3, &[0, !0 << 36], 100, 0);
        assert_eq!(e.get(2), Hit::Mixed(&[5, 0]));
        assert_eq!(e.get(3), Hit::None);
    }

    /// The hit selection memo: a mixed granule published with `list_max` keeps its survivors as
    /// the rows its words set, exactly `words_to_sel`'s selection of those words (bits past the rows ignored), for every
    /// word shape and density; none past `list_max`, none for a none / all granule, none with `list_max` 0; the first
    /// publish's list stays with its words.
    #[test]
    fn a_sparse_mixed_granule_keeps_the_rows_its_words_set() {
        let _s = serial();
        set_capacity(100 << 20);
        let o = owner();
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let (mut sel, mut sel16) = (Vec::with_capacity(8192), Vec::with_capacity(8192)); // alloc: Cold
        for case in 0..400u64 {
            let rows = [8192usize, 8191, 1500, 777, 64, 65, 1][(case % 7) as usize];
            let n = rows.div_ceil(64);
            let words: Vec<u64> = (0..n) // alloc: Cold
                .map(|w| match (case / 7 + w as u64) % 6 {
                    0 => 0,
                    1 => 1u64 << (next() % 64),
                    2 => next() & next() & next() & next(),
                    3 => next() & next(),
                    4 => !0,
                    _ => next(),
                })
                .collect();
            let list_max = [rows / 8, rows, 0, 3][(case % 4) as usize];
            let e = entry_for([7, case], [8, 9], &o, 2, 8192);
            e.put(0, &words, rows, list_max);
            let mut masked = words.clone();
            if rows % 64 != 0 {
                masked[n - 1] &= (1u64 << (rows % 64)) - 1;
            }
            let count = crate::ops::scan::pgrc::granule_source::words_to_sel(&masked, &mut sel, &mut sel16);
            match e.get(0) {
                Hit::Mixed(w) => {
                    assert_eq!(&w[..n], &masked[..], "case {case}: the words");
                    match e.rows(0) {
                        Some(r) => {
                            assert!(count <= list_max, "case {case}: a list past list_max");
                            assert_eq!(r, &sel16[..], "case {case}: the list is words_to_sel's selection of the words");
                        }
                        None => assert!(count > list_max, "case {case}: {count} survivors <= {list_max} kept no list"),
                    }
                }
                Hit::None | Hit::All => assert_eq!(e.rows(0), None, "case {case}: no list for a none / all granule"),
                Hit::Unknown => panic!("case {case}: published"),
            }
            assert_eq!(e.rows(1), None, "case {case}: an unpublished granule has no list");
            // the first publish wins, its list with it
            e.put(0, &[!0; 128][..n], rows, rows);
            if let Some(r) = e.rows(0) {
                assert_eq!(r, &sel16[..], "case {case}: a later publish never replaces the list");
            }
        }
        drop_all();
    }

    /// The list is charged to its entry with the words (2 bytes a row, rounded to 8), counted while the entry is resident
    /// (`LIVE`) and leaving the count with the entry: a list never outlives its words.
    #[test]
    fn a_kept_list_is_charged_to_its_entry_and_freed_with_it() {
        let _s = serial();
        set_capacity(100 << 20);
        let o = owner();
        let e = entry_for([11, 0], [0, 0], &o, 4, 8192);
        let b0 = e.bytes();
        assert!(e.charge.load(Ordering::Relaxed) & LIVE != 0, "resident");
        let mut words = [0u64; 128];
        words[3] = 0b1011; // 3 survivors
        e.put(0, &words, 8192, 1024);
        let wbytes = 128 * 8;
        assert_eq!(e.rows(0), Some(&[192u16, 193, 195][..]));
        assert_eq!(e.bytes(), b0 + wbytes + 8, "the words and the 3-row list (6 bytes, rounded to 8)");
        words[5] = !0; // 67 survivors, past list_max 64: the words alone
        e.put(1, &words, 8192, 64);
        assert_eq!(e.rows(1), None);
        assert_eq!(e.bytes(), b0 + 2 * wbytes + 8);
        // CACHE-CHARGE: the ledger holds the entry's bytes once: its scope's charge (none here: no cache arena installed in
        // a test binary, so the blocks are the general road's) and the rest committed at the publish
        assert_eq!(e.held.lock().unwrap().committed(), e.bytes(), "the entry's charge is its bytes, counted once");
        drop_all();
        assert!(e.charge.load(Ordering::Relaxed) & LIVE == 0, "unlinked: its words and list left HELD together");
        assert_eq!(e.bytes(), b0 + 2 * wbytes + 8);
        set_capacity(100 << 20);
    }

    /// D4527: a new entry's charge is what it allocates: its `Arc` block and, per granule, its state byte, its words cell (a
    /// `OnceLock` over two boxed slices) and its count; it was 21 bytes a granule + 64, about half of that.
    #[test]
    fn a_new_entrys_charge_is_its_arc_block_and_its_per_granule_lists() {
        use core::mem::size_of;
        let _s = serial();
        set_capacity(100 << 20);
        let o = owner();
        let granules = 1000;
        let e = entry_for([4527, 0], [0, 0], &o, granules, 64 * granules);
        let per = size_of::<AtomicU8>() + size_of::<OnceLock<Mixed>>() + size_of::<AtomicU32>();
        assert!(per >= 1 + 2 * size_of::<Box<[u64]>>() + 4, "a granule's cells: the state, the words' two slices, the count ({per})");
        assert_eq!(e.bytes(), (2 * size_of::<usize>() + size_of::<Entry>() + granules * per) as u64);
        assert!(e.bytes() > (granules * 21 + 64) as u64, "more than the old estimate");
        assert_eq!(e.held.lock().unwrap().committed(), e.bytes(), "the charge commits it, once");
        drop_all();
        set_capacity(100 << 20);
    }

    #[test]
    fn another_owner_under_the_same_key_never_sees_the_old_words() {
        let _s = serial();
        set_capacity(100 << 20);
        let (a, b) = (owner(), owner());
        let e = entry_for([9, 9], [7, 7], &a, 2, 64);
        e.put(0, &[1], 64, 0);
        assert_eq!(entry_for([9, 9], [7, 7], &a, 2, 64).get(0), Hit::Mixed(&[1]));
        // the same part identity reopened (inode reuse, a new generation): a fresh entry
        assert_eq!(entry_for([9, 9], [7, 7], &b, 2, 64).get(0), Hit::Unknown);
        assert_eq!(entry_for([9, 9], [7, 8], &b, 2, 64).get(0), Hit::Unknown);
        assert_eq!(entry_for([9, 8], [7, 7], &b, 2, 64).get(0), Hit::Unknown);
    }

    #[test]
    fn the_budget_evicts_the_least_recently_used_and_zero_keeps_nothing() {
        let _s = serial();
        drop_all();
        let o = owner();
        let one = entry_for([1, 0], [0, 0], &o, 8, 64).bytes();
        drop_all();
        set_capacity(one * 2);
        let a = entry_for([1, 0], [0, 0], &o, 8, 64);
        a.put(0, &[0], 64, 0);
        let _b = entry_for([2, 0], [0, 0], &o, 8, 64);
        let _c = entry_for([3, 0], [0, 0], &o, 8, 64);
        assert_eq!(entry_for([1, 0], [0, 0], &o, 8, 64).get(0), Hit::Unknown, "the oldest entry was evicted");
        set_capacity(0);
        let d = entry_for([4, 0], [0, 0], &o, 8, 64);
        d.put(0, &[0], 64, 0);
        assert_eq!(entry_for([4, 0], [0, 0], &o, 8, 64).get(0), Hit::Unknown, "a zero budget registers nothing");
        set_capacity(100 << 20);
    }

    /// Broker (b), `broker_value=1`: each LRU eviction moves the byte clock; a sampled evicted key asked again is a ghost
    /// hit of the sample's weight; a hit on the entry the published tail covers (the coldest tenth) is a tail hit, one on
    /// the newest is not. Never switched off again: the switch is process-wide and other doors' tests read it.
    #[test]
    fn broker_value_counts_evictions_ghosts_and_tail_hits() {
        use memledger::{value, Consumer::CondCache};
        let _s = serial();
        value::set_ext(true);
        drop_all();
        set_capacity(100 << 20);
        let o = owner();
        let one = entry_for([70, 0], [0, 0], &o, 8, 64).bytes();
        drop_all();
        set_capacity(one * 2);
        let e0 = value::ext_stats(CondCache);
        for i in 0..256u64 {
            let _ = entry_for([70, i], [0, 0], &o, 8, 64);
        }
        let e1 = value::ext_stats(CondCache);
        assert_eq!(e1.evicted_bytes - e0.evicted_bytes, 254 * one, "all but the two newest evicted");
        for i in 0..254u64 {
            let _ = entry_for([70, i], [0, 0], &o, 8, 64);
        }
        let g = value::ext_stats(CondCache).ghost_hits - e1.ghost_hits;
        assert!(g > 0 && g % GHOST_SAMPLE == 0, "the sampled ghosts taken, each counting the sample ({g})");

        drop_all();
        set_capacity(100 << 20);
        for i in 0..10u64 {
            let _ = entry_for([71, i], [0, 0], &o, 8, 64);
        }
        publish_tail();
        assert_eq!(value::ext_stats(CondCache).tail_bytes, one, "a tenth of ten equal entries: the oldest");
        let t0 = value::ext_stats(CondCache).tail_hits;
        let _ = entry_for([71, 9], [0, 0], &o, 8, 64);
        assert_eq!(value::ext_stats(CondCache).tail_hits, t0, "the newest: no tail hit");
        let _ = entry_for([71, 0], [0, 0], &o, 8, 64);
        assert_eq!(value::ext_stats(CondCache).tail_hits, t0 + 1, "the oldest: a tail hit");
        drop_all();
        set_capacity(100 << 20);
    }

    #[test]
    fn the_fingerprint_follows_every_bound_word_null_and_varlena_byte() {
        gravity_dict::fp::install(0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908);
        let shape = [11, 22];
        let short: [u8; 3] = [(3 << 1) | 1, b'a', b'b'];
        let long: [u8; 6] = [6 << 2, 0, 0, 0, b'a', b'b'];
        let (ps, pl) = (short.as_ptr() as u64, long.as_ptr() as u64);
        let mask = |len: usize, v: u64| (0..len).map(|i| (i, v >> i & 1 != 0)).collect::<Vec<_>>();
        let f = |w: &[u64], n: &[u8], v: u64| fingerprint(shape, w, n, &mask(w.len(), v));
        let a = f(&[5, ps], &[0, 0], 2).expect("fp");
        // the one key's producer over the domain, the shape, then each word's null byte and bytes
        let mut want = DOMAIN.to_vec();
        want.extend_from_slice(&11u64.to_le_bytes());
        want.extend_from_slice(&22u64.to_le_bytes());
        want.push(0);
        want.extend_from_slice(&5u64.to_le_bytes());
        assert_eq!(f(&[5], &[0], 0), gravity_dict::fp::fp128(&want));
        // a varlena hashes its bytes, never its pointer: the short and the plain header forms of 'ab' agree
        assert_eq!(f(&[5, pl], &[0, 0], 2), Some(a));
        assert_ne!(f(&[6, ps], &[0, 0], 2), Some(a), "another literal");
        assert_ne!(f(&[5, ps], &[1, 0], 2), Some(a), "a null");
        assert_ne!(fingerprint([11, 23], &[5, ps], &[0, 0], &mask(2, 2)), Some(a), "another shape (type, typmod, collation)");
        assert_ne!(f(&[5, ps], &[0, 0], 0), Some(a), "a word read as a by-value datum");
        let external: [u8; 2] = [0x01, 18];
        let compressed: [u8; 8] = [(8 << 2) | 2, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(f(&[external.as_ptr() as u64], &[0], 1), None);
        assert_eq!(f(&[compressed.as_ptr() as u64], &[0], 1), None);
        assert_eq!(fingerprint(shape, &[1, 2], &[0, 0], &[(1, false), (2, false)]), None, "a word outside the block");
        // the vector has no length cap and no order rule: 100 words, a repeat and a descending pair each key, in order
        let many: Vec<u64> = (0..100).collect();
        let zeros = [0u8; 100];
        let up: Vec<(usize, bool)> = (0..100).map(|i| (i, false)).collect();
        let all = fingerprint(shape, &many, &zeros, &up).expect("100 words");
        let mut want = DOMAIN.to_vec();
        want.extend_from_slice(&11u64.to_le_bytes());
        want.extend_from_slice(&22u64.to_le_bytes());
        for i in 0..100u64 {
            want.push(0);
            want.extend_from_slice(&i.to_le_bytes());
        }
        assert_eq!(Some(all), gravity_dict::fp::fp128(&want));
        let down = fingerprint(shape, &many, &zeros, &[(7, false), (3, false)]).expect("a descending pair");
        assert_ne!(Some(down), fingerprint(shape, &many, &zeros, &[(3, false), (7, false)]), "the order is the need order");
        assert_ne!(fingerprint(shape, &many, &zeros, &[(3, false), (3, false)]), fingerprint(shape, &many, &zeros, &[(3, false)]), "a repeated word is hashed twice");
        assert_eq!(fingerprint(shape, &[5, ps, 7], &[0, 0, 0], &[(1, true), (0, false)]), fingerprint(shape, &[7, pl, 5], &[0, 0, 0], &[(1, true), (2, false)]), "the values in need order, not the positions");
    }

    #[test]
    fn a_dropped_owner_s_entries_are_swept() {
        let _s = serial();
        drop_all();
        set_capacity(100 << 20);
        let o = owner();
        entry_for([5, 5], [5, 5], &o, 4, 64).put(0, &[3], 64, 0);
        drop(o);
        let p = owner();
        for i in 0..SWEEP_FLOOR as u64 - 1 {
            let _ = entry_for([6, i], [6, 6], &p, 4, 64);
        }
        assert!(pgsync::lock(store()).map.contains_key(&([5, 5], [5, 5], ConditionCacheMode::Full as u8)), "no walk per insert: the closed part's entry waits");
        let _ = entry_for([6, 99], [6, 6], &p, 4, 64);
        let s = pgsync::lock(store());
        assert!(!s.map.contains_key(&([5, 5], [5, 5], ConditionCacheMode::Full as u8)), "the store reached its sweep floor: swept");
        assert_eq!((s.map.len(), s.lru.len()), (SWEEP_FLOOR, SWEEP_FLOOR));
        drop(s);
        drop_all();
    }

    /// D4607: a part closed since the last sweep: an insert at least the gap after that sweep lets every closed part's
    /// entries go (a dropped database's or table's), far under the doubling threshold; inside the gap the entry waits.
    #[test]
    fn a_closed_part_s_entries_go_at_the_next_insert_a_gap_after_the_last_sweep() {
        let _s = serial();
        drop_all();
        set_capacity(100 << 20);
        let dead = ([8, 8], [8, 8], ConditionCacheMode::Full as u8);
        let o = owner();
        entry_for(dead.0, dead.1, &o, 4, 64).put(0, &[5], 64, 0);
        drop(o);
        let p = owner();
        let live = ([9, 0], [9, 9], ConditionCacheMode::Full as u8);
        entry_for(live.0, live.1, &p, 4, 64).put(0, &[6], 64, 0);
        // a part closed, but drop_all's stamp is the last sweep's: inside the gap, no walk
        pgrc_read::openpart::note_part_closed();
        let _ = entry_for([9, 1], [9, 9], &p, 4, 64);
        assert!(pgsync::lock(store()).map.contains_key(&dead), "inside the gap after the last sweep: the entry waits");
        // the last sweep a gap ago and a part closed since: the next insert sweeps, at three entries of a floor of 64
        pgsync::lock(store()).swept_ns = 0;
        pgrc_read::openpart::note_part_closed();
        let sweeps = SWEEPS.load(Ordering::Relaxed);
        let _ = entry_for([9, 2], [9, 9], &p, 4, 64);
        let s = pgsync::lock(store());
        assert!(!s.map.contains_key(&dead), "a part closed since the last sweep: its entries go at the next insert");
        assert!(s.map.contains_key(&live) && s.map.contains_key(&([9, 2], [9, 9], ConditionCacheMode::Full as u8)), "a live part's entries stay");
        assert_eq!(s.map.len(), s.lru.len());
        assert_eq!(held(), s.map.values().map(|v| v.bytes()).sum::<u64>(), "the dead entry's bytes left the running total");
        assert!(s.swept_ns > 0 && s.closed_seen > 0, "the sweep stamped its end and the closed count it saw");
        drop(s);
        assert!(SWEEPS.load(Ordering::Relaxed) > sweeps);
        drop_all();
    }

    /// DEAD_BYTES: a closed part's resident entries are dead (their bytes in `dead_bytes`) from the close until the sweep
    /// takes them; a live part's entries, however many, are not.
    #[test]
    fn a_closed_part_s_entries_are_dead_bytes_until_the_sweep() {
        let _s = serial();
        drop_all();
        set_capacity(100 << 20);
        assert_eq!(dead_bytes(), 0, "an empty store");
        let (o, p) = (owner(), owner());
        let dead = ([10, 10], [10, 10], ConditionCacheMode::Full as u8);
        entry_for(dead.0, dead.1, &o, 4, 64).put(0, &[7], 64, 0);
        for i in 0..8u64 {
            entry_for([11, i], [11, 11], &p, 4, 64).put(0, &[1], 64, 0);
        }
        assert_eq!(dead_bytes(), 0, "every part open: the live fill is not dead");
        let bytes = pgsync::lock(store()).map[&dead].bytes();
        drop(o);
        pgrc_read::openpart::note_part_closed();
        assert_eq!(dead_bytes(), bytes, "the closed part's entry is dead, its bytes exactly");
        // the sweep's gap since drop_all's stamp: the next insert sweeps
        pgsync::lock(store()).swept_ns = 0;
        let _ = entry_for([11, 99], [11, 11], &p, 4, 64);
        assert!(!pgsync::lock(store()).map.contains_key(&dead), "swept");
        assert_eq!(dead_bytes(), 0, "the sweep took the dead bytes");
        drop(p);
        assert!(dead_bytes() > 0, "the live part closed: its entries are dead now");
        drop_all();
        assert_eq!(dead_bytes(), 0);
    }

    /// 1a's miss under new literals (main 09-24): thousands of entries, each insert evicting the least recently used one
    /// (a touched entry stays), the bytes held always the residents' sum, and the dropped-owner sweep run once per
    /// doubling of the store, never once per insert.
    #[test]
    fn thousands_of_entries_evict_in_lru_order_without_a_store_walk_per_insert() {
        let _s = serial();
        drop_all();
        set_capacity(100 << 20);
        let o = owner();
        let one = entry_for([0, 1], [0, 0], &o, 8, 64).bytes();
        drop_all();
        set_capacity(one * 1024);
        let (evicted, sweeps) = (EVICTED.load(Ordering::Relaxed), SWEEPS.load(Ordering::Relaxed));
        let kept = ([0, 0], [1, 1], ConditionCacheMode::Full as u8);
        entry_for(kept.0, kept.1, &o, 8, 64).put(0, &[1], 64, 0);
        for i in 1..4096u64 {
            let _ = entry_for([i, 0], [0, 0], &o, 8, 64);
            if i % 100 == 0 {
                assert_eq!(entry_for(kept.0, kept.1, &o, 8, 64).get(0), Hit::Mixed(&[1]), "a touched entry is never the oldest");
            }
            if i % 512 == 0 {
                let s = pgsync::lock(store());
                assert_eq!(held(), s.map.values().map(|v| v.bytes()).sum::<u64>(), "the running total is the residents' sum");
            }
        }
        let s = pgsync::lock(store());
        // the touched entry (one + a words page) and the newest 1 022 inserts fill the budget
        assert_eq!((s.map.len(), s.lru.len()), (1023, 1023));
        assert!(s.map.contains_key(&kept) && s.map.contains_key(&([4095, 0], [0, 0], ConditionCacheMode::Full as u8)) && !s.map.contains_key(&([3073, 0], [0, 0], ConditionCacheMode::Full as u8)));
        assert!(s.map.contains_key(&([3074, 0], [0, 0], ConditionCacheMode::Full as u8)), "the oldest resident insert is the 1 022nd newest");
        assert_eq!(held(), s.map.values().map(|v| v.bytes()).sum::<u64>());
        drop(s);
        assert_eq!(EVICTED.load(Ordering::Relaxed) - evicted, 4095 - 1022);
        assert!(SWEEPS.load(Ordering::Relaxed) - sweeps <= 5, "a sweep per doubling (64 .. 1 024), not per insert");
        drop_all();
        assert_eq!(held(), 0);
        set_capacity(100 << 20);
    }

    /// V4R-33: a granule's count is its words' popcount (none 0, all its rows), unknown until published; the unit's total
    /// appears only once every granule is known, and a second publish of a granule adds nothing.
    #[test]
    fn counts_are_the_published_words_popcounts_and_the_total_waits_for_every_granule() {
        let o = owner();
        let e = Entry::new(&o, 4, 128, ConditionCacheMode::Full);
        assert_eq!((e.count(0), e.part_total()), (None, None));
        e.put(0, &[0; 128], 8192, 0);
        e.put(1, &[!0; 128], 8192, 0);
        let mut w = [0u64; 128];
        w[0] = 0b1011;
        w[127] = 1 << 63;
        e.put(2, &w, 8192, 0);
        assert_eq!((e.count(0), e.count(1), e.count(2), e.count(3)), (Some(0), Some(8192), Some(4), None));
        assert_eq!(e.part_total(), None, "granule 3 unknown");
        e.put(3, &[!0; 128], 100, 64);
        e.put(3, &[0; 128], 100, 64);
        assert_eq!(e.count(3), Some(100), "the first publish stands; bits past the granule's rows ignored");
        assert_eq!(e.part_total(), Some(8192 + 4 + 100));
    }

    /// V4R-33: a lookup under another fingerprint (the verdict_cond `cond.3` domain bump) or another layout (granules, words
    /// a granule) is a miss: a fresh entry with nothing known, never the old entry's words or counts, and no error.
    #[test]
    fn another_fingerprint_or_layout_is_a_miss_with_no_counts() {
        let _s = serial();
        let o = owner();
        let old = entry_for([2, 2], [9, 9], &o, 3, 8192);
        for g in 0..3 {
            old.put(g, &[!0; 128], 8192, 0);
        }
        assert_eq!(old.part_total(), Some(3 * 8192));
        let other_fp = entry_for([3, 3], [9, 9], &o, 3, 8192);
        assert_eq!((other_fp.get(0), other_fp.count(0), other_fp.part_total()), (Hit::Unknown, None, None));
        let other_layout = entry_for([2, 2], [9, 9], &o, 4, 8192);
        assert_eq!((other_layout.get(0), other_layout.count(0), other_layout.part_total()), (Hit::Unknown, None, None));
        drop_all();
    }
}
