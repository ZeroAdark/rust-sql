//! M2's TID and TABLESAMPLE hosts (cards/M2.md L3 `m2_tid_sample.rs`, lowerv3's R1b `TidSource` and R1c `TsmFilter`).
//! - `TidSource`: PostgreSQL's TidScan / TidRangeScan. At each seek the key cells become the sorted, deduplicated TID
//!   list (TidListEval: NULLs and TIDs outside the relation dropped), each fetched once under the snapshot (TidNext), or
//!   the range bounds (TidRangeEval: a NULL bound is no rows) walked by the heap's TID range scan. `WHERE CURRENT OF` asks
//!   the cursor's position (`execCurrentOf`) and fetches the newest version from it (`table_tuple_get_latest_tid`).
//!   EvalPlanQual rechecks the substituted TID against the list or the range (TidRecheck / TidRangeRecheck).
//! - `TsmFilter`: TABLESAMPLE over the heap range source (`heap_source.rs` `SampleFilter`): PostgreSQL's TSM picks the
//!   blocks and the offsets, a picked offset kept when visible; the percent and REPEATABLE are the site's key words.
//! Created by M2 (owner M2-HOSTS).
use std::rc::{Rc, Weak};

use gravity_abi::ctx::lane;
use gravity_abi::{Datum as Word, Lane, ScanPos};
use gravity_driver::{box_source, ArenaRef, BindingRef, Cursor, Error, Result, Source, SourceBox, Window, WindowBase};
use gravity_program::SiteDesc;
use tablesample::{Tsm, TsmState};
use types_core::{BlockNumber, InvalidBlockNumber, Oid};
use types_error::{PgError, PgResult, ERRCODE_INVALID_TABLESAMPLE_ARGUMENT, ERRCODE_INVALID_TABLESAMPLE_REPEAT, ERRCODE_UNDEFINED_OBJECT};
use types_rel::{Relation, RelationData};
use types_scan::sdir::ScanDirection;
use types_slot::SlotData;
use types_snapshot::SnapshotData;
use types_storage::lock::NoLock;
use types_tuple::{ItemPointerCompare, ItemPointerData, ItemPointerDec, ItemPointerInc};

use crate::ops::scan::heap_source::{HeapRangeSource, RangeRecipe, SampleFilter};
use crate::ops::scan::index_source::{heap_am, open_heap, park, pos_of, tid_of};
use crate::ops::shared::rel::snapshot_of;
use crate::recipe::HostRecipe;
use crate::scratch::NodeScratch;

use crate::ops::scan::tid_keys::{cursor_cell, tid_array_cell, tid_cell};

/// D3887: the error a TID list's sort or walk returns when it stopped for its statement's cancel (`crate::stash` reports
/// it as the kernel's cancel).
const TID_CANCELED: &str = "gravity: a TID scan stopped for its statement's cancel";

/// One TID clause, in the plan's order (its value is the key cell of the same index).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TidClause {
    Eq,
    Any,
    Lower { inclusive: bool },
    Upper { inclusive: bool },
    /// `WHERE CURRENT OF` (`CurrentOfExpr`): its key cell is the cursor, bound per execution (D422: a cached program serves
    /// every cursor): the refcursor parameter `param`'s value when `by_param`, else the address of the execution's
    /// `CurrentOfExpr` (its name; exec admit `WordTpl::Cursor`).
    Current { by_param: bool, param: i32 },
}

/// A TID site's recipe: the heap, the columns, the clauses; `range` = a TidRangeScan (every clause a bound).
#[derive(Clone)]
pub struct TidRecipe {
    pub heap: Oid, pub heap_natts: i32,
    pub attnos: Vec<i16>, pub clauses: Vec<TidClause>, pub range: bool,
}

/// PGRCSERVE M3: the TID source reads any carried table AM through tableam (another AM's on the rows road, its own TIDs).
fn tid_am(heap: &Relation<'_>) -> bool {
    heap_am(heap) || crate::ops::scan::index_source::index_fetch_am(heap)
}

fn needed_fetch(heap: &Relation<'static>, attnos: &[i16]) -> PgResult<tableam::IndexFetchTableData<'static>> {
    let mut fetch = tableam::table_index_fetch_begin(heap)?;
    let mut m = vec![false; heap.rd_att.natts as usize]; // alloc: Node
    // bounded("key-columns: one flag per read column, at most MaxTupleAttributeNumber (1664)")
    for &a in attnos {
        if let Some(x) = usize::try_from(a).ok().and_then(|a| a.checked_sub(1)).and_then(|i| m.get_mut(i)) {
            *x = true;
        }
    }
    tableam::table_index_fetch_set_needed_attrs(&mut fetch, Some(m.into_boxed_slice()));
    Ok(fetch)
}

impl TidRecipe {
    pub fn from_plan(heap: &Relation<'_>, attnos: &[i16], clauses: Vec<TidClause>, range: bool) -> PgResult<TidRecipe> {
        if !tid_am(heap) {
            return Err(crate::pg_err("gravity: a TID scan over a relation without a table access method"));
        }
        Ok(TidRecipe { heap: heap.rd_id, heap_natts: heap.rd_att.natts, attnos: attnos.to_vec(), clauses, range }) // alloc: Program
    }
    /// The site's key cells: one per clause.
    pub fn nkeys(&self) -> usize {
        self.clauses.len()
    }
}

impl HostRecipe for TidRecipe {
    fn rel(&self) -> Oid { self.heap }
    fn size_hint(&self) -> usize { core::mem::size_of::<TidSource>() }
    fn derive(&self, _desc: &SiteDesc, a: ArenaRef) -> SourceBox { box_source(TidSource::new(self.clone()), a) }
    fn clone_box(&self) -> Box<dyn HostRecipe> { Box::new(self.clone()) } // alloc: Program
}

pub struct TidSource {
    recipe: TidRecipe,
    heap: Option<Relation<'static>>,
    parked: Option<Weak<RelationData<'static>>>,
    snapshot: Option<Rc<SnapshotData<'static>>>,
    slot: Option<SlotData<'static>>,
    scan: Option<tableam::TableScanDesc<'static>>,
    /// Another AM's fetch state: its fetches decode only the columns the site reads (r13h's TidNext).
    fetch: Option<tableam::IndexFetchTableData<'static>>,
    /// The point list and the next point; `current` = it is a CURRENT OF cursor's row (followed to its newest version).
    tids: Vec<ItemPointerData>, at: usize, current: bool,
    lo: ItemPointerData, hi: ItemPointerData,
    substituted: Option<ScanPos>, pos: ScanPos, open: bool, bound: bool,
    row: Vec<Word>, row_valid: Vec<u64>, nbuf: Vec<u8>,
    cur: Cursor,
    scratch: NodeScratch,
}

impl TidSource {
    pub fn new(recipe: TidRecipe) -> TidSource {
        let natts = recipe.attnos.len();
        let z = ItemPointerData::new(0, 0);
        TidSource { recipe, heap: None, parked: None, snapshot: None, slot: None, scan: None, fetch: None, tids: Vec::new(), at: 0, current: false, lo: z, hi: z, substituted: None, pos: ScanPos::NONE, open: false, bound: false, row: vec![0; natts], row_valid: vec![0; natts], nbuf: vec![0; natts], cur: Cursor::new(), scratch: NodeScratch::new("GravityTidSource") } // alloc: Node
    }
    fn bind_pg(&mut self, snapshot: Rc<SnapshotData<'static>>) -> PgResult<()> {
        let mcx = self.scratch.mcx();
        let heap = open_heap(mcx, &self.parked, self.recipe.heap)?;
        if heap.rd_att.natts != self.recipe.heap_natts || !tid_am(&heap) || !heap.is_scannable() {
            heap.close(NoLock)?;
            return Err(crate::pg_err("gravity: relation changed under the parked plan"));
        }
        self.slot = Some(match self.slot.take() {
            Some(s) => s,
            None => tableam::table_slot_create(mcx, &heap)?,
        });
        // the point scan is per bind (its snapshot is the binding's); the range scan starts at the first seek. Both live in
        // the exec context the unbind resets (D3195: a worker slot binds every host at each section's open)
        self.scan = if self.recipe.range { None } else { Some(tableam::table_beginscan_tid(self.scratch.exec_mcx(), &heap, Some(snapshot.clone()))?) };
        self.fetch = if heap_am(&heap) || self.recipe.range { None } else { Some(needed_fetch(&heap, &self.recipe.attnos)?) };
        (self.heap, self.snapshot, self.pos, self.open, self.bound) = (Some(heap), Some(snapshot), ScanPos::NONE, false, true);
        Ok(())
    }
    fn unbind_pg(&mut self) -> PgResult<()> {
        (self.open, self.substituted) = (false, None);
        if !std::mem::replace(&mut self.bound, false) {
            return Ok(());
        }
        if let Some(slot) = self.slot.as_mut() {
            exectuples::exec_clear_tuple(slot, self.scratch.mcx());
        }
        self.scan.take().map(tableam::table_endscan).transpose()?;
        if let Some(f) = self.fetch.take() {
            tableam::table_index_fetch_end(f);
        }
        (self.parked, self.snapshot) = (park(self.heap.take())?, None);
        self.scratch.reset_exec();
        Ok(())
    }
    /// TidRangeEval: the bounds tightened over the clauses; false = no rows (a NULL bound).
    fn eval_range(&mut self, keys: &[Word], keys_null: &[u8]) -> PgResult<bool> {
        let (mut lo, mut hi) = (ItemPointerData::new(0, 0), ItemPointerData::new(InvalidBlockNumber, u16::MAX));
        // bounded("other: one bound per TID clause of the scan, which the statement's quals bound")
        for (i, c) in self.recipe.clauses.iter().enumerate() {
            if keys_null[i] != 0 {
                return Ok(false);
            }
            // a non-null tid key cell is a pointer to an ItemPointerData image
            let mut t = tid_cell(keys[i]);
            match *c {
                TidClause::Lower { inclusive } => {
                    if !inclusive { ItemPointerInc(&mut t) }
                    lo = if ItemPointerCompare(&t, &lo) > 0 { t } else { lo };
                }
                TidClause::Upper { inclusive } => {
                    if !inclusive { ItemPointerDec(&mut t) }
                    hi = if ItemPointerCompare(&t, &hi) < 0 { t } else { hi };
                }
                _ => return Err(crate::pg_err("gravity: a TID range clause that is not a bound")),
            }
        }
        (self.lo, self.hi) = (lo, hi);
        let mcx = self.scratch.exec_mcx();
        match self.scan.as_mut() {
            Some(s) => tableam::table_rescan_tidrange(mcx, s, &lo, &hi)?,
            None => {
                let heap = self.heap.as_ref().ok_or_else(|| crate::pg_err("gravity: seek on an unbound tid source"))?;
                self.scan = Some(tableam::table_beginscan_tidrange(mcx, heap, self.snapshot.clone(), &lo, &hi)?);
            }
        }
        Ok(true)
    }
    /// TidListEval: the points (a CURRENT OF clause asks its cursor), valid for the relation, sorted and deduplicated.
    fn eval_points(&mut self, keys: &[Word], keys_null: &[u8]) -> PgResult<bool> {
        let (mut all, mut w) = (Vec::new(), 0); // alloc: Exec
        self.current = false;
        let heap = self.heap.as_ref().ok_or_else(|| crate::pg_err("gravity: seek on an unbound tid source"))?;
        // bounded("other: one TID clause of the scan per pass (the statement's quals bound them); an array clause's elements are read under their own step")
        for c in self.recipe.clauses.iter() {
            let (k, null) = (keys[w], keys_null[w] != 0);
            w += 1;
            match c {
                // fetch_cursor_param_value (execCurrent.c): a refcursor parameter without a value
                TidClause::Current { by_param: true, param } if null => {
                    return Err(PgError::error(format!("no value found for parameter {param}")).with_sqlstate(ERRCODE_UNDEFINED_OBJECT).into()); // alloc: Cold
                }
                _ if null => {}
                // a non-null tid key cell is a pointer to an ItemPointerData image
                TidClause::Eq => all.push(tid_cell(k)), // alloc: Exec
                // a non-null array key cell addresses a live array image
                TidClause::Any => tid_array_cell(k, &mut all)?,
                TidClause::Current { by_param, param } => {
                    let mcx = self.scratch.exec_mcx();
                    // the clause's key cell, non-null: the bind's cursor word (`TidClause::Current`)
                    let name = cursor_cell(mcx, k, *by_param)?;
                    if let Some(t) = execmain_seams::exec_current_of::call(Some(name), *param, heap.rd_id, heap.name())? {
                        all.push(t); // alloc: Exec
                        self.current = true;
                    }
                }
                _ => return Err(crate::pg_err("gravity: a TID point clause that is a bound")),
            }
        }
        let scan = self.scan.as_mut().ok_or_else(|| crate::pg_err("gravity: seek on an unbound tid source"))?;
        self.tids.clear();
        self.tids.extend(all.into_iter().filter(|t| tableam::table_tuple_tid_valid(scan, t))); // alloc: Exec
        // the points are as many as the arrays' elements: the sort polls
        let mut step = cancelpoll::step("tid_sample eval_points: the TID list sorted");
        let mut scratch = if self.tids.len() > cancelpoll::SORT_RUN { vec![ItemPointerData::default(); self.tids.len()] } else { Vec::new() }; // alloc: Exec
        cancelpoll::sort_by(&mut step, &mut self.tids, &mut scratch, |a, b| ItemPointerCompare(a, b).cmp(&0)).map_err(|_| crate::pg_err(TID_CANCELED))?;
        drop(scratch);
        self.tids.dedup_by(|a, b| ItemPointerCompare(a, b) == 0);
        self.at = 0;
        Ok(true)
    }
    fn next_pg(&mut self) -> PgResult<bool> {
        let mcx = self.scratch.exec_mcx();
        let (Some(heap), Some(slot), Some(snapshot)) = (self.heap.as_ref(), self.slot.as_mut(), self.snapshot.as_ref()) else { return Err(crate::pg_err("gravity: a tid window on an unbound source")) };
        if let Some(pos) = self.substituted.take() {
            self.open = false;
            let tid = tid_of(pos);
            let held = if self.recipe.range { ItemPointerCompare(&tid, &self.lo) >= 0 && ItemPointerCompare(&tid, &self.hi) <= 0 } else { self.tids.binary_search_by(|p| ItemPointerCompare(p, &tid).cmp(&0)).is_ok() };
            if !held || !crate::ops::shared::heap_fetch::fetch_any(mcx, heap, &tid, slot)? {
                return Ok(false);
            }
        } else if self.recipe.range {
            let scan = self.scan.as_mut().ok_or_else(|| crate::pg_err("gravity: a tid range walk without its scan"))?;
            if !tableam::table_scan_getnextslot_tidrange(mcx, scan, ScanDirection::ForwardScanDirection, slot)? {
                self.open = false;
                return Ok(false);
            }
        } else {
            // a listed TID with no row the snapshot sees is the unit (TidNext checks for interrupts after each)
            let mut step = cancelpoll::step("tid_sample next_pg: a listed TID with no visible row");
            loop {
                let Some(mut tid) = self.tids.get(self.at).copied() else {
                    self.open = false;
                    return Ok(false);
                };
                self.at += 1;
                if self.current {
                    // TidNext: a CURRENT OF row is followed to its newest version
                    let scan = self.scan.as_mut().ok_or_else(|| crate::pg_err("gravity: a tid walk without its scan"))?;
                    tableam::table_tuple_get_latest_tid(mcx, scan, &mut tid)?;
                }
                let found = match self.fetch.as_mut() {
                    Some(f) => tableam::table_index_fetch_tuple(mcx, f, &mut tid, &mut Some(snapshot.clone()), slot, &mut false, None)?,
                    None => tableam::table_tuple_fetch_row_version(mcx, heap, &tid, &Some(snapshot.clone()), slot)?,
                };
                if found {
                    break;
                }
                step.tick().map_err(|_| crate::pg_err(TID_CANCELED))?;
            }
        }
        crate::ops::shared::heap_fetch::project(slot, &self.recipe.attnos, &mut self.row, &mut self.nbuf)?;
        self.pos = pos_of(self.recipe.heap, &slot.base().tts_tid);
        Ok(true)
    }
}

impl Source for TidSource {
    fn bind(&mut self, b: &BindingRef<'_>) -> Result<()> {
        self.scratch.home_statement();
        self.bind_pg(snapshot_of(b)).map_err(crate::stash)
    }

    fn seek(&mut self, keys: &[Word], keys_null: &[u8]) -> Result<()> {
        self.pos = ScanPos::NONE;
        // EvalPlanQual's recheck evaluates the list / range first (TidRecheck, TidRangeRecheck): a source that never ran
        // its own scan (the recheck's) holds no bounds until it does
        let held = if self.recipe.range { self.eval_range(keys, keys_null) } else { self.eval_points(keys, keys_null) }.map_err(crate::stash)?;
        self.open = self.substituted.is_some() || held;
        Ok(())
    }
    /// The row as a 1-row window, its position the window's.
    fn window(&mut self, w: &mut Window<'_>) -> Result<u32> {
        if !self.open || !self.next_pg().map_err(crate::stash)? {
            return Ok(0);
        }
        // bounded("key-columns: one lane per output column, at most MaxTupleAttributeNumber (1664) columns")
        for (k, &a) in self.recipe.attnos.iter().enumerate() {
            let null = self.nbuf[k] != 0;
            self.row_valid[k] = (!null) as u64;
            let valid = if null { &self.row_valid[k] as *const u64 } else { core::ptr::null() };
            w.set(k, Lane::byval(&self.row[k] as *const Word as *const u8, valid, 8, lane::REGION_ARENA, lane::id(lane::ID_ATTR, a as u16 as u32)));
        }
        w.rows = 1;
        w.set_base(WindowBase::at(self.pos));
        Ok(1)
    }
    fn cursor(&self) -> &Cursor { &self.cur }
    fn cursor_mut(&mut self) -> &mut Cursor { &mut self.cur }
    fn lanes_hint(&self) -> usize { self.recipe.attnos.len() }

    fn refetch(&mut self, values: &mut [Word], isnull: &mut [u8]) -> Result<bool> {
        if !self.pos.is_valid() {
            return Ok(false);
        }
        let (Some(heap), Some(snapshot), Some(slot)) = (self.heap.as_ref(), self.snapshot.as_ref(), self.slot.as_mut()) else { return Err(Error::host("refetch on an unbound source")) };
        crate::ops::shared::heap_fetch::fetch_by_tid(self.scratch.exec_mcx(), heap, &tid_of(self.pos), snapshot, slot, &self.recipe.attnos, values, isnull).map_err(crate::stash)
    }
    fn substitute(&mut self, pos: ScanPos) { self.substituted = Some(pos) }
    fn unbind(&mut self) { self.unbind_pg().expect("gravity: tid source unbind") }

    /// D4464: the slot depot's hook: the source's scratch to `to` (the depot's holder at a worker set's park, the taking
    /// statement at its take).
    fn transfer_held(&mut self, to: memledger::Owner, _release: memledger::Release) {
        self.scratch.home(to);
    }
}

impl Drop for TidSource {
    fn drop(&mut self) {
        let _ = self.unbind_pg();
        self.slot = None;
    }
}

/// A TABLESAMPLE over a table AM that samples no rows raises the AM's own error at
/// admission, as ExecInitSampleScan's begin would.
pub fn admit_samples<'p>(pstmt: &'p types_nodes::plannodes::PlannedStmt<'p>) -> PgResult<()> {
    let mut rtis: Vec<u32> = Vec::new(); // alloc: Exec
    gravity_decompose::rerun::each_plan_node(pstmt, &mut |n| {
        if let Some(s) = n.as_sample_scan() {
            rtis.push(s.scan.scanrelid); // alloc: Cold
        }
    });
    // bounded("catalog-sized: the plan's TABLESAMPLE nodes")
    for rti in rtis {
        let rte = (rti as usize).checked_sub(1).filter(|&i| i < pstmt.rtable.len()).and_then(|i| pstmt.rtable.nth(i).as_range_tbl_entry());
        let Some(relid) = rte.filter(|r| r.relid != 0).map(|r| r.relid) else { continue };
        let scratch = NodeScratch::new("GravitySampleAdmit");
        let mcx = scratch.mcx();
        let rel = table::table_open(mcx, relid, NoLock)?;
        let r = match heap_am(&rel) || tableam_vocab::TableAm::of(&rel).is_none() {
            true => Ok(()),
            false => tableam::table_beginscan_sampling(mcx, &rel, None, 0, mcx::PgVec::new_in(mcx), false, false, false).and_then(tableam::table_endscan),
        };
        rel.close(NoLock)?;
        r?;
    }
    Ok(())
}

/// D1920 / D1644: a sample plan node's one seed in an execution (no REPEATABLE), shared by every source of that node
/// (`HostRegistry::sample_seed`): PostgreSQL draws it once per PlanState (nodeSamplescan.c's init) and keeps it over every
/// rescan, and the plan has one PlanState per node however many sites map it (a SubPlan several plan nodes call, a later
/// grouping-sets phase's copy of an aggregate input's SubPlan). Drawn at the first start that needs it.
pub type SampleSeed = Rc<SeedCell>;

/// memsig E-pub t4: a plan node's seed cell, its own block charged to its registry's owner (`HostRegistry::charge_to`: the
/// binding backend's session, as the admission holding the registry outlives the statement; uncharged without one). The
/// charge is the FIRST field (rule RCF).
pub struct SeedCell {
    _charge: memledger::Charge,
    cell: core::cell::Cell<Option<u32>>,
}

impl core::ops::Deref for SeedCell {
    type Target = core::cell::Cell<Option<u32>>;
    fn deref(&self) -> &core::cell::Cell<Option<u32>> {
        &self.cell
    }
}

/// A new seed cell (none drawn yet) on `owner`: the Rc block's bytes (its two counts and the cell) on the owner's charge.
pub fn new_seed(owner: Option<(memledger::Owner, memledger::Kind)>) -> SampleSeed {
    let bytes = (2 * core::mem::size_of::<usize>() + core::mem::size_of::<SeedCell>()) as u64;
    let charge = owner.map_or(memledger::Charge::NONE, |(o, k)| memledger::Charge::open(o, memledger::charge::release_of(o), k, memledger::charge::Door::Heap, bytes));
    Rc::new(SeedCell { _charge: charge, cell: core::cell::Cell::new(None) }) // alloc: Exec charged owner
}

/// TABLESAMPLE's recipe: the heap range recipe, the method (resolved at admission: `Tsm::get`, BERNOULLI / SYSTEM and the
/// tsm_system_rows / tsm_system_time extensions), whether a REPEATABLE word follows the argument, the identity of its plan
/// node in the plan (`node`: its argument list's address, which every site mapping that node shares) and, once bound into
/// an execution's registry, that node's seed there (`seed`; `None`: each source draws its own).
#[derive(Clone)]
pub struct SampleRecipe {
    pub range: RangeRecipe, pub tsm: Tsm, pub repeatable: bool, pub node: usize, pub seed: Option<SampleSeed>,
}

impl SampleRecipe {
    /// The method resolved from the plan's `tsmhandler` (`Tsm::get`: an unknown handler is PostgreSQL's error).
    pub fn from_plan(range: RangeRecipe, mcx: mcx::Mcx<'_>, handler: Oid, repeatable: bool, node: usize) -> PgResult<SampleRecipe> {
        Ok(SampleRecipe { range, tsm: Tsm::get(mcx, handler)?, repeatable, node, seed: None })
    }

    /// This recipe bound into registry `reg`: its plan node's seed there.
    pub fn bound(&self, reg: &mut crate::HostRegistry) -> SampleRecipe {
        SampleRecipe { seed: Some(reg.sample_seed(self.node)), ..self.clone() }
    }

    /// A filter for one source of this recipe.
    pub fn filter(&self) -> TsmFilter {
        TsmFilter { shared: self.seed.clone(), ..TsmFilter::new(self.tsm, self.repeatable) }
    }
}

impl HostRecipe for SampleRecipe {
    fn rel(&self) -> Oid { self.range.heap }
    fn size_hint(&self) -> usize { core::mem::size_of::<HeapRangeSource<TsmFilter>>() }
    fn derive(&self, _desc: &SiteDesc, a: ArenaRef) -> SourceBox { box_source(HeapRangeSource::with_sample(self.range.clone(), self.filter()), a) }
    fn clone_box(&self) -> Box<dyn HostRecipe> { Box::new(self.clone()) } // alloc: Program
}

pub struct TsmFilter {
    state: TsmState,
    block: bool, next: BlockNumber, repeatable: bool,
    /// Without REPEATABLE: one random seed per source, kept over rescans (nodeSamplescan.c's init), or the plan node's
    /// (`shared`, D1920 / D1644) when the recipe was bound.
    seed: Option<u32>,
    shared: Option<SampleSeed>,
    done: i64,
    /// D4735: the method's (use_bulkread, use_pagemode) from its begin, the AM's sample scan flags
    modes: (bool, bool),
}

impl TsmFilter {
    pub fn new(tsm: Tsm, repeatable: bool) -> TsmFilter {
        TsmFilter { state: tsm.init_state(), block: false, next: 0, repeatable, seed: None, shared: None, done: 0, modes: (true, true) }
    }

    /// D4735: this sampler on a plan node's seed (`SampleRecipe::filter`'s), for the rows road's sample (am_rows.rs).
    pub fn sharing(self, shared: Option<SampleSeed>) -> TsmFilter {
        TsmFilter { shared, ..self }
    }

    /// D4735: the AM's sample scan flags (allow_strat, allow_sync, allow_pagemode), as nodeSamplescan.c's tablesample_init.
    pub fn am_flags(&self) -> (bool, bool, bool) {
        (self.modes.0, !self.block, self.modes.1)
    }

    /// D4735: the AM's next sampled block, this sampler driving it (`table_scan_sample_next_block`).
    pub fn am_block<'m>(&mut self, mcx: mcx::Mcx<'m>, scan: &mut tableam::TableScanDesc<'m>) -> PgResult<bool> {
        tableam::table_scan_sample_next_block(mcx, scan, &mut self.state, self.done)
    }

    /// D4735: the AM's next sampled row of that block into `slot`, counted as nodeSamplescan.c's donetuples.
    pub fn am_tuple<'m>(&mut self, mcx: mcx::Mcx<'m>, scan: &mut tableam::TableScanDesc<'m>, slot: &mut SlotData<'m>) -> PgResult<bool> {
        let got = tableam::table_scan_sample_next_tuple(mcx, scan, &mut self.state, self.done, slot)?;
        self.done += got as i64;
        Ok(got)
    }

    /// The seed of a sample without REPEATABLE: the plan node's when shared, else this source's; drawn once.
    fn drawn(&mut self) -> u32 {
        let draw = || pg_prng::global_prng(pg_prng::PgPrng::next_u32);
        match &self.shared {
            Some(s) => s.get().unwrap_or_else(|| {
                let v = draw();
                s.set(Some(v));
                v
            }),
            None => *self.seed.get_or_insert_with(draw),
        }
    }
}

#[cold]
fn arg_err(msg: &'static str, code: types_error::SqlState) -> Box<PgError> {
    Box::new(PgError::error(msg).with_sqlstate(code)) // alloc: Cold
}

impl SampleFilter for TsmFilter {
    fn start(&mut self, keys: &[Word], keys_null: &[u8]) -> PgResult<()> {
        if keys_null[0] != 0 {
            return Err(arg_err("TABLESAMPLE parameter cannot be null", ERRCODE_INVALID_TABLESAMPLE_ARGUMENT));
        }
        let seed = match self.repeatable {
            true if keys_null[1] != 0 => return Err(arg_err("TABLESAMPLE REPEATABLE parameter cannot be null", ERRCODE_INVALID_TABLESAMPLE_REPEAT)),
            true => adt_float::builtins::hashfloat8(f64::from_bits(keys[1])),
            false => self.drawn(),
        };
        self.modes = self.state.begin_sample_scan(&[datum::Datum::from_u64(keys[0])], seed)?;
        (self.block, self.next, self.done) = (self.state.has_next_sample_block(), 0, 0);
        Ok(())
    }
    fn next_block(&mut self, nblocks: BlockNumber) -> PgResult<Option<BlockNumber>> {
        let b = if self.block { self.state.next_sample_block(nblocks, self.done)? } else { self.next };
        self.next = b.wrapping_add(1);
        Ok((b != InvalidBlockNumber && b < nblocks).then_some(b))
    }
    fn keep(&mut self, blk: BlockNumber, maxoff: u16, vis: &mut [u16]) -> usize {
        let (mut n, mut at) = (0, 0);
        // bounded("small-fixed: one page's sampled offsets, which the tablesample method draws from the page's line pointers (at most MaxHeapTuplesPerPage, 291 at 8 K pages)")
        loop {
            let off = self.state.next_sample_tuple(blk, maxoff, self.done);
            if off == 0 {
                return n;
            }
            // bounded("small-fixed: one page's visible line pointers, passed once over the whole page (at most MaxHeapTuplesPerPage, 291 at 8 K pages)")
            while at < vis.len() && vis[at] < off {
                at += 1;
            }
            if at < vis.len() && vis[at] == off {
                vis[n] = off;
                n += 1;
                self.done += 1;
            }
        }
    }
}
