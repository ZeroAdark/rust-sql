use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use gravity_abi::ctx::lane;
use gravity_abi::{Datum as Word, Lane, ScanPos};
use gravity_driver::{BindingRef, Cursor, Error, Result, Source, Window, WindowBase};
use mcx::Mcx;
use types_core::Oid;
use types_error::{PgError, PgResult};
use types_nodes::node_tree::Node;
use types_nodes::primnodes::{RowCompareExpr, INDEX_VAR};
use types_nodes::NodeList;
use types_rel::{Relation, RelationData, LOCKMODE, RELKIND_MATVIEW, RELKIND_RELATION};
use types_relscan::IndexScanDescData;
use types_scan::sdir::ScanDirection;
use types_scan::scankey::{ScanKeyData, StrategyNumber, SK_ISNULL, SK_ROW_END, SK_ROW_HEADER, SK_ROW_MEMBER, SK_ORDER_BY, SK_SEARCHARRAY, SK_SEARCHNOTNULL, SK_SEARCHNULL};
use types_slot::SlotData;
use types_fmgr::LocalFcinfo;
use types_snapshot::SnapshotData;
use types_storage::lock::NoLock;

pub use crate::ops::shared::rel::{open_heap, open_index, park, pos_of, repin, tid_of};
use crate::ops::shared::rel::snapshot_of;
use crate::scratch::NodeScratch;
use crate::ops::scan::index_window::Quals;
use crate::FilterFn;

mod raw;
use raw::{plain_image, varlena_image};

pub const BTREE_AM_OID: Oid = 403;
pub const HEAP_TABLE_AM_OID: Oid = 2;

/// Whether `heap` is read through the heap table AM, the one page format these sources read: pg_am's heap, or a clone whose
/// handler is heap_tableam_handler (CREATE ACCESS METHOD .. TYPE TABLE HANDLER heap_tableam_handler, which relcache
/// registers at entry build as C resolves rd_tableam: tableam_vocab `TableAm::of`, the range source's test). Not relam 0
/// (a sequence, which these sources never read).
pub fn heap_am(heap: &Relation<'_>) -> bool {
    heap.rd_rel.relam == HEAP_TABLE_AM_OID || (heap.rd_rel.relam != 0 && tableam_vocab::TableAm::of(heap) == Some(tableam_vocab::TableAm::Heap))
}

/// Whether `heap`'s table AM fetches by index TID and walks a bitmap through tableam (the pool forms still need `heap_am`).
pub fn index_fetch_am(heap: &Relation<'_>) -> bool {
    heap.rd_rel.relam != 0 && tableam_vocab::TableAm::of(heap).is_some()
}
/// A btree opfamily's 3-way comparison support function (a row comparison member's `sk_func`, as nodeIndexscan.c).
const BTORDER_PROC: i16 = 1;
/// INDEX_MAX_KEYS: the index columns FormIndexDatum forms for a substituted row's expression keys.
pub(crate) const INDEX_COLS: usize = 32;

/// A site key word's target: key `k`'s own argument, or member `j` of row comparison `r` (`SiteRecipe::rows`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyWord {
    Key(u16),
    Member(u16, u16),
    /// ORDER BY key `j` of an any-AM walk (`AmRecipe::orderby`, V4R-49).
    OrderBy(u16),
}

/// A row comparison index key `(a, b) op (x, y)` (PostgreSQL's ExecIndexBuildScanKeys): scan key `key` is its SK_ROW_HEADER,
/// `members` the SK_ROW_MEMBER template (BTORDER_PROC, the clause's strategy / subtype / collation) the source copies at every
/// seek (nbtree marks and, on a DESC column, commutes the members in place), `heap_attnos` each member's table column (0: an
/// expression column).
#[derive(Clone)]
pub struct RowKey {
    pub key: usize,
    pub members: Vec<ScanKeyData>,
    pub heap_attnos: Vec<i16>,
}

#[derive(Clone)]
pub struct SiteRecipe {
    pub heap: Oid,
    pub index: Oid,
    pub index_lockmode: LOCKMODE,
    pub nkeys: usize,
    pub keys: Vec<ScanKeyData>,
    pub natts: usize,
    pub attnos: Vec<i16>,
    pub key_heap_attnos: Vec<i16>,
    /// Key `k` is `column = ANY (constant array)`: the recipe holds its argument (`SK_SEARCHARRAY`, the plan's Const), the seek
    /// leaves it (the kernel's key cell is a placeholder); a search array is never a substituted row's qual.
    pub host_keys: Vec<bool>,
    /// The search arrays' images, owned by the recipe (the plan's Const is freed with its plan; the template outlives it): `sk_argument` points into them.
    pub host_args: Vec<Option<Arc<[u8]>>>,
    /// A pre-check of the scan node's own qual on each fetched heap row, before its cells are copied into the window
    /// (ExecScan's ExecQual before its projection, execScan.c). The program applies the same qual to every row it gets,
    /// so the pre-check only drops rows early: the walk may skip it (`Prefilter`), never the program. D2988: a bitmap leaf's
    /// is its BitmapHeapScan's qual, checked after the recheck (bitmap_source.rs; exec ops/scan/sites.rs `bitmap_prefilter`).
    pub filter: Option<FilterFn>,
    /// oltpfix G1d (D3065): a btree walk over a unique index with an equality key on every key column (no row, array or
    /// null-test key), so one walk yields at most one visible row. Its qual pre-check (`filter`) can drop at most that one
    /// row, so exec's bind arms none (sites.rs `prefilter`), and the recipe parks (`park_salt`).
    point: bool,
    /// The window's copy of a fetched row is dear (`copy_dear`): a bind arms a qual program's pre-check only then (exec
    /// ops/scan/sites.rs prefilter).
    copy_dear: bool,
    /// The scan's plan node id (`plan_node_id`), for a bind that compiles its qual into `filter`; -1 when unknown.
    plan_node: i32,
    /// The walk's direction (`IndexScan.indexorderdir < 0`): an `ORDER BY k DESC` over the index's
    /// own order reads it backward. The graph's key carries it (`Source::Index.backward`), so a
    /// forward and a backward walk over one index are two programs and two recipes.
    pub backward: bool,
    /// An index-only scan (`IndexOnlyScan`): the rows' cells come from the index tuple, the heap fetched only where the
    /// visibility map does not vouch for the page (`indexonly.rs`).
    pub only: bool,
    /// The row comparison keys (`RowKey`), in key order.
    pub rows: Vec<RowKey>,
    /// The site's key words in cell order: one per key argument, one per row comparison member (a row key's header has none).
    pub words: Vec<KeyWord>,
    /// An any-AM walk's ORDER BY keys (hosts v4/m2_index.rs, V4R-49).
    pub am: Option<crate::ops::scan::index_window::AmRecipe>,
    heap_natts: i32,
    /// The index's access method the recipe was built over (V4R-36): a rebind checks it did not change. A btree recipe
    /// (`from_plan`) is always over btree; an any-AM one (`from_plan_any_am`) over its own AM.
    index_am: Oid,
    /// An index-only walk's heap fetches (PostgreSQL's `InstrCountTuples2`: a tuple on a page the visibility map does not
    /// vouch for), shared by every clone of the recipe (a worker's walk, a rescan); EXPLAIN ANALYZE's "Heap Fetches"
    /// (entry run.rs reads it per site through the registry, D574).
    heap_fetches: Arc<AtomicU64>,
    /// M12 Row B: a bitmap walk's counters (hosts v4/m12.rs), shared by every clone as `heap_fetches` is.
    bitmap: Arc<crate::instr::BitmapCounts>,
    /// The rows one walk may read ahead of its consumer (D583): `Some(n)` under a LIMIT that stops after `n` rows of this
    /// walk (the windows after a seek hold `n` rows in all, then one each), `Some(0)` under one whose count the walk's rows do
    /// not bound (one-row windows), `None` without a LIMIT (windows double). PostgreSQL's Limit never pulls a row past its
    /// count: a row read ahead is a heap visit and a SIREAD lock it never takes (a 40001 where PostgreSQL commits).
    row_budget: Option<u64>,
    /// D588 / D590: the walk's run bound (a parameter Limit, a root-bound path: `v4::m2::index::RunBound`) and its cell, shared
    /// by every clone of the recipe as `heap_fetches` is (a parked Node's walk is the next execution's): entry sets it at each
    /// execution's open and step (the rows the Limit or the FETCH still asks for, `RUN_NONE` when nothing does), the walk's
    /// `IosBatch` takes it at its next window. A clone the template never bound (no bound) has no cell.
    run_bound: crate::ops::scan::index_window::RunBound,
    run: Option<Arc<AtomicU64>>,
    /// D3014O: the worker form of a site a worker probes (`index_probe.rs`), when the walk is a plain point probe.
    worker: Option<Arc<crate::ops::scan::index_probe::WorkerProbe>>,
    /// The leaf form of an index-only site the pool reads (`index_leaves.rs`), when the walk has one.
    leaves: Option<Arc<crate::ops::scan::index_leaves::LeafRecipe>>,
    /// The ranges form of an ordered site the pool reads in key ranges (`index_ranges.rs`), when the walk has one.
    ranges: Option<Arc<crate::ops::scan::index_ranges::RangesRecipe>>,
}

/// planprod P6-c2: a `SiteRecipe` without its lists (`SiteRecipe::split`): the part a program's recipe set (exec admit
/// `SiteTpl`) keeps as admitted while it files the lists in its own root. `SiteRecipe::assemble` puts the two back together.
pub struct SiteCore {
    heap: Oid,
    index: Oid,
    index_lockmode: LOCKMODE,
    nkeys: usize,
    natts: usize,
    filter: Option<FilterFn>,
    point: bool,
    copy_dear: bool,
    plan_node: i32,
    backward: bool,
    only: bool,
    am: Option<crate::ops::scan::index_window::AmRecipe>,
    heap_natts: i32,
    index_am: Oid,
    heap_fetches: Arc<AtomicU64>,
    bitmap: Arc<crate::instr::BitmapCounts>,
    row_budget: Option<u64>,
    run_bound: crate::ops::scan::index_window::RunBound,
    run: Option<Arc<AtomicU64>>,
    worker: Option<Arc<crate::ops::scan::index_probe::WorkerProbe>>,
    leaves: Option<Arc<crate::ops::scan::index_leaves::LeafRecipe>>,
    ranges: Option<Arc<crate::ops::scan::index_ranges::RangesRecipe>>,
}

/// A bound copy (exec admit `SiteTpl::bind_copy`, once per execution): each field as `#[derive(Clone)]` copies it, the
/// shared cells by their count alone; only the any-AM ORDER BY keys (`am`) allocate.
impl Clone for SiteCore {
    fn clone(&self) -> SiteCore {
        SiteCore {
            heap: self.heap,
            index: self.index,
            index_lockmode: self.index_lockmode,
            nkeys: self.nkeys,
            natts: self.natts,
            filter: self.filter.as_ref().map(|f| f.clone()),
            point: self.point,
            copy_dear: self.copy_dear,
            plan_node: self.plan_node,
            backward: self.backward,
            only: self.only,
            am: self.am.clone(), // alloc: Exec
            heap_natts: self.heap_natts,
            index_am: self.index_am,
            heap_fetches: self.heap_fetches.clone(),
            bitmap: self.bitmap.clone(),
            row_budget: self.row_budget,
            run_bound: self.run_bound,
            run: self.run.as_ref().map(|r| r.clone()),
            worker: self.worker.as_ref().map(|w| w.clone()),
            leaves: self.leaves.as_ref().map(|l| l.clone()),
            ranges: self.ranges.as_ref().map(|r| r.clone()),
        }
    }
}

impl SiteCore {
    /// No qual pre-check compiled into the recipe (`SiteRecipe::filter`).
    pub fn unfiltered(&self) -> bool {
        self.filter.is_none()
    }
}

/// planprod P6-c2: a `SiteRecipe`'s lists (`SiteRecipe::split`, `SiteRecipe::assemble`), as the recipe's fields.
pub struct SiteLists {
    pub keys: Vec<ScanKeyData>,
    pub attnos: Vec<i16>,
    pub key_heap_attnos: Vec<i16>,
    pub host_keys: Vec<bool>,
    pub host_args: Vec<Option<Arc<[u8]>>>,
    pub rows: Vec<RowKey>,
    pub words: Vec<KeyWord>,
}

impl SiteRecipe {
    /// The recipe as its core and its lists, each field moved (none copied).
    pub fn split(self) -> (SiteCore, SiteLists) {
        let SiteRecipe { heap, index, index_lockmode, nkeys, keys, natts, attnos, key_heap_attnos, host_keys, host_args, filter, point, copy_dear, plan_node, backward, only, rows, words, am, heap_natts, index_am, heap_fetches, bitmap, row_budget, run_bound, run, worker, leaves, ranges } = self;
        let core = SiteCore { heap, index, index_lockmode, nkeys, natts, filter, point, copy_dear, plan_node, backward, only, am, heap_natts, index_am, heap_fetches, bitmap, row_budget, run_bound, run, worker, leaves, ranges };
        (core, SiteLists { keys, attnos, key_heap_attnos, host_keys, host_args, rows, words })
    }

    /// The recipe `split` took apart, each field moved back.
    pub fn assemble(core: SiteCore, lists: SiteLists) -> SiteRecipe {
        let SiteCore { heap, index, index_lockmode, nkeys, natts, filter, point, copy_dear, plan_node, backward, only, am, heap_natts, index_am, heap_fetches, bitmap, row_budget, run_bound, run, worker, leaves, ranges } = core;
        let SiteLists { keys, attnos, key_heap_attnos, host_keys, host_args, rows, words } = lists;
        SiteRecipe { heap, index, index_lockmode, nkeys, keys, natts, attnos, key_heap_attnos, host_keys, host_args, filter, point, copy_dear, plan_node, backward, only, rows, words, am, heap_natts, index_am, heap_fetches, bitmap, row_budget, run_bound, run, worker, leaves, ranges }
    }
}

/// A run cell holding nothing new for the walk (`SiteRecipe::set_run`).
pub const RUN_NONE: u64 = u64::MAX;

/// Which walk a recipe is built for (`SiteRecipe::build`).
#[derive(Clone, Copy, PartialEq)]
enum Walk {
    Btree,
    Am,
    Bitmap,
}

fn err(msg: &'static str) -> Box<PgError> {
    crate::pg_err(msg)
}

/// D3887: the error an index walk's host loop returns when it stopped for its statement's cancel (`crate::stash` reports
/// it as the kernel's cancel).
const WALK_CANCELED: &str = "gravity: an index walk stopped for its statement's cancel";

impl SiteRecipe {
    pub fn from_plan(heap: &Relation<'_>, index: &Relation<'_>, indexqual: &NodeList<'_>, indexorderby: &NodeList<'_>, index_lockmode: LOCKMODE, attnos: &[i16]) -> PgResult<SiteRecipe> {
        if index.rd_rel.relam != BTREE_AM_OID {
            return Err(err("gravity: valid btree index only"));
        }
        SiteRecipe::build(heap, index, indexqual, indexorderby, index_lockmode, attnos, Walk::Btree, false)
    }

    /// `from_plan` for the v4 btree walks (V4R-108, PERF-OLTP's sketch): a constant `= ANY` array is bound from its key word
    /// at every seek as a parameter's is (V4R-44), never held by the recipe (lowerv4 `key_words` gives the Const a word): a
    /// filed program serves every literal array of its shape, where the image in the recipe made each `IN (…)` list its own
    /// program. The shape table keeps `from_plan`.
    pub fn from_plan_words(heap: &Relation<'_>, index: &Relation<'_>, indexqual: &NodeList<'_>, index_lockmode: LOCKMODE, attnos: &[i16]) -> PgResult<SiteRecipe> {
        if index.rd_rel.relam != BTREE_AM_OID {
            return Err(err("gravity: valid btree index only"));
        }
        SiteRecipe::build(heap, index, indexqual, &NodeList::nil(), index_lockmode, attnos, Walk::Btree, true)
    }

    /// An any-AM index walk's recipe (V4R-49): `from_plan_any_am` for the ordered walk
    /// (no word); the caller adds the walk's ORDER BY keys (`am`, hosts v4/m2_index.rs `AmRecipe::new`).
    pub fn from_plan_am(heap: &Relation<'_>, index: &Relation<'_>, indexqual: &NodeList<'_>, index_lockmode: LOCKMODE, attnos: &[i16]) -> PgResult<SiteRecipe> {
        SiteRecipe::build(heap, index, indexqual, &NodeList::nil(), index_lockmode, attnos, Walk::Am, true)
    }

    /// `from_plan` for the bitmap walk (`bitmap_source.rs`): any access method with `amgetbitmap` (btree, hash, gist, gin,
    /// spgist, brin, bloom; the AM's own recheck flag drives the host's recheck of the original quals), the keys bound the same way
    /// (a constant `= ANY` array too, V4R-108).
    pub fn from_plan_any_am(heap: &Relation<'_>, index: &Relation<'_>, indexqual: &NodeList<'_>, indexorderby: &NodeList<'_>, index_lockmode: LOCKMODE, attnos: &[i16]) -> PgResult<SiteRecipe> {
        SiteRecipe::build(heap, index, indexqual, indexorderby, index_lockmode, attnos, Walk::Bitmap, true)
    }

    /// `walk`: a btree index scan's keys (`from_plan`) also take a row comparison; every walk's keys take an expression
    /// column (the index and any-AM walks form the index's value of it, FormIndexDatum; the bitmap walk rechecks and tests a
    /// substituted row by the plan's `bitmapqualorig` alone, BitmapHeapRecheck). `array_words`: a constant `= ANY` array is bound
    /// from its key word (V4R-108) rather than held by the recipe.
    #[allow(clippy::too_many_arguments)]
    fn build(heap: &Relation<'_>, index: &Relation<'_>, indexqual: &NodeList<'_>, indexorderby: &NodeList<'_>, index_lockmode: LOCKMODE, attnos: &[i16], walk: Walk, array_words: bool) -> PgResult<SiteRecipe> {
        let btree = walk == Walk::Btree;
        if !matches!(heap.rd_rel.relkind, RELKIND_RELATION | RELKIND_MATVIEW) || !index_fetch_am(heap) {
            return Err(err("gravity: a table AM without an index fetch"));
        }
        if !index.rd_index.as_ref().is_some_and(|i| i.indisvalid) {
            return Err(err("gravity: valid index only"));
        }
        if !indexorderby.is_nil() {
            return Err(err("gravity: index qual shape (an ORDER BY operator)"));
        }
        // No qual = the key-less ordered walk (need 8): `nkeys = 0`, begin / rescan with no
        // keys, the whole index in its own order (forward, or backward from the plan's
        // indexorderdir) — a merge join's ordinary sides.
        let mut keys: Vec<ScanKeyData> = (0..indexqual.len()).map(|_| ScanKeyData::empty()).collect(); // alloc: Program
        let mut key_heap_attnos = vec![0i16; indexqual.len()]; // alloc: Program
        let mut host_keys = vec![false; indexqual.len()]; // alloc: Program
        let mut host_args: Vec<Option<Arc<[u8]>>> = vec![None; indexqual.len()]; // alloc: Program
        let (mut rows, mut words) = (Vec::new(), Vec::with_capacity(indexqual.len())); // alloc: Program
        // bounded("other: one scan key built per clause of the plan's index quals, at plan time; the statement's quals bound them")
        for (i, clause) in indexqual.iter().enumerate() {
            // a row comparison is nbtree's (SK_ROW_HEADER): the btree walk's, and a bitmap walk's over a btree index
            if let Some(rc) = clause.as_row_compare_expr().filter(|_| btree || index.rd_rel.relam == BTREE_AM_OID) {
                rows.push(row_key(index, &mut keys[i], rc, i, rows.len(), &mut words)?); // alloc: Program
                continue;
            }
            // `column IS [NOT] NULL` (every AM the planner puts it on has amsearchnulls; D409 for btree and the bitmap walk)
            if let Some(nt) = clause.as_null_test() {
                let var = nt.arg.and_then(|a| strip_relabel(a).as_var()).filter(|v| v.varno == INDEX_VAR).ok_or_else(|| err("gravity: index qual key side"))?;
                key_heap_attnos[i] = key_column(index, var.varattno, index.rd_indcollation.get(var.varattno as usize - 1).copied().unwrap_or(0), Some(walk))?;
                let search = if nt.nulltesttype == types_nodes::primnodes::NullTestType::IS_NULL { SK_SEARCHNULL } else { SK_SEARCHNOTNULL };
                (keys[i].sk_flags, keys[i].sk_attno, host_keys[i]) = (SK_ISNULL | search, var.varattno, true);
                continue;
            }
            words.push(KeyWord::Key(i as u16)); // alloc: Program
            if let Some(sa) = clause.as_scalar_array_op_expr() {
                let var = strip_relabel(sa.args.nth(0)).as_var().filter(|v| v.varno == INDEX_VAR).ok_or_else(|| err("gravity: index qual key side"))?;
                if !sa.useOr || sa.args.len() != 2 || strip_relabel(sa.args.nth(1)).as_var().is_some_and(|v| v.varno == INDEX_VAR) {
                    return Err(err("gravity: index qual argument"));
                }
                key_heap_attnos[i] = key_from_op(index, &mut keys[i], var.varattno, sa.opno, sa.opfuncid, sa.inputcollid, Some(walk))?;
                // a parameter's or an expression's array (V4R-44), and a constant's under `array_words` (V4R-108): the seek binds
                // it from the key word like a comparison's value (nbtree preprocesses the array at every rescan: sorted,
                // deduplicated, NULL elements dropped); a NULL array word matches nothing
                let Some(c) = sa.args.nth(1).as_const().filter(|_| !array_words) else {
                    keys[i].sk_flags = types_scan::scankey::SK_SEARCHARRAY;
                    continue;
                };
                keys[i].sk_flags = types_scan::scankey::SK_SEARCHARRAY | if c.constisnull { types_scan::scankey::SK_ISNULL } else { 0 };
                if !c.constisnull {
                    // the filed recipe keeps the array's image: a bake (gravity_decompose::bake)
                    let img = varlena_image(gravity_decompose::bake::read(c).as_usize() as *const u8).ok_or_else(|| err("gravity: a search array whose image is a toast pointer"))?;
                    keys[i].sk_argument = datum::Datum::from_u64(img.as_ptr() as u64);
                    host_args[i] = Some(img);
                }
                host_keys[i] = true;
                continue;
            }
            let op = clause.as_op_expr().ok_or_else(|| err("gravity: index qual is not an operator"))?;
            let var = strip_relabel(op.args.nth(0)).as_var().filter(|v| v.varno == INDEX_VAR).ok_or_else(|| err("gravity: index qual key side"))?;
            // The argument side may be a Param, a Const (plain-text query, the
            // planner folded it) or any non-INDEX_VAR expression: its value is
            // supplied at seek time from the kernel's key cells, so the recipe
            // only reads the operator, never the argument. Reject only another
            // INDEX_VAR (a column = column qual is not a probe key).
            if op.args.len() != 2 || strip_relabel(op.args.nth(1)).as_var().is_some_and(|v| v.varno == INDEX_VAR) {
                return Err(err("gravity: index qual argument"));
            }
            key_heap_attnos[i] = key_from_op(index, &mut keys[i], var.varattno, op.opno, op.opfuncid, op.inputcollid, Some(walk))?;
        }
        let eq = types_scan::BTEqualStrategyNumber as types_scan::scankey::StrategyNumber;
        let point = btree
            && rows.is_empty()
            && index.rd_index.as_ref().is_some_and(|i| i.indisunique)
            && (1..=index.indnkeyatts()).all(|a| keys.iter().any(|k| k.sk_attno as i32 == a && k.sk_flags == 0 && k.sk_strategy == eq));
        Ok(SiteRecipe { heap: heap.rd_id, index: index.rd_id, index_lockmode, nkeys: indexqual.len(), keys, natts: attnos.len(), attnos: attnos.to_vec(), key_heap_attnos, host_keys, host_args, filter: None, point, copy_dear: copy_dear(heap, attnos), plan_node: -1, backward: false, only: false, rows, words, heap_natts: heap.rd_att.natts, index_am: index.rd_rel.relam, am: None, heap_fetches: Arc::default(), bitmap: Arc::default(), row_budget: None, run_bound: Default::default(), run: None, worker: None, leaves: None, ranges: None }) // alloc: Program
    }


    /// The columns a fetch delivers: none for an index-only walk, a btree walk's own, every one for another index AM's.
    fn fetch_needed(&self, natts: usize) -> Option<Box<[bool]>> {
        if !self.only && self.index_am != BTREE_AM_OID {
            return None;
        }
        let mut m = vec![false; natts]; // alloc: Node
        // bounded("key-columns: one flag per read column and key column, at most MaxTupleAttributeNumber (1664)")
        for &a in self.attnos.iter().chain(&self.key_heap_attnos).filter(|_| !self.only) {
            if let Some(x) = usize::try_from(a).ok().and_then(|a| a.checked_sub(1)).and_then(|i| m.get_mut(i)) {
                *x = true;
            }
        }
        Some(m.into_boxed_slice())
    }

    /// The index-only walk's heap fetches so far over every clone of this recipe (D574).
    pub fn heap_fetches(&self) -> u64 {
        self.heap_fetches.load(Ordering::Relaxed)
    }

    /// D3128: the counter cells every clone of this recipe shares (`heap_fetches`, made with the template as its run
    /// bound and bitmap counts are), by address: which template a registry's recipe was cloned from.
    pub fn cells(&self) -> u64 {
        Arc::as_ptr(&self.heap_fetches) as usize as u64
    }

    /// M12 Row B: the bitmap walk's counters over every clone of this recipe.
    pub fn bitmap(&self) -> &crate::instr::BitmapCounts {
        &self.bitmap
    }

    /// The counters shared with a morsel walk's filters (bitmap_morsel.rs).
    pub fn bitmap_arc(&self) -> Arc<crate::instr::BitmapCounts> {
        self.bitmap.clone()
    }

    pub fn with_filter(mut self, filter: Option<FilterFn>) -> SiteRecipe {
        self.filter = filter;
        self
    }

    pub fn with_plan_node(mut self, id: i32) -> SiteRecipe {
        self.plan_node = id;
        self
    }

    /// Whether the window's copy of a fetched row is dear (`copy_dear`).
    /// At most one visible row per walk (`point`): a unique btree index, equality on every key column.
    pub fn point(&self) -> bool {
        self.point
    }

    pub fn copy_dear(&self) -> bool {
        self.copy_dear
    }

    /// The scan's plan node id, `None` when the recipe was built without it.
    pub fn plan_node(&self) -> Option<i32> {
        (self.plan_node >= 0).then_some(self.plan_node)
    }

    /// The graph's source and the shape table's probe both carry the plan's `indexorderdir`.
    pub fn with_only(mut self, only: bool) -> SiteRecipe {
        self.only = only;
        self
    }

    /// The walk's read-ahead bound (`row_budget`, D583).
    pub fn row_budget(&self) -> Option<u64> {
        self.row_budget
    }

    /// The walk's read-ahead bound (`row_budget`, D583), exec's `limit_budget` of the site's plan node.
    pub fn with_row_budget(mut self, budget: Option<u64>) -> SiteRecipe {
        self.row_budget = budget;
        self
    }

    /// The walk's run bound (D588 / D590), exec's `run_bound` of the site's plan node: a cell when anything bounds the run.
    pub fn with_run_bound(mut self, bound: crate::ops::scan::index_window::RunBound) -> SiteRecipe {
        self.run = bound.any().then(|| Arc::new(AtomicU64::new(RUN_NONE))); // alloc: Program
        self.run_bound = bound;
        self
    }

    pub fn run_bound(&self) -> crate::ops::scan::index_window::RunBound {
        self.run_bound
    }

    /// The rows the walk may read from its next window on (`None`: nothing bounds them; `RUN_NONE` stored), for every clone.
    pub fn set_run(&self, rows: Option<u64>) {
        if let Some(c) = self.run.as_ref() {
            c.store(rows.unwrap_or(RUN_NONE), Ordering::Relaxed);
        }
    }

    pub fn with_backward(mut self, backward: bool) -> SiteRecipe {
        self.backward = backward;
        self
    }

    /// D3014O: the recipe of a site a worker probes (`SiteDesc::worker_probe`) keeps its worker form when the walk is a
    /// plain point probe (`WorkerProbe::of`); after `with_backward` / `with_only`.
    pub fn with_worker(mut self, heap: &Relation<'_>) -> SiteRecipe {
        self.worker = heap_am(heap).then(|| crate::ops::scan::index_probe::WorkerProbe::of(&self, heap)).flatten().map(Arc::new); // alloc: Program
        self
    }

    /// D3014O: the worker form (`with_worker`), `None` when the walk has none.
    pub fn worker(&self) -> Option<&crate::ops::scan::index_probe::WorkerProbe> {
        self.worker.as_deref()
    }

    /// The leaf form of an index-only site the pool reads (`index_leaves.rs`, `SiteDesc::leaves`): `None` when the walk
    /// has none (`LeafRecipe::of`), so exec runs its pipeline on the leader.
    pub fn with_leaves(mut self, heap: &Relation<'_>, index: &Relation<'_>) -> PgResult<SiteRecipe> {
        if !heap_am(heap) {
            return Ok(self);
        }
        self.leaves = crate::ops::scan::index_leaves::LeafRecipe::of(&self, heap, index)?.map(Arc::new); // alloc: Program
        Ok(self)
    }

    /// The leaf form (`with_leaves`), `None` when the walk has none.
    pub fn leaves(&self) -> Option<&crate::ops::scan::index_leaves::LeafRecipe> {
        self.leaves.as_deref()
    }

    /// The ranges form of an ordered site the pool reads in key ranges (`index_ranges.rs`, `SiteDesc::ranges`): `None`
    /// when the walk has none (`RangesRecipe::of`), so exec runs its pipeline on the leader.
    pub fn with_ranges(mut self, heap: &Relation<'_>, index: &Relation<'_>) -> PgResult<SiteRecipe> {
        if !heap_am(heap) {
            return Ok(self);
        }
        self.ranges = crate::ops::scan::index_ranges::RangesRecipe::of(&self, heap, index)?.map(Arc::new); // alloc: Program
        Ok(self)
    }

    /// The ranges form (`with_ranges`), `None` when the walk has none.
    pub fn ranges(&self) -> Option<&crate::ops::scan::index_ranges::RangesRecipe> {
        self.ranges.as_deref()
    }

    /// The recipe's keys bound to one execution's key words (`words` / `nulls`, the site's key cells as a seek reads
    /// them; a search array's compressed or external image detoasted into `mcx`): `None` when a key's argument is NULL,
    /// which matches nothing (`seek`'s rule). Plain keys only: a row comparison member or an ORDER BY word is refused.
    pub fn bind_keys(&self, mcx: Mcx<'_>, words: &[Word], nulls: &[u8]) -> PgResult<Option<Vec<ScanKeyData>>> {
        if words.len() < self.words.len() || nulls.len() < self.words.len() {
            return Err(crate::pg_err("gravity: a key bind with fewer key words than its recipe"));
        }
        let mut keys = self.keys[..self.nkeys].to_vec(); // alloc: Exec
        // bounded("other: one key word per scan key of the site, which the statement's index quals bound")
        for (w, word) in self.words.iter().enumerate() {
            match *word {
                KeyWord::Key(i) if !self.host_keys[i as usize] => {
                    if nulls[w] != 0 {
                        return Ok(None);
                    }
                    let k = &mut keys[i as usize];
                    k.sk_argument = datum::Datum::from_u64(words[w]);
                    if k.sk_flags & SK_SEARCHARRAY != 0 {
                        k.sk_argument = plain_array(mcx, k.sk_argument)?;
                    }
                }
                KeyWord::Key(_) => {}
                KeyWord::OrderBy(_) | KeyWord::Member(..) => return Err(crate::pg_err("gravity: a key bind over a row comparison or an ORDER BY key")),
            }
        }
        Ok(Some(keys))
    }

    /// An index-only walk's heap fetch counter (EXPLAIN ANALYZE's "Heap Fetches"), shared by every clone of the recipe.
    pub fn heap_fetch_cell(&self) -> Arc<AtomicU64> {
        self.heap_fetches.clone()
    }

    /// The heap's attribute count the recipe was built over (a rebind checks the relation did not change shape).
    pub fn heap_natts(&self) -> i32 {
        self.heap_natts
    }

    /// The recipe's identity for a park key (H16 b, B1: the simple protocol parks a Node over index recipes, keyed by the
    /// program's content, this salt and the relcache epoch): everything the recipe holds of the statement that built it,
    /// FNV-1a. A key's argument is left out unless the recipe owns it (a search array's image): the seek overwrites the
    /// others from the kernel's key cells every time. `None` for a recipe with a filter (a closure over the admitting plan
    /// no hash can compare): such a Node is never parked across statements.
    pub fn park_salt(&self) -> Option<u64> {
        if self.filter.is_some() || self.am.is_some() {
            return None;
        }
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        let mut mix = |w: u64| {
            // bounded("const-domain: a word's eight bytes")
            for b in w.to_le_bytes() {
                h = (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
            }
        };
        for w in [self.heap as u64, self.index as u64, self.index_lockmode as u64, self.nkeys as u64, self.natts as u64, self.heap_natts as u64, self.backward as u64, self.only as u64] {
            mix(w);
        }
        mix(self.attnos.len() as u64);
        self.attnos.iter().for_each(|&a| mix(a as u64));
        mix(self.key_heap_attnos.len() as u64);
        self.key_heap_attnos.iter().for_each(|&a| mix(a as u64));
        // bounded("other: one scan key of the recipe per pass, seven words each; the statement's index quals bound them")
        for (k, key) in self.keys.iter().enumerate().take(self.nkeys) {
            let host = self.host_keys.get(k).copied().unwrap_or(false);
            for w in [key.sk_flags as u64, key.sk_attno as u64, key.sk_strategy as u64, key.sk_subtype as u64, key.sk_collation as u64, key.sk_func.fn_oid as u64, host as u64] {
                mix(w);
            }
            if let Some(img) = self.host_args.get(k).and_then(|a| a.as_ref()).filter(|_| host) {
                mix(img.len() as u64);
                // bounded("other: the image of a search array the recipe owns (a constant of the statement's text, which bounds it), eight bytes a pass, hashed once at the park key's first ask")
                for c in img.chunks(8) {
                    let mut w = [0u8; 8];
                    w[..c.len()].copy_from_slice(c);
                    mix(u64::from_le_bytes(w));
                }
            }
        }
        // bounded("other: one row comparison of the recipe's keys per pass; the statement's index quals bound them")
        for r in &self.rows {
            mix(r.key as u64);
            // bounded("small-fixed: a row compare's members (at most INDEX_MAX_KEYS), seven words each")
            for (m, &a) in r.members.iter().zip(r.heap_attnos.iter()) {
                for w in [m.sk_flags as u64, m.sk_attno as u64, m.sk_strategy as u64, m.sk_subtype as u64, m.sk_collation as u64, m.sk_func.fn_oid as u64, a as u64] {
                    mix(w);
                }
            }
        }
        // bounded("other: one key word of the recipe per pass; the statement's index quals bound them")
        for w in &self.words {
            mix(match *w {
                KeyWord::Key(k) => k as u64,
                KeyWord::Member(r, j) => 1 << 32 | (r as u64) << 16 | j as u64,
                KeyWord::OrderBy(j) => 2 << 32 | j as u64,
            });
        }
        Some(h)
    }
}

/// One scan key from the plan's operator on index column `attno`: strategy
/// and subtype from the column's opfamily, the operator's function as the
/// plan resolved it, the clause's collation. Returns the heap column the
/// index column is on: 0 for an expression column, which a walk's key takes
/// (`walk`, `key_column`) and the equality probe's (`None`) does not.
#[allow(clippy::too_many_arguments)]
fn key_from_op(index: &Relation<'_>, k: &mut ScanKeyData, attno: i16, opno: Oid, opfuncid: Oid, collid: Oid, walk: Option<Walk>) -> PgResult<i16> {
    let heap_attno = key_column(index, attno, collid, walk)?;
    let opfamily = index.rd_opfamily[attno as usize - 1];
    let (strategy, _left, right) = gravity_fn::typesem::op_opfamily_properties(opno, opfamily, false)?;
    k.sk_flags = 0;
    k.sk_attno = attno;
    k.sk_strategy = strategy as types_scan::scankey::StrategyNumber;
    k.sk_subtype = right;
    k.sk_collation = collid;
    fmgr_core::fmgr_info_into(opfuncid, &mut k.sk_func)?;
    Ok(heap_attno)
}

/// Index column `attno`'s table column (0: an expression column, taken by a walk's key, not the equality probe's), refused
/// by name when a host-rechecked key (the index and any-AM walks'; the bitmap walk rechecks by `bitmapqualorig`) is in a
/// collation that is not its column's (the planner's IndexCollMatchesExprColl, kept here as the key's own check).
fn key_column(index: &Relation<'_>, attno: i16, collid: Oid, walk: Option<Walk>) -> PgResult<i16> {
    let exprs = walk.is_some();
    if attno < 1 || attno as i32 > index.indnkeyatts() {
        return Err(err("gravity: index qual attno"));
    }
    let heap_attno = index.rd_index.as_ref().and_then(|f| f.indkey.get(attno as usize - 1).copied()).unwrap_or(0);
    if heap_attno < 0 || (heap_attno == 0 && !exprs) {
        return Err(err("gravity: expression index key"));
    }
    let coll = index.rd_indcollation.get(attno as usize - 1).copied().unwrap_or(0);
    if matches!(walk, Some(Walk::Btree | Walk::Am)) && coll != 0 && coll != collid {
        return Err(err("gravity: an index key whose collation is not its index column's"));
    }
    Ok(heap_attno)
}

/// Row comparison `rc` (clause `key`, row `r`) as PostgreSQL's ExecIndexBuildScanKeys builds it: a SK_ROW_MEMBER per column
/// (BTORDER_PROC of the column's opfamily, the operator's strategy, which must be the comparison's, its right type, the
/// member's collation), SK_ROW_END on the last, and `header` the SK_ROW_HEADER (the first member's column, the comparison's
/// strategy; its argument is the source's member array). Each member's value is a site key word (`KeyWord::Member`).
fn row_key(index: &Relation<'_>, header: &mut ScanKeyData, rc: &RowCompareExpr<'_>, key: usize, r: usize, words: &mut Vec<KeyWord>) -> PgResult<RowKey> {
    let n = rc.opnos.len();
    if n == 0 || rc.largs.len() != n || rc.rargs.len() != n || rc.inputcollids.len() != n {
        return Err(err("gravity: a row comparison index key with unlike sides"));
    }
    let (mut members, mut heap_attnos) = (Vec::with_capacity(n), Vec::with_capacity(n)); // alloc: Program
    // bounded("small-fixed: a row compare's members (at most INDEX_MAX_KEYS, 32), one member key built each, at plan time")
    for j in 0..n {
        let var = strip_relabel(rc.largs.nth(j)).as_var().filter(|v| v.varno == INDEX_VAR).ok_or_else(|| err("gravity: index qual key side"))?;
        if strip_relabel(rc.rargs.nth(j)).as_var().is_some_and(|v| v.varno == INDEX_VAR) {
            return Err(err("gravity: index qual argument"));
        }
        let (attno, collid) = (var.varattno, rc.inputcollids.nth(j));
        heap_attnos.push(key_column(index, attno, collid, Some(Walk::Btree))?); // alloc: Program
        let opfamily = index.rd_opfamily[attno as usize - 1];
        let (strategy, left, right) = gravity_fn::typesem::op_opfamily_properties(rc.opnos.nth(j), opfamily, false)?;
        if strategy != rc.cmptype {
            return Err(err("gravity: a row comparison member whose operator is not the comparison's"));
        }
        let proc = gravity_fn::typesem::opfamily_proc(opfamily, left, right, BTORDER_PROC)?;
        if proc == 0 {
            return Err(err("gravity: a row comparison member without its opfamily's comparison function"));
        }
        let mut m = ScanKeyData::empty();
        m.sk_flags = SK_ROW_MEMBER;
        m.sk_attno = attno;
        m.sk_strategy = strategy as StrategyNumber;
        m.sk_subtype = right;
        m.sk_collation = collid;
        fmgr_core::fmgr_info_into(proc, &mut m.sk_func)?;
        members.push(m); // alloc: Program
        words.push(KeyWord::Member(r as u16, j as u16)); // alloc: Program
    }
    if let Some(last) = members.last_mut() {
        last.sk_flags |= SK_ROW_END;
    }
    *header = ScanKeyData::empty();
    header.sk_flags = SK_ROW_HEADER;
    header.sk_attno = members[0].sk_attno;
    header.sk_strategy = rc.cmptype as StrategyNumber;
    Ok(RowKey { key, members, heap_attnos })
}

/// D4528: whether a bound search array's image is read in place (a plain 4-byte or short header), so `plain_array` has
/// nothing to expand: the bitmap sources' bind (`bitmap_window::bind_words`) asks before it opens a seek region.
pub(crate) fn search_array_plain(d: datum::Datum) -> bool {
    plain_image(d)
}

/// V4R-44: a bound search array's image as nbtree reads it (a plain 4-byte or short header): a compressed or external
/// image detoasted into `mcx`. D4528: the bitmap sources' bind uses it too.
pub(crate) fn plain_array(mcx: Mcx<'_>, d: datum::Datum) -> PgResult<datum::Datum> {
    if plain_image(d) {
        return Ok(d);
    }
    let p = d.as_u64() as *const u8;
    // as above; a toast pointer's or a compressed image's length is its header's
    let img = raw::varlena_bytes(p);
    let v = detoast_seams::detoast_attr::call(mcx, img)?;
    Ok(datum::Datum::from_u64(v.leak().as_ptr() as u64))
}

/// V4R-44: `column op ANY (array)` for a substituted row: some non-null element of the key's array passes its operator.
fn any_element(mcx: Mcx<'_>, key: &mut ScanKeyData, d: datum::Datum, fcinfo: &mut LocalFcinfo<2>) -> PgResult<bool> {
    let p = key.sk_argument.as_u64() as *const u8;
    // the key's array image is live (the recipe's Const image or the seek's plain image)
    let img: Vec<u8> = raw::array_image(p);
    let (len, byval, align) = gravity_fn::typesem::storage::len_byval_align(arrayfuncs::arr_elemtype(&img))?;
    let (vals, nulls) = arrayfuncs::deconstruct_array(mcx, &img, len as i32, byval, align as u8, true)?;
    // an element's operator call is the unit: the array is as long as its value
    let mut step = cancelpoll::step("index_source any_element: one operator call per array element");
    for (v, _) in vals.iter().zip(nulls.iter()).filter(|(_, n)| !**n) {
        step.tick().map_err(|_| err(WALK_CANCELED))?;
        fcinfo.rearm(key.sk_collation);
        // the exec context outlives this call; reset at unbind
        crate::ops::scan::raw::arm_result_mcx(fcinfo, mcx);
        fcinfo.set_arg(0, d);
        fcinfo.set_arg(1, *v);
        let r = key.sk_func.invoke(fcinfo)?;
        if !fcinfo.isnull && r.as_bool() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn strip_relabel(n: Node<'_>) -> Node<'_> {
    match n.as_relabel_type() {
        Some(r) => r.arg,
        None => n,
    }
}

pub struct IndexSource {
    recipe: SiteRecipe,
    keys: mcx::PgVec<'static, ScanKeyData>,
    /// The row comparison keys' members (`SiteRecipe::rows`), each header's `sk_argument`: the source's own, refilled per seek.
    rows: Vec<Vec<ScanKeyData>>,
    heap: Option<Relation<'static>>,
    index: Option<Relation<'static>>,
    parked_heap: Option<Weak<RelationData<'static>>>,
    parked_index: Option<Weak<RelationData<'static>>>,
    scan: Option<IndexScanDescData<'static>>,
    slot: Option<SlotData<'static>>,
    snapshot: Option<Rc<SnapshotData<'static>>>,
    /// A recheck's row (`substitute`: EvalPlanQual's test tuple of the scanned relation), handed at every open while it
    /// passes that open's keys, as ExecScanFetch hands the test slot after each rescan (D3508: a nest's inner re-opened
    /// at every row of a union its recheck runs again); a key it fails clears it (IndexRecheck false: ExecClearTuple of
    /// the test slot), so no later open yields it. The scan's own qual failing keeps it (ExecScan's qual, after the fetch).
    /// A REFERENCE mark's row (PostgreSQL refetches it at each rescan, a failed key or not) opens once per recheck in
    /// every served form: its nest's outer yields one row there.
    substituted: Option<ScanPos>,
    /// `substitute` ran this binding: no open walks the index (ExecScanFetch never reaches the access method for a
    /// relation with a test tuple), the substituted row or nothing.
    recheck: bool,
    /// The index's IndexInfo for a substituted row's expression keys and an any-AM walk's lossy recheck or distance on an
    /// expression column (FormIndexDatum), built in the exec context at the first such row of a bind and dropped at unbind
    /// (declared before `scratch`: it drops first).
    index_info: Option<execindexing::IndexInfo<'static>>,
    fcinfo: LocalFcinfo<2>,
    pos: ScanPos,
    open: bool,
    bound: bool,
    row: Vec<Word>,
    row_valid: Vec<u64>,
    /// The row protocol's cells (`natts`), gathered into `row` per window.
    vbuf: Vec<Word>,
    nbuf: Vec<u8>,
    cur: Cursor,
    /// D2181: the plain images of one seek's compressed or external search arrays (`plain_array`), made at the first
    /// such seek and reset at each later one, when the keys that pointed into them are rebound (nbtree reads a bound
    /// array only in the preprocessing of the rescan that follows its seek). The exec context lives until unbind, so an
    /// image per rescan there grew a nest loop's inner with its rescan count (a toast fetch's index scan with each).
    seek_images: crate::ops::scan::raw::SeekImages,
    /// An any-AM walk's run state (V4R-49): its bound ORDER BY keys and reorder queue. Before `scratch`: its comparison's
    /// sort supports are in the scratch's node context (memsig E-pub t3).
    am: Option<crate::ops::scan::index_window::AmWalk>,
    scratch: NodeScratch,
    /// The index-only scan's visibility-map pin and its out attnos as index attribute numbers (`indexonly.rs`).
    vm: visibilitymap::VmBuffer,
    itup_attnums: Vec<i16>,
    itup_names: Vec<bool>,
    /// D3453: the index-only walk's image column per index column (indexonly.rs `image_cols`), set with `itup_attnums`.
    image_cols: Vec<i16>,
    name_bufs: Vec<u8>,
    /// D2900: the index-only walk's heap fetches not yet added to the recipe's shared count (`SiteRecipe::heap_fetches`, an
    /// atomic every clone shares): counted per row as PostgreSQL's `InstrCountTuples2` (a plain counter), added once a
    /// window (`flush_fetches`), at the end of `next` and at unbind, so a reader after the walk sees every fetch.
    fetched: u64,
    /// M2's N-row index-only windows (hosts v4/m2_index.rs), asked by a site that keeps a RowId lane: made at the first bind.
    batched: bool,
    batch: Option<crate::ops::scan::index_window::IosBatch>,
    pre: Prefilter,
    proj: crate::ops::shared::heap_fetch::Proj,
}

/// The columns past which a projection's deform alone is dear (`copy_dear`).
pub const PREFILTER_DEFORM: i16 = 8;

/// Whether the window's copy of a fetched row is dear: the walk projects a by-reference cell (copied into the window) or
/// deforms past `PREFILTER_DEFORM` columns. A copy of a few by-value cells costs less than an interpreted qual program.
fn copy_dear(heap: &Relation<'_>, attnos: &[i16]) -> bool {
    let by_ref = |a: i16| a > 0 && heap.rd_att.attrs.get(a as usize - 1).is_some_and(|x| !x.attbyval);
    attnos.iter().any(|&a| by_ref(a)) || attnos.iter().copied().max().unwrap_or(0) > PREFILTER_DEFORM
}

/// The recipe's qual pre-check (`SiteRecipe::filter`) and its tally over the source's life. A row it keeps pays for the qual
/// twice (here and in the program), a row it drops saves the window's copy of its cells and the program's filter, so it
/// stays on while it has kept no more rows than it dropped, plus one page-sized window (`IOS_MAX_ROWS`). D2988: a bitmap
/// heap walk's page windows keep one too (bitmap_window.rs `PageBatch::fill`, bitmap_source.rs).
#[derive(Default)]
pub struct Prefilter {
    kept: u64,
    dropped: u64,
    /// The qual raised an error: the walk checks nothing early from then on.
    failed: bool,
}

impl Prefilter {
    pub fn on(&self) -> bool {
        !self.failed && self.kept <= self.dropped + crate::ops::scan::index_window::IOS_MAX_ROWS as u64
    }

    /// One row's pre-check result: keep it or not. An error the qual raised from the row keeps the row: a window may read
    /// rows past the one where PostgreSQL stops, and the program, which evaluates the qual on the row again, raises it or
    /// holds it until PostgreSQL would reach the row, as without the pre-check; the qual's state is not used again. An
    /// interrupt (SQLSTATE class 57: a cancel, a statement timeout, a shutdown) is raised: ProcessInterrupts clears its
    /// pending flag as it raises it (postgres.c), so a kept row would lose it.
    pub fn check(&mut self, r: PgResult<bool>) -> PgResult<bool> {
        let keep = match r {
            Ok(k) => k,
            Err(e) if types_error::errcode_to_category(e.sqlstate()) == types_error::ERRCODE_OPERATOR_INTERVENTION => return Err(e),
            Err(_) => {
                self.failed = true;
                true
            }
        };
        if keep {
            self.kept += 1;
        } else {
            self.dropped += 1;
        }
        Ok(keep)
    }
}

impl IndexSource {
    pub fn new(recipe: SiteRecipe) -> IndexSource {
        // memsig E-pub t3: the bound keys in the host's node scratch (declared after `keys`), homed with it at the bind
        let scratch = NodeScratch::new("GravityIndexSource");
        let mut keys = mcx::PgVec::new_in(scratch.mcx());
        mcx::GrowIn::extend_from_slice_in(&mut keys, &recipe.keys);
        let rows: Vec<Vec<ScanKeyData>> = recipe.rows.iter().map(|r| r.members.clone()).collect(); // alloc: Node
        // bounded("other: one pointer per row comparison of the recipe's keys, which the statement's index quals bound")
        for (r, m) in recipe.rows.iter().zip(rows.iter()) {
            keys[r.key].sk_argument = datum::Datum::from_usize(m.as_ptr() as usize);
        }
        let natts = recipe.natts;
        let am = recipe.am.as_ref().map(|a| crate::ops::scan::index_window::AmWalk::new(a, &scratch));
        let proj = crate::ops::shared::heap_fetch::Proj::of(&recipe.attnos);
        IndexSource { recipe, keys, rows, heap: None, index: None, parked_heap: None, parked_index: None, scan: None, slot: None, snapshot: None, substituted: None, recheck: false, fcinfo: LocalFcinfo::<2>::new(0), pos: ScanPos::NONE, open: false, bound: false, row: vec![0; natts], row_valid: vec![0; natts], vbuf: vec![0; natts], nbuf: vec![0; natts], cur: Cursor::new(), seek_images: Default::default(), scratch, vm: visibilitymap::VmBuffer::default(), itup_attnums: Vec::new(), itup_names: Vec::new(), image_cols: Vec::new(), name_bufs: Vec::new(), fetched: 0, index_info: None, batched: false, batch: None, am, pre: Prefilter::default(), proj } // alloc: Node
    }

    pub fn scratch_used(&self) -> usize {
        self.scratch.used()
    }

    /// `scratch_used` by root (node, exec, bind).
    pub fn scratch_parts(&self) -> [usize; 3] {
        self.scratch.used_parts()
    }

    /// N-row windows (M2, the index N-row windows card): a site that keeps a RowId lane batches its rows (hosts
    /// v4/m2_index.rs `IosBatch`) when every column is a table column or a V4R-88 system lane, a by-value word (a `ctid`
    /// cell is the slot's, not copied).
    pub fn batched(mut self, on: bool) -> IndexSource {
        self.batched = on && self.recipe.attnos.iter().all(|&a| a > 0);
        self
    }

    fn bind_pg(&mut self, snapshot: Rc<SnapshotData<'static>>) -> PgResult<()> {
        let mcx = self.scratch.mcx();
        let heap = open_heap(mcx, &self.parked_heap, self.recipe.heap)?;
        let index = match open_index(mcx, &self.parked_index, self.recipe.index, self.recipe.index_lockmode) {
            Ok(i) => i,
            Err(e) => {
                heap.close(NoLock)?;
                return Err(e);
            }
        };
        let stale = heap.rd_att.natts != self.recipe.heap_natts
            || !index_fetch_am(&heap)
            || index.rd_rel.relam != self.recipe.index_am
            || !index.rd_index.as_ref().is_some_and(|i| i.indisvalid && i.indrelid == heap.rd_id);
        if stale {
            index.close(NoLock)?;
            heap.close(NoLock)?;
            return Err(err("gravity: relation changed under the parked plan"));
        }
        if !heap.is_scannable() {
            // an unpopulated materialized view: PostgreSQL's ExecOpenScanRelation error (the heap walk's, heap_source.rs)
            let e = crate::ops::scan::heap_source::unpopulated(heap.name());
            index.close(NoLock)?;
            heap.close(NoLock)?;
            return Err(e);
        }
        if self.recipe.only && self.itup_attnums.is_empty() {
            // D3453: the columns the AM returns (a non-returnable one's image cell is NULL), asked once per bind
            let only = crate::ops::scan::indexonly::returnable(mcx, &index, self.recipe.index_am == BTREE_AM_OID)
                .and_then(|r| crate::ops::scan::indexonly::index_attnums(&index, &heap, &self.recipe.attnos, &r).map(|x| (x, r)));
            match only {
                Ok(((v, names), r)) => {
                    if names.iter().any(|&n| n) {
                        self.name_bufs = vec![0; names.len() * crate::ops::scan::indexonly::NAMEDATALEN]; // alloc: Node
                    }
                    (self.itup_attnums, self.itup_names) = (v, names);
                    self.image_cols = crate::ops::scan::indexonly::image_cols(&index, &r);
                }
                Err(e) => {
                    index.close(NoLock)?;
                    heap.close(NoLock)?;
                    return Err(e);
                }
            }
        }
        match self.scan.as_mut() {
            None => {
                // V4R-49: a scan of an AM that cannot park lives in the exec context and ends at unbind
                let (smcx, norderbys) = if self.recipe.index_am == BTREE_AM_OID { (mcx, 0) } else { (self.scratch.exec_mcx(), self.am.as_ref().map_or(0, |a| a.orderby.len() as i32)) };
                self.scan = Some(indexam::index_beginscan(smcx, &heap, &index, snapshot.clone(), self.recipe.nkeys as i32, norderbys)?);
                if self.slot.is_none() {
                    self.slot = Some(tableam::table_slot_create(mcx, &heap)?);
                }
            }
            Some(scan) => indexam::index_rearmscan(scan, &heap, &index, snapshot.clone())?,
        }
        if let Some(scan) = self.scan.as_mut() {
            scan.xs_want_itup = self.recipe.only;
            if !heap_am(&heap) {
                if let Some(indexam::IndexFetchTableData::Table(f)) = scan.xs_heapfetch.as_mut() {
                    tableam::table_index_fetch_set_needed_attrs(f, self.recipe.fetch_needed(heap.rd_att.natts as usize));
                }
            }
        }
        if self.batched && self.batch.is_none() {
            self.batch = Some(crate::ops::scan::index_window::IosBatch::new(&heap, &self.recipe.attnos, self.recipe.row_budget, self.recipe.run.as_ref().map(|r| r.clone())));
        }
        self.heap = Some(heap);
        self.index = Some(index);
        self.snapshot = Some(snapshot);
        self.pos = ScanPos::NONE;
        self.open = false;
        self.bound = true;
        Ok(())
    }

    fn unbind_pg(&mut self) -> PgResult<()> {
        self.flush_fetches();
        self.open = false;
        self.substituted = None;
        self.recheck = false;
        if !self.bound {
            return Ok(());
        }
        self.bound = false;
        self.vm.release();
        if let Some(slot) = self.slot.as_mut() {
            exectuples::exec_clear_tuple(slot, self.scratch.mcx());
        }
        if let Some(w) = self.am.as_mut() {
            w.rewind();
        }
        if self.recipe.index_am != BTREE_AM_OID {
            if let Some(scan) = self.scan.take() {
                indexam::index_endscan(scan)?;
            }
        } else if let Some(scan) = self.scan.as_mut() {
            indexam::index_parkscan(scan)?;
        }
        self.parked_index = park(self.index.take())?;
        self.parked_heap = park(self.heap.take())?;
        self.snapshot = None;
        self.index_info = None;
        self.scratch.reset_exec();
        Ok(())
    }

    /// The EPQ test: the newest version at `pos` (any visibility), against
    /// this site's index keys (as ordinary quals, PostgreSQL's IndexRecheck)
    /// and its filter. A pass yields that version and moves the site's
    /// position to it; a miss yields no row.
    /// An N-row window (hosts v4/m2_index.rs `IosBatch::fill`): `next`'s rows, each copied into the window.
    fn window_batch(&mut self, w: &mut Window<'_>) -> Result<u32> {
        let Some(mut b) = self.batch.take() else { return Ok(0) };
        // D544: an any-AM walk's window is one row (its AM's consistent / distance / recheck calls on an entry past the row
        // where PostgreSQL's LIMIT stops pulling can raise, gist's KNN distance among them)
        if self.recipe.index_am != BTREE_AM_OID {
            b.rewind();
        }
        let heap = self.recipe.heap;
        // a heap row of table columns goes from the slot into the window; an index-only row or one with a system cell
        // comes as next's arrays
        let got = if self.recipe.only || !self.proj.plain() {
            b.fill_rows(|v, n| Ok::<_, Error>(if self.next_row(v, n)? { Some(self.pos) } else { None }), heap, w)
        } else {
            b.fill(|c| Ok::<_, Error>(if self.next_cells(c)? { Some(self.pos) } else { None }), heap, w)
        };
        self.flush_fetches();
        self.batch = Some(b);
        let (rows, last, ended) = got?;
        self.open &= !ended;
        if rows > 0 {
            self.pos = last;
        }
        Ok(rows)
    }

    /// The heap walk's next row, its qual's pre-check passed, left in the slot with `pos` at it; false at the end.
    fn walk(&mut self) -> Result<bool> {
        let mcx = self.scratch.exec_mcx();
        let dir = if self.recipe.backward { ScanDirection::BackwardScanDirection } else { ScanDirection::ForwardScanDirection };
        // D3970 / D4046 (C9): a tick per row visited, dropped or passed, and D4213 one per TID whose row is not visible (a
        // row is a heap fetch, microseconds: a per-unit tick); a heap fetch's own interrupt check runs under its buffer's
        // content lock (held off)
        let mut step = cancelpoll::step("index_source walk: the heap walk's next row");
        loop {
            let (scan, slot) = (self.scan.as_mut().expect("bound"), self.slot.as_mut().expect("bound"));
            let got = match (self.am.as_mut(), self.recipe.am.as_ref()) {
                (Some(w), Some(am)) => w.next(&mut step, am, Quals { keys: &mut self.keys[..self.recipe.nkeys], heap: &self.recipe.key_heap_attnos, index: self.index.as_ref().expect("bound"), info: &mut self.index_info, cols: &[] }, mcx, scan, slot, dir),
                _ => crate::ops::scan::index_window::getnext_slot_polled(&mut step, mcx, scan, dir, slot),
            };
            if !got.map_err(crate::stash)? {
                self.open = false;
                return Ok(false);
            }
            step.tick()?;
            // the qual's pre-check before the row's cells are read: a row it drops is deformed only as far as the qual reads
            if let Some(f) = self.recipe.filter.as_ref().filter(|_| self.pre.on()) {
                if !self.pre.check(f(slot)).map_err(crate::stash)? {
                    // EXPLAIN ANALYZE's Rows Removed by Filter: a row the walk's qual drops never reaches the program's Filter
                    self.recipe.bitmap().add(crate::instr::BitmapCounts::FILTERED, 1);
                    continue;
                }
            }
            // D2947: a row of the reorder queue (AmWalk::next: a lossy KNN distance) carries the invalid TID PostgreSQL's
            // ExecForceStoreHeapTuple leaves it, its ctid (4294967295,0) as there
            self.pos = crate::ops::shared::rel::pos_of_any(self.recipe.heap, &slot.base().tts_tid);
            return Ok(true);
        }
    }

    /// `next` of a heap walk over table columns into an N-row window's row (`IosBatch::fill`): the cells go from the slot's
    /// deformed columns straight into the window's lanes, with no row arrays between (one deform to the highest column
    /// read, as `heap_fetch::project`): the same row and the same cells as `next`.
    fn next_cells(&mut self, cells: &mut crate::ops::scan::index_window::Cells<'_>) -> Result<bool> {
        debug_assert!(!self.recipe.only && self.proj.plain() && !self.recheck);
        if !self.open || !self.walk()? {
            return Ok(false);
        }
        let slot = self.slot.as_mut().expect("bound");
        let last = self.proj.last();
        if last > 0 {
            exectuples::slot_getsomeattrs(slot, last as i32);
        }
        let base = slot.base();
        let (vals, nulls) = (&base.tts_values[..last as usize], &base.tts_isnull[..last as usize]);
        // bounded("key-columns: one cell per output column, at most MaxTupleAttributeNumber (1664) columns")
        for (k, &a) in self.recipe.attnos.iter().enumerate() {
            cells.put(k, vals[a as usize - 1].as_u64(), nulls[a as usize - 1]);
        }
        Ok(true)
    }

    fn next_substituted(&mut self, pos: ScanPos, values: &mut [Word], isnull: &mut [u8]) -> PgResult<Subst> {
        // D2048 (lockrows): the empty row position a NULL REFERENCE mark stores (offset 0): no row, as the heap source's
        // window_substituted, never a fetch at block 0 (an empty relation has none)
        if !pos.is_valid() || pos.off == 0 {
            return Ok(Subst::Cleared);
        }
        let mcx = self.scratch.exec_mcx();
        let heap = self.heap.as_ref().expect("bound");
        let slot = self.slot.as_mut().expect("bound");
        let tid = tid_of(pos);
        if !crate::ops::shared::heap_fetch::fetch_any(mcx, heap, &tid, slot)? {
            return Ok(Subst::Cleared);
        }
        // a key on an expression column reads the index's own value of this version (FormIndexDatum: the index expressions'
        // calls, as PostgreSQL's IndexRecheck makes them through indexqualorig on the same row)
        let mut ivals = [datum::Datum::from_u64(0); INDEX_COLS];
        let mut inull = [false; INDEX_COLS];
        if self.recipe.key_heap_attnos.iter().chain(self.recipe.rows.iter().flat_map(|r| r.heap_attnos.iter())).any(|&a| a == 0) {
            if self.index_info.is_none() {
                self.index_info = Some(execindexing::BuildIndexInfo(mcx, self.index.as_ref().expect("bound"))?);
            }
            let info = self.index_info.as_mut().expect("built");
            execindexing::FormIndexDatum(mcx, mcx, info, slot, &mut ivals, &mut inull)?;
        }
        for i in 0..self.recipe.nkeys {
            let search = self.keys[i].sk_flags & (SK_SEARCHNULL | SK_SEARCHNOTNULL);
            if search != 0 {
                // V4R-49: a null test key rechecks its column's nullness
                if column(slot, self.recipe.key_heap_attnos[i], self.keys[i].sk_attno, (&ivals, &inull)).1 != (search == SK_SEARCHNULL) {
                    return Ok(Subst::Cleared);
                }
                continue;
            }
            if self.keys[i].sk_flags & SK_SEARCHARRAY != 0 {
                // V4R-44: a search array rechecks as `column op ANY (array)` (PostgreSQL's IndexRecheck over
                // indexqualorig): some non-null element passes
                let key = &mut self.keys[i];
                let (d, null) = column(slot, self.recipe.key_heap_attnos[i], key.sk_attno, (&ivals, &inull));
                if null || key.sk_flags & SK_ISNULL != 0 || !any_element(mcx, key, d, &mut self.fcinfo)? {
                    return Ok(Subst::Cleared);
                }
                continue;
            }
            if let Some(r) = self.recipe.rows.iter().position(|r| r.key == i) {
                let strategy = self.recipe.keys[i].sk_strategy;
                if !row_passes(&mut self.rows[r], &self.recipe.rows[r].heap_attnos, strategy, &mut self.fcinfo, slot, (&ivals, &inull), mcx)? {
                    return Ok(Subst::Cleared);
                }
                continue;
            }
            let key = &mut self.keys[i];
            let (d, null) = column(slot, self.recipe.key_heap_attnos[i], key.sk_attno, (&ivals, &inull));
            if null {
                return Ok(Subst::Cleared);
            }
            self.fcinfo.rearm(key.sk_collation);
            // the exec context outlives this call; reset at unbind
            crate::ops::scan::raw::arm_result_mcx(&mut self.fcinfo, mcx);
            self.fcinfo.set_arg(0, d);
            self.fcinfo.set_arg(1, key.sk_argument);
            let r = key.sk_func.invoke(&mut self.fcinfo)?;
            if self.fcinfo.isnull || !r.as_bool() {
                return Ok(Subst::Cleared);
            }
        }
        if let Some(f) = self.recipe.filter.as_ref() {
            if !f(slot)? {
                return Ok(Subst::Filtered);
            }
        }
        crate::ops::shared::heap_fetch::project_by(slot, &self.recipe.attnos, self.proj, values, isnull)?;
        self.pos = ScanPos::row(pos.rel, pos.block, pos.off);
        Ok(Subst::Row)
    }
}

/// A substituted row at one open (`next_substituted`): handed on, dropped by the scan's own qual (kept for the next open),
/// or failing the open's keys or gone (cleared: no later open yields it).
enum Subst {
    Row,
    Filtered,
    Cleared,
}

/// A key column's value in a substituted row: the table column, or the index's formed value of an expression column.
fn column(slot: &mut SlotData<'static>, heap_attno: i16, index_attno: i16, (ivals, inull): (&[datum::Datum; INDEX_COLS], &[bool; INDEX_COLS])) -> (datum::Datum, bool) {
    if heap_attno > 0 {
        let mut null = false;
        let d = exectuples::slot_getattr(slot, heap_attno as i32, &mut null);
        return (d, null);
    }
    let at = index_attno as usize - 1;
    (ivals[at], inull[at])
}

/// A row comparison over a substituted row, as PostgreSQL's ExecEvalRowCompare: the members in order, a NULL reached is not a
/// match, the first unequal member decides by the comparison's strategy, all equal matches `<=` / `>=` only.
fn row_passes(members: &mut [ScanKeyData], heap_attnos: &[i16], strategy: StrategyNumber, fcinfo: &mut LocalFcinfo<2>, slot: &mut SlotData<'static>, formed: (&[datum::Datum; INDEX_COLS], &[bool; INDEX_COLS]), mcx: Mcx<'_>) -> PgResult<bool> {
    use types_scan::scankey::{BTGreaterEqualStrategyNumber, BTLessEqualStrategyNumber};
    // bounded("small-fixed: a row compare's members (at most INDEX_MAX_KEYS), one column compare each")
    for (m, &a) in members.iter_mut().zip(heap_attnos) {
        if m.sk_flags & SK_ISNULL != 0 {
            return Ok(false);
        }
        let (d, null) = column(slot, a, m.sk_attno, formed);
        if null {
            return Ok(false);
        }
        fcinfo.rearm(m.sk_collation);
        // the exec context outlives this call; reset at unbind
        crate::ops::scan::raw::arm_result_mcx(fcinfo, mcx);
        fcinfo.set_arg(0, d);
        fcinfo.set_arg(1, m.sk_argument);
        let c = m.sk_func.invoke(fcinfo)?;
        if fcinfo.isnull {
            return Ok(false);
        }
        let c = c.as_i32();
        if c != 0 {
            return Ok(if strategy <= BTLessEqualStrategyNumber { c < 0 } else { c > 0 });
        }
    }
    Ok(strategy == BTLessEqualStrategyNumber || strategy == BTGreaterEqualStrategyNumber)
}

impl Source for IndexSource {
    fn bind(&mut self, b: &BindingRef<'_>) -> Result<()> {
        // D4464: the scratch and the walk's per-row context on the binding statement
        if let Some((o, _)) = gravity_driver::stmt_context() {
            self.scratch.home(o);
            if let Some(w) = self.am.as_mut() {
                w.home(o);
            }
        }
        self.bind_pg(snapshot_of(b)).map_err(crate::stash)
    }

    fn seek(&mut self, keys: &[Word], keys_null: &[u8]) -> Result<()> {
        let n = self.recipe.nkeys;
        self.pos = ScanPos::NONE;
        if keys.len() < self.recipe.words.len() || keys_null.len() < self.recipe.words.len() {
            return Err(Error::host("an index seek with fewer key words than its recipe"));
        }
        // a NULL key argument matches nothing; a NULL row comparison member is nbtree's (SK_ISNULL: a first member makes
        // the key unsatisfiable, a later one decides only the rows the earlier members tie)
        if self.recipe.words.iter().zip(keys_null).any(|(w, &x)| x != 0 && matches!(w, KeyWord::Key(_))) {
            self.open = false;
            self.substituted = None;
            return Ok(());
        }
        let mut images_reset = false;
        // bounded("other: one key word per scan key, row member or ORDER BY key of the site, which the statement's quals bound")
        for (w, word) in self.recipe.words.iter().enumerate() {
            match *word {
                KeyWord::Key(i) if !self.recipe.host_keys[i as usize] => {
                    let k = &mut self.keys[i as usize];
                    k.sk_argument = datum::Datum::from_u64(keys[w]);
                    if k.sk_flags & SK_SEARCHARRAY != 0 && !plain_image(k.sk_argument) {
                        // V4R-44: a bound array whose image is compressed or external is expanded here (nbtree borrows
                        // a plain or short image only); it lives until the next seek that expands one (D2181)
                        let mcx = self.seek_images.region(!images_reset);
                        images_reset = true;
                        k.sk_argument = plain_array(mcx, k.sk_argument).map_err(crate::stash)?;
                    }
                }
                KeyWord::Key(_) => {}
                KeyWord::OrderBy(j) => {
                    let k = &mut self.am.as_mut().ok_or_else(|| Error::host("an ORDER BY word without its walk"))?.orderby[j as usize];
                    k.sk_flags = SK_ORDER_BY | if keys_null[w] != 0 { SK_ISNULL } else { 0 };
                    k.sk_argument = datum::Datum::from_u64(keys[w]);
                }
                KeyWord::Member(r, j) => {
                    let (t, m) = (&self.recipe.rows[r as usize].members[j as usize], &mut self.rows[r as usize][j as usize]);
                    let null = keys_null[w] != 0;
                    m.sk_flags = t.sk_flags | if null { SK_ISNULL } else { 0 };
                    m.sk_strategy = t.sk_strategy;
                    m.sk_argument = datum::Datum::from_u64(if null { 0 } else { keys[w] });
                }
            }
        }
        if self.recheck {
            self.open = self.substituted.is_some();
            return Ok(());
        }
        let scan = self.scan.as_mut().ok_or_else(|| Error::host("seek on an unbound source"))?;
        let keys = if n == 0 { None } else { Some(&self.keys[..n]) };
        let orderbys = self.am.as_mut().map(|w| {
            w.rewind();
            &w.orderby[..]
        });
        indexam::index_rescan(scan, keys, orderbys.filter(|o| !o.is_empty())).map_err(crate::stash)?;
        if let Some(b) = self.batch.as_mut() {
            b.rewind();
        }
        self.open = true;
        Ok(())
    }

    /// The probe row as a 1-row window (the driver's default `next()` over it is exact).
    fn window(&mut self, w: &mut Window<'_>) -> Result<u32> {
        if self.batch.is_some() && !self.recheck {
            return self.window_batch(w);
        }
        let natts = self.recipe.natts;
        let mut v = core::mem::take(&mut self.vbuf);
        let mut n = core::mem::take(&mut self.nbuf);
        let got = self.next(&mut v, &mut n);
        self.row.copy_from_slice(&v);
        self.vbuf = v;
        self.nbuf = n;
        if !got? {
            return Ok(0);
        }
        // bounded("key-columns: one lane per output column, at most MaxTupleAttributeNumber (1664) columns")
        for k in 0..natts {
            let null = self.nbuf[k] != 0;
            self.row_valid[k] = (!null) as u64;
            let valid = if null { &self.row_valid[k] as *const u64 } else { core::ptr::null() };
            w.set(k, Lane::byval(&self.row[k] as *const Word as *const u8, valid, 8, lane::REGION_ARENA, lane::id(lane::ID_ATTR, self.recipe.attnos[k] as u32)));
        }
        if let Some(b) = self.batch.as_mut() {
            b.one(self.pos, w);
        }
        w.rows = 1;
        w.set_base(WindowBase::at(self.pos));
        Ok(1)
    }

    fn cursor(&self) -> &Cursor {
        &self.cur
    }

    fn cursor_mut(&mut self) -> &mut Cursor {
        &mut self.cur
    }

    fn next(&mut self, values: &mut [Word], isnull: &mut [u8]) -> Result<bool> {
        let got = self.next_row(values, isnull);
        self.flush_fetches();
        got
    }

    fn position(&self) -> ScanPos {
        self.pos
    }

    fn lanes_hint(&self) -> usize {
        self.recipe.natts
    }

    fn refetch(&mut self, values: &mut [Word], isnull: &mut [u8]) -> Result<bool> {
        if !self.pos.is_valid() {
            return Ok(false);
        }
        let mcx = self.scratch.exec_mcx();
        let tid = tid_of(self.pos);
        let heap = self.heap.as_ref().ok_or_else(|| Error::host("refetch on an unbound source"))?;
        let snapshot = self.snapshot.as_ref().expect("bound");
        let slot = self.slot.as_mut().expect("bound");
        crate::ops::shared::heap_fetch::fetch_by_tid(mcx, heap, &tid, snapshot, slot, &self.recipe.attnos, values, isnull).map_err(crate::stash)
    }

    fn substitute(&mut self, pos: ScanPos) {
        self.substituted = Some(pos);
        self.recheck = true;
    }

    fn unbind(&mut self) {
        self.unbind_pg().expect("gravity: index source unbind");
    }

    /// The slot depot's hook (the depot's holder at a worker set's park, the taking statement at its take): the source's
    /// scratch (D4464), and a KNN walk's parked queue-arena charge and per-row context (`AmWalk::transfer_held`).
    fn transfer_held(&mut self, to: memledger::Owner, release: memledger::Release) {
        self.scratch.home(to);
        if let Some(a) = self.am.as_mut() {
            a.transfer_held(to, release);
        }
    }
}

impl IndexSource {
    /// D2900: the walk's uncounted heap fetches into the recipe's shared count, one atomic add a window.
    fn flush_fetches(&mut self) {
        if self.fetched != 0 {
            self.recipe.heap_fetches.fetch_add(core::mem::take(&mut self.fetched), Ordering::Relaxed);
        }
    }

    /// `next`'s row without the heap-fetch count's flush (an N-row window flushes once, `window_batch`).
    fn next_row(&mut self, values: &mut [Word], isnull: &mut [u8]) -> Result<bool> {
        if !self.open {
            return Ok(false);
        }
        if self.recheck {
            self.open = false;
            let Some(pos) = self.substituted else { return Ok(false) };
            return Ok(match self.next_substituted(pos, values, isnull).map_err(crate::stash)? {
                Subst::Row => true,
                Subst::Filtered => false,
                Subst::Cleared => {
                    self.substituted = None;
                    false
                }
            });
        }
        let mcx = self.scratch.exec_mcx();
        let dir = if self.recipe.backward { ScanDirection::BackwardScanDirection } else { ScanDirection::ForwardScanDirection };
        if self.recipe.only {
            let (scan, slot, heap) = (self.scan.as_mut().expect("bound"), self.slot.as_mut().expect("bound"), self.heap.as_ref().expect("bound"));
            let snapshot = self.snapshot.as_ref().expect("bound");
            // D3970 / D4046 (C9): a tick per row visited, as `walk`
            let mut step = cancelpoll::step("index_source walk: the index-only walk's next row");
            loop {
                if !crate::ops::scan::indexonly::next(&mut step, mcx, scan, heap, slot, snapshot, &mut self.vm, dir, &self.itup_attnums, &self.itup_names, &mut self.name_bufs, values, isnull, &mut self.fetched).map_err(crate::stash)? {
                    self.open = false;
                    return Ok(false);
                }
                step.tick()?;
                if let Some(w) = self.am.as_mut() {
                    if !w.only_passes(Quals { keys: &mut self.keys[..self.recipe.nkeys], heap: &self.recipe.key_heap_attnos, index: self.index.as_ref().expect("bound"), info: &mut self.index_info, cols: &self.image_cols }, mcx, scan, slot).map_err(crate::stash)? {
                        continue;
                    }
                }
                break;
            }
            self.pos = pos_of(self.recipe.heap, &scan.xs_heaptid);
            return Ok(true);
        }
        if !self.walk()? {
            return Ok(false);
        }
        crate::ops::shared::heap_fetch::project_by(self.slot.as_mut().expect("bound"), &self.recipe.attnos, self.proj, values, isnull).map_err(crate::stash)?;
        Ok(true)
    }
}

impl Drop for IndexSource {
    fn drop(&mut self) {
        let _ = self.unbind_pg();
        if let Some(scan) = self.scan.take() {
            let _ = indexam::index_endscan(scan);
        }
        self.slot = None;
    }
}

#[cfg(test)]
mod tests {
    use super::Prefilter;
    use crate::ops::scan::index_window::IOS_MAX_ROWS;

    /// The pre-check stays on while it has kept no more rows than it dropped plus one page-sized window, and goes off
    /// once a walk keeps more (the program still filters every row).
    #[test]
    fn the_prefilter_stays_on_while_it_drops_as_many_rows_as_it_keeps() {
        let mut p = Prefilter::default();
        for _ in 0..IOS_MAX_ROWS {
            assert!(p.check(Ok(true)).unwrap());
        }
        assert!(p.on(), "one window of kept rows");
        p.check(Ok(true)).unwrap();
        assert!(!p.on(), "past it, with nothing dropped");
        assert!(!p.check(Ok(false)).unwrap());
        assert!(p.on(), "a dropped row brings it back");
        let mut q = Prefilter::default();
        (0..10_000).for_each(|i| _ = q.check(Ok(i % 400 == 0)));
        assert!(q.on(), "a selective qual keeps it on");
    }

    /// An error from the row keeps it for the program and turns the pre-check off; an interrupt is raised.
    #[test]
    fn an_error_in_the_prefilter_keeps_the_row_and_turns_it_off() {
        let mut p = Prefilter::default();
        let div = types_error::PgError::new(types_error::ERROR, "division by zero").with_sqlstate(types_error::ERRCODE_DIVISION_BY_ZERO);
        assert!(p.check(Err(Box::new(div))).unwrap(), "kept for the program"); // alloc: Cold
        assert!(!p.on());
        let mut l = Prefilter::default();
        let long = types_error::PgError::new(types_error::ERROR, "requested length too large").with_sqlstate(types_error::ERRCODE_PROGRAM_LIMIT_EXCEEDED);
        assert!(l.check(Err(Box::new(long))).unwrap(), "a limit error from the row is kept too"); // alloc: Cold
        assert!(!l.on());
        let mut q = Prefilter::default();
        let cancel = types_error::PgError::new(types_error::ERROR, "canceling statement due to user request").with_sqlstate(types_error::ERRCODE_QUERY_CANCELED);
        assert!(q.check(Err(Box::new(cancel))).is_err(), "a cancel is raised"); // alloc: Cold
        assert!(q.on());
    }
}
