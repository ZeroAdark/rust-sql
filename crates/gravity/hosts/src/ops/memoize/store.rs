//! `nest.memo`'s store (M8-plan §3.4, cards/M8.md L4; a `#[path]` submodule of hosts v4/m8.rs): PostgreSQL's Memoize
//! cache (nodeMemoize.c) for one execution. Entries keyed by the parameter values, each an LRU node holding the rows the
//! inner handed on for that key; `get_hash_memory_limit()` bytes at most, PG's accounting (`EMPTY_ENTRY_MEMORY_BYTES`,
//! `CACHE_TUPLE_BYTES`) and eviction from the LRU head (`cache_reduce_memory`). The lowered IR drives it as ExecMemoize's
//! states do:
//! - `lookup` (MEMO_CACHE_LOOKUP): a complete entry is a hit (its rows' address, moved to the LRU tail); an incomplete one
//!   (a scan cut short: a LIMIT above, a SEMI / ANTI leave) is purged and refilled, as `entry_purge_tuples`; a new key
//!   makes an empty entry, and a table past its limit evicts, which may evict the new entry itself (`Miss { current:
//!   false }`: PG's `cache_lookup` returning NULL, the scan then runs uncached).
//! - `store` (MEMO_FILLING_CACHE): one row appended to the current entry while the inner hands it on (never ahead of it:
//!   a row PostgreSQL never fetches is never evaluated); `singlerow` completes the entry at its first row; past the limit
//!   the table evicts, and losing the current entry is MEMO_CACHE_BYPASS_MODE (`false`) until the next lookup.
//! - `done`: the inner reached its end, the entry is complete (an empty result is cached as PG caches it).
//!
//! Keys compare as the plan says (`Memoize.binary_mode`): by image (`datum_image_eq`: the null flags, then the bytes;
//! equal bytes are equal values, so a byte comparison never hits wrongly) under `binary_mode`, else by the key's hash
//! equality operator (PG's `ExecBuildParamSetEqual`: NOT DISTINCT over the operator's function, the table's key first)
//! and the operator's hash function, each with the key's collation, through `FmgrCall`. The functions are resolved by
//! the protocol (exec v4/m8_memo.rs) and bound here. Owner M8-MEMO-CTE.
use datum::Datum;
use gravity_fn::fmgr_call::FmgrCall;
use memledger::charge::{Charge, Door, Owner, Release};
use memledger::Kind;

/// The store's raw half (lint R6): the probe and row reads through the IR's addresses (`lookup`, `store`).
mod raw;

/// One key's function calls under `!binary_mode`: the operator's hash function and its equality function.
pub struct KeyFns {
    pub hash: FmgrCall,
    pub eq: FmgrCall,
    /// D2988: `eq` is a same-type by-value equality that is word equality (int2 / int4 / int8 / oid / bool / char / date
    /// `=`), so the key compares its word without the call, as binary mode does (D3075: and hashes its word without the
    /// hash call: equal words are the only equal values, so any function of the word is a hash the table may use; the
    /// hash is the table's own, never shown)
    pub word_eq: bool,
}

/// A probe key as the IR lays it: the datum word, NULL, and for a by-reference datum its image's bytes (0: by value).
#[derive(Clone, Copy, Debug)]
pub struct Probe {
    pub word: u64,
    pub null: bool,
    pub len: u64,
}

/// A stored key: the word of a by-value datum, or the image copied (8-aligned words) of a by-reference one.
struct Stored {
    null: bool,
    word: u64,
    image: Option<Box<[u64]>>,
    len: usize,
}

impl Stored {
    /// The key as a Datum the equality function reads.
    fn datum(&self) -> Datum {
        match &self.image {
            Some(b) => Datum::from_u64(b.as_ptr() as u64),
            None => Datum::from_u64(self.word),
        }
    }
}

/// A row cell as the IR lays it for `store`: the datum word, NULL, the image's bytes when by reference (0: by value).
#[derive(Clone, Copy, Debug)]
pub struct Col {
    pub word: u64,
    pub null: bool,
    pub len: u64,
}

/// D3075: one entry's probe half, packed (40 bytes, the table's `hot` slab): what a hit reads (its hash, its chain and LRU
/// links, a lone word key, its rows' address), so a probe and the LRU move touch this slab only, never the entry's key and
/// row allocations (`Cold`). Links are slab indexes (`NIL` none).
#[derive(Clone, Copy)]
struct Hot {
    hash: u32,
    prev: u32,
    next: u32,
    /// The next entry of the same bucket (`MemoTable::buckets` holds the head).
    chain: u32,
    /// D2988: a lone non-null by-value key's word (`WORD1`), compared beside the chain under binary mode or a word-equality
    /// operator (`KeyFns::word_eq`) without the call nor reading `Cold::key`.
    word: u64,
    /// `Cold::rows`' address (`[n, cols, (word, null) x cols x n]`), kept current by every write of the rows.
    rows: u64,
    flags: u32,
}

/// `Hot::flags`: a live entry, a complete one, a `Hot::word` key.
const LIVE: u32 = 1;
const COMPLETE: u32 = 2;
const WORD1: u32 = 4;

impl Hot {
    const FREE: Hot = Hot { hash: 0, prev: NIL, next: NIL, chain: NIL, word: 0, rows: 0, flags: 0 };
}

/// One entry's owned half: its key, its rows (`rows[0]` = the row count, `rows[1]` = the columns, then per row per column
/// the datum word and the NULL word), the by-reference copies its rows point into, and the bytes PG counts for it
/// (`EMPTY_ENTRY_MEMORY_BYTES` plus each row's `CACHE_TUPLE_BYTES`). A free slot holds empty vectors.
#[derive(Default)]
struct Cold {
    key: Vec<Stored>,
    rows: Vec<u64>,
    copies: Vec<Box<[u64]>>,
    mem: u64,
    /// CONVERT wave 2 (site 11): the heap bytes this entry's own blocks hold (the key vector and images, the rows'
    /// capacity, the copies and their vector), as the allocator handed them out (`MemoTable::heap_bytes` sums them).
    heap: u64,
}

/// `sizeof(MemoizeEntry) + sizeof(MemoizeKey)` and a MinimalTuple's header (PG's 64-bit layout).
const ENTRY_BYTES: u64 = 24 + 24;
const TUPLE_BYTES: u64 = 16;
const MINTUPLE_HEADER: u64 = 16;
/// No link.
const NIL: u32 = u32::MAX;

/// What a lookup found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lookup {
    /// A complete entry: the address of its rows (`[n, cols, (word, null) x cols x n]`), stable until the next lookup.
    Hit(*const u64),
    /// No complete entry: the inner runs. `current`: an entry takes its rows (`false`: the scan runs uncached).
    Miss { current: bool },
}

/// The most the cache's charge lags its heap bytes, either way, between two syncs (RULINGS-1 R1: a batched charge, the
/// lag bounded and stated); exact at a purge, and the drop takes what is committed.
const SYNC_SLACK: u64 = 64 << 10;

/// One execution's Memoize cache.
pub struct MemoTable {
    /// CONVERT wave 2 (site 11): the cache's heap bytes (`heap_bytes`) charged to the owner `own` gives it (the breaker
    /// the memo's pipeline runs for), `charged` of them committed: re-synced after every lookup and store once the lag
    /// reaches `SYNC_SLACK`, exactly at `purge_all`. An evicted entry's bytes leave the charge as they leave the cache.
    /// Declared first, so dropped first (door 1: the bytes move to the freeing thread's account, where the frees land).
    charge: Charge,
    charged: u64,
    /// `own` ran; `on`: with an owner.
    owned: bool,
    on: bool,
    /// Every live entry's `Cold::heap`.
    entries_heap: u64,
    binary: bool,
    singlerow: bool,
    limit: u64,
    fns: Vec<KeyFns>,
    /// D3075: the entries' slabs, one index per entry (`Hot` the probe half, `Cold` the owned half); `free` the dead slots.
    hot: Vec<Hot>,
    cold: Vec<Cold>,
    free: Vec<u32>,
    live: usize,
    /// D3075: buckets by the hash's low bits (a power-of-two length, 0 before the first entry): the first entry there, the
    /// rest by `Hot::chain`. The hash is already murmur-mixed, so it indexes as is (no second hasher).
    buckets: Vec<u32>,
    head: u32,
    tail: u32,
    mem_used: u64,
    /// The entry the running scan fills (PG's `mstate->entry` while MEMO_FILLING_CACHE).
    current: Option<usize>,
    pub stats: Stats,
}

/// EXPLAIN ANALYZE's counters (M12 reads them; kept as PG keeps them).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub overflows: u64,
    pub mem_peak: u64,
}

impl MemoTable {
    /// A cache under `limit` bytes; `fns` one per key unless `binary`.
    pub fn new(binary: bool, singlerow: bool, limit: u64, fns: Vec<KeyFns>) -> MemoTable {
        MemoTable { charge: Charge::NONE, charged: 0, owned: false, on: false, entries_heap: 0, binary, singlerow, limit, fns, hot: Vec::new(), cold: Vec::new(), free: Vec::new(), live: 0, buckets: Vec::new(), head: NIL, tail: NIL, mem_used: 0, current: None, stats: Stats::default() } // alloc: Exec
    }

    pub fn mem_used(&self) -> u64 {
        self.mem_used
    }

    /// Whether the cache still waits for its owner (`own`): its caller reads it after a lookup, so the lookup of the owner
    /// is once.
    pub fn needs_owner(&self) -> bool {
        !self.owned
    }

    /// The owner the cache's bytes are charged to from now, with what it holds already (`None`: no one; a unit test's
    /// table, a pipeline with no owner).
    pub fn own(&mut self, owner: Option<(Owner, Kind)>) {
        self.owned = true;
        if let Some((o, k)) = owner {
            (self.charge, self.charged, self.on) = (Charge::open(o, Release::Statement, k, Door::Heap, 0), 0, true);
            self.sync(0);
        }
    }

    /// The heap bytes the cache holds: every live entry's own blocks, and the slabs', free list's and buckets' blocks.
    pub fn heap_bytes(&self) -> u64 {
        let slabs = self.hot.capacity() * core::mem::size_of::<Hot>() + self.cold.capacity() * core::mem::size_of::<Cold>();
        self.entries_heap + (slabs + (self.free.capacity() + self.buckets.capacity()) * 4) as u64
    }

    /// The bytes the charge has committed (tests).
    #[cfg(test)]
    fn charged(&self) -> u64 {
        self.charged
    }

    /// `heap_bytes` counted again from every live entry's blocks (tests: the running sums against a full walk).
    #[cfg(test)]
    fn recount(&self) -> u64 {
        let one = |c: &Cold| {
            let key = c.key.capacity() * core::mem::size_of::<Stored>() + c.key.iter().map(|k| k.image.as_ref().map_or(0, |b| b.len() * 8)).sum::<usize>();
            key + c.rows.capacity() * 8 + c.copies.capacity() * core::mem::size_of::<Box<[u64]>>() + c.copies.iter().map(|b| b.len() * 8).sum::<usize>()
        };
        let entries = (0..self.hot.len()).filter(|&i| self.hot[i].flags & LIVE != 0).map(|i| one(&self.cold[i]) as u64).sum::<u64>();
        entries + self.heap_bytes() - self.entries_heap
    }

    /// The charge re-synced to `heap_bytes` once they differ by `slack` or more (0: exactly): a commit of blocks this
    /// thread allocated for the cache, a free of blocks it just freed (evicted, purged).
    fn sync(&mut self, slack: u64) {
        if !self.on {
            return;
        }
        let (h, c) = (self.heap_bytes(), self.charged);
        if h > c && h - c >= slack {
            self.charge.commit(h - c);
            self.charged = h;
        } else if c > h && c - h >= slack {
            self.charge.free(c - h);
            self.charged = h;
        }
    }

    pub fn len(&self) -> usize {
        self.live
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The inner's end with an entry being filled: it is complete (an empty one included).
    pub fn done(&mut self) {
        if let Some(s) = self.current.take() {
            self.hot[s].flags |= COMPLETE;
        }
    }

    /// PG's `cache_purge_all` (ExecReScanMemoize, a changed param outside the keys): every entry, its key image and row
    /// copies dropped, the LRU and the running scan's entry cleared, `mem_used` 0; the entries count as evictions.
    pub fn purge_all(&mut self) {
        self.stats.evictions += self.len() as u64;
        (self.head, self.tail, self.mem_used, self.current, self.live, self.entries_heap) = (NIL, NIL, 0, None, 0, 0);
        self.hot.clear();
        self.cold.clear();
        self.free.clear();
        self.buckets.clear();
        self.sync(0);
    }

    /// `cache_reduce_memory`: entries evicted from the LRU head until the table is within its limit; `false` when `keep`
    /// was among them.
    fn reduce(&mut self, keep: usize) -> bool {
        self.stats.mem_peak = self.stats.mem_peak.max(self.mem_used);
        let mut intact = true;
        // a label-only step (no tick), opened only on this over-the-limit road: a long eviction is checker 2's under its own label
        let _step = cancelpoll::step("memoize store reduce: entries evicted down to the limit");
        // bounded("other: PG's cache_reduce_memory: least recently used entries evicted until the cache is within its limit: each pass only unlinks one entry (its LRU and bucket links, by slab index) and frees its key, rows and copies, with no key compared and no function called; at most the cache's entries, each one a lookup stored (amortized over them); a stop would leave the cache over its limit, and PG polls no deeper")
        while self.mem_used > self.limit && self.head != NIL {
            let s = self.head as usize;
            if s == keep {
                intact = false;
            }
            self.remove(s);
            self.stats.evictions += 1;
        }
        intact
    }

    /// The bucket of hash `h` (the bucket array is not empty).
    fn bucket(&self, h: u32) -> usize {
        h as usize & (self.buckets.len() - 1)
    }

    /// Live entry `s` (its `Hot::hash` set) chained into its bucket; the buckets double (every live entry re-chained) when
    /// the live entries outnumber them.
    fn chain_in(&mut self, s: usize) {
        self.live += 1;
        if self.live > self.buckets.len() {
            // a power of two, twice the live entries: doubled each time they outnumber it
            let n = (self.live * 2).next_power_of_two();
            self.buckets.clear();
            self.buckets.resize(n, NIL); // alloc: Exec charged self.charge
            // a label-only step (no tick), opened only when the buckets double: a long relink is checker 2's under its own label
            let _step = cancelpoll::step("memoize store chain_in: the entries relinked into the doubled buckets");
            // bounded("other: the buckets' doubling: each pass only relinks one slab entry into its bucket by its stored hash, with no key read and no function called; run when the live entries outnumber the buckets, so its cost is amortized over the lookups that stored them; a doubling runs to its end (a stop would leave entries out of their chains), as PG's simplehash grow relinks the same with no poll")
            for i in 0..self.hot.len() {
                if self.hot[i].flags & LIVE != 0 {
                    let b = self.bucket(self.hot[i].hash);
                    self.hot[i].chain = self.buckets[b];
                    self.buckets[b] = i as u32;
                }
            }
            return;
        }
        let b = self.bucket(self.hot[s].hash);
        self.hot[s].chain = self.buckets[b];
        self.buckets[b] = s as u32;
    }

    fn remove(&mut self, s: usize) {
        self.unlink(s);
        // the entry out of its bucket's chain
        let (b, next) = (self.bucket(self.hot[s].hash), self.hot[s].chain);
        if self.buckets[b] == s as u32 {
            self.buckets[b] = next;
        } else {
            let mut c = self.buckets[b];
            // bounded("other: one bucket's chain walked to the entry before the removed one: the buckets are as many as the live entries or more (`chain_in`), so a chain is a few entries of a mixed hash")
            while c != NIL {
                if self.hot[c as usize].chain == s as u32 {
                    self.hot[c as usize].chain = next;
                    break;
                }
                c = self.hot[c as usize].chain;
            }
        }
        self.mem_used -= self.cold[s].mem;
        self.entries_heap -= self.cold[s].heap;
        self.hot[s] = Hot::FREE;
        self.cold[s] = Cold::default();
        self.live -= 1;
        if self.current == Some(s) {
            self.current = None;
        }
        self.free.push(s as u32); // alloc: Exec charged self.charge
    }

    fn unlink(&mut self, s: usize) {
        let Hot { prev: p, next: n, .. } = self.hot[s];
        match p {
            NIL => self.head = n,
            p => self.hot[p as usize].next = n,
        }
        match n {
            NIL => self.tail = p,
            n => self.hot[n as usize].prev = p,
        }
    }

    fn push_tail(&mut self, s: usize) {
        let t = self.tail;
        self.hot[s].prev = t;
        self.hot[s].next = NIL;
        match t {
            NIL => self.head = s as u32,
            t => self.hot[t as usize].next = s as u32,
        }
        self.tail = s as u32;
    }

    /// `dlist_move_tail` of the LRU list.
    fn touch(&mut self, s: usize) {
        if self.tail != s as u32 {
            self.unlink(s);
            self.push_tail(s);
        }
    }

    /// The chains' count (tests).
    #[cfg(test)]
    fn chains(&self) -> usize {
        self.buckets.iter().filter(|&&b| b != NIL).count()
    }
}

/// A key's bytes as PG counts the key tuple (`params->t_len`).
fn key_bytes(k: &[Stored]) -> u64 {
    MINTUPLE_HEADER + k.iter().map(|s| if s.image.is_some() { (s.len as u64).next_multiple_of(8) } else { 8 }).sum::<u64>()
}

/// PG's `murmurhash32` finalizer.
fn murmur32(data: u32) -> u32 {
    let mut h = data;
    h ^= h >> 16;
    h = h.wrapping_mul(0x85eb_ca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2_ae35);
    h ^= h >> 16;
    h
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use super::*;

    fn int(v: u64) -> Probe {
        Probe { word: v, null: false, len: 0 }
    }

    fn col(v: u64) -> Col {
        Col { word: v, null: false, len: 0 }
    }

    /// Row `r` of a hit's rows: its (word, null) pairs.
    unsafe fn rows(at: *const u64) -> Vec<Vec<(u64, u64)>> {
        let (n, c) = (*at as usize, *at.add(1) as usize);
        (0..n).map(|r| (0..c).map(|k| (*at.add(2 + 2 * (r * c + k)), *at.add(3 + 2 * (r * c + k)))).collect()).collect()
    }

    /// CONVERT wave 2 (site 11): with by-reference keys and rows (images, copies), refills and evictions past the limit,
    /// the running heap bytes equal a full recount, the owner's charge follows them within `SYNC_SLACK` at every call,
    /// exactly at `purge_all`, and leaves with the table.
    #[test]
    fn the_cache_heap_bytes_are_on_the_owners_charge_through_evictions_purges_and_the_drop() {
        std::thread::spawn(|| {
            memledger::bind_backend(3994, 0);
            memledger::statement_begin();
            let (o, _) = memledger::charge::context();
            let committed = || memledger::charge::owner_row(o).committed;
            let base = committed();
            let mut t = MemoTable::new(true, false, 256 << 10, Vec::new()); // alloc: Cold
            let text = [7u8; 300];
            let check = |t: &MemoTable, at: &str| {
                let (h, c) = (t.heap_bytes(), t.charged());
                assert_eq!(h, t.recount(), "{at}: the running heap bytes");
                assert_eq!(committed() - base, c as i64, "{at}: the owner's committed bytes");
                assert!(h.abs_diff(c) < SYNC_SLACK, "{at}: heap {h}, charged {c}");
            };
            unsafe {
                t.lookup(&[Probe { word: text.as_ptr() as u64, null: false, len: 40 }, int(u64::MAX)]).unwrap();
                assert!(t.needs_owner());
                t.own(Some((o, memledger::Kind::Region)));
                check(&t, "owned after the first lookup");
                for k in 0..3000u64 {
                    let key = Probe { word: text.as_ptr() as u64, null: false, len: 8 + k % 200 };
                    if t.lookup(&[key, int(k)]).unwrap() == (Lookup::Miss { current: true }) {
                        for r in 0..(k % 5) {
                            t.store(&[col(r), Col { word: text.as_ptr() as u64, null: false, len: 16 + r * 50 }]).unwrap();
                        }
                        if k % 7 != 0 {
                            t.done();
                        }
                    }
                    check(&t, &format!("key {k}")); // alloc: Cold
                }
                assert!(t.stats.evictions > 0 && t.heap_bytes() > SYNC_SLACK, "evictions {}, heap {}", t.stats.evictions, t.heap_bytes());
                t.purge_all();
                assert_eq!(t.charged(), t.heap_bytes(), "exact at the purge");
                check(&t, "purged");
            }
            drop(t);
            assert_eq!(committed(), base, "the table's bytes left the owner with it");
            memledger::statement_end();
            memledger::unbind_backend();
        })
        .join()
        .unwrap();
    }

    #[test]
    fn a_complete_entry_hits_with_its_rows_and_an_empty_result_is_cached() {
        let mut t = MemoTable::new(true, false, 1 << 20, Vec::new()); // alloc: Cold
        unsafe {
            assert_eq!(t.lookup(&[int(7)]).unwrap(), Lookup::Miss { current: true });
            assert!(t.store(&[col(1), col(2)]).unwrap());
            assert!(t.store(&[col(3), Col { word: 0, null: true, len: 0 }]).unwrap());
            t.done();
            let Lookup::Hit(at) = t.lookup(&[int(7)]).unwrap() else { panic!("a hit") };
            assert_eq!(rows(at), vec![vec![(1, 0), (2, 0)], vec![(3, 0), (0, 1)]]); // alloc: Cold
            assert_eq!(t.lookup(&[int(8)]).unwrap(), Lookup::Miss { current: true });
            t.done();
            let Lookup::Hit(at) = t.lookup(&[int(8)]).unwrap() else { panic!("an empty hit") };
            assert!(rows(at).is_empty());
        }
        assert_eq!((t.stats.hits, t.stats.misses), (2, 2));
    }

    #[test]
    fn an_incomplete_entry_is_purged_and_refilled_never_read_as_complete() {
        let mut t = MemoTable::new(true, false, 1 << 20, Vec::new()); // alloc: Cold
        unsafe {
            t.lookup(&[int(1)]).unwrap();
            t.store(&[col(10)]).unwrap();
            // the scan was cut short (a LIMIT above the nest): no done
            assert_eq!(t.lookup(&[int(1)]).unwrap(), Lookup::Miss { current: true });
            t.store(&[col(10)]).unwrap();
            t.store(&[col(11)]).unwrap();
            t.done();
            let Lookup::Hit(at) = t.lookup(&[int(1)]).unwrap() else { panic!("a hit") };
            assert_eq!(rows(at), vec![vec![(10, 0)], vec![(11, 0)]]); // alloc: Cold
        }
    }

    #[test]
    fn purge_all_drops_every_entry_and_the_running_fill() {
        let mut t = MemoTable::new(true, false, 1 << 20, Vec::new()); // alloc: Cold
        unsafe {
            for k in 0..3 {
                t.lookup(&[int(k)]).unwrap();
                t.store(&[col(k)]).unwrap();
                t.done();
            }
            t.lookup(&[int(9)]).unwrap();
            t.store(&[col(9)]).unwrap();
            t.purge_all();
            assert_eq!((t.len(), t.mem_used()), (0, 0));
            assert_eq!(t.stats.evictions, 4, "the three complete entries and the one being filled");
            assert!(!t.store(&[col(1)]).unwrap(), "no fill survives a purge (bypass until the next lookup)");
            for k in [0, 9] {
                assert_eq!(t.lookup(&[int(k)]).unwrap(), Lookup::Miss { current: true }, "key {k} refills after the purge");
                assert!(t.store(&[col(k + 100)]).unwrap());
                t.done();
                let Lookup::Hit(at) = t.lookup(&[int(k)]).unwrap() else { panic!("a hit") };
                assert_eq!(rows(at), vec![vec![(k + 100, 0)]]); // alloc: Cold
            }
            assert_eq!(t.len(), 2);
        }
    }

    #[test]
    fn null_keys_are_not_distinct_and_singlerow_completes_at_the_first_row() {
        let mut t = MemoTable::new(true, true, 1 << 20, Vec::new()); // alloc: Cold
        let null = Probe { word: 0, null: true, len: 0 };
        unsafe {
            t.lookup(&[null, int(2)]).unwrap();
            t.store(&[col(5)]).unwrap();
            let Lookup::Hit(at) = t.lookup(&[null, int(2)]).unwrap() else { panic!("singlerow: complete after one row") };
            assert_eq!(rows(at), vec![vec![(5, 0)]]); // alloc: Cold
            assert_eq!(t.lookup(&[int(0), int(2)]).unwrap(), Lookup::Miss { current: true });
            t.store(&[col(6)]).unwrap();
            let e = t.store(&[col(7)]).unwrap_err();
            assert_eq!(e.message(), "cache entry already complete");
        }
    }

    #[test]
    fn binary_keys_compare_by_image() {
        let mut t = MemoTable::new(true, false, 1 << 20, Vec::new()); // alloc: Cold
        let (a, b) = (b"\x0babc".to_vec(), b"\x0babd".to_vec());
        let p = |v: &Vec<u8>| Probe { word: v.as_ptr() as u64, null: false, len: v.len() as u64 };
        unsafe {
            t.lookup(&[p(&a)]).unwrap();
            t.store(&[col(1)]).unwrap();
            t.done();
            assert!(matches!(t.lookup(&[p(&a.clone())]).unwrap(), Lookup::Hit(_)));
            assert_eq!(t.lookup(&[p(&b)]).unwrap(), Lookup::Miss { current: true });
        }
    }

    #[test]
    fn keys_of_one_hash_chain_and_either_end_of_the_chain_evicts() {
        // 122032 and 132242 hash alike (`image_hash` folds them to one u32); room for two single-row entries
        let (a, b, c) = (122_032, 132_242, 7);
        let limit = 2 * (ENTRY_BYTES + MINTUPLE_HEADER + 8 + TUPLE_BYTES + MINTUPLE_HEADER + 8) + 8;
        let mut t = MemoTable::new(true, false, limit, Vec::new()); // alloc: Cold
        unsafe {
            let fill = |t: &mut MemoTable, k: u64| {
                assert_eq!(t.lookup(&[int(k)]).unwrap(), Lookup::Miss { current: true }, "key {k} misses");
                assert!(t.store(&[col(k)]).unwrap());
                t.done();
            };
            let hit = |t: &mut MemoTable, k: u64| {
                let Lookup::Hit(at) = t.lookup(&[int(k)]).unwrap() else { panic!("key {k} hits") };
                assert_eq!(rows(at), vec![vec![(k, 0)]]); // alloc: Cold
            };
            fill(&mut t, a);
            fill(&mut t, b);
            hit(&mut t, a);
            hit(&mut t, b);
            assert_eq!(t.chains(), 1, "one hash, one chain");
            // the LRU head is a, the chain's last entry
            fill(&mut t, c);
            hit(&mut t, b);
            fill(&mut t, a);
            // c went; the chain is a then b: touch b so a, the chain's first entry, is the LRU head
            hit(&mut t, b);
            fill(&mut t, c);
            hit(&mut t, b);
            assert_eq!(t.lookup(&[int(a)]).unwrap(), Lookup::Miss { current: true }, "a went from the chain's head");
        }
    }

    #[test]
    fn past_the_limit_the_lru_head_goes_and_losing_the_current_entry_bypasses() {
        // room for about two single-row entries
        let limit = 2 * (ENTRY_BYTES + MINTUPLE_HEADER + 8 + TUPLE_BYTES + MINTUPLE_HEADER + 8) + 8;
        let mut t = MemoTable::new(true, false, limit, Vec::new()); // alloc: Cold
        unsafe {
            for k in 0..2 {
                t.lookup(&[int(k)]).unwrap();
                assert!(t.store(&[col(k)]).unwrap());
                t.done();
            }
            assert!(matches!(t.lookup(&[int(0)]).unwrap(), Lookup::Hit(_)), "key 0 is now the LRU tail");
            t.lookup(&[int(2)]).unwrap();
            assert!(t.store(&[col(2)]).unwrap());
            t.done();
            assert!(t.mem_used() <= limit);
            assert_eq!(t.lookup(&[int(1)]).unwrap(), Lookup::Miss { current: true }, "key 1 was the LRU head");
            // a scan far past the limit evicts everything, itself last: bypass
            let mut stored = true;
            for r in 0..64 {
                stored = t.store(&[col(r)]).unwrap();
                if !stored {
                    break;
                }
            }
            assert!(!stored);
            assert!(!t.store(&[col(0)]).unwrap(), "bypass until the next lookup");
            t.done();
            assert_eq!(t.lookup(&[int(1)]).unwrap(), Lookup::Miss { current: true }, "nothing cached for the bypassed scan");
        }
        assert!(t.stats.evictions >= 3);
    }

    #[test]
    fn many_keys_rechain_as_the_buckets_grow_and_evict_in_lru_order() {
        let mut t = MemoTable::new(true, false, 1 << 30, Vec::new()); // alloc: Cold
        unsafe {
            for k in 0..5000u64 {
                assert_eq!(t.lookup(&[int(k * 7919)]).unwrap(), Lookup::Miss { current: true });
                assert!(t.store(&[col(k)]).unwrap());
                t.done();
            }
            for k in (0..5000u64).rev() {
                let Lookup::Hit(at) = t.lookup(&[int(k * 7919)]).unwrap() else { panic!("key {k} hits") };
                assert_eq!(rows(at), vec![vec![(k, 0)]]); // alloc: Cold
            }
            assert_eq!(t.len(), 5000);
            // the LRU is now 4999 .. 0: a limit of 1000 entries evicts 4999 first
            let one = ENTRY_BYTES + MINTUPLE_HEADER + 8 + TUPLE_BYTES + MINTUPLE_HEADER + 8;
            t.limit = 1000 * one;
            assert_eq!(t.lookup(&[int(u64::MAX)]).unwrap(), Lookup::Miss { current: true });
            assert!(t.store(&[col(1)]).unwrap());
            t.done();
            assert_eq!(t.len(), 1000);
            assert_eq!(t.stats.evictions, 4001);
            assert!(matches!(t.lookup(&[int(0)]).unwrap(), Lookup::Hit(_)), "the most recent survives");
            assert_eq!(t.lookup(&[int(4999 * 7919)]).unwrap(), Lookup::Miss { current: true }, "the oldest went");
        }
    }
}
