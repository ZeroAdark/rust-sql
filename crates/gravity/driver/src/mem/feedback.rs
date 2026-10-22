//! Memory step 3a: each breaker slot's charged peak over its recent runs, and the start size admission gives it.
//!
//! One process-wide store of fixed capacity (`HISTORY_KEYS`), keyed per database, program content key, breaker slot and
//! participants (W0 = 1, W8 = 9: a width is measured, never scaled). Each key keeps its last `HISTORY_RUNS` samples within
//! `HISTORY_AGE_S`. `History::start` is the policy: the max of the clean samples under `P90_FROM` of them, their p90 (nearest
//! rank) from then on, raised x2 after a spill and x3 after a refusal, lowered only when the last two clean runs each used
//! under half of what they were given, frozen at the window's largest start after three alternating moves. Counts are KiB.
//! Step 3b keeps the store across restarts: `export` / `import` are its cluster file's body (exec
//! `inputs::knobs::feedback_file`), written at checkpoints and read at the first admission under the switch.

use pgsync::{Mutex, MutexGuard, PoisonError};

pub const HISTORY_RUNS: usize = 32;
pub const HISTORY_AGE_S: u32 = 24 * 60 * 60;
const SHARDS: usize = 16;
const PER_SHARD: usize = 1024;
pub const HISTORY_KEYS: usize = SHARDS * PER_SHARD;
const INDEX: usize = 2 * PER_SHARD;
pub const P90_FROM: usize = 5;

/// A sample's flags; none set is a clean run, the only kind that lowers a start or feeds its percentile.
pub const SPILLED: u8 = 1;
pub const REFUSED: u8 = 2;
/// The run drew past what it was given (its share and its floor's commit room), or failed: it may raise, never lower.
pub const HELD_BACK: u8 = 4;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Key {
    /// The program's shape (`Program::front_key`: layout, sites, output, write plan and pipelines with their estimates),
    /// never its content key: a compiled or interpreted code's identity holds host addresses, which move with every
    /// process (ASLR), and the history outlives the process (3b). Two programs of one shape that differ only in code share
    /// a history (as one literal-free program already serves every literal).
    pub program: u128,
    pub db: u32,
    pub slot: u16,
    pub participants: u16,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sample {
    /// The slot's charged peak (`Node::slot_charged`, its max over the statement).
    pub peak: u32,
    /// What the run needed: the peak, plus what it spilled or the draw it was refused.
    pub need: u32,
    /// The start the run was given.
    pub given: u32,
    /// Wall-clock Unix seconds; 0 is an empty ring entry.
    pub at: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum Source {
    #[default]
    Est,
    Max,
    P90,
    Raised,
    Frozen,
}

impl Source {
    pub const fn name(self) -> &'static str {
        match self {
            Source::Est => "est",
            Source::Max => "max<5",
            Source::P90 => "p90",
            Source::Raised => "raised",
            Source::Frozen => "frozen",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Start {
    pub kib: u32,
    pub source: Source,
}

/// The clean samples' spread in the window (n, min, p50, p90, max), for the PHASES line: a key whose max / min stays past
/// 4x over many runs is a parameter-bucket candidate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Spread {
    pub n: u32,
    pub min: u32,
    pub p50: u32,
    pub p90: u32,
    pub max: u32,
}

/// One key's samples and moves. All-zero is empty, so the store is zero-initialized.
#[derive(Clone, Copy, Debug)]
pub struct History {
    ring: [Sample; HISTORY_RUNS],
    flags: [u8; HISTORY_RUNS],
    next: u8,
    /// The floor a spill or a refusal raised the start to, 0 for none; the first clean run given at least it clears it.
    raised: u32,
    /// The source of the start the latest run was given: a start held by the drop guard keeps it.
    last: Source,
    /// The last three moves of the given start (times; directions alternate, a repeated direction only moves the time).
    moves_at: [u32; 3],
    moves_dir: [i8; 3],
}

/// Nearest rank: the ceil(q n)-th smallest of `v` (sorted in place), q = num / den.
fn rank(v: &mut [u32], num: usize, den: usize) -> u32 {
    v.sort_unstable();
    v[(v.len() * num).div_ceil(den).max(1) - 1]
}

impl History {
    pub const EMPTY: History = History { ring: [Sample { peak: 0, need: 0, given: 0, at: 0 }; HISTORY_RUNS], flags: [0; HISTORY_RUNS], next: 0, raised: 0, last: Source::Est, moves_at: [0; 3], moves_dir: [0; 3] };

    fn fresh(at: u32, now: u32) -> bool {
        at != 0 && now.saturating_sub(at) <= HISTORY_AGE_S
    }

    /// The window's samples, oldest first.
    fn window(&self, now: u32) -> impl DoubleEndedIterator<Item = (Sample, u8)> + '_ {
        (0..HISTORY_RUNS).map(move |i| (self.next as usize + i) % HISTORY_RUNS).filter(move |&i| Self::fresh(self.ring[i].at, now)).map(move |i| (self.ring[i], self.flags[i]))
    }

    fn clean(&self, now: u32, out: &mut [u32; HISTORY_RUNS]) -> usize {
        let mut n = 0;
        for (s, _) in self.window(now).filter(|&(_, f)| f == 0) {
            out[n] = s.peak;
            n += 1;
        }
        n
    }

    pub fn frozen(&self, now: u32) -> bool {
        self.moves_dir[0] != 0 && Self::fresh(self.moves_at[0], now)
    }

    /// The start for the key's next run; `None` with no sample in the window: the estimate's path. A squeezed run (spilled,
    /// refused or held back) raises the start to at least what it needed and never lowers it.
    pub fn start(&self, now: u32) -> Option<Start> {
        let latest = self.window(now).last()?.0;
        if self.frozen(now) {
            return Some(Start { kib: self.window(now).map(|(s, _)| s.given).max().unwrap_or(0), source: Source::Frozen });
        }
        let mut v = [0u32; HISTORY_RUNS];
        let base = match self.clean(now, &mut v) {
            0 => None,
            n if n < P90_FROM => Some(Start { kib: v[..n].iter().copied().max().unwrap_or(0), source: Source::Max }),
            n => Some(Start { kib: rank(&mut v[..n], 9, 10), source: Source::P90 }),
        };
        let base = base.map(|b| if b.kib >= latest.given || self.fell(now) { b } else { Start { kib: latest.given, source: self.last } });
        let squeezed = self.window(now).filter(|&(_, f)| f != 0).map(|(s, _)| s.need).max().unwrap_or(0);
        match (base, self.raised.max(squeezed)) {
            (b, 0) => b,
            (Some(b), r) if b.kib >= r => Some(b),
            (_, r) => Some(Start { kib: r, source: Source::Raised }),
        }
    }

    /// The drop guard: the last two clean runs each used under half of what they were given.
    fn fell(&self, now: u32) -> bool {
        let mut last = self.window(now).filter(|&(_, f)| f == 0).map(|(s, _)| s);
        let (a, b) = (last.next_back(), last.next_back());
        [a, b].into_iter().all(|s| s.is_some_and(|s| (s.peak as u64) * 2 < s.given as u64))
    }

    pub fn spread(&self, now: u32) -> Spread {
        let mut v = [0u32; HISTORY_RUNS];
        match self.clean(now, &mut v) {
            0 => Spread::default(),
            n => {
                let s = &mut v[..n];
                Spread { n: n as u32, p50: rank(s, 1, 2), p90: rank(s, 9, 10), min: s[0], max: s[n - 1] }
            }
        }
    }

    /// A finished run of the key: `s` (its `at` = `now`), its flags, and the source of the start it was given. `measured`:
    /// the key's kind charges what it touched (`measured`), so a later clean run can bring a raise back down; otherwise a
    /// raise is permanent while the key runs (its clean peaks only repeat its start) and it raises to the need alone.
    pub fn record(&mut self, s: Sample, flags: u8, source: Source, now: u32, measured: bool) {
        let prev = self.window(now).last().map(|(p, _)| p.given);
        for i in 0..HISTORY_RUNS {
            if self.ring[i].at != 0 && !Self::fresh(self.ring[i].at, now) {
                (self.ring[i], self.flags[i]) = (Sample::default(), 0);
            }
        }
        let i = self.next as usize;
        (self.ring[i], self.flags[i]) = (Sample { at: now.max(1), ..s }, flags);
        self.next = ((i + 1) % HISTORY_RUNS) as u8;
        self.last = source;
        let up = |k: u32| if measured { s.need.max(s.given).saturating_mul(k) } else { s.need.max(s.given) };
        if flags & REFUSED != 0 {
            self.raised = self.raised.max(up(3));
        } else if flags & SPILLED != 0 {
            self.raised = self.raised.max(up(2));
        } else if flags == 0 && s.given >= self.raised {
            self.raised = 0;
        }
        if let Some(p) = prev.filter(|&p| p != s.given) {
            self.moved(if s.given > p { 1 } else { -1 }, now.max(1));
        }
    }

    fn moved(&mut self, dir: i8, at: u32) {
        if self.moves_dir[2] == dir {
            self.moves_at[2] = at;
        } else {
            self.moves_at.rotate_left(1);
            self.moves_dir.rotate_left(1);
            (self.moves_at[2], self.moves_dir[2]) = (at, dir);
        }
    }
}

const NIL: u16 = 0;

#[derive(Clone, Copy)]
struct Entry {
    key: Key,
    hist: History,
    /// LRU links, entry + 1 (`NIL` = none).
    prev: u16,
    next: u16,
}

/// A shard: `PER_SHARD` entries, an open-addressed index (entry + 1, linear probing, backward-shift deletion) and an exact
/// LRU list (head = most recently used). D4632: `used` is the entries ever filled (each one's pages touched, in order);
/// `free` lists the entries a dropped database's keys gave back (linked by `next`), filled again before `used` grows.
struct Shard {
    used: u16,
    head: u16,
    tail: u16,
    free: u16,
    index: [u16; INDEX],
    entries: [Entry; PER_SHARD],
}

const EMPTY_ENTRY: Entry = Entry { key: Key { program: 0, db: 0, slot: 0, participants: 0 }, hist: History::EMPTY, prev: NIL, next: NIL };

impl Shard {
    const EMPTY: Shard = Shard { used: 0, head: NIL, tail: NIL, free: NIL, index: [NIL; INDEX], entries: [EMPTY_ENTRY; PER_SHARD] };

    fn find(&self, key: &Key, h: u64) -> Result<usize, usize> {
        let mut i = (h as usize >> 4) % INDEX;
        loop {
            match self.index[i] {
                NIL => return Err(i),
                e if self.entries[e as usize - 1].key == *key => return Ok(i),
                _ => i = (i + 1) % INDEX,
            }
        }
    }

    fn unlink(&mut self, e: u16) {
        let (p, n) = (self.entries[e as usize - 1].prev, self.entries[e as usize - 1].next);
        if p == NIL { self.head = n } else { self.entries[p as usize - 1].next = n }
        if n == NIL { self.tail = p } else { self.entries[n as usize - 1].prev = p }
    }

    fn push_front(&mut self, e: u16) {
        (self.entries[e as usize - 1].prev, self.entries[e as usize - 1].next) = (NIL, self.head);
        if self.head != NIL {
            self.entries[self.head as usize - 1].prev = e;
        }
        self.head = e;
        if self.tail == NIL {
            self.tail = e;
        }
    }

    fn touch(&mut self, e: u16) {
        if self.head != e {
            self.unlink(e);
            self.push_front(e);
        }
    }

    /// Backward-shift deletion of index position `i`.
    fn remove_at(&mut self, mut i: usize) {
        let mut j = i;
        loop {
            self.index[i] = NIL;
            loop {
                j = (j + 1) % INDEX;
                if self.index[j] == NIL {
                    return;
                }
                let e = &self.entries[self.index[j] as usize - 1];
                let home = (hash(&e.key) as usize >> 4) % INDEX;
                // `j`'s entry may move to `i` unless its home lies cyclically in (i, j]
                let stays = if i <= j { i < home && home <= j } else { i < home || home <= j };
                if !stays {
                    break;
                }
            }
            self.index[i] = self.index[j];
            i = j;
        }
    }

    /// The entry for `key`, made (the least recently used one evicted when the shard is full) when absent.
    fn entry(&mut self, key: &Key, h: u64) -> &mut Entry {
        let e = match self.find(key, h) {
            Ok(i) => self.index[i],
            Err(_) if self.free != NIL => {
                let e = self.free;
                self.free = self.entries[e as usize - 1].next;
                self.fill(key, h, e)
            }
            Err(_) if (self.used as usize) < PER_SHARD => {
                self.used += 1;
                self.fill(key, h, self.used)
            }
            Err(_) => {
                let e = self.tail;
                let old = self.entries[e as usize - 1].key;
                if let Ok(i) = self.find(&old, hash(&old)) {
                    self.remove_at(i);
                }
                self.unlink(e);
                self.fill(key, h, e)
            }
        };
        self.touch(e);
        &mut self.entries[e as usize - 1]
    }

    /// Entry `e` (unlinked) made `key`'s, indexed and most recently used.
    fn fill(&mut self, key: &Key, h: u64, e: u16) -> u16 {
        if let Err(i) = self.find(key, h) {
            self.index[i] = e;
        }
        self.entries[e as usize - 1] = Entry { key: *key, ..EMPTY_ENTRY };
        self.push_front(e);
        e
    }

    /// D4632: every key of database `db` dropped, its entry on the free list (DROP DATABASE: its programs never run again).
    fn forget_db(&mut self, db: u32) -> usize {
        let (mut e, mut n) = (self.head, 0);
        // one walk of the shard's LRU list: at most PER_SHARD entries
        while e != NIL {
            let (key, next) = (self.entries[e as usize - 1].key, self.entries[e as usize - 1].next);
            if key.db == db {
                if let Ok(i) = self.find(&key, hash(&key)) {
                    self.remove_at(i);
                }
                self.unlink(e);
                self.entries[e as usize - 1] = Entry { next: self.free, ..EMPTY_ENTRY };
                self.free = e;
                n += 1;
            }
            e = next;
        }
        n
    }

    fn get(&mut self, key: &Key, h: u64) -> Option<&History> {
        let e = self.index[self.find(key, h).ok()?];
        self.touch(e);
        Some(&self.entries[e as usize - 1].hist)
    }
}

fn hash(k: &Key) -> u64 {
    let mut h = gravity_abi::hash::Mix128::new();
    for w in [k.program as u64, (k.program >> 64) as u64, (k.db as u64) << 32 | (k.slot as u64) << 16 | k.participants as u64] {
        h.word(w);
    }
    h.key128() as u64
}

static STORE: [Mutex<Shard>; SHARDS] = [const { Mutex::new(Shard::EMPTY) }; SHARDS];

/// D4632: `STORE` is a static (the process's zero pages), resident as its shards' entries fill: each shard's pages touched so
/// far (its fixed fields and index once used, then `used` entries in order), and their bytes on the program caches' holder
/// as mapped pages (`RESIDENT`). The pages stay resident when entries are freed and filled again, so the charge only grows,
/// to the store's size at most.
static PAGED: [core::sync::atomic::AtomicU32; SHARDS] = [const { core::sync::atomic::AtomicU32::new(0) }; SHARDS];
static RESIDENT: Mutex<memledger::Charge> = Mutex::new(memledger::Charge::NONE);
const PAGE: usize = 4096;

/// D4632: shard `i` (its lock held) has `used` entries filled: the pages that adds to its high water charged.
fn touched(i: usize, used: u16) {
    use core::sync::atomic::Ordering::Relaxed;
    if used == 0 {
        return;
    }
    let fixed = core::mem::size_of::<Shard>() - core::mem::size_of::<[Entry; PER_SHARD]>();
    let pages = (fixed + used as usize * core::mem::size_of::<Entry>()).div_ceil(PAGE) as u32;
    let had = PAGED[i].load(Relaxed);
    if pages <= had {
        return;
    }
    PAGED[i].store(pages, Relaxed);
    const OWNER: memledger::Owner = memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::ProgramCaches));
    let n = ((pages - had) as usize * PAGE) as u64;
    let mut c = RESIDENT.lock().unwrap_or_else(PoisonError::into_inner);
    if c.owner() != OWNER {
        *c = memledger::Charge::open(OWNER, memledger::Release::Evict, memledger::Kind::Mapped, memledger::charge::Door::Mapped, n);
    } else {
        c.commit(n);
    }
}

/// D4632: every key of database `db` forgotten (exec's DROP DATABASE hook): its entries filled again by other keys first.
pub fn forget_db(db: u32) {
    for s in STORE.iter() {
        s.lock().unwrap_or_else(PoisonError::into_inner).forget_db(db);
    }
}

/// D4632: the bytes `RESIDENT` charges (tests).
pub fn resident_charged() -> u64 {
    RESIDENT.lock().unwrap_or_else(PoisonError::into_inner).committed()
}

fn shard(h: u64) -> MutexGuard<'static, Shard> {
    shard_in(&STORE, h)
}

fn shard_in(store: &[Mutex<Shard>], h: u64) -> MutexGuard<'_, Shard> {
    store[h as usize % SHARDS].lock().unwrap_or_else(PoisonError::into_inner)
}

/// The wall clock in Unix seconds as the store keeps it.
pub fn now() -> u32 {
    pg_clock::wall_secs().clamp(1, u32::MAX as i64) as u32
}

/// `key`'s start at `now` (a read counts as a use for the LRU).
pub fn start_of(key: &Key, now: u32) -> Option<Start> {
    let h = hash(key);
    shard(h).get(key, h).and_then(|x| x.start(now))
}

pub fn spread_of(key: &Key, now: u32) -> Spread {
    let h = hash(key);
    shard(h).get(key, h).map_or_else(Spread::default, |x| x.spread(now))
}

pub fn record(key: &Key, s: Sample, flags: u8, source: Source, now: u32, measured: bool) {
    let h = hash(key);
    let mut g = shard(h);
    g.entry(key, h).hist.record(s, flags, source, now, measured);
    touched(h as usize % SHARDS, g.used);
}

/// Whether a breaker of `kind` charges what it touched (memory step 2's commit protocol), so its charged peak measures its
/// need and a start may come back down. None yet: a share region is charged its whole length, so a clean run's peak is
/// the start it was given. 2c-host H1 adds the kind-7 builds, K1 and N1 the hash aggregates and DISTINCTs.
pub const fn measured(kind: gravity_abi::breaker::BreakerKind) -> bool {
    let _ = kind;
    false
}

/// Memory step 3b: one key and its history as the cluster file keeps them, little-endian: the key (program 16, database
/// 4, slot 2, participants 2), each sample (peak, need, given, at: 4 each) and its flags, then the ring's next, the raise,
/// the last source and the three moves (time 4, direction 1).
pub const ENTRY_BYTES: usize = 24 + HISTORY_RUNS * 17 + 1 + 4 + 1 + 3 * 5;

fn encode(e: &Entry, out: &mut Vec<u8>) {
    let (k, h) = (&e.key, &e.hist);
    out.extend_from_slice(&k.program.to_le_bytes());
    out.extend_from_slice(&k.db.to_le_bytes());
    out.extend_from_slice(&k.slot.to_le_bytes());
    out.extend_from_slice(&k.participants.to_le_bytes());
    for (s, f) in h.ring.iter().zip(h.flags) {
        for w in [s.peak, s.need, s.given, s.at] {
            out.extend_from_slice(&w.to_le_bytes());
        }
        out.push(f);
    }
    out.push(h.next);
    out.extend_from_slice(&h.raised.to_le_bytes());
    out.push(h.last as u8);
    for i in 0..3 {
        out.extend_from_slice(&h.moves_at[i].to_le_bytes());
        out.push(h.moves_dir[i] as u8);
    }
}

/// `encode`'s bytes back (`b` is `ENTRY_BYTES` long): the key, and its history with every sample outside the window at `now`
/// dropped; `None` when no sample is left, `Err` when a field is out of range.
fn decode(b: &[u8], now: u32) -> Result<Option<(Key, History)>, ()> {
    let mut at = 0;
    let mut take = |n: usize| {
        at += n;
        &b[at - n..at]
    };
    let w32 = |x: &[u8]| u32::from_le_bytes([x[0], x[1], x[2], x[3]]);
    let w16 = |x: &[u8]| u16::from_le_bytes([x[0], x[1]]);
    let program = u128::from_le_bytes(take(16).try_into().map_err(|_| ())?);
    let key = Key { program, db: w32(take(4)), slot: w16(take(2)), participants: w16(take(2)) };
    let mut h = History::EMPTY;
    for i in 0..HISTORY_RUNS {
        let s = Sample { peak: w32(take(4)), need: w32(take(4)), given: w32(take(4)), at: w32(take(4)) };
        let f = take(1)[0];
        if f & !(SPILLED | REFUSED | HELD_BACK) != 0 {
            return Err(());
        }
        if History::fresh(s.at, now) {
            (h.ring[i], h.flags[i]) = (s, f);
        }
    }
    h.next = take(1)[0];
    h.raised = w32(take(4));
    h.last = match take(1)[0] {
        0 => Source::Est,
        1 => Source::Max,
        2 => Source::P90,
        3 => Source::Raised,
        4 => Source::Frozen,
        _ => return Err(()),
    };
    for i in 0..3 {
        h.moves_at[i] = w32(take(4));
        h.moves_dir[i] = take(1)[0] as i8;
    }
    if (h.next as usize) >= HISTORY_RUNS || h.moves_dir.iter().any(|d| !(-1..=1).contains(d)) {
        return Err(());
    }
    let kept = h.window(now).next().is_some();
    Ok(kept.then_some((key, h)))
}

/// Memory step 3b: every key with a sample in the window at `now` appended to `out` (`ENTRY_BYTES` each), each shard's
/// least recently used first, so a load past capacity keeps the most recent; the count.
pub fn export(out: &mut Vec<u8>, now: u32) -> u32 {
    export_from(&STORE, out, now)
}

fn export_from(store: &[Mutex<Shard>], out: &mut Vec<u8>, now: u32) -> u32 {
    let mut n = 0;
    for s in store.iter() {
        let s = s.lock().unwrap_or_else(PoisonError::into_inner);
        let mut e = s.tail;
        while e != NIL {
            let x = &s.entries[e as usize - 1];
            if x.hist.window(now).next().is_some() {
                encode(x, out);
                n += 1;
            }
            e = x.prev;
        }
    }
    n
}

/// Memory step 3b: `export`'s entries back into the store, each sample re-checked against the window at `now`; the count
/// kept. `Err` when `body` is not whole entries or an entry is out of range: nothing is kept then.
pub fn import(body: &[u8], now: u32) -> Result<u32, ()> {
    let kept = import_into(&STORE, body, now);
    for (i, s) in STORE.iter().enumerate() {
        let g = s.lock().unwrap_or_else(PoisonError::into_inner);
        touched(i, g.used);
    }
    kept
}

fn import_into(store: &[Mutex<Shard>], body: &[u8], now: u32) -> Result<u32, ()> {
    if body.len() % ENTRY_BYTES != 0 {
        return Err(());
    }
    for b in body.chunks_exact(ENTRY_BYTES) {
        decode(b, now)?;
    }
    let mut kept = 0;
    for b in body.chunks_exact(ENTRY_BYTES) {
        if let Some((key, hist)) = decode(b, now)? {
            let h = hash(&key);
            shard_in(store, h).entry(&key, h).hist = hist;
            kept += 1;
        }
    }
    Ok(kept)
}

/// Forget every key (tests).
pub fn forget() {
    for s in STORE.iter() {
        *s.lock().unwrap_or_else(PoisonError::into_inner) = Shard::EMPTY;
    }
}

/// The breaker slots a statement's starts cover; a program's slots past them take the estimate's path.
pub const SLOTS: usize = 32;
/// The statements under feedback one thread holds at once (open cursors beside the running statement); past it a
/// statement takes the estimate's path.
const DEPTH: usize = 4;
/// A key's slot bit for the workers' share (the leader's share is the slot itself).
const WORKER: u16 = 0x8000;

/// A statement under feedback, on the thread that admitted it: the starts its slots were given and what its run did.
#[derive(Clone, Copy, Debug)]
pub struct Live {
    pub id: u64,
    /// The program's content key: what the driver's calls (`raise`, `observe`, `finish`) name the statement by.
    pub program: u128,
    /// Its shape, the history's key (`Key::program`).
    pub shape: u128,
    pub width: u16,
    pub db: u32,
    /// Per slot, [leader, worker]: the start given (KiB; 0 = the estimate's path), its source, the charged peak (KiB).
    pub start: [[u32; 2]; SLOTS],
    pub source: [[Source; 2]; SLOTS],
    pub peak: [[u32; 2]; SLOTS],
    /// Per slot, the flags its run earned (`REFUSED` as it runs, the rest at `finish`).
    pub flags: [u8; SLOTS],
    /// The breaker slots that take starts, those whose kind is `measured`, and those whose run ended (a bit per slot).
    pub slots: u32,
    pub measured: u32,
    pub seen: u32,
    pub hits: u16,
    pub misses: u16,
    /// This thread's refusal count at the last observation.
    refusals: u64,
    /// The refused draw (bytes) a slot's need adds.
    refused: [u64; SLOTS],
}

thread_local! {
    static LIVE: core::cell::RefCell<[Option<Live>; DEPTH]> = const { core::cell::RefCell::new([None; DEPTH]) };
    static REFUSALS: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
}

static NEXT_ID: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);

/// W0 = 1, W8 = 9: the leader and the workers (a width under 2 runs serially).
pub const fn participants(width: u16) -> u16 {
    if width >= 2 { width + 1 } else { 1 }
}

fn key_of(l: &Live, slot: u16) -> Key {
    Key { program: l.shape, db: l.db, slot, participants: participants(l.width) }
}

fn kib(bytes: u64) -> u32 {
    bytes.div_ceil(1024).min(u32::MAX as u64) as u32
}

/// Admission (exec): `program` the content key the driver's calls name it by, `shape` its history's key (`Key::program`);
/// `slots` = `program`'s share breakers (slot, parallel, `measured`) at `width`, each role's start clamped to at least
/// `FLOOR_CHARGED_MIN` and at most `cap` (the statement's fair share of the big lane), held for the statement until `end`.
/// Returns its id, and whether a leader share had no history (the no-history arm's first run); `None` past `DEPTH`.
pub fn begin(program: u128, shape: u128, width: u16, db: u32, slots: impl Iterator<Item = (u8, bool, bool)>, cap: u64) -> Option<(u64, bool)> {
    let now = now();
    let mut l = Live { id: NEXT_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed), program, shape, width, db, start: [[0; 2]; SLOTS], source: [[Source::Est; 2]; SLOTS], peak: [[0; 2]; SLOTS], flags: [0; SLOTS], slots: 0, measured: 0, seen: 0, hits: 0, misses: 0, refusals: REFUSALS.with(core::cell::Cell::get), refused: [0; SLOTS] };
    let (min, cap) = (kib(crate::mem::grant::FLOOR_CHARGED_MIN), kib(cap));
    let mut first = false;
    for (slot, parallel, measured) in slots.filter(|&(s, _, _)| (s as usize) < SLOTS) {
        if l.slots & (1 << slot) != 0 {
            continue;
        }
        l.slots |= 1 << slot;
        l.measured |= (measured as u32) << slot;
        for role in 0..1 + parallel as usize {
            match start_of(&key_of(&l, slot as u16 | if role == 1 { WORKER } else { 0 }), now) {
                Some(s) => {
                    // A measured slot (one that starts small) takes its learned peak, however small; the rest at
                    // least `FLOOR_CHARGED_MIN` as before
                    (l.start[slot as usize][role], l.source[slot as usize][role]) = (s.kib.min(cap).max(if measured { 0 } else { min }), s.source);
                    l.hits += 1;
                }
                None => {
                    first |= role == 0;
                    l.misses += 1;
                }
            }
        }
    }
    LIVE.with(|v| {
        let mut v = v.borrow_mut();
        let free = v.iter_mut().find(|x| x.is_none())?;
        *free = Some(l);
        Some((l.id, first))
    })
}

/// The statement's end (exec, its lease dropped): its entry, for the report.
pub fn end(id: u64) -> Option<Live> {
    LIVE.with(|v| v.borrow_mut().iter_mut().find(|x| x.as_ref().is_some_and(|l| l.id == id))?.take())
}

/// The innermost live statement running `program`.
fn with_live<R>(program: u128, f: impl FnOnce(&mut Live) -> R) -> Option<R> {
    LIVE.with(|v| v.borrow_mut().iter_mut().flatten().filter(|l| l.program == program).max_by_key(|l| l.id).map(f))
}

/// The one divisor's floors for breaker slot `slot` of `program` (`grant::share_of`): the leader's `floor` and each
/// worker's `worker_floor`, raised to the starts its statement was given. Raise only: lowering waits for the kinds on the
/// commit protocol, where a lower ask no longer lays a smaller table.
pub fn raise(program: u128, slot: u8, floor: u64, worker_floor: u64) -> (u64, u64) {
    if (slot as usize) >= SLOTS {
        return (floor, worker_floor);
    }
    with_live(program, |l| {
        let s = l.start[slot as usize];
        (floor.max((s[0] as u64) << 10), worker_floor.max((s[1] as u64) << 10))
    })
    .unwrap_or((floor, worker_floor))
}

/// A draw past G refused on this thread (region.rs `refused`).
pub fn note_refusal() {
    REFUSALS.with(|c| c.set(c.get() + 1));
}

/// Slot `slot` of `program` filled (the Node, after its producer's pipeline, and at the statement's end for the slot still
/// open): the leader's and the widest worker's charged bytes now; a refusal since the last observation is the slot's,
/// with the draw refused (`refused`, bytes).
pub fn observe(program: u128, slot: u8, leader: u64, worker: u64, refused: u64) {
    if (slot as usize) >= SLOTS {
        return;
    }
    let n = REFUSALS.with(core::cell::Cell::get);
    with_live(program, |l| {
        let k = slot as usize;
        if l.slots & (1 << k) == 0 {
            return;
        }
        l.peak[k] = [l.peak[k][0].max(kib(leader)), l.peak[k][1].max(kib(worker))];
        l.seen |= 1 << k;
        if n > l.refusals {
            (l.refusals, l.flags[k], l.refused[k]) = (n, l.flags[k] | REFUSED, l.refused[k].max(refused));
        }
    });
}

/// Whether `program` runs under feedback on this thread (the Node's observations cost nothing otherwise).
pub fn live(program: u128) -> bool {
    with_live(program, |_| ()).is_some()
}

/// The statement's end (driver, before its regions settle): a sample per observed slot and role. `ok` false (an error)
/// holds every sample back; `spilled(slot)` = the slot's spilled bytes; `given(slot, width)` = ([leader, worker] given,
/// [leader, worker] room) bytes: a peak past its room drew past what it was given, held back (an allocator region's
/// charged bytes are its whole length, so a peak at its room is its layout, not a squeeze). The flags each slot earned
/// stay on the entry (the report).
pub fn finish(program: u128, ok: bool, spilled: &dyn Fn(u8) -> u64, given: &dyn Fn(u8, u16) -> ([u64; 2], [u64; 2])) {
    let now = now();
    // copied out: `given` reads the one divisor, which reads this statement's starts (`raise`)
    let Some(l) = with_live(program, |l| core::mem::replace(l, Live { seen: 0, ..*l })).filter(|l| l.seen != 0) else { return };
    let mut flags = l.flags;
    for k in (0..SLOTS).filter(|&k| l.seen & (1 << k) != 0) {
        let (g, room) = given(k as u8, l.width);
        let spill = spilled(k as u8);
        flags[k] |= (if spill > 0 { SPILLED } else { 0 }) | (if ok { 0 } else { HELD_BACK });
        for role in (0..2).filter(|&r| r == 0 || g[1] > 0) {
            let peak = l.peak[k][role];
            let f = flags[k] | (if room[role] > 0 && peak as u64 > room[role].div_ceil(1024) { HELD_BACK } else { 0 });
            let extra = if role == 0 { spill + l.refused[k] } else { 0 };
            let s = Sample { peak, need: kib(((peak as u64) << 10) + extra), given: kib(g[role]), at: now };
            let source = if l.start[k][role] != 0 { l.source[k][role] } else { Source::Est };
            record(&key_of(&l, k as u16 | if role == 1 { WORKER } else { 0 }), s, f, source, now, l.measured & (1 << k) != 0);
            flags[k] |= f & HELD_BACK;
        }
    }
    with_live(program, |x| x.flags = flags);
}

/// Slot `slot`'s spread (the report), per role.
pub fn spreads(l: &Live, slot: u8) -> [Spread; 2] {
    let now = now();
    [spread_of(&key_of(l, slot as u16), now), spread_of(&key_of(l, slot as u16 | WORKER), now)]
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u32 = 1_000_000;

    fn run(h: &mut History, peak: u32, given: u32, flags: u8, at: u32) {
        let src = h.start(at).map_or(Source::Est, |s| s.source);
        h.record(Sample { peak, need: peak, given, at }, flags, src, at, true);
    }

    #[test]
    fn under_five_clean_runs_the_start_is_their_max_and_from_five_their_p90() {
        let mut h = History::EMPTY;
        assert_eq!(h.start(T0), None, "no history: the estimate's path");
        for (i, p) in [100, 300, 200].into_iter().enumerate() {
            run(&mut h, p, 100, 0, T0 + i as u32);
        }
        assert_eq!(h.start(T0 + 9), Some(Start { kib: 300, source: Source::Max }));
        let mut h = History::EMPTY;
        for (i, p) in (1..=10).map(|k| k * 100).enumerate() {
            run(&mut h, p, 100, 0, T0 + i as u32);
        }
        assert_eq!(h.start(T0 + 99), Some(Start { kib: 900, source: Source::P90 }), "the 9th of 10");
        let mut h = History::EMPTY;
        for (i, p) in (1..=32).map(|k| k * 10).enumerate() {
            run(&mut h, p, 10, 0, T0 + i as u32);
        }
        assert_eq!(h.start(T0 + 99), Some(Start { kib: 290, source: Source::P90 }), "ceil(0.9 x 32) = 29th");
    }

    #[test]
    fn p90_at_five_is_the_largest() {
        let mut h = History::EMPTY;
        for (i, p) in [10, 50, 20, 40, 30].into_iter().enumerate() {
            run(&mut h, p, 10, 0, T0 + i as u32);
        }
        assert_eq!(h.start(T0 + 9), Some(Start { kib: 50, source: Source::P90 }), "ceil(4.5) = 5th of 5");
    }

    #[test]
    fn held_back_runs_never_feed_the_percentile_or_a_drop() {
        let mut h = History::EMPTY;
        run(&mut h, 400, 400, HELD_BACK, T0);
        assert_eq!(h.start(T0 + 1), Some(Start { kib: 400, source: Source::Raised }), "a squeezed run: at least what it needed");
        run(&mut h, 100, 400, 0, T0 + 1);
        run(&mut h, 50, 400, HELD_BACK, T0 + 2);
        run(&mut h, 60, 400, HELD_BACK, T0 + 3);
        assert_eq!(h.start(T0 + 4).map(|s| s.kib), Some(400), "one clean run under half: no drop");
    }

    #[test]
    fn samples_age_out_after_the_window() {
        let mut h = History::EMPTY;
        run(&mut h, 500, 100, 0, T0);
        assert_eq!(h.start(T0 + HISTORY_AGE_S).map(|s| s.kib), Some(500));
        assert_eq!(h.start(T0 + HISTORY_AGE_S + 1), None, "aged out");
        run(&mut h, 200, 100, 0, T0 + HISTORY_AGE_S + 1);
        assert_eq!(h.window(T0 + HISTORY_AGE_S + 1).count(), 1, "the aged sample dropped at the write");
    }

    #[test]
    fn a_spill_raises_twice_and_a_refusal_three_times_what_it_needed() {
        let mut h = History::EMPTY;
        h.record(Sample { peak: 100, need: 300, given: 100, at: T0 }, SPILLED, Source::Est, T0, true);
        assert_eq!(h.start(T0 + 1), Some(Start { kib: 600, source: Source::Raised }));
        let mut h = History::EMPTY;
        h.record(Sample { peak: 100, need: 150, given: 200, at: T0 }, REFUSED, Source::Est, T0, true);
        assert_eq!(h.start(T0 + 1), Some(Start { kib: 600, source: Source::Raised }), "max(need, given) x 3");
        let mut h = History::EMPTY;
        h.record(Sample { peak: 100, need: 300, given: 100, at: T0 }, SPILLED | REFUSED, Source::Est, T0, false);
        assert_eq!(h.start(T0 + 1), Some(Start { kib: 300, source: Source::Raised }), "a kind that cannot come back down: the need alone");
    }

    #[test]
    fn the_raise_stays_until_a_clean_run_given_at_least_it() {
        let mut h = History::EMPTY;
        h.record(Sample { peak: 100, need: 300, given: 100, at: T0 }, SPILLED, Source::Est, T0, true);
        run(&mut h, 320, 400, 0, T0 + 1);
        assert_eq!(h.start(T0 + 2), Some(Start { kib: 600, source: Source::Raised }), "given 400 < 600: kept");
        run(&mut h, 330, 600, 0, T0 + 2);
        assert_eq!(h.raised, 0, "cleared");
        assert_eq!(h.start(T0 + 3), Some(Start { kib: 600, source: Source::Raised }), "held by the drop guard: the last run used over half");
    }

    #[test]
    fn the_start_drops_only_after_two_clean_runs_under_half() {
        let mut h = History::EMPTY;
        run(&mut h, 40, 1000, 0, T0);
        assert_eq!(h.start(T0 + 1), Some(Start { kib: 1000, source: Source::Est }), "run 2 keeps the estimate's start");
        run(&mut h, 60, 1000, 0, T0 + 1);
        assert_eq!(h.start(T0 + 2), Some(Start { kib: 60, source: Source::Max }), "run 3 drops to the max");
        let mut h = History::EMPTY;
        run(&mut h, 40, 1000, 0, T0);
        run(&mut h, 600, 1000, 0, T0 + 1);
        assert_eq!(h.start(T0 + 2).map(|s| s.kib), Some(1000), "the last run used more than half: held");
    }

    #[test]
    fn three_alternating_moves_freeze_the_key_at_its_largest_start_until_they_age_out() {
        let mut h = History::EMPTY;
        for (i, g) in [100, 400, 100, 400].into_iter().enumerate() {
            run(&mut h, 50, g, 0, T0 + i as u32);
        }
        assert!(h.frozen(T0 + 4));
        assert_eq!(h.start(T0 + 4), Some(Start { kib: 400, source: Source::Frozen }));
        let mut h = History::EMPTY;
        for (i, g) in [100, 200, 300, 400].into_iter().enumerate() {
            run(&mut h, 50, g, 0, T0 + i as u32);
        }
        assert!(!h.frozen(T0 + 4), "one direction: no flip");
        let mut h = History::EMPTY;
        for (i, g) in [100, 400, 100, 400].into_iter().enumerate() {
            run(&mut h, 50, g, 0, T0 + i as u32 * 1000);
        }
        assert!(!h.frozen(T0 + 1000 + HISTORY_AGE_S + 1), "the first flip aged out");
    }

    #[test]
    fn a_full_shard_evicts_its_least_recently_used_key() {
        let mut s = Box::new(Shard::EMPTY); // alloc: Cold
        let key = |i: u32| Key { program: 7, db: 1, slot: (i % 60_000) as u16, participants: (i / 60_000) as u16 + 1 };
        let rec = |s: &mut Shard, k: &Key| s.entry(k, hash(k)).hist.record(Sample { peak: 1, need: 1, given: 1, at: T0 }, 0, Source::Est, T0, true);
        for i in 0..PER_SHARD as u32 {
            rec(&mut *s, &key(i));
        }
        assert!(s.get(&key(0), hash(&key(0))).is_some(), "key 0 used again: now the most recent");
        rec(&mut *s, &key(PER_SHARD as u32));
        assert!(s.get(&key(1), hash(&key(1))).is_none(), "key 1 was the least recently used");
        for i in (0..PER_SHARD as u32 + 1).filter(|&i| i != 1) {
            assert!(s.get(&key(i), hash(&key(i))).is_some(), "key {i} kept");
        }
        assert_eq!(s.used as usize, PER_SHARD);
    }

    /// D4632: a dropped database's keys leave the shard and their entries are filled again before `used` grows (a fresh
    /// entry touches the next pages of the static store), the other database's keys kept.
    #[test]
    fn a_dropped_databases_keys_leave_and_their_entries_are_filled_first() {
        let mut s = Box::new(Shard::EMPTY); // alloc: Cold
        let key = |db: u32, i: u32| Key { program: 9, db, slot: i as u16, participants: 1 };
        let rec = |s: &mut Shard, k: &Key| s.entry(k, hash(k)).hist.record(Sample { peak: 1, need: 1, given: 1, at: T0 }, 0, Source::Est, T0, true);
        for i in 0..40 {
            rec(&mut *s, &key(5, i));
            rec(&mut *s, &key(6, i));
        }
        assert_eq!(s.used, 80);
        assert_eq!(s.forget_db(5), 40, "database 5's keys");
        assert!((0..40).all(|i| s.get(&key(5, i), hash(&key(5, i))).is_none()), "gone");
        assert!((0..40).all(|i| s.get(&key(6, i), hash(&key(6, i))).is_some()), "database 6's kept, still found by the index");
        for i in 0..40 {
            rec(&mut *s, &key(7, i));
        }
        assert_eq!(s.used, 80, "database 7's keys took the freed entries: no new entry touched");
        rec(&mut *s, &key(7, 40));
        assert_eq!(s.used, 81, "past the free list: the next entry");
        assert!((0..41).all(|i| s.get(&key(7, i), hash(&key(7, i))).is_some()) && (0..40).all(|i| s.get(&key(6, i), hash(&key(6, i))).is_some()));
    }

    /// D4632: the store's touched pages are on the program caches' holder as mapped bytes, whole pages.
    #[test]
    fn the_stores_touched_pages_are_charged_whole() {
        record(&Key { program: 0x4632, db: 4632, slot: 1, participants: 1 }, Sample { peak: 1, need: 1, given: 1, at: T0 }, 0, Source::Est, T0, true);
        let c = resident_charged();
        assert!(c > 0 && c % PAGE as u64 == 0, "whole pages charged: {c}");
        forget_db(4632);
        assert!(resident_charged() >= c, "the pages stay resident: the charge never shrinks");
    }

    #[test]
    fn the_store_is_bounded() {
        assert!(core::mem::size_of::<Entry>() <= 640, "{}", core::mem::size_of::<Entry>());
        assert!(core::mem::size_of_val(&STORE) <= 11 << 20, "{}", core::mem::size_of_val(&STORE));
    }

    /// A statement's run is recorded at its end and the next statement of the program starts from it; a slot that never ran
    /// keeps the estimate's path (the no-history arm); a refusal is the observed slot's and raises it three times.
    #[test]
    fn a_statement_records_its_slots_and_the_next_one_starts_from_them() {
        const MIB: u64 = 1 << 20;
        let p = 0x3a3a_u128 << 64 | 1;
        let slots = || [(0u8, false, true), (1, false, true), (0, true, true)].into_iter();
        let (id, first) = begin(p, p ^ 0x5a, 0, 7, slots(), u64::MAX).unwrap();
        assert!(first, "no history");
        assert_eq!(raise(p, 0, MIB, MIB), (MIB, MIB), "the estimate's floors");
        observe(p, 0, 40 * MIB, 0, 0);
        finish(p, true, &|_| 0, &|_, _| ([8 * MIB, 0], [8 * MIB, 0]));
        let l = end(id).unwrap();
        assert_eq!((l.peak[0][0], l.flags[0], l.seen), (40 << 10, HELD_BACK, 0), "a peak past its room is held back");
        let (id, first) = begin(p, p ^ 0x5a, 0, 7, slots(), u64::MAX).unwrap();
        assert!(first, "slot 1 never ran");
        assert_eq!(raise(p, 0, MIB, MIB).0, 40 * MIB, "slot 0 starts at what it needed");
        note_refusal();
        observe(p, 0, 50 * MIB, 0, 4 * MIB);
        finish(p, true, &|_| 0, &|_, _| ([40 * MIB, 0], [40 * MIB, 0]));
        assert_eq!(end(id).unwrap().flags[0], REFUSED | HELD_BACK);
        let (id, _) = begin(p, p ^ 0x5a, 0, 7, slots(), u64::MAX).unwrap();
        assert_eq!(raise(p, 0, MIB, MIB).0, 3 * 54 * MIB, "x3 of max(need, given) after a refusal");
        assert_eq!(end(id).map(|l| (l.hits, l.misses)), Some((1, 1)));
        assert!(end(id).is_none() && !live(p), "ended once");
        let (id, _) = begin(p, p ^ 0x5a, 0, 7, slots(), 12 * MIB).unwrap();
        assert_eq!(raise(p, 0, MIB, MIB).0, 12 * MIB, "clamped to the fair share");
        end(id);
        // the same shape under another content key (a restarted process: its code's host addresses moved): its history
        let q = p ^ 0xffff;
        let (id, first) = begin(q, p ^ 0x5a, 0, 7, slots(), u64::MAX).unwrap();
        assert!(first, "slot 1 still never ran");
        assert_eq!(raise(q, 0, MIB, MIB).0, 3 * 54 * MIB, "the shape's history");
        end(id);
    }

    /// 3b: a key's history goes out and comes back as it was, less the samples the window dropped meanwhile; a malformed
    /// body keeps nothing. On a store of its own (the process-wide one is shared with the other tests).
    #[test]
    fn an_exported_history_comes_back_as_it_was() {
        // a shard is ~600 KiB, built on the stack before it moves into the Vec
        std::thread::Builder::new().stack_size(64 << 20).spawn(round_trip).unwrap().join().unwrap();
    }

    fn round_trip() {
        let fresh = || (0..SHARDS).map(|_| Mutex::new(Shard::EMPTY)).collect::<Vec<_>>();
        let store = fresh();
        let key = Key { program: 0x3b3b_u128 << 64 | 7, db: 5, slot: 3 | WORKER, participants: 9 };
        let h = hash(&key);
        let start = |st: &[Mutex<Shard>], now| shard_in(st, h).get(&key, h).and_then(|x| x.start(now));
        let spread = |st: &[Mutex<Shard>], now| shard_in(st, h).get(&key, h).map_or_else(Spread::default, |x| x.spread(now));
        for (i, g) in [100u32, 400, 100, 400].into_iter().enumerate() {
            let at = T0 + i as u32 * 1000;
            let src = start(&store, at).map_or(Source::Est, |x| x.source);
            shard_in(&store, h).entry(&key, h).hist.record(Sample { peak: 50 + i as u32, need: 60, given: g, at }, if i == 1 { SPILLED } else { 0 }, src, at, true);
        }
        let now = T0 + 3001;
        let before = (start(&store, now), spread(&store, now));
        assert_eq!(before.0.map(|s| s.source), Some(Source::Frozen));
        let mut out = Vec::new(); // alloc: Cold
        assert_eq!(export_from(&store, &mut out, now), 1);
        assert_eq!(out.len(), ENTRY_BYTES);
        let back = fresh();
        assert_eq!(import_into(&back, &out, now), Ok(1));
        assert_eq!((start(&back, now), spread(&back, now)), before, "the same start (frozen) and spread");
        let later = T0 + 1000 + HISTORY_AGE_S + 1;
        let back = fresh();
        assert_eq!(import_into(&back, &out, later), Ok(1), "the first two samples aged out, two kept");
        assert_eq!(spread(&back, later).n, 2, "both clean");
        assert_eq!(import_into(&fresh(), &out, T0 + 3000 + HISTORY_AGE_S + 1), Ok(0), "all aged out");
        assert_eq!(import_into(&fresh(), &out[1..], now), Err(()), "not whole entries");
        let mut bad = out.clone();
        bad[24 + HISTORY_RUNS * 17 + 5] = 9;
        assert_eq!(import_into(&fresh(), &bad, now), Err(()), "a source out of range");
    }

    #[test]
    fn the_spread_names_the_clean_samples() {
        let mut h = History::EMPTY;
        for (i, p) in [10, 20, 30, 40, 50, 60].into_iter().enumerate() {
            run(&mut h, p, 10, 0, T0 + i as u32);
        }
        run(&mut h, 999, 10, SPILLED, T0 + 7);
        assert_eq!(h.spread(T0 + 8), Spread { n: 6, min: 10, p50: 30, p90: 60, max: 60 });
    }
}
