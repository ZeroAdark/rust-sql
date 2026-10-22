//! P2 W1 (`PGRUST_GRAVITY_PARK_INDEX`): the index over a park's slots, so a take, a put and an eviction find their slot
//! without a walk over every slot, and a release record passes the parked Nodes it cannot reach (lanes/p2design §4 W1).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use gravity_abi::hash::{Depth, KeyHash};
use gravity_program::cost::runtime::{PARK_INDEX_FROM, PARK_RELS_READ};
use gravity_program::PlanKey;

use super::{Entry, Park};

/// P2 W1 `PGRUST_GRAVITY_PARK_INDEX` (exec knobs, set once per process at the first statement): a park's take finds its
/// key's newest entry, and its put the free slot and the least recently put entry, through an index beside the slots
/// (`Index`), and `release_recorded` passes the Nodes whose sources keep nothing of a released relation (`RelsRead`); off,
/// the walks over every slot, as before. A park indexes once it has grown `PARK_INDEX_FROM` slots (a smaller one walks
/// them). Either way the same slot, the same entry and the same Node: the index keeps the
/// slots' layout (the lowest free slot first), so the byte cap's and the budget's slot-order ties fall as before.
static PARK_INDEX: AtomicBool = AtomicBool::new(true);

/// `PGRUST_GRAVITY_PARK_INDEX_CHECK` (exec knobs; every debug build): an indexed park also walks its slots at each take,
/// put, eviction and release, and compares (`index_mismatch`); the walk's answer is the one used.
static PARK_INDEX_CHECK: AtomicBool = AtomicBool::new(cfg!(debug_assertions));

/// The check's mismatches in this process (`index_mismatches`).
static INDEX_MISMATCHES: AtomicU64 = AtomicU64::new(0);

/// W1: the park index's switch and its check (a debug build checks whatever `check` says).
pub fn set_park_index(on: bool, check: bool) {
    PARK_INDEX.store(on, Ordering::Relaxed);
    PARK_INDEX_CHECK.store(check || cfg!(debug_assertions), Ordering::Relaxed);
}

/// W1: whether parks index their slots (`set_park_index`).
pub fn park_index() -> bool {
    PARK_INDEX.load(Ordering::Relaxed)
}

/// W1: the index check's mismatches so far in this process (0 on a sound index).
pub fn index_mismatches() -> u64 {
    INDEX_MISMATCHES.load(Ordering::Relaxed)
}

pub(super) fn index_check() -> bool {
    PARK_INDEX_CHECK.load(Ordering::Relaxed)
}

/// W1: the index answered otherwise than the walk: a LOG line, the count, the report's fire, and a debug build's panic.
#[cold]
#[inline(never)]
pub(super) fn index_mismatch(what: core::fmt::Arguments<'_>) {
    INDEX_MISMATCHES.fetch_add(1, Ordering::Relaxed);
    fires::fire("park.index_mismatch");
    eprintln!("gravity: park: index check mismatch: {what}");
    if cfg!(debug_assertions) {
        panic!("gravity: park: index check mismatch: {what}");
    }
}

/// The end of an index list; no slot.
pub(super) const NIL: u32 = u32::MAX;

/// W1: the relations a Node's sources keep state on between statements (`Source::rel_read`, nonzero), at most
/// `RelsRead::CAP` of them, else any relation. `Source::release_rel(rel)` releases nothing from a source that reads another
/// relation, so a release record naming none of them leaves the Node as it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RelsRead {
    /// The relations listed, or `RelsRead::ANY_N`: any relation.
    n: u8,
    rels: [u32; PARK_RELS_READ],
}

impl RelsRead {
    const ANY_N: u8 = u8::MAX;
    /// No source keeps state on a relation of its own.
    pub(crate) const NONE: RelsRead = RelsRead { n: 0, rels: [0; PARK_RELS_READ] };
    /// Any relation: a release record always reaches the Node.
    pub(crate) const ANY: RelsRead = RelsRead { n: RelsRead::ANY_N, rels: [0; PARK_RELS_READ] };

    /// `rel` (0: none of its own) added.
    pub(crate) fn add(&mut self, rel: u32) {
        if rel == 0 || self.n == RelsRead::ANY_N || self.rels[..self.n as usize].contains(&rel) {
            return;
        }
        if self.n as usize == PARK_RELS_READ {
            *self = RelsRead::ANY;
        } else {
            self.rels[self.n as usize] = rel;
            self.n += 1;
        }
    }

    /// Whether `record` may release anything from a Node that reads these relations.
    pub(super) fn reached_by(&self, record: &crate::slots::Released) -> bool {
        self.n == RelsRead::ANY_N || record.touches(&self.rels[..self.n as usize])
    }
}

/// W1: one key's bucket in `Index::table`: its hash's stored tag (`KeyHash::stored`, a hint before the key compare) and the
/// slot of its newest entry (`NIL`: empty).
#[derive(Clone, Copy, Debug)]
pub(super) struct Bucket {
    tag: u32,
    slot: u32,
}

pub(super) const EMPTY: Bucket = Bucket { tag: 0, slot: NIL };

/// W1: the index over a park's slots (`PARK_INDEX`): the LRU as a list by stamp, the occupied slots as bits, and per key
/// the slot of its newest entry (the key's older entries chain from it, `Entry::kolder`). Kept by every put, take, eviction
/// and drop while `on`; rebuilt from the slots when the switch turns it on; the slots stay the truth.
pub(super) struct Index {
    pub(super) on: bool,
    /// The allocator refused the index's room: the park stays on its walks.
    pub(super) refused: bool,
    /// The entry put least recently (the LRU victim) and most recently.
    pub(super) oldest: u32,
    pub(super) newest: u32,
    /// Bit `i`: slot `i` holds an entry (a word per 64 slots grown).
    pub(super) used: memledger::ChargedVec<u64>,
    /// Open addressing, linear probing: a power of two buckets (none until the first key), at most half of them used.
    pub(super) table: memledger::ChargedVec<Bucket>,
    /// The keys `table` holds.
    pub(super) keys: usize,
}

/// W1: the index's two vectors are the backend level's, as its fixed tables are (exec `BACKEND_OWNER`).
const INDEX_OWNER: memledger::Owner = memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::PerBackend));

impl Index {
    pub(super) const fn new() -> Index {
        Index { on: false, refused: false, oldest: NIL, newest: NIL, used: memledger::ChargedVec::none(), table: memledger::ChargedVec::none(), keys: 0 }
    }

    pub(super) fn bytes(&self) -> usize {
        self.used.capacity() * core::mem::size_of::<u64>() + self.table.capacity() * core::mem::size_of::<Bucket>()
    }
}

/// W1: a park key's hash, its image the handle and the generation.
#[inline]
fn key_hash(k: PlanKey) -> KeyHash {
    KeyHash::of(&[k.handle.0, k.generation.0 as u64])
}

/// The home bucket of `h` in a table of `len` (a power of two) buckets.
#[inline]
fn home(h: KeyHash, len: usize) -> usize {
    h.home(len.trailing_zeros(), Depth::TOP)
}

impl Park {
    /// W1: whether this park's slots are indexed now (the switch on and the index's room granted).
    #[doc(hidden)]
    pub fn indexed(&self) -> bool {
        self.ix.on
    }

    /// The lowest free slot grown so far (a put fills it; none: the put grows a slot or evicts).
    pub(super) fn free_slot(&self) -> Option<usize> {
        if !self.ix.on {
            return self.slots.iter().position(Option::is_none);
        }
        let mut got = None;
        for (w, &bits) in self.ix.used.as_slice().iter().enumerate() {
            if bits != u64::MAX {
                got = Some(w * 64 + (!bits).trailing_zeros() as usize).filter(|&i| i < self.slots.len());
                break;
            }
        }
        if index_check() {
            let want = self.slots.iter().position(Option::is_none);
            if got != want {
                index_mismatch(format_args!("put: index free slot {got:?}, walk free slot {want:?}"));
                return want;
            }
        }
        got
    }

    /// The entry put least recently (every slot is full): the count cap's victim.
    pub(super) fn lru_victim(&self) -> usize {
        let walked = || (0..self.slots.len()).min_by_key(|&i| self.slots[i].as_ref().map_or(0, |e| e.stamp)).expect("non-empty park");
        if !self.ix.on {
            return walked();
        }
        let got = self.ix.oldest as usize;
        if index_check() {
            let want = walked();
            if got != want {
                index_mismatch(format_args!("evict: index victim slot {got}, walk victim slot {want}"));
                return want;
            }
        }
        got
    }

    fn at(&self, i: u32) -> &Entry {
        self.slots[i as usize].as_ref().expect("an indexed slot holds its entry")
    }

    fn at_mut(&mut self, i: u32) -> &mut Entry {
        self.slots[i as usize].as_mut().expect("an indexed slot holds its entry")
    }

    /// The switch read: an index built when it turns on (from the slots) and the park has grown `PARK_INDEX_FROM` slots,
    /// dropped when it turns off.
    #[inline]
    pub(super) fn sync_index(&mut self) {
        let want = PARK_INDEX.load(Ordering::Relaxed) && self.slots.len() >= PARK_INDEX_FROM;
        if want != self.ix.on && !(want && self.ix.refused) {
            self.ix_rebuild(want);
        }
    }

    /// The index as the slots say, or off (`on` false, its room kept).
    #[cold]
    fn ix_rebuild(&mut self, on: bool) {
        self.ix.on = false;
        if !on {
            return;
        }
        let words = self.slots.len().div_ceil(64);
        let mut buckets = 16;
        while self.live * 2 > buckets {
            buckets *= 2;
        }
        self.ix.used.clear();
        self.ix.table.clear();
        if self.ix.used.capacity() == 0 {
            self.ix.used = memledger::ChargedVec::new(Some(INDEX_OWNER), memledger::Kind::Region);
        }
        if self.ix.table.capacity() == 0 {
            self.ix.table = memledger::ChargedVec::new(Some(INDEX_OWNER), memledger::Kind::Region);
        }
        if self.ix.used.resize(words, 0).is_err() || self.ix.table.resize(buckets, EMPTY).is_err() {
            self.ix.refused = true;
            return;
        }
        (self.ix.oldest, self.ix.newest, self.ix.keys, self.ix.on) = (NIL, NIL, 0, true);
        // linked in put order (by stamp): each link appends to the LRU and heads its key's chain, as its put did; a walk
        // per entry, once per switch, and no buffer
        let mut last = 0;
        while let Some(i) = (0..self.slots.len()).filter(|&i| self.slots[i].as_ref().is_some_and(|e| e.stamp > last)).min_by_key(|&i| self.slots[i].as_ref().map_or(0, |e| e.stamp)) {
            last = self.slots[i].as_ref().map_or(0, |e| e.stamp);
            self.ix_link(i);
            if !self.ix.on {
                return;
            }
        }
        self.ix_checked("rebuild");
    }

    /// The slot of `key`'s newest entry, through its bucket.
    #[inline]
    pub(super) fn ix_newest(&self, key: PlanKey) -> Option<usize> {
        self.ix_find(key, key_hash(key)).map(|b| self.ix.table.as_slice()[b].slot as usize)
    }

    /// Every slot emptied (`Park::clear`): an empty index, its room kept.
    pub(super) fn ix_clear(&mut self) {
        if self.ix.on {
            self.ix.used.as_mut_slice().fill(0);
            self.ix.table.as_mut_slice().fill(EMPTY);
            (self.ix.oldest, self.ix.newest, self.ix.keys) = (NIL, NIL, 0);
        }
    }

    /// The bucket of `key` (hash `h`), if the table holds it.
    #[inline]
    fn ix_find(&self, key: PlanKey, h: KeyHash) -> Option<usize> {
        let t = self.ix.table.as_slice();
        if t.is_empty() {
            return None;
        }
        let (mask, tag) = (t.len() - 1, h.stored());
        let mut b = home(h, t.len());
        loop {
            let x = t[b];
            if x.slot == NIL {
                return None;
            }
            if x.tag == tag && self.slots[x.slot as usize].as_ref().is_some_and(|e| e.key == key) {
                return Some(b);
            }
            b = (b + 1) & mask;
        }
    }

    /// Slot `i`'s new entry (the newest of all) into the index: the LRU's newest end, its key's chain head, its bit.
    pub(super) fn ix_link(&mut self, i: usize) {
        let (key, slot) = (self.at(i as u32).key, i as u32);
        let w = i / 64;
        if w >= self.ix.used.len() && self.ix.used.resize(w + 1, 0).is_err() {
            return self.ix_refuse();
        }
        self.ix.used.as_mut_slice()[w] |= 1 << (i % 64);
        let newest = self.ix.newest;
        {
            let e = self.at_mut(slot);
            (e.older, e.newer, e.kolder, e.knewer) = (newest, NIL, NIL, NIL);
        }
        match newest {
            NIL => self.ix.oldest = slot,
            n => self.at_mut(n).newer = slot,
        }
        self.ix.newest = slot;
        let h = key_hash(key);
        match self.ix_find(key, h) {
            Some(b) => {
                let head = self.ix.table.as_slice()[b].slot;
                self.at_mut(slot).kolder = head;
                self.at_mut(head).knewer = slot;
                self.ix.table.as_mut_slice()[b].slot = slot;
            }
            None if (self.ix.keys + 1) * 2 > self.ix.table.len() => {
                // the table grows: every chain head again, this entry's among them
                let n = (self.ix.table.len() * 2).max(16);
                self.ix.table.clear();
                if self.ix.table.capacity() == 0 {
                    self.ix.table = memledger::ChargedVec::new(Some(INDEX_OWNER), memledger::Kind::Region);
                }
                if self.ix.table.resize(n, EMPTY).is_err() {
                    return self.ix_refuse();
                }
                self.ix.keys = 0;
                for j in 0..self.slots.len() {
                    if let Some(e) = self.slots[j].as_ref().filter(|e| e.knewer == NIL && (self.ix.used.as_slice()[j / 64] >> (j % 64)) & 1 == 1) {
                        let h = key_hash(e.key);
                        self.ix_insert(h, j as u32);
                    }
                }
            }
            None => self.ix_insert(h, slot),
        }
    }

    /// A key's bucket (the table has room: at most half full after it).
    fn ix_insert(&mut self, h: KeyHash, slot: u32) {
        let t = self.ix.table.as_mut_slice();
        let mask = t.len() - 1;
        let mut b = home(h, t.len());
        while t[b].slot != NIL {
            b = (b + 1) & mask;
        }
        t[b] = Bucket { tag: h.stored(), slot };
        self.ix.keys += 1;
    }

    /// Slot `i`'s entry out of the index (before it leaves its slot): out of the LRU and its key's chain, its bit cleared;
    /// the key's bucket moves to its next older entry, or goes when it had none.
    pub(super) fn ix_unlink(&mut self, i: usize) {
        let (key, older, newer, kolder, knewer) = {
            let e = self.at(i as u32);
            (e.key, e.older, e.newer, e.kolder, e.knewer)
        };
        match older {
            NIL => self.ix.oldest = newer,
            o => self.at_mut(o).newer = newer,
        }
        match newer {
            NIL => self.ix.newest = older,
            n => self.at_mut(n).older = older,
        }
        if knewer != NIL {
            self.at_mut(knewer).kolder = kolder;
        } else {
            let b = self.ix_find(key, key_hash(key)).expect("an indexed key's newest entry has its bucket");
            match kolder {
                NIL => self.ix_remove_bucket(b),
                k => self.ix.table.as_mut_slice()[b].slot = k,
            }
        }
        if kolder != NIL {
            self.at_mut(kolder).knewer = knewer;
        }
        self.ix.used.as_mut_slice()[i / 64] &= !(1 << (i % 64));
    }

    /// Bucket `hole` emptied, the probe runs after it shifted back over it (no tombstones).
    fn ix_remove_bucket(&mut self, mut hole: usize) {
        let (len, mask) = (self.ix.table.len(), self.ix.table.len() - 1);
        let mut j = hole;
        loop {
            j = (j + 1) & mask;
            let x = self.ix.table.as_slice()[j];
            if x.slot == NIL {
                break;
            }
            // `x` may fill the hole unless its home bucket lies after the hole, up to `j` (cyclically)
            let at = home(key_hash(self.at(x.slot).key), len);
            if (j.wrapping_sub(at) & mask) >= (j.wrapping_sub(hole) & mask) {
                self.ix.table.as_mut_slice()[hole] = x;
                hole = j;
            }
        }
        self.ix.table.as_mut_slice()[hole] = EMPTY;
        self.ix.keys -= 1;
    }

    /// The allocator refused the index's room: the walks from now on.
    #[cold]
    fn ix_refuse(&mut self) {
        self.ix.on = false;
        self.ix.refused = true;
        fires::fire("park.index_refused");
    }

    /// The check (`PARK_INDEX_CHECK`): the index rebuilt in thought from the slots and compared (bits, LRU, chains,
    /// buckets); a mismatch is counted and logged.
    pub(super) fn ix_checked(&self, at: &str) {
        if !self.ix.on || !index_check() {
            return;
        }
        if let Err(why) = self.ix_sound() {
            index_mismatch(format_args!("{at}: {why}"));
        }
    }

    fn ix_sound(&self) -> Result<(), String> {
        let ix = &self.ix;
        for (i, s) in self.slots.iter().enumerate() {
            let bit = ix.used.as_slice().get(i / 64).is_some_and(|w| (w >> (i % 64)) & 1 == 1);
            if bit != s.is_some() {
                return Err(format!("slot {i}: bit {bit}, entry {}", s.is_some())); // alloc: Cold
            }
        }
        let (mut n, mut at, mut prev, mut stamp) = (0, ix.oldest, NIL, 0);
        while at != NIL {
            let e = self.slots.get(at as usize).and_then(Option::as_ref).ok_or_else(|| format!("LRU names empty slot {at}"))?; // alloc: Cold
            if e.older != prev || e.stamp <= stamp {
                return Err(format!("LRU at slot {at}: older {} want {prev}, stamp {} after {stamp}", e.older, e.stamp)); // alloc: Cold
            }
            (n, prev, stamp, at) = (n + 1, at, e.stamp, e.newer);
            if n > self.live {
                return Err(format!("LRU longer than the {} entries", self.live)); // alloc: Cold
            }
        }
        if n != self.live || ix.newest != prev {
            return Err(format!("LRU holds {n} of {} entries, newest {} want {prev}", self.live, ix.newest)); // alloc: Cold
        }
        let mut heads = 0;
        for (i, e) in self.slots.iter().enumerate() {
            let Some(e) = e else { continue };
            if e.knewer == NIL {
                heads += 1;
                let b = self.ix_find(e.key, key_hash(e.key)).ok_or_else(|| format!("slot {i}: key {:?} has no bucket", e.key))?; // alloc: Cold
                if ix.table.as_slice()[b].slot != i as u32 {
                    return Err(format!("slot {i}: key {:?}'s bucket names slot {}", e.key, ix.table.as_slice()[b].slot)); // alloc: Cold
                }
            } else if self.slots.get(e.knewer as usize).and_then(Option::as_ref).is_none_or(|x| x.key != e.key || x.kolder != i as u32 || x.stamp <= e.stamp) {
                return Err(format!("slot {i}: key chain broken at knewer {}", e.knewer)); // alloc: Cold
            }
            if e.kolder != NIL && self.slots.get(e.kolder as usize).and_then(Option::as_ref).is_none_or(|x| x.key != e.key || x.knewer != i as u32 || x.stamp >= e.stamp) {
                return Err(format!("slot {i}: key chain broken at kolder {}", e.kolder)); // alloc: Cold
            }
        }
        if heads != ix.keys || ix.table.as_slice().iter().filter(|b| b.slot != NIL).count() != heads {
            return Err(format!("{heads} keys, the table counts {} and holds {}", ix.keys, ix.table.as_slice().iter().filter(|b| b.slot != NIL).count())); // alloc: Cold
        }
        Ok(())
    }
}
