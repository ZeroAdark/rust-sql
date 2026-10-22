use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use pgsync::Mutex;

use gravity_program::cost::runtime::REGION_MIN_SHIFT;
use gravity_program::{ContentKey, Handle, PlanKey};

use crate::node::{Node, RegionBuf};

mod index;

use index::{index_check, index_mismatch, Index, NIL};
pub(crate) use index::RelsRead;
pub use index::{index_mismatches, park_index, set_park_index};

struct Entry {
    key: PlanKey,
    node: Box<Node>,
    stamp: u64,
    /// `node.bytes()` as the park last read it: at the put, again after a shed (H16 (b) B2).
    bytes: usize,
    /// The Node's regions went back since it parked (`Node::shed_regions`): a parked Node runs nothing, so a second shed
    /// frees nothing and the over-budget loop passes it (`shed_once`).
    shed: bool,
    /// W1 (`Index`, read only while the park is indexed): the entries put just before and just after this one (the LRU,
    /// by stamp), and the entries of its key put just before and just after it (its key's chain); `NIL` at an end.
    older: u32,
    newer: u32,
    kolder: u32,
    knewer: u32,
    /// W1: the relations its Node's sources keep state on (`RelsRead`, the Node's own; any relation while it holds worker
    /// slots): a release record that names none of them releases nothing from it, so `release_recorded` passes it.
    rels: RelsRead,
}

/// `PGRUST_GRAVITY_SHED_ONCE` (exec knobs, set once per process at the first statement): a parked Node is shed at most once
/// while it stays parked; off, every put over the park budget sheds every entry again, as before.
static SHED_ONCE: AtomicBool = AtomicBool::new(true);

pub fn set_shed_once(on: bool) {
    SHED_ONCE.store(on, Ordering::Relaxed);
}

fn shed_once() -> bool {
    SHED_ONCE.load(Ordering::Relaxed)
}

/// A backend's parked Nodes. H16 (b) B2 (FRAME's design): the bytes are a running total kept at every put,
/// take, eviction, drop and shed (never summed per put), and `take` looks at the last put's slot first; the LRU, the cap,
/// the sheds and the park budget are as before.
pub struct Park {
    /// D3620: grown a slot at a time up to `cap`, as Nodes are put: a park holds no slot it never filled (an `Entry`
    /// holds its Node boxed). A deep statement level's park (gravity_exec `DEEP_PARK_NODES`) is empty for
    /// as long as its one statement runs, and every level of a deep SQL-function recursion holds one.
    slots: Vec<Option<Entry>>,
    cap: usize,
    clock: u64,
    evictions: u64,
    cap_bytes: usize,
    /// The sum of the entries' `bytes`.
    total: usize,
    /// The occupied slots.
    live: usize,
    /// The last put's slot: when it still holds that entry (the newest of all) and the key matches, it is the newest of its key.
    hint: usize,
    /// W1: the index over the slots (`PARK_INDEX`).
    ix: Index,
    /// The parked Nodes `release_recorded` visited (a test's witness of the passes).
    visits: u64,
}

pub use mempolicy::keep::PARK_BYTES;

/// D3063 (b): what the pool holds for no running statement (the parked regions and the parked worker-slot sets: pages a
/// statement touched, kept resident for the next one) trimmed, least recently put first, to a quarter of the pool at
/// each admitted statement's end. The depots' own caps (regions 1/6, slots 1/3 of the pool) and the backends' parks (1/11
/// each) let more than half the pool sit idle and resident between statements; a back-to-back run never reaches the
/// idle release, and beside shared_buffers that residue took a 30 GiB box past its RAM (D3063: 'pool held 15889 of 17709
/// MiB' with one 7629 MiB statement in flight). The bytes freed.
pub fn trim_idle_holds() -> usize {
    let cap = crate::mem::grant::POOL.capacity() as usize;
    if cap == 0 {
        return 0;
    }
    let keep = mempolicy::keep::idle_keep(cap);
    let slots = crate::slots::DEPOT.stats().bytes;
    let Some(to) = mempolicy::keep::end_regions_to(keep, REGIONS.stats().bytes, slots) else { return 0 };
    let mut freed = REGIONS.trim_to(to);
    match mempolicy::keep::end_slots_over(keep, REGIONS.stats().bytes + slots) {
        0 => {}
        over => freed += crate::slots::DEPOT.trim_for(over),
    }
    freed
}

/// DR-18: a backend's park cap: 1/11 of the pool (`PARK_BYTES` at the measured pool).
pub fn park_bytes() -> usize {
    mempolicy::keep::park_bytes(crate::mem::grant::POOL.capacity())
}

/// DR-18: the process's parked-region cap: 1/6 of the pool (`REGION_DEPOT_BYTES` at the measured pool).
pub fn region_depot_bytes() -> usize {
    mempolicy::keep::region_depot_bytes(crate::mem::grant::POOL.capacity())
}

impl Park {
    pub fn new(capacity: usize) -> Park {
        Park::with_bytes(capacity, park_bytes())
    }

    pub fn with_bytes(capacity: usize, cap_bytes: usize) -> Park {
        Park { slots: Vec::new(), cap: capacity.max(1), clock: 0, evictions: 0, cap_bytes, total: 0, live: 0, hint: 0, ix: Index::new(), visits: 0 } // alloc: Exec
    }

    pub fn bytes(&self) -> usize {
        debug_assert_eq!(self.total, self.slots.iter().flatten().map(|e| e.node.bytes()).sum::<usize>(), "the park's running byte total");
        self.total
    }

    /// Slot `i`'s entry out of the park, the total and the count with it (and out of the index).
    fn remove(&mut self, i: usize) -> Option<Entry> {
        if self.ix.on && self.slots[i].is_some() {
            self.ix_unlink(i);
        }
        let e = self.slots[i].take()?;
        self.total -= e.bytes;
        self.live -= 1;
        Some(e)
    }

    /// D3147: every parked Node's pool regions and arena chunks to the depot (`Node::shed_regions`, its image kept), as a put
    /// over the park budget sheds them: the idle release's reach into this backend's park. D3999: and its sources' kept
    /// cursors and pins, each a use of its part, so an idle session pins no part of the part cache. The bytes shed.
    pub fn shed_all(&mut self) -> usize {
        let mut freed = 0;
        for i in 0..self.slots.len() {
            if let Some(e) = self.slots[i].as_mut() {
                freed += e.node.shed_regions();
                e.shed = true;
                self.reread(i);
            }
        }
        let mut doomed: Vec<Box<dyn core::any::Any>> = Vec::new(); // alloc: Cold
        self.release_recorded(&crate::slots::Released::ALL, &mut doomed);
        drop(doomed);
        freed
    }

    /// Slot `i`'s bytes read again (a shed gave regions back).
    fn reread(&mut self, i: usize) {
        if let Some(e) = self.slots[i].as_mut() {
            let b = e.node.bytes();
            self.total = self.total - e.bytes + b;
            e.bytes = b;
        }
    }

    pub fn take(&mut self, key: PlanKey, content: ContentKey) -> Option<Box<Node>> {
        self.sync_index();
        let best = if self.slots.get(self.hint).and_then(Option::as_ref).is_some_and(|e| e.key == key && e.stamp == self.clock) {
            Some(self.hint)
        } else if self.ix.on {
            let got = self.ix_newest(key);
            if index_check() {
                let want = self.newest_walked(key);
                if got != want {
                    index_mismatch(format_args!("take {key:?}: index slot {got:?}, walk slot {want:?}"));
                    return self.take_at(want, key, content);
                }
            }
            got
        } else {
            self.newest_walked(key)
        };
        self.take_at(best, key, content)
    }

    /// The newest entry of `key`, by a walk over every slot.
    fn newest_walked(&self, key: PlanKey) -> Option<usize> {
        let mut best: Option<usize> = None;
        for (i, e) in self.slots.iter().enumerate() {
            if let Some(e) = e {
                if e.key == key && best.is_none_or(|b| e.stamp > self.slots[b].as_ref().map_or(0, |x| x.stamp)) {
                    best = Some(i);
                }
            }
        }
        best
    }

    /// `take`'s entry at slot `best` out, its content checked.
    fn take_at(&mut self, best: Option<usize>, key: PlanKey, content: ContentKey) -> Option<Box<Node>> {
        let e = self.remove(best?)?;
        if e.node.program().key != content {
            self.drop_for(key.handle);
            return None;
        }
        let mut node = e.node;
        node.unpark();
        Some(node)
    }

    pub fn put(&mut self, key: PlanKey, mut node: Box<Node>) -> bool {
        node.park();
        self.put_parked(key, node)
    }

    /// D3881: every Node parked under `key`, still parked, out of the park onto `out` with its key (a freed statement
    /// level's, for the thread's pool of kept executions).
    pub fn take_parked_into(&mut self, key: PlanKey, out: &mut Vec<(PlanKey, Box<Node>)>) {
        self.sync_index();
        for i in 0..self.slots.len() {
            if self.slots[i].as_ref().is_some_and(|e| e.key == key) {
                if let Some(e) = self.remove(i) {
                    out.push((key, e.node)); // alloc: Exec
                }
            }
        }
        self.ix_checked("take_parked_into");
    }

    /// D3881: a Node `take_parked_into` gave out, parked here under `key` as `put` parks one.
    pub fn put_parked(&mut self, key: PlanKey, node: Box<Node>) -> bool {
        self.sync_index();
        self.clock += 1;
        let free = self.free_slot().or_else(|| {
            (self.slots.len() < self.cap).then(|| {
                self.slots.push(None); // alloc: Exec
                self.slots.len() - 1
            })
        });
        let idx = match free {
            Some(i) => i,
            None => {
                let victim = self.lru_victim();
                self.remove(victim);
                self.evictions += 1;
                victim
            }
        };
        let bytes = node.bytes();
        // a Node holding worker slots (a parallel statement's, kept with it) is reached by every record, as before
        let rels = if node.workers.is_empty() { node.rels } else { RelsRead::ANY };
        self.slots[idx] = Some(Entry { key, node, stamp: self.clock, bytes, shed: false, older: NIL, newer: NIL, kolder: NIL, knewer: NIL, rels });
        if self.ix.on {
            self.ix_link(idx);
        }
        self.total += bytes;
        self.live += 1;
        self.hint = idx;
        while self.live > 1 && self.total > self.cap_bytes {
            let victim = (0..self.slots.len()).filter(|&i| i != idx && self.slots[i].is_some()).max_by_key(|&i| self.slots[i].as_ref().map_or(0, |e| e.bytes)).expect("an older entry");
            self.remove(victim);
            self.evictions += 1;
        }
        // under a rail (the pool gated) a parked Node keeps its image, never its regions: another
        // backend's park is out of admission's reach, and the depot the regions go to is trimmable;
        // ungated, only over the park budget — largest first, the one just parked included
        if crate::mem::grant::POOL.max_inflight() != 0 {
            if let Some(e) = self.slots[idx].as_mut() {
                e.node.shed_regions();
                e.shed = true;
            }
            self.reread(idx);
        }
        if crate::mem::grant::POOL.over_park_budget() {
            let once = shed_once();
            let mut order: Vec<usize> = (0..self.slots.len()).filter(|&i| self.slots[i].as_ref().is_some_and(|e| !(once && e.shed))).collect(); // alloc: Cold
            order.sort_by_key(|&i| core::cmp::Reverse(self.slots[i].as_ref().map_or(0, |e| e.bytes)));
            for i in order {
                if !crate::mem::grant::POOL.over_park_budget() {
                    break;
                }
                if let Some(e) = self.slots[i].as_mut() {
                    e.node.shed_regions();
                    e.shed = true;
                }
                self.reread(i);
            }
        }
        // the put that grew the park's `PARK_INDEX_FROM`th slot builds its index
        self.sync_index();
        self.ix_checked("put");
        free.is_some()
    }

    /// D1507: `record` applied to every parked Node (its sources' kept cursors into `doomed`, dropped by the caller);
    /// the bytes stay (a cursor is no Node byte). W1: with the switch on, a Node whose sources read none of the record's relations
    /// (`Entry::rels`) is passed: `Source::release_rel` would release nothing from it. The check releases it anyway and
    /// counts a mismatch if that released anything.
    pub(crate) fn release_recorded(&mut self, record: &crate::slots::Released, doomed: &mut Vec<Box<dyn core::any::Any>>) {
        if !park_index() {
            for e in self.slots.iter_mut().flatten() {
                e.node.release_recorded(record, doomed);
                self.visits += 1;
            }
            return;
        }
        let check = index_check();
        for (i, e) in self.slots.iter_mut().enumerate() {
            let Some(e) = e else { continue };
            if e.rels.reached_by(record) {
                e.node.release_recorded(record, doomed);
                self.visits += 1;
            } else if check {
                let before = doomed.len();
                e.node.release_recorded(record, doomed);
                if doomed.len() != before {
                    index_mismatch(format_args!("release passed slot {i} (reads {:?}), which kept {} released item(s)", e.rels, doomed.len() - before));
                }
            }
        }
    }

    pub fn drop_for(&mut self, handle: Handle) -> usize {
        self.sync_index();
        let mut dropped = 0;
        for i in 0..self.slots.len() {
            if self.slots[i].as_ref().is_some_and(|e| e.key.handle == handle) {
                self.remove(i);
                dropped += 1;
            }
        }
        if dropped > 0 {
            self.ix_checked("drop_for");
        }
        dropped
    }

    pub fn len(&self) -> usize {
        debug_assert_eq!(self.live, self.slots.iter().filter(|s| s.is_some()).count(), "the park's entry count");
        self.live
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// D3620: the slots the park holds now (grown with its puts, up to `capacity`).
    #[doc(hidden)]
    pub fn held_slots(&self) -> usize {
        self.slots.len()
    }

    pub fn evictions(&self) -> u64 {
        self.evictions
    }

    /// W1: the parked Nodes the release passes visited so far (a record's pass skips those it cannot reach, indexed).
    #[doc(hidden)]
    pub fn release_visits(&self) -> u64 {
        self.visits
    }

    pub fn clear(&mut self) {
        self.slots.iter_mut().for_each(|s| *s = None);
        self.total = 0;
        self.live = 0;
        self.ix_clear();
    }

    /// D3881: the bytes of the slots grown so far and of the boxed Nodes the occupied ones hold, and (W1) the index's.
    pub fn slot_bytes(&self) -> usize {
        self.slots.capacity() * core::mem::size_of::<Option<Entry>>() + self.live * core::mem::size_of::<Node>() + self.ix.bytes()
    }

}

/// The process-wide region depot (partitioned-record.md §4.5 as landed): breaker regions parked by size class (a power of
/// two from `REGION_MIN`) when their Node drops, taken by the next slot open. Capacity only: nothing reads a parked region's
/// bytes (`open_slot` zeroes `ZERO_PREFIX`, every table lays out its own extent), so a fresh backend takes resident pages
/// instead of first-touch faults. `REGION_DEPOT_BYTES` caps the parked bytes; over it the least recently put regions drop first.
/// D2687: a statement's spill regions park here at its end too (`arena::drop_result_spills`), their pages MADV_FREE'd first.
/// D405: over the grant pool's park budget a put evicts the least recently put regions (any class) down to the budget and
/// parks the returned one; it drops the returned one only when evicting every parked region would not bring the pool under.
pub struct RegionDepot {
    classes: Mutex<Classes>,
    bytes: AtomicUsize,
    takes: AtomicU64,
    puts: AtomicU64,
    allocs: AtomicU64,
    evictions: AtomicU64,
    /// RULE 7: the unspent reservations of the statements whose regions park now (`parking_beside`).
    beside: AtomicU64,
}

pub use mempolicy::keep::REGION_DEPOT_BYTES;
/// The smallest class (one page); a region asks for its class's size, so any parked region of the class serves it.
pub const REGION_MIN: usize = 1 << REGION_MIN_SHIFT;
const REGION_CLASSES: usize = 40 - REGION_MIN_SHIFT;

/// Each class oldest put first (a take removes in place, keeping the order), every region stamped with its put's clock.
struct Classes {
    q: [VecDeque<(u64, RegionBuf)>; REGION_CLASSES],
    clock: u64,
}

impl Classes {
    /// The class whose front (its oldest) region was put least recently, if any region is parked.
    fn oldest(&self) -> Option<usize> {
        (0..REGION_CLASSES).filter_map(|k| self.q[k].front().map(|e| (e.0, k))).min().map(|(_, k)| k)
    }

    fn entries(&self) -> usize {
        self.q.iter().map(VecDeque::len).sum()
    }

    /// P3: the stamp of the region put most recently, if any is parked.
    fn newest(&self) -> Option<u64> {
        self.q.iter().filter_map(|q| q.back().map(|e| e.0)).max()
    }
}

pub static REGIONS: RegionDepot = RegionDepot { classes: Mutex::new(Classes { q: [const { VecDeque::new() }; REGION_CLASSES], clock: 0 }), bytes: AtomicUsize::new(0), takes: AtomicU64::new(0), puts: AtomicU64::new(0), allocs: AtomicU64::new(0), evictions: AtomicU64::new(0), beside: AtomicU64::new(0) }; // alloc: Cold

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RegionDepotStats {
    pub entries: usize,
    pub bytes: usize,
    pub takes: u64,
    pub puts: u64,
    pub allocs: u64,
    pub evictions: u64,
}

/// The class a take of `bytes` looks in first: every region parked there is at least `bytes` long.
fn class_of(bytes: usize) -> usize {
    (bytes.max(REGION_MIN).next_power_of_two().trailing_zeros() as usize - REGION_MIN_SHIFT).min(REGION_CLASSES - 1)
}

/// The class a region of `len` parks in: the power of two at or under it (a region is its asked bytes, page-rounded, never its class).
fn park_class_of(len: usize) -> usize {
    ((usize::BITS - 1 - len.max(REGION_MIN).leading_zeros()) as usize - REGION_MIN_SHIFT).min(REGION_CLASSES - 1)
}

/// The bytes a fresh region maps for a take of `bytes`: page-rounded (D157: the power-of-two class drew up to twice the share).
pub fn region_bytes(bytes: usize) -> usize {
    bytes.max(REGION_MIN).next_multiple_of(REGION_MIN)
}

impl RegionDepot {
    /// A region of at least `bytes`: a parked one that fits, else a fresh page-rounded allocation (alloc: Node).
    pub fn take(&self, bytes: usize, stmt: Option<&crate::mem::grant::Reservation>) -> crate::Result<RegionBuf> {
        self.take_for(bytes, stmt, None)
    }

    /// `take` charged to `life` (else `stmt`'s G).
    pub fn take_for(&self, bytes: usize, stmt: Option<&crate::mem::grant::Reservation>, life: Option<&std::rc::Rc<crate::mem::grant::StmtCtx>>) -> crate::Result<RegionBuf> {
        // a slot's or a program constant's region: never under a strict reservation (H16 (a); arena chunks take `take_parked`)
        if let Some(s) = stmt {
            s.region_draw()?;
        }
        match self.take_parked(bytes, stmt, life) {
            Some(buf) => Ok(buf),
            None => {
                self.allocs.fetch_add(1, Ordering::Relaxed);
                crate::node::uninit_region(region_bytes(bytes), stmt, life)
            }
        }
    }

    /// A parked region of at least `bytes` (the class one up before none, D157 / D223), recharged to the taker (D191).
    /// D3074 / D3089, one rule for every taker (a slot's share, a worker's, an arena chunk): (R1) a region within an eighth
    /// of the page-rounded ask (D225's fit, which a two-key group-by taking 512 MiB for 277 named); (R2) none, and the
    /// alternative is a fresh draw: the smallest region covering the ask, at most four times it (the class one up). (R3)
    /// Either is fitted to the ask before it is charged (`RegionBuf::fit_to`): the taker is charged the ask, never the
    /// region's size, and the rest of the allocation is an uncharged tail with its pages given back. Before R2, a slot that
    /// took a 512 MiB worker region for 227 MiB mapped 227 MiB fresh beside it; an arena chunk took 32 MiB for 8 MiB and
    /// charged W or G all 32.
    pub fn take_parked(&self, bytes: usize, stmt: Option<&crate::mem::grant::Reservation>, life: Option<&std::rc::Rc<crate::mem::grant::StmtCtx>>) -> Option<RegionBuf> {
        let c = class_of(bytes);
        // D369: the ask is the page-rounded region a fresh take maps; against the raw ask a sub-page one never fit its own
        // parked page, so every take mapped and parked another (+1 entry a take, the scan under the lock growing with them)
        let want = region_bytes(bytes);
        // option c': a parked reservation serves only a reservation ask (`take_reserved`), never a plain share
        let fits = |b: &RegionBuf| !b.is_reserved() && b.whole() >= bytes && b.whole().saturating_sub(want) <= want / 8;
        let covers = |b: &RegionBuf| !b.is_reserved() && b.whole() >= bytes && b.whole() <= want.saturating_mul(4);
        let parked = {
            let mut classes = self.classes.lock().unwrap_or_else(|p| p.into_inner());
            let (lo, hi) = (c.saturating_sub(1), (c + 1).min(REGION_CLASSES - 1));
            let near = (lo..=hi).find_map(|k| classes.q[k].iter().rposition(|e| fits(&e.1)).and_then(|i| classes.q[k].remove(i)).map(|e| e.1));
            let taken = near.or_else(|| {
                let mut best: Option<(usize, usize, usize)> = None;
                for k in lo..=hi {
                    for (i, e) in classes.q[k].iter().enumerate() {
                        if covers(&e.1) && best.is_none_or(|b| e.1.whole() < b.0) {
                            best = Some((e.1.whole(), k, i));
                        }
                    }
                }
                best.and_then(|(_, k, i)| classes.q[k].remove(i)).map(|e| e.1)
            });
            // D4587: out of the count under the lock that took it out (a count changed only under the lock is the parked
            // regions' bytes whenever the lock is free: subtracted after it, a `clear` between took the count below zero)
            if let Some(b) = &taken {
                self.bytes.fetch_sub(b.len(), Ordering::Relaxed);
            }
            taken
        };
        // take-back: past the depot, a queued whole give of the ask's shape (its pages still resident) before a fresh one
        let (parked, back) = match parked {
            Some(b) => (Some(b), 0),
            None => crate::mem::giveback::take_back_give(want).map_or((None, 0), |(b, n)| (Some(b), n)),
        };
        if crate::node::region_debug() & crate::node::TRACE != 0 {
            eprintln!("gravity: region: depot take {bytes} class {c} {} (parked entries {}, bytes {})", parked.as_ref().map_or("alloc".to_string(), |b| format!("parked {}", b.whole())), self.classes.lock().unwrap_or_else(|p| p.into_inner()).entries(), self.bytes.load(Ordering::Relaxed)); // alloc: Cold
        }
        match parked {
            Some(mut buf) => {
                buf.unparked();
                // fitted to the ask (its pool hold with it), then charged to the taker; larger than the taker's grant holds
                // (S7), or no pool room for a growth into its tail: dropped, the taker allocates what it asked
                if !buf.fit_to(want) || !buf.recharge(stmt, life) {
                    self.evictions.fetch_add(1, Ordering::Relaxed);
                    // a region taken back goes back to the lane as its owner gave it (no give on this thread)
                    if back != 0 {
                        crate::mem::giveback::give(buf);
                    }
                    return None;
                }
                if back != 0 {
                    crate::mem::giveback::note_taken_back(back);
                }
                self.takes.fetch_add(1, Ordering::Relaxed);
                Some(buf)
            }
            None => {
                crate::mem::giveback::note_take_miss();
                None
            }
        }
    }

    /// Option c': a parked reservation for a `Budget::Pool` side's ask (`reserved_region`): its share within an eighth over
    /// the page-rounded `bytes` (D225's fit) and an address space of at least `reserve`. Its share's pages are resident, as a
    /// parked share's are: a fresh reservation faulted every page its table laid, every statement (c-setop's worst case at
    /// W8: nine shares mapped fresh a run). It comes back with its settled hold; the taker charges it anew.
    pub(crate) fn take_reserved(&self, bytes: usize, reserve: usize) -> Option<RegionBuf> {
        let c = class_of(bytes);
        let want = region_bytes(bytes);
        let fits = |b: &RegionBuf| b.is_reserved() && b.len() >= bytes && b.len().saturating_sub(want) <= want / 8 && b.reserve() >= reserve;
        let buf = {
            let mut classes = self.classes.lock().unwrap_or_else(|p| p.into_inner());
            let taken = (c.saturating_sub(1)..=(c + 1).min(REGION_CLASSES - 1)).find_map(|k| classes.q[k].iter().rposition(|e| fits(&e.1)).and_then(|i| classes.q[k].remove(i)).map(|e| e.1));
            // D4587: out of the count under the lock that took it out (`take_parked`)
            if let Some(b) = &taken {
                self.bytes.fetch_sub(b.len(), Ordering::Relaxed);
            }
            taken
        };
        let Some(mut buf) = buf else {
            crate::mem::giveback::note_take_miss();
            return None;
        };
        buf.unparked();
        self.takes.fetch_add(1, Ordering::Relaxed);
        Some(buf)
    }

    /// A parked region that started small (`RegionBuf::is_laid`), its grown pages resident (`uncommit` keeps
    /// them in its length), for a laid ask (`region::laid_region`): the longest of at least `bytes` and at most `reserve`
    /// whose reservation is at least `reserve`. It comes back with its settled hold; the taker holds it anew. A region shorter
    /// than the start is never taken and lengthened in place: its pages past its length are no part of what its taker lays.
    /// D3794: the bound is the ask's reservation (what its taker lays at and may commit in place up to), not its share: a
    /// partitioned hash aggregate's worker starts small and commits past its share in place, so a share bound never took
    /// its region back, and every repeat of the statement mapped a fresh reservation per worker and faulted its growth
    /// again (each first write of a huge page zeroed by the kernel). One longer than the reservation is not taken: its
    /// taker could commit nothing past it.
    pub(crate) fn take_laid(&self, bytes: usize, reserve: usize) -> Option<RegionBuf> {
        let fits = |b: &RegionBuf| b.is_laid() && b.len() >= bytes && b.len() <= reserve && b.reserve() >= reserve;
        let mut buf = {
            let mut classes = self.classes.lock().unwrap_or_else(|p| p.into_inner());
            let mut best: Option<(usize, usize, usize)> = None;
            for (k, q) in classes.q.iter().enumerate() {
                for (i, e) in q.iter().enumerate() {
                    if fits(&e.1) && best.is_none_or(|b| e.1.len() > b.0) {
                        best = Some((e.1.len(), k, i));
                    }
                }
            }
            let (_, k, i) = best?;
            let taken = classes.q[k].remove(i).map(|e| e.1);
            // D4587: out of the count under the lock that took it out (`take_parked`)
            if let Some(b) = &taken {
                self.bytes.fetch_sub(b.len(), Ordering::Relaxed);
            }
            taken
        }?;
        buf.unparked();
        self.takes.fetch_add(1, Ordering::Relaxed);
        Some(buf)
    }

    /// A region whose life ended mid-statement: its charge handed to the pool (G's room back), then parked; dropped without capacity.
    /// RULE 7: parked beside `stmt`'s unspent reservation (`parking_beside`), which now holds the region's bytes too: counted
    /// whole, a reservation past half the pool dropped a slot freed mid-statement (CB9 W8's 3.3 GiB slot-0 share every run).
    pub fn put_from_life(&self, mut buf: RegionBuf, stmt: Option<&crate::mem::grant::Reservation>) {
        if buf.to_pool() {
            self.parking_beside(stmt.map_or(0, crate::mem::grant::Reservation::left), || self.put(buf));
        } else {
            self.evictions.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// D3063 (b): the least recently put regions dropped until at most `keep` bytes stay parked; the bytes freed.
    /// D4398: in the ladder thread's pass, nothing while another thread holds the depot (`memledger::ladder_pass`).
    pub fn trim_to(&self, keep: usize) -> usize {
        let Some(mut classes) = memledger::ladder_pass::lock_or_skip(&self.classes) else { return 0 };
        let mut freed = 0;
        let mut doomed = Vec::new(); // alloc: Cold
        while self.bytes.load(Ordering::Relaxed) > keep {
            let Some(k) = classes.oldest() else { break };
            let (_, victim) = classes.q[k].pop_front().expect("a front");
            self.bytes.fetch_sub(victim.len(), Ordering::Relaxed);
            freed += victim.len();
            self.evictions.fetch_add(1, Ordering::Relaxed);
            doomed.push(victim); // alloc: Cold
        }
        drop(classes);
        drop(doomed);
        freed
    }

    /// P3 (`parked_trim_rung`), R2p's reach over the parked regions: what `trim_account_but_newest` could take off their
    /// account now, every parked region's account bytes (`RegionBuf::account_bytes`) but the newest's (the next statement
    /// takes it first). Never their lengths: a mapping's unwritten or purged pages and a heap region's given-back ones are on
    /// no account and free nothing. 0 in the ladder thread's pass while another thread holds the depot (D4398).
    pub fn trimmable_account_bytes(&self) -> usize {
        let Some(classes) = memledger::ladder_pass::lock_or_skip(&self.classes) else { return 0 };
        let newest = classes.newest();
        classes.q.iter().flat_map(|q| q.iter()).filter(|e| Some(e.0) != newest).map(|e| e.1.account_bytes()).sum()
    }

    /// P3, R2p's trim: the least recently put regions dropped, the newest kept, until `want` of their account bytes are
    /// (`trimmable_account_bytes`' measure). The account bytes the drops take off; a large mapped one goes to the reaper.
    /// Nothing in the ladder thread's pass while another thread holds the depot (D4398).
    pub fn trim_account_but_newest(&self, want: usize) -> usize {
        let mut freed = 0;
        // one region a turn, dropped after the lock (its drop gives its pages back, or hands a large mapped one to the
        // reaper), so the trim keeps no list of its own
        while freed < want {
            let victim = {
                let Some(mut classes) = memledger::ladder_pass::lock_or_skip(&self.classes) else { break };
                if classes.entries() <= 1 {
                    break;
                }
                let Some(k) = classes.oldest() else { break };
                let (_, v) = classes.q[k].pop_front().expect("a front");
                self.bytes.fetch_sub(v.len(), Ordering::Relaxed);
                self.evictions.fetch_add(1, Ordering::Relaxed);
                v
            };
            freed += victim.account_bytes();
            drop(victim);
        }
        freed
    }

    /// An empty depot of its own (the tests that read one depot's numbers while other tests park in `REGIONS`).
    #[cfg(test)]
    pub(crate) fn fresh() -> RegionDepot {
        RegionDepot { classes: Mutex::new(Classes { q: [const { VecDeque::new() }; REGION_CLASSES], clock: 0 }), bytes: AtomicUsize::new(0), takes: AtomicU64::new(0), puts: AtomicU64::new(0), allocs: AtomicU64::new(0), evictions: AtomicU64::new(0), beside: AtomicU64::new(0) }
    }

    /// The tests' read of the parked regions' charges: each one's committed bytes, as its move to PARKED's account booked
    /// them (`RegionBuf::parked`, `Charge::transfer`).
    #[cfg(test)]
    pub(crate) fn charged_bytes(&self) -> u64 {
        let classes = self.classes.lock().unwrap_or_else(|p| p.into_inner());
        classes.q.iter().flat_map(|q| q.iter()).map(|e| e.1.charge_committed()).sum()
    }

    /// Every parked region dropped (the grant pool under pressure, `GrantPool::admit`; an idle backend's release), their
    /// pool bytes back when it returns: a large mapped one went to the reaper (`mem::reap`), which it waits for.
    pub fn trim(&self) -> usize {
        // H3 step (4): no wait for the give-back service's jobs (`giveback::drain_here`); past the cancel line its whole
        // gives queued before this trim are given back here first, its park jobs stay the service's (each owns its region)
        crate::mem::giveback::drain_here(false);
        let freed = self.trim_handed();
        crate::mem::reap::drain_reaper();
        freed
    }

    /// `trim` without its wait for the reaper (D4098: a draw inside a kernel call waits for it polled, `GrantPool::take_wait`).
    pub(crate) fn trim_handed(&self) -> usize {
        let mut classes = self.classes.lock().unwrap_or_else(|p| p.into_inner());
        let mut freed = 0;
        for class in classes.q.iter_mut() {
            for (_, buf) in class.drain(..) {
                freed += buf.len();
                self.evictions.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.bytes.fetch_sub(freed, Ordering::Relaxed);
        drop(classes);
        freed
    }

    /// RULE 7: `f` parks a statement's regions at its end (`Entry::release`) while its reservation is still held: the park
    /// budget (half the pool) leaves out the `room` of it the statement never drew, which goes back with G a moment later
    /// (or is drawn again at the same statement's next open); statements parking at once leave out their rooms' sum.
    /// Counted, a reservation past half the pool dropped every region its statement put back and the next run mapped them
    /// fresh: CB9 W8 under DR-12's parallel-owner reservation dropped 8 of 11 regions a run, 189-252 k minor faults a warm
    /// run against 16-59 k parked (trace, box 18).
    pub fn parking_beside<R>(&self, room: u64, f: impl FnOnce() -> R) -> R {
        struct Beside<'a>(&'a AtomicU64, u64);
        impl Drop for Beside<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(self.1, Ordering::Relaxed);
            }
        }
        self.beside.fetch_add(room, Ordering::Relaxed);
        let _beside = Beside(&self.beside, room);
        f()
    }

    /// Park `buf` (an empty one is nothing). Over the grant pool's park budget the least recently put regions drop first
    /// (D405: dropping `buf` instead kept earlier statements' classes parked and threw the running statement's own sizes
    /// away at every put: a regexp_replace text group-by after a run of other analytic statements mapped 86.6 GiB fresh over 7 statements, 37.6 alone);
    /// `buf` drops only when the pool stays over the budget with nothing parked. Over the cap the least recently put regions
    /// other than `buf` drop (D2869: dropping the largest classes threw away the statement just run's own regions when they
    /// were the largest: after another statement's ~3 GiB, a W8 statement mapped ~1 GiB of fresh huge pages every run, ~50
    /// MiB run first).
    pub fn put(&self, buf: RegionBuf) {
        if buf.is_empty() {
            return;
        }
        // H3 step (4) C1-c: what its fold committed past the share goes back on the give-back service's thread, which
        // parks it after (`giveback::park`); a put beside a statement's reservation (`parking_beside`) stays here, as its
        // budget reads that reservation now
        let mut buf = if self.beside.load(Ordering::Relaxed) == 0 {
            match crate::mem::giveback::park(buf) {
                Ok(()) => return,
                Err(b) => b,
            }
        } else {
            buf
        };
        // option c': a reservation parks with its share only: what its fold committed past the share goes back first (pages
        // to the kernel, holds released), so a later taker never holds those pages uncounted; only `take_reserved` takes it
        buf.uncommit();
        buf.settle();
        self.insert(buf);
    }

    /// `put`'s insertion of a settled region, onto the depot's account, under the park budget and the cap. A park job's
    /// region (`giveback::park`) is the depot's since its put (E2): `parked` moves nothing then.
    pub(crate) fn insert(&self, mut buf: RegionBuf) {
        buf.parked();
        let pool = &crate::mem::grant::POOL;
        // the park budget (`GrantPool::over_park_budget`) without the parking statement's unspent reservation (`parking_beside`)
        let beside = self.beside.load(Ordering::Relaxed);
        let over = || pool.capacity() != 0 && pool.held().saturating_sub(beside) > pool.capacity() / 2;
        if crate::node::region_debug() & crate::node::TRACE != 0 {
            eprintln!("gravity: region: depot put {} (over park budget {})", buf.len(), over());
        }
        let c = park_class_of(buf.len());
        let len = buf.len();
        let mut classes = self.classes.lock().unwrap_or_else(|p| p.into_inner());
        if over() {
            // every parked region is pool-held (settled): evicting all of them is the most a put can free
            let parked = self.bytes.load(Ordering::Relaxed) as u64;
            if pool.held().saturating_sub(parked + beside) > pool.capacity() / 2 {
                drop(classes);
                self.evictions.fetch_add(1, Ordering::Relaxed);
                drop(buf);
                return;
            }
            let mut lru = 0u64;
            while over() {
                let Some(k) = classes.oldest() else { break };
                let (_, victim) = classes.q[k].pop_front().expect("a front");
                self.bytes.fetch_sub(victim.len(), Ordering::Relaxed);
                drop(victim);
                lru += 1;
            }
            self.evictions.fetch_add(lru, Ordering::Relaxed);
            if crate::node::region_debug() & crate::node::TRACE != 0 {
                eprintln!("gravity: region: depot put {len} evicted {lru} least recently put (over park budget {})", over());
            }
            if over() {
                drop(classes);
                self.evictions.fetch_add(1, Ordering::Relaxed);
                drop(buf);
                return;
            }
        }
        classes.clock += 1;
        let stamp = classes.clock;
        classes.q[c].push_back((stamp, buf)); // alloc: Cold
        self.puts.fetch_add(1, Ordering::Relaxed);
        // `buf` is the newest region: `oldest` reaches its class's front only when it is the one region left
        let mut bytes = self.bytes.fetch_add(len, Ordering::Relaxed) + len;
        let cap = region_depot_bytes();
        while bytes > cap && classes.entries() > 1 {
            let Some(k) = classes.oldest() else { break };
            let freed = classes.q[k].pop_front().map_or(0, |e| e.1.len());
            bytes = self.bytes.fetch_sub(freed, Ordering::Relaxed) - freed;
            self.evictions.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn stats(&self) -> RegionDepotStats {
        let classes = self.classes.lock().unwrap_or_else(|p| p.into_inner());
        RegionDepotStats { entries: classes.entries(), bytes: self.bytes.load(Ordering::Relaxed), takes: self.takes.load(Ordering::Relaxed), puts: self.puts.load(Ordering::Relaxed), allocs: self.allocs.load(Ordering::Relaxed), evictions: self.evictions.load(Ordering::Relaxed) }
    }

    /// The parked regions' bytes (each pool-held), without the lock `stats` takes.
    pub fn parked_bytes(&self) -> usize {
        self.bytes.load(Ordering::Relaxed)
    }

    /// Drop every parked region (the tests; a process never needs it). D4587: their bytes leave the count as a trim's do
    /// (every change to the count is under the lock, so it is exactly what was parked).
    pub fn clear(&self) {
        let mut classes = self.classes.lock().unwrap_or_else(|p| p.into_inner());
        let mut freed = 0;
        for class in classes.q.iter_mut() {
            freed += class.iter().map(|e| e.1.len()).sum::<usize>();
            class.clear();
        }
        self.bytes.fetch_sub(freed, Ordering::Relaxed);
    }
}
