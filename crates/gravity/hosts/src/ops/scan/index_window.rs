//! M2's index hosts (cards/M2.md L3 `m2_index.rs`, M2-inventory rows 34 / 36): the index-only scan's N-row window. v1 read
//! index-only rows in batches (`execmain/src/gravity/sources.rs` `IosBatch` :50-60, `batch_next` :479-490); M1's index
//! form is one row per window, its position the window's. A site whose descriptor keeps a RowId lane (`index_only.rs`'s)
//! gets `IosBatch` windows: every row from `indexonly::next` (the heap fetched only where the visibility map does not
//! vouch), the RowId lane holding each row's heap tid (`tid_word`).
//!
//! The batch size is derived, not tuned: a rescan's first window holds one row and each next window doubles, up to the
//! rows one index page's line pointers can address (`MaxIndexTuplesPerPage`, from BLCKSZ). A LIMIT that stops after the
//! first rows pays for no read-ahead (a heap fetch on a page the VM does not vouch for is real work), a long scan reaches
//! the page-sized window after log2(408) = 9 windows (read_stream's distance rule, C `read_stream_look_ahead`).
//! Once a LIMIT's budget is spent and its consumer still asks (a filter above dropped rows), nodeLimit.c's ExecLimit
//! pulls one row at a time and stops at the first that passes: doubling again would read up to twice the rows it reads,
//! heap fetches included (MinMaxAgg's `LIMIT 1` over a filtered backward index scan). From there each window grows by
//! the Limit's count (one row at least) instead, so the rows read past PostgreSQL's stop stay within one window of
//! about sqrt(2 x rows x count), and a long filtered walk still reaches the page-sized window.
//! By-reference cells are copied into the window's own bytes: an index tuple's image lives only until the AM's next page.
//!
//! The any-AM walk (`AmRecipe` / `AmWalk`, V4R-49, M2-INDEX): a hash / GiST / SP-GiST walk through `IndexSource`, its lossy
//! rows rechecked and its KNN rows reordered as nodeIndexscan.c / nodeIndexonlyscan.c do.
//! Created by M2 (owner M2-HOSTS).
use datum::Datum;
use gravity_abi::ctx::lane;
use gravity_abi::{Datum as Word, Lane, ScanPos};
use gravity_driver::mem::root::Root;
use gravity_driver::{Window, WindowBase};
use gravity_kernels::ops::scan::entries::tid_word;
use gravity_program::LaneId;
use heaptuple::HeapTuple;
use mcx::Mcx;
use pairingheap::PairingHeap;
use tuplesort::{SortSupport, SortSupportInit};
use types_core::Oid;
use types_error::PgResult;
use types_fmgr::LocalFcinfo;
use types_nodes::NodeList;
use types_nodes::node_tree::Node;
use types_nodes::primnodes::INDEX_VAR;
use types_rel::Relation;
use types_relscan::IndexScanDescData;
use types_scan::scankey::{ScanKeyData, StrategyNumber, SK_ISNULL, SK_ORDER_BY, SK_SEARCHNOTNULL, SK_SEARCHNULL};
use types_scan::sdir::ScanDirection;
use types_slot::SlotData;
use types_storage::bufpage::MaxIndexTuplesPerPage;

use crate::ops::scan::heap_source::HeldCharge;
use crate::ops::scan::index_source::{KeyWord, INDEX_COLS};

/// The largest window: the rows one index page can address.
pub const IOS_MAX_ROWS: usize = MaxIndexTuplesPerPage;
const WORDS: usize = IOS_MAX_ROWS.div_ceil(64);

/// D3887: the error a row walk's loop returns when it stopped for its statement's cancel (`crate::stash` reports it as
/// the kernel's cancel).
const WALK_CANCELED: &str = "gravity: an index row walk stopped for its statement's cancel";

pub use crate::ops::shared::index_bound::{limit_budget, run_bound, LimitWord, RunBound};

mod raw;
pub(crate) use raw::copy_cell;

/// An index-only source's N-row window state.
pub struct IosBatch {
    sizes: Sizes,
    /// D588 / D590: the recipe's run cell (`SiteRecipe::set_run`): a value entry stored (a parameter Limit's rows at the open,
    /// a FETCH's count at each step) becomes the rows left at the next window, taken once.
    run: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
    /// Per lane: the column's `attlen` when it is by reference (-1 varlena, -2 cstring, > 0 fixed), 0 by value.
    byref: Vec<i16>,
    lanes: Vec<u64>,
    valid: Vec<u64>,
    /// Per lane: whether a cell of this window is NULL (its `valid` words are then written at the window's end).
    has_null: Vec<bool>,
    tids: Vec<u64>,
    bytes: Vec<u8>,
    vals: Vec<Word>,
    nulls: Vec<u8>,
    /// The site's attribute lanes' table columns (lane `k` is column `attnos[k]`).
    attnos: Vec<i16>,
}

/// A walk's window sizes since its seek (the rule in the module header).
struct Sizes {
    /// This window's row budget: 1 after a seek, doubled per window to `IOS_MAX_ROWS` (grown by `step` once a budget is
    /// spent).
    cap: usize,
    /// The recipe's read-ahead bound (`SiteRecipe::row_budget`, D583) and the rows it leaves this walk since the seek: a
    /// window holds at most those, so a walk under a LIMIT reads no row past the Limit's count; spent, windows grow again
    /// from one row. `Some(0)`: every window holds one row (a single-match nest loop's inner, `limit_budget`).
    budget: Option<u64>,
    left: Option<u64>,
    /// The count the rows left were last set from (the budget at the seek, a run cell's value when taken).
    count: u64,
    /// 0 while the count is unspent; once spent, the rows each next window adds (the count, one at least).
    step: usize,
}

impl Sizes {
    fn new(budget: Option<u64>) -> Sizes {
        Sizes { cap: 1, budget, left: budget, count: budget.unwrap_or(0), step: 0 }
    }

    /// A run cell's value: the rows left from the next window on, windows doubling again until it is spent.
    fn set_left(&mut self, rows: u64) {
        (self.left, self.count, self.step) = (Some(rows), rows, 0);
    }

    /// The next window's row cap.
    fn cap(&self) -> usize {
        match (self.budget, self.left.filter(|&l| l > 0)) {
            (Some(0), _) => 1,
            (_, Some(l)) => self.cap.min(l as usize),
            _ => self.cap,
        }
    }

    /// After a window of `rows` (> 0) rows.
    fn after(&mut self, rows: usize) {
        self.cap = if self.step > 0 { self.cap + self.step } else { self.cap * 2 }.min(IOS_MAX_ROWS);
        if let Some(l) = self.left.filter(|&l| l > 0) {
            // the budget spent, the consumer asks on (a filter above dropped rows): windows grow again from one row, by
            // the count per window
            self.left = Some(l.saturating_sub(rows as u64));
            if l <= rows as u64 {
                self.cap = 1;
                self.step = (self.count.max(1) as usize).min(IOS_MAX_ROWS);
            }
        }
    }
}

impl IosBatch {
    /// The batch over `attnos` of `heap` (by-reference columns named from the heap descriptor).
    pub fn new(heap: &Relation<'_>, attnos: &[i16], budget: Option<u64>, run: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>) -> IosBatch {
        IosBatch::with_byref(byref_of(heap, attnos), attnos, budget, run)
    }

    /// `new` with the lanes' by-reference lengths (`byref_of`) taken on another thread (a pool worker has no relcache).
    pub fn with_byref(byref: Vec<i16>, attnos: &[i16], budget: Option<u64>, run: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>) -> IosBatch {
        let n = attnos.len();
        IosBatch { sizes: Sizes::new(budget), run, byref, lanes: vec![0; n * IOS_MAX_ROWS], valid: vec![0; n * WORDS], has_null: vec![false; n], tids: vec![0; IOS_MAX_ROWS], bytes: Vec::new(), vals: vec![0; n], nulls: vec![0; n], attnos: attnos.to_vec() } // alloc: Node
    }

    /// A seek: the next window starts at one row again.
    pub fn rewind(&mut self) {
        self.sizes = Sizes::new(self.sizes.budget);
    }

    /// A one-row window's RowId lane (an EvalPlanQual row, `IndexSource::window`'s single-row path): the row at `pos`.
    pub fn one(&mut self, pos: ScanPos, w: &mut Window<'_>) {
        self.tids[0] = tid_word(pos.block, pos.off);
        w.set(self.attnos.len(), Lane::byval(self.tids.as_ptr() as *const u8, core::ptr::null(), 8, lane::REGION_ARENA, LaneId::RowId.encode()));
    }

    /// The next window: up to `cap` rows, the attribute lanes, the RowId lane (lane `natts`) and the base at the first row.
    /// `next` is a heap walk's row (`IndexSource::next_cells`, its filter applied): it puts the row's cells into the window
    /// (`Cells::put`) and returns its heap position, `None` at the end (no cell put). Returns `(rows, the last row's
    /// position, ended)`: `ended` once the walk reported its end (never asked again: a btree asked past its end starts
    /// over, v1 `IosBatch.eof`).
    pub fn fill<E>(&mut self, mut next: impl FnMut(&mut Cells<'_>) -> Result<Option<ScanPos>, E>, heap_oid: u32, w: &mut Window<'_>) -> Result<(u32, ScanPos, bool), E> {
        let cap = self.start();
        let (mut rows, mut last, mut ended) = (0usize, ScanPos::NONE, false);
        // bounded("small-fixed: at most the window's row cap (IOS_MAX_ROWS: MaxIndexTuplesPerPage, 408 at 8 K pages), one row of the caller's walk each")
        while rows < cap {
            let mut cells = Cells { byref: &self.byref, lanes: &mut self.lanes, valid: &mut self.valid, has_null: &mut self.has_null, bytes: &mut self.bytes, row: rows };
            let Some(pos) = next(&mut cells)? else {
                ended = true;
                break;
            };
            self.tids[rows] = tid_word(pos.block, pos.off);
            last = ScanPos::row(heap_oid, pos.block, pos.off);
            rows += 1;
        }
        Ok(self.finish(rows, last, ended, heap_oid, w))
    }

    /// `fill` for a walk that hands each row over as arrays (`IndexSource::next`: an index-only row formed from the index
    /// tuple, a row with a system cell): `next` writes lane `k`'s word and null flag into `vals[k]` / `nulls[k]`, copied
    /// into the window before the next row (a by-reference image lives only until then).
    pub fn fill_rows<E>(&mut self, mut next: impl FnMut(&mut [Word], &mut [u8]) -> Result<Option<ScanPos>, E>, heap_oid: u32, w: &mut Window<'_>) -> Result<(u32, ScanPos, bool), E> {
        let cap = self.start();
        let (mut rows, mut last, mut ended) = (0usize, ScanPos::NONE, false);
        // bounded("small-fixed: at most the window's row cap (IOS_MAX_ROWS: MaxIndexTuplesPerPage, 408 at 8 K pages), one row of the caller's walk each")
        while rows < cap {
            let Some(pos) = next(&mut self.vals, &mut self.nulls)? else {
                ended = true;
                break;
            };
            self.tids[rows] = tid_word(pos.block, pos.off);
            last = ScanPos::row(heap_oid, pos.block, pos.off);
            let mut cells = Cells { byref: &self.byref, lanes: &mut self.lanes, valid: &mut self.valid, has_null: &mut self.has_null, bytes: &mut self.bytes, row: rows };
            // bounded("key-columns: one cell per output column of a row, at most MaxTupleAttributeNumber (1664) columns")
            for (k, (&v, &null)) in self.vals.iter().zip(&self.nulls).enumerate() {
                cells.put(k, v, null != 0);
            }
            rows += 1;
        }
        Ok(self.finish(rows, last, ended, heap_oid, w))
    }

    /// A window's start: its bytes and NULL flags cleared, a run cell's value taken; the window's row cap.
    fn start(&mut self) -> usize {
        self.bytes.clear();
        self.has_null.fill(false);
        if let Some(v) = self.run.as_ref().map(|c| c.swap(crate::ops::scan::index_source::RUN_NONE, std::sync::atomic::Ordering::Relaxed)).filter(|&v| v != crate::ops::scan::index_source::RUN_NONE) {
            self.sizes.set_left(v);
        }
        self.sizes.cap()
    }

    /// A window of `rows` rows filled: its copied images' offsets made pointers, its lanes, RowId lane and base set.
    fn finish(&mut self, rows: usize, last: ScanPos, ended: bool, heap_oid: u32, w: &mut Window<'_>) -> (u32, ScanPos, bool) {
        let natts = self.attnos.len();
        // the copied images: offsets into `bytes` until the window is complete (it may grow), then pointers
        let base = self.bytes.as_ptr() as u64;
        // bounded("key-columns: one by-reference lane per output column, over at most IOS_MAX_ROWS rows, at most MaxTupleAttributeNumber (1664) columns")
        for k in (0..natts).filter(|&k| self.byref[k] != 0) {
            let lane = &mut self.lanes[k * IOS_MAX_ROWS..k * IOS_MAX_ROWS + rows];
            if self.has_null[k] {
                let words = &self.valid[k * WORDS..(k + 1) * WORDS];
                // bounded("small-fixed: one window's rows, at most IOS_MAX_ROWS (MaxIndexTuplesPerPage, 408 at 8 K pages), an add each")
                for (r, cell) in lane.iter_mut().enumerate().filter(|(r, _)| words[r / 64] >> (r % 64) & 1 != 0) {
                    *cell += base;
                }
            } else {
                lane.iter_mut().for_each(|cell| *cell += base);
            }
        }
        if rows == 0 {
            return (0, last, ended);
        }
        self.sizes.after(rows);
        // bounded("key-columns: one lane per output column, at most MaxTupleAttributeNumber (1664) columns")
        for (k, &a) in self.attnos.iter().enumerate() {
            let valid = if self.has_null[k] { self.valid[k * WORDS..].as_ptr() } else { core::ptr::null() };
            let mut l = Lane::byval(self.lanes[k * IOS_MAX_ROWS..].as_ptr() as *const u8, valid, 8, lane::REGION_ARENA, lane::id(lane::ID_ATTR, a as u32));
            if self.byref[k] != 0 {
                l.kind = lane::KIND_BYREF;
            }
            w.set(k, l);
        }
        w.set(natts, Lane::byval(self.tids.as_ptr() as *const u8, core::ptr::null(), 8, lane::REGION_ARENA, LaneId::RowId.encode()));
        w.rows = rows as u32;
        let (b0, o0) = gravity_kernels::ops::scan::entries::tid_of_word(self.tids[0]);
        w.set_base(WindowBase::at(ScanPos::row(heap_oid, b0, o0)));
        (rows as u32, last, ended)
    }
}

/// Per lane of `attnos` (columns of `heap`): the column's `attlen` when it is by reference, 0 by value (`IosBatch`).
pub fn byref_of(heap: &Relation<'_>, attnos: &[i16]) -> Vec<i16> {
    attnos.iter().map(|&a| heap.rd_att.compact_attrs.get(a as usize - 1).map_or(0, |c| if c.attbyval { 0 } else { c.attlen })).collect() // alloc: Node
}

/// One row of an `IosBatch` window as the source's walk writes it (`IosBatch::fill`'s `next`): lane `k`'s cell by `put`.
pub struct Cells<'a> {
    byref: &'a [i16],
    lanes: &'a mut [u64],
    valid: &'a mut [u64],
    has_null: &'a mut [bool],
    bytes: &'a mut Vec<u8>,
    row: usize,
}

impl Cells<'_> {
    /// Lane `k`'s cell of this row: its word, or NULL (the word is never read: its valid bit is clear). A lane's valid
    /// words are written only once the window holds a NULL cell of it, every earlier row then valid; a lane with none
    /// carries no valid words. A by-reference cell's word points at its image, live until the walk's next row (inside the
    /// index tuple, the name buffer or the slot's heap tuple): the image is copied into the window's own bytes.
    #[inline]
    pub fn put(&mut self, k: usize, v: Word, null: bool) {
        let r = self.row;
        if null {
            if !self.has_null[k] {
                self.has_null[k] = true;
                let words = &mut self.valid[k * WORDS..(k + 1) * WORDS];
                words.fill(0);
                words[..r / 64].fill(u64::MAX);
                if r % 64 != 0 {
                    words[r / 64] = (1u64 << (r % 64)) - 1;
                }
            }
            return;
        }
        if self.has_null[k] {
            self.valid[k * WORDS + r / 64] |= 1 << (r % 64);
        }
        let len = self.byref[k];
        // a non-null by-reference cell points at its live image (the walk's contract above)
        self.lanes[k * IOS_MAX_ROWS + r] = if len == 0 { v } else { raw::copy_cell(self.bytes, v as *const u8, len) };
    }
}

/// An any-AM walk's plan half (`SiteRecipe::am`, V4R-49): the ORDER BY keys (SK_ORDER_BY, bound from `KeyWord::OrderBy`
/// words), their table columns, sort support and result types (ExecInitIndexScan). A lossy row is rechecked by the scan
/// keys' operators (IndexRecheck), a lossy distance recomputed by its key's operator on the table column (an expression
/// column's value formed by the index's own expression, FormIndexDatum): the planner never commutes an ORDER BY operator,
/// so that is indexorderbyorig's own call.
#[derive(Clone, Default)]
pub struct AmRecipe {
    pub orderby: Vec<ScanKeyData>,
    pub orderby_heap: Vec<i16>,
    pub ssup: Vec<SortSupport>,
    /// Each distance's (typlen, typbyval): a queued row's distances are copied (datumCopy).
    pub typs: Vec<(i16, bool)>,
}

impl AmRecipe {
    /// The keys of `indexorderby`; `ops[i]` = clause `i`'s indexorderbyops operator, result type and collation (none for
    /// an index-only walk, which never reorders); each key's word appended to `words`.
    pub fn new(index: &Relation<'_>, indexorderby: &NodeList<'_>, ops: &[(Oid, Oid, Oid)], words: &mut Vec<KeyWord>) -> PgResult<AmRecipe> {
        let mut r = AmRecipe::default();
        // bounded("other: one key built per clause of the plan's index ORDER BY, at plan time; the statement's ORDER BY bounds them")
        for (j, clause) in indexorderby.iter().enumerate() {
            let op = clause.as_op_expr().filter(|o| o.args.len() == 2).ok_or_else(|| crate::pg_err("gravity: an ORDER BY key that is not an operator"))?;
            let var = strip_relabel(op.args.nth(0)).as_var().filter(|v| v.varno == INDEX_VAR && v.varattno >= 1 && v.varattno as i32 <= index.indnkeyatts()).ok_or_else(|| crate::pg_err("gravity: ORDER BY key side"))?;
            // 0: an expression column (its value formed by the index's expression)
            let heap = index.rd_index.as_ref().and_then(|f| f.indkey.get(var.varattno as usize - 1).copied()).filter(|&a| a >= 0).ok_or_else(|| crate::pg_err("gravity: an ORDER BY key on a system column"))?;
            let (strategy, _, right) = gravity_fn::typesem::op_opfamily_properties(op.opno, index.rd_opfamily[var.varattno as usize - 1], true)?;
            let mut k = ScanKeyData::empty();
            (k.sk_flags, k.sk_attno, k.sk_strategy, k.sk_subtype, k.sk_collation) = (SK_ORDER_BY, var.varattno, strategy as StrategyNumber, right, op.inputcollid);
            fmgr_core::fmgr_info_into(op.opfuncid, &mut k.sk_func)?;
            r.orderby.push(k); // alloc: Program
            r.orderby_heap.push(heap); // alloc: Program
            if let Some(&(sortop, typ, coll)) = ops.get(j) {
                r.ssup.push(tuplesort::prepare_sort_support_from_ordering_op(sortop, &SortSupportInit { ssup_collation: coll, ssup_nulls_first: false, ssup_attno: 0 })?); // alloc: Program
                r.typs.push(gravity_fn::typesem::storage::len_byval(typ)?); // alloc: Program
            }
            words.push(KeyWord::OrderBy(j as u16)); // alloc: Program
        }
        Ok(r)
    }
}

/// The scan keys a lossy row is rechecked by (`IndexSource`'s bound keys), each key's table column (0: an expression
/// column), the index, and its IndexInfo for FormIndexDatum (`IndexSource::index_info`, built at the first formed row).
pub struct Quals<'a> {
    pub keys: &'a mut [ScanKeyData],
    pub heap: &'a [i16],
    pub index: &'a Relation<'static>,
    pub info: &'a mut Option<execindexing::IndexInfo<'static>>,
    /// D3453: an index-only walk's image column per index column (indexonly.rs `image_cols`); empty: each its own.
    pub cols: &'a [i16],
}

/// Index column `attno`'s expression (varno 1, RelationGetIndexExpressions), `None` for a table column: exec's
/// admission compares it with the plan's original clause (m2_index_admit.rs).
pub fn index_expression<'m>(mcx: Mcx<'m>, index: &Relation<'_>, attno: i16) -> PgResult<Option<Node<'m>>> {
    let Some(ind) = index.rd_index.as_ref().filter(|_| attno >= 1 && attno as i32 <= index.indnkeyatts()) else { return Ok(None) };
    if ind.indkey.get(attno as usize - 1).copied() != Some(0) {
        return Ok(None);
    }
    let k = ind.indkey[..attno as usize - 1].iter().filter(|&&a| a == 0).count();
    let exprs = execindexing::RelationGetIndexExpressions(mcx, index)?;
    Ok((k < exprs.len()).then(|| exprs.nth(k)))
}

/// nodeIndexscan.c ReorderTuple: a heap copy and its distances, both in the walk's context (reorderqueue_push), freed at
/// the pop.
struct Queued {
    htup: HeapTuple<'static>,
    vals: mcx::PgVec<'static, Datum>,
    nulls: mcx::PgVec<'static, bool>,
}

type QueueCmp = Box<dyn Fn(&Queued, &Queued) -> i32>;

/// D4213: `indexam::index_getnext_slot` with a tick of the walk's `step` before each TID past the first. A run of TIDs whose
/// table rows are dead or invisible to the walk's snapshot is that many table fetches before the next row (each a buffer
/// read, and a dirty victim's write and WAL flush under a small shared_buffers), with no poll between them: PostgreSQL's
/// loop checks for interrupts only as the index walk steps to its next page.
pub(crate) fn getnext_slot_polled<'mcx>(step: &mut cancelpoll::Step, mcx: Mcx<'mcx>, scan: &mut IndexScanDescData<'mcx>, dir: ScanDirection, slot: &mut SlotData<'mcx>) -> PgResult<bool> {
    loop {
        if !scan.xs_heap_continue && indexam::index_getnext_tid(scan, dir)?.is_none() {
            return Ok(false);
        }
        if indexam::index_fetch_heap(mcx, scan, slot)? {
            return Ok(true);
        }
        step.tick().map_err(|_| crate::pg_err(WALK_CANCELED))?;
    }
}

/// An any-AM walk's run state: the bound ORDER BY keys and IndexNextWithReorder's queue, kept across windows (a stopped
/// walk resumes from it) until the next seek or unbind.
pub struct AmWalk {
    /// CONVERT wave 2 (alloclint round 3 addendum): the queue's arena (`PairingHeap::heap_bytes`, a slot and a free-list
    /// id a queued row at its high water; the rows' copies are in the walk's mcx), charged to the statement from the
    /// binding's first queued row (`next` has no window) and parked on the session at each `rewind` (a seek or the
    /// unbind: the arena keeps its capacity in the parked Node). Declared first: it drops before the arena.
    held: HeldCharge,
    /// The arena bytes `held` was last synced to.
    synced: usize,
    pub orderby: Vec<ScanKeyData>,
    queue: PairingHeap<Queued, QueueCmp>,
    reached_end: bool,
    vals: Vec<Datum>,
    nulls: Vec<bool>,
    fcinfo: LocalFcinfo<2>,
    /// A row's formed index values (FormIndexDatum) for keys on expression columns, in `row`, reset per row.
    ivals: [Datum; INDEX_COLS],
    inull: [bool; INDEX_COLS],
    row: Root,
    /// D4464: the owner `row` was last homed on (`home`), as a host's scratch's (`NodeScratch::home`).
    row_at: Option<memledger::Owner>,
}

/// nodeIndexscan.c cmp_orderbyvals: NULLS LAST, both null a tie of the whole row.
fn cmp(a: (&[Datum], &[bool]), b: (&[Datum], &[bool]), ssup: &[SortSupport]) -> i32 {
    // bounded("other: one distance compare per ORDER BY key of the walk, which the statement's ORDER BY bounds")
    for (i, s) in ssup.iter().enumerate() {
        match (a.1[i], b.1[i]) {
            (true, false) => return 1,
            (false, true) => return -1,
            (true, true) => return 0,
            _ => match tuplesort::apply_cmp(s.comparator, a.0[i], b.0[i]) {
                0 => {}
                c => return c,
            },
        }
    }
    0
}

/// Key `k`'s operator on `d` and its argument; `None` when it returns NULL.
fn call(fc: &mut LocalFcinfo<2>, k: &mut ScanKeyData, d: Datum, mcx: Mcx<'_>) -> PgResult<Option<Datum>> {
    fc.rearm(k.sk_collation);
    // the exec context outlives this call; it resets at unbind
    crate::ops::scan::raw::arm_result_mcx(fc, mcx);
    fc.set_arg(0, d);
    fc.set_arg(1, k.sk_argument);
    let r = k.sk_func.invoke(fc)?;
    Ok((!fc.isnull).then_some(r))
}

/// IndexRecheck over the scan keys: a null test on its column's nullness, an operator key on its column (`col(i)`).
fn passes(fc: &mut LocalFcinfo<2>, keys: &mut [ScanKeyData], mut col: impl FnMut(usize, i16) -> (Datum, bool), mcx: Mcx<'_>) -> PgResult<bool> {
    // bounded("other: one recheck per scan key of the walk, which the statement's index quals bound")
    for (i, k) in keys.iter_mut().enumerate() {
        let (d, null) = col(i, k.sk_attno);
        let search = k.sk_flags & (SK_SEARCHNULL | SK_SEARCHNOTNULL);
        if search != 0 {
            if null != (search == SK_SEARCHNULL) {
                return Ok(false);
            }
        } else if null || k.sk_flags & SK_ISNULL != 0 || !call(fc, k, d, mcx)?.is_some_and(|r| r.as_bool()) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// A key's column: table column `heap`, or (0) index column `attno`'s formed value.
fn key_col(slot: &mut SlotData<'static>, heap: i16, attno: i16, formed: (&[Datum; INDEX_COLS], &[bool; INDEX_COLS])) -> (Datum, bool) {
    if heap == 0 {
        return (formed.0[attno as usize - 1], formed.1[attno as usize - 1]);
    }
    let mut null = false;
    (exectuples::slot_getattr(slot, heap as i32, &mut null), null)
}

/// FormIndexDatum for the row in `slot` when some key in `heap` is on an expression column (IndexRecheck evaluates the
/// same expressions through indexqualorig), into `ivals` / `inull` allocated in `row` (reset first).
#[allow(clippy::too_many_arguments)]
fn form<'m>(heap: &[i16], index: &Relation<'static>, info: &mut Option<execindexing::IndexInfo<'m>>, slot: &mut SlotData<'m>, row: &mut Root, ivals: &mut [Datum; INDEX_COLS], inull: &mut [bool; INDEX_COLS], mcx: Mcx<'m>) -> PgResult<()> {
    if !heap.contains(&0) {
        return Ok(());
    }
    if info.is_none() {
        *info = Some(execindexing::BuildIndexInfo(mcx, index)?);
    }
    row.reset();
    execindexing::FormIndexDatum(mcx, row.mcx(), info.as_mut().expect("built"), slot, ivals, inull)
}

/// An index-only row's table columns as a virtual row in `slot`: each plain index column's value (`att`) in its table
/// column, the others null.
fn table_row<'m>(slot: &mut SlotData<'m>, index: &Relation<'static>, att: impl Fn(i16) -> (Datum, bool), mcx: Mcx<'m>) {
    exectuples::exec_clear_tuple(slot, mcx);
    let base = slot.base_mut();
    base.tts_isnull.iter_mut().for_each(|n| *n = true);
    // bounded("key-columns: one value per index column, at most INDEX_MAX_KEYS (32)")
    for (j, &h) in index.rd_index.as_ref().map_or(&[][..], |f| &f.indkey[..]).iter().enumerate() {
        if h > 0 && (h as usize) <= base.tts_values.len() && base.tts_isnull[h as usize - 1] {
            (base.tts_values[h as usize - 1], base.tts_isnull[h as usize - 1]) = att(j as i16 + 1);
        }
    }
    exectuples::exec_store_virtual_tuple(slot);
}

impl AmWalk {
    /// memsig E-pub t3: the queue's comparison keeps its copy of the ORDER BY sort supports in `scratch`'s node context:
    /// the walking host's, which outlives the walk (declared after it).
    pub fn new(am: &AmRecipe, scratch: &crate::scratch::NodeScratch) -> AmWalk {
        let mut ssup = mcx::PgVec::new_in(scratch.mcx());
        mcx::GrowIn::extend_from_slice_in(&mut ssup, &am.ssup);
        // reorderqueue_cmp: the pairing heap surfaces its greatest element, KNN wants the nearest
        let qcmp: QueueCmp = Box::new(move |a, b| cmp((&b.vals, &b.nulls), (&a.vals, &a.nulls), &ssup)); // alloc: Node
        let n = am.orderby.len();
        // D2944: reset wholesale per row (FormIndexDatum's values live until the next row's reset, a per-tuple context)
        let row = Root::wholesale("gravity index recheck row"); // alloc: Node
        AmWalk { held: HeldCharge::new(), synced: 0, orderby: am.orderby.clone(), queue: PairingHeap::new(qcmp), reached_end: false, vals: vec![Datum::null(); n], nulls: vec![true; n], fcinfo: LocalFcinfo::<2>::new(0), ivals: [Datum::null(); INDEX_COLS], inull: [true; INDEX_COLS], row, row_at: None } // alloc: Node
    }

    /// The slot depot's hook for its `IndexSource` (`Source::transfer_held`): the parked queue arena's charge and the
    /// per-row context (D4464) to `to`.
    pub fn transfer_held(&mut self, to: memledger::Owner, release: memledger::Release) {
        self.held.transfer(to, release);
        self.home(to);
    }

    /// D4464: the per-row context on `to` from now (`NodeScratch::home`: the binding statement's from the source's bind,
    /// the depot's holder and the taking statement at a worker set's park and take). Nothing when it is already `to`'s.
    pub fn home(&mut self, to: memledger::Owner) {
        if self.row_at != Some(to) {
            self.row.set_owner_tree(to);
            self.row_at = Some(to);
        }
    }

    /// A seek or unbind (ExecReScanIndexScan): the queue's copies dropped before the exec context resets.
    pub fn rewind(&mut self) {
        // every queued copy is freed at once (`reset` drops them with no pop, so no compare of the rows' distances); a long
        // release reads under this label
        let _step = cancelpoll::step("index_window rewind: the reorder queue's copies released");
        self.queue.reset();
        self.synced = self.queue.heap_bytes();
        self.held.park(self.synced as u64);
        self.reached_end = false;
    }

    /// The next row into `slot` in the scan's order (IndexNextWithReorder when the walk has ORDER BY keys). `step` is the
    /// caller's row walk's: a row the reorder queue takes and a row a lossy recheck drops tick it (IndexNextWithReorder
    /// checks for interrupts per pass).
    #[allow(clippy::too_many_arguments)]
    pub fn next(&mut self, step: &mut cancelpoll::Step, am: &AmRecipe, q: Quals<'_>, mcx: Mcx<'static>, scan: &mut IndexScanDescData<'static>, slot: &mut SlotData<'static>, dir: ScanDirection) -> PgResult<bool> {
        if self.orderby.is_empty() {
            return self.fetch(step, q, mcx, scan, slot, dir);
        }
        loop {
            step.tick().map_err(|_| crate::pg_err(WALK_CANCELED))?;
            if let Some(top) = self.queue.first() {
                if self.reached_end || cmp((&top.vals, &top.nulls), (&scan.xs_orderbyvals, &scan.xs_orderbynulls), &am.ssup) <= 0 {
                    let top = self.queue.remove_first().expect("a non-empty queue"); // alloc: Node charged self.held
                    self.track();
                    exectuples::exec_force_store_heap_tuple_owned(top.htup, slot, mcx)?;
                    return Ok(true);
                }
            } else if self.reached_end {
                return Ok(false);
            }
            if !self.fetch(step, Quals { keys: &mut *q.keys, heap: q.heap, index: q.index, info: &mut *q.info, cols: q.cols }, mcx, scan, slot, dir)? {
                self.reached_end = true;
                continue;
            }
            let exact = if scan.xs_recheckorderby {
                // EvalOrderByExpressions
                form(&am.orderby_heap, q.index, &mut *q.info, slot, &mut self.row, &mut self.ivals, &mut self.inull, mcx)?;
                // bounded("other: one distance per ORDER BY key of the walk, which the statement's ORDER BY bounds")
                for (i, k) in self.orderby.iter_mut().enumerate() {
                    let (d, null) = key_col(slot, am.orderby_heap[i], k.sk_attno, (&self.ivals, &self.inull));
                    let v = if null || k.sk_flags & SK_ISNULL != 0 { None } else { call(&mut self.fcinfo, k, d, mcx)? };
                    (self.vals[i], self.nulls[i]) = (v.unwrap_or(Datum::null()), v.is_none());
                }
                let c = cmp((&self.vals, &self.nulls), (&scan.xs_orderbyvals, &scan.xs_orderbynulls), &am.ssup);
                if c < 0 {
                    return Err(crate::pg_err("index returned tuples in wrong order"));
                }
                c == 0
            } else {
                self.vals.copy_from_slice(&scan.xs_orderbyvals);
                self.nulls.copy_from_slice(&scan.xs_orderbynulls);
                true
            };
            if exact && self.queue.first().is_none_or(|t| cmp((&self.vals, &self.nulls), (&t.vals, &t.nulls), &am.ssup) <= 0) {
                return Ok(true);
            }
            let htup = exectuples::exec_copy_slot_heap_tuple(slot, mcx, mcx)?;
            let (mut vals, nulls) = (mcx::slice_in(mcx, &self.vals)?, mcx::slice_in(mcx, &self.nulls)?);
            // bounded("other: one distance per ORDER BY key of the walk, which the statement's ORDER BY bounds")
            for ((v, &n), &(len, byval)) in vals.iter_mut().zip(&self.nulls).zip(&am.typs) {
                *v = if n { Datum::null() } else { adt_scalar::datum_copy(mcx, *v, byval, len)? };
            }
            self.queue.add(Queued { htup, vals, nulls }); // alloc: Node charged self.held
            self.track();
        }
    }

    /// The queue's arena on the statement, its charge re-synced only when the arena's capacity moved, or on the first
    /// queued row after a `rewind` parked it (homed on the running statement again).
    #[inline]
    fn track(&mut self) {
        let bytes = self.queue.heap_bytes();
        if bytes != self.synced || self.held.owner().is_none() {
            self.held.home(HeldCharge::of_statement());
            self.held.sync(bytes as u64);
            self.synced = bytes;
        }
    }

    /// next_indextuple: the AM's next visible row, a lossy one rechecked on the table row.
    fn fetch(&mut self, step: &mut cancelpoll::Step, q: Quals<'_>, mcx: Mcx<'static>, scan: &mut IndexScanDescData<'static>, slot: &mut SlotData<'static>, dir: ScanDirection) -> PgResult<bool> {
        loop {
            if !getnext_slot_polled(step, mcx, scan, dir, slot)? {
                return Ok(false);
            }
            if !scan.xs_recheck {
                return Ok(true);
            }
            form(q.heap, q.index, &mut *q.info, slot, &mut self.row, &mut self.ivals, &mut self.inull, mcx)?;
            let formed = (&self.ivals, &self.inull);
            if passes(&mut self.fcinfo, &mut *q.keys, |i, attno| key_col(slot, q.heap[i], attno, formed), mcx)? {
                return Ok(true);
            }
            // a row the lossy recheck drops is the unit: a run of them is as long as the index
            step.tick().map_err(|_| crate::pg_err(WALK_CANCELED))?;
        }
    }

    /// An index-only walk's recheck (IndexOnlyNext): a lossy row's keys on the index row's columns (recheckqual), a key
    /// on an expression column on its value formed from the row's table columns (`slot`: every column an index-only
    /// plan reads is one the AM returns, check_index_only; recheckqual computes the expression from them); a lossy
    /// distance is PostgreSQL's error.
    pub fn only_passes(&mut self, q: Quals<'_>, mcx: Mcx<'static>, scan: &IndexScanDescData<'static>, slot: &mut SlotData<'static>) -> PgResult<bool> {
        if scan.xs_recheck {
            let Some(row) = crate::ops::scan::raw::OnlyRow::of(scan) else { return Err(crate::pg_err("gravity: a lossy index-only row without its index tuple")) };
            let att = |attno: i16| {
                let mut null = false;
                // D3453: a column the AM cannot return reads the returnable column on its table column (PostgreSQL's
                // recheckqual reads the index tlist without the non-returnable entries, setrefs.c)
                let attno = q.cols.get(attno as usize - 1).copied().unwrap_or(attno);
                // the AM's image of this row (`xs_hitup` / `xs_itup` with its descriptor), live until the next amgettuple
                (row.attr(attno, &mut null), null)
            };
            if q.heap.contains(&0) {
                table_row(slot, q.index, &att, mcx);
                form(q.heap, q.index, &mut *q.info, slot, &mut self.row, &mut self.ivals, &mut self.inull, mcx)?;
                exectuples::exec_clear_tuple(slot, mcx);
            }
            let (ivals, inull) = (&self.ivals, &self.inull);
            let col = |i: usize, attno: i16| if q.heap[i] == 0 { (ivals[attno as usize - 1], inull[attno as usize - 1]) } else { att(attno) };
            if !passes(&mut self.fcinfo, q.keys, col, mcx)? {
                return Ok(false);
            }
        }
        if !self.orderby.is_empty() && scan.xs_recheckorderby {
            return Err(Box::new(types_error::PgError::error("lossy distance functions are not supported in index-only scans").with_sqlstate(types_error::ERRCODE_FEATURE_NOT_SUPPORTED))); // alloc: Cold
        }
        Ok(true)
    }
}

fn strip_relabel(n: Node<'_>) -> Node<'_> {
    stack_depth_core::check_stack_depth_or_panic();
    n.as_relabel_type().map_or(n, |r| strip_relabel(r.arg))
}

#[cfg(test)]
mod tests {
    use super::{cmp, passes, Sizes, IOS_MAX_ROWS};
    use datum::Datum;
    use tuplesort::{SortComparator, SortSupport};
    use types_fmgr::LocalFcinfo;
    use types_scan::scankey::{ScanKeyData, SK_ISNULL, SK_SEARCHNOTNULL, SK_SEARCHNULL};

    /// nodeIndexscan.c cmp_orderbyvals (V4R-49): ascending distances, NULLS LAST, and two NULLs tie the whole row (later
    /// keys unread); equal distances fall to the next key.
    #[test]
    fn the_reorder_comparison_is_postgres_cmp_orderbyvals() {
        let ss = [SortSupport { ssup_collation: 0, ssup_reverse: false, ssup_nulls_first: false, ssup_attno: 0, comparator: SortComparator::Float64 }; 2];
        let d = |x: f64| Datum::from_f64(x);
        assert_eq!(cmp((&[d(1.0), d(0.0)], &[false, false]), (&[d(2.0), d(0.0)], &[false, false]), &ss), -1);
        assert_eq!(cmp((&[d(2.0), d(5.0)], &[false, false]), (&[d(2.0), d(3.0)], &[false, false]), &ss), 1, "a tie falls to the next key");
        assert_eq!(cmp((&[d(0.0), d(0.0)], &[true, false]), (&[d(9.0), d(0.0)], &[false, false]), &ss), 1, "NULLS LAST");
        assert_eq!(cmp((&[d(0.0), d(1.0)], &[true, false]), (&[d(0.0), d(9.0)], &[true, false]), &ss), 0, "two NULLs tie the row");
    }

    /// IndexRecheck of null-test keys (V4R-49): IS NULL passes a null column, IS NOT NULL a non-null one; a NULL operator
    /// argument passes nothing.
    #[test]
    fn a_null_test_key_rechecks_the_column_nullness() {
        let mcx = Box::leak(Box::new(mcx::MemoryContext::new("v4r-49"))).mcx(); // alloc: Exec
        let mut fc = LocalFcinfo::<2>::new(0);
        let key = |flags| {
            let mut k = ScanKeyData::empty();
            (k.sk_flags, k.sk_attno) = (flags, 1);
            k
        };
        for (flags, null, want) in [(SK_ISNULL | SK_SEARCHNULL, true, true), (SK_ISNULL | SK_SEARCHNULL, false, false), (SK_ISNULL | SK_SEARCHNOTNULL, false, true), (SK_ISNULL | SK_SEARCHNOTNULL, true, false), (SK_ISNULL, false, false)] {
            let mut keys = [key(flags)];
            assert_eq!(passes(&mut fc, &mut keys, |_, _| (Datum::from_u64(7), null), mcx).unwrap(), want, "flags {flags:#x} null {null}");
        }
    }

    /// A window's lanes: a by-value cell as its word, a by-reference cell at its own copy of the image; a lane with no NULL
    /// cell in the window carries no valid words (every row valid), one with a NULL cell anywhere has its valid bit clear
    /// at exactly the NULL rows, wherever the first NULL falls.
    #[test]
    fn a_windows_valid_words_mark_exactly_its_null_cells() {
        use gravity_abi::{Lane, ScanPos};
        let n = 3;
        let mut b = super::IosBatch { sizes: Sizes::new(None), run: None, byref: vec![0, -1, 0], lanes: vec![0; n * IOS_MAX_ROWS], valid: vec![0; n * super::WORDS], has_null: vec![false; n], tids: vec![0; IOS_MAX_ROWS], bytes: Vec::new(), vals: vec![0; n], nulls: vec![0; n], attnos: vec![1, 2, 3] }; // alloc: Cold
        let texts: Vec<Vec<u8>> = (0..70u8).map(|i| vec![(3 << 1) | 1, b'a' + i % 26, b'z']).collect(); // alloc: Cold
        let null_at = |k: usize, r: usize| match k {
            0 => false,
            1 => r % 5 == 3,
            _ => r == 66,
        };
        let word = |k: usize, r: usize| if k == 1 { texts[r].as_ptr() as u64 } else { 1000 * k as u64 + r as u64 };
        // a NULL first at row 66 (lane 2) and every fifth row from row 3 (lane 1); a short window after has none; the
        // slot-cells fill and the arrays fill alike
        for (cap, arrays) in [(70usize, false), (3, false), (70, false), (70, true), (3, true), (70, true)] {
            let mut lanes = [Lane::NONE; 8];
            let mut site = gravity_driver::blank_site(lanes.as_mut_ptr());
            let mut w = gravity_driver::Window::over(&mut site);
            b.sizes.cap = cap;
            let mut r = 0usize;
            let got = if arrays {
                b.fill_rows(
                    |v, nl| {
                        for k in 0..n {
                            (v[k], nl[k]) = (word(k, r), null_at(k, r) as u8);
                        }
                        r += 1;
                        Ok::<_, ()>(Some(ScanPos::row(1, 0, r as u16)))
                    },
                    1,
                    &mut w,
                )
            } else {
                b.fill(
                    |c| {
                        for k in 0..n {
                            c.put(k, word(k, r), null_at(k, r));
                        }
                        r += 1;
                        Ok::<_, ()>(Some(ScanPos::row(1, 0, r as u16)))
                    },
                    1,
                    &mut w,
                )
            };
            let rows = got.unwrap().0 as usize;
            assert_eq!(rows, cap, "a window holds its cap");
            for k in 0..n {
                let l = lanes[k];
                let any = (0..rows).any(|r| null_at(k, r));
                assert_eq!(l.ptr, b.lanes[k * IOS_MAX_ROWS..].as_ptr() as *const u8, "lane {k} is the batch's");
                assert_eq!(l.valid.is_null(), !any, "lane {k}: valid words only when a cell is NULL");
                if any {
                    assert_eq!(l.valid, b.valid[k * super::WORDS..].as_ptr(), "lane {k}'s valid words are the batch's");
                }
                for r in 0..rows {
                    let ok = !any || b.valid[k * super::WORDS + r / 64] >> (r % 64) & 1 != 0;
                    assert_eq!(ok, !null_at(k, r), "lane {k} row {r}");
                    let word = b.lanes[k * IOS_MAX_ROWS + r];
                    if !ok {
                        continue;
                    }
                    if k == 1 {
                        let off = (word - b.bytes.as_ptr() as u64) as usize;
                        assert_eq!(&b.bytes[off..off + 3], &texts[r][..], "row {r}'s image, copied into the window");
                    } else {
                        assert_eq!(word, 1000 * k as u64 + r as u64);
                    }
                }
            }
        }
    }

    /// The window caps of a walk that reads `total` rows (each window full) under `sizes`.
    fn caps(mut sizes: Sizes, total: usize) -> Vec<usize> {
        let (mut out, mut read) = (Vec::new(), 0); // alloc: Cold
        while read < total {
            let n = sizes.cap().min(total - read);
            out.push(n);
            sizes.after(n);
            read += n;
        }
        out
    }

    /// No budget: 1, 2, 4 .. up to one index page's rows. A LIMIT's budget caps the windows at its count; spent (a filter
    /// above asks on), windows grow from one row by the count per window, as ExecLimit pulls one row at a time, and still
    /// reach the page-sized window on a long walk.
    #[test]
    fn windows_double_until_a_limits_budget_is_spent_then_grow_by_its_count() {
        assert_eq!(caps(Sizes::new(None), 40), [1, 2, 4, 8, 16, 9]);
        assert_eq!(caps(Sizes::new(Some(1)), 12), [1, 1, 2, 3, 4, 1]);
        assert_eq!(caps(Sizes::new(Some(10)), 80), [1, 2, 4, 3, 1, 11, 21, 31, 6]);
        assert_eq!(caps(Sizes::new(Some(0)), 3), [1, 1, 1], "a single-match inner: one row per window");
        // a long walk: windows 1, 1, 2 .. IOS_MAX_ROWS - 1, then page-sized ones
        let long = caps(Sizes::new(Some(1)), 200_000);
        assert_eq!(&long[..4], [1, 1, 2, 3]);
        assert_eq!(long[IOS_MAX_ROWS], IOS_MAX_ROWS);
        assert!(long[IOS_MAX_ROWS..long.len() - 1].iter().all(|&n| n == IOS_MAX_ROWS));
        let mut run = Sizes::new(None);
        run.set_left(3);
        assert_eq!(caps(run, 8), [1, 2, 1, 4], "a run cell's count is spent like a budget");
    }
}
