//! An order-free btree Index Only Scan read by the pool (the leaf source): every participant, the leader too, seizes
//! leaf pages of one shared PostgreSQL parallel btree scan (nbtree parallel.rs seize / release / done) through its own
//! view of the execution's frozen index image (`nbtree::IndexImage` / `ImageRel`: buffers by locator, catalog answers
//! from the image's tables, functions from the built-in table), and returns:
//! - each entry whose heap page the visibility map vouches for from the index tuple, as PostgreSQL's IndexOnlyNext does
//!   (the map read by locator, `visibilitymap::vm_all_visible_by_locator`);
//! - the other entries from the heap: their TIDs, up to a window's worth at a time, sorted by block and read as the heap
//!   range source reads a probe's TIDs (tid_pages.rs: relcache-free page reads under the snapshot, the visible tuple
//!   whose HOT chain root is the TID), with the same lanes (the columns, then the RowId lane).
//! Rows come in no order (a seized leaf at a time, the heap's in block order per batch): the decomposition takes the form
//! only where no reader depends on the scan's order. The image, the bound keys and the shared scan state are one run of
//! the pipeline's (exec runner plan.rs captures them on the leader when the pipeline starts, its keys bound then, and
//! puts them in the execution's `LeafCell`, filed in its `Resolved`, D3117), never the recipe's. Every claim joins the
//! walk until it ends, so the claims' count does not matter: a claim after the end gets no row. When the capture does not
//! hold, the cell stays empty and the pipeline runs on the leader, whose source (`IndexLeaves`) walks the index as the
//! serial index source does. Each walk's pgstat counts (its primitive scans, the entries it returned, the index's block
//! reads) land at the unbind, its heap fetches in the recipe's EXPLAIN ANALYZE counter.
//! Dead entries (PostgreSQL's `kill_prior_tuple`, which an index-only scan sets for an entry whose HOT chain is all dead):
//! the participant that reads an entry's heap page judges it there, under the page's share lock: a listed TID none of
//! whose chain the snapshot sees and whose chain is all dead before the walk's horizon (`TidPages::keep_page_dead`; the
//! horizon is `GetOldestNonRemovableTransactionId`, taken once on the leader at the capture, as a parallel index build
//! takes its OldestXmin, since the GlobalVis state is the backend thread's). Each dead entry keeps its leaf position
//! (block, offset, the leaf's LSN under its read lock); when a heap batch ends, the same participant reads each leaf the
//! walk has left again by block and, when its LSN is unchanged, marks the dead entries LP_DEAD (`nbtree::bt_kill_by_block`,
//! _bt_killitems' drop-pin path). The leaf the walk is still on waits for the next batch (a posting tuple's TIDs may span
//! two), and a claim's end marks what is left. A leaf whose LSN moved is left as it is.
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use gravity_abi::{Datum as Word, Lane, ScanPos};
use gravity_driver::{BindingRef, Cursor, Error, Result, Source, Window};
use pgsync::Mutex;
use types_core::{BlockNumber, Oid, XLogRecPtr};
use types_error::PgResult;
use types_rel::Relation;
use types_scan::scankey::ScanKeyData;
use types_scan::sdir::ForwardScanDirection;
use types_storage::bufpage::PageRef;
use types_storage::RelFileLocator;
use types_tuple::itemptr::{ItemPointerData, ItemPointerGetBlockNumber, ItemPointerGetOffsetNumber};

use crate::ops::scan::bitmap_leaves::SharedScanKey;
use crate::ops::scan::heap_source::{charged_scratch, HeapRangeSource, HeldCharge, HeldOwner, RangeRecipe, SampleFilter};
use crate::ops::scan::index_window::IosBatch;
use crate::ops::scan::tid_pages::TidPages;
use crate::scratch::NodeScratch;

mod raw;

/// One execution's leaf walk, shared by its participants: the index image, the parallel scan state every participant
/// seizes leaves of, the scan keys as any thread rebuilds them, and the heap's visibility map by locator.
pub struct Filed {
    img: Arc<nbtree::IndexImage>,
    shared: Arc<types_relscan::ParallelIndexScanDescShared>,
    keys: Vec<SharedScanKey>,
    /// The leader's `TransactionStartedDuringRecovery`.
    in_recovery: bool,
    /// The heap's map fork: its locator, whether the relation is permanent, and its blocks at the capture (smgr state is
    /// the leader's; a map page added later answers not all-visible, which only fetches the heap).
    vm: (RelFileLocator, bool, BlockNumber),
    /// The index's `relisshared` when its statistics count (`pgstat_enabled`).
    stats: Option<bool>,
    /// The walk marks the dead entries it meets: the index is WAL-logged (its page LSN moves with any change that could
    /// move an entry, PostgreSQL's drop-pin condition) and the transaction did not start in recovery.
    kill: bool,
    /// When `kill`: the heap's oldest transaction not yet removable at the capture (`GetOldestNonRemovableTransactionId`),
    /// the horizon a dead entry's chain is judged against on any thread.
    horizon: types_core::TransactionId,
}

impl Filed {
    /// The shared scan, ended for every participant (an error or an unwind in one: the others parked on the next leaf
    /// wake and see it done).
    fn abort(&self) {
        let types_relscan::ParallelIndexAmShared::Btree(bt) = &self.shared.am;
        nbtree::bt_parallel_abort(bt);
    }
}

/// One execution's leaf walk slot of a site (`Resolved::leaf_cell`): the walk of the pipeline's current run, put by exec
/// when the run starts (`None` when its capture does not hold: the leader's serial walk).
#[derive(Default)]
pub struct LeafCell(Mutex<Option<Arc<Filed>>>);

impl LeafCell {
    pub fn set(&self, f: Option<Arc<Filed>>) {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = f;
    }

    pub fn get(&self) -> Option<Arc<Filed>> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

/// The leaf form of an index-only recipe: the heap read of the entries the visibility map does not vouch for, the index,
/// its key template (arguments bound per execution), and the window's lanes.
#[derive(Clone)]
pub struct LeafRecipe {
    pub range: RangeRecipe,
    pub heap: Oid,
    pub index: Oid,
    /// Lane `k`: table column `attnos[k]`, index column `index_attnums[k]`, a `name` column (`names[k]`: btree stores a
    /// cstring), its by-reference length (`byref[k]`).
    pub attnos: Vec<i16>,
    pub index_attnums: Vec<i16>,
    pub names: Vec<bool>,
    pub byref: Vec<i16>,
    /// The index-only recipe's heap fetch counter (EXPLAIN ANALYZE's "Heap Fetches", `SiteRecipe::heap_fetch_cell`).
    heap_fetches: Arc<AtomicU64>,
}

impl LeafRecipe {
    /// `None` when the recipe's walk is not one the pool may share: an index-only btree walk (either direction: the form
    /// is order-free, the pool walks forward), every key a plain key a thread rebuilds (no row comparison, no ORDER BY
    /// operator; arrays and NULL tests are nbtree's own), table columns only, over relations the heap range source reads
    /// by locator (not temporary).
    pub fn of(r: &crate::ops::scan::index_source::SiteRecipe, heap: &Relation<'_>, index: &Relation<'_>) -> PgResult<Option<LeafRecipe>> {
        if !r.only || index.rd_rel.relam != crate::ops::scan::index_source::BTREE_AM_OID || r.am.is_some() || !r.rows.is_empty() || heap.uses_local_buffers() || index.uses_local_buffers() {
            return Ok(None);
        }
        if r.attnos.iter().any(|&a| a < 1) || r.keys[..r.nkeys].iter().any(|k| SharedScanKey::of(k).is_none()) || r.words.iter().any(|w| !matches!(w, crate::ops::scan::index_source::KeyWord::Key(_))) {
            return Ok(None);
        }
        let (index_attnums, names) = crate::ops::scan::indexonly::index_attnums(index, heap, &r.attnos, &[])?;
        let range = RangeRecipe::from_plan(heap, &r.attnos)?;
        let byref = crate::ops::scan::index_window::byref_of(heap, &r.attnos);
        Ok(Some(LeafRecipe { range, heap: heap.rd_id, index: r.index, attnos: r.attnos.clone(), index_attnums, names, byref, heap_fetches: r.heap_fetch_cell() }))
    }

    /// One run's shared walk over `index` with `keys` (`SiteRecipe::bind_keys`: the recipe's keys bound to the run's key
    /// words; a by-reference argument's image lives as long as the execution), for the leader and the pool:
    /// `None` when it does not hold: not imageable, a function it or a key calls not pool-hosted, or a SERIALIZABLE
    /// transaction (the image takes no predicate lock). The leader's thread, when the pipeline's run starts.
    pub fn capture(&self, index: &Relation<'_>, heap: &Relation<'_>, keys: &[ScanKeyData], snapshot: &std::rc::Rc<types_snapshot::SnapshotData<'static>>, in_recovery: bool) -> PgResult<Option<Arc<Filed>>> {
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
        let shared = Arc::new(types_relscan::ParallelIndexScanDescShared { // alloc: Exec
            ps_locator: img.locator(),
            ps_indexlocator: img.locator(),
            snapshot: snapmgr::SerializeSnapshot(snapshot),
            am: types_relscan::ParallelIndexAmShared::Btree(types_relscan::BTParallelScanShared::new()),
        });
        let permanent = heap.rd_rel.relpersistence == types_core::RELPERSISTENCE_PERMANENT;
        let vm = (heap.rd_locator.get(), permanent, visibilitymap::vm_nblocks(heap)?);
        let stats = index.pgstat_enabled.get().then_some(index.rd_rel.relisshared);
        let kill = !in_recovery && heapam::relation_needs_wal(index);
        let horizon = if kill { procarray_seams::get_oldest_non_removable_transaction_id::call(heap)? } else { types_core::InvalidTransactionId };
        Ok(Some(Arc::new(Filed { img: Arc::new(img), shared, keys: images, in_recovery, vm, stats, kill, horizon }))) // alloc: Exec
    }
}

/// A participant's view of the filed walk on the thread that built it: its `ImageRel` and scan descriptor over the shared
/// state, in its own context. Fields drop in order: the scan and the view before the image they borrow, all before the
/// context they live in.
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

/// The heap read's filter: the TIDs a batch of the walk left to the heap, in block order (tid_pages.rs).
pub struct LeafTids {
    pages: TidPages,
    /// The batch's TIDs whose HOT chain is all dead before `horizon` (collected while `room` > 0, up to `room`).
    dead: Vec<ItemPointerData>,
    room: usize,
    horizon: types_core::TransactionId,
}

impl SampleFilter for LeafTids {
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

    fn keep_page(&mut self, blk: BlockNumber, page: PageRef<'_>, vis: &mut [u16]) -> PgResult<usize> {
        if self.room == 0 {
            return self.pages.keep_page(page, vis);
        }
        self.pages.keep_page_dead(blk, page, vis, self.horizon, &mut self.dead, self.room)
    }
}

/// Which part of the walk the next window comes from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// The index: entries the visibility map vouches for.
    Index,
    /// The heap: the batch's other entries.
    Heap,
}

/// A participant's leaf source (the registry derives one for the leader and for each worker of a leaves site).
pub struct LeafSource {
    site: usize,
    /// A worker's (`derive`): its claims run whole on one thread inside a section, so a claim that ends before the walk
    /// did was cut short (a LIMIT met, an error) and its pins go at `release`. The leader's (in `IndexLeaves`) keeps
    /// them to `unbind`, on the backend's thread.
    worker: bool,
    recipe: LeafRecipe,
    /// The execution's slot of the site (`bind`), read at each claim's seek.
    cell: Option<Arc<LeafCell>>,
    filed: Option<Arc<Filed>>,
    view: Option<View>,
    batch: Option<IosBatch>,
    /// Per lane, a `name` cell's NAMEDATALEN image (btree stores a cstring), live until the row is copied into the window.
    name_bufs: Vec<u8>,
    heap: HeapRangeSource<LeafTids>,
    phase: Phase,
    /// The shared scan reported its end to this participant.
    done: bool,
    cur: Cursor,
    /// `(primitive scans, entries, blocks fetched, blocks hit)` of the index since the bind.
    counts: (u64, u64, u64, u64),
    stats: Option<(Oid, bool)>,
    /// The entries whose heap page the map did not vouch for since the last flush (PostgreSQL's `InstrCountTuples2`).
    fetches: u64,
    /// The heap batch's entries when the walk marks dead ones (`Filed::kill`): per entry its heap TID (`tid_key`) and its
    /// leaf (an index into `leaves`, shifted 16) and offset; per leaf its block and its LSN when read.
    tags: Vec<(u64, u64)>,
    leaves: Vec<(BlockNumber, XLogRecPtr)>,
    /// Dead entries the heap reads found, to mark when the walk has left their leaf: at most one batch's entries beside
    /// the entries of the one leaf the walk is still on (PostgreSQL's `killedItems` holds up to one leaf's,
    /// `MaxTIDsPerBTreePage`). memsig E-pub t3: charged to the last window's owner (`held`), as `mark`'s per-leaf lists are.
    cands: memledger::ChargedVec<Cand>,
    held: crate::ops::scan::heap_source::HeldOwner,
}

/// An entry whose HOT chain is all dead: its heap TID, its leaf's block, its offset there, and the leaf's LSN when the walk
/// read it.
#[derive(Clone, Copy)]
struct Cand {
    tid: ItemPointerData,
    leaf: BlockNumber,
    off: u16,
    lsn: XLogRecPtr,
}

/// A heap TID as a key in block, then offset order.
#[inline]
fn tid_key(t: &ItemPointerData) -> u64 {
    (ItemPointerGetBlockNumber(t) as u64) << 16 | ItemPointerGetOffsetNumber(t) as u64
}

/// The TIDs a batch leaves to the heap before its heap read runs: a window's rows (`IosBatch`'s largest window).
const HEAP_BATCH: usize = crate::ops::scan::index_window::IOS_MAX_ROWS;

impl LeafSource {
    pub fn new(r: &LeafRecipe, site: usize, worker: bool) -> LeafSource {
        let heap = HeapRangeSource::with_sample(r.range.clone(), LeafTids { pages: TidPages::new(), dead: Vec::new(), room: 0, horizon: types_core::InvalidTransactionId }).serving_worker(); // alloc: Node
        LeafSource { site, worker, recipe: r.clone(), cell: None, filed: None, view: None, batch: Some(IosBatch::with_byref(r.byref.clone(), &r.attnos, None, None)), name_bufs: vec![0; r.attnos.len() * crate::ops::scan::indexonly::NAMEDATALEN], heap, phase: Phase::Index, done: false, cur: Cursor::default(), counts: (0, 0, 0, 0), stats: None, fetches: 0, tags: Vec::new(), leaves: Vec::new(), cands: memledger::ChargedVec::none(), held: None } // alloc: Node
    }

    /// At a window's start: the view goes when it is another thread's or another execution's (a claim may move between
    /// pool threads; one window runs on one). `view` then reads it with no check per entry: no reference count of the
    /// shared image is touched per entry (every participant's would contend on it).
    fn check_view(&mut self) {
        let me = std::thread::current().id();
        let filed = self.filed.as_ref();
        if self.view.as_ref().is_some_and(|v| v.thread != me || !filed.is_some_and(|f| Arc::ptr_eq(&v.filed, f))) {
            self.drop_view();
        }
    }

    /// This thread's view of the bound execution's walk (`check_view` at the window's start), built at its first use on
    /// the thread, its keys rebuilt here and the scan joined to the shared state.
    fn view(&mut self, owner: HeldOwner) -> PgResult<&mut View> {
        if self.view.is_none() {
            let filed = self.filed.clone().ok_or_else(|| crate::pg_err("gravity: a leaf walk without its execution's image"))?;
            let me = std::thread::current().id();
            let scratch = NodeScratch::new("GravityIndexLeaves");
            let mcx = scratch.mcx();
            // the view's fields hold `filed` (the image) and `scratch` (the context) past `rel` and `scan` (`View`)
            let img = raw::image(&filed);
            let rel = nbtree::ImageRel::new(img, mcx)?;
            let mut scan = rel.begin_scan(mcx, filed.keys.len() as i32, filed.in_recovery)?;
            scan.parallel_scan = Some(filed.shared.clone());
            scan.xs_want_itup = true;
            let mut keys = charged_scratch::<ScanKeyData>(owner);
            keys.with_vec(|v| -> PgResult<()> {
                v.reserve_exact(filed.keys.len()); // alloc: Node charged owner
                // bounded("key-columns: one scan key per index key column, at most INDEX_MAX_KEYS (32); PG's btrescan copies them with no poll")
                for k in filed.keys.iter() {
                    v.push(SharedScanKey::key(k)?); // alloc: Node charged owner
                }
                Ok(())
            })?;
            nbtree::btrescan(&mut scan, if keys.is_empty() { None } else { Some(&keys) })?;
            if filed.kill {
                nbtree::bt_scan_lsn_with_pin(&mut scan);
            }
            self.stats = filed.stats.map(|shared| (filed.img.oid(), shared));
            self.view = Some(View { scan, vm: visibilitymap::VmBuffer::new(), rel, filed, thread: me, _scratch: scratch });
        }
        Ok(self.view.as_mut().expect("built"))
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

    /// The walk's next entry the map vouches for, its cells into `vals` / `nulls`; the others' TIDs into the heap
    /// batch until it is full. `None` when the shared scan ended or the heap batch is full.
    fn next_visible(&mut self, vals: &mut [Word], nulls: &mut [u8], owner: HeldOwner) -> PgResult<Option<ScanPos>> {
        let filed = self.filed.as_ref().ok_or_else(|| crate::pg_err("gravity: a leaf walk without its execution's image"))?;
        let ((loc, permanent, vm_blocks), kill) = (filed.vm, filed.kill);
        let heap_oid = self.recipe.heap;
        // bounded("small-fixed: every pass but the last files one TID into the heap batch, which is full at HEAP_BATCH (IOS_MAX_ROWS: MaxIndexTuplesPerPage, 408 at 8 K pages)")
        loop {
            if self.done || self.heap.sample_mut().pages.len() >= HEAP_BATCH {
                return Ok(None);
            }
            let stepped = self.view(owner).and_then(|v| nbtree::btgettuple_over(&mut v.scan, &v.rel, ForwardScanDirection).map(|got| (got, v.scan.xs_heaptid)));
            let (got, tid) = match stepped {
                Ok(x) => x,
                Err(e) => {
                    if let Some(f) = self.filed.as_ref() {
                        f.abort();
                    }
                    return Err(e);
                }
            };
            if !got {
                // the walk's end: this thread's pins go now (the scan's are gone with its last page)
                self.done = true;
                self.flush_fetches();
                if let Some(v) = self.view.as_mut() {
                    v.vm.release();
                }
                return Ok(None);
            }
            self.counts.1 += 1;
            let block = ItemPointerGetBlockNumber(&tid);
            let v = self.view.as_mut().expect("built");
            let visible = match visibilitymap::vm_all_visible_by_locator(loc, permanent, vm_blocks, block, &mut v.vm) {
                Ok(x) => x,
                Err(e) => {
                    v.filed.abort();
                    return Err(e);
                }
            };
            if !visible {
                self.fetches += 1;
                if kill {
                    self.tag(tid);
                }
                self.heap.sample_mut().pages.push(tid);
                continue;
            }
            let v = self.view.as_ref().expect("built");
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
            return Ok(Some(ScanPos::row(heap_oid, block, ItemPointerGetOffsetNumber(&tid))));
        }
    }
}

impl LeafSource {
    /// The heap-bound entry the walk returned last: its leaf position beside its TID (`tags`, `leaves`).
    fn tag(&mut self, tid: ItemPointerData) {
        let Some(v) = self.view.as_ref() else { return };
        let Some((leaf, off, lsn)) = nbtree::bt_scan_entry(&v.scan) else { return };
        if self.leaves.last() != Some(&(leaf, lsn)) {
            self.leaves.push((leaf, lsn)); // alloc: Node
        }
        self.tags.push((tid_key(&tid), ((self.leaves.len() - 1) as u64) << 16 | off as u64)); // alloc: Node
    }

    /// A heap batch is over (`all`: the claim too): each dead TID its read found becomes a candidate with its leaf
    /// position, the batch's tags go, and the candidates of every leaf the walk has left are marked on this thread
    /// (`mark`); the leaf the walk is still on keeps its own for the next batch, unless `all` or the walk ended.
    fn settle(&mut self, all: bool) {
        let on = if all || self.done { None } else { self.leaves.last().copied() };
        let dead = &mut self.heap.sample_mut().dead;
        if !dead.is_empty() {
            // bounded("small-fixed: one heap batch's tags, at most HEAP_BATCH (MaxIndexTuplesPerPage, 408 at 8 K pages), an integer sort")
            self.tags.sort_unstable_by_key(|t| t.0);
            // bounded("small-fixed: one heap batch's dead TIDs, at most HEAP_BATCH (MaxIndexTuplesPerPage, 408 at 8 K pages), a binary search each")
            for tid in dead.drain(..) {
                let Ok(i) = self.tags.binary_search_by_key(&tid_key(&tid), |t| t.0) else { continue };
                let tag = self.tags[i].1;
                let (leaf, lsn) = self.leaves[(tag >> 16) as usize];
                self.cands.with_vec(|c| c.push(Cand { tid, leaf, off: tag as u16, lsn })); // alloc: Node charged owner
            }
        }
        self.tags.clear();
        self.leaves.clear();
        self.mark(on);
    }

    /// The candidates of every leaf but `on` (the leaf, at its LSN, the walk is still on) marked per leaf, each leaf read
    /// again by block and left alone when its LSN moved (`bt_kill_by_block`). A failed read or lock leaves a leaf
    /// unmarked: the next scan that meets its entries reads their heap again.
    fn mark(&mut self, on: Option<(BlockNumber, XLogRecPtr)>) {
        if self.cands.is_empty() {
            return;
        }
        let Some(filed) = self.filed.clone().filter(|f| f.kill) else {
            self.cands.clear();
            return;
        };
        let held = self.held;
        let mut cands = core::mem::replace(&mut self.cands, crate::ops::scan::heap_source::charged_scratch(held));
        // bounded("small-fixed: one heap batch's dead TIDs (at most HEAP_BATCH, 408) and the candidates kept for the one leaf the walk is on (at most MaxTIDsPerBTreePage, 1358 at 8 K pages)")
        cands.as_mut_slice().sort_unstable_by_key(|c| (c.leaf, c.lsn, c.off, tid_key(&c.tid)));
        let mut items: memledger::ChargedVec<(u16, ItemPointerData)> = crate::ops::scan::heap_source::charged_scratch(held);
        let mut tids: memledger::ChargedVec<ItemPointerData> = crate::ops::scan::heap_source::charged_scratch(held);
        // bounded("small-fixed: one leaf read and marked per leaf of the candidates, at most one per dead TID of a heap batch (HEAP_BATCH, 408) and the one leaf kept")
        for group in cands.chunk_by(|a, b| a.leaf == b.leaf && a.lsn == b.lsn) {
            if on == Some((group[0].leaf, group[0].lsn)) {
                self.cands.with_vec(|c| c.extend_from_slice(group)); // alloc: Node charged owner
                continue;
            }
            items.clear();
            tids.clear();
            items.with_vec(|v| v.extend(group.iter().map(|c| (c.off, c.tid)))); // alloc: Node charged owner
            tids.with_vec(|v| v.extend(group.iter().map(|c| c.tid))); // alloc: Node charged owner
            // bounded("small-fixed: one leaf's candidates, at most MaxTIDsPerBTreePage (1358 at 8 K pages), an integer sort")
            tids.as_mut_slice().sort_unstable_by_key(tid_key);
            let _ = mark_leaf(&filed, group[0].leaf, group[0].lsn, &items, &tids);
        }
    }
}

/// Leaf `leaf`'s dead entries `items` (offset, heap TID; `tids` their TIDs sorted) marked when its LSN is still `lsn`
/// (`nbtree::bt_kill_by_block`), the leaf read by block through the image's locator (the index is WAL-logged: permanent).
fn mark_leaf(filed: &Filed, leaf: BlockNumber, lsn: XLogRecPtr, items: &[(u16, ItemPointerData)], tids: &[ItemPointerData]) -> PgResult<()> {
    let buf = bufmgr_seams::read_buffer_without_relcache::call(filed.img.locator(), types_core::ForkNumber::MAIN_FORKNUM, leaf, types_storage::ReadBufferMode::Normal, None, true)?;
    let Some(pin) = bufmgr_seams::BufferPin::adopt(buf) else { return Ok(()) };
    let marked = nbtree::bt_kill_by_block(pin.buffer(), lsn, items, tids);
    pin.release();
    marked.map(|_| ())
}

impl Source for LeafSource {
    fn bind(&mut self, b: &BindingRef<'_>) -> Result<()> {
        let resolved = b.resolved.and_then(|r| r.downcast_ref::<crate::ops::scan::pgrc::granule_source::Resolved>());
        self.cell = resolved.and_then(|r| r.leaf_cell(self.site)).cloned();
        self.heap.bind(b)
    }

    /// A claim's start: this participant joins the run's shared walk (its view on this thread) until the walk ends; a
    /// claim after this participant saw the end gets no row.
    fn seek(&mut self, _keys: &[Word], _keys_null: &[u8]) -> Result<()> {
        let filed = self.cell.as_ref().and_then(|c| c.get()).ok_or_else(|| Error::host("gravity: a leaf walk without its run's image"))?;
        // what an earlier claim left is that walk's, marked through its image
        self.settle(true);
        let same = self.filed.as_ref().is_some_and(|f| Arc::ptr_eq(f, &filed));
        if !same {
            self.drop_view();
            self.filed = Some(filed);
            self.done = false;
        }
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
        let mut step = cancelpoll::step("index_leaves window: one batch of entries or its heap pages");
        let owner = HeldCharge::owner_of(w);
        // memsig E-pub t3: the TID list and the dead-entry candidates on the window's owner before the walk fills them
        self.heap.sample_mut().pages.set_owner(owner);
        self.held = owner;
        crate::ops::scan::tid_pages::charged_onto(&mut self.cands, owner);
        loop {
            step.tick()?;
            if self.phase == Phase::Heap {
                let n = self.heap.window(w)?;
                if n > 0 {
                    return Ok(n);
                }
                self.settle(false);
                self.heap.sample_mut().pages.clear();
                self.phase = Phase::Index;
            }
            let heap_oid = self.recipe.heap;
            self.check_view();
            let Some(mut b) = self.batch.take() else { return Ok(0) };
            let got = b.fill_rows(|v, n| self.next_visible(v, n, owner), heap_oid, w);
            self.batch = Some(b);
            let (rows, _, _) = got.map_err(crate::stash)?;
            if rows > 0 {
                return Ok(rows);
            }
            if self.heap.sample_mut().pages.is_empty() {
                return Ok(0);
            }
            let room = self.tags.len();
            let horizon = self.filed.as_ref().map_or(types_core::InvalidTransactionId, |f| f.horizon);
            let sample = self.heap.sample_mut();
            (sample.room, sample.horizon) = (room, horizon);
            sample.pages.set_owner(crate::ops::scan::heap_source::HeldCharge::owner_of(w));
            sample.pages.order(&mut step)?;
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

    /// A claim's end, on the thread that ran it: a worker's (`worker`) marks the dead entries it has left (drops them on an
    /// unwind: a mark is a hint), and when it ended before the walk did, it leaves the walk, its view's pins (the leaf page,
    /// the map page) dropped here. The leader's call may stop between two rows and go on: its entries wait for its next
    /// batch's end or its unbind.
    fn release(&mut self) {
        if self.worker && std::thread::panicking() {
            self.heap.sample_mut().dead.clear();
            self.cands.clear();
        } else if self.worker {
            self.settle(true);
        }
        if self.worker && !self.done && self.view.is_some() {
            self.drop_view();
            self.done = true;
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

    /// The walk's counts since the bind, landed on the leader's thread (bitmap_image.rs's), the view and the map pin
    /// dropped.
    fn unbind(&mut self) {
        self.settle(true);
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

impl Drop for LeafSource {
    fn drop(&mut self) {
        self.drop_view();
    }
}

/// The leader's source of a leaves site: its share of the run's leaf walk when the run's capture held (`LeafCell`), else
/// the serial index source's walk of the whole index (exec then runs the pipeline on the leader alone).
pub struct IndexLeaves {
    serial: crate::ops::scan::index_source::IndexSource,
    leaf: LeafSource,
    /// The current claim reads the leaf walk (decided at each seek from the run's cell).
    leaf_mode: bool,
}

impl IndexLeaves {
    pub fn new(serial: crate::ops::scan::index_source::IndexSource, leaf: LeafSource) -> IndexLeaves {
        IndexLeaves { serial, leaf, leaf_mode: false }
    }
}

impl Source for IndexLeaves {
    fn bind(&mut self, b: &BindingRef<'_>) -> Result<()> {
        self.leaf_mode = false;
        self.serial.bind(b)?;
        self.leaf.bind(b)
    }

    fn seek(&mut self, keys: &[Word], keys_null: &[u8]) -> Result<()> {
        self.leaf_mode = self.leaf.cell.as_ref().is_some_and(|c| c.get().is_some());
        if self.leaf_mode { self.leaf.seek(keys, keys_null) } else { self.serial.seek(keys, keys_null) }
    }

    fn window(&mut self, w: &mut Window<'_>) -> Result<u32> {
        if self.leaf_mode { self.leaf.window(w) } else { self.serial.window(w) }
    }

    fn cursor(&self) -> &Cursor {
        if self.leaf_mode { self.leaf.cursor() } else { self.serial.cursor() }
    }

    fn cursor_mut(&mut self) -> &mut Cursor {
        if self.leaf_mode { self.leaf.cursor_mut() } else { self.serial.cursor_mut() }
    }

    fn next(&mut self, values: &mut [Word], isnull: &mut [u8]) -> Result<bool> {
        if self.leaf_mode { self.leaf.next(values, isnull) } else { self.serial.next(values, isnull) }
    }

    fn position(&self) -> ScanPos {
        if self.leaf_mode { self.leaf.position() } else { self.serial.position() }
    }

    fn lanes_hint(&self) -> usize {
        if self.leaf_mode { self.leaf.lanes_hint() } else { self.serial.lanes_hint() }
    }

    fn lane(&mut self, k: u16, sel: &[u32], out: &mut Lane) -> Result<u32> {
        if self.leaf_mode { self.leaf.lane(k, sel, out) } else { self.serial.lane(k, sel, out) }
    }

    fn refetch(&mut self, values: &mut [Word], isnull: &mut [u8]) -> Result<bool> {
        if self.leaf_mode { self.leaf.refetch(values, isnull) } else { self.serial.refetch(values, isnull) }
    }

    fn substitute(&mut self, pos: ScanPos) {
        if self.leaf_mode { self.leaf.substitute(pos) } else { self.serial.substitute(pos) }
    }

    fn pins(&self) -> u64 {
        self.serial.pins() + self.leaf.pins()
    }

    fn release(&mut self) {
        self.serial.release();
        self.leaf.release();
    }

    fn park(&mut self) {
        self.serial.park();
        self.leaf.park();
    }

    fn unpark(&mut self) {
        self.serial.unpark();
        self.leaf.unpark();
    }

    /// The slot depot's hook: both walks' (D4464).
    fn transfer_held(&mut self, to: memledger::Owner, release: memledger::Release) {
        self.serial.transfer_held(to, release);
        self.leaf.transfer_held(to, release);
    }

    fn unbind(&mut self) {
        self.leaf_mode = false;
        self.serial.unbind();
        self.leaf.unbind();
    }
}

/// A worker's source for recipe `r` at registry entry `site` (`HostRegistry::derive_worker_source`).
pub fn derive(r: &LeafRecipe, site: usize, a: gravity_driver::ArenaRef) -> gravity_driver::SourceBox {
    gravity_driver::box_source(LeafSource::new(r, site, true), a)
}

/// The leader's source of a leaves site (`derive.rs`): the serial index source of `serial` and the leaf walk of `r`.
pub fn derive_leader(serial: crate::ops::scan::index_source::IndexSource, r: &LeafRecipe, site: usize, a: gravity_driver::ArenaRef) -> gravity_driver::SourceBox {
    gravity_driver::box_source(IndexLeaves::new(serial, LeafSource::new(r, site, false)), a)
}

pub const SIZE: usize = core::mem::size_of::<IndexLeaves>();
