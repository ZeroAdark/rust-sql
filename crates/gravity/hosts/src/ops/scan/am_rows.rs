//! The rows road's host: a range site whose relation's rows come through its table AM's slot scan, as the row
//! executor's SeqScan reads them. Serial on the leader: the scan is this backend's. Pushed conjuncts only let an AM that
//! filters skip rows that cannot pass; the program still evaluates the whole qual on every row it gets.
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::Arc;

use datum::Datum;
use gravity_abi::ctx::lane;
use gravity_abi::{Datum as Word, Lane, ScanPos};
use gravity_driver::mem::root::Root;
use gravity_driver::{box_source, ArenaRef, BindingRef, Cursor, Error, Result, Source, SourceBox, Window, WindowBase};
use gravity_decompose::ops::scan::identity::{self, Need};
use gravity_program::{LaneId, SiteDesc, SiteKind, ZoneOp};
use tablesample::Tsm;
use tableam_vocab::{ScanRoad, ZoneCmp, ZoneQual, HEAP_TABLE_AM_OID};
use types_core::{BlockNumber, Oid};
use types_error::PgResult;
use types_nodes::parsenodes::RTEKind;
use types_nodes::plannodes::PlannedStmt;
use types_nodes::CmdType;
use types_rel::{Relation, RelationData, RELKIND_MATVIEW, RELKIND_RELATION};
use types_scan::sdir::ScanDirection;
use types_slot::SlotData;
use types_snapshot::SnapshotData;
use types_storage::lock::NoLock;
use types_tuple::{ItemPointerData, ItemPointerGetBlockNumberNoCheck, ItemPointerGetOffsetNumberNoCheck};

use crate::ops::scan::heap_source::SampleFilter;
use crate::ops::scan::heap_window::{PAGE_ROWS, PAGE_VALIDITY_WORDS};
use crate::ops::scan::tid_sample::{SampleSeed, TsmFilter};
use crate::ops::shared::heap_fetch::{fetch_any, project_by, Proj, SELF_ITEM_POINTER};
use crate::ops::shared::rel::{open_heap, park, snapshot_of, tid_of};
use crate::recipe::HostRecipe;
use crate::scratch::NodeScratch;

/// The twin lever: `on` sends every user heap relation a statement scans down the rows road. A changed value moves the
/// program epoch (pgrc residency.rs `road_watch`).
pub const ROWS_ROAD_GUC: &str = "pgrust.gravity_rows_road";

const ROWS_CANCELED: &str = "gravity: a rows road scan stopped for its statement's cancel";

leader_tls::leader_tls! {
    thread_local! {
        static ROWS_SLOT: Cell<crate::inputs::OptionSlot> = const { Cell::new(crate::inputs::OptionSlot::Unknown) };
        static FORCED: Cell<(u64, bool)> = const { Cell::new((u64::MAX, false)) };
        static ROADS: RefCell<Option<Scoped>> = const { RefCell::new(None) };
    }
}

pub fn forced_now() -> bool {
    let v = ROWS_SLOT.with(|slot| crate::inputs::custom_guc_text_at(slot, ROWS_ROAD_GUC));
    v.is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "on" | "true" | "1" | "yes"))
}

pub fn forced() -> bool {
    let at = crate::inputs::guc_store_mutations();
    FORCED.with(|c| {
        let (seen, on) = c.get();
        if seen == at {
            return on;
        }
        let on = forced_now();
        c.set((at, on));
        on
    })
}

/// Empty for a statement over heap relations alone (nothing asked), so the front key and the plan memo of such a
/// statement are as before.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Roads(Vec<(u32, bool)>, Vec<(u32, Need)>);

impl Roads {
    pub fn choose<'p>(mcx: mcx::Mcx<'_>, pstmt: &'p PlannedStmt<'p>) -> PgResult<Roads> {
        let mut out: Vec<(u32, bool)> = Vec::new(); // alloc: Exec
        for relid in Roads::asked(pstmt)? { // bounded("catalog-sized: the plain relations Roads::asked lists")
            out.push((relid, tableam::table_scan_road(mcx, relid)? == ScanRoad::Rows)); // alloc: Program
        }
        let ident = identity::relids(pstmt).into_iter().filter(|r| out.iter().any(|&(x, _)| x == r.0)).collect(); // alloc: Program
        Ok(Roads(out, ident))
    }

    /// Whether `pstmt` reads a relation whose road is asked (`choose`): its refusal under the fallback off is never held
    /// for the plan's next execution (runner admission.rs `remember_off_refusal`), whose answers may differ.
    pub fn asks(pstmt: &PlannedStmt<'_>) -> PgResult<bool> {
        Ok(!Roads::asked(pstmt)?.is_empty())
    }

    /// An INSERT's target is written, not read, so its road is never asked; an UPDATE, DELETE or MERGE target is read,
    /// whatever the statement's own command.
    fn asked(pstmt: &PlannedStmt<'_>) -> PgResult<Vec<u32>> {
        let written = insert_targets(pstmt);
        let mut out: Vec<u32> = Vec::new(); // alloc: Exec
        for (i, n) in pstmt.rtable.iter().enumerate() { // bounded("catalog-sized: the statement's range table entries")
            let Some(rte) = n.as_range_tbl_entry() else { continue };
            if rte.rtekind != RTEKind::RTE_RELATION || rte.relid == 0 || !matches!(rte.relkind, RELKIND_RELATION | RELKIND_MATVIEW) || out.contains(&rte.relid) {
                continue;
            }
            if written.contains(&(i as u32 + 1)) {
                continue;
            }
            if lsyscache::relation::get_rel_relam(rte.relid)? != HEAP_TABLE_AM_OID {
                out.push(rte.relid); // alloc: Program
            }
        }
        Ok(out)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn rows(&self, relid: u32) -> bool {
        self.0.iter().any(|&(r, rows)| r == relid && rows) || self.1.iter().any(|x| x.0 == relid)
    }

    /// Whether every answer stands now, each relation asked again under the active snapshot: a program admitted under
    /// these answers serves again only so (the plan memo, the shelf, a rearm, bind's snapshot; `Backend::serve_stale`).
    /// An error asking is a change: the admission meets it again.
    pub fn hold(&self) -> bool {
        if self.0.is_empty() {
            return true;
        }
        let ctx = Root::new("GravityRoads");
        self.0.iter().all(|&(relid, rows)| tableam::table_scan_road(ctx.mcx(), relid).is_ok_and(|r| (r == ScanRoad::Rows) == rows))
    }
}

/// An INSERT reads no row of its target (pgrcolumnar refuses ON CONFLICT). The same relation read under another
/// range-table entry (`INSERT INTO t SELECT ... FROM t`) is read there.
fn insert_targets(pstmt: &PlannedStmt<'_>) -> Vec<u32> {
    let mut out: Vec<u32> = Vec::new(); // alloc: Exec
    for m in pstmt.planTree.into_iter().chain(pstmt.subplans.iter().flatten()).filter_map(|n| n.as_modify_table()) { // bounded("catalog-sized: the plan's ModifyTable nodes")
        if m.operation == CmdType::CMD_INSERT {
            out.extend(m.resultRelations.iter().map(|r| r as u32).chain((m.rootRelation != 0).then_some(m.rootRelation))); // alloc: Program
        }
    }
    out
}

struct Scoped {
    roads: Roads,
    shaped: Vec<u32>,
    /// `shape_rows` added a relation since `take_reshaped` last looked.
    reshaped: bool,
}

/// PGRCSERVE G5: `roads` are the admission's answers until the scope drops, which restores the scope it replaced (an
/// enclosing statement's admission).
pub struct RoadScope(Option<Scoped>);

impl RoadScope {
    pub fn enter(roads: Roads) -> RoadScope {
        RoadScope(ROADS.with(|r| r.replace(Some(Scoped { roads, shaped: Vec::new(), reshaped: false })))) // alloc: Exec
    }
}

impl Drop for RoadScope {
    fn drop(&mut self) {
        let outer = self.0.take();
        ROADS.with(|r| *r.borrow_mut() = outer);
    }
}

pub fn scoped_rows(relid: u32) -> bool {
    ROADS.with(|r| r.borrow().as_ref().is_some_and(|x| x.roads.rows(relid) || x.shaped.contains(&relid)))
}

/// The answers of the admission running now into `h` (the front key), nothing when there are none. The shaped
/// relations (D2) are not keyed: they follow from the plan and the answers, which the key holds, so the program a
/// shaped lowering files under the key is the one every admission of that key lowers.
pub fn hash_scoped<H: core::hash::Hasher>(h: &mut H) {
    ROADS.with(|r| {
        if let Some(x) = r.borrow().as_ref().filter(|x| !x.roads.is_empty()) {
            core::hash::Hash::hash(&x.roads, h);
        }
    })
}

/// Decided at the admission's compile from the lowering's named refusal (never a retry of a failed run): true when it
/// was asked and not on the rows road yet.
pub fn shape_rows(relid: u32) -> bool {
    ROADS.with(|r| {
        let mut r = r.borrow_mut();
        let Some(x) = r.as_mut() else { return false };
        let native = x.roads.0.iter().any(|&(rel, _)| rel == relid) && !x.roads.rows(relid);
        if !native || x.shaped.contains(&relid) {
            return false;
        }
        x.shaped.push(relid); // alloc: Program
        x.reshaped = true;
        true
    })
}

pub fn take_reshaped() -> bool {
    ROADS.with(|r| r.borrow_mut().as_mut().is_some_and(|x| core::mem::replace(&mut x.reshaped, false)))
}

/// PGRCSERVE M3 (§2 row 6): the TIDs a rows-road scan of `relid` asks its AM for: an identity relation's need, a native
/// shape's every position, else none (no row map loads).
fn positions(relid: u32) -> tableam::ScanTidNeed {
    use tableam::ScanTidNeed as T;
    ROADS.with(|r| match r.borrow().as_ref() {
        None => T::Direct,
        Some(x) => match x.roads.1.iter().find(|i| i.0 == relid) {
            Some((_, Need::Dml)) => T::Dml,
            Some((_, Need::Lock)) => T::Lock,
            Some((_, Need::Epq)) => T::Epq,
            Some((_, Need::Ctid)) => T::Ctid,
            None if x.shaped.contains(&relid) => T::Direct,
            None => T::None,
        },
    })
}

/// Each column a datum word or (`lens[k]` not 0) a by-reference image of that `typlen` (-1 a varlena, -2 a cstring,
/// above 0 a fixed length) the window copies.
#[derive(Clone)]
pub struct AmRowsRecipe {
    pub heap: Oid,
    heap_natts: i32,
    pub attnos: Arc<[i16]>,
    lens: Arc<[i16]>,
    pub node_id: i32,
    quals: Arc<[ZoneQual]>,
    tid: tableam::ScanTidNeed,
    /// D4735: a TABLESAMPLE on the rows road: its method's handler (0: none), whether a REPEATABLE word follows and the plan
    /// node's identity (tid_sample.rs `SampleRecipe`'s), the AM's sample scan in place of its slot scan.
    tsm_handler: Oid,
    tsm_repeatable: bool,
    tsm_node: usize,
}

impl AmRowsRecipe {
    pub fn from_plan(heap: &Relation<'_>, attnos: &[i16], byref: &[u64]) -> PgResult<AmRowsRecipe> {
        if !matches!(heap.rd_rel.relkind, RELKIND_RELATION | RELKIND_MATVIEW) || tableam_vocab::TableAm::of(heap).is_none() {
            return Err(crate::pg_err("gravity: a rows road site needs a table with a table access method"));
        }
        let mut lens = Vec::with_capacity(attnos.len()); // alloc: Program
        for (k, &a) in attnos.iter().enumerate() { // bounded("key-columns: the site's read columns, at most MaxTupleAttributeNumber (1664)")
            let by_ref = byref.get(k / 64).is_some_and(|w| (w >> (k % 64)) & 1 != 0);
            let len = match a {
                _ if !by_ref => 0,
                SELF_ITEM_POINTER => core::mem::size_of::<ItemPointerData>() as i16,
                a if a > 0 => match heap.rd_att.attrs.get(a as usize - 1) {
                    Some(att) if !att.attbyval && att.attlen != 0 => att.attlen,
                    _ => return Err(crate::pg_err("gravity: a rows road site's by-reference lane over no by-reference column")),
                },
                _ => return Err(crate::pg_err("gravity: a rows road site's by-reference system lane")),
            };
            lens.push(len); // alloc: Program
        }
        Ok(AmRowsRecipe { heap: heap.rd_id, heap_natts: heap.rd_att.natts, attnos: Arc::from(attnos), lens: Arc::from(lens), node_id: -1, quals: Arc::from([]), tid: positions(heap.rd_id), tsm_handler: 0, tsm_repeatable: false, tsm_node: 0 }) // alloc: Program
    }

    /// D4735: the sample's recipe, its method resolved from the plan's `tsmhandler` (`Tsm::get`); `node` as
    /// `SampleRecipe::from_plan`'s.
    pub fn with_sample(self, mcx: mcx::Mcx<'_>, handler: Oid, repeatable: bool, node: usize) -> PgResult<AmRowsRecipe> {
        Tsm::get(mcx, handler)?;
        Ok(AmRowsRecipe { tsm_handler: handler, tsm_repeatable: repeatable, tsm_node: node, ..self })
    }

    /// Into an execution's registry: a sample's with its plan node's one seed there (D1920 / D1644).
    pub fn bound(self, reg: &mut crate::HostRegistry) -> AmRowsBound {
        AmRowsBound { seed: (self.tsm_handler != 0).then(|| reg.sample_seed(self.tsm_node)), rows: self }
    }

    fn source(&self, desc: &SiteDesc, seed: Option<SampleSeed>) -> AmRowsSource {
        assert!(self.heap == desc.relation, "gravity: rows road recipe does not match its site descriptor");
        let zkeys = match desc.kind {
            SiteKind::RangeSource { zkeys, .. } => zkeys as usize,
            _ => 0,
        };
        let mut src = AmRowsSource::new(self.clone());
        src.zkeys = zkeys;
        src.tsm_seed = seed;
        src
    }

    pub fn at_node(self, node_id: i32) -> AmRowsRecipe {
        AmRowsRecipe { node_id, ..self }
    }

    pub fn with_quals(&self, quals: Vec<ZoneQual>) -> AmRowsRecipe {
        AmRowsRecipe { quals: Arc::from(quals), ..self.clone() } // alloc: Exec
    }

    fn needed(&self) -> Vec<bool> {
        let mut n = vec![false; self.heap_natts.max(0) as usize]; // alloc: Exec
        for &a in self.attnos.iter().filter(|&&a| a > 0) { // bounded("key-columns: the site's read columns, at most MaxTupleAttributeNumber (1664)")
            if let Some(x) = n.get_mut(a as usize - 1) {
                *x = true;
            }
        }
        n
    }
}

/// A rows-road recipe bound into an execution (`AmRowsRecipe::bound`): D4735, a sample's with its plan node's seed.
pub struct AmRowsBound {
    rows: AmRowsRecipe,
    seed: Option<SampleSeed>,
}

impl Clone for AmRowsBound {
    fn clone(&self) -> AmRowsBound {
        let seed = self.seed.as_ref().map(|s| s.clone());
        AmRowsBound { rows: self.rows.clone(), seed }
    }
}

impl HostRecipe for AmRowsBound {
    fn rel(&self) -> Oid {
        self.rows.heap
    }
    fn size_hint(&self) -> usize {
        core::mem::size_of::<AmRowsSource>()
    }
    fn derive(&self, desc: &SiteDesc, a: ArenaRef) -> SourceBox {
        let seed = self.seed.as_ref().map(|s| s.clone());
        box_source(self.rows.source(desc, seed), a) // alloc: Node
    }
    fn clone_box(&self) -> Box<dyn HostRecipe> {
        Box::new(self.clone()) // alloc: Program
    }
}

pub struct AmRowsSource {
    recipe: AmRowsRecipe,
    proj: Proj,
    heap: Option<Relation<'static>>,
    parked: Option<Weak<RelationData<'static>>>,
    snapshot: Option<Rc<SnapshotData<'static>>>,
    slot: Option<SlotData<'static>>,
    scan: Option<tableam::TableScanDesc<'static>>,
    /// The slot holds the scan's next row, not yet published (it opened another block: the next window starts with it).
    held: bool,
    open: bool,
    bound: bool,
    substituted: Option<ScanPos>,
    /// Lane `k`'s words at `k * PAGE_ROWS`, its validity words at `k * PAGE_VALIDITY_WORDS`; the rows' offsets.
    words: Vec<Word>,
    valid: Vec<u64>,
    offs: Vec<u16>,
    row: Vec<Word>,
    nulls: Vec<u8>,
    /// The window's by-reference images, reset at every window.
    images: Root,
    cur: Cursor,
    scratch: NodeScratch,
    zkeys: usize,
    params: Vec<ZoneQual>,
    pushed: Vec<ZoneQual>,
    /// D4735: a TABLESAMPLE's sampler, which drives the AM's sample scan, and whether its current block has rows left.
    sample: Option<TsmFilter>,
    in_block: bool,
    /// D4735: the plan node's seed the sampler takes at its first seek (`AmRowsBound`).
    tsm_seed: Option<SampleSeed>,
}

/// PSZP: lowerv4 heap.rs `rows_param_desc` read back as `(attnum, op, typlen, type)`; `None` when the AM has no word.
pub fn rows_param_of(desc: u64) -> Option<(u16, ZoneCmp, i32, u32)> {
    let op = match (desc >> 16) as u8 {
        x if x == ZoneOp::Lt as u8 => ZoneCmp::Lt,
        x if x == ZoneOp::Le as u8 => ZoneCmp::Le,
        x if x == ZoneOp::Eq as u8 => ZoneCmp::Eq,
        x if x == ZoneOp::Ge as u8 => ZoneCmp::Ge,
        x if x == ZoneOp::Gt as u8 => ZoneCmp::Gt,
        x if x == ZoneOp::Ne as u8 => ZoneCmp::Ne,
        _ => return None,
    };
    Some((desc as u16, op, (desc >> 24) as u8 as i32, (desc >> 32) as u32))
}

fn param_quals(keys: &[Word], keys_null: &[u8], zkeys: usize, out: &mut Vec<ZoneQual>) {
    use gravity_decompose::ops::shared::scan::zone_word_of;
    out.clear();
    let Some(at) = keys.len().checked_sub(zkeys).filter(|_| keys_null.len() >= keys.len()) else { return };
    for c in (at..keys.len().saturating_sub(1)).step_by(2) { // bounded("key-columns: the seek's zone key cells, two per zone column of the relation")
        let (Some((attnum, op, len, typid)), 0) = (rows_param_of(keys[c]), keys_null[c + 1]) else { continue };
        if let Some(val) = zone_word_of(typid, len, keys[c + 1]) {
            out.push(ZoneQual { attnum, op, val }); // alloc: Exec
        }
    }
}

impl AmRowsSource {
    pub fn new(recipe: AmRowsRecipe) -> AmRowsSource {
        let n = recipe.attnos.len();
        let proj = Proj::of(&recipe.attnos);
        AmRowsSource {
            recipe,
            proj,
            heap: None,
            parked: None,
            snapshot: None,
            slot: None,
            scan: None,
            held: false,
            open: false,
            bound: false,
            substituted: None,
            words: vec![0; n * PAGE_ROWS],          // alloc: Node
            valid: vec![0; n * PAGE_VALIDITY_WORDS], // alloc: Node
            offs: vec![0; PAGE_ROWS],               // alloc: Node
            row: vec![0; n],                        // alloc: Node
            nulls: vec![0; n],                      // alloc: Node
            images: Root::bump("GravityAmRowsImages"),
            cur: Cursor::new(),
            scratch: NodeScratch::new("GravityAmRowsSource"),
            zkeys: 0,
            params: Vec::new(), // alloc: Node
            pushed: Vec::new(), // alloc: Node
            sample: None,
            in_block: false,
            tsm_seed: None,
        }
    }

    fn bind_pg(&mut self, snapshot: Rc<SnapshotData<'static>>) -> PgResult<()> {
        let mcx = self.scratch.mcx();
        let heap = open_heap(mcx, &self.parked, self.recipe.heap)?;
        if heap.rd_att.natts != self.recipe.heap_natts || tableam_vocab::TableAm::of(&heap).is_none() || !heap.is_scannable() {
            heap.close(NoLock)?;
            return Err(crate::pg_err("gravity: relation changed under the parked plan"));
        }
        self.slot = Some(match self.slot.take() {
            Some(s) => s,
            None => tableam::table_slot_create(mcx, &heap)?,
        });
        (self.heap, self.snapshot, self.open, self.held, self.bound) = (Some(heap), Some(snapshot), false, false, true);
        Ok(())
    }

    fn unbind_pg(&mut self) -> PgResult<()> {
        (self.open, self.held, self.substituted) = (false, false, None);
        if !std::mem::replace(&mut self.bound, false) {
            return Ok(());
        }
        if let Some(slot) = self.slot.as_mut() {
            exectuples::exec_clear_tuple(slot, self.scratch.mcx());
        }
        // the scan lives in the exec context the unbind resets
        self.scan.take().map(tableam::table_endscan).transpose()?;
        (self.parked, self.snapshot) = (park(self.heap.take())?, None);
        self.images.reset();
        self.scratch.reset_exec();
        Ok(())
    }

    fn start(&mut self) -> PgResult<()> {
        let mcx = self.scratch.exec_mcx();
        if let Some(slot) = self.slot.as_mut() {
            exectuples::exec_clear_tuple(slot, self.scratch.mcx());
        }
        // D4735: a sample's AM scan takes the flags its method's begin chose (the sampler began at this seek)
        let flags = self.sample.as_ref().map(TsmFilter::am_flags);
        match (self.scan.as_mut(), flags) {
            (Some(s), None) => tableam::table_rescan(mcx, s, None)?,
            (Some(s), Some((strat, sync, page))) => tableam::table_rescan_set_params(mcx, s, None, strat, sync, page)?,
            (None, _) => {
                let heap = self.heap.as_ref().ok_or_else(|| crate::pg_err("gravity: seek on an unbound rows road source"))?;
                let snapshot = self.snapshot.as_ref().map(|s| s.clone());
                let mut s = match flags {
                    None => tableam::table_beginscan(mcx, heap, snapshot, 0, mcx::PgVec::new_in(mcx))?,
                    Some((strat, sync, page)) => tableam::table_beginscan_sampling(mcx, heap, snapshot, 0, mcx::PgVec::new_in(mcx), strat, sync, page)?,
                };
                tableam::table_scan_set_needed_attrs(&mut s, &self.recipe.needed());
                tableam::table_scan_set_tid_need(&mut s, self.recipe.tid);
                self.scan = Some(s);
            }
        }
        // PGRCSERVE M0: the pushed conjuncts, at each pass (a rescan drops them); PSZP: this seek's param ones too
        self.pushed.clear();
        self.pushed.extend(self.recipe.quals.iter().chain(self.params.iter()).copied()); // alloc: Exec
        if let (Some(s), false) = (self.scan.as_mut(), self.pushed.is_empty()) {
            tableam::table_scan_push_zone_quals(s, &self.pushed);
            tableam::table_scan_push_quals(s, &self.pushed);
        }
        (self.open, self.held, self.in_block) = (true, false, false);
        Ok(())
    }

    fn put(&mut self, i: usize, tid: &ItemPointerData) -> PgResult<()> {
        let slot = self.slot.as_mut().ok_or_else(|| crate::pg_err("gravity: a rows road window without its slot"))?;
        project_by(slot, &self.recipe.attnos, self.proj, &mut self.row, &mut self.nulls)?;
        let images = self.images.mcx();
        for (k, &len) in self.recipe.lens.iter().enumerate() { // bounded("key-columns: one row's read columns, at most 1664; window_pg's loop ticks per row")
            let mut v = self.row[k];
            if self.nulls[k] == 0 {
                if len != 0 {
                    v = adt_scalar::datum_ops::datum_copy(images, Datum::from_u64(v), false, len)?.as_u64();
                }
                self.valid[k * PAGE_VALIDITY_WORDS + i / 64] |= 1 << (i % 64);
            }
            self.words[k * PAGE_ROWS + i] = v;
        }
        self.offs[i] = ItemPointerGetOffsetNumberNoCheck(tid);
        Ok(())
    }

    fn publish(&self, w: &mut Window<'_>, n: usize, blk: BlockNumber) -> u32 {
        let natts = self.recipe.attnos.len();
        for k in 0..natts { // bounded("key-columns: the window's read columns, at most MaxTupleAttributeNumber (1664)")
            let words = &self.valid[k * PAGE_VALIDITY_WORDS..(k + 1) * PAGE_VALIDITY_WORDS];
            let all = (0..n).all(|r| (words[r / 64] >> (r % 64)) & 1 != 0);
            let mut l = Lane::byval(self.words[k * PAGE_ROWS..].as_ptr() as *const u8, if all { core::ptr::null() } else { words.as_ptr() }, 8, lane::REGION_ARENA, lane::id(lane::ID_ATTR, self.recipe.attnos[k] as u32));
            if self.recipe.lens[k] != 0 {
                l.kind = lane::KIND_BYREF;
            }
            w.set(k, l);
        }
        w.set(natts, Lane::byval(self.offs.as_ptr() as *const u8, core::ptr::null(), 2, lane::REGION_ARENA, LaneId::Offset.encode()));
        w.rows = n as u32;
        w.set_base(WindowBase { pos: ScanPos::new(self.recipe.heap, blk, 0), part_id: 0, dict_epoch: 0, granule: 0, claim_id: 0 });
        n as u32
    }

    fn clear(&mut self) {
        self.images.reset();
        self.valid.fill(0);
    }

    fn window_pg(&mut self, w: &mut Window<'_>) -> PgResult<u32> {
        if let Some(pos) = self.substituted.take() {
            return self.window_substituted(pos, w);
        }
        if !self.open {
            return Ok(0);
        }
        if self.sample.is_some() {
            return self.window_sampled(w);
        }
        self.clear();
        let mcx = self.scratch.exec_mcx();
        let (mut n, mut blk) = (0usize, 0 as BlockNumber);
        let mut step = cancelpoll::step("am_rows window_pg: a fetched row");
        loop {
            if !self.held {
                step.tick().map_err(|_| crate::pg_err(ROWS_CANCELED))?;
                let (Some(scan), Some(slot)) = (self.scan.as_mut(), self.slot.as_mut()) else { return Err(crate::pg_err("gravity: a rows road window on an unbound source")) };
                if !tableam::table_scan_getnextslot(mcx, scan, ScanDirection::ForwardScanDirection, slot)? {
                    self.open = false;
                    break;
                }
                self.held = true;
            }
            let tid = self.slot.as_ref().map_or(ItemPointerData::invalid(), |s| s.base().tts_tid);
            let b = ItemPointerGetBlockNumberNoCheck(&tid);
            if n > 0 && (b != blk || n == PAGE_ROWS) {
                break;
            }
            blk = b;
            self.put(n, &tid)?;
            self.held = false;
            n += 1;
        }
        Ok(if n == 0 { 0 } else { self.publish(w, n, blk) })
    }

    /// D4735: the sampled rows of one block as a window, the AM's sample scan driven by the sampler (nodeSamplescan.c's
    /// tablesample_getnext): the method picks the blocks and the offsets, the AM keeps a picked offset when visible.
    fn window_sampled(&mut self, w: &mut Window<'_>) -> PgResult<u32> {
        self.clear();
        let mcx = self.scratch.exec_mcx();
        let (mut n, mut blk) = (0usize, 0 as BlockNumber);
        let mut step = cancelpoll::step("am_rows window_sampled: a sampled row");
        loop {
            step.tick().map_err(|_| crate::pg_err(ROWS_CANCELED))?;
            let (Some(scan), Some(slot), Some(f)) = (self.scan.as_mut(), self.slot.as_mut(), self.sample.as_mut()) else { return Err(crate::pg_err("gravity: a rows road sample on an unbound source")) };
            if !self.in_block {
                if n > 0 {
                    break;
                }
                if !f.am_block(mcx, scan)? {
                    self.open = false;
                    break;
                }
                self.in_block = true;
            }
            if n == PAGE_ROWS {
                break;
            }
            if !f.am_tuple(mcx, scan, slot)? {
                self.in_block = false;
                continue;
            }
            let tid = slot.base().tts_tid;
            blk = ItemPointerGetBlockNumberNoCheck(&tid);
            self.put(n, &tid)?;
            n += 1;
        }
        Ok(if n == 0 { 0 } else { self.publish(w, n, blk) })
    }

    /// The EPQ substitution: the version at `pos` as a one-row window at its own position, any visibility; the walk ends
    /// after it.
    fn window_substituted(&mut self, pos: ScanPos, w: &mut Window<'_>) -> PgResult<u32> {
        (self.open, self.held) = (false, false);
        if pos.rel != self.recipe.heap || !pos.is_valid() || pos.off == 0 {
            return Ok(0);
        }
        self.clear();
        let tid = tid_of(pos);
        let mcx = self.scratch.exec_mcx();
        let (Some(heap), Some(slot)) = (self.heap.as_ref(), self.slot.as_mut()) else { return Err(crate::pg_err("gravity: a rows road substitution on an unbound source")) };
        if !fetch_any(mcx, heap, &tid, slot)? {
            return Ok(0);
        }
        self.put(0, &tid)?;
        Ok(self.publish(w, 1, ItemPointerGetBlockNumberNoCheck(&tid)))
    }
}

impl Source for AmRowsSource {
    fn bind(&mut self, b: &BindingRef<'_>) -> Result<()> {
        self.bind_pg(snapshot_of(b)).map_err(crate::stash)
    }

    /// The claim words are ignored: the site is serial and walks the whole relation at each open.
    fn seek(&mut self, keys: &[Word], keys_null: &[u8]) -> Result<()> {
        if !self.bound {
            return Err(Error::host("seek on an unbound rows road source"));
        }
        if self.substituted.is_some() {
            return Ok(());
        }
        if self.zkeys > 0 {
            param_quals(keys, keys_null, self.zkeys, &mut self.params);
        }
        // D4735: the method's argument and REPEATABLE words (the sampled site's first key cells) begin its sampler, made at
        // the first seek (its method resolved from the handler)
        if self.sample.is_none() && self.recipe.tsm_handler != 0 {
            let tsm = Tsm::get(self.scratch.mcx(), self.recipe.tsm_handler).map_err(crate::stash)?;
            self.sample = Some(TsmFilter::new(tsm, self.recipe.tsm_repeatable).sharing(self.tsm_seed.take()));
        }
        if let Some(f) = self.sample.as_mut() {
            f.start(keys, keys_null).map_err(crate::stash)?;
        }
        self.start().map_err(crate::stash)
    }

    fn window(&mut self, w: &mut Window<'_>) -> Result<u32> {
        self.window_pg(w).map_err(crate::stash)
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

    fn refetch(&mut self, _values: &mut [Word], _isnull: &mut [u8]) -> Result<bool> {
        Err(crate::unsupported())
    }

    fn substitute(&mut self, pos: ScanPos) {
        self.substituted = Some(pos);
    }

    fn unbind(&mut self) {
        self.unbind_pg().expect("gravity: rows road source unbind");
    }

    fn kept_bytes(&self) -> usize {
        use gravity_driver::vec_bytes as v;
        v(&self.words) + v(&self.valid) + v(&self.offs) + v(&self.row) + v(&self.nulls) + self.images.used()
    }
}

impl Drop for AmRowsSource {
    fn drop(&mut self) {
        let _ = self.unbind_pg();
        self.slot = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lever_reads_off_without_a_guc_store() {
        assert!(!forced_now());
        assert!(!forced());
    }

    fn keyed() -> u64 {
        use core::hash::Hasher;
        let mut h = std::collections::hash_map::DefaultHasher::new();
        hash_scoped(&mut h);
        h.finish()
    }

    #[test]
    fn a_road_scope_restores_the_one_it_replaced_and_keys_only_answers() {
        let none = keyed();
        assert!(!scoped_rows(7));
        let outer = RoadScope::enter(Roads(vec![(7, true), (8, false)], vec![])); // alloc: Cold
        assert!(scoped_rows(7) && !scoped_rows(8));
        assert_ne!(keyed(), none);
        {
            let _inner = RoadScope::enter(Roads::default());
            assert!(!scoped_rows(7));
            assert_eq!(keyed(), none);
        }
        assert!(scoped_rows(7));
        drop(outer);
        assert!(!scoped_rows(7));
        assert_eq!(keyed(), none);
        assert!(Roads::default().hold());
    }

    #[test]
    fn a_shape_moves_an_asked_native_relation_down_the_rows_road_unkeyed() {
        assert!(!shape_rows(7) && !take_reshaped());
        let scope = RoadScope::enter(Roads(vec![(7, false), (8, true)], vec![])); // alloc: Cold
        let key = keyed();
        assert!(!scoped_rows(7) && scoped_rows(8));
        assert!(!shape_rows(8) && !shape_rows(9) && !take_reshaped());
        assert!(shape_rows(7));
        assert!(scoped_rows(7) && !shape_rows(7));
        assert!(take_reshaped() && !take_reshaped());
        assert_eq!(keyed(), key);
        drop(scope);
        assert!(!scoped_rows(7));
    }

    #[test]
    fn an_identity_relation_takes_the_rows_road_over_a_native_answer() {
        let scope = RoadScope::enter(Roads(vec![(7, false), (8, false)], vec![(7, Need::Dml)])); // alloc: Cold
        assert!(scoped_rows(7) && !scoped_rows(8));
        assert!(!shape_rows(7) && !take_reshaped());
        assert!(shape_rows(8) && take_reshaped());
        drop(scope);
        assert!(!scoped_rows(7));
    }
}
