//! M2's bitmap hosts (cards/M2.md L3 `m2_bitmap.rs`, M2-inventory row 36): the page window of `BitmapSource` and the
//! BitmapAnd / BitmapOr tree.
//! - `PageBatch`: one window per heap page of the bitmap (v1 `heap_scan_bitmap_next_pagebatch`, heapam `bitmap.rs:45`;
//!   `nodebitmapheapscan` `bitmap_scan_next_pagebatch` :166): the page's visible tuples, rechecked when the page is lossy
//!   or the AM asked (`bitmapqualorig`), as attribute lanes beside the Offset lane, based at the page's block. Asked by a
//!   site that keeps an Offset lane or a RowId lane; any other keeps one row per window. The window size is the page's own
//!   tuple count. D2988: a RowId-lane site's window (each row's tid in that lane) goes on to the next pages while it holds
//!   fewer than `SPAN_ROWS` rows (at most `SPAN_PAGES` pages), so a bitmap of sparse pages (a nested loop's inner: one row
//!   a page) runs one program call per few dozen pages, not per page; an earlier page's by-reference cells are copied into
//!   the window's bytes before the slot's next store unpins it. Not under a LIMIT's read-ahead bound (`row_budget`) nor
//!   in a SERIALIZABLE transaction: a page read ahead is a SIREAD lock PostgreSQL never takes.
//! - `Tree`: a bitmap built from several index probes (C `MultiExecBitmapAnd` / `MultiExecBitmapOr`): a leaf is one
//!   index's `amgetbitmap` over its keys, And intersects its children's bitmaps (stopping at the first empty one), Or
//!   unions them (`TIDBitmap::intersect` / `union`). The site's key words are the leaves' in leaf order. A leaf whose
//!   key word is NULL is an empty bitmap (D2180): it empties an And, and an Or still unions its other children.
//! Created by M2 (owner M2-HOSTS).
use std::rc::Weak;

use gravity_abi::ctx::lane;
use gravity_abi::{Datum as Word, Lane, ScanPos};
use gravity_driver::{box_source, ArenaRef, SourceBox, Window, WindowBase};
use gravity_program::{LaneId, SiteDesc};
use mcx::Mcx;
use types_error::PgResult;
use types_rel::{Relation, RelationData};
use types_relscan::IndexScanDescData;
use types_scan::scankey::{ScanKeyData, SK_ISNULL, SK_SEARCHARRAY};
use types_slot::SlotData;
use types_snapshot::SnapshotData;
use types_storage::lock::NoLock;
use types_tuple::ItemPointerData;

use crate::ops::scan::heap_window::PAGE_ROWS;
use crate::ops::scan::bitmap_source::BitmapSource;
use crate::ops::scan::index_source::{open_index, park, KeyWord, SiteRecipe};
use crate::ops::scan::raw::SeekImages;
use crate::recipe::HostRecipe;
use crate::FilterFn;

/// The attribute number of `ctid` (`SelfItemPointerAttributeNumber`): its cell points at a per-row copy of the tid.
const SELF_ITEM_POINTER: i16 = -1;

/// D2988: a spanning window goes on to the next page while it holds fewer rows than this, ...
pub const SPAN_ROWS: usize = 64;
/// ... and has read fewer pages than this.
pub const SPAN_PAGES: usize = 64;

/// `BitmapSource`'s page window state.
pub struct PageBatch {
    /// Lane `k`'s cells at `k * cap ..`, its validity words at `k * vw ..`.
    lanes: Vec<u64>,
    valid: Vec<u64>,
    offs: Vec<u16>,
    tids: Vec<ItemPointerData>,
    vals: Vec<Word>,
    nulls: Vec<u8>,
    /// A window's row capacity (`PAGE_ROWS`; a RowId site's `PAGE_ROWS + SPAN_ROWS`) and validity words per lane.
    cap: usize,
    vw: usize,
    /// A RowId-lane site's (D2988): each row's tid word, and the spanning state.
    span: Option<Span>,
}

/// A RowId-lane window's own state (D2988).
struct Span {
    rowids: Vec<u64>,
    /// Per attribute lane: the column's `attlen` when it is by reference (-1 varlena, -2 cstring, > 0 fixed), 0 by value.
    byref: Vec<i16>,
    /// The copied by-reference images of a spanning window's earlier pages (offsets until the window ends, then pointers).
    bytes: Vec<u8>,
    /// Whether windows may span pages (`PageBatch::arm`).
    on: bool,
}

/// What one page fill reads through (`BitmapSource`'s bound state).
pub struct PageParts<'a> {
    pub mcx: Mcx<'static>,
    pub hscan: &'a mut tableam::TableScanDesc<'static>,
    pub tbm: Option<&'a tidbitmap::TIDBitmap<'static>>,
    pub iter: &'a mut tidbitmap::TbmIterator,
    pub slot: &'a mut SlotData<'static>,
    pub recheck: Option<&'a FilterFn>,
    pub page_recheck: &'a mut bool,
    pub lossy: &'a mut u64,
    pub exact: &'a mut u64,
    /// M12 Row B: the recipe's counters (a recheck's removed row; hosts v4/m12.rs).
    pub counts: &'a crate::instr::BitmapCounts,
    /// D2988: the BitmapHeapScan's own qual (`SiteRecipe::filter`) and its tally (`Prefilter`), checked on a row the recheck
    /// keeps before its cells are read; the program applies the qual to every row it gets.
    pub filter: Option<&'a FilterFn>,
    pub pre: &'a mut crate::ops::scan::index_source::Prefilter,
}

impl PageBatch {
    pub fn new(natts: usize) -> PageBatch {
        Self::sized(natts, PAGE_ROWS, None)
    }

    /// The buffers' capacity in bytes (`BitmapSource`'s kept heap, `Source::kept_bytes`).
    pub(crate) fn kept_bytes(&self) -> usize {
        use gravity_driver::vec_bytes as v;
        let span = self.span.as_ref().map_or(0, |s| v(&s.rowids) + v(&s.byref) + v(&s.bytes));
        v(&self.lanes) + v(&self.valid) + v(&self.offs) + v(&self.tids) + v(&self.vals) + v(&self.nulls) + span
    }

    /// D2988: the windows of a site that keeps a RowId lane (lane `natts`, each row's tid word); `arm` says whether they span.
    pub fn with_rowids(natts: usize) -> PageBatch {
        let cap = PAGE_ROWS + SPAN_ROWS;
        Self::sized(natts, cap, Some(Span { rowids: vec![0; cap], byref: vec![0; natts], bytes: Vec::new(), on: false })) // alloc: Node
    }

    fn sized(natts: usize, cap: usize, span: Option<Span>) -> PageBatch {
        let vw = cap.div_ceil(64);
        PageBatch { lanes: vec![0; natts * cap], valid: vec![0; natts * vw], offs: vec![0; cap], tids: vec![ItemPointerData::invalid(); cap], vals: vec![0; natts], nulls: vec![0; natts], cap, vw, span } // alloc: Node
    }

    /// D2988: at a bind, a RowId site's by-reference columns (from `heap`'s descriptor) and whether its windows span pages
    /// (`on`: no LIMIT read-ahead bound, not SERIALIZABLE).
    pub fn arm(&mut self, heap: &RelationData<'_>, attnos: &[i16], on: bool) {
        let Some(sp) = self.span.as_mut() else { return };
        // bounded("key-columns: one attlen read per scanned column, at most MaxTupleAttributeNumber (1664) columns")
        for (b, &a) in sp.byref.iter_mut().zip(attnos) {
            *b = if a < 1 { 0 } else { heap.rd_att.compact_attrs.get(a as usize - 1).map_or(0, |c| if c.attbyval { 0 } else { c.attlen }) };
        }
        sp.on = on;
    }

    /// D696: an EvalPlanQual row (`BitmapSource::window`'s single-row path) under a program lowered for page windows: its
    /// Offset lane (lane `natts`), the row's offset, as `HeapSource::window_substituted` gives one (a RowId site: its
    /// RowId lane, the row's tid word). Without it the program's row position read a lane the window never set (a null
    /// pointer: SIGSEGV).
    pub fn one(&mut self, pos: ScanPos, natts: usize, w: &mut Window<'_>) {
        self.offs[0] = pos.off;
        match self.span.as_mut() {
            Some(sp) => {
                sp.rowids[0] = gravity_kernels::ops::scan::entries::tid_word(pos.block, pos.off);
                w.set(natts, Lane::byval(sp.rowids.as_ptr() as *const u8, core::ptr::null(), 8, lane::REGION_ARENA, LaneId::RowId.encode()));
            }
            None => w.set(natts, Lane::byval(self.offs.as_ptr() as *const u8, core::ptr::null(), 2, lane::REGION_ARENA, LaneId::Offset.encode())),
        }
    }

    /// The bitmap's next page with a surviving row as a window: its rows' cells, the Offset lane (lane `natts`; a RowId
    /// site's RowId lane), the base at the page's block. D2988: a spanning RowId window goes on to the next pages while
    /// it holds fewer than `SPAN_ROWS` rows (module header); its base is then the last page's block. Returns `(rows, the
    /// last row's position)`; 0 rows when the bitmap is exhausted.
    pub fn fill(&mut self, p: PageParts<'_>, heap_oid: u32, attnos: &[i16], w: &mut Window<'_>) -> PgResult<(u32, ScanPos)> {
        let tableam::TableScanDesc::Heap(h) = p.hscan else { return Err(crate::pg_err("gravity: a bitmap page window over a table AM other than heap")) };
        let (natts, cap, vw) = (attnos.len(), self.cap, self.vw);
        let spans = self.span.as_ref().is_some_and(|sp| sp.on);
        if let Some(sp) = self.span.as_mut() {
            sp.bytes.clear();
        }
        self.valid.fill(0);
        // `kept`: the rows below it are an earlier page's, their by-reference cells copied (offsets into the span's bytes)
        let (mut rows, mut kept, mut pages, mut blk) = (0usize, 0usize, 0usize, 0u32);
        // a page of the bitmap is the unit: the pages whose every row the recheck or the qual drops are as many as the
        // bitmap's
        let mut step = cancelpoll::step("bitmap_window fill: one page of the bitmap");
        loop {
            let n = heapam::bitmap::heap_scan_bitmap_next_pagebatch(p.mcx, h, p.tbm, p.iter, p.page_recheck, p.lossy, p.exact)?;
            if n == 0 {
                break;
            }
            step.tick().map_err(|_| crate::pg_err("gravity: a bitmap page walk stopped for its statement's cancel"))?;
            // the slot still pins the last page until its next store (this page's first row): its rows' images copied now
            if kept < rows {
                self.keep(kept, rows);
                kept = rows;
            }
            let page = h.rs_cblock;
            let first = rows;
            // bounded("small-fixed: one page's visible rows, at most PAGE_ROWS (MaxHeapTuplesPerPage, 291 at 8 K pages), a recheck and a projection each")
            for i in 0..n {
                heapam::bitmap::heap_scan_bitmap_batch_store(p.mcx, h, i, p.slot);
                if *p.page_recheck {
                    if let Some(f) = p.recheck {
                        if !f(p.slot)? {
                            p.counts.add(crate::instr::BitmapCounts::REMOVED, 1);
                            continue;
                        }
                    }
                }
                // D2988: a row the scan's qual drops is deformed only as far as the qual reads, never copied into the window
                if let Some(f) = p.filter.filter(|_| p.pre.on()) {
                    if !p.pre.check(f(p.slot))? {
                        p.counts.add(crate::instr::BitmapCounts::FILTERED, 1);
                        continue;
                    }
                }
                crate::ops::shared::heap_fetch::project(p.slot, attnos, &mut self.vals, &mut self.nulls)?;
                self.tids[rows] = p.slot.base().tts_tid;
                self.offs[rows] = h.rs_vistuples[i as usize];
                if let Some(sp) = self.span.as_mut() {
                    sp.rowids[rows] = gravity_kernels::ops::scan::entries::tid_word(page, self.offs[rows]);
                }
                // bounded("key-columns: one cell per output column of a row (the row loop is the window's), at most MaxTupleAttributeNumber (1664) columns")
                for k in 0..natts {
                    self.valid[k * vw + rows / 64] |= ((self.nulls[k] == 0) as u64) << (rows % 64);
                    self.lanes[k * cap + rows] = if attnos[k] == SELF_ITEM_POINTER { &self.tids[rows] as *const ItemPointerData as u64 } else { self.vals[k] };
                }
                rows += 1;
            }
            if rows == first {
                continue;
            }
            (pages, blk) = (pages + 1, page);
            if !spans || rows >= SPAN_ROWS || pages >= SPAN_PAGES {
                break;
            }
        }
        if rows == 0 {
            return Ok((0, ScanPos::NONE));
        }
        if kept > 0 {
            self.point(kept);
        }
        // bounded("key-columns: one lane per output column, its validity over at most a window's rows, at most MaxTupleAttributeNumber (1664) columns")
        for (k, &a) in attnos.iter().enumerate() {
            let words = &self.valid[k * vw..(k + 1) * vw];
            let all = (0..rows).all(|r| words[r / 64] >> (r % 64) & 1 != 0);
            w.set(k, Lane::byval(self.lanes[k * cap..].as_ptr() as *const u8, if all { core::ptr::null() } else { words.as_ptr() }, 8, lane::REGION_ARENA, lane::id(lane::ID_ATTR, a as u16 as u32)));
        }
        match self.span.as_ref() {
            Some(sp) => w.set(natts, Lane::byval(sp.rowids.as_ptr() as *const u8, core::ptr::null(), 8, lane::REGION_ARENA, LaneId::RowId.encode())),
            None => w.set(natts, Lane::byval(self.offs.as_ptr() as *const u8, core::ptr::null(), 2, lane::REGION_ARENA, LaneId::Offset.encode())),
        }
        w.rows = rows as u32;
        w.set_base(WindowBase::at(ScanPos::new(heap_oid, blk, 0)));
        Ok((rows as u32, ScanPos::row(heap_oid, blk, self.offs[rows - 1])))
    }

    /// D2988: rows `from..to` (the page the slot pins until its next store) keep their by-reference cells: each non-null
    /// one's image copied into the span's bytes, the cell its offset there (`point` makes it a pointer at the window's end).
    fn keep(&mut self, from: usize, to: usize) {
        let (cap, vw) = (self.cap, self.vw);
        let Some(sp) = self.span.as_mut() else { return };
        // bounded("key-columns: one by-reference lane per output column, at most MaxTupleAttributeNumber (1664) columns")
        for (k, &len) in sp.byref.iter().enumerate().filter(|(_, &len)| len != 0) {
            // bounded("small-fixed: one page's rows of the window, at most PAGE_ROWS (MaxHeapTuplesPerPage, 291 at 8 K pages), a cell copy each")
            for r in from..to {
                if self.valid[k * vw + r / 64] >> (r % 64) & 1 != 0 {
                    let cell = &mut self.lanes[k * cap + r];
                    // the slot's page pin holds the image (above)
                    *cell = crate::ops::scan::index_window::copy_cell(&mut sp.bytes, *cell as *const u8, len);
                }
            }
        }
    }

    /// The kept rows' (`..kept`) copied cells made pointers into the span's bytes, which no longer grow this window.
    fn point(&mut self, kept: usize) {
        let (cap, vw) = (self.cap, self.vw);
        let Some(sp) = self.span.as_ref() else { return };
        let base = sp.bytes.as_ptr() as u64;
        // bounded("key-columns: one by-reference lane per output column, at most MaxTupleAttributeNumber (1664) columns")
        for (k, _) in sp.byref.iter().enumerate().filter(|(_, &len)| len != 0) {
            // bounded("small-fixed: the window's kept rows, at most its capacity (PAGE_ROWS + SPAN_ROWS, 355 at 8 K pages), an add each")
            for r in 0..kept {
                if self.valid[k * vw + r / 64] >> (r % 64) & 1 != 0 {
                    self.lanes[k * cap + r] += base;
                }
            }
        }
    }
}

/// A recipe's row comparison members, each key's SK_ROW_HEADER pointed at its row (IndexSource::new's): the row vectors
/// live as long as the owner, whose moves never move their buffers.
pub(crate) fn row_members(recipe: &SiteRecipe, keys: &mut [ScanKeyData]) -> Vec<Vec<ScanKeyData>> {
    let rows: Vec<Vec<ScanKeyData>> = recipe.rows.iter().map(|r| r.members.clone()).collect(); // alloc: Node
    // bounded("other: one pointer per row comparison of the probe's keys, which the statement's index quals bound")
    for (r, m) in recipe.rows.iter().zip(rows.iter()) {
        keys[r.key].sk_argument = datum::Datum::from_usize(m.as_ptr() as usize);
    }
    rows
}

/// The seek's key words bound by the recipe's words (IndexSource::seek's walk): a key's argument (a host key keeps the
/// recipe's), a row member's (a NULL member is nbtree's SK_ISNULL); `Ok(false)` when a key's word is NULL (no row
/// matches), an error for a word a bitmap walk never has (an ORDER BY key) or fewer words than the recipe's.
/// D4528: a bound search array whose image is compressed or external (an outer row's column stored out of line, an
/// initplan's value) is detoasted into `images`, as PostgreSQL's ExecIndexEvalRuntimeKeys (key_toastable) and
/// ExecIndexEvalArrayKeys (DatumGetArrayTypeP) detoast it: bitmap_array's per-element scans and nbtree's preprocessing
/// read a plain or short image only (index_source's V4R-44 rule). `reset` is true until the seek's first expansion,
/// which empties the region (D2181: one seek's images, not one per rescan).
pub(crate) fn bind_words(recipe: &SiteRecipe, keys: &mut [ScanKeyData], rows: &mut [Vec<ScanKeyData>], words: &[Word], nulls: &[u8], images: &mut SeekImages, reset: &mut bool) -> PgResult<bool> {
    if words.len() < recipe.words.len() || nulls.len() < recipe.words.len() {
        return Err(crate::pg_err("gravity: a bitmap seek with fewer key words than its recipe"));
    }
    if recipe.words.iter().zip(nulls).any(|(w, &x)| x != 0 && matches!(w, KeyWord::Key(_))) {
        return Ok(false);
    }
    // bounded("other: one key word per scan key or row member of the probe, which the statement's index quals bound")
    for (w, word) in recipe.words.iter().enumerate() {
        match *word {
            KeyWord::Key(i) if !recipe.host_keys[i as usize] => {
                let k = &mut keys[i as usize];
                k.sk_argument = datum::Datum::from_u64(words[w]);
                if k.sk_flags & SK_SEARCHARRAY != 0 && !crate::ops::scan::index_source::search_array_plain(k.sk_argument) {
                    let mcx = images.region(*reset);
                    *reset = false;
                    k.sk_argument = crate::ops::scan::index_source::plain_array(mcx, k.sk_argument)?;
                }
            }
            KeyWord::Key(_) => {}
            KeyWord::Member(r, j) => {
                let (t, m) = (&recipe.rows[r as usize].members[j as usize], &mut rows[r as usize][j as usize]);
                let null = nulls[w] != 0;
                m.sk_flags = t.sk_flags | if null { SK_ISNULL } else { 0 };
                m.sk_strategy = t.sk_strategy;
                m.sk_argument = datum::Datum::from_u64(if null { 0 } else { words[w] });
            }
            KeyWord::OrderBy(_) => return Err(crate::pg_err("gravity: an ORDER BY key word under a bitmap walk")),
        }
    }
    Ok(true)
}

/// A bitmap tree's shape over its leaves (indexes into `TreeRecipe::leaves`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shape {
    Leaf(usize),
    And(Vec<Shape>),
    Or(Vec<Shape>),
}

/// The tree's recipe: one index recipe per leaf (`SiteRecipe::from_plan_any_am`), the shape over them.
#[derive(Clone)]
pub struct TreeRecipe {
    pub leaves: Vec<SiteRecipe>,
    pub shape: Shape,
}

impl TreeRecipe {
    /// The site's key words: the leaves' in leaf order (each leaf's `words`: a null test has none, a row comparison one
    /// per member).
    pub fn nkeys(&self) -> usize {
        self.leaves.iter().map(|l| l.words.len()).sum()
    }
}

/// A bound tree: each leaf's index and bitmap scan, begun at bind and ended at unbind (a non-btree scan cannot be parked).
pub struct Tree {
    recipe: TreeRecipe,
    keys: Vec<Vec<ScanKeyData>>,
    rows: Vec<Vec<Vec<ScanKeyData>>>,
    index: Vec<Option<Relation<'static>>>,
    parked: Vec<Option<Weak<RelationData<'static>>>>,
    scans: Vec<Option<IndexScanDescData<'static>>>,
    /// D2180: the leaves this seek bound a NULL key word to. Each is an empty bitmap and is never probed: its keys still
    /// hold the previous seek's arguments (PG: SK_ISNULL or a NULL array key skips that index scan only).
    dead: Vec<bool>,
    /// D2255: the error a leaf's computed key raised at this seek (bitmap_param.rs `ParamBitmap::seek`), raised only when the
    /// build reaches the leaf: PostgreSQL evaluates a BitmapIndexScan's runtime keys when its parent runs it, and
    /// MultiExecBitmapAnd stops at its first empty child.
    held: Vec<Option<Box<types_error::PgError>>>,
    /// D4528: the plain images of one seek's compressed or external search arrays over all leaves (`bind_words`), reset at
    /// the next seek that expands one; read only by the build that follows the seek (index_source's D2181 rule).
    seek_images: SeekImages,
}

impl Tree {
    pub fn new(recipe: TreeRecipe) -> Tree {
        let n = recipe.leaves.len();
        let mut keys: Vec<Vec<ScanKeyData>> = recipe.leaves.iter().map(|l| l.keys.clone()).collect(); // alloc: Node
        let rows = recipe.leaves.iter().zip(keys.iter_mut()).map(|(l, k)| row_members(l, k)).collect(); // alloc: Node
        Tree { recipe, keys, rows, index: (0..n).map(|_| None).collect(), parked: vec![None; n], scans: (0..n).map(|_| None).collect(), dead: vec![false; n], held: (0..n).map(|_| None).collect(), seek_images: SeekImages::default() } // alloc: Node
    }

    pub fn nkeys(&self) -> usize {
        self.recipe.nkeys()
    }

    /// Every leaf's index opened (a validated relation of `heap`, in `mcx`) and its bitmap scan begun (in `smcx`, the
    /// source's per-bind context: D3195).
    pub fn bind(&mut self, mcx: Mcx<'static>, smcx: Mcx<'static>, heap: u32, snapshot: &std::rc::Rc<SnapshotData<'static>>) -> PgResult<()> {
        // bounded("other: one index opened and one scan begun per leaf of the plan's bitmap tree (its BitmapIndexScan nodes), which the statement's quals bound")
        for (i, l) in self.recipe.leaves.iter().enumerate() {
            let index = open_index(mcx, &self.parked[i], l.index, l.index_lockmode)?;
            if !index.rd_index.as_ref().is_some_and(|x| x.indisvalid && x.indrelid == heap) {
                index.close(NoLock)?;
                return Err(crate::pg_err("gravity: relation changed under the parked plan"));
            }
            // D4599: a leaf whose scan does not begin closes its index (the earlier leaves are the caller's `unbind`)
            let scan = match indexam::index_beginscan_bitmap(smcx, &index, snapshot.clone(), l.nkeys as i32) {
                Ok(scan) => scan,
                Err(e) => {
                    let _ = index.close(NoLock);
                    return Err(e);
                }
            };
            self.scans[i] = Some(scan);
            self.index[i] = Some(index);
        }
        Ok(())
    }

    pub fn unbind(&mut self) -> PgResult<()> {
        // bounded("other: one scan ended per leaf of the plan's bitmap tree (its BitmapIndexScan nodes), which the statement's quals bound")
        for i in 0..self.scans.len() {
            if let Some(s) = self.scans[i].take() {
                indexam::index_endscan(s)?;
            }
            self.parked[i] = park(self.index[i].take())?;
        }
        Ok(())
    }

    /// The seek's key words into the leaves' keys (a search-array key keeps the recipe's argument); a leaf whose key is
    /// NULL is dead (no row matches it: a NULL argument never satisfies an index qual). `false` when the whole tree is
    /// empty by its dead leaves alone. A held leaf (D2255) binds nothing and is not dead; with one held, the build runs
    /// in plan order (a held leaf before an AND's empty child raises there, one after it never runs).
    pub fn seek(&mut self, keys: &[Word], keys_null: &[u8]) -> PgResult<bool> {
        let (mut w, mut reset) = (0, true);
        // bounded("other: one key binding per leaf of the plan's bitmap tree (its BitmapIndexScan nodes), which the statement's quals bound")
        for ((((l, k), r), dead), held) in self.recipe.leaves.iter().zip(self.keys.iter_mut()).zip(self.rows.iter_mut()).zip(self.dead.iter_mut()).zip(self.held.iter()) {
            let n = l.words.len();
            if keys.len() < w + n || keys_null.len() < w + n {
                return Err(crate::pg_err("gravity: a bitmap tree seek with fewer key words than its leaves"));
            }
            *dead = held.is_none() && !bind_words(l, k, r, &keys[w..w + n], &keys_null[w..w + n], &mut self.seek_images, &mut reset)?;
            w += n;
        }
        Ok(self.held.iter().any(Option::is_some) || !self.empty(&self.recipe.shape))
    }

    /// D2255: no leaf holds an error (every seek's computed keys are evaluated anew).
    pub fn hold_reset(&mut self) {
        self.held.iter_mut().for_each(|h| *h = None);
    }

    /// D2255: the leaf owning site key word `w` (the leaves' words in leaf order) holds `e` until the build reaches it; a
    /// leaf keeps its first error (its runtime keys are evaluated in order). `Err(e)` back when no leaf owns `w`.
    pub fn hold(&mut self, w: usize, e: Box<types_error::PgError>) -> types_error::PgResult<()> {
        let mut start = 0;
        // bounded("other: one range test per leaf of the plan's bitmap tree (its BitmapIndexScan nodes), which the statement's quals bound")
        for (i, l) in self.recipe.leaves.iter().enumerate() {
            let n = l.words.len();
            if (start..start + n).contains(&w) {
                self.held[i].get_or_insert(e);
                return Ok(());
            }
            start += n;
        }
        Err(e)
    }

    /// Whether a node is empty by the dead leaves alone: a dead leaf, an And with an empty child, an Or whose every child
    /// is empty (MultiExecBitmapOr unions the rest).
    fn empty(&self, s: &Shape) -> bool {
        stack_depth_core::check_stack_depth_or_panic();
        match s {
            Shape::Leaf(i) => self.dead[*i],
            Shape::And(kids) => kids.iter().any(|k| self.empty(k)),
            Shape::Or(kids) => kids.iter().all(|k| self.empty(k)),
        }
    }

    /// The tree's bitmap over this seek's keys, `maxbytes` per node (work_mem, as PostgreSQL's).
    pub fn build<'m>(&mut self, mcx: Mcx<'m>, maxbytes: usize) -> PgResult<tidbitmap::TIDBitmap<'m>> {
        let shape = self.recipe.shape.clone();
        self.node(mcx, &shape, maxbytes)
    }

    fn node<'m>(&mut self, mcx: Mcx<'m>, s: &Shape, maxbytes: usize) -> PgResult<tidbitmap::TIDBitmap<'m>> {
        stack_depth_core::check_stack_depth()?;
        match s {
            Shape::Leaf(i) => {
                // D2255: the build reached a leaf whose computed key raised at the seek: PostgreSQL raises here
                if let Some(e) = self.held[*i].take() {
                    return Err(e);
                }
                let mut tbm = tidbitmap::TIDBitmap::new(mcx, maxbytes);
                if self.dead[*i] {
                    return Ok(tbm);
                }
                let n = self.recipe.leaves[*i].nkeys;
                let native = self.index[*i].as_ref().is_some_and(|x| x.rd_rel.relam == crate::ops::scan::index_source::BTREE_AM_OID);
                let scan = self.scans[*i].as_mut().ok_or_else(|| crate::pg_err("gravity: a bitmap tree leaf on an unbound source"))?;
                // D433: a non-btree leaf's = ANY keys scan once per element (m2_bitmap_array.rs)
                super::bitmap_array::getbitmap(mcx, scan, &self.keys[*i][..n], native, &mut tbm)?;
                Ok(tbm)
            }
            Shape::And(kids) | Shape::Or(kids) => {
                let and = matches!(s, Shape::And(_));
                let mut acc: Option<tidbitmap::TIDBitmap<'m>> = None;
                let mut step = cancelpoll::step("bitmap tree node: its children's bitmaps built and combined");
                for k in kids {
                    step.tick().map_err(|_| crate::pg_err("gravity: a bitmap tree build stopped for its statement's cancel"))?;
                    let b = self.node(mcx, k, maxbytes)?;
                    match acc.as_mut() {
                        None => acc = Some(b),
                        Some(a) if and => a.intersect(&b),
                        Some(a) => a.union(&b)?,
                    }
                    // MultiExecBitmapAnd: an empty intersection ends the AND (no later probe can add a tid)
                    if and && acc.as_ref().is_some_and(|a| a.is_empty()) {
                        break;
                    }
                }
                acc.ok_or_else(|| crate::pg_err("gravity: a bitmap AND / OR node without children"))
            }
        }
    }
}

/// The tree's host recipe (`Recipe::Rung`): a `BitmapSource` whose bitmap the tree builds, its recheck the plan's
/// `bitmapqualorig` compiled for this execution; the source's own index recipe is the first leaf's (the heap, the columns).
#[derive(Clone)]
pub struct BitmapTreeRecipe {
    pub tree: TreeRecipe,
    pub recheck: Option<FilterFn>,
}

impl HostRecipe for BitmapTreeRecipe {
    fn rel(&self) -> types_core::Oid {
        self.tree.leaves[0].heap
    }
    fn size_hint(&self) -> usize {
        core::mem::size_of::<BitmapSource>()
    }
    fn derive(&self, desc: &SiteDesc, a: ArenaRef) -> SourceBox {
        box_source(BitmapSource::new(self.tree.leaves[0].clone(), self.recheck.clone()).with_tree(Tree::new(self.tree.clone())).paged_by(desc), a)
    }
    fn clone_box(&self) -> Box<dyn HostRecipe> {
        Box::new(self.clone()) // alloc: Program
    }
}
