//! Block → shared-buffer hints per relation file, process-wide: where a page was last pinned, for `read_recent_buffer` to verify.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;

use pgsync::Mutex;

use types_core::{BlockNumber, Buffer, Oid};
use types_storage::{RelFileLocator, RelFileLocatorBackend};

// Every array's entries together: the cost home's floor (rule R11; construct bundle scan-07), grown to the pool (`budget`).
use gravity_program::cost::PAGE_HINTS_BUDGET as BUDGET;
/// Blocks an array grows past `nblocks` so a relation that is still growing is not re-copied at each bind.
/// ARBITRARY-SAFE (growth headroom, 4 KiB per array): too small costs one re-copy per bind, never an answer.
const SLACK: usize = 1024;

struct Table {
    charge: memledger::Charge,
    arrays: HashMap<RelFileLocator, Arc<[AtomicI32]>>,
    entries: usize,
}

impl Table {
    fn bytes(&self) -> u64 {
        self.bytes_with(self.arrays.len(), self.entries)
    }

    /// `bytes` with `len` arrays of `entries` entries together in the map's capacity.
    fn bytes_with(&self, len: usize, entries: usize) -> u64 {
        let slot = core::mem::size_of::<(RelFileLocator, Arc<[AtomicI32]>)>() + 1;
        (self.arrays.capacity() * slot + len * array_bytes(0) + entries * core::mem::size_of::<AtomicI32>()) as u64
    }

    /// D4635: the arrays of the relation files `gone` names let go. The charge refits down for them first (their bytes
    /// leave the holder for this thread's account), then they leave the map: each frees here, on that account, unless a
    /// scan still holds it (then at its last drop, as a replaced array does).
    fn forget_where(&mut self, gone: impl Fn(&RelFileLocator) -> bool) {
        let (n, entries) = self.arrays.iter().filter(|(l, _)| gone(l)).fold((0, 0), |(n, e), (_, a)| (n + 1, e + a.len()));
        if n == 0 {
            return;
        }
        self.entries -= entries;
        let bytes = self.bytes_with(self.arrays.len() - n, self.entries);
        self.charge.fit_committed(bytes);
        self.arrays.retain(|l, _| !gone(l));
    }

    fn refit(&mut self) {
        const OWNER: memledger::Owner = memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::SharedBuffers));
        if self.charge.owner() != OWNER {
            self.charge = memledger::Charge::open(OWNER, memledger::charge::release_of(OWNER), memledger::Kind::Region, memledger::charge::Door::Heap, 0);
        }
        let bytes = self.bytes();
        self.charge.fit_committed(bytes);
    }
}

fn array_bytes(len: usize) -> usize {
    (2 * core::mem::size_of::<usize>() + len * core::mem::size_of::<AtomicI32>()).next_multiple_of(core::mem::size_of::<usize>())
}

static TABLE: Mutex<Option<Table>> = Mutex::new(None);

/// One relation file's hints, shared by every source over it; an entry is a guess the reader verifies by tag (`read_recent_buffer`).
#[derive(Clone)]
pub struct PageHints(Arc<[AtomicI32]>);

/// H42 (G, capacities grow): every array's entries together, the cost home's floor or the pool's size, whichever is larger,
/// so the table always holds one pool-sized array: a relation past 8 Mi blocks in a pool past 8 Mi buffers (64 GiB) is
/// hinted too, where a fixed budget left it none. 4 B an entry against 8 KiB a buffer: at most 1/2048 of the pool's bytes
/// past the floor. A hint is a guess the reader verifies by tag, so the size moves no answer.
fn budget(nbuffers: Buffer) -> usize {
    BUDGET.max(nbuffers.max(0) as usize)
}

/// D4635: the hints of relation files whose storage is about to be unlinked (bufmgr `DropRelationsAllBuffers`: a dropped or
/// rewritten relation's old file at its commit, a created one's at its abort): a relation that comes back reads another
/// file, so nothing reads them again.
pub fn forget(rels: &[RelFileLocatorBackend]) {
    if rels.is_empty() {
        return;
    }
    let mut guard = TABLE.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(t) = guard.as_mut() {
        t.forget_where(|l| rels.iter().any(|r| r.locator == *l));
    }
}

/// D4635: every hint of database `db` (bufmgr `DropDatabaseBuffers`: DROP DATABASE, a database's move to another
/// tablespace).
pub fn forget_db(db: Oid) {
    let mut guard = TABLE.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(t) = guard.as_mut() {
        t.forget_where(|l| l.dbOid == db);
    }
}

/// D4635: `forget` and `forget_db` on bufmgr's drop hooks (the process's seams, once).
pub fn install_forget() {
    bufmgr::set_dropped_hooks(forget, forget_db);
}

impl PageHints {
    /// The array covering `nblocks` (capped at the pool's size, so always within `budget`): once per bind, never per page.
    pub fn of(locator: RelFileLocator, nblocks: BlockNumber, nbuffers: Buffer) -> Option<PageHints> {
        let want = (nblocks as usize).min(nbuffers.max(0) as usize);
        let budget = budget(nbuffers);
        if want == 0 || want > budget {
            return None;
        }
        let mut guard = TABLE.lock().unwrap_or_else(|p| p.into_inner());
        let t = guard.get_or_insert_with(|| Table { arrays: HashMap::new(), entries: 0, charge: memledger::Charge::NONE });
        if let Some(a) = t.arrays.get(&locator) {
            if a.len() >= want {
                return Some(PageHints(a.clone()));
            }
        }
        let len = (want + SLACK).min(budget);
        let old = t.arrays.remove(&locator);
        t.entries -= old.as_ref().map_or(0, |a| a.len());
        if t.entries + len > budget {
            t.forget_where(|_| true);
        }
        let fresh: Arc<[AtomicI32]> = (0..len).map(|i| AtomicI32::new(old.as_ref().and_then(|a| a.get(i)).map_or(0, |h| h.load(Ordering::Relaxed)))).collect(); // alloc: Exec charged shared buffers (Table::refit)
        t.arrays.insert(locator, fresh.clone()); // alloc: Exec charged shared buffers (Table::refit)
        t.entries += len;
        t.refit();
        Some(PageHints(fresh))
    }

    /// 0 = none.
    #[inline]
    pub fn get(&self, blk: BlockNumber) -> Buffer {
        self.0.get(blk as usize).map_or(0, |h| h.load(Ordering::Relaxed))
    }

    #[inline]
    pub fn set(&self, blk: BlockNumber, buf: Buffer) {
        if let Some(h) = self.0.get(blk as usize) {
            h.store(buf, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// H42: the budget is the cost home's floor up to a pool of that many buffers, the pool's size past it; a relation as
    /// large as such a pool still gets its whole array (a fixed budget refused it).
    #[test]
    fn budget_grows_with_the_pool() {
        assert_eq!(budget(16_384), BUDGET, "a 128 MiB pool: the floor");
        assert_eq!(budget(BUDGET as Buffer), BUDGET);
        assert_eq!(budget(-1), BUDGET);
        let big = (BUDGET + 4096) as Buffer;
        assert_eq!(budget(big), BUDGET + 4096, "a pool past 64 GiB: its own size");
        let h = PageHints::of(RelFileLocator::new(1663, 5, 900_001), BUDGET as BlockNumber + 100, big).expect("a pool-sized array");
        assert_eq!(h.0.len(), BUDGET + 100 + SLACK, "nblocks + SLACK, under the budget");
        h.set(BUDGET as BlockNumber + 99, 7);
        assert_eq!(h.get(BUDGET as BlockNumber + 99), 7);
        assert!(PageHints::of(RelFileLocator::new(1663, 5, 900_002), 0, big).is_none(), "an empty relation: none");
    }

    #[test]
    fn the_tables_map_and_arrays_are_on_shared_buffers() {
        let loc = RelFileLocator::new(1663, 5, 900_101);
        let h = PageHints::of(loc, 64, 16_384).expect("hints");
        let g = TABLE.lock().unwrap_or_else(|p| p.into_inner());
        let t = g.as_ref().expect("the table");
        assert_eq!(t.charge.owner(), memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::SharedBuffers)));
        assert_eq!(t.charge.committed(), t.bytes(), "refit at the insert");
        if t.arrays.contains_key(&loc) {
            assert!(t.bytes() >= (array_bytes(0) + (64 + SLACK) * 4) as u64, "the array is in it: {}", t.bytes());
        }
        drop(g);
        drop(h);
    }
}
