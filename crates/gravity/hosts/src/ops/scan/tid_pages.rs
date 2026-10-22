//! A list of heap TIDs read as the heap range source reads a bitmap's exact pages (`HeapRangeSource` with a filter whose
//! blocks are the TIDs' blocks in order): per block, the visible tuples whose HOT chain root is a listed offset
//! (PostgreSQL's heap_hot_search_buffer from an index's TID). Shared by a worker's index probe (index_probe.rs: one
//! probe's TIDs) and the pool's index-only leaf walk (index_leaves.rs: the entries the visibility map does not vouch for).
//! An ordered reader (index_ranges.rs) leaves the list in index order unsorted: each staged block is then a run of
//! consecutive TIDs on one block, and `keep_page_listed` hands its tuples on in the run's order.
use types_core::{BlockNumber, InvalidTransactionId, TransactionId, TransactionIdIsValid, TransactionIdPrecedes};
use types_error::PgResult;
use types_storage::bufpage::{MaxHeapTuplesPerPage, PageRef};
use types_tuple::htup::{HeapTupleHeaderData, HEAP_XMAX_COMMITTED, HEAP_XMAX_INVALID, HEAP_XMAX_IS_LOCKED_ONLY, HEAP_XMAX_IS_MULTI};
use types_tuple::itemptr::{FirstOffsetNumber, ItemPointerData, ItemPointerGetBlockNumber, ItemPointerGetOffsetNumber};

pub struct TidPages {
    /// memsig E-pub t3: charged to the owner of the window that fills it (`set_owner`), as `order`'s sort scratch is;
    /// uncharged until a window names one.
    tids: memledger::ChargedVec<ItemPointerData>,
    /// The next TID of `tids` to stage.
    at: usize,
    /// The staged block's listed offsets.
    words: [u64; MaxHeapTuplesPerPage.div_ceil(64)],
    roots: Box<[u16; MaxHeapTuplesPerPage]>,
    /// The staged run's TIDs, `tids[run.0..run.1]`.
    run: (usize, usize),
    /// `keep`'s roots are the page's (`heap_get_root_tuples` ran): a kept offset's root is `roots[off - 1]`, else itself.
    walked: bool,
    /// The owner `order`'s sort scratch is charged to (`set_owner`).
    owner: crate::ops::scan::heap_source::HeldOwner,
}

impl Default for TidPages {
    fn default() -> TidPages {
        TidPages::new()
    }
}

impl TidPages {
    pub fn new() -> TidPages {
        TidPages { tids: memledger::ChargedVec::none(), at: 0, words: [0; MaxHeapTuplesPerPage.div_ceil(64)], roots: Box::new([0; MaxHeapTuplesPerPage]), run: (0, 0), walked: false, owner: None } // alloc: Node
    }

    /// The list, emptied for a new fill (its capacity kept).
    pub fn clear(&mut self) {
        self.tids.clear();
        self.at = 0;
    }

    /// CONVERT wave 2: the owner of the window that fills the list (the scan's breaker, else the statement). memsig E-pub
    /// t3: the list moves to it too; a window names it before it fills, so the move finds the list empty (a list a
    /// window already filled is copied over once).
    pub fn set_owner(&mut self, owner: crate::ops::scan::heap_source::HeldOwner) {
        self.owner = owner;
        charged_onto(&mut self.tids, owner);
    }

    /// One listed TID (then `order`), on the list's charge.
    pub fn push(&mut self, tid: ItemPointerData) {
        self.tids.with_vec(|v| v.push(tid)); // alloc: Node charged owner
    }

    /// The list, taken for a filler that needs it apart from the pages (a probe's `rel.probe`), then put back (`put_tids`).
    pub fn take_tids(&mut self) -> memledger::ChargedVec<ItemPointerData> {
        core::mem::take(&mut self.tids)
    }

    pub fn put_tids(&mut self, tids: memledger::ChargedVec<ItemPointerData>) {
        self.tids = tids;
    }

    pub fn len(&self) -> usize {
        self.tids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tids.is_empty()
    }

    /// The filled list in block order, staged from its first TID: sorted under `step` (a probe's list is as long as its
    /// matches). `Err`: the statement's cancel, the list in no order.
    pub fn order(&mut self, step: &mut cancelpoll::Step) -> Result<(), cancelpoll::Canceled> {
        self.at = 0;
        let key = |t: &ItemPointerData| (ItemPointerGetBlockNumber(t), ItemPointerGetOffsetNumber(t));
        let (n, mut scratch) = (self.tids.len(), crate::ops::scan::heap_source::charged_scratch(self.owner));
        if n > cancelpoll::SORT_RUN && scratch.resize(n, ItemPointerData::default()).is_err() {
            // a refused scratch: the list sorted in place, polled as the merge is
            return heap_sort(step, self.tids.as_mut_slice(), key);
        }
        cancelpoll::sort_by(step, self.tids.as_mut_slice(), scratch.as_mut_slice(), |a, b| key(a).cmp(&key(b)))
    }

    /// The next block holding a listed TID, its offsets staged; `None` past the last. A block past the relation's end at
    /// the bind (`nblocks`) holds no tuple the snapshot sees (as a bitmap's) and is skipped.
    pub fn next_block(&mut self, nblocks: BlockNumber) -> PgResult<Option<BlockNumber>> {
        // a skipped block is the unit: a relation extended after the bind can leave any number of listed blocks past its end
        let mut step = cancelpoll::step("tid_pages next_block: a listed block past the relation's end skipped");
        while self.at < self.tids.len() {
            let blk = ItemPointerGetBlockNumber(&self.tids[self.at]);
            self.words = [0; MaxHeapTuplesPerPage.div_ceil(64)];
            self.run.0 = self.at;
            // bounded("small-fixed: one block's listed TIDs (an index holds a TID once: at most MaxHeapTuplesPerPage, 291 at 8 K pages), a bit set each")
            while self.at < self.tids.len() && ItemPointerGetBlockNumber(&self.tids[self.at]) == blk {
                let off = ItemPointerGetOffsetNumber(&self.tids[self.at]) as usize;
                if (1..=MaxHeapTuplesPerPage).contains(&off) {
                    self.words[(off - 1) / 64] |= 1 << ((off - 1) % 64);
                }
                self.at += 1;
            }
            self.run.1 = self.at;
            if blk < nblocks {
                return Ok(Some(blk));
            }
            step.tick().map_err(|_| crate::pg_err("gravity: a TID list's walk stopped for its statement's cancel"))?;
        }
        Ok(None)
    }

    /// The visible tuples `vis` of the staged block's pinned, share-locked `page` whose HOT chain root is a listed offset,
    /// compacted to the front (bitmap_morsel.rs's exact page): when every listed offset is a visible tuple's own, those
    /// are the answer and the roots are not walked.
    pub fn keep_page(&mut self, page: PageRef<'_>, vis: &mut [u16]) -> PgResult<usize> {
        self.keep(page, vis, None)
    }

    /// `keep_page`, and each listed offset of block `blk` whose HOT chain has no member the snapshot sees and is all dead
    /// before `horizon` (`chain_all_dead`) into `dead` (up to `cap` TIDs in all): the index-only leaf walk's dead entries
    /// (index_leaves). Only the walk of the roots finds one: when every listed offset is a visible tuple's own, there is none.
    pub fn keep_page_dead(&mut self, blk: BlockNumber, page: PageRef<'_>, vis: &mut [u16], horizon: TransactionId, dead: &mut Vec<ItemPointerData>, cap: usize) -> PgResult<usize> {
        self.keep(page, vis, Some((blk, horizon, dead, cap)))
    }

    /// `keep_page` (or `keep_page_dead` with `dead`) over an unsorted list's staged run, the kept tuples in the run's
    /// order (each its listed TID's chain member) rather than the page's: an ordered reader's heap fetches.
    pub fn keep_page_listed(&mut self, page: PageRef<'_>, vis: &mut [u16], dead: Option<(BlockNumber, TransactionId, &mut Vec<ItemPointerData>, usize)>) -> PgResult<usize> {
        let n = self.keep(page, vis, dead)?;
        if n > 1 {
            // each kept tuple at its root's place in the run (a TID is listed once: an index holds a TID once)
            let (run, walked) = (self.run, self.walked);
            let mut at = [u16::MAX; MaxHeapTuplesPerPage];
            // bounded("small-fixed: the staged block's listed TIDs (an index holds a TID once: at most MaxHeapTuplesPerPage, 291 at 8 K pages), a store each")
            for (i, t) in self.tids[run.0..run.1].iter().enumerate() {
                let off = ItemPointerGetOffsetNumber(t) as usize;
                if (1..=MaxHeapTuplesPerPage).contains(&off) {
                    at[off - 1] = i as u16;
                }
            }
            let roots = &self.roots;
            // bounded("small-fixed: one page's kept line pointers, at most MaxHeapTuplesPerPage (291 at 8 K pages), sorted by a table lookup")
            vis[..n].sort_unstable_by_key(|&off| at[(if walked { roots[off as usize - 1] } else { off }) as usize - 1]);
        }
        Ok(n)
    }

    fn keep(&mut self, page: PageRef<'_>, vis: &mut [u16], dead: Option<(BlockNumber, TransactionId, &mut Vec<ItemPointerData>, usize)>) -> PgResult<usize> {
        self.walked = false;
        let words = self.words;
        let has = |off: u16| off >= 1 && (off as usize) <= MaxHeapTuplesPerPage && words[(off as usize - 1) / 64] >> ((off as usize - 1) % 64) & 1 != 0;
        let set: u32 = words.iter().map(|w| w.count_ones()).sum();
        if vis.iter().filter(|&&o| has(o)).count() as u32 == set {
            let mut n = 0;
            // bounded("small-fixed: one page's visible line pointers, at most MaxHeapTuplesPerPage (291 at 8 K pages), a bit test each")
            for i in 0..vis.len() {
                if has(vis[i]) {
                    vis[n] = vis[i];
                    n += 1;
                }
            }
            return Ok(n);
        }
        pruneheap::heap_get_root_tuples(page, &mut self.roots)?;
        self.walked = true;
        let mut left = words;
        let mut n = 0;
        // bounded("small-fixed: one page's visible line pointers, at most MaxHeapTuplesPerPage (291 at 8 K pages), a bit test each")
        for i in 0..vis.len() {
            let off = vis[i];
            let root = self.roots[off as usize - 1];
            if root != 0 && has(root) {
                vis[n] = off;
                n += 1;
                left[(root as usize - 1) / 64] &= !(1 << ((root as usize - 1) % 64));
            }
        }
        if let Some((blk, horizon, out, cap)) = dead {
            // bounded("small-fixed: the page's listed-offset words, MaxHeapTuplesPerPage / 64 rounded up (5 at 8 K pages)")
            for (w, &bits) in left.iter().enumerate() {
                let mut bits = bits;
                // bounded("const-domain: a word's 64 bits, one chain walk per set bit")
                while bits != 0 && out.len() < cap {
                    let off = (w * 64 + bits.trailing_zeros() as usize + 1) as u16;
                    if chain_all_dead(page, off, horizon) {
                        out.push(ItemPointerData::new(blk, off)); // alloc: Node
                    }
                    bits &= bits - 1;
                }
            }
        }
        Ok(n)
    }
}

/// heapam's `heap_hot_search_buffer` all-dead answer for the HOT chain an index entry's TID `root` names on the pinned,
/// share-locked `page`, none of whose members the snapshot sees: every member it walks is surely dead before `horizon`
/// (`surely_dead`). As there, a TID past the page's line pointers, at an unused or dead one, or at a heap-only tuple
/// (another chain's member: the root was pruned away) is all dead, and the walk ends where the chain breaks (an
/// out-of-page link, a line pointer not normal, an xmin that is not the previous member's xmax). A walk longer than the
/// page's line pointers (a cycle) is not dead.
fn chain_all_dead(page: PageRef<'_>, root: u16, horizon: TransactionId) -> bool {
    let maxoff = page.max_offset_number();
    let (mut off, mut at_start, mut prev_xmax) = (root, true, InvalidTransactionId);
    // bounded("small-fixed: one HOT chain's members, at most the page's line pointers plus one (MaxHeapTuplesPerPage, 291 at 8 K pages)")
    for _ in 0..=maxoff as usize {
        if off < FirstOffsetNumber || off > maxoff {
            return true;
        }
        let lp = page.item_id(off);
        if !lp.is_normal() {
            if lp.is_redirected() && at_start {
                off = lp.lp_off();
                at_start = false;
                continue;
            }
            return true;
        }
        let Some(h) = crate::ops::scan::raw::heap_header(page, lp) else { return false };
        if (at_start && h.is_heap_only()) || (TransactionIdIsValid(prev_xmax) && prev_xmax != h.xmin()) {
            return true;
        }
        if !surely_dead(h, horizon) {
            return false;
        }
        if !h.is_hot_updated() {
            return true;
        }
        off = ItemPointerGetOffsetNumber(&h.t_ctid);
        at_start = false;
        // a surely dead member's xmax is no MultiXact: its raw xmax is the updater
        prev_xmax = h.xmax_raw();
    }
    false
}

/// heapam_visibility's `HeapTupleIsSurelyDead` from the hint bits alone, against a horizon taken once on the leader
/// (`GetOldestNonRemovableTransactionId`: a horizon only moves forward, so an older one is conservative): the insert
/// aborted, or a committed delete (not a lock, not a MultiXact) by a transaction before `horizon`.
fn surely_dead(h: &HeapTupleHeaderData, horizon: TransactionId) -> bool {
    if !h.xmin_committed() {
        return h.xmin_invalid();
    }
    let m = h.t_infomask;
    if m & HEAP_XMAX_INVALID != 0 || HEAP_XMAX_IS_LOCKED_ONLY(m) || m & HEAP_XMAX_IS_MULTI != 0 || m & HEAP_XMAX_COMMITTED == 0 {
        return false;
    }
    TransactionIdPrecedes(h.xmax_raw(), horizon)
}

/// `v` in `key` order with no scratch (a heapsort), a tick per element placed: `TidPages::order` when the merge's scratch
/// is refused. `Err`: the statement's cancel, `v` in no order (as `cancelpoll::sort_by`).
/// memsig E-pub t3: a window's scratch list on `owner` (a `HeldOwner`, the window's): nothing when it is already its; else
/// its items, if any, copied once into a list charged to `owner` (an empty list just changes hands).
pub(crate) fn charged_onto<T: Copy>(v: &mut memledger::ChargedVec<T>, owner: crate::ops::scan::heap_source::HeldOwner) {
    if v.owner() == owner.map(|o| o.0) {
        return;
    }
    let mut to = crate::ops::scan::heap_source::charged_scratch(owner);
    if !v.is_empty() {
        to.with_vec(|t| t.extend_from_slice(v)); // alloc: Node charged owner
    }
    *v = to;
}

fn heap_sort<K: Ord>(step: &mut cancelpoll::Step, v: &mut [ItemPointerData], key: impl Fn(&ItemPointerData) -> K) -> Result<(), cancelpoll::Canceled> {
    let sift = |v: &mut [ItemPointerData], mut root: usize, end: usize| {
        // bounded("small-fixed: one sift-down, a level a pass: at most log2 of the list's length (64) levels")
        loop {
            let mut child = 2 * root + 1;
            if child >= end {
                break;
            }
            if child + 1 < end && key(&v[child]) < key(&v[child + 1]) {
                child += 1;
            }
            if key(&v[root]) >= key(&v[child]) {
                break;
            }
            v.swap(root, child);
            root = child;
        }
    };
    let n = v.len();
    for (k, i) in (0..n / 2).rev().enumerate() {
        step.batch(k)?;
        sift(v, i, n);
    }
    for (k, end) in (1..n).rev().enumerate() {
        step.batch(k)?;
        v.swap(0, end);
        sift(v, 0, end);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The refused-scratch order (`heap_sort`) puts a long probe list in the merge's block order.
    #[test]
    fn heap_sort_orders_a_list_as_the_merge_does() {
        let key = |t: &ItemPointerData| (ItemPointerGetBlockNumber(t), ItemPointerGetOffsetNumber(t));
        let tid = |i: u32| {
            let mut t = ItemPointerData::default();
            types_tuple::itemptr::ItemPointerSet(&mut t, i.wrapping_mul(7919) % 5000, (i % 291 + 1) as u16);
            t
        };
        let mut a: Vec<ItemPointerData> = (0..(cancelpoll::SORT_RUN as u32 + 1000)).map(tid).collect(); // alloc: Cold
        let mut b = a.clone();
        let mut scratch = vec![ItemPointerData::default(); b.len()]; // alloc: Cold
        heap_sort(&mut cancelpoll::step("test: heap sort"), &mut a, key).unwrap();
        cancelpoll::sort_by(&mut cancelpoll::step("test: merge sort"), &mut b, &mut scratch, |x, y| key(x).cmp(&key(y))).unwrap();
        assert!(a.iter().map(key).eq(b.iter().map(key)), "the same order");
        assert!(a.windows(2).all(|w| key(&w[0]) <= key(&w[1])));
    }
}
