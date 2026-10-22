//! The bitmap heap scan as a Gravity morsel source. The leader builds the bitmap once per execution
//! (exec runner plan.rs `BitmapRun`, through `BitmapMorselRecipe::build`: the serial source's own build, `BitmapSource::freeze`)
//! and freezes it (`TIDBitmap::prepare_shared_iterate`, PostgreSQL's parallel bitmap leader's step); a claim is a range of
//! the frozen entries (`TbmRangeIterator`: the exact pages in block order, then the lossy chunks), walked by the heap range
//! source (`HeapRangeSource<BitmapFilter>`: relcache-free page reads on any pool thread, the snapshot's visibility, D865's
//! deferred prune, the SSI rule) whose filter picks the entries' blocks and keeps, on an exact page, each visible tuple
//! whose HOT chain's root line pointer the bitmap holds (PostgreSQL's heap_hot_search_buffer from the index's TID), on a
//! lossy page every visible tuple. No recheck here: a lossy page, or an exact page an AM marked for recheck (GIN, BRIN,
//! hash), publishes `site::PAGE_RECHECK` on its window, and the morsel form's filter tests `bitmapqualorig` on exactly
//! those windows (lowerv4 step/filter.rs `guard`), as PostgreSQL's bitmap heap scan rechecks.
use std::rc::Rc;
use std::sync::Arc;
use pgsync::Mutex;

use gravity_abi::Datum as Word;
use gravity_driver::{box_source, ArenaRef, SourceBox};
use gravity_program::SiteDesc;
use tidbitmap::{TbmRangeIterator, TbmSharedIterState};
use types_core::{BlockNumber, Oid};
use types_error::PgResult;
use types_snapshot::SnapshotData;
use types_storage::bufpage::{MaxHeapTuplesPerPage, PageRef};

use crate::ops::scan::bitmap_source::BitmapSource;
use crate::ops::scan::bitmap_window::{Tree, TreeRecipe};
use crate::ops::scan::heap_source::{HeapRangeSource, RangeRecipe, SampleFilter};
use crate::ops::scan::index_source::SiteRecipe;
use crate::recipe::HostRecipe;

/// The bitmap the build wants: one index's, or a BitmapAnd / BitmapOr tree's.
#[derive(Clone)]
pub enum MorselBitmap {
    Leaf(SiteRecipe),
    Tree(TreeRecipe),
}

/// This execution's frozen bitmap, shared by the claims (leader) and every worker's filter.
pub type FrozenSlot = Arc<Mutex<Option<Arc<TbmSharedIterState>>>>;

/// The morsel form's recipe: the heap range recipe the walk deforms by, the bitmap to build, and the execution's frozen
/// slot (filed per execution by exec's BIND, so a cached program's next execution starts empty).
#[derive(Clone)]
pub struct BitmapMorselRecipe {
    pub range: RangeRecipe,
    pub bitmap: MorselBitmap,
    pub frozen: FrozenSlot,
    /// A2 entry 2: the build may walk a btree's leaves on pool threads (exec's bind: an uninstrumented execution; EXPLAIN
    /// ANALYZE keeps the leader walk, so its Heap Blocks exact / lossy counts are PostgreSQL's).
    pub wide_walk: bool,
}

impl BitmapMorselRecipe {
    pub fn new(range: RangeRecipe, bitmap: MorselBitmap) -> BitmapMorselRecipe {
        BitmapMorselRecipe { range, bitmap, frozen: Arc::new(Mutex::new(None)), wide_walk: false }
    }

    /// `self` whose build may walk a btree's leaves on pool threads (`wide_walk`).
    pub fn walking_wide(mut self, wide: bool) -> BitmapMorselRecipe {
        self.wide_walk = wide;
        self
    }

    /// The bitmap of key words `keys` (the site's `param_slots` read from this execution's words) under `snapshot`,
    /// frozen into the slot: `(entries, heap pages)` — the claim units, and the pages they expand to (an exact page one,
    /// a lossy chunk its set bits). Leader only (the index walk reads its relcache); `leaves`: the pool threads a btree's
    /// leaf walk may take (A2 entry 2, only with `wide_walk`; < 2: the leader walks).
    pub fn build(&self, snapshot: Rc<SnapshotData<'static>>, keys: &[Word], nulls: &[u8], leaves: u16) -> PgResult<(u64, u64)> {
        let mut src = match &self.bitmap {
            MorselBitmap::Leaf(r) => BitmapSource::new(r.clone(), None),
            MorselBitmap::Tree(t) => BitmapSource::new(t.leaves[0].clone(), None).with_tree(Tree::new(t.clone())),
        };
        let frozen = src.freeze(snapshot, keys, nulls, if self.wide_walk { leaves } else { 0 })?;
        let (units, pages) = frozen.as_ref().map_or((0, 0), |f| {
            let (exact, chunks) = f.entry_counts();
            (exact + chunks, f.expanded_pages())
        });
        *self.frozen.lock().unwrap_or_else(|p| p.into_inner()) = frozen;
        Ok((units, pages))
    }

    /// The execution's end: the frozen entries go (memory flat across executions, D2971's rule).
    pub fn release(&self) {
        *self.frozen.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }
}

impl HostRecipe for BitmapMorselRecipe {
    fn rel(&self) -> Oid {
        self.range.heap
    }
    fn size_hint(&self) -> usize {
        core::mem::size_of::<HeapRangeSource<BitmapFilter>>()
    }
    fn derive(&self, desc: &SiteDesc, a: ArenaRef) -> SourceBox {
        let counts = match &self.bitmap {
            MorselBitmap::Leaf(r) => Some(r.bitmap_arc()),
            MorselBitmap::Tree(_) => None,
        };
        box_source(HeapRangeSource::with_sample(self.range.clone(), BitmapFilter::new(self.frozen.clone(), counts)).walk(desc.walk), a)
    }
    fn clone_box(&self) -> Box<dyn HostRecipe> {
        Box::new(self.clone()) // alloc: Program
    }
    fn bitmap_morsel(&self) -> Option<&BitmapMorselRecipe> {
        Some(self)
    }
}

/// The heap walk's filter over one claim of the frozen entries.
pub struct BitmapFilter {
    frozen: FrozenSlot,
    iter: Option<TbmRangeIterator>,
    /// The current block's entry: its tuple bits (exact) or none (lossy).
    words: Option<[types_nodes::bitmapword; tidbitmap::TBM_WORDS]>,
    roots: Box<[u16; MaxHeapTuplesPerPage]>,
    /// The current block's entry asks for the recheck (lossy, or an AM's recheck flag): `PAGE_RECHECK` on its window.
    recheck: bool,
    /// A one-index bitmap's EXPLAIN ANALYZE counters (instr.rs: "Heap Blocks" exact / lossy), shared with the recipe.
    counts: Option<Arc<crate::instr::BitmapCounts>>,
}

impl BitmapFilter {
    pub fn new(frozen: FrozenSlot, counts: Option<Arc<crate::instr::BitmapCounts>>) -> BitmapFilter {
        BitmapFilter { frozen, iter: None, words: None, roots: Box::new([0; MaxHeapTuplesPerPage]), recheck: false, counts } // alloc: Node
    }
}

fn no_build() -> Box<types_error::PgError> {
    crate::pg_err("gravity: a bitmap morsel claim before its bitmap was built")
}

impl SampleFilter for BitmapFilter {
    const SPARSE: bool = true;
    const PAGE: bool = true;
    const BITMAP: bool = true;

    /// The claim `[keys[0], keys[1])` over the frozen entries (`MorselRange::ALL` on the width-1 arm: every entry).
    fn start(&mut self, keys: &[Word], keys_null: &[u8]) -> PgResult<()> {
        let st = self.frozen.lock().unwrap_or_else(|p| p.into_inner()).clone().ok_or_else(no_build)?;
        let (pages, chunks) = st.entry_counts();
        let total = pages + chunks;
        let (lo, hi) = if keys_null.first().is_some_and(|&n| n != 0) || keys_null.get(1).is_some_and(|&n| n != 0) { (0, 0) } else { (keys[0].min(total), keys[1].min(total)) };
        self.iter = Some(TbmRangeIterator::new(st, lo, hi.max(lo)));
        self.words = None;
        Ok(())
    }

    fn next_block(&mut self, nblocks: BlockNumber) -> PgResult<Option<BlockNumber>> {
        self.walk_block(nblocks)
    }

    fn keep(&mut self, _blk: BlockNumber, _maxoff: u16, vis: &mut [u16]) -> usize {
        vis.len()
    }

    fn page_flags(&self) -> u16 {
        if self.recheck { gravity_abi::site::site::PAGE_RECHECK } else { 0 }
    }

    /// An exact page: the visible tuples whose HOT chain root (`heap_get_root_tuples`) has its bit; a lossy page: all.
    /// Under an MVCC snapshot a chain has at most one visible member, so when every set bit is a visible tuple's own
    /// offset (no chain moved past its root: the common page) those tuples are the answer and the roots are not walked.
    fn keep_page(&mut self, _blk: BlockNumber, page: PageRef<'_>, vis: &mut [u16]) -> PgResult<usize> {
        let Some(words) = self.words else { return Ok(vis.len()) };
        let has = |off: u16| {
            let bit = off as usize - 1;
            words[bit / types_nodes::BITS_PER_BITMAPWORD] >> (bit % types_nodes::BITS_PER_BITMAPWORD) & 1 != 0
        };
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
        let mut n = 0;
        // bounded("small-fixed: one page's visible line pointers, at most MaxHeapTuplesPerPage (291 at 8 K pages), a bit test each")
        for i in 0..vis.len() {
            let off = vis[i];
            let root = self.roots[off as usize - 1];
            if root == 0 {
                continue;
            }
            let bit = root as usize - 1;
            if words[bit / types_nodes::BITS_PER_BITMAPWORD] >> (bit % types_nodes::BITS_PER_BITMAPWORD) & 1 != 0 {
                vis[n] = off;
                n += 1;
            }
        }
        Ok(n)
    }
}

impl BitmapFilter {
    /// D3128: the walk's next block (the sample filter's `next_block` is fallible: NextSampleBlock can raise).
    #[inline(always)]
    fn walk_block(&mut self, nblocks: BlockNumber) -> PgResult<Option<BlockNumber>> {
        let Some(it) = self.iter.as_mut() else { return Ok(None) };
        // a skipped entry is the unit (an iterator step, so a tick per batch): a relation extended after the bind can
        // leave any number of them
        let (mut step, mut skipped) = (cancelpoll::step("bitmap_morsel walk_block: a bitmap entry past the relation's end skipped"), 0usize);
        loop {
            let Some(r) = it.next() else { return Ok(None) };
            // a block past the relation's end at the bind (PG's bitmap scan skips it: `blockno >= rs_nblocks`)
            if r.blockno >= nblocks {
                step.batch(skipped).map_err(|_| crate::pg_err("gravity: a bitmap walk stopped for its statement's cancel"))?;
                skipped += 1;
                continue;
            }
            self.words = r.tuple_bits();
            self.recheck = r.recheck || r.lossy;
            if let Some(c) = &self.counts {
                c.add(if self.words.is_some() { crate::instr::BitmapCounts::EXACT } else { crate::instr::BitmapCounts::LOSSY }, 1);
            }
            return Ok(Some(r.blockno));
        }
    }
}
