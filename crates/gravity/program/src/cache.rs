use std::collections::HashMap;
use std::sync::Arc;

use crate::program::{ContentKey, Program};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Handle(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Generation(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PlanKey {
    pub handle: Handle,
    pub generation: Generation,
}

impl PlanKey {
    /// The handle bit of a content park key (exec `park_key`: the program's content and its recipes' salt); a generic
    /// plan's key never carries it (exec glue `plan_key`).
    pub const CONTENT: u64 = 1 << 63;

    /// D3253: a content park key, whose parked Node any admission of that content may take (its recipes hold nothing of
    /// the admitting statement); else a plan key, whose Node only the admission that built it may take (`Entry::acquire`).
    pub const fn is_content(&self) -> bool {
        self.handle.0 & Self::CONTENT != 0
    }

    /// D3865 L5: the generation word's bit of an identity key: the plan key of a CURRENT-OF-eligible cursor's statement
    /// served over its identity copy (exec entry `current_of`), filed apart from the plain program of the same statement.
    /// The handle stays the plan's, so an invalidation by handle drops both. A plan key's generation keeps 23 bits (exec
    /// glue `plan_key`).
    pub const IDENTITY: u32 = 1 << 23;

    /// D3865 L5: this plan key's identity key.
    pub const fn identity(self) -> PlanKey {
        PlanKey { handle: self.handle, generation: Generation(self.generation.0 | Self::IDENTITY) }
    }

    /// D3865 L5: an identity key (`identity`); never a content key.
    pub const fn is_identity(&self) -> bool {
        !self.is_content() && self.generation.0 & Self::IDENTITY != 0
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub l1_hits: u64,
    pub l2_hits: u64,
    pub misses: u64,
    pub evictions: u64,
}

struct L2Entry {
    key: ContentKey,
    program: Arc<Program>,
    prev: usize,
    next: usize,
    /// D2948: the code bytes `l2_bytes` counted when the entry was filed: a tier-0 program's code grows when it is promoted
    /// (`Code::bytes`), and subtracting its later size at eviction underflowed the count (a debug panic; in a release build
    /// a near-2^64 `l2_bytes`, after which every filing evicted the whole L2).
    bytes: usize,
}

pub struct ProgramCache {
    l1: HashMap<PlanKey, Arc<Program>>,
    l2: HashMap<ContentKey, usize>,
    entries: Vec<L2Entry>,
    oldest: usize,
    newest: usize,
    l2_bytes: usize,
    budget_bytes: usize,
    max_entries: usize,
    stats: CacheStats,
}

const NO_ENTRY: usize = usize::MAX;

impl ProgramCache {
    /// D3620: the tables grow with their filings (up to `max_entries`): a backend level that files one program holds one
    /// entry's table, not `max_entries`' (every level of a deep SQL-function recursion builds its own cache).
    pub fn new(budget_bytes: usize, max_entries: usize) -> ProgramCache {
        ProgramCache { l1: HashMap::new(), l2: HashMap::new(), entries: Vec::new(), oldest: NO_ENTRY, newest: NO_ENTRY, l2_bytes: 0, budget_bytes, max_entries, stats: CacheStats::default() }
    }

    pub fn l1(&mut self, key: PlanKey) -> Option<Arc<Program>> {
        let hit = self.l1.get(&key).cloned();
        if hit.is_some() {
            self.stats.l1_hits += 1;
        }
        hit
    }

    pub fn l2(&mut self, key: ContentKey) -> Option<Arc<Program>> {
        match self.l2.get(&key).copied() {
            Some(i) => {
                self.touch(i);
                self.stats.l2_hits += 1;
                Some(self.entries[i].program.clone())
            }
            None => {
                self.stats.misses += 1;
                None
            }
        }
    }

    pub fn insert(&mut self, plan: Option<PlanKey>, program: Arc<Program>) {
        if let Some(k) = plan {
            self.l1.insert(k, program.clone());
        }
        let content = program.key;
        if let Some(i) = self.l2.get(&content).copied() {
            self.touch(i);
            return;
        }
        self.file(content, program);
    }

    /// File `program` under a front key (`Program::front_key`): a later `l2(front)` returns it. The
    /// entry is an L2 entry like any other (budget, eviction, `reset`).
    pub fn install(&mut self, front: ContentKey, program: Arc<Program>) {
        // Credit the replaced entry before making room. Never serve an old front after replacement,
        // including when its replacement is too large to cache.
        if let Some(i) = self.l2.get(&front).copied() {
            self.remove(i);
        }
        self.file(front, program);
    }

    fn file(&mut self, key: ContentKey, program: Arc<Program>) {
        let bytes = program.code_bytes();
        // An uncacheable program must not flush useful entries or exceed the L2 admission budget.
        // L1 plan ownership and code promoted after filing are accounted separately, as upstream.
        if self.max_entries == 0 || self.budget_bytes == 0 || bytes > self.budget_bytes {
            return;
        }
        while self.l2.len() >= self.max_entries || self.l2_bytes > self.budget_bytes - bytes {
            self.evict_one();
        }
        let i = self.entries.len();
        self.entries.push(L2Entry { key, program, prev: self.newest, next: NO_ENTRY, bytes });
        if self.newest != NO_ENTRY {
            self.entries[self.newest].next = i;
        } else {
            self.oldest = i;
        }
        self.newest = i;
        self.l2.insert(key, i);
        self.l2_bytes += bytes;
    }

    // Dense, index-linked LRU: one hash lookup per hit, no hit allocation, and no cache scan on
    // eviction. Indices survive Vec growth; swap removal repairs only the moved entry's neighbors.
    fn unlink(&mut self, i: usize) {
        let (prev, next) = (self.entries[i].prev, self.entries[i].next);
        if prev == NO_ENTRY { self.oldest = next; } else { self.entries[prev].next = next; }
        if next == NO_ENTRY { self.newest = prev; } else { self.entries[next].prev = prev; }
    }

    fn touch(&mut self, i: usize) {
        if self.newest == i { return; }
        self.unlink(i);
        self.entries[i].prev = self.newest;
        self.entries[i].next = NO_ENTRY;
        self.entries[self.newest].next = i;
        self.newest = i;
    }

    fn remove(&mut self, i: usize) {
        self.unlink(i);
        let old = self.entries.swap_remove(i);
        self.l2.remove(&old.key);
        self.l2_bytes -= old.bytes;
        if i < self.entries.len() {
            let moved = &self.entries[i];
            let (key, prev, next) = (moved.key, moved.prev, moved.next);
            *self.l2.get_mut(&key).expect("moved entry present") = i;
            if prev == NO_ENTRY { self.oldest = i; } else { self.entries[prev].next = i; }
            if next == NO_ENTRY { self.newest = i; } else { self.entries[next].prev = i; }
        }
    }

    fn evict_one(&mut self) {
        if self.oldest != NO_ENTRY {
            self.remove(self.oldest);
            self.stats.evictions += 1;
        }
    }

    pub fn drop_for(&mut self, handle: Handle) -> usize {
        let before = self.l1.len();
        self.l1.retain(|k, _| k.handle != handle);
        before - self.l1.len()
    }

    pub fn reset(&mut self) {
        self.l1.clear();
        self.l2.clear();
        self.entries.clear();
        self.oldest = NO_ENTRY;
        self.newest = NO_ENTRY;
        self.l2_bytes = 0;
    }

    pub fn stats(&self) -> CacheStats {
        self.stats
    }

    pub fn l1_len(&self) -> usize {
        self.l1.len()
    }

    pub fn l2_len(&self) -> usize {
        self.l2.len()
    }

    pub fn l2_bytes(&self) -> usize {
        self.l2_bytes
    }
}

/// The process-wide program tier (PROGCACHE, V4R-35): the values a
/// backend filed under a catalog-stamped graph key (exec admit.rs `shared_key`), shared read-only by every backend
/// thread behind its own `ProgramCache`. `SHARDS` shards, each an LRU bounded by entries and bytes; a lookup takes
/// its shard's read lock and stamps the entry's use with one atomic store. Eviction drops the tier's `Arc` only: a
/// backend that took the value keeps it. Plan-shape keyed, never results.
pub struct SharedCache<V> {
    shards: Box<[pgsync::RwLock<SharedShard<V>>]>,
    clock: std::sync::atomic::AtomicU64,
    hits: std::sync::atomic::AtomicU64,
    misses: std::sync::atomic::AtomicU64,
    inserts: std::sync::atomic::AtomicU64,
    evictions: std::sync::atomic::AtomicU64,
}

struct SharedShard<V> {
    entries: HashMap<u128, SharedEntry<V>>,
    bytes: usize,
    max_entries: usize,
    budget_bytes: usize,
}

impl<V> SharedShard<V> {
    fn remove(&mut self, key: &u128) -> Option<SharedEntry<V>> {
        let entry = self.entries.remove(key)?;
        self.bytes -= entry.bytes;
        Some(entry)
    }
}

struct SharedEntry<V> {
    value: V,
    bytes: usize,
    last_use: std::sync::atomic::AtomicU64,
}

/// The tier's census (tests, the report line).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SharedStats {
    pub hits: u64,
    pub misses: u64,
    pub inserts: u64,
    pub evictions: u64,
    pub entries: usize,
    pub bytes: usize,
}

pub const SHARDS: usize = 16;

impl<V: Clone> SharedCache<V> {
    /// Divide both hard limits across shards, including remainders. A zero limit disables admission.
    pub fn new(max_entries: usize, budget_bytes: usize) -> SharedCache<V> {
        use std::sync::atomic::AtomicU64;
        let shards = (0..SHARDS).map(|i| pgsync::RwLock::new(SharedShard {
            entries: HashMap::new(), bytes: 0,
            max_entries: max_entries / SHARDS + usize::from(i < max_entries % SHARDS),
            budget_bytes: budget_bytes / SHARDS + usize::from(i < budget_bytes % SHARDS),
        })).collect::<Vec<_>>().into_boxed_slice(); // alloc: Cold
        SharedCache { shards, clock: AtomicU64::new(0), hits: AtomicU64::new(0), misses: AtomicU64::new(0), inserts: AtomicU64::new(0), evictions: AtomicU64::new(0) }
    }

    fn shard(&self, key: u128) -> &pgsync::RwLock<SharedShard<V>> {
        let h = (key as u64) ^ ((key >> 64) as u64);
        &self.shards[gravity_abi::hash::phi_top(h, 4) as usize % SHARDS]
    }

    fn tick(&self) -> u64 {
        self.clock.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
    }

    pub fn get(&self, key: u128) -> Option<V> {
        use std::sync::atomic::Ordering::Relaxed;
        let s = self.shard(key).read().unwrap_or_else(|e| e.into_inner());
        match s.entries.get(&key) {
            Some(e) => {
                e.last_use.store(self.tick(), Relaxed);
                self.hits.fetch_add(1, Relaxed);
                Some(e.value.clone())
            }
            None => {
                self.misses.fetch_add(1, Relaxed);
                None
            }
        }
    }

    /// Files `value` (`bytes` its footprint) under `key`: a key already filed keeps the first value (two backends that
    /// lowered one shape at once built equal programs) and returns it; else the value filed. A value over the shard's
    /// whole budget is not filed.
    pub fn insert(&self, key: u128, value: V, bytes: usize) -> V {
        self.insert_filed(key, value, bytes).0
    }

    /// `insert`, and whether `value` itself was filed (false: the key's earlier value came back, or `value` is over the
    /// shard's budget and came back unfiled).
    pub fn insert_filed(&self, key: u128, value: V, bytes: usize) -> (V, bool) {
        self.insert_over(key, value, bytes, |_| true)
    }

    /// `insert_filed`, where a value already filed is kept only while `live` holds of it (a stale one is replaced).
    pub fn insert_over(&self, key: u128, value: V, bytes: usize, live: impl Fn(&V) -> bool) -> (V, bool) {
        use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
        let mut s = self.shard(key).write().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = s.entries.get(&key) {
            if live(&e.value) {
                e.last_use.store(self.tick(), Relaxed);
                return (e.value.clone(), false);
            }
            s.remove(&key);
            self.evictions.fetch_add(1, Relaxed);
        }
        if s.max_entries == 0 || s.budget_bytes == 0 || bytes > s.budget_bytes {
            return (value, false);
        }
        // O(1) byte accounting on the uncontended insertion path. Keep shared/read-locked hits;
        // an eviction still scans its bounded shard rather than making every hit a write lock.
        while s.entries.len() >= s.max_entries || s.bytes > s.budget_bytes - bytes {
            let victim = s.entries.iter().min_by_key(|(_, e)| e.last_use.load(Relaxed)).map(|(k, _)| *k).expect("a non-empty shard");
            s.remove(&victim);
            self.evictions.fetch_add(1, Relaxed);
        }
        s.entries.insert(key, SharedEntry { value: value.clone(), bytes, last_use: AtomicU64::new(self.tick()) }); // alloc: Program
        s.bytes += bytes;
        self.inserts.fetch_add(1, Relaxed);
        (value, true)
    }

    pub fn contains(&self, key: u128) -> bool {
        self.shard(key).read().unwrap_or_else(|e| e.into_inner()).entries.contains_key(&key)
    }

    /// D4579: takes out every entry whose value `doomed` names (one that can never be served again: its database dropped,
    /// a relation it read superseded) and hands each to `gone` outside the shard's lock, where the tier's `Arc` drops. Per
    /// shard: ONE read pass collects up to `PASS` doomed keys, ONE write lock takes them (`doomed` asked again there),
    /// then their values drop; a shard with nothing doomed is only read. Without allocating: the pass's keys and values are
    /// on the stack. The caller sizes `PASS` to its shards' entry bound (the program tier: SHARED_ENTRIES / SHARDS), so
    /// each shard takes one pass, O(entries) `doomed` calls; a shard over `PASS` takes one more pass per full one. The
    /// entries taken.
    pub fn remove_where<const PASS: usize>(&self, doomed: impl Fn(&V) -> bool, mut gone: impl FnMut(V)) -> usize {
        use std::sync::atomic::Ordering::Relaxed;
        assert!(PASS > 0, "remove_where requires a nonzero pass size");
        let mut n = 0;
        for s in self.shards.iter() {
            // bounded("a shard's doomed entries, PASS per pass: a pass that finds fewer, or takes none, ends the shard")
            loop {
                let mut keys = [0u128; PASS];
                let mut found = 0;
                {
                    let r = s.read().unwrap_or_else(|e| e.into_inner());
                    // bounded("a shard's entries, read once per pass")
                    for (k, e) in r.entries.iter() {
                        if doomed(&e.value) {
                            keys[found] = *k;
                            found += 1;
                            if found == PASS {
                                break;
                            }
                        }
                    }
                }
                if found == 0 {
                    break;
                }
                let mut taken: [Option<V>; PASS] = std::array::from_fn(|_| None);
                let mut took = 0;
                {
                    let mut w = s.write().unwrap_or_else(|e| e.into_inner());
                    // bounded("the pass's keys, at most PASS")
                    for (k, slot) in keys[..found].iter().zip(taken.iter_mut()) {
                        if w.entries.get(k).is_some_and(|e| doomed(&e.value)) {
                            *slot = w.remove(k).map(|e| e.value);
                            took += 1;
                        }
                    }
                }
                // bounded("the pass's values, at most PASS")
                for v in taken.into_iter().flatten() {
                    n += 1;
                    self.evictions.fetch_add(1, Relaxed);
                    gone(v);
                }
                if found < PASS || took == 0 {
                    break;
                }
            }
        }
        n
    }

    pub fn reset(&self) {
        for s in self.shards.iter() {
            let mut s = s.write().unwrap_or_else(|e| e.into_inner());
            s.entries.clear();
            s.bytes = 0;
        }
    }

    pub fn stats(&self) -> SharedStats {
        use std::sync::atomic::Ordering::Relaxed;
        let (mut entries, mut bytes) = (0, 0);
        for s in self.shards.iter() {
            let s = s.read().unwrap_or_else(|e| e.into_inner());
            entries += s.entries.len();
            bytes += s.bytes;
        }
        SharedStats { hits: self.hits.load(Relaxed), misses: self.misses.load(Relaxed), inserts: self.inserts.load(Relaxed), evictions: self.evictions.load(Relaxed), entries, bytes }
    }
}
