//! An ordered btree scan read by the pool in key ranges (the ranges source): the statement's emit keeps the scan's index
//! order, so the key space is cut into ranges at the index's own separator keys, each claim is a run of ranges in index
//! order, every participant reads its claim's ranges forward with an ordinary btree scan through its own view of the
//! execution's frozen index image (`nbtree::IndexImage` / `ImageRel`: buffers by locator, catalog answers from the
//! image's tables, functions from the built-in table), and the emit hands the claims' rows on in claim order (driver
//! order.rs): the rows leave in the index's order.
//!
//! The cut (`cut`): the separators of the highest btree level that has at least `parts` of them (`nbtree::bt_pivots`),
//! read on the leader; each one's first key column, NULLs dropped (the NULLs are a range of their own), consecutive
//! equal images merged, and an even sample of `parts - 1` kept. Range `i` holds the entries whose first column lies
//! between boundary `i - 1` (inclusive) and boundary `i` (exclusive) in index order: two scan keys on the first column
//! (`>=` and `<`, or `<=` and `>` on a DESC column) added to the scan's own keys (ordered by column, as nbtree takes them),
//! which nbtree's preprocessing merges, so a range below or above the quals' bounds costs one descent. The first range has
//! no lower key and the last no upper one; the NULL range (`IS NULL`) comes first under NULLS FIRST, else last. Equal first
//! columns never straddle a boundary. A boundary made stale by a split meanwhile only skews the ranges' sizes: they still
//! cover the key space.
//!
//! Each range's entries, in index order: an index-only scan returns an entry whose heap page the visibility map vouches
//! for from the index tuple (as PostgreSQL's IndexOnlyNext); every other entry (all of a plain index scan's) is read from
//! the heap in a batch of consecutive heap-bound entries, the batch's TIDs left in index order (tid_pages.rs
//! `keep_page_listed`: per run of consecutive TIDs on one block, each listed TID's visible HOT chain member, in the
//! list's order). The batch ends at the next entry the map vouches for, so the order holds across both.
//!
//! The image, the bound keys and the ranges are one run of the pipeline's (exec runner plan.rs captures them on the
//! leader when the pipeline starts, its keys bound then, and puts them in the execution's `RangeCell`, filed in its
//! `Resolved`), never the recipe's. When the capture does not hold, the cell stays empty and the pipeline runs on the
//! leader, whose source (`IndexRanges`) walks the index as the serial index source does. Each participant's pgstat
//! counts (its primitive scans, the entries it returned, the index's block reads) land at the unbind, its heap fetches
//! in the recipe's EXPLAIN ANALYZE counter.
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use gravity_abi::{Datum as Word, Lane, ScanPos};
use gravity_driver::{BindingRef, Cursor, Error, Result, Source, Window};
use pgsync::Mutex;
use types_core::{BlockNumber, Oid};
use types_error::PgResult;
use types_rel::Relation;
use gravity_decompose::catalog::{INDOPTION_DESC, INDOPTION_NULLS_FIRST};
use types_scan::scankey::{BTGreaterEqualStrategyNumber, BTGreaterStrategyNumber, BTLessEqualStrategyNumber, BTLessStrategyNumber, ScanKeyData, StrategyNumber, SK_ISNULL, SK_SEARCHNULL};
use types_scan::sdir::ForwardScanDirection;
use types_storage::bufpage::PageRef;
use types_storage::RelFileLocator;
use types_tuple::itemptr::{ItemPointerGetBlockNumber, ItemPointerGetOffsetNumber};

use crate::ops::scan::bitmap_leaves::SharedScanKey;
use crate::ops::scan::heap_source::{HeapRangeSource, RangeRecipe, SampleFilter};
use crate::ops::scan::index_window::IosBatch;
use crate::ops::scan::tid_pages::TidPages;
use crate::scratch::NodeScratch;

mod raw;

/// One execution's ranges, shared by its participants: the index image, the scan keys and each range's bound keys as
/// any thread rebuilds them, and the heap's visibility map by locator.
pub struct Filed {
    img: Arc<nbtree::IndexImage>,
    keys: Vec<SharedScanKey>,
    /// Range `i`'s keys past `keys` (its bounds on the first column, or its NULL test), in index order.
    ranges: Vec<Vec<SharedScanKey>>,
    /// The separator copies a by-reference bound's argument points into.
    _bounds: Vec<Vec<u64>>,
    /// The leader's `TransactionStartedDuringRecovery`.
    in_recovery: bool,
    /// The heap's map fork: its locator, whether the relation is permanent, and its blocks at the capture (a map page
    /// added later answers not all-visible, which only fetches the heap).
    vm: (RelFileLocator, bool, BlockNumber),
    /// The index's `relisshared` when its statistics count (`pgstat_enabled`).
    stats: Option<bool>,
}

impl Filed {
    /// The run's ranges, the claims' units.
    pub fn len(&self) -> usize {
        self.ranges.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }
}

/// One execution's ranges slot of a site (`Resolved::range_cell`): the ranges of the pipeline's current run, put by exec
/// when the run starts (`None` when its capture does not hold: the leader's serial walk).
#[derive(Default)]
pub struct RangeCell(Mutex<Option<Arc<Filed>>>);

impl RangeCell {
    pub fn set(&self, f: Option<Arc<Filed>>) {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = f;
    }

    pub fn get(&self) -> Option<Arc<Filed>> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

/// The ranges form of an index recipe: the heap read of the entries the scan fetches from the heap, the index, its key
/// count, and an index-only scan's lanes.
#[derive(Clone)]
pub struct RangesRecipe {
    pub range: RangeRecipe,
    pub heap: Oid,
    pub index: Oid,
    /// An index-only scan (its rows from the index tuple where the map vouches for the page).
    pub only: bool,
    /// The site's key words (the claim's cells follow them).
    pub words: usize,
    /// Lane `k`: table column `attnos[k]`, index column `index_attnums[k]` (index-only), a `name` column (`names[k]`:
    /// btree stores a cstring), its by-reference length (`byref[k]`).
    pub attnos: Vec<i16>,
    pub index_attnums: Vec<i16>,
    pub names: Vec<bool>,
    pub byref: Vec<i16>,
    /// The index-only recipe's heap fetch counter (EXPLAIN ANALYZE's "Heap Fetches", `SiteRecipe::heap_fetch_cell`).
    heap_fetches: Arc<AtomicU64>,
}

impl RangesRecipe {
    /// `None` when the recipe's walk is not one the pool may cut: a forward btree walk, every key a plain key a thread
    /// rebuilds (no row comparison, no ORDER BY operator; arrays and NULL tests are nbtree's own), table columns only,
    /// over relations the heap range source reads by locator (not temporary).
    pub fn of(r: &crate::ops::scan::index_source::SiteRecipe, heap: &Relation<'_>, index: &Relation<'_>) -> PgResult<Option<RangesRecipe>> {
        if index.rd_rel.relam != crate::ops::scan::index_source::BTREE_AM_OID || r.backward || r.am.is_some() || !r.rows.is_empty() || heap.uses_local_buffers() || index.uses_local_buffers() {
            return Ok(None);
        }
        if r.attnos.iter().any(|&a| a < 1) || r.keys[..r.nkeys].iter().any(|k| SharedScanKey::of(k).is_none()) || r.words.iter().any(|w| !matches!(w, crate::ops::scan::index_source::KeyWord::Key(_))) {
            return Ok(None);
        }
        let (index_attnums, names) = if r.only { crate::ops::scan::indexonly::index_attnums(index, heap, &r.attnos, &[])? } else { (Vec::new(), Vec::new()) }; // alloc: Program
        let range = RangeRecipe::from_plan(heap, &r.attnos)?;
        let byref = crate::ops::scan::index_window::byref_of(heap, &r.attnos);
        Ok(Some(RangesRecipe { range, heap: heap.rd_id, index: r.index, only: r.only, words: r.words.len(), attnos: r.attnos.clone(), index_attnums, names, byref, heap_fetches: r.heap_fetch_cell() }))
    }

    /// One run's ranges over `index` with `keys` (`SiteRecipe::bind_keys`: the recipe's keys bound to the run's key words;
    /// a by-reference argument's image lives as long as the execution), cut into at most `parts` ranges: `None` when it
    /// does not hold: not imageable, a function it or a key calls not pool-hosted, a first key column whose stored type is
    /// not its operator class's, or a SERIALIZABLE transaction (the image takes no predicate lock). The leader's thread,
    /// when the pipeline's run starts.
    pub fn capture(&self, index: &Relation<'_>, heap: &Relation<'_>, keys: &[ScanKeyData], in_recovery: bool, parts: usize) -> PgResult<Option<Arc<Filed>>> {
        if xact_seams::isolation_is_serializable::is_installed() && xact_seams::isolation_is_serializable::call() {
            return Ok(None);
        }
        let Some(images) = keys.iter().map(SharedScanKey::of).collect::<Option<Vec<SharedScanKey>>>() else { return Ok(None) }; // alloc: Exec
        if !images.iter().all(|k| k.func == 0 || gravity_fn::session::pool_hosted(k.func)) {
            return Ok(None);
        }
        let Some(img) = nbtree::IndexImage::capture(index)? else { return Ok(None) };
        if !img.proc_oids().all(gravity_fn::session::pool_hosted) {
            return Ok(None);
        }
        let Some((ranges, bounds)) = cut(index, &img, parts)? else { return Ok(None) };
        let permanent = heap.rd_rel.relpersistence == types_core::RELPERSISTENCE_PERMANENT;
        let vm = (heap.rd_locator.get(), permanent, visibilitymap::vm_nblocks(heap)?);
        let stats = index.pgstat_enabled.get().then_some(index.rd_rel.relisshared);
        Ok(Some(Arc::new(Filed { img: Arc::new(img), keys: images, ranges, _bounds: bounds, in_recovery, vm, stats }))) // alloc: Exec
    }
}

/// The ranges of `index` (its image `img`, read on this thread) in index order, at most `parts` of them past the NULL
/// range, each as its keys past the scan's own, and the separator copies their arguments point into. `None` when the
/// first key column's stored type is not its operator class's input type, or a bound's function is not pool-hosted.
#[allow(clippy::type_complexity)]
fn cut(index: &Relation<'_>, img: &nbtree::IndexImage, parts: usize) -> PgResult<Option<(Vec<Vec<SharedScanKey>>, Vec<Vec<u64>>)>> {
    let (Some(&family), Some(&opcintype), Some(att)) = (index.rd_opfamily.first(), index.rd_opcintype.first(), index.rd_att.attrs.first()) else { return Ok(None) };
    if att.atttypid != opcintype {
        return Ok(None);
    }
    let option = index.rd_indoption.first().copied().unwrap_or(0) as u8;
    let collation = index.rd_indcollation.first().copied().unwrap_or(0);
    let (desc, nulls_first) = (option & INDOPTION_DESC != 0, option & INDOPTION_NULLS_FIRST != 0);
    // a range's lower bound in index order is inclusive, its upper exclusive
    let (lo, hi) = if desc { (BTLessEqualStrategyNumber, BTGreaterStrategyNumber) } else { (BTGreaterEqualStrategyNumber, BTLessStrategyNumber) };
    let func = |strategy: StrategyNumber| gravity_fn::typesem::opfamily_member_proc(family, opcintype, strategy as i16);
    let (lo_fn, hi_fn) = (func(lo)?, func(hi)?);
    if lo_fn == 0 || hi_fn == 0 || !gravity_fn::session::pool_hosted(lo_fn) || !gravity_fn::session::pool_hosted(hi_fn) {
        return Ok(None);
    }
    let scratch = NodeScratch::new("GravityIndexRanges");
    let rel = nbtree::ImageRel::new(img, scratch.mcx())?;
    let pivots = nbtree::bt_pivots(&rel, parts)?;
    let copies: Vec<Vec<u64>> = pivots.keys.iter().map(|k| raw::aligned(k)).collect(); // alloc: Exec
    let (byval, len) = (att.attbyval, att.attlen);
    // each separator's first column, NULLs dropped, consecutive equal images merged (equal values whose images differ
    // only make an empty range between them)
    let mut kept: Vec<datum::Datum> = Vec::new(); // alloc: Exec
    // bounded("other: the separators of the one btree level the cut took (nbtree::bt_pivots): the level above cuts fewer than parts ranges, so this one has fewer than parts pages of at most MaxIndexTuplesPerPage (408) separators; parts is INDEX_RANGES_PER_WORKER per participant")
    for k in &copies {
        let Some(d) = raw::first_column(k, &index.rd_att, index.indnkeyatts()) else { continue };
        if let Some(&last) = kept.last() {
            if adt_scalar::datum_ops::datum_is_equal(last, d, byval, len)? {
                continue;
            }
        }
        kept.push(d); // alloc: Exec
    }
    // an even sample of `parts - 1` boundaries
    let want = parts.saturating_sub(1);
    let bounds: Vec<datum::Datum> = if kept.len() <= want { kept } else { (1..=want).map(|j| kept[j * kept.len() / (want + 1)]).collect() }; // alloc: Exec
    let key = |strategy: StrategyNumber, f: Oid, arg: datum::Datum| SharedScanKey::bound(0, 1, strategy, opcintype, collation, f, arg);
    let mut ranges: Vec<Vec<SharedScanKey>> = Vec::with_capacity(bounds.len() + 2); // alloc: Exec
    if bounds.is_empty() {
        // one range: the whole key space, its NULLs too
        ranges.push(Vec::new()); // alloc: Exec
        return Ok(Some((ranges, copies)));
    }
    let null = vec![SharedScanKey::bound(SK_ISNULL | SK_SEARCHNULL, 1, 0, 0, 0, 0, datum::Datum::null())]; // alloc: Exec
    if nulls_first {
        ranges.push(null.clone()); // alloc: Exec
    }
    ranges.push(vec![key(hi, hi_fn, bounds[0])]); // alloc: Exec
    // bounded("other: one range per pair of the sampled boundaries, at most parts - 1 of them (INDEX_RANGES_PER_WORKER per participant)")
    for w in bounds.windows(2) {
        ranges.push(vec![key(lo, lo_fn, w[0]), key(hi, hi_fn, w[1])]); // alloc: Exec
    }
    ranges.push(vec![key(lo, lo_fn, bounds[bounds.len() - 1])]); // alloc: Exec
    if !nulls_first {
        ranges.push(null); // alloc: Exec
    }
    Ok(Some((ranges, copies)))
}

/// A participant's view of the filed ranges on the thread that built it: its `ImageRel` and scan descriptor, in its own
/// context. Fields drop in order: the scan and the view before the image they borrow, all before the context they live
/// in.
struct View {
    scan: types_relscan::IndexScanDescData<'static>,
    /// The heap's map page this thread pinned last (a pin is the thread's, as the scan's).
    vm: visibilitymap::VmBuffer,
    rel: nbtree::ImageRel<'static, 'static>,
    filed: Arc<Filed>,
    thread: std::thread::ThreadId,
    /// The context the view's scan and relation live in (kept to the view's end, dropped last).
    _scratch: NodeScratch,
}

/// The heap read's filter: a batch of consecutive heap-bound entries, their TIDs in index order (tid_pages.rs
/// `keep_page_listed`).
pub struct RangeTids {
    pages: TidPages,
}

impl SampleFilter for RangeTids {
    const SPARSE: bool = true;
    const PAGE: bool = true;
    const BITMAP: bool = true;
    const RING: bool = false;

    fn start(&mut self, _keys: &[Word], _keys_null: &[u8]) -> PgResult<()> {
        Ok(())
    }

    fn next_block(&mut self, nblocks: BlockNumber) -> PgResult<Option<BlockNumber>> {
        self.pages.next_block(nblocks)
    }

    fn keep(&mut self, _blk: BlockNumber, _maxoff: u16, vis: &mut [u16]) -> usize {
        vis.len()
    }

    fn keep_page(&mut self, _blk: BlockNumber, page: PageRef<'_>, vis: &mut [u16]) -> PgResult<usize> {
        self.pages.keep_page_listed(page, vis, None)
    }
}

/// Which part of the claim the next window comes from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// The index: entries the visibility map vouches for (an index-only scan's).
    Index,
    /// The heap: the batch of consecutive entries before the next one the map vouches for.
    Heap,
}

/// A participant's range source (the registry derives one for the leader and for each worker of a ranges site).
pub struct RangeSource {
    site: usize,
    /// A worker's (`derive`): its claims run whole on one thread inside a section, so its release is a claim's end and its
    /// view's pins go there. The leader's (in `IndexRanges`) may stop between two rows and go on: its view stays to
    /// `unbind`, on the backend's thread.
    worker: bool,
    recipe: RangesRecipe,
    /// The execution's slot of the site (`bind`), read at each claim's seek.
    cell: Option<Arc<RangeCell>>,
    filed: Option<Arc<Filed>>,
    view: Option<View>,
    batch: Option<IosBatch>,
    /// Per lane, a `name` cell's NAMEDATALEN image (btree stores a cstring), live until the row is copied into the window.
    name_bufs: Vec<u8>,
    heap: HeapRangeSource<RangeTids>,
    phase: Phase,
    /// The claim's ranges: the next one to open and the end (`[next, end)`), and whether the current one is open.
    next: usize,
    end: usize,
    open: bool,
    /// The scan stands on an entry the map vouches for that the index phase has not returned yet (it came after a heap
    /// batch's entries).
    pending: bool,
    /// The rows the current index window returned so far: a heap-bound entry ends a window that has rows.
    emitted: u32,
    /// A range past the run's first just opened (the scan's primitive scans so far already taken into `counts`). Its
    /// first descent is the cut's, not one PostgreSQL's walk would make, so it is not counted (pgstat's idx_scan).
    fresh: bool,
    cur: Cursor,
    /// `(primitive scans, entries, blocks fetched, blocks hit)` of the index since the bind.
    counts: (u64, u64, u64, u64),
    stats: Option<(Oid, bool)>,
    /// The entries whose heap page the map did not vouch for since the last flush (PostgreSQL's `InstrCountTuples2`).
    fetches: u64,
}

/// The heap-bound entries a batch holds at most before its heap read runs: a window's rows (`IosBatch`'s largest window).
const HEAP_BATCH: usize = crate::ops::scan::index_window::IOS_MAX_ROWS;

impl RangeSource {
    pub fn new(r: &RangesRecipe, site: usize, worker: bool) -> RangeSource {
        let heap = HeapRangeSource::with_sample(r.range.clone(), RangeTids { pages: TidPages::new() }).serving_worker(); // alloc: Node
        let batch = r.only.then(|| IosBatch::with_byref(r.byref.clone(), &r.attnos, None, None));
        RangeSource { site, worker, recipe: r.clone(), cell: None, filed: None, view: None, batch, name_bufs: vec![0; r.attnos.len() * crate::ops::scan::indexonly::NAMEDATALEN], heap, phase: Phase::Index, next: 0, end: 0, open: false, pending: false, emitted: 0, fresh: false, cur: Cursor::default(), counts: (0, 0, 0, 0), stats: None, fetches: 0 } // alloc: Node
    }

    /// This thread's view of the bound execution's ranges, checked at each index window's start and built at the first
    /// one on the thread (a claim runs whole on one thread, the leader's on the backend's). The entry loop then reads it
    /// with `cur`: no reference count of the shared image is touched per entry (every participant's would contend on it).
    fn ensure_view(&mut self) -> PgResult<()> {
        let filed = self.filed.as_ref().ok_or_else(|| crate::pg_err("gravity: a range walk without its execution's image"))?;
        let me = std::thread::current().id();
        if self.view.as_ref().is_some_and(|v| v.thread == me && Arc::ptr_eq(&v.filed, filed)) {
            return Ok(());
        }
        if self.open || self.pending {
            // the scan stands inside a range: another thread's view could not go on from its position
            return Err(crate::pg_err("gravity: a range walk moved to another thread inside a range"));
        }
        let filed = filed.clone();
        self.drop_view();
        let scratch = NodeScratch::new("GravityIndexRanges");
        let mcx = scratch.mcx();
        // the view's fields hold `filed` (the image) and `scratch` (the context) past `rel` and `scan` (`View`)
        let img = raw::image(&filed);
        let rel = nbtree::ImageRel::new(img, mcx)?;
        let most = filed.ranges.iter().map(Vec::len).max().unwrap_or(0);
        let mut scan = rel.begin_scan(mcx, (filed.keys.len() + most) as i32, filed.in_recovery)?;
        scan.xs_want_itup = self.recipe.only;
        self.stats = filed.stats.map(|shared| (filed.img.oid(), shared));
        self.view = Some(View { scan, vm: visibilitymap::VmBuffer::new(), rel, filed, thread: me, _scratch: scratch });
        Ok(())
    }

    /// The window's view (`ensure_view`).
    fn cur(&mut self) -> PgResult<&mut View> {
        self.view.as_mut().ok_or_else(|| crate::pg_err("gravity: a range walk without its view"))
    }

    /// The view ended: its scan closed on the thread that built it (a pin is that thread's), its counts taken.
    fn drop_view(&mut self) {
        let Some(mut v) = self.view.take() else { return };
        self.counts.0 += v.scan.xs_pgstat.take_detached_scans();
        self.counts.2 += v.rel.blocks_fetched();
        self.counts.3 += v.rel.blocks_hit();
        if v.thread == std::thread::current().id() {
            v.vm.release();
            let _ = nbtree::btendscan(&mut v.scan);
        }
    }

    /// The heap fetches counted since the last flush, into the recipe's EXPLAIN ANALYZE counter.
    fn flush_fetches(&mut self) {
        let n = core::mem::take(&mut self.fetches);
        if n > 0 {
            self.recipe.heap_fetches.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// The claim's next entry in index order: `true` with the scan standing on it, `false` past the claim's last range
    /// (each range opened in turn: the scan restarted with the scan's keys and the range's).
    fn step(&mut self) -> PgResult<bool> {
        // bounded("other: a pass returns the scan's next entry or opens the claim's next range, so at most one pass per range of the claim and one more (the cut makes at most parts + 1 ranges)")
        loop {
            if !self.open {
                if self.next >= self.end {
                    return Ok(false);
                }
                let i = self.next;
                self.next += 1;
                let v = self.cur()?;
                let mut keys = v.filed.keys.iter().chain(v.filed.ranges.get(i).into_iter().flatten()).map(SharedScanKey::key).collect::<PgResult<Vec<ScanKeyData>>>()?; // alloc: Node
                // D3541: nbtree's preprocessing takes the keys ordered by index column (the planner's index quals are); the
                // range's first-column bounds go after the scan's first-column keys, before its keys on later columns
                // (a stable sort: each column's keys keep their order)
                // bounded("other: the scan's keys and the range's two bounds; the statement's index quals bound the scan's")
                keys.sort_by_key(|k| k.sk_attno);
                v.scan.numberOfKeys = keys.len() as i32;
                nbtree::btrescan(&mut v.scan, if keys.is_empty() { None } else { Some(&keys) })?;
                let before = if i > 0 { v.scan.xs_pgstat.take_detached_scans() } else { 0 };
                self.counts.0 += before;
                self.fresh = i > 0;
                self.open = true;
            }
            let fresh = core::mem::take(&mut self.fresh);
            let v = self.cur()?;
            let got = nbtree::btgettuple_over(&mut v.scan, &v.rel, ForwardScanDirection)?;
            if fresh {
                // the range's first descent, counted by its first step, is not one
                let n = v.scan.xs_pgstat.take_detached_scans();
                self.counts.0 += n.saturating_sub(1);
            }
            if got {
                self.counts.1 += 1;
                return Ok(true);
            }
            self.open = false;
        }
    }

    /// The claim's next entry the map vouches for, its cells into `vals` / `nulls`; the heap-bound entries before it into
    /// the heap batch. `None` at the claim's end, when the batch is full, when a heap-bound entry follows rows this window
    /// returned, or when an entry the map vouches for follows the batch's entries (kept `pending` for after them).
    fn next_visible(&mut self, vals: &mut [Word], nulls: &mut [u8]) -> PgResult<Option<ScanPos>> {
        let (loc, permanent, vm_blocks) = self.filed.as_ref().ok_or_else(|| crate::pg_err("gravity: a range walk without its execution's image"))?.vm;
        // bounded("small-fixed: every pass but the last files one TID into the heap batch, which is full at HEAP_BATCH (IOS_MAX_ROWS: MaxIndexTuplesPerPage, 408 at 8 K pages)")
        loop {
            if !self.pending {
                if self.heap.sample_mut().pages.len() >= HEAP_BATCH || !self.step()? {
                    return Ok(None);
                }
                let v = self.cur()?;
                let tid = v.scan.xs_heaptid;
                let block = ItemPointerGetBlockNumber(&tid);
                if !visibilitymap::vm_all_visible_by_locator(loc, permanent, vm_blocks, block, &mut v.vm)? {
                    self.fetches += 1;
                    self.heap.sample_mut().pages.push(tid);
                    if self.emitted > 0 {
                        return Ok(None);
                    }
                    continue;
                }
                if !self.heap.sample_mut().pages.is_empty() {
                    self.pending = true;
                    return Ok(None);
                }
            }
            self.pending = false;
            let v = self.view.as_ref().expect("built");
            let tid = v.scan.xs_heaptid;
            let Some(row) = crate::ops::scan::raw::OnlyRow::of(&v.scan) else { return Err(crate::pg_err("gravity: the index returned no tuple image")) };
            // bounded("key-columns: one cell per output column read from the index tuple, at most INDEX_MAX_KEYS (32) columns")
            for (k, &attnum) in self.recipe.index_attnums.iter().enumerate() {
                let mut null = false;
                let d = row.attr(attnum as types_core::AttrNumber, &mut null);
                vals[k] = d.as_u64();
                nulls[k] = null as u8;
                if self.recipe.names[k] && !null && row.is_index() {
                    // the cstring into a NAMEDATALEN image (NUL-padded), live until this row is copied into the window
                    // (indexonly.rs's)
                    const N: usize = crate::ops::scan::indexonly::NAMEDATALEN;
                    let buf = &mut self.name_bufs[k * N..(k + 1) * N];
                    buf.fill(0);
                    let src = crate::ops::scan::raw::index_cstr(d).to_bytes();
                    let len = src.len().min(N - 1);
                    buf[..len].copy_from_slice(&src[..len]);
                    vals[k] = buf.as_ptr() as u64;
                }
            }
            self.emitted += 1;
            return Ok(Some(ScanPos::row(self.recipe.heap, ItemPointerGetBlockNumber(&tid), ItemPointerGetOffsetNumber(&tid))));
        }
    }

    /// A plain index scan's next heap batch: up to `HEAP_BATCH` entries in index order.
    fn next_batch(&mut self) -> PgResult<()> {
        // bounded("small-fixed: one entry per pass into the heap batch, at most HEAP_BATCH (IOS_MAX_ROWS: MaxIndexTuplesPerPage, 408 at 8 K pages)")
        while self.heap.sample_mut().pages.len() < HEAP_BATCH && self.step()? {
            let tid = self.cur()?.scan.xs_heaptid;
            self.heap.sample_mut().pages.push(tid);
        }
        Ok(())
    }
}

impl Source for RangeSource {
    fn bind(&mut self, b: &BindingRef<'_>) -> Result<()> {
        let resolved = b.resolved.and_then(|r| r.downcast_ref::<crate::ops::scan::pgrc::granule_source::Resolved>());
        self.cell = resolved.and_then(|r| r.range_cell(self.site)).cloned();
        self.heap.bind(b)
    }

    /// A claim's start: its ranges are the claim's cells past the key words (`[lo, hi)`), read in turn from the first.
    fn seek(&mut self, keys: &[Word], _keys_null: &[u8]) -> Result<()> {
        let filed = self.cell.as_ref().and_then(|c| c.get()).ok_or_else(|| Error::host("gravity: a range walk without its run's ranges"))?;
        let n = self.recipe.words;
        let (Some(&lo), Some(&hi)) = (keys.get(n), keys.get(n + 1)) else { return Err(Error::host("gravity: a range walk's claim without its cells")) };
        let same = self.filed.as_ref().is_some_and(|f| Arc::ptr_eq(f, &filed));
        if !same {
            self.drop_view();
            self.filed = Some(filed.clone());
        }
        (self.next, self.end) = (lo as usize, (hi as usize).min(filed.len()));
        (self.open, self.pending, self.emitted) = (false, false, 0);
        self.phase = Phase::Index;
        if let Some(b) = self.batch.as_mut() {
            b.rewind();
        }
        self.heap.sample_mut().pages.clear();
        Ok(())
    }

    fn window(&mut self, w: &mut Window<'_>) -> Result<u32> {
        // a pass is the unit: a batch of index entries walked or its heap pages read (a run of batches whose every heap
        // tuple is dead returns no row)
        let mut step = cancelpoll::step("index_ranges window: one batch of entries or its heap pages");
        // memsig E-pub t3: the TID list on the window's owner before the walk fills it
        self.heap.sample_mut().pages.set_owner(crate::ops::scan::heap_source::HeldCharge::owner_of(w));
        loop {
            step.tick()?;
            if self.phase == Phase::Heap {
                let n = self.heap.window(w)?;
                if n > 0 {
                    return Ok(n);
                }
                self.heap.sample_mut().pages.clear();
                self.phase = Phase::Index;
            }
            if self.heap.sample_mut().pages.is_empty() {
                self.ensure_view().map_err(crate::stash)?;
                if self.recipe.only {
                    let heap_oid = self.recipe.heap;
                    let Some(mut b) = self.batch.take() else { return Ok(0) };
                    self.emitted = 0;
                    let got = b.fill_rows(|v, n| self.next_visible(v, n), heap_oid, w);
                    self.batch = Some(b);
                    let (rows, _, _) = got.map_err(crate::stash)?;
                    if rows > 0 {
                        return Ok(rows);
                    }
                } else {
                    self.next_batch().map_err(crate::stash)?;
                }
                if self.heap.sample_mut().pages.is_empty() {
                    self.flush_fetches();
                    if let Some(v) = self.view.as_mut() {
                        v.vm.release();
                    }
                    return Ok(0);
                }
            }
            self.heap.seek(&[], &[])?;
            self.phase = Phase::Heap;
        }
    }

    fn cursor(&self) -> &Cursor {
        &self.cur
    }

    fn cursor_mut(&mut self) -> &mut Cursor {
        &mut self.cur
    }

    fn lanes_hint(&self) -> usize {
        self.recipe.attnos.len() + 1
    }

    fn serves_worker(&self) -> bool {
        true
    }

    fn lane(&mut self, k: u16, sel: &[u32], out: &mut Lane) -> Result<u32> {
        self.heap.lane(k, sel, out)
    }

    fn refetch(&mut self, values: &mut [Word], isnull: &mut [u8]) -> Result<bool> {
        self.heap.refetch(values, isnull)
    }

    fn substitute(&mut self, pos: ScanPos) {
        self.heap.substitute(pos)
    }

    fn pins(&self) -> u64 {
        self.heap.pins()
    }

    /// A worker's claim end, on the thread that ran it: the scan's leaf pin and the map pin go with the view (its next
    /// claim may run on another thread). The leader's call may stop between two rows and go on: its view stays.
    fn release(&mut self) {
        if self.worker && self.view.is_some() {
            self.drop_view();
            self.flush_fetches();
        }
        self.heap.release()
    }

    fn park(&mut self) {
        self.heap.park()
    }

    fn unpark(&mut self) {
        self.heap.unpark()
    }

    /// The slot depot's hook: the heap read's held charge and scratch (D4464); the view is dropped at the unbind.
    fn transfer_held(&mut self, to: memledger::Owner, release: memledger::Release) {
        self.heap.transfer_held(to, release)
    }

    /// The counts since the bind, landed on the leader's thread (bitmap_image.rs's), the view and the map pin dropped.
    fn unbind(&mut self) {
        self.drop_view();
        self.flush_fetches();
        self.filed = None;
        self.cell = None;
        let (scans, tuples, fetched, hit) = core::mem::take(&mut self.counts);
        if let Some((index, shared)) = self.stats {
            pgstat::relation::pgstat_count_index_scan_batched(index, shared, scans, tuples, 0);
            pgstat::relation::pgstat_count_buffer_reads(index, shared, fetched, hit);
        }
        self.heap.unbind()
    }
}

impl Drop for RangeSource {
    fn drop(&mut self) {
        self.drop_view();
    }
}

/// The leader's source of a ranges site: its claims' ranges when the run's capture held (`RangeCell`), else the serial
/// index source's walk of the whole index (exec then runs the pipeline on the leader alone, as one claim).
pub struct IndexRanges {
    serial: crate::ops::scan::index_source::IndexSource,
    ranges: RangeSource,
    /// The current claim reads the run's ranges (decided at each seek from the run's cell).
    ranges_mode: bool,
}

impl IndexRanges {
    pub fn new(serial: crate::ops::scan::index_source::IndexSource, ranges: RangeSource) -> IndexRanges {
        IndexRanges { serial, ranges, ranges_mode: false }
    }
}

impl Source for IndexRanges {
    fn bind(&mut self, b: &BindingRef<'_>) -> Result<()> {
        self.ranges_mode = false;
        self.serial.bind(b)?;
        self.ranges.bind(b)
    }

    /// The serial walk reads the key words alone (the claim's cells follow them).
    fn seek(&mut self, keys: &[Word], keys_null: &[u8]) -> Result<()> {
        self.ranges_mode = self.ranges.cell.as_ref().is_some_and(|c| c.get().is_some());
        if self.ranges_mode {
            return self.ranges.seek(keys, keys_null);
        }
        let n = self.ranges.recipe.words.min(keys.len()).min(keys_null.len());
        self.serial.seek(&keys[..n], &keys_null[..n])
    }

    fn window(&mut self, w: &mut Window<'_>) -> Result<u32> {
        if self.ranges_mode { self.ranges.window(w) } else { self.serial.window(w) }
    }

    fn cursor(&self) -> &Cursor {
        if self.ranges_mode { self.ranges.cursor() } else { self.serial.cursor() }
    }

    fn cursor_mut(&mut self) -> &mut Cursor {
        if self.ranges_mode { self.ranges.cursor_mut() } else { self.serial.cursor_mut() }
    }

    fn next(&mut self, values: &mut [Word], isnull: &mut [u8]) -> Result<bool> {
        if self.ranges_mode { self.ranges.next(values, isnull) } else { self.serial.next(values, isnull) }
    }

    fn position(&self) -> ScanPos {
        if self.ranges_mode { self.ranges.position() } else { self.serial.position() }
    }

    fn lanes_hint(&self) -> usize {
        if self.ranges_mode { self.ranges.lanes_hint() } else { self.serial.lanes_hint() }
    }

    fn lane(&mut self, k: u16, sel: &[u32], out: &mut Lane) -> Result<u32> {
        if self.ranges_mode { self.ranges.lane(k, sel, out) } else { self.serial.lane(k, sel, out) }
    }

    fn refetch(&mut self, values: &mut [Word], isnull: &mut [u8]) -> Result<bool> {
        if self.ranges_mode { self.ranges.refetch(values, isnull) } else { self.serial.refetch(values, isnull) }
    }

    fn substitute(&mut self, pos: ScanPos) {
        if self.ranges_mode { self.ranges.substitute(pos) } else { self.serial.substitute(pos) }
    }

    fn pins(&self) -> u64 {
        self.serial.pins() + self.ranges.pins()
    }

    fn release(&mut self) {
        self.serial.release();
        self.ranges.release();
    }

    fn park(&mut self) {
        self.serial.park();
        self.ranges.park();
    }

    fn unpark(&mut self) {
        self.serial.unpark();
        self.ranges.unpark();
    }

    /// The slot depot's hook: both walks' (D4464).
    fn transfer_held(&mut self, to: memledger::Owner, release: memledger::Release) {
        self.serial.transfer_held(to, release);
        self.ranges.transfer_held(to, release);
    }

    fn unbind(&mut self) {
        self.ranges_mode = false;
        self.serial.unbind();
        self.ranges.unbind();
    }
}

/// A worker's source for recipe `r` at registry entry `site` (`HostRegistry::derive_worker_source`).
pub fn derive(r: &RangesRecipe, site: usize, a: gravity_driver::ArenaRef) -> gravity_driver::SourceBox {
    gravity_driver::box_source(RangeSource::new(r, site, true), a)
}

/// The leader's source of a ranges site (`derive.rs`): the serial index source of `serial` and the ranges of `r`.
pub fn derive_leader(serial: crate::ops::scan::index_source::IndexSource, r: &RangesRecipe, site: usize, a: gravity_driver::ArenaRef) -> gravity_driver::SourceBox {
    gravity_driver::box_source(IndexRanges::new(serial, RangeSource::new(r, site, false)), a)
}

pub const SIZE: usize = core::mem::size_of::<IndexRanges>();
