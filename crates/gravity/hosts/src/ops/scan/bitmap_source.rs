//! The bitmap heap scan as a source (P1 item 1): the index AM's `amgetbitmap` builds a `TIDBitmap` from the bound keys at
//! the seek, the heap walk over its pages hands each visible row to the program as a 1-row window, exactly as
//! `IndexSource` does for a btree probe. The AM's recheck flag (a lossy page, or an AM whose bitmap is never exact:
//! BRIN, bloom) rechecks the plan's original quals (`BitmapHeapScan.bitmapqualorig`) on the heap tuple, PostgreSQL's
//! `BitmapHeapNext`. One index probe per bitmap in this landing (`BitmapAnd` / `BitmapOr` trees refuse by name in the lowerer).
//!
//! The recipe is an index recipe (`SiteRecipe::from_plan_any_am`: any AM with `amgetbitmap`), the recheck qual compiled per
//! execution (`gravity_exec::admit`: its params are the statement's), the walk direction irrelevant (a bitmap has no order).
//! A non-btree scan cannot be parked (`index_parkscan` is btree's), so the scan and the heap walk end at unbind and begin at bind.
//! A source derived at a registry entry (`at_entry`) serves a parked Node: its unbind drops the recheck qual (the
//! execution's context ends with the statement) and its next bind takes that execution's from the statement's `Resolved`.
use std::rc::{Rc, Weak};

use gravity_abi::ctx::lane;
use gravity_abi::{Datum as Word, Lane, ScanPos};
use gravity_driver::{BindingRef, Cursor, Error, Result, Source, Window, WindowBase};
use types_error::PgResult;
use types_fmgr::LocalFcinfo;
use types_rel::{Relation, RelationData};
use types_relscan::IndexScanDescData;
use types_scan::scankey::{ScanKeyData, SK_ROW_HEADER, SK_SEARCHARRAY, SK_SEARCHNOTNULL, SK_SEARCHNULL};
use types_slot::SlotData;
use types_snapshot::SnapshotData;
use types_storage::lock::NoLock;

use crate::ops::scan::index_source::{heap_am, index_fetch_am, open_heap, open_index, park, pos_of, tid_of, SiteRecipe};
use crate::ops::scan::heap_source::HeldCharge;
use crate::ops::scan::pgrc::granule_source::Resolved;
use crate::ops::shared::rel::snapshot_of;
use crate::scratch::NodeScratch;
use crate::FilterFn;

pub struct BitmapSource {
    /// The heap kept in the source's own buffers (`kept_bytes`): on the statement from the bind, then on each window's
    /// owner, parked on the session at the unbind (a parked Node keeps them), and where the slot depot's
    /// `Source::transfer_held` sends it. Declared first: it drops before the buffers.
    held: HeldCharge,
    recipe: SiteRecipe,
    /// The plan's `bitmapqualorig` over the heap tuple, evaluated where the AM asks for a recheck (never on an exact page).
    recheck: Option<FilterFn>,
    /// The registry entry the source was derived at, and whether its recipe rechecks (`at_entry`): the unbind drops the
    /// recheck and the bind takes the executing statement's (`Resolved::recheck`). `None`: the recheck it was made with.
    entry: Option<(usize, bool)>,
    keys: Vec<ScanKeyData>,
    /// The row comparison keys' members (SK_ROW_MEMBER), each header's argument pointing at its row.
    rows: Vec<Vec<ScanKeyData>>,
    heap: Option<Relation<'static>>,
    index: Option<Relation<'static>>,
    parked_heap: Option<Weak<RelationData<'static>>>,
    parked_index: Option<Weak<RelationData<'static>>>,
    scan: Option<IndexScanDescData<'static>>,
    hscan: Option<tableam::TableScanDesc<'static>>,
    tbm: Option<tidbitmap::TIDBitmap<'static>>,
    iter: tidbitmap::TbmIterator,
    slot: Option<SlotData<'static>>,
    snapshot: Option<Rc<SnapshotData<'static>>>,
    substituted: Option<ScanPos>,
    fcinfo: LocalFcinfo<2>,
    pos: ScanPos,
    open: bool,
    bound: bool,
    page_recheck: bool,
    lossy_pages: u64,
    exact_pages: u64,
    row: Vec<Word>,
    row_valid: Vec<u64>,
    vbuf: Vec<Word>,
    nbuf: Vec<u8>,
    cur: Cursor,
    scratch: NodeScratch,
    /// M2 (hosts v4/m2_bitmap.rs): page windows for a site that keeps an Offset lane, and a BitmapAnd / BitmapOr tree.
    page: Option<crate::ops::scan::bitmap_window::PageBatch>,
    /// The bound relation's pages are heap's: `page`'s windows read them; another AM's rows come one per window.
    pages: bool,
    tree: Option<crate::ops::scan::bitmap_window::Tree>,
    /// Set for `freeze`'s build only: a btree probe reads through the index's captured image (bitmap_image.rs).
    via_image: bool,
    /// A2 entry 2: the pool threads the morsel leader's btree build may walk the leaves on (`freeze`'s `leaves`; < 2: the
    /// leader walks).
    leaves: u16,
    /// D2988: the tally of the recipe's qual pre-check (`SiteRecipe::filter`, index_source.rs `Prefilter`) over the source's life.
    pre: crate::ops::scan::index_source::Prefilter,
    /// D4528: the plain images of one seek's compressed or external search arrays (`bitmap_window::bind_words`), reset at
    /// the next seek that expands one; read only by the bitmap build that follows the seek (index_source's D2181 rule).
    seek_images: crate::ops::scan::raw::SeekImages,
}

impl BitmapSource {
    pub fn new(recipe: SiteRecipe, recheck: Option<FilterFn>) -> BitmapSource {
        let mut keys = recipe.keys.clone();
        let rows = crate::ops::scan::bitmap_window::row_members(&recipe, &mut keys);
        let natts = recipe.natts;
        BitmapSource { held: HeldCharge::new(), recipe, recheck, entry: None, keys, rows, heap: None, index: None, parked_heap: None, parked_index: None, scan: None, hscan: None, tbm: None, iter: tidbitmap::TbmIterator::empty(), slot: None, snapshot: None, substituted: None, fcinfo: LocalFcinfo::<2>::new(0), pos: ScanPos::NONE, open: false, bound: false, page_recheck: false, lossy_pages: 0, exact_pages: 0, row: vec![0; natts], row_valid: vec![0; natts], vbuf: vec![0; natts], nbuf: vec![0; natts], cur: Cursor::new(), scratch: NodeScratch::new("GravityBitmapSource").with_bind(), page: None, pages: true, tree: None, via_image: false, leaves: 0, pre: Default::default(), seek_images: Default::default() } // alloc: Node
    }

    /// Derived at registry entry `i` (scan derive.rs): each bind takes the executing statement's recheck qual for that entry
    /// from its `Resolved`, so a parked Node's source never runs the qual of the statement that built it.
    pub fn at_entry(mut self, i: usize) -> BitmapSource {
        self.entry = Some((i, self.recheck.is_some()));
        self
    }

    /// A source at a registry entry (`at_entry`) lets go of its recheck qual: it was compiled in an execution's context,
    /// which ends with the statement, and the next bind takes the executing statement's (`bind_recheck`).
    fn drop_entry_recheck(&mut self) {
        if self.entry.is_some() {
            self.recheck = None;
        }
    }

    /// The buffers kept between statements (`Source::kept_bytes`): the keys, the row buffers and the page windows.
    fn kept(&self) -> usize {
        use gravity_driver::vec_bytes as v;
        let rows = self.rows.iter().map(v).sum::<usize>();
        v(&self.keys) + v(&self.rows) + rows + v(&self.row) + v(&self.row_valid) + v(&self.vbuf) + v(&self.nbuf) + self.page.as_ref().map_or(0, |p| p.kept_bytes())
    }

    /// The bind's recheck qual: the executing statement's for a source at a registry entry (`at_entry`), else the one it
    /// was made with; a worker slot's binding takes it as the leader's does (pglfast#3520: a parked worker set's source
    /// is bound again at each later W8 execution). A rechecking entry without one in the binding's `Resolved` keeps the
    /// qual it holds, and errors when it holds none (its unbind dropped the last execution's).
    fn bind_recheck(&mut self, b: &BindingRef<'_>) -> PgResult<()> {
        let Some((i, rechecks)) = self.entry else { return Ok(()) };
        if let Some(f) = b.resolved.and_then(|r| r.downcast_ref::<Resolved>()).and_then(|r| r.recheck(i)) {
            self.recheck = Some(f);
        } else if rechecks && self.recheck.is_none() {
            return Err(crate::pg_err("gravity: a parked bitmap source's bind without this execution's recheck qual"));
        }
        Ok(())
    }

    /// One window per heap page (M2): the page's surviving rows beside the Offset lane.
    pub fn paged(mut self, on: bool) -> BitmapSource {
        self.page = on.then(|| crate::ops::scan::bitmap_window::PageBatch::new(self.recipe.natts));
        self
    }

    /// The windows `desc`'s lanes ask for: whole pages beside a RowId lane (D2988: spanning several sparse ones), one page
    /// beside an Offset lane, else one row.
    pub fn paged_by(mut self, desc: &gravity_program::SiteDesc) -> BitmapSource {
        let has = |id: gravity_program::LaneId| desc.lanes().iter().any(|l| l.id == id);
        if has(gravity_program::LaneId::RowId) {
            self.page = Some(crate::ops::scan::bitmap_window::PageBatch::with_rowids(self.recipe.natts));
            return self;
        }
        self.paged(has(gravity_program::LaneId::Offset))
    }

    /// D2255: no tree leaf holds an error (bitmap_window.rs `Tree::hold_reset`); nothing without a tree.
    pub fn hold_reset(&mut self) {
        if let Some(t) = self.tree.as_mut() {
            t.hold_reset();
        }
    }

    /// D2255: the tree leaf owning key word `w` holds `e` until the build reaches it (`Tree::hold`); `Err(e)` back when the
    /// source has no tree or no leaf owns `w` (the caller raises it at once, as a one-index scan's rescan does).
    pub fn hold(&mut self, w: usize, e: Box<types_error::PgError>) -> types_error::PgResult<()> {
        match self.tree.as_mut() {
            Some(t) => t.hold(w, e),
            None => Err(e),
        }
    }

    /// The bitmap built by a BitmapAnd / BitmapOr tree (M2) in place of the recipe's one index probe.
    pub fn with_tree(mut self, tree: crate::ops::scan::bitmap_window::Tree) -> BitmapSource {
        self.tree = Some(tree);
        self
    }

    pub fn scratch_used(&self) -> usize {
        self.scratch.used()
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
        let stale = heap.rd_att.natts != self.recipe.heap_natts() || !index_fetch_am(&heap) || !index.rd_index.as_ref().is_some_and(|i| i.indisvalid && i.indrelid == heap.rd_id);
        if stale {
            index.close(NoLock)?;
            heap.close(NoLock)?;
            return Err(crate::pg_err("gravity: relation changed under the parked plan"));
        }
        if !heap.is_scannable() {
            // an unpopulated materialized view: PostgreSQL's ExecOpenScanRelation error (the heap walk's, heap_source.rs)
            let e = crate::ops::scan::heap_source::unpopulated(heap.name());
            index.close(NoLock)?;
            heap.close(NoLock)?;
            return Err(e);
        }
        // the scans are per bind (no park for a non-btree AM), in the per-bind context the unbind resets (D3195); the slot
        // outlives them
        if let Err(e) = self.begin_scans(&heap, &index, &snapshot) {
            self.abandon_bind(heap, index);
            return Err(e);
        }
        // D2988: a RowId site's windows span sparse pages unless a LIMIT bounds the walk's read-ahead or the transaction is
        // SERIALIZABLE (a page read ahead is a SIREAD lock PostgreSQL never takes)
        if let Some(b) = self.page.as_mut() {
            let serializable = xact_seams::isolation_is_serializable::is_installed() && xact_seams::isolation_is_serializable::call();
            b.arm(&heap, &self.recipe.attnos, self.recipe.row_budget().is_none() && !serializable);
        }
        self.heap = Some(heap);
        self.index = Some(index);
        self.snapshot = Some(snapshot);
        self.pos = ScanPos::NONE;
        self.open = false;
        self.bound = true;
        Ok(())
    }

    /// The bind's scans over the opened relations: the index's bitmap scan and the heap's, a tree's leaves (in the
    /// source's per-bind context, D3195), the slot (in its own context: it outlives them).
    fn begin_scans(&mut self, heap: &Relation<'static>, index: &Relation<'static>, snapshot: &Rc<SnapshotData<'static>>) -> PgResult<()> {
        let (mcx, smcx) = (self.scratch.mcx(), self.scratch.bind_mcx());
        self.scan = Some(indexam::index_beginscan_bitmap(smcx, index, snapshot.clone(), self.recipe.nkeys as i32)?);
        self.hscan = Some(tableam::table_beginscan_bm(smcx, heap, Some(snapshot.clone()))?);
        self.pages = heap_am(heap);
        if let (false, Some(h)) = (self.pages, self.hscan.as_mut()) {
            let mut needed = vec![false; heap.rd_att.natts as usize]; // alloc: Node
            // bounded("key-columns: one flag per read column, at most MaxTupleAttributeNumber (1664)")
            for &a in &self.recipe.attnos {
                if let Some(x) = usize::try_from(a).ok().and_then(|a| a.checked_sub(1)).and_then(|i| needed.get_mut(i)) {
                    *x = true;
                }
            }
            tableam::table_scan_set_needed_attrs(h, &needed);
        }
        if let Some(t) = self.tree.as_mut() {
            t.bind(mcx, smcx, heap.rd_id, snapshot)?;
        }
        if self.slot.is_none() {
            self.slot = Some(tableam::table_slot_create(mcx, heap)?);
        }
        Ok(())
    }

    /// D4599 (the D4602 audit): a bind that failed after its relations opened is undone here, whatever `bound` says: the
    /// driver never unbinds a host whose own bind failed (`Node::apply`), and the Node then parks (`Entry::release`). As
    /// the unbind: the scans begun end, the tree's leaves unbind (`Tree::unbind`), the relations close (parked as the
    /// unbind parks them), the per-bind context resets. The bind's own error is the one raised; a cleanup error is dropped.
    fn abandon_bind(&mut self, heap: Relation<'static>, index: Relation<'static>) {
        if let Some(h) = self.hscan.take() {
            let _ = tableam::table_endscan(h);
        }
        if let Some(scan) = self.scan.take() {
            let _ = indexam::index_endscan(scan);
        }
        if let Some(t) = self.tree.as_mut() {
            let _ = t.unbind();
        }
        self.parked_index = park(Some(index)).unwrap_or(None);
        self.parked_heap = park(Some(heap)).unwrap_or(None);
        self.scratch.reset_bind();
    }

    fn unbind_pg(&mut self) -> PgResult<()> {
        self.open = false;
        self.substituted = None;
        if !self.bound {
            return Ok(());
        }
        self.bound = false;
        self.iter.end_iterate();
        self.tbm = None;
        if let Some(slot) = self.slot.as_mut() {
            exectuples::exec_clear_tuple(slot, self.scratch.mcx());
        }
        if let Some(h) = self.hscan.take() {
            tableam::table_endscan(h)?;
        }
        if let Some(scan) = self.scan.take() {
            indexam::index_endscan(scan)?;
        }
        if let Some(t) = self.tree.as_mut() {
            t.unbind()?;
        }
        self.parked_index = park(self.index.take())?;
        self.parked_heap = park(self.heap.take())?;
        self.snapshot = None;
        self.scratch.reset_exec();
        self.scratch.reset_bind();
        Ok(())
    }

    /// The bitmap of the bound keys: `amgetbitmap` into a fresh `TIDBitmap` (work_mem-sized, PostgreSQL's), its
    /// private iterator armed; the heap walk rewound for a second seek (a nested loop's inner).
    fn build_bitmap(&mut self) -> PgResult<()> {
        let mut tbm = self.fresh_bitmap()?;
        let it = tbm.begin_private_iterate()?;
        self.iter = tidbitmap::TbmIterator::private(it);
        self.tbm = Some(tbm);
        if let Some(h) = self.hscan.as_mut() {
            tableam::table_rescan(self.scratch.exec_mcx(), h, None)?;
        }
        Ok(())
    }

    /// The bitmap of `keys` built once on the leader and frozen for the pool (`prepare_shared_iterate`:
    /// its entries copied out of the scan's context, which the unbind resets at once), as PostgreSQL's parallel bitmap heap
    /// scan leader builds it; `None` when a key word makes the scan empty (a NULL comparison key). The source is bound
    /// and unbound here: it serves no window.
    /// A2 entry 2: `leaves` pool threads may walk a btree's leaves (`bitmap_image::getbitmap_wide`; < 2: the leader).
    pub fn freeze(&mut self, snapshot: Rc<SnapshotData<'static>>, keys: &[Word], nulls: &[u8], leaves: u16) -> PgResult<Option<std::sync::Arc<tidbitmap::TbmSharedIterState>>> {
        self.bind_pg(snapshot)?;
        let built = (|| -> PgResult<Option<std::sync::Arc<tidbitmap::TbmSharedIterState>>> {
            let open = match self.tree.as_mut() {
                Some(t) => t.seek(keys, nulls)?,
                None => crate::ops::scan::bitmap_window::bind_words(&self.recipe, &mut self.keys, &mut self.rows, keys, nulls, &mut self.seek_images, &mut true)?,
            };
            if !open {
                return Ok(None);
            }
            (self.via_image, self.leaves) = (true, leaves);
            let tbm = self.fresh_bitmap();
            (self.via_image, self.leaves) = (false, 0);
            Ok(Some(tbm?.prepare_shared_iterate()?))
        })();
        let unbound = self.unbind_pg();
        let frozen = built?;
        unbound?;
        Ok(frozen)
    }

    /// A fresh `TIDBitmap` of the bound keys (not yet iterated).
    fn fresh_bitmap(&mut self) -> PgResult<tidbitmap::TIDBitmap<'static>> {
        // D2970: the last seek's bitmap, its page reads and its rescan die here, as PostgreSQL frees the TIDBitmap at
        // ExecReScanBitmapHeapScan: the exec context is reset per seek, not only at unbind, so a SubPlan / nested loop
        // inner rescanned once per outer row holds one bitmap, not every one it built (rv Q2: 45 GB)
        self.iter.end_iterate();
        self.tbm = None;
        if let Some(slot) = self.slot.as_mut() {
            exectuples::exec_clear_tuple(slot, self.scratch.mcx());
        }
        self.scratch.reset_exec();
        let mcx = self.scratch.exec_mcx();
        let n = self.recipe.nkeys;
        let maxbytes = crate::inputs::work_mem_kb() as usize * 1024;
        let tbm = match self.tree.as_mut() {
            Some(t) => t.build(mcx, maxbytes)?,
            None => {
                let scan = self.scan.as_mut().ok_or_else(|| crate::pg_err("gravity: seek on an unbound bitmap source"))?;
                let mut tbm = tidbitmap::TIDBitmap::new(mcx, maxbytes);
                // D433: a non-btree AM's = ANY keys scan once per element (hosts v4/m2_bitmap_array.rs)
                let native = self.index.as_ref().is_some_and(|x| x.rd_rel.relam == crate::ops::scan::index_source::BTREE_AM_OID);
                // the morsel leader's build reads a btree through its image (the keys pass as they are: btree is native)
                let imaged = match (self.via_image && native, self.index.as_ref(), self.snapshot.as_ref()) {
                    (true, Some(index), Some(snapshot)) => match crate::ops::scan::bitmap_image::getbitmap_wide(index, snapshot, &self.keys[..n], scan.xactStartedInRecovery, self.leaves, maxbytes, &mut tbm)? {
                        Some(n) => Some(n),
                        None => crate::ops::scan::bitmap_image::getbitmap(mcx, index, snapshot, &self.keys[..n], scan.xactStartedInRecovery, &mut tbm)?,
                    },
                    _ => None,
                };
                let ntids = match imaged {
                    Some(n) => n,
                    None => crate::ops::scan::bitmap_array::getbitmap(mcx, scan, &self.keys[..n], native, &mut tbm)?,
                };
                self.recipe.bitmap().add(crate::instr::BitmapCounts::NTIDS, ntids);
                tbm
            }
        };
        Ok(tbm)
    }

    /// One heap page of the bitmap as a window (hosts v4/m2_bitmap.rs `PageBatch::fill`).
    fn window_page(&mut self, w: &mut Window<'_>) -> PgResult<u32> {
        if !self.open {
            return Ok(0);
        }
        let (Some(b), Some(hscan), Some(slot)) = (self.page.as_mut(), self.hscan.as_mut(), self.slot.as_mut()) else {
            return Err(crate::pg_err("gravity: a bitmap page window on an unbound source"));
        };
        let at = (self.exact_pages, self.lossy_pages);
        let parts = crate::ops::scan::bitmap_window::PageParts { mcx: self.scratch.exec_mcx(), hscan, tbm: self.tbm.as_ref(), iter: &mut self.iter, slot, recheck: self.recheck.as_ref(), page_recheck: &mut self.page_recheck, lossy: &mut self.lossy_pages, exact: &mut self.exact_pages, counts: self.recipe.bitmap(), filter: self.recipe.filter.as_ref(), pre: &mut self.pre };
        let (rows, last) = b.fill(parts, self.recipe.heap, &self.recipe.attnos, w)?;
        self.recipe.bitmap().pages(at, (self.exact_pages, self.lossy_pages));
        self.open = rows > 0;
        self.pos = last;
        Ok(rows)
    }

    /// The EPQ test (`IndexSource::next_substituted`): the newest version at `pos` against the keys as ordinary quals and the recheck qual.
    fn next_substituted(&mut self, pos: ScanPos, values: &mut [Word], isnull: &mut [u8]) -> PgResult<bool> {
        // mergecmd D1684: the empty row position a NULL REFERENCE mark stores (offset 0, a MERGE's NOT MATCHED BY SOURCE
        // row): no row, as the heap and index sources' (D2048), never a fetch at block 0 (an empty relation has none)
        if !pos.is_valid() || pos.off == 0 {
            return Ok(false);
        }
        let mcx = self.scratch.exec_mcx();
        let heap = self.heap.as_ref().expect("bound");
        let slot = self.slot.as_mut().expect("bound");
        let tid = tid_of(pos);
        if !crate::ops::shared::heap_fetch::fetch_any(mcx, heap, &tid, slot)? {
            return Ok(false);
        }
        // a tree's keys are several probes' (AND / OR): its recheck, the plan's `bitmapqualorig`, is the whole qual
        let nkeys = if self.tree.is_some() { 0 } else { self.recipe.nkeys };
        if self.tree.is_some() && self.recheck.is_none() {
            return Err(crate::pg_err("gravity: a bitmap tree's substituted row without its recheck qual"));
        }
        // bounded("other: one recheck per scan key of the probe, which the statement's index quals bound")
        for i in 0..nkeys {
            if self.keys[i].sk_flags & SK_ROW_HEADER != 0 || self.recipe.key_heap_attnos[i] == 0 {
                // a row comparison or a key on an expression column: the recheck qual (`bitmapqualorig`, the whole clause
                // over the table's columns) decides the row, as BitmapHeapRecheck does for every key
                if self.recheck.is_none() {
                    return Err(crate::pg_err("gravity: a bitmap row comparison's or expression key's substituted row without its recheck qual"));
                }
                continue;
            }
            let search = self.keys[i].sk_flags & (SK_SEARCHNULL | SK_SEARCHNOTNULL);
            let mut null = false;
            let d = exectuples::slot_getattr(slot, self.recipe.key_heap_attnos[i] as i32, &mut null);
            if search != 0 {
                // D409: a null test key rechecks its column's nullness (IndexSource's substituted row, V4R-49)
                if null != (search == SK_SEARCHNULL) {
                    return Ok(false);
                }
                continue;
            }
            if self.recipe.host_keys[i] || self.keys[i].sk_flags & SK_SEARCHARRAY != 0 {
                // a search array (= ANY; a constant's bound from its word, V4R-108): the recheck qual (`bitmapqualorig`,
                // the whole `= ANY`) decides the row
                if self.recheck.is_none() {
                    return Err(crate::pg_err("gravity: a search-array key under a substituted row without its recheck qual"));
                }
                continue;
            }
            if null {
                return Ok(false);
            }
            let key = &mut self.keys[i];
            self.fcinfo.rearm(key.sk_collation);
            // the exec context outlives this call; reset at unbind
            crate::ops::scan::raw::arm_result_mcx(&mut self.fcinfo, mcx);
            self.fcinfo.set_arg(0, d);
            self.fcinfo.set_arg(1, key.sk_argument);
            let r = key.sk_func.invoke(&mut self.fcinfo)?;
            if self.fcinfo.isnull || !r.as_bool() {
                return Ok(false);
            }
        }
        if let Some(f) = self.recheck.as_ref() {
            if !f(slot)? {
                return Ok(false);
            }
        }
        crate::ops::shared::heap_fetch::project(slot, &self.recipe.attnos, values, isnull)?;
        self.pos = ScanPos::row(pos.rel, pos.block, pos.off);
        Ok(true)
    }
}

impl Source for BitmapSource {
    fn bind(&mut self, b: &BindingRef<'_>) -> Result<()> {
        self.scratch.home_statement();
        self.bind_recheck(b).map_err(crate::stash)?;
        if let Err(e) = self.bind_pg(snapshot_of(b)) {
            // a host whose bind fails is never unbound (driver `Node::apply` unbinds the hosts that bound) and its Node
            // parks (`Entry::release`): the qual this bind took goes now, with the execution's context still live
            self.drop_entry_recheck();
            return Err(crate::stash(e));
        }
        self.held.home(HeldCharge::of_statement());
        self.held.sync(self.kept() as u64);
        Ok(())
    }

    fn seek(&mut self, keys: &[Word], keys_null: &[u8]) -> Result<()> {
        self.pos = ScanPos::NONE;
        if let Some(t) = self.tree.as_mut() {
            self.open = t.seek(keys, keys_null).map_err(crate::stash)?;
            if !self.open {
                self.substituted = None;
            }
            if self.open && self.substituted.is_none() {
                self.build_bitmap().map_err(crate::stash)?;
            }
            return Ok(());
        }
        // the recipe's words (a null test has none, a row comparison one per member), as IndexSource binds them
        if !crate::ops::scan::bitmap_window::bind_words(&self.recipe, &mut self.keys, &mut self.rows, keys, keys_null, &mut self.seek_images, &mut true).map_err(crate::stash)? {
            self.open = false;
            self.substituted = None;
            return Ok(());
        }
        if self.substituted.is_some() {
            self.open = true;
            return Ok(());
        }
        self.build_bitmap().map_err(crate::stash)?;
        self.open = true;
        Ok(())
    }

    /// The row as a 1-row window (`IndexSource::window`), or the page as one (M2's `PageBatch`).
    fn window(&mut self, w: &mut Window<'_>) -> Result<u32> {
        if self.held.home(HeldCharge::owner_of(w)) {
            self.held.sync(self.kept() as u64);
        }
        if self.page.is_some() && self.pages && self.substituted.is_none() {
            let r = self.window_page(w).map_err(crate::stash);
            self.held.sync(self.kept() as u64);
            return r;
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
        if let Some(b) = self.page.as_mut() {
            b.one(self.pos, self.recipe.attnos.len(), w);
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
        if !self.open {
            return Ok(false);
        }
        if let Some(pos) = self.substituted.take() {
            self.open = false;
            return self.next_substituted(pos, values, isnull).map_err(crate::stash);
        }
        let mcx = self.scratch.exec_mcx();
        // a tick per row visited, dropped or passed, as the index walk's (a row is a heap fetch, microseconds: a per-unit
        // tick)
        let mut step = cancelpoll::step("bitmap_source next: the bitmap walk's next row");
        loop {
            let (hscan, slot) = (self.hscan.as_mut().expect("bound"), self.slot.as_mut().expect("bound"));
            let at = (self.exact_pages, self.lossy_pages);
            let found = tableam::table_scan_bitmap_next_tuple(mcx, hscan, self.tbm.as_ref(), &mut self.iter, slot, &mut self.page_recheck, &mut self.lossy_pages, &mut self.exact_pages).map_err(crate::stash)?;
            self.recipe.bitmap().pages(at, (self.exact_pages, self.lossy_pages));
            if !found {
                self.open = false;
                return Ok(false);
            }
            step.tick()?;
            if self.page_recheck {
                if let Some(f) = self.recheck.as_ref() {
                    if !f(slot).map_err(crate::stash)? {
                        self.recipe.bitmap().add(crate::instr::BitmapCounts::REMOVED, 1);
                        continue;
                    }
                }
            }
            // D2988: the scan's qual pre-check (PageBatch::fill's)
            if let Some(f) = self.recipe.filter.as_ref().filter(|_| self.pre.on()) {
                if !self.pre.check(f(slot)).map_err(crate::stash)? {
                    self.recipe.bitmap().add(crate::instr::BitmapCounts::FILTERED, 1);
                    continue;
                }
            }
            crate::ops::shared::heap_fetch::project(slot, &self.recipe.attnos, values, isnull).map_err(crate::stash)?;
            self.pos = pos_of(self.recipe.heap, &slot.base().tts_tid);
            return Ok(true);
        }
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
    }

    fn unbind(&mut self) {
        self.unbind_pg().expect("gravity: bitmap source unbind");
        // the qual was compiled in the execution's context, which ends with the statement (a parked Node keeps none)
        self.drop_entry_recheck();
        self.held.park(self.kept() as u64);
    }

    /// D4465's crash (SIGSEGV at session teardown, `Park::clear` dropping a parked Node's qual whose context was gone): a
    /// Node parks (`Node::park`: the park's, and the slot depot's worker sets) with sources its last execution left holding
    /// a qual without an unbind: one whose bind failed (`Entry::release` parks a Node whose `apply` failed; the unbind
    /// covers only the hosts that bound), one the execution never bound, the qual it was derived with. A source at a
    /// registry entry drops it here too, so no parked Node holds a qual of an execution's context on any path.
    fn park(&mut self) {
        self.drop_entry_recheck();
    }

    /// D4464 + C5: the slot depot's hook: the kept buffers' charge and the source's scratch to `to` (the depot's holder
    /// at a worker set's park, the taking statement at its take).
    fn transfer_held(&mut self, to: memledger::Owner, release: memledger::Release) {
        self.held.transfer(to, release);
        self.scratch.home(to);
    }

    fn kept_bytes(&self) -> usize {
        self.kept()
    }
}

impl Drop for BitmapSource {
    fn drop(&mut self) {
        let _ = self.unbind_pg();
        self.slot = None;
    }
}
