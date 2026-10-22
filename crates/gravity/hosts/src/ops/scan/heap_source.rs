use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use crate::ops::scan::pgrc::lanebits::LaneBits;

use bufmgr_seams::{free_access_strategy, get_access_strategy, get_access_strategy_buffer_count, get_access_strategy_with_size, lock_buffer, read_buffer, read_buffer_batched_without_relcache, read_buffer_without_relcache, read_recent_buffer, release_buffer, relation_get_number_of_blocks_in_fork, BUFFER_LOCK_SHARE, BUFFER_LOCK_UNLOCK};
use gravity_abi::ctx::lane;
use gravity_abi::site::site::{PAGE_FULL, PAGE_IOTA, PAGE_NONULLS};
use gravity_abi::{Datum as Word, Lane, ScanPos};
use gravity_driver::{BindingRef, Cursor, Error, Result, Source, Window, WindowBase};
use gravity_program::LaneId;
use heapam::HeapCheckForSerializableConflictOut;
use types_core::{BlockNumber, Buffer, ForkNumber, Oid, BLCKSZ, RELPERSISTENCE_PERMANENT, RELPERSISTENCE_TEMP, RELPERSISTENCE_UNLOGGED};
use types_error::{PgError, PgResult, ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE};
use types_rel::{Relation, RelationData, RELKIND_MATVIEW, RELKIND_RELATION, RELKIND_SEQUENCE, RELKIND_TOASTVALUE};
use types_snapshot::{SnapshotData, XidVisMemo};
use types_storage::buf::{BufferAccessStrategy, BufferAccessStrategyType};
use types_storage::bufpage::{MaxHeapTuplesPerPage, PageRef};
use types_storage::{ReadBufferMode, RelFileLocator};
use types_tuple::tupdesc::CompactAttribute;
use types_tuple::{HeapTupleData, ItemPointerData, HEAP_HASNULL};

use crate::ops::scan::index_source::{open_heap, park};
use crate::ops::shared::rel::snapshot_ptr;
use crate::ops::scan::heap_window::{count_hinted, deform_rows, hinted_visible, reach, Lanes, Shape, LANE_LATE, PAGE_ALL_VISIBLE};
use crate::ops::scan::page_hints::PageHints;
use crate::scratch::NodeScratch;

use crate::ops::scan::heap_window::{PAGE_ROWS, PAGE_VALIDITY_WORDS};

mod raw;
use raw::{collect_visible, normal_line};

static RAW_WINDOWS: AtomicU64 = AtomicU64::new(0);
static DEFORMED_PAGES: AtomicU64 = AtomicU64::new(0);

/// `(raw windows, pages deformed into lanes)` so far in this process: a page the program read alone never counts in the second.
pub fn page_census() -> (u64, u64) {
    (RAW_WINDOWS.load(Ordering::Relaxed), DEFORMED_PAGES.load(Ordering::Relaxed))
}

#[derive(Clone)]
pub struct RangeRecipe {
    pub heap: Oid,
    pub natts: usize,
    pub attnos: Arc<[i16]>,
    want: usize,
    atts: Arc<[CompactAttribute]>,
    heap_natts: i32,
    relam: Oid,
    /// Lane `k` set: a by-reference (varlena) column published as its datum pointer into the
    /// pinned page (`KIND_BYREF`; milestone-2 §2.6 item 22).
    byref: LaneBits,
    /// Per deformed cell `i < want`: `(true, word)` = the column's `attmissingval` (C `getmissingattr`) for a tuple stored
    /// before the column existed; the deform NULL-fills that tail. A by-reference default's word addresses its image in
    /// `missing_imgs` (the lane publishes a datum pointer, as a page cell's).
    missing: Arc<[(bool, u64)]>,
    /// The by-reference fast defaults' images, copied out of the relcache at the recipe's build (the recipe outlives any
    /// one relcache entry; its key's relcache epoch retires it at an ALTER), 8-byte aligned.
    missing_imgs: Arc<[Box<[u64]>]>,
    /// Road `heap.stream` (V4R-32): the program's `VERDICT_STREAM` enable, set by exec's `src.verdicts` at prepare (the bit
    /// is the program's, the same for every execution of this recipe); off until then (a recipe no v4 program declared).
    stream: Arc<AtomicBool>,
    /// D3585: a relation-set site's relations (`SiteDesc::set`), `heap` the first: twins of one layout (decompose
    /// `Facts::heap_twin`), read in this order through one claim space (`RelSet`). `None`: `heap` alone.
    pub set: Option<Arc<[Oid]>>,
    /// D3663: `set` is an Append's members its initial pruning narrows (`SiteDesc::set_prune`): every execution binds the
    /// survivors filed for it (exec append/proto.rs `Initial::file_sets`), never the whole set.
    pub set_pruned: bool,
    /// D3670: a rerun set's or an InitPlan's members drive this site (exec admit `AdmitCx::run_counted`): the leader's bind
    /// counts no scan, and each run of a member counts one (`Source::count_scan`), as PG's initscan counts at a scan's
    /// begin and at every rescan; a member that never runs counts none.
    pub run_counted: bool,
    /// D3670: no pipeline drives this site (a nest's or a SubPlan's embedded inner, exec admit `AdmitCx::probed`): the
    /// leader's bind counts no scan, and each seek (a re-open's first fetch) counts one, a relation set's for each relation
    /// its walk reaches; a re-open never fetched counts none.
    pub seek_counted: bool,
}

impl RangeRecipe {
    /// Road `heap.stream` on or off for this recipe's scans (exec's `src.verdicts`, V4R-32).
    pub fn set_stream(&self, on: bool) {
        self.stream.store(on, Ordering::Relaxed);
    }
}

fn err(msg: &'static str) -> Box<PgError> {
    crate::pg_err(msg)
}

/// D3887: the error a heap walk returns when it stopped for its statement's cancel (`crate::stash` reports it as the
/// kernel's cancel).
const WALK_CANCELED: &str = "gravity: a heap walk stopped for its statement's cancel";

#[cold]
pub(crate) fn unpopulated(name: &str) -> Box<PgError> {
    let e = PgError::error(format!("materialized view \"{name}\" has not been populated")); // alloc: Cold
    Box::<PgError>::new(e.with_sqlstate(ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE).with_hint("Use the REFRESH MATERIALIZED VIEW command.")) // alloc: Cold
}

impl RangeRecipe {
    /// A worker range scan over `heap` projecting `attnos`: by-value, live
    /// columns; temp tables are refused (their buffers are not reachable by locator).
    pub fn from_plan(heap: &Relation<'_>, attnos: &[i16]) -> PgResult<RangeRecipe> {
        RangeRecipe::from_plan_byref(heap, attnos, &[])
    }

    /// `from_plan` with the lanes in `byref` (lane-set words over `attnos`) being varlena columns published
    /// as datum pointers into the pinned page — valid while the window is current, never
    /// detoasted (a compressed or external image is the kernel's to refuse). The other lanes
    /// follow `from_plan`'s rule.
    pub fn from_plan_byref(heap: &Relation<'_>, attnos: &[i16], byref: &[u64]) -> PgResult<RangeRecipe> {
        let table_am = matches!(heap.rd_rel.relkind, RELKIND_RELATION | RELKIND_MATVIEW | RELKIND_TOASTVALUE);
        // a sequence's one tuple is a heap page's (no table AM: relam 0), which PostgreSQL's SeqScan reads as heap
        let sequence = heap.rd_rel.relkind == RELKIND_SEQUENCE;
        if !sequence && (!table_am || tableam_vocab::TableAm::of(heap) != Some(tableam_vocab::TableAm::Heap)) {
            return Err(err("gravity: heap AM only"));
        }
        let byref = LaneBits::from_words(attnos.len(), byref);
        let mut want = 0usize;
        // bounded("key-columns: one attno test per scanned column, at plan time, at most MaxTupleAttributeNumber (1664) columns")
        for (k, &a) in attnos.iter().enumerate() {
            // V4R-88: a system lane (xmin / cmin / xmax / cmax), filled from the tuple header; never deformed, never by-reference
            if crate::ops::shared::heap_fetch::is_sys(a) {
                if byref.has(k) {
                    return Err(err("gravity: a by-reference system lane"));
                }
                continue;
            }
            if a < 1 || a as i32 > heap.rd_att.natts {
                return Err(err("gravity: range source attno"));
            }
            let c = &heap.rd_att.compact_attrs[a as usize - 1];
            // a by-reference lane: a varlena or a fixed-width by-reference column (its image is `typlen` bytes at the datum: the
            // consumer copies by the out column's typlen); a plain lane over a by-reference column is a driving lane (its pointer
            // word is never read: the lowerer's columnless scan). Item 12 of r1-index, re-landed once E2 (Memoize instrumentation
            // to the row executor) landed and E1 (a completion run's row order) was ruled: multiset-identical to PostgreSQL is
            // the bar, an order-unspecified diff is annotated (regress-residual.py), not fixed in the engine.
            // D2789: a dropped attribute is read for a whole row (decompose `whole_row_atts`): the deform reads its stored datum
            // by the layout the descriptor keeps (`attlen` / `attalign` / `attbyval`), as the row executor's slot does
            if byref.has(k) && c.attbyval {
                return Err(err("gravity: a by-reference lane over a by-value column"));
            }
            want = want.max(a as usize);
        }
        let mut atts: Vec<CompactAttribute> = Vec::with_capacity(want); // alloc: Program
        let mut missing: Vec<(bool, u64)> = Vec::with_capacity(want); // alloc: Program
        let mut imgs: Vec<Box<[u64]>> = Vec::new(); // alloc: Program
        // bounded("key-columns: one attribute copy per column up to the last one read, at plan time, at most MaxTupleAttributeNumber (1664) columns")
        for i in 0..want {
            let slot = heap.rd_att.compact_attrs[i].clone();
            slot.attcacheoff.set(-1);
            let m = if slot.atthasmissing { heap.rd_att.constr.as_ref().and_then(|c| c.missing.get(i)).filter(|m| m.am_present) } else { None };
            let word = match m {
                Some(m) if !slot.attbyval => {
                    let p = m.am_value.as_u64() as *const u8;
                    // a present by-reference attmissingval addresses its image in the relcache entry (raw::missing_len)
                    let len = raw::missing_len(slot.attlen, p);
                    let mut img = vec![0u64; len.div_ceil(8)].into_boxed_slice(); // alloc: Program
                    // `img` holds at least `len` bytes; the source is `len` bytes (above)
                    raw::copy_missing(p, &mut img, len);
                    let w = img.as_ptr() as u64;
                    imgs.push(img);
                    (true, w)
                }
                Some(m) => (true, m.am_value.as_u64()),
                None => (false, 0),
            };
            missing.push(word);
            atts.push(slot);
        }
        Ok(RangeRecipe { heap: heap.rd_id, natts: attnos.len(), attnos: Arc::from(attnos), want, atts: Arc::from(atts), heap_natts: heap.rd_att.natts, relam: heap.rd_rel.relam, byref, missing: Arc::from(missing), missing_imgs: Arc::from(imgs), stream: Arc::new(AtomicBool::new(false)), set: None, set_pruned: false, run_counted: false, seek_counted: false }) // alloc: Program
    }
}

/// The relation's main-fork block count, for the leader's block-range claims.
pub fn nblocks(mcx: mcx::Mcx<'_>, heap: Oid) -> PgResult<u64> {
    let rel = table::table_open(mcx, heap, types_storage::lock::NoLock)?;
    let n = relation_get_number_of_blocks_in_fork::call(&rel, ForkNumber::MAIN_FORKNUM);
    rel.close(types_storage::lock::NoLock)?;
    Ok(n? as u64)
}

/// D3867: the line pointers of the relation's main fork while it holds at most `max_blocks` blocks (each page read pinned and
/// share-locked, as a scan reads it), `None` past that. Every tuple a heap scan can return has a line pointer, so the count
/// bounds the rows a scan of the relation reads now; tier 0's gate reads it for a relation no ANALYZE has counted.
pub fn small_line_pointers(mcx: mcx::Mcx<'_>, heap: Oid, max_blocks: u64) -> PgResult<Option<u64>> {
    let rel = table::table_open(mcx, heap, types_storage::lock::NoLock)?;
    let counted = (|| -> PgResult<Option<u64>> {
        let n = relation_get_number_of_blocks_in_fork::call(&rel, ForkNumber::MAIN_FORKNUM)? as u64;
        if n > max_blocks {
            return Ok(None);
        }
        let mut lines = 0u64;
        // bounded("small-fixed: at most max_blocks pages (the test above), UNCOUNTED_BLOCKS (8) at the one caller; a pinned read of a line pointer count each")
        for blk in 0..n {
            let buf = read_buffer::call(&rel, blk as BlockNumber)?;
            // the pin is released whatever the lock's outcome
            let read = lock_buffer::call(buf, BUFFER_LOCK_SHARE).and_then(|()| {
                lines += raw::pinned_page(buf).max_offset_number() as u64;
                lock_buffer::call(buf, BUFFER_LOCK_UNLOCK)
            });
            release_buffer::call(buf)?;
            read?;
        }
        Ok(Some(lines))
    })();
    rel.close(types_storage::lock::NoLock)?;
    counted
}

/// D3585: a relation-set site's relations as one execution reads them, resolved on the leader where its claims are cut
/// (exec runner plan.rs `claims_of`) and handed to each participant's source through the execution's `Resolved`: each
/// relation's storage and block count, and its first block in the claims' space, which tiles the relations' blocks in
/// set order. A claim never crosses a relation (`bounds`); a whole-range read (`MorselRange::ALL`) walks them in turn.
pub struct RelSet {
    rels: Box<[SetRel]>,
    /// `starts[i]`: relation `i`'s first block in the claims' space; `starts[n]` the total.
    starts: Box<[u64]>,
}

#[derive(Clone, Copy)]
struct SetRel {
    oid: Oid,
    locator: RelFileLocator,
    nblocks: BlockNumber,
    /// `(relisshared, pgstat_enabled)`.
    shared: bool,
    stats: bool,
}

impl RelSet {
    /// Each relation of `recipe`'s set opened (the plan holds its lock) and held to the recipe's layout: a relation
    /// changed since the lowering (another attribute count or access method, a persistence the recipe does not read)
    /// is the parked plan's error, as a one-relation site's bind raises it.
    pub fn resolve(mcx: mcx::Mcx<'_>, recipe: &RangeRecipe) -> PgResult<RelSet> {
        RelSet::resolve_kept(mcx, recipe, &|_| true)
    }

    /// D3663: `resolve` over the members `keep` names by their position in the set (an Append's initial pruning keeps
    /// them, exec append/proto.rs `Initial::file_sets`): no other member is opened or sized.
    pub fn resolve_kept(mcx: mcx::Mcx<'_>, recipe: &RangeRecipe, keep: &dyn Fn(usize) -> bool) -> PgResult<RelSet> {
        let set: &[Oid] = recipe.set.as_deref().unwrap_or(core::slice::from_ref(&recipe.heap));
        let mut rels = Vec::with_capacity(set.len()); // alloc: Exec
        let mut starts = Vec::with_capacity(set.len() + 1); // alloc: Exec
        let mut total = 0u64;
        // bounded("catalog-sized: one relation per member of the site's relation set (an Append's arms, a partitioned table's leaves), opened and sized as PG's executor opens each at its start")
        for (_, &oid) in set.iter().enumerate().filter(|&(i, _)| keep(i)) {
            let heap = table::table_open(mcx, oid, types_storage::lock::NoLock)?;
            let same = heap.rd_att.natts == recipe.heap_natts && heap.rd_rel.relam == recipe.relam && heap.rd_rel.relpersistence == RELPERSISTENCE_PERMANENT && !heap.uses_local_buffers();
            let n = if same { relation_get_number_of_blocks_in_fork::call(&heap, ForkNumber::MAIN_FORKNUM) } else { Ok(0) };
            let r = SetRel { oid, locator: heap.rd_locator.get(), nblocks: 0, shared: heap.rd_rel.relisshared, stats: heap.pgstat_enabled.get() };
            heap.close(types_storage::lock::NoLock)?;
            if !same {
                return Err(err("gravity: relation changed under the parked plan"));
            }
            let nblocks = n?;
            starts.push(total); // alloc: Exec
            total += nblocks as u64;
            rels.push(SetRel { nblocks, ..r }); // alloc: Exec
        }
        starts.push(total); // alloc: Exec
        Ok(RelSet { rels: rels.into_boxed_slice(), starts: starts.into_boxed_slice() })
    }

    /// The blocks of every relation.
    pub fn total(&self) -> u64 {
        self.starts.last().copied().unwrap_or(0)
    }

    /// The relations' first blocks past the first: the claim boundaries (`RangeCursor::with_boundaries`).
    pub fn bounds(&self) -> &[u64] {
        &self.starts[1.min(self.starts.len())..self.starts.len().saturating_sub(1)]
    }

    pub fn len(&self) -> usize {
        self.rels.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rels.is_empty()
    }

    /// The relation holding block `g` of the claims' space (`g < total`): the last whose first block is at most `g` (an
    /// empty relation's first block is its successor's).
    fn at(&self, g: u64) -> usize {
        self.starts[..self.rels.len()].partition_point(|&s| s <= g).saturating_sub(1)
    }
}

/// TABLESAMPLE's choice over the heap walk (`v3_r1c.rs`): started by the seek's key words, then the blocks and the offsets
/// PostgreSQL's TSM picks (a picked offset is kept when it is visible, as `heapam_scan_sample_next_tuple`).
/// A type parameter, not a trait object: `NoSample` is zero-sized and `ACTIVE = false`, so the plain walk compiles to the
/// source without the hook.
pub trait SampleFilter {
    const ACTIVE: bool = true;
    /// The filter's blocks are sparse (a bitmap's): each is read alone, no combined read or advice past it.
    const SPARSE: bool = false;
    /// `keep_page` (the page's line pointers, under the share lock) decides in place of `keep`.
    const PAGE: bool = false;
    /// A bitmap heap scan's walk: pgstat counts no sequential scan and its tuples as fetches (`idx_tup_fetch`), as heapam's
    /// bitmap scan (`pgstat_count_heap_fetch` per tuple returned, `initscan` counting only a sequential scan).
    const BITMAP: bool = false;
    /// A large relation's walk reads through the bulk-read ring (a sequential walk's); a probe's sparse reads do not
    /// (PostgreSQL's index scan takes no strategy).
    const RING: bool = true;
    fn start(&mut self, keys: &[Word], keys_null: &[u8]) -> PgResult<()>;
    fn next_block(&mut self, nblocks: BlockNumber) -> PgResult<Option<BlockNumber>>;
    fn keep(&mut self, blk: BlockNumber, maxoff: u16, vis: &mut [u16]) -> usize;
    /// The visible tuples `vis` of pinned, share-locked page `page` (block `blk`) to keep, compacted to the front.
    fn keep_page(&mut self, _blk: BlockNumber, _page: PageRef<'_>, vis: &mut [u16]) -> PgResult<usize> {
        Ok(vis.len())
    }
    /// The current page's window flags (`site::PAGE_RECHECK`), published on its host-deformed window.
    fn page_flags(&self) -> u16 {
        0
    }
    /// The source's bind (the bind thread): a filter takes its execution's state from the binding here (D3117: a nest
    /// probe's image, from the `Resolved`).
    fn bind(&mut self, _b: &BindingRef<'_>) -> PgResult<()> {
        Ok(())
    }
    /// The source's unbind (the bind thread): a filter's own counts land here.
    fn unbind(&mut self) {}
    /// CONVERT wave 2: the owner the source's bytes are charged to, named at each window (`HeldCharge::owner_of`): a
    /// filter charges its own scratch to it.
    fn home(&mut self, _owner: HeldOwner) {}
    /// D4464: the source's `Source::transfer_held` (the slot depot's holder at a worker set's park, the taking statement
    /// at its take): a filter that keeps contexts of its own moves them to `to`.
    fn transfer_held(&mut self, _to: memledger::Owner, _release: memledger::Release) {}
}

pub struct NoSample;

impl SampleFilter for NoSample {
    const ACTIVE: bool = false;
    #[inline(always)]
    fn start(&mut self, _keys: &[Word], _keys_null: &[u8]) -> PgResult<()> {
        Ok(())
    }
    #[inline(always)]
    fn next_block(&mut self, _nblocks: BlockNumber) -> PgResult<Option<BlockNumber>> {
        Ok(None)
    }
    #[inline(always)]
    fn keep(&mut self, _blk: BlockNumber, _maxoff: u16, vis: &mut [u16]) -> usize {
        vis.len()
    }
}

/// CONVERT wave 2: the heap a scan source keeps in its own buffers, charged while it is bound to the
/// breaker its pipeline feeds, else to the statement or the portal (on the statement from its bind, `of_statement`, then
/// on the owner each window names at its start, `owner_of`); at the unbind to the backend's session (the parked Node keeps
/// the buffers past the statement), and where the slot depot's `Source::transfer_held` sends a parked one (the depot's
/// holder at a worker set's park, the taking statement at its take). The source re-syncs it after each window and at its
/// unbind, so the charge follows the buffers' capacity, lagging by what the binding's `lane` / `fetch` / `dict_value`
/// calls grew since its last window. It drops with the source (declared first: its bytes move to the dropping thread's
/// account before the frees).
pub(crate) struct HeldCharge {
    charge: memledger::Charge,
    /// The owner and kind the charge is on while bound (the statement's from the bind, then each window's); `None`
    /// before the bind homes it (or, with no statement context, until a window names an owner) and once parked.
    at: HeldOwner,
    live: bool,
}

/// The owner a source's bytes are charged to and its kind lane (`Window::op_owner`'s shape).
pub type HeldOwner = Option<(memledger::Owner, memledger::Kind)>;

/// An empty scratch charged to `owner` as it grows (uncharged with none).
pub(crate) fn charged_scratch<T>(owner: HeldOwner) -> memledger::ChargedVec<T> {
    memledger::ChargedVec::new(owner.map(|o| o.0), owner.map_or(memledger::Kind::Region, |o| o.1)) // alloc: Node charged owner
}

/// The release a charge on `o` states (memledger `ChargedVec`'s rule).
pub(crate) fn release_of(o: memledger::Owner) -> memledger::Release {
    match o {
        memledger::Owner::Portal { .. } => memledger::Release::Portal,
        memledger::Owner::Session(_) => memledger::Release::Session,
        memledger::Owner::Cache(_) => memledger::Release::Evict,
        memledger::Owner::Statement { .. } | memledger::Owner::WorkerSet(_) | memledger::Owner::Op { .. } => memledger::Release::Statement,
    }
}

impl HeldCharge {
    pub(crate) const fn new() -> HeldCharge {
        HeldCharge { charge: memledger::Charge::NONE, at: None, live: false }
    }

    /// The slot the window's pipeline produces into (its breaker's kind), else the statement or the portal
    /// (`Kind::Region`); `None` with neither (a rig's window).
    pub(crate) fn owner_of(w: &Window<'_>) -> HeldOwner {
        w.op_owner().or_else(|| w.stmt_owner().map(|(o, _)| (o, memledger::Kind::Region)))
    }

    /// The bind's owner until a window names the breaker: the statement or the portal (`gravity_driver::stmt_context`;
    /// a bound source no window drives keeps its buffers for the statement).
    pub(crate) fn of_statement() -> HeldOwner {
        gravity_driver::stmt_context().map(|(o, _)| (o, memledger::Kind::Region))
    }

    /// The owner it is charged to while bound.
    pub(crate) fn owner(&self) -> HeldOwner {
        self.at
    }

    /// On `to` from now on, when it was on another owner or on none: the bytes the old charge held move there with it in
    /// one step (D4536: dropped and re-opened empty, they sat on the thread's account, unowned, until the window's `sync`).
    /// True when it moved (a source hands its cursors the new owner).
    #[inline]
    pub(crate) fn home(&mut self, to: HeldOwner) -> bool {
        let Some((o, k)) = to.filter(|&t| self.at != Some(t)) else { return false };
        let old = core::mem::replace(&mut self.charge, memledger::Charge::open(o, release_of(o), k, memledger::charge::Door::Heap, 0));
        self.charge.absorb(old);
        (self.at, self.live) = (to, true);
        true
    }

    /// The counted buffers hold `bytes` now: the charge follows (a commit of what grew, a free of what shrank).
    pub(crate) fn sync(&mut self, bytes: u64) {
        if !self.live {
            return;
        }
        let have = self.charge.committed();
        if bytes > have {
            self.charge.commit(bytes - have);
        } else if bytes < have {
            self.charge.free(have - bytes);
        }
    }

    /// D4536: a counted buffer of `bytes` just made, charged now rather than at the next `sync` (a lane's buffer made
    /// after the window's sync would otherwise sit on the thread's account until the next window).
    pub(crate) fn grow(&mut self, bytes: u64) {
        if self.live {
            self.charge.commit(bytes);
        }
    }

    /// The unbind: `bytes` kept past the statement, on the backend's session from now on.
    pub(crate) fn park(&mut self, bytes: u64) {
        self.sync(bytes);
        if let Some((o, _)) = self.at.take() {
            self.charge.transfer(memledger::Owner::Session(o.acct()), memledger::Release::Session);
        }
    }

    /// `Source::transfer_held`: an unbound source's parked bytes to `to` (the slot depot's holder at a worker set's park,
    /// the taking statement at its take); a bound source keeps its owner, and the next bind or window re-homes it.
    pub(crate) fn transfer(&mut self, to: memledger::Owner, release: memledger::Release) {
        if self.live && self.at.is_none() {
            self.charge.transfer(to, release);
        }
    }
}

pub struct HeapRangeSource<S: SampleFilter = NoSample> {
    held: HeldCharge,
    recipe: RangeRecipe,
    sample: S,
    parked: Option<Weak<RelationData<'static>>>,
    /// D3663: the relation `parked` holds (a set's walk moves `recipe.heap` to the relation it enters).
    parked_oid: Oid,
    /// The thread whose relcache holds `parked` (the relcache is thread-local; a pool thread never touches it).
    bind_thread: Option<std::thread::ThreadId>,
    locator: RelFileLocator,
    permanent: bool,
    /// A temporary relation: its pages are the backend's local buffers (thread-local), read through the parked
    /// relation on `bind_thread` alone (item 11 of r1-index: the pipeline is serial on the leader, `Reason::TempSource`).
    local: bool,
    nblocks: BlockNumber,
    /// The leader's registered snapshot for this statement, by pointer (`snapshot_ptr`); null unbound.
    snapshot: *const SnapshotData<'static>,
    lo: u64,
    hi: u64,
    block: u64,
    buf: Option<Buffer>,
    vis: [u16; MaxHeapTuplesPerPage],
    nvis: usize,
    blocks_fetched: u64,
    bound: bool,
    lanes: Vec<u64>,
    valid: Vec<u64>,
    /// One deformed tuple's cells (`want` of them), reused per row.
    vals: Vec<datum::Datum>,
    nulls: Vec<bool>,
    /// The EPQ position the driver substituted for this site: the next window is that row alone.
    substituted: Option<ScanPos>,
    /// The page protocol (`SiteDesc::raw_pages`): the window is the pinned page, its visible offsets and its flags; the lanes are
    /// deformed on the first `lane` request of a window (`filled`) — a page the flags do not clear.
    raw: bool,
    /// The attribute every row of the staged page is deformed to (0 = none yet this window).
    filled: usize,
    /// Late decode (M2-inventory row 8): the attribute the rows of `sel_rows` are deformed to, and that selection.
    sel_filled: usize,
    sel_rows: Vec<u32>,
    /// V4R-88: every row's system lanes filled this window (a full-page deform fills them with the attributes).
    sys_filled: bool,
    cur: Cursor,
    scratch: NodeScratch,
    /// `(relisshared, tuples, blocks fetched, blocks hit)` of a scan whose relation has stats (`pgstat_enabled`): C's
    /// `pgstat_count_heap_scan` at the bind, the visible tuples staged (`pgstat_count_heap_getnext`) and the block reads
    /// (`pgstat_count_buffer_read` / `_hit`: the hit is this thread's `shared_blks_hit` delta) landed at the unbind, batched.
    stats: Option<(bool, u64, u64, u64)>,
    /// D3670: the scan the leader's bind left to the member runs or the seeks (`RangeRecipe::run_counted` /
    /// `seek_counted`): `(relation, relisshared, index)`, the bound relation's sequential scan, or the plan's btree walk's
    /// index scan when `index`; a member run counts the set's other survivors beside it, as a bind does (a probed set
    /// counts the relations its walk reaches). `None`: the bind counted its scan, or there is none to count.
    run_scan: Option<(Oid, bool, bool)>,
    /// D3670: the leader's bind of a member-driven site (`RangeRecipe::run_counted`): `count_scan` counts at each run.
    run_counted: bool,
    /// D3670: the leader's bind of a probed site (`RangeRecipe::seek_counted`): each seek counts (`reach` for a set).
    seek_count: bool,
    /// D3670: the relations of the set this re-open of a probed site has counted (its walk's first `reached`).
    reached: usize,
    /// The bind's `check_for_serializable_conflict_out_needed` (asked again per page on the bind thread, as heapam's page scan):
    /// the conflict check needs the backend's predicate state, so a serializable claim never stages off the bind thread.
    ssi: bool,
    /// The staged page's `page_flags` (`PAGE_NONULLS` / `PAGE_FULL` over its visible tuples), from `collect_visible`'s walk.
    flags: u16,
    /// `(direct, tested)`: pages answered from their line pointers (PD_ALL_VISIBLE, a snapshot not taken in recovery, no
    /// serializable check) and pages whose tuples went through the MVCC seam, since the bind.
    pages: (u64, u64),
    /// Per claim (the seek), not per page: the seek's thread is the bind thread, and the parked relation upgraded there (the
    /// prune and the serializable check take it per page).
    on_bind_thread: bool,
    /// D1408: the thread of the claim that pins `buf` (set at the seek): a pin is its thread's (bufmgr's private refcount), never
    /// released off it (a panicked claim's pin, released at the leader's unbind, was "buffer is not pinned", then an abort).
    claim_thread: Option<std::thread::ThreadId>,
    rel: Option<Rc<RelationData<'static>>>,
    /// The next block's buffer guess (the last pinned shared buffer + 1: a table loaded or first read in block order sits in
    /// consecutive buffers; 0 = none), verified by tag through `read_recent_buffer` (C `ReadRecentBuffer`: the header lock and
    /// one pin, no partition lock, no hash probe; a wrong guess falls back), bounded by `nbuffers`; `recent` counts its pages
    /// since the bind (the census; the pg_stat_io hits C's `PinBufferForBlock` counts per block, drained at the unbind).
    guess: Buffer,
    nbuffers: Buffer,
    recent: u64,
    /// Where each block was last pinned (shared per relation file; a parallel-filled pool is not in block order): tried before the guess, verified the same way.
    hints: Option<PageHints>,
    /// heapam's bulk-read ring for a relation over a quarter of the pool (`initscan`), so a large scan recycles its own buffers.
    strategy: BufferAccessStrategy,
    /// Road `heap.stream`: the statement's read-ahead window over the relation (`v4::m2::heap::ReadAhead`), shared by its workers.
    ahead: Option<Arc<crate::ops::scan::heap_window::ReadAhead>>,
    /// D865: blocks a claim off the bind thread staged with a valid `pd_prune_xid`. PostgreSQL's parallel workers prune every
    /// page they visit (heap_prepare_pagescan -> heap_page_prune_opt); the prune needs the backend's relation, GlobalVis horizons,
    /// WAL and pgstat state, which live on the bind thread, so these pages are pruned there at the unbind, once the claims are done.
    defer_prune: Vec<BlockNumber>,
    /// D3014O: a worker's frozen-image index probe (`index_probe.rs`): the site is an index site a worker calls
    /// (`Source::serves_worker`), whose lanes are the attribute lanes then the index site's RowId lane (no offset lane):
    /// each kept row's heap tid (`tid_word`), as `IosBatch` publishes it.
    worker: bool,
    tids: Vec<u64>,
    /// D3475: the plan's full btree walk this range reads in its place (`SiteDesc::walk`: the index, index-only): pg_stat
    /// counts the walk, not a sequential scan. `walk_fetched`: the visible tuples staged on pages without PD_ALL_VISIBLE
    /// since the bind (an index-only walk's heap fetches).
    walk: Option<(Oid, bool)>,
    walk_fetched: u64,
    /// D3585: a relation-set site's registry index (`at_set`), under which the bind finds the execution's `RelSet`.
    set_at: Option<usize>,
    set: Option<Arc<RelSet>>,
    /// The set's relation the walk reads (`usize::MAX`: none since the bind); `recipe.heap`, `locator`, `nblocks`, the
    /// hints, the read-ahead and `stats` are that relation's. `block` / `hi` are its blocks; `set_hi` the seek's end in
    /// the claims' space (a whole-range read walks on into the next relations).
    set_cur: usize,
    set_hi: u64,
    /// The bind was the leader's (a worker's read-ahead joins the leader's window).
    leader: bool,
    /// The counts of the set's relations the walk left, landed at the unbind: `(relation, relisshared, tuples, blocks
    /// fetched, blocks hit)`.
    set_counts: Vec<(Oid, bool, u64, u64, u64)>,
    /// D865 for a set: the pages a claim off the bind thread staged with a prune xid, by relation index.
    set_prune: Vec<(u32, BlockNumber)>,
    /// D3586: the source's registry index (`at_site`; `usize::MAX`: a source built outside a program's registry), which
    /// the bind asks the binding's `Resolved` about (`site_pruned`).
    site: usize,
}

impl HeapRangeSource {
    pub fn new(recipe: RangeRecipe) -> HeapRangeSource {
        HeapRangeSource::with_sample(recipe, NoSample)
    }
}

impl<S: SampleFilter> HeapRangeSource<S> {
    pub fn page_paths(&self) -> (u64, u64) {
        self.pages
    }

    /// The walk's filter (a leaf walk loads the TIDs it reads through it, index_leaves.rs).
    pub fn sample_mut(&mut self) -> &mut S {
        &mut self.sample
    }

    pub fn with_sample(recipe: RangeRecipe, sample: S) -> HeapRangeSource<S> {
        let natts = recipe.natts;
        let want = recipe.want;
        HeapRangeSource {
            held: HeldCharge::new(),
            recipe,
            sample,
            parked: None,
            parked_oid: 0,
            bind_thread: None,
            locator: RelFileLocator::new(0, 0, 0),
            permanent: true,
            local: false,
            nblocks: 0,
            snapshot: core::ptr::null(),
            lo: 0,
            hi: 0,
            block: 0,
            buf: None,
            vis: [0; MaxHeapTuplesPerPage],
            nvis: 0,
            blocks_fetched: 0,
            bound: false,
            lanes: vec![0; natts * PAGE_ROWS], // alloc: Node
            valid: vec![0; natts * PAGE_VALIDITY_WORDS], // alloc: Node
            vals: vec![datum::Datum::from_u64(0); want], // alloc: Node
            nulls: vec![false; want], // alloc: Node
            substituted: None,
            raw: false,
            filled: 0,
            sel_filled: 0,
            sel_rows: Vec::new(), // alloc: Node
            sys_filled: false,
            cur: Cursor::new(),
            scratch: NodeScratch::new("GravityHeapRangeSource"),
            stats: None,
            run_scan: None,
            run_counted: false,
            seek_count: false,
            reached: 0,
            ssi: false,
            flags: 0,
            pages: (0, 0),
            on_bind_thread: false,
            claim_thread: None,
            rel: None,
            guess: 0,
            hints: None,
            nbuffers: 0,
            recent: 0,
            strategy: None,
            ahead: None,
            defer_prune: Vec::new(), // alloc: Node
            worker: false,
            tids: Vec::new(), // alloc: Node
            walk: None,
            walk_fetched: 0,
            set_at: None,
            set: None,
            set_cur: usize::MAX,
            set_hi: 0,
            leader: true,
            set_counts: Vec::new(), // alloc: Node
            set_prune: Vec::new(), // alloc: Node
            site: usize::MAX,
        }
    }

    /// Pages pinned through the recent-buffer guess since the bind.
    pub fn recent_pins(&self) -> u64 {
        self.recent
    }

    /// D3014O: the source of an index site a worker probes (`index_probe::derive`).
    pub fn serving_worker(mut self) -> HeapRangeSource<S> {
        self.worker = true;
        self.tids = vec![0; PAGE_ROWS]; // alloc: Node
        self
    }

    pub fn raw(mut self, on: bool) -> HeapRangeSource<S> {
        self.raw = on;
        self
    }

    /// D3586: the registry index the source serves (pgrc derive.rs, a program's range site).
    pub fn at_site(mut self, site: usize) -> HeapRangeSource<S> {
        self.site = site;
        self
    }

    /// D3475: the plan's full btree walk read as this range (`SiteDesc::walk`): its index and whether it is index-only.
    pub fn walk(mut self, walk: Option<(u32, bool)>) -> HeapRangeSource<S> {
        self.walk = walk.map(|(index, only)| (index as Oid, only));
        self
    }

    /// D3585: the source of relation-set site `i` (the registry index the execution's `RelSet` is filed under).
    pub fn at_set(mut self, i: usize) -> HeapRangeSource<S> {
        self.set_at = Some(i);
        self
    }

    pub fn blocks_fetched(&self) -> u64 {
        self.blocks_fetched
    }

    pub fn nblocks(&self) -> BlockNumber {
        self.nblocks
    }

    fn bind_pg(&mut self, b: &BindingRef<'_>) -> PgResult<()> {
        // D3586 / D3597: a plain scan of an Append arm this execution's initial pruning removed (`Resolved::site_pruned`),
        // or (D3663) a relation set it removed whole; any other relation-set site (D3585) reads its execution's relations
        // as `claims_of` or `Initial::file_sets` resolved them
        if !S::ACTIVE && !S::BITMAP && self.walk.is_none() && !self.worker && b.resolved.and_then(|r| r.downcast_ref::<crate::ops::scan::pgrc::granule_source::Resolved>()).is_some_and(|r| r.site_pruned(self.site)) {
            self.bind_pruned(b);
            return Ok(());
        }
        if S::ACTIVE {
            self.sample.bind(b)?;
        }
        if let Some(i) = self.set_at {
            // the set's first relation is bound, parked and counted as a one-relation site's; the claims' relations are
            // taken at their seeks (`enter`)
            let filed = b.resolved.and_then(|r| r.downcast_ref::<crate::ops::scan::pgrc::granule_source::Resolved>()).and_then(|r| r.rel_set(i)).cloned();
            let set = match filed {
                Some(set) => set,
                // D3663: a pruned set's survivors are this execution's; resolving the whole set would open, size and count
                // every member its Append removed: no path resolves the whole set
                None if self.recipe.set_pruned => return Err(err("gravity: a pruned relation set bound without its survivors (D3663)")),
                None => Arc::new(RelSet::resolve(self.scratch.mcx(), &self.recipe)?), // alloc: Exec
            };
            // D3663: a pruned set's first survivor moves with the execution's parameters: the relation parked by an
            // earlier bind is another one then (`open_heap` repins the parked relation whatever the oid)
            // D3681: a recheck node's site substituted with a row's newest version (EvalPlanQual, exec rerun/epq.rs) binds
            // that row's own member, where the walk stopped: its window is that row alone (`window_substituted`), and no
            // other member is counted, as a one-relation arm's recheck binds its relation alone
            let sub = self.substituted.map(|p| p.rel).filter(|&r| set.rels.iter().any(|e| e.oid == r));
            self.recipe.heap = sub.unwrap_or_else(|| set.rels.first().map_or(self.recipe.heap, |r| r.oid));
            if self.parked_oid != self.recipe.heap {
                self.parked = None;
            }
            self.set = Some(set);
            self.set_cur = usize::MAX;
            self.set_hi = 0;
            self.set_counts.clear();
            self.set_prune.clear();
            self.leader = b.ordinal.is_none();
        }
        let heap = open_heap(self.scratch.mcx(), &self.parked, self.recipe.heap)?;
        if heap.rd_att.natts != self.recipe.heap_natts || heap.rd_rel.relam != self.recipe.relam {
            heap.close(types_storage::lock::NoLock)?;
            return Err(err("gravity: relation changed under the parked plan"));
        }
        if !heap.is_scannable() {
            let e = unpopulated(heap.name());
            heap.close(types_storage::lock::NoLock)?;
            return Err(e);
        }
        self.locator = heap.rd_locator.get();
        self.local = heap.uses_local_buffers();
        self.permanent = match heap.rd_rel.relpersistence {
            RELPERSISTENCE_PERMANENT => true,
            RELPERSISTENCE_UNLOGGED | RELPERSISTENCE_TEMP => false,
            _ => {
                heap.close(types_storage::lock::NoLock)?;
                return Err(err("gravity: range source over a relation of unknown persistence"));
            }
        };
        self.nblocks = relation_get_number_of_blocks_in_fork::call(&heap, ForkNumber::MAIN_FORKNUM)?;
        let walk = self.walk;
        // D3670: a member-driven site's scans count at its runs (`count_scan`), a probed site's at its seeks, each what the
        // bind counts here for any other site
        let (counts, at_run) = (b.ordinal.is_none() && !S::BITMAP, self.recipe.run_counted || self.recipe.seek_counted);
        self.run_scan = None;
        self.stats = heap.pgstat_enabled.get().then(|| {
            let scan = match walk {
                // D3475: the plan's btree walk counts one index scan, at its descent (nbtsearch.c `_bt_first`), and no
                // sequential scan
                Some((index, _)) => (index, heap.rd_rel.relisshared, true),
                None => (heap.rd_id, heap.rd_rel.relisshared, false),
            };
            match (counts, at_run) {
                (true, false) => count_one(scan),
                (true, true) => self.run_scan = Some(scan),
                (false, _) => {}
            }
            (heap.rd_rel.relisshared, 0, 0, 0)
        });
        self.walk_fetched = 0;
        // D3585: every relation of the set counts its scan at the bind, as a one-relation site per arm does (D3681: a
        // substituted recheck's bind its row's member alone, above)
        if let (Some(set), true, false, None) = (self.set.as_ref(), counts, at_run, self.substituted) {
            // bounded("catalog-sized: one scan count per relation of the site's relation set")
            for r in set.rels.iter().skip(1).filter(|r| r.stats) {
                pgstat::relation::pgstat_count_heap_scan(r.oid, r.shared);
            }
        }
        (self.run_counted, self.seek_count, self.reached) = (counts && self.recipe.run_counted, counts && self.recipe.seek_counted && !self.recipe.run_counted, 0);
        let snapshot = snapshot_ptr(b);
        // the leader's registered snapshot, alive from bind to unbind (`snapshot_ptr`); null on a claimless bind
        let ssi = match raw::snapshot_opt(snapshot) {
            Some(s) if predicate_seams::check_for_serializable_conflict_out_needed::is_installed() => serializable_read(&heap, s),
            _ => Ok(false),
        };
        self.ssi = match ssi {
            Ok(v) => v,
            Err(e) => {
                heap.close(types_storage::lock::NoLock)?;
                return Err(e);
            }
        };
        self.parked_oid = heap.rd_id;
        self.parked = park(Some(heap))?;
        // D3585: a serializable read predicate-locks every relation of the set at the bind, as a one-relation site per arm
        // does at its own (an empty relation included: an insert into it conflicts with this read)
        if let (true, Some(set), Some(snap), None) = (self.ssi, self.set.clone(), raw::snapshot_opt(snapshot), self.substituted) {
            // bounded("catalog-sized: one predicate lock per relation of the site's relation set, as PG's Append takes one per arm it begins")
            for r in set.rels.iter().skip(1) {
                let rel = table::table_open(self.scratch.mcx(), r.oid, types_storage::lock::NoLock)?;
                let locked = serializable_read(&rel, snap);
                rel.close(types_storage::lock::NoLock)?;
                locked?;
            }
        }
        self.bind_thread = Some(me());
        self.on_bind_thread = true;
        self.rel = None;
        self.snapshot = snapshot;
        self.blocks_fetched = 0;
        self.pages = (0, 0);
        self.recent = 0;
        self.guess = 0;
        self.nbuffers = match bufmgr::NBuffersInited() {
            0 => Buffer::MAX, // no shared pool in this process: the hosts rig's fake bufmgr verifies a guess itself
            n => n,
        };
        self.hints = if self.local { None } else { PageHints::of(self.locator, self.nblocks, self.nbuffers) };
        let stream = !self.local && self.permanent && self.recipe.stream.load(Ordering::Relaxed);
        self.ahead = if stream { crate::ops::scan::heap_window::read_ahead(self.locator, self.nblocks, b.ordinal.is_none()) } else { None };
        if S::RING && !self.local && self.nblocks as u64 > (self.nbuffers / 4) as u64 && self.strategy.is_none() && get_access_strategy::is_installed() {
            self.strategy = bulk_read_ring();
        }
        self.cur.clear();
        self.defer_prune.clear();
        self.hi = 0;
        self.block = 0;
        self.bound = true;
        Ok(())
    }

    /// D3586 / D3597: the bind of an arm the initial pruning removed, which runs empty (exec `Live::pruned_whole`): a
    /// relation of no blocks, so every claim and window over it is empty, with the relation never opened nor sized and no
    /// scan counted, as PostgreSQL never initializes a subplan its initial pruning removed (no heap_beginscan, no
    /// pgstat_count_heap_scan: a generic EXECUTE over 2000 partitions opened, sized and counted all 2000 every run).
    /// The parked relation, the locator and the ring stay as a previous bind left them; no page is ever read here.
    fn bind_pruned(&mut self, b: &BindingRef<'_>) {
        self.set = None;
        self.nblocks = 0;
        self.stats = None;
        self.run_scan = None;
        self.run_counted = false;
        self.seek_count = false;
        self.walk_fetched = 0;
        self.ssi = false;
        self.bind_thread = Some(me());
        self.on_bind_thread = true;
        self.rel = None;
        self.snapshot = snapshot_ptr(b);
        self.blocks_fetched = 0;
        self.pages = (0, 0);
        self.recent = 0;
        self.guess = 0;
        self.hints = None;
        self.ahead = None;
        self.cur.clear();
        self.defer_prune.clear();
        self.hi = 0;
        self.block = 0;
        self.bound = true;
    }

    /// Block `blk` pinned and staged (`buf`), the fetch and hit counts landed: by locator into shared buffers (the guess first)
    /// or, for a temporary relation, into this backend's local buffers — on the bind thread alone (the pipeline is serial there).
    fn read_block(&mut self, blk: BlockNumber) -> PgResult<Buffer> {
        let buf = if !self.local {
            let hinted = self.hints.as_ref().map_or(0, |h| h.get(blk));
            let g = if hinted > 0 { hinted } else { self.guess };
            if g > 0 && g <= self.nbuffers && read_recent_buffer::call(self.locator, ForkNumber::MAIN_FORKNUM, blk, g)? {
                if hinted == 0 {
                    if let Some(h) = &self.hints {
                        h.set(blk, g);
                    }
                } else {
                    fires::fire_shared("heap.page_hint");
                }
                self.recent += 1;
                self.guess = g + 1;
                self.buf = Some(g);
                self.blocks_fetched += 1;
                if let Some(st) = self.stats.as_mut() {
                    st.2 += 1;
                    st.3 += 1;
                }
                return Ok(g);
            }
            let hit0 = bufmgr::counters::shared_blks_hit();
            // the claim's remaining run as one combined read (heapam's `heap_fetch_next_buffer`): bufmgr caps it by io_combine_limit.
            // D2939: a read outside a claim's run (the EPQ substitution: `hi` is 0) reads its one block; `hi - blk` wrapped
            // there (D1360's sweep: an overflow panic in a debug build; in a release one a read-ahead of up to
            // io_combine_limit blocks the recheck never visits).
            let ahead = if S::SPARSE { 1 } else { self.hi.min(self.nblocks as u64).saturating_sub(blk as u64) as BlockNumber };
            let b = if ahead > 1 && read_buffer_batched_without_relcache::is_installed() {
                let (b, hit) = read_buffer_batched_without_relcache::call(self.locator, blk, ahead, self.strategy.clone(), self.permanent)?;
                // heap.stream: a combined read that missed shared buffers advises the blocks past it (a hit advises nothing)
                if let (false, Some(a)) = (hit, &self.ahead) {
                    a.advise(blk)?;
                }
                b
            } else {
                read_buffer_without_relcache::call(self.locator, ForkNumber::MAIN_FORKNUM, blk, ReadBufferMode::Normal, self.strategy.clone(), self.permanent)?
            };
            if let Some(h) = &self.hints {
                h.set(blk, b);
            }
            self.guess = b + 1;
            self.buf = Some(b);
            if let Some(st) = self.stats.as_mut() {
                st.2 += 1;
                st.3 += bufmgr::counters::shared_blks_hit() - hit0;
            }
            b
        } else {
            if !self.on_bind_thread {
                return Err(err("gravity: a temporary relation read off its backend's thread"));
            }
            let Some(rel) = self.rel.as_deref() else { return Err(err("gravity: a temporary relation's parked entry is gone")) };
            let b = read_buffer::call(rel, blk)?;
            self.buf = Some(b);
            if let Some(st) = self.stats.as_mut() {
                st.2 += 1;
            }
            b
        };
        self.blocks_fetched += 1;
        Ok(buf)
    }

    /// D3585: the walk moves to the set's relation `r`: the relation's storage, block count, hints and read-ahead, its
    /// counts (the left relation's kept for the unbind), and on the bind thread its relcache entry (the prune and the
    /// serializable check take it; a serializable walk predicate-locks each relation it enters, as heapam's beginscan).
    fn enter(&mut self, r: usize) -> PgResult<()> {
        let Some(e) = self.set.as_ref().and_then(|set| set.rels.get(r)).copied() else { return Err(err("gravity: a claim outside its relation set")) };
        if let Some((shared, n, fetched, hit)) = self.stats.take() {
            if n | fetched | hit != 0 {
                self.set_counts.push((self.recipe.heap, shared, n, fetched, hit)); // alloc: Node
            }
        }
        self.stats = e.stats.then_some((e.shared, 0, 0, 0));
        self.recipe.heap = e.oid;
        self.locator = e.locator;
        self.nblocks = e.nblocks;
        self.set_cur = r;
        self.guess = 0;
        self.hints = PageHints::of(self.locator, self.nblocks, self.nbuffers);
        let stream = self.permanent && self.recipe.stream.load(Ordering::Relaxed);
        self.ahead = if stream { crate::ops::scan::heap_window::read_ahead(self.locator, self.nblocks, self.leader) } else { None };
        if S::RING && self.nblocks as u64 > (self.nbuffers / 4) as u64 && self.strategy.is_none() && get_access_strategy::is_installed() {
            self.strategy = bulk_read_ring();
        }
        self.rel = None;
        if self.on_bind_thread {
            let heap = table::table_open(self.scratch.mcx(), e.oid, types_storage::lock::NoLock)?;
            let ssi = match raw::snapshot_opt(self.snapshot) {
                Some(snap) if self.ssi => serializable_read(&heap, snap),
                _ => Ok(self.ssi),
            };
            self.rel = Some(Rc::clone(heap.data_rc()));
            heap.close(types_storage::lock::NoLock)?;
            self.ssi = ssi?;
        }
        Ok(())
    }

    /// D3670: a probed site's set walk reached relation `r` in this re-open: it, and each relation before it this re-open
    /// has not counted (an empty one the walk stepped over), count their scans, as PG's Append begins each arm it reaches;
    /// on the bind thread only (the probe's seeks are the leader's).
    fn reach(&mut self, r: usize) {
        if !self.seek_count || !self.on_bind_thread || r < self.reached {
            return;
        }
        let Some(set) = self.set.as_ref() else { return };
        // bounded("catalog-sized: one scan count per relation of the site's relation set, each counted once")
        for e in set.rels.iter().take(r + 1).skip(self.reached).filter(|e| e.stats) {
            pgstat::relation::pgstat_count_heap_scan(e.oid, e.shared);
        }
        self.reached = r + 1;
    }

    /// D3585: past the walk's relation, the next of the set the seek's range reaches (`set_hi`), its blocks the walk's;
    /// false at the range's end (or for a one-relation site).
    fn next_rel(&mut self) -> PgResult<bool> {
        let Some(set) = self.set.as_ref() else { return Ok(false) };
        let mut r = self.set_cur.wrapping_add(1);
        // bounded("catalog-sized: steps over the empty relations of the site's relation set, two loads each")
        while r < set.rels.len() && set.starts[r] < self.set_hi && set.rels[r].nblocks == 0 {
            r += 1;
        }
        if r >= set.rels.len() || set.starts[r] >= self.set_hi {
            // D3670: a probed set's walk ran to the set's end: PG's Append reached every arm left (an empty one too)
            let (end, last) = (self.set_hi >= set.total(), set.rels.len() - 1);
            if end {
                self.reach(last);
            }
            return Ok(false);
        }
        let (start, nblocks) = (set.starts[r], set.rels[r].nblocks as u64);
        self.reach(r);
        self.enter(r)?;
        self.block = 0;
        self.hi = (self.set_hi - start).min(nblocks);
        Ok(true)
    }

    /// D865: a page staged off the bind thread whose `pd_prune_xid` is set (read unlocked under the pin, as heap_page_prune_opt's
    /// first test) is pruned on the bind thread at the unbind (`prune_deferred`).
    fn note_prune(&mut self, blk: BlockNumber, buf: Buffer) {
        // `buf` is pinned by this thread
        if raw::pinned_page(buf).prune_xid() != 0 {
            match self.set {
                Some(_) => self.set_prune.push((self.set_cur as u32, blk)), // alloc: Node
                None => self.defer_prune.push(blk), // alloc: Node
            }
        }
    }

    /// D865: the pages pool-thread claims staged with a prune xid, pruned on the bind thread through the parked relation, each
    /// pinned alone (heap_page_prune_opt takes its cleanup lock conditionally, as heapam's page scan). Off the bind thread (a
    /// drop elsewhere) or with the relation gone the list is dropped: nothing a prune leaves is needed for correctness.
    fn prune_deferred(&mut self) -> PgResult<()> {
        if !self.set_prune.is_empty() {
            self.prune_deferred_set()?;
        }
        if self.defer_prune.is_empty() {
            return Ok(());
        }
        let blocks = core::mem::take(&mut self.defer_prune);
        if self.local || self.bind_thread != Some(me()) {
            return Ok(());
        }
        let Some(rel) = self.parked.as_ref().and_then(|w| w.upgrade()) else { return Ok(()) };
        // a page's prune is the unit; a canceled statement drops the rest of its list, as a drop off the bind thread does
        let mut step = cancelpoll::step("heap_source prune_deferred: a deferred page's prune");
        for blk in blocks {
            if step.tick().is_err() {
                break;
            }
            if blk >= self.nblocks {
                continue;
            }
            let buf = read_buffer_without_relcache::call(self.locator, ForkNumber::MAIN_FORKNUM, blk, ReadBufferMode::Normal, self.strategy.clone(), self.permanent)?;
            let pruned = pruneheap_seams::heap_page_prune_opt::call(&rel, buf);
            release_buffer::call(buf)?;
            pruned?;
        }
        Ok(())
    }

    /// D865 for a set (`set_prune`): each relation's deferred pages pruned through its own relcache entry, on the bind thread.
    fn prune_deferred_set(&mut self) -> PgResult<()> {
        let mut pages = core::mem::take(&mut self.set_prune);
        let Some(set) = self.set.clone().filter(|_| self.bind_thread == Some(me())) else { return Ok(()) };
        // the sort and a page's prune are the units; a canceled statement drops the rest of its list (`prune_deferred`)
        let mut step = cancelpoll::step("heap_source prune_deferred_set: a deferred page's prune");
        // a refused scratch drops the list, as a cancel does
        let mut scratch = charged_scratch(self.held.owner());
        if pages.len() > cancelpoll::SORT_RUN && scratch.resize(pages.len(), pages[0]).is_err() {
            return Ok(());
        }
        if cancelpoll::sort_by(&mut step, &mut pages, scratch.as_mut_slice(), |a, b| a.cmp(b)).is_err() {
            return Ok(());
        }
        drop(scratch);
        let (mut i, mut stopped) = (0, false);
        while i < pages.len() && !stopped {
            let r = pages[i].0;
            let j = i + pages[i..].partition_point(|p| p.0 == r);
            let Some(e) = set.rels.get(r as usize).copied() else { break };
            let rel = table::table_open(self.scratch.mcx(), e.oid, types_storage::lock::NoLock)?;
            for &(_, blk) in pages[i..j].iter().filter(|p| p.1 < e.nblocks) {
                if step.tick().is_err() {
                    stopped = true;
                    break;
                }
                let buf = read_buffer_without_relcache::call(e.locator, ForkNumber::MAIN_FORKNUM, blk, ReadBufferMode::Normal, self.strategy.clone(), true)?;
                let pruned = pruneheap_seams::heap_page_prune_opt::call(&rel, buf);
                release_buffer::call(buf)?;
                pruned?;
            }
            rel.close(types_storage::lock::NoLock)?;
            i = j;
        }
        Ok(())
    }

    fn unpin(&mut self) -> PgResult<()> {
        if let Some(buf) = self.buf.take() {
            release_buffer::call(buf)?;
        }
        self.nvis = 0;
        self.cur.clear();
        Ok(())
    }

    /// D1408: a pin left by a claim on another thread is leaked, never released here (it would take another thread's
    /// refcount): a WARNING names it (the buffer stays pinned, blocking a cleanup lock, until that thread exits).
    fn leak_foreign_pin(&mut self, me: std::thread::ThreadId) {
        if self.buf.is_some() && self.claim_thread.is_some_and(|t| t != me) {
            self.leak_pin();
        }
    }

    #[cold]
    fn leak_pin(&mut self) {
        let buf = self.buf.take().unwrap_or_default();
        self.nvis = 0;
        self.cur.clear();
        let msg = format!("gravity: D1408 buffer {buf} of relation {} was pinned by a claim on another thread and is leaked", self.recipe.heap); // alloc: Cold
        elog::emit_error_report_for(&PgError::warning(msg));
    }

    fn stage(&mut self) -> PgResult<bool> {
        if self.snapshot.is_null() {
            return Err(err("gravity: range claim on an unbound source"));
        }
        // the leader's registered snapshot, alive and immutable from bind to unbind (`snapshot_ptr`)
        let snapshot = raw::snapshot_at(self.snapshot);
        // The prune and the serializable check take the parked relation (`seek` upgraded it on the bind thread alone). The check
        // is asked per page as heapam does, but only when the bind said yes: C's `SerializationNeededForRead` never turns true.
        if self.rel.is_none() && self.ssi {
            return Err(err("gravity: a serializable range claim off its backend's thread"));
        }
        if S::ACTIVE {
            return self.stage_sampled(snapshot);
        }
        loop {
            if self.stage_rel(snapshot)? {
                return Ok(true);
            }
            if !self.next_rel()? {
                return Ok(false);
            }
        }
    }

    /// `stage` over the walk's relation's blocks `[block, hi)`.
    fn stage_rel(&mut self, snapshot: &SnapshotData<'_>) -> PgResult<bool> {
        // a page that kept no row is the unit (heapam's page scan checks for interrupts per page): a run of them is as long
        // as the relation
        let mut step = cancelpoll::step("heap_source stage_rel: a page that kept no visible row");
        while self.block < self.hi {
            let blk = self.block as BlockNumber;
            let buf = self.read_block(blk)?;
            // PostgreSQL's heap scan prunes every page it visits (heap_prepare_pagescan -> heap_page_prune_opt) while holding
            // its ONE pin, so the cleanup lock succeeds; without it a hot page under a served seq-scan write bloats (C21).
            let ssi = match self.rel.as_deref() {
                Some(rel) => {
                    pruneheap_seams::heap_page_prune_opt::call(rel, buf)?;
                    (self.ssi && predicate_seams::check_for_serializable_conflict_out_needed::call(rel, snapshot)?).then_some(rel)
                }
                None => {
                    self.note_prune(blk, buf);
                    None
                }
            };
            lock_buffer::call(buf, BUFFER_LOCK_SHARE)?;
            let walked = collect_visible(buf, blk, self.recipe.heap, self.recipe.want, snapshot, &mut self.vis, ssi);
            // D3475: an index-only walk reads the heap only for an entry whose page the visibility map does not vouch for
            // (nodeIndexonlyscan.c); PD_ALL_VISIBLE is set and cleared with the map's bit, read here under the share lock
            let fetches = matches!(self.walk, Some((_, true))) && !raw::pinned_page(buf).is_all_visible();
            lock_buffer::call(buf, BUFFER_LOCK_UNLOCK)?;
            let (nvis, flags, direct) = walked?;
            self.nvis = nvis;
            self.flags = flags;
            if direct {
                self.pages.0 += 1;
            } else {
                self.pages.1 += 1;
            }
            if let Some(st) = self.stats.as_mut() {
                st.1 += self.nvis as u64;
                if fetches {
                    self.walk_fetched += self.nvis as u64;
                }
            }
            self.block += 1;
            if self.nvis > 0 {
                return Ok(true);
            }
            if let Some(b) = self.buf.take() {
                release_buffer::call(b)?;
            }
            step.tick().map_err(|_| err(WALK_CANCELED))?;
        }
        Ok(false)
    }

    /// `stage` under a TABLESAMPLE filter: the TSM picks the blocks and keeps the offsets; the no-filter walk stays apart.
    #[inline(never)]
    fn stage_sampled(&mut self, snapshot: &SnapshotData<'_>) -> PgResult<bool> {
        // a page the filter kept no row of is the unit, as `stage_rel`'s
        let mut step = cancelpoll::step("heap_source stage_sampled: a sampled page that kept no row");
        while let Some(blk) = self.sample.next_block(self.nblocks)? {
            self.block = blk as u64;
            let buf = self.read_block(blk)?;
            let ssi = match self.rel.as_deref() {
                Some(rel) => {
                    pruneheap_seams::heap_page_prune_opt::call(rel, buf)?;
                    (self.ssi && predicate_seams::check_for_serializable_conflict_out_needed::call(rel, snapshot)?).then_some(rel)
                }
                None => {
                    self.note_prune(blk, buf);
                    None
                }
            };
            lock_buffer::call(buf, BUFFER_LOCK_SHARE)?;
            let walked = collect_visible(buf, blk, self.recipe.heap, self.recipe.want, snapshot, &mut self.vis, ssi);
            // A bitmap keeps by the page's HOT chains, read under the same share lock
            let kept = match &walked {
                Ok((nvis, _, _)) if S::PAGE => Some(self.sample.keep_page(blk, raw::pinned_page(buf), &mut self.vis[..*nvis])),
                _ => None,
            };
            lock_buffer::call(buf, BUFFER_LOCK_UNLOCK)?;
            let (nvis, flags, direct) = walked?;
            self.nvis = match kept {
                Some(k) => k?,
                None => {
                    // the page stays pinned (`buf`) while its line pointer count is read
                    let maxoff = raw::pinned_page(buf).max_offset_number();
                    self.sample.keep(blk, maxoff, &mut self.vis[..nvis])
                }
            };
            // a sampled page publishes the kept tuples only: its line pointers are not the window
            self.flags = flags & !(PAGE_ALL_VISIBLE | PAGE_IOTA);
            if direct {
                self.pages.0 += 1;
            } else {
                self.pages.1 += 1;
            }
            if let Some(st) = self.stats.as_mut() {
                st.1 += self.nvis as u64;
            }
            self.block += 1;
            if self.nvis > 0 {
                return Ok(true);
            }
            if let Some(b) = self.buf.take() {
                release_buffer::call(b)?;
            }
            step.tick().map_err(|_| err(WALK_CANCELED))?;
        }
        Ok(false)
    }

    /// One page as a window: every visible tuple deformed once into the
    /// column arrays, the visible line pointers as the offset lane; the pin
    /// is held while the window is current (R3).
    fn window_pg(&mut self, w: &mut Window<'_>) -> PgResult<u32> {
        if let Some(pos) = self.substituted.take() {
            return self.window_substituted(pos, w);
        }
        self.unpin()?;
        if !self.stage()? {
            return Ok(0);
        }
        let blk = (self.block - 1) as BlockNumber;
        if self.raw {
            return Ok(self.fill_raw(w, blk, ScanPos::new(self.recipe.heap, blk, 0)));
        }
        self.fill(w, blk, ScanPos::new(self.recipe.heap, blk, 0))
    }

    /// The page protocol's window: the page, its visible offsets (the offset lane) and the flags `collect_visible` summarised on
    /// its walk — no visible tuple with a null bitmap (`PAGE_NONULLS`), every visible tuple carrying the recipe's `want`
    /// attributes (`PAGE_FULL`); the attribute lanes are published empty and deformed by `lane` on demand.
    fn fill_raw(&mut self, w: &mut Window<'_>, _blk: BlockNumber, base: ScanPos) -> u32 {
        let buf = self.buf.expect("staged");
        let natts = self.recipe.natts;
        let nvis = self.nvis;
        (self.filled, self.sel_filled, self.sys_filled) = (0, 0, false);
        RAW_WINDOWS.fetch_add(1, Ordering::Relaxed);
        // bounded("key-columns: one lane per output column, at most MaxTupleAttributeNumber (1664) columns")
        for k in 0..natts {
            let mut l = Lane::byval(core::ptr::null(), core::ptr::null(), 8, lane::REGION_ARENA, lane::id(lane::ID_ATTR, self.recipe.attnos[k] as u32));
            if self.recipe.byref.has(k) {
                l.kind = lane::KIND_BYREF;
            }
            w.set(k, l);
        }
        w.set(natts, Lane::byval(self.vis.as_ptr() as *const u8, core::ptr::null(), 2, lane::REGION_ARENA, LaneId::Offset.encode()));
        w.set_page(bufmgr_seams::buffer_page_ptr(buf).as_ptr() as *const u8, self.flags);
        w.rows = nvis as u32;
        // a heap window has no granule: the word carries the pinned buffer (host_abi v4 m2 `PageWin::buf`)
        w.set_base(WindowBase { pos: base, part_id: 0, dict_epoch: 0, granule: buf as u32, claim_id: self.lo as u32 });
        nvis as u32
    }

    /// Lane `k` of a raw-page window, deformed on demand (hosts v4/m2_heap.rs `reach`): a plain request deforms every wanted
    /// attribute of every row once; a `LANE_LATE` request only to its own attribute, over the site's selection `sel` when it
    /// selects fewer rows (late decode: the filter lanes first, the rest for the survivors). The site's offset lane
    /// (`k == natts`) is published as it stands.
    fn lane_pg(&mut self, k: usize, sel: &[u32], out: &mut Lane) -> PgResult<u32> {
        if !self.raw {
            return Ok(0);
        }
        let buf = self.buf.ok_or_else(|| err("gravity: lane request without a staged page"))?;
        let (late, k) = (k as u32 & LANE_LATE != 0, (k as u32 & !LANE_LATE) as usize);
        if k >= self.recipe.natts {
            *out = Lane::byval(self.vis.as_ptr() as *const u8, core::ptr::null(), 2, lane::REGION_ARENA, LaneId::Offset.encode());
            return Ok(self.nvis as u32);
        }
        // a system lane reads the headers only (upto 0), every row's once per window
        let sys = crate::ops::shared::heap_fetch::is_sys(self.recipe.attnos[k]);
        let (upto, rows) = if sys { (0, None) } else { reach(late, self.recipe.attnos[k] as usize, self.recipe.want, sel, self.nvis) };
        let covered = if sys { self.sys_filled } else { self.filled >= upto || rows.is_some_and(|r| self.sel_filled >= upto && self.sel_rows[..] == *r) };
        if !covered {
            let blk = (self.block - 1) as BlockNumber;
            // as `fill`
            let page = raw::pinned_page(buf);
            self.deform(page, blk, rows, upto)?;
            match rows {
                _ if sys => {}
                Some(r) => {
                    self.sel_rows.clear();
                    self.sel_rows.extend_from_slice(r); // alloc: Node
                    self.sel_filled = upto;
                }
                None => self.filled = upto,
            }
        }
        *out = self.lane_desc(k);
        Ok(self.nvis as u32)
    }

    fn lane_desc(&self, k: usize) -> Lane {
        let nvis = self.nvis;
        let words = &self.valid[k * PAGE_VALIDITY_WORDS..(k + 1) * PAGE_VALIDITY_WORDS];
        let all = (0..nvis).all(|r| (words[r / 64] >> (r % 64)) & 1 != 0);
        let mut l = Lane::byval(self.lanes[k * PAGE_ROWS..].as_ptr() as *const u8, if all { core::ptr::null() } else { words.as_ptr() }, 8, lane::REGION_ARENA, lane::id(lane::ID_ATTR, self.recipe.attnos[k] as u32));
        if self.recipe.byref.has(k) {
            l.kind = lane::KIND_BYREF;
        }
        l
    }

    /// The staged page's rows `rows` (every visible one: `None`) deformed to attribute `upto` into the column arrays and the
    /// validity words; the window's first deform clears the words (a row it does not reach reads NULL).
    fn deform(&mut self, page: PageRef<'_>, blk: BlockNumber, rows: Option<&[u32]>, upto: usize) -> PgResult<()> {
        if self.filled == 0 && self.sel_filled == 0 {
            self.valid.fill(0);
        }
        if rows.is_none() {
            DEFORMED_PAGES.fetch_add(1, Ordering::Relaxed);
        }
        let r = &self.recipe;
        let shape = Shape { heap: r.heap, atts: &r.atts[..], missing: &r.missing[..], attnos: &r.attnos[..r.natts] };
        let mut out = Lanes { lanes: &mut self.lanes, valid: &mut self.valid, vals: &mut self.vals, nulls: &mut self.nulls };
        // the page stays pinned while it is staged; `vis` holds its normal line pointers
        raw::deform_page(&page, blk, &shape, &mut out, &self.vis[..self.nvis], rows, upto)?;
        self.sys_filled |= rows.is_none();
        Ok(())
    }

    /// The staged page's `vis[..nvis]` rows into the lanes, the window based at `base`.
    fn fill(&mut self, w: &mut Window<'_>, blk: BlockNumber, base: ScanPos) -> PgResult<u32> {
        let buf = self.buf.expect("staged");
        // the page stays pinned while it is staged; a normal line pointer carries a full tuple image
        let page = raw::pinned_page(buf);
        let natts = self.recipe.natts;
        let nvis = self.nvis;
        (self.filled, self.sel_filled, self.sys_filled) = (0, 0, false);
        self.deform(page, blk, None, self.recipe.want)?;
        self.filled = self.recipe.want;
        // bounded("key-columns: one lane per output column, at most MaxTupleAttributeNumber (1664) columns")
        for k in 0..natts {
            let l = self.lane_desc(k);
            w.set(k, l);
        }
        // D3014O: an index site a worker probes takes the RowId lane where a range site takes the offset lane
        if self.worker {
            // bounded("small-fixed: one page's visible line pointers, at most PAGE_ROWS (MaxHeapTuplesPerPage, 291 at 8 K pages), a word each")
            for (t, &off) in self.tids.iter_mut().zip(&self.vis[..nvis]) {
                *t = gravity_kernels::ops::scan::entries::tid_word(blk, off);
            }
            w.set(natts, Lane::byval(self.tids.as_ptr() as *const u8, core::ptr::null(), 8, lane::REGION_ARENA, LaneId::RowId.encode()));
        } else {
            w.set(natts, Lane::byval(self.vis.as_ptr() as *const u8, core::ptr::null(), 2, lane::REGION_ARENA, LaneId::Offset.encode()));
        }
        w.rows = nvis as u32;
        w.set_base(WindowBase { pos: base, part_id: 0, dict_epoch: 0, granule: 0, claim_id: self.lo as u32 });
        if S::PAGE {
            w.set_page_flags(self.sample.page_flags());
        }
        Ok(nvis as u32)
    }

    /// The EPQ substitution (lower.md §8.6 need 3): the version at `pos` as a one-row window at
    /// its own position, any visibility — a range scan's quals are the program's, so the recheck
    /// re-evaluates them over this row; the scan ends after it.
    fn window_substituted(&mut self, pos: ScanPos, w: &mut Window<'_>) -> PgResult<u32> {
        self.unpin()?;
        self.hi = 0;
        self.block = 0;
        if pos.rel != self.recipe.heap || !pos.is_valid() || pos.off == 0 || pos.block >= self.nblocks {
            return Ok(0);
        }
        let buf = self.read_block(pos.block)?;
        lock_buffer::call(buf, BUFFER_LOCK_SHARE)?;
        let normal = normal_line(buf, pos.off);
        lock_buffer::call(buf, BUFFER_LOCK_UNLOCK)?;
        if !normal? {
            self.unpin()?;
            return Ok(0);
        }
        self.vis[0] = pos.off;
        self.nvis = 1;
        self.fill(w, pos.block, ScanPos::row(self.recipe.heap, pos.block, pos.off))
    }

    /// D3585: a claim `[lo, hi)` in the set's claims' space: the relation holding `lo` (a claim stays inside it; a
    /// whole-range read walks on through `next_rel`) and its blocks of the range.
    fn seek_set(&mut self, lo: u64, hi: u64) -> PgResult<()> {
        let Some(set) = self.set.as_ref() else { return Ok(()) };
        let (total, last) = (set.total(), set.rels.len().saturating_sub(1));
        let hi = hi.min(total);
        self.set_hi = hi;
        self.lo = lo;
        if lo >= hi {
            (self.block, self.hi) = (0, 0);
            // D3670: a probed set with no blocks at all: PG's Append reaches every (empty) arm
            if lo == 0 && hi == total {
                self.reach(last);
            }
            return Ok(());
        }
        let r = set.at(lo);
        let (start, nblocks) = (set.starts[r], set.rels[r].nblocks as u64);
        self.reach(r);
        if r != self.set_cur {
            self.enter(r)?;
        }
        self.block = lo - start;
        self.hi = (hi - start).min(nblocks);
        Ok(())
    }

    fn unbind_pg(&mut self) -> PgResult<()> {
        if !self.bound {
            return Ok(());
        }
        self.bound = false;
        self.substituted = None;
        self.leak_foreign_pin(me());
        self.sample.unbind();
        if let Some((shared, n, fetched, hit)) = self.stats.take() {
            match self.walk {
                // D3475: the plan's btree walk: each tuple it returns is an index entry read (`idx_tup_read`, indexam.c
                // `index_getnext_tid`) and a heap fetch counted on the index (`idx_tup_fetch`, `index_fetch_heap`); an
                // index-only walk fetches only off the pages the visibility map does not vouch for
                // D3581: the bitmap morsel form of an order-free Index Scan: the bitmap's build counted the scan and its
                // entries; the heap fetches go to the index, as the planned scan's `index_fetch_heap` counts them
                Some((index, _)) if S::BITMAP => pgstat::relation::pgstat_count_index_scan_batched(index, shared, 0, 0, n),
                Some((index, only)) => {
                    let fetches = if only { core::mem::take(&mut self.walk_fetched) } else { n };
                    pgstat::relation::pgstat_count_index_scan_batched(index, shared, 0, n, fetches);
                }
                None => {
                    let (returned, fetched_tuples) = if S::BITMAP { (0, n) } else { (n, 0) };
                    pgstat::relation::pgstat_count_heap_scan_batched(self.recipe.heap, shared, returned, fetched_tuples);
                }
            }
            pgstat::relation::pgstat_count_buffer_reads(self.recipe.heap, shared, fetched, hit);
        }
        // bounded("catalog-sized: one counter flush per relation of the site's relation set the walk entered")
        for (oid, shared, n, fetched, hit) in core::mem::take(&mut self.set_counts) {
            let (returned, fetched_tuples) = if S::BITMAP { (0, n) } else { (n, 0) };
            pgstat::relation::pgstat_count_heap_scan_batched(oid, shared, returned, fetched_tuples);
            pgstat::relation::pgstat_count_buffer_reads(oid, shared, fetched, hit);
        }
        if self.recent > 0 {
            pgstat::io::pgstat_count_io_op(pgstat::io::IOObject::Relation, types_storage::buf::IOContext::IOCONTEXT_NORMAL, pgstat::io::IOOp::Hit, self.recent as u32, 0);
        }
        self.unpin()?;
        self.prune_deferred()?;
        self.rel = None;
        self.hints = None;
        self.ahead = None;
        if self.strategy.is_some() {
            free_access_strategy::call(self.strategy.take());
        }
        self.snapshot = core::ptr::null();
        self.scratch.reset_exec();
        Ok(())
    }
}

/// One scan counted as PG counts it as it begins or rescans one: `(relation, relisshared, index)`, heapam initscan's
/// sequential scan, or the btree walk's index scan at its descent (nbtsearch.c `_bt_first`) when `index`.
fn count_one((rel, shared, index): (Oid, bool, bool)) {
    match index {
        true => pgstat::relation::pgstat_count_index_scan(rel, shared),
        false => pgstat::relation::pgstat_count_heap_scan(rel, shared),
    }
}

thread_local! {
    /// This thread's id: `std::thread::current` takes and drops a reference to the thread's handle (two atomic writes),
    /// at each bind and unbind of every site (D3586: thousands per EXECUTE over a pruned Append's arms).
    static ME: std::thread::ThreadId = std::thread::current().id();
}

/// This thread's id (`ME`).
fn me() -> std::thread::ThreadId {
    ME.with(|t| *t)
}

/// The range scan's bulk-read ring. heapam's (256 kB + effective_io_concurrency x io_combine_limit blocks) leaves room for
/// a read stream's in-flight reads; this source waits for its one combined read before walking it, so 256 kB + one combined
/// read is its working set. Perf: the smaller ring stays in L2 (buffer headers, mapping entries, copy targets).
/// Never larger than heapam's.
fn bulk_read_ring() -> BufferAccessStrategy {
    let heapam = get_access_strategy::call(BufferAccessStrategyType::BasBulkread);
    if !get_access_strategy_with_size::is_installed() || !get_access_strategy_buffer_count::is_installed() {
        return heapam;
    }
    let combine = crate::inputs::io_combine_blocks();
    let Some(kb) = ring_kb(combine, get_access_strategy_buffer_count::call(&heapam)) else { return heapam };
    free_access_strategy::call(heapam);
    get_access_strategy_with_size::call(BufferAccessStrategyType::BasBulkread, kb)
}

/// The ring for `combine`-block reads when heapam's holds `heapam` buffers: `None` keeps heapam's (not larger).
fn ring_kb(combine: i32, heapam: i32) -> Option<i32> {
    let blk_kb = (BLCKSZ / 1024) as i32;
    let kb = 256 + combine * blk_kb;
    (kb / blk_kb < heapam).then_some(kb)
}

/// The bind's serializable read, as heapam's `heap_beginscan` for a seq or sample scan: the whole relation predicate-locked
/// (`PredicateLockRelation`, a no-op unless the transaction is serializable), so a later write in a concurrent serializable
/// transaction records its rw-conflict with this read (D551: a served read took no lock, and write skew committed); then
/// whether each page's tuples need the conflict-out check (`CheckForSerializableConflictOutNeeded`, asked again per page).
fn serializable_read(heap: &RelationData<'_>, snapshot: &SnapshotData<'_>) -> PgResult<bool> {
    if predicate_seams::predicate_lock_relation::is_installed() {
        predicate_seams::predicate_lock_relation::call(heap, snapshot)?;
    }
    predicate_seams::check_for_serializable_conflict_out_needed::call(heap, snapshot)
}

impl<S: SampleFilter> Source for HeapRangeSource<S> {
    fn bind(&mut self, b: &BindingRef<'_>) -> Result<()> {
        self.scratch.home_statement();
        self.bind_pg(b).map_err(crate::stash)?;
        self.held.home(HeldCharge::of_statement());
        self.held.sync(self.kept_bytes() as u64);
        Ok(())
    }

    fn seek(&mut self, keys: &[Word], keys_null: &[u8]) -> Result<()> {
        if !self.bound {
            return Err(Error::host("seek on an unbound range source"));
        }
        let me = me();
        self.leak_foreign_pin(me);
        self.claim_thread = Some(me);
        self.unpin().map_err(crate::stash)?;
        self.on_bind_thread = self.bind_thread == Some(me);
        self.rel = if self.on_bind_thread { self.parked.as_ref().and_then(|w| w.upgrade()) } else { None };
        if self.substituted.is_some() {
            return Ok(());
        }
        // D3670: a probed site's seek is a re-open's first fetch, PG's scan begin or rescan: a one-relation site counts its
        // scan here, a set each relation its walk reaches (`reach`)
        if self.seek_count && self.on_bind_thread {
            self.reached = 0;
            if let (None, Some(scan)) = (self.set.as_ref(), self.run_scan) {
                count_one(scan);
            }
        }
        let total = self.nblocks as u64;
        if S::ACTIVE {
            self.sample.start(keys, keys_null).map_err(crate::stash)?;
            (self.lo, self.block, self.hi) = (0, 0, total);
            return Ok(());
        }
        if keys_null[0] != 0 || keys_null[1] != 0 {
            self.hi = 0;
            self.set_hi = 0;
            return Ok(());
        }
        if self.set.is_some() {
            return self.seek_set(keys[0], keys[1]).map_err(crate::stash);
        }
        self.hi = keys[1].min(total);
        self.block = keys[0].min(self.hi);
        self.lo = self.block;
        Ok(())
    }

    fn lane(&mut self, k: u16, sel: &[u32], out: &mut Lane) -> Result<u32> {
        self.lane_pg(k as usize, sel, out).map_err(crate::stash)
    }
    fn window(&mut self, w: &mut Window<'_>) -> Result<u32> {
        self.held.home(HeldCharge::owner_of(w));
        self.sample.home(self.held.owner());
        let r = self.window_pg(w).map_err(crate::stash);
        self.held.sync(self.kept_bytes() as u64);
        r
    }

    fn cursor(&self) -> &Cursor {
        &self.cur
    }

    fn cursor_mut(&mut self) -> &mut Cursor {
        &mut self.cur
    }

    fn lanes_hint(&self) -> usize {
        self.recipe.natts + 1
    }

    fn serves_worker(&self) -> bool {
        self.worker
    }

    fn refetch(&mut self, _values: &mut [Word], _isnull: &mut [u8]) -> Result<bool> {
        Err(crate::unsupported())
    }

    fn substitute(&mut self, pos: ScanPos) {
        self.substituted = Some(pos);
    }

    fn unbind(&mut self) {
        self.unbind_pg().expect("gravity: heap range source unbind");
        self.held.park(self.kept_bytes() as u64);
    }

    fn transfer_held(&mut self, to: memledger::Owner, release: memledger::Release) {
        self.held.transfer(to, release);
        self.scratch.home(to);
        self.sample.transfer_held(to, release);
    }

    /// D3213: the lanes and the deform buffers, kept between statements.
    fn kept_bytes(&self) -> usize {
        use gravity_driver::vec_bytes as v;
        v(&self.lanes) + v(&self.valid) + v(&self.vals) + v(&self.nulls) + v(&self.sel_rows) + v(&self.defer_prune) + v(&self.tids) + v(&self.set_counts) + v(&self.set_prune)
    }

    fn pins(&self) -> u64 {
        self.buf.is_some() as u64
    }

    fn release(&mut self) {
        self.unpin().expect("gravity: heap range source release");
    }

    /// D3196 (rearm E1): a warm worker slot's next section: the pages its claims staged with a prune xid are pruned now, on
    /// the bind thread, as the unbind did after every section (the list would grow by the same pages at every rescan).
    fn rearm(&mut self) -> Result<()> {
        self.unpin().and_then(|()| self.prune_deferred()).map_err(crate::stash)
    }

    /// D3670: a member run of the site this leader's bind left its count to (`run_counted`): the bound relation's scan
    /// (`run_scan`) and, for a relation set, each other survivor's, as the bind counts them for a site no member drives;
    /// on the bind thread only (pgstat's pending counts are the backend's).
    fn count_scan(&mut self) {
        if !self.bound || !self.run_counted || self.bind_thread != Some(me()) {
            return;
        }
        if let Some(scan) = self.run_scan {
            count_one(scan);
        }
        if let Some(set) = self.set.as_ref() {
            // bounded("catalog-sized: one scan count per relation of the site's relation set")
            for r in set.rels.iter().skip(1).filter(|r| r.stats) {
                pgstat::relation::pgstat_count_heap_scan(r.oid, r.shared);
            }
        }
    }
}

impl<S: SampleFilter> Drop for HeapRangeSource<S> {
    fn drop(&mut self) {
        let _ = self.unbind_pg();
    }
}

#[cfg(test)]
mod tests {
    /// CONVERT wave 2: a source's held charge sits on the owner its windows name (with its kind), follows its bytes, leaves
    /// an owner it moves off, parks on the session at the unbind, goes where `transfer_held` sends a parked one (the slot
    /// depot at a park, the taking statement at a take), and leaves with the source.
    #[test]
    fn a_held_charge_follows_its_owner_and_its_bytes() {
        use memledger::{charge, Holder, Kind, Owner, Release};
        std::thread::spawn(|| {
            memledger::bind_backend(3992, 0);
            memledger::statement_begin();
            let (stmt, _) = charge::context();
            let op = charge::op_begin(stmt, 1, 0);
            let session = Owner::Session(stmt.acct());
            let depot = Owner::Cache(Holder::Parked);
            let c = |o| charge::owner_row(o).committed;
            let (s0, d0) = (c(session), c(depot));
            let mut h = super::HeldCharge::new();
            h.sync(100);
            h.home(None);
            assert_eq!((c(stmt), c(op), h.owner()), (0, 0, None), "no window named an owner: nothing charged");
            h.home(Some((op, Kind::HashAgg)));
            h.sync(4096);
            assert_eq!((c(op), h.owner()), (4096, Some((op, Kind::HashAgg))));
            h.sync(1024);
            assert_eq!(c(op), 1024, "a shrink frees its share");
            h.home(Some((op, Kind::HashAgg)));
            assert_eq!(c(op), 1024, "the same owner: kept");
            h.home(Some((stmt, Kind::Region)));
            assert_eq!((c(op), c(stmt)), (0, 1024), "another owner: its bytes move from the first to the second in one step (D4536)");
            h.sync(2048);
            assert_eq!(c(stmt), 2048);
            h.grow(1 << 20);
            assert_eq!(c(stmt), 2048 + (1 << 20), "a buffer made between two syncs: its owner's at once (D4536)");
            h.sync(2048 + (1 << 20));
            assert_eq!(c(stmt), 2048 + (1 << 20), "the next sync finds it charged");
            h.sync(2048);
            h.transfer(depot, Release::Evict);
            assert_eq!((c(stmt), c(depot) - d0), (2048, 0), "a bound source's bytes stay its owner's");
            h.park(3000);
            assert_eq!((c(stmt), c(session) - s0, h.owner()), (0, 3000, None), "parked: the session's");
            h.transfer(depot, Release::Evict);
            assert_eq!((c(session) - s0, c(depot) - d0), (0, 3000), "a parked worker set's: the depot's");
            h.transfer(stmt, Release::Statement);
            assert_eq!((c(depot) - d0, c(stmt)), (0, 3000), "taken from the depot: the taking statement's");
            h.home(Some((stmt, Kind::Region)));
            h.sync(512);
            assert_eq!((c(depot) - d0, c(stmt)), (0, 512), "bound again: the binding's owner, re-synced");
            drop(h);
            assert_eq!(c(stmt), 0, "dropped with the source");
            charge::op_end(op);
            memledger::statement_end();
            memledger::unbind_backend();
        })
        .join()
        .unwrap();
    }

    #[test]
    fn the_ring_holds_one_combined_read_and_never_exceeds_heapams() {
        assert_eq!(super::ring_kb(16, 288), Some(384));
        assert_eq!(super::ring_kb(1, 288), Some(264));
        assert_eq!(super::ring_kb(16, 48), None);
        assert_eq!(super::ring_kb(16, 32), None);
    }

    /// A relation set of these block counts (the claims' space tiles them in order).
    fn set_of(blocks: &[u32]) -> super::RelSet {
        let mut starts = vec![0u64]; // alloc: Cold
        for &b in blocks {
            starts.push(starts.last().copied().unwrap_or(0) + b as u64); // alloc: Cold
        }
        let rels = blocks.iter().enumerate().map(|(i, &nblocks)| super::SetRel { oid: i as u32 + 1, locator: super::RelFileLocator::new(0, 0, 0), nblocks, shared: false, stats: true });
        super::RelSet { rels: rels.collect(), starts: starts.into_boxed_slice() }
    }

    #[test]
    fn a_relation_set_maps_each_block_to_its_relation_and_cuts_claims_at_each_relation() {
        let s = set_of(&[3, 0, 2, 0, 0, 4]);
        assert_eq!(s.total(), 9);
        assert_eq!(s.bounds(), &[3, 3, 5, 5, 5]);
        let at: Vec<usize> = (0..9).map(|g| s.at(g)).collect();
        assert_eq!(at, [0, 0, 0, 2, 2, 5, 5, 5, 5]);
        let one = set_of(&[7]);
        assert_eq!((one.total(), one.bounds().len(), one.at(6)), (7, 0, 0));
        let none = set_of(&[0, 0]);
        assert_eq!((none.total(), none.len()), (0, 2));
    }
}
