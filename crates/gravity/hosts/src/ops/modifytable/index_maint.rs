use std::any::Any;
use std::rc::Weak;

use datum::Datum;
use execindexing::{check_exclusion_constraint, check_exclusion_or_unique_constraint, BuildIndexInfo, CeoucWaitMode, IndexInfo, ResultRelIndexState};
use gravity_abi::Datum as Word;
use gravity_program::IndexPlan;
use mcx::{Mcx, MemoryContext};
use types_core::Oid;
use types_error::PgResult;
use types_nbtree::IndexUniqueCheck;
use types_rel::{Relation, RelationData};
use types_relscan::IndexAmKind;
use types_storage::lock::{RowExclusiveLock, NoLock};
use types_nodes::Bitmapset;
use types_tuple::ItemPointerData;

use crate::ops::shared::rel::{open_index, park};
use crate::ops::modifytable::write_index_am::{summarizing, unmaintainable_am};

#[derive(Clone, Debug)]
pub struct IndexRecipe {
    pub oid: Oid,
    pub nkeys: usize,
    pub key_attnos: Vec<i16>,
    pub unique: bool,
}

impl IndexRecipe {
    pub fn from_plan(p: &IndexPlan) -> IndexRecipe {
        IndexRecipe { oid: p.index_oid, nkeys: p.keys().len(), key_attnos: p.keys().to_vec(), unique: p.unique } // alloc: Node
    }
}

struct Slot {
    recipe: IndexRecipe,
    rel: Option<Relation<'static>>,
    parked: Option<Weak<RelationData<'static>>>,
    am_cache: Option<Box<dyn Any>>,
    /// `amsummarizing` (BRIN): maintained on a HOT update too.
    summarizing: bool,
    /// the AM keeps a per-statement insert state in `am_cache` (brin / gist / spgist): allocated in `IndexMaint.cache`
    cached: bool,
    /// `indisexclusion` (EXCLUDE, WITHOUT OVERLAPS): the executor's IndexInfo for `check_exclusion_constraint` after the insert.
    exclusion: Option<Box<IndexInfo<'static>>>,
    /// `open` holds this index's RowExclusiveLock for the execution (D613)
    locked: bool,
    /// Decision 18 (partial / expression index): the executor's IndexInfo, whose predicate and expression states
    /// ExecInsertIndexTuples evaluates over the row (`index_predicate_passes`, `FormIndexDatum`); None for a plain one,
    /// whose key cells are the row's own. Built at `open` in the AM cache context, dropped at `unbind`.
    general: Option<Box<IndexInfo<'static>>>,
    /// H-a2: a deferrable unique index (`!indimmediate`): inserted with UNIQUE_CHECK_PARTIAL; V4R-297: a deferrable exclusion
    /// constraint probed with CEOUC_NOWAIT, violation allowed; either's possible conflict is queued for the constraint's
    /// deferred recheck (ExecInsertIndexTuples), set at `open`.
    deferred: bool,
    /// H-a (D235): an UPDATE's `indexUnchanged` hint for this index (execIndexing.c `index_unchanged_by_update`: no key
    /// column among the statement's updated columns), set per execution by `set_updated` once the indexes are open.
    unchanged: bool,
}

pub struct IndexMaint {
    slots: Vec<Slot>,
    /// The key cells of the widest index (`insert` forms every index's key from them).
    keys: Vec<Datum>,
    nulls: Vec<bool>,
    /// The AMs' per-statement insert states (C's `ii_AmCache`: the per-query context, never the per-row one — BRIN's
    /// descriptor was scribbled over sharing the write's per-row context with a btree's inserts); reset at `unbind`.
    /// Boxed: every allocation made in it (`general`'s IndexInfo and ExprStates, the AM states) holds a `Mcx` pointing at
    /// the context, and the host moves after it binds (a routed INSERT's leaf host into `RouteHost::hosts`, that Vec's
    /// growth when a later row reaches a higher leaf): inline, those drops read a moved-from context (the SIGSEGV of
    /// m4_idx_route's expression index over two leaves). NodeScratch boxes its contexts for the same reason.
    cache: Box<MemoryContext>,
    /// C's `ri_IndexRelationDescs != NULL`: this execution opened the indexes (`open`), each under the RowExclusiveLock
    /// `unbind` releases (D613).
    opened: bool,
    /// H-a2: the last `insert`'s recheckIndexes (ExecInsertIndexTuples' result): the deferrable unique indexes that saw a
    /// possible conflict, for the AR triggers (the constraint's unique_key_recheck event).
    recheck: Vec<Oid>,
    /// Decision 18: the written row as a virtual slot for a partial / expression index's predicate and keys (made at the
    /// first such row, in the AM cache context; dropped at `unbind` before its reset).
    slot: Option<types_slot::SlotData<'static>>,
    /// D765: this execution's indexes when the relation's index list at the first row is not the plan's (an index
    /// dropped or created since the plan froze it, by the statement's own first row or by another session): PG's
    /// ExecOpenIndices over the list as it is then, maintained by ExecInsertIndexTuples, every AM and form as PG
    /// maintains it. In the AM cache context; closed (ExecCloseIndices) at `unbind` before its reset.
    generic: Option<Generic>,
}

/// D765: the executor's own index state for an execution whose index list moved (`IndexMaint::generic`).
struct Generic {
    state: ResultRelIndexState<'static>,
    /// an UPDATE's ExecGetAllUpdatedCols (`set_updated`), for ExecInsertIndexTuples' indexUnchanged hints
    updated: Option<Bitmapset<'static>>,
}

/// Why a lowered write may not maintain `index` — `supported`'s form conditions, one reason each,
/// for the admissions to refuse before a bind would (a bind's refusal is an ERROR, not a fallback). Decision 18: a
/// partial, expression or INCLUDE index is maintained when the decomposer froze its IndexPlan (`supported` takes it);
/// the hand shapes, whose recipes carry the key columns alone, keep refusing it here.
pub fn unmaintainable(index: &Relation<'_>) -> Option<&'static str> {
    unmaintainable_form(index, false)
}

/// Decision 18b: a routed INSERT's leaf (`write_route.rs` revalidate, `facts.rs` leaf_fact) builds its recipes from the
/// relcache with every index column (indnatts), so a partial, expression or INCLUDE index is maintained as `general`.
pub fn unmaintainable_routed(index: &Relation<'_>) -> Option<&'static str> {
    unmaintainable_form(index, true)
}

fn unmaintainable_form(index: &Relation<'_>, general: bool) -> Option<&'static str> {
    let Some(form) = index.rd_index.as_ref() else { return Some("an index without its pg_index form") };
    if let Some(what) = unmaintainable_am(index) {
        Some(what)
    } else if !(form.indisvalid && form.indisready) {
        Some("an index that is not valid or not ready")
    } else if !general && form.indnatts != form.indnkeyatts {
        Some("an index with INCLUDE columns (the lowered write maintains key columns)")
    } else if !general && form.has_indpred {
        Some("a partial index")
    } else if !general && form.indexprs_src.is_some() {
        Some("an index with an expression key")
    } else {
        None
    }
}

pub fn supported(index: &Relation<'_>, recipe: &IndexRecipe) -> bool {
    let Some(form) = index.rd_index.as_ref() else { return false };
    unmaintainable_am(index).is_none()
        && form.indisvalid
        && form.indisready
        // H-a2: a deferrable unique index is maintained with UNIQUE_CHECK_PARTIAL, a deferrable exclusion constraint with
        // the CEOUC_NOWAIT probe (V4R-297); their possible conflicts go to the AR triggers' recheck list
        // Decision 18: a predicate and expression keys are the executor's to evaluate (`general`); an expression key is
        // 0 in the recipe as in indkey, and an INCLUDE column is carried as the AM receives it (indnatts, not indnkeyatts)
        && form.indisunique == recipe.unique
        && form.indnatts as usize == recipe.nkeys
        && form.indkey.iter().take(recipe.nkeys).zip(recipe.key_attnos.iter()).all(|(a, b)| *a == *b)
}

/// The AM cache context: a bump context (an 8 KiB first block, kept over the reset at unbind); under
/// `scratch::small_roots` (a host kept between statements, the point path's memo pieces) a small-first one, reset
/// wholesale like the bump context, whose first block waits for the first allocation: a btree-only write puts nothing in
/// it (D4572: the bump keeper was 8,192 of a kept point UPDATE's 37,784 bytes).
fn am_cache() -> MemoryContext {
    if crate::scratch::small() { MemoryContext::new_small_wholesale("GravityIndexAmCache") } else { MemoryContext::new_bump("GravityIndexAmCache") }
}

impl IndexMaint {
    pub fn new(plans: &[IndexPlan]) -> IndexMaint {
        IndexMaint::from_recipes(plans.iter().map(IndexRecipe::from_plan).collect()) // alloc: Node
    }

    /// The same over recipes read from the relcache (a routed INSERT's leaf, `write_route.rs`).
    pub fn from_recipes(recipes: Vec<IndexRecipe>) -> IndexMaint {
        let widest = recipes.iter().map(|r| r.nkeys).max().unwrap_or(0);
        let slots = recipes.into_iter().map(|recipe| Slot { recipe, rel: None, parked: None, am_cache: None, summarizing: false, cached: false, exclusion: None, locked: false, deferred: false, unchanged: false, general: None }).collect(); // alloc: Node
        IndexMaint { slots, keys: vec![Datum::null(); widest], nulls: vec![false; widest], cache: Box::new(am_cache()), opened: false, recheck: Vec::new(), slot: None, generic: None } // alloc: Node
    }

    /// The AM cache context (and every context under it) on `to` from now, its bytes moving with it: the maintenance of a
    /// write host a holder keeps past the statement that made it (`WriteHost::home`).
    pub fn home(&self, to: memledger::Owner) {
        self.cache.set_owner_tree(to);
    }

    /// The AM cache context's blocks' bytes.
    pub fn cache_bytes(&self) -> usize {
        self.cache.mem_allocated()
    }

    fn cache_mcx(&self) -> Mcx<'static> {
        // the AMs' states (`am_cache`) drop at `unbind` before the reset; `cache` is the last field to drop, and it is
        // boxed, so a move of this IndexMaint (its host's) leaves the context where the handed-out `Mcx`s point
        super::raw::cache_mcx(&self.cache)
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// H-a2: the last `insert`'s recheckIndexes, in index order (empty when it inserted into no deferrable index, or none
    /// saw a possible conflict).
    pub fn rechecks(&self) -> &[Oid] {
        &self.recheck
    }

    /// `open` ran for this execution (C's `ri_IndexRelationDescs != NULL`).
    pub fn is_open(&self) -> bool {
        self.opened
    }

    /// `ExecOpenIndices`, where PostgreSQL calls it (D613): at the first row that inserts or updates (ExecInsert,
    /// ExecUpdatePrologue), never at the bind and never for a DELETE; a later row finds them open. Each index takes the
    /// RowExclusiveLock that `unbind` releases as `ExecCloseIndices` does, so what outlives the statement is only a lock
    /// the planner took itself (an UPDATE / DELETE target's indexes, `get_relation_info`). D765: when the list at this
    /// row is not the plan's, or a plan index's form is no longer its recipe's, the execution takes the executor's own
    /// ExecOpenIndices / ExecInsertIndexTuples (`generic`) over the list as it is, so it is never refused here.
    pub fn open(&mut self, mcx: Mcx<'static>, heap: &Relation<'static>) -> PgResult<bool> {
        if self.opened {
            return Ok(true);
        }
        self.opened = true;
        let cache = self.cache_mcx();
        if !self.list_is_the_plans(heap)? {
            // D765: ExecOpenIndices reads the list at this row (a DDL in the statement's own first row, or another
            // session's, since the plan froze it); the executor's own path maintains it, every index as PG does
            self.generic = Some(Generic { state: execindexing::ExecOpenIndices(cache, heap, false)?, updated: None });
            return Ok(true);
        }
        // bounded("catalog-sized: one per index of the relation (its pg_index rows), locked and opened as ExecOpenIndices does")
        for s in self.slots.iter_mut() {
            lmgr_seams::lock_relation_oid::call(s.recipe.oid, RowExclusiveLock)?;
            s.locked = true;
            let rel = open_index(mcx, &s.parked, s.recipe.oid, NoLock)?;
            let ok = supported(&rel, &s.recipe) && rel.rd_index.as_ref().is_some_and(|f| f.indrelid == heap.rd_id);
            if !ok {
                // D765: an index the plan's recipe no longer describes (its form changed since): the executor's path
                rel.close(NoLock)?;
                self.unbind()?;
                self.opened = true;
                self.generic = Some(Generic { state: execindexing::ExecOpenIndices(self.cache_mcx(), heap, false)?, updated: None });
                return Ok(true);
            }
            s.summarizing = summarizing(&rel);
            s.deferred = rel.rd_index.as_ref().is_some_and(|f| !f.indimmediate && (s.recipe.unique || f.indisexclusion));
            s.cached = matches!(IndexAmKind::from_relam(rel.rd_rel.relam), IndexAmKind::Brin | IndexAmKind::Gist | IndexAmKind::Spgist);
            // the exclusion info is this execution's (ExecOpenIndices'): in the AM cache context, dropped and reset at
            // unbind, never in the node context a parked Node keeps (D263)
            s.exclusion = if rel.rd_index.as_ref().is_some_and(|f| f.indisexclusion) { Some(Box::new(BuildIndexInfo(cache, &rel)?)) } else { None }; // alloc: Exec
            s.general = if rel.rd_index.as_ref().is_some_and(|f| f.has_indpred || f.indexprs_src.is_some()) { Some(Box::new(BuildIndexInfo(cache, &rel)?)) } else { None }; // alloc: Exec
            s.rel = Some(rel);
        }
        Ok(true)
    }

    /// D765: the relation's index list at the first row (ExecOpenIndices' RelationGetIndexList) is the plan's, as a set:
    /// the common case, compared without allocating when the relcache holds the list. Otherwise (an index dropped or
    /// created since the plan froze it; D624's index outside the recipes) `open` takes the executor's path.
    fn list_is_the_plans(&self, heap: &Relation<'static>) -> PgResult<bool> {
        let same = |list: &[Oid]| list.len() == self.slots.len() && list.iter().all(|o| self.slots.iter().any(|s| s.recipe.oid == *o));
        if let Some(l) = heap.rd_indexlist.borrow().as_ref() {
            return Ok(same(&l.list));
        }
        // the AM cache context: reset at every unbind, so a relation without the cached list grows nothing per execution
        let list = relcache_seams::relation_get_index_list::call(self.cache_mcx(), heap.rd_id)?; // alloc: Exec
        Ok(same(&list))
    }

    /// H-a (D235): an UPDATE's per-index `indexUnchanged`, execIndexing.c `index_unchanged_by_update` over the statement's
    /// updated columns (`ExecGetAllUpdatedCols`: updatedCols with the stored generated columns they recompute; members offset
    /// by `FirstLowInvalidHeapAttributeNumber`). Key columns only, as PG loops `ii_NumIndexKeyAttrs` (an INCLUDE column is AM
    /// payload). An expression key (keycol <= 0) takes PG's own walk: the index's IndexInfo from the relcache
    /// (`BuildIndexInfo`, per bind) through `execindexing::index_unchanged_by_update`, whose walker looks for a Var over an
    /// updated column in `RelationGetIndexExpressions`. The predicate is not consulted. `None` (an INSERT, or no column list)
    /// = no hint, as ExecInsert passes. Called after `open` (the expression walk reads the opened index): the first
    /// UPDATE row, as ExecUpdatePrologue opens them.
    pub fn set_updated(&mut self, mcx: Mcx<'static>, all_updated: Option<&Bitmapset<'_>>) -> PgResult<()> {
        if self.generic.is_some() {
            let cache = self.cache_mcx();
            // the columns copied into the AM cache context, where the executor's state lives until `unbind`
            let updated = match all_updated {
                Some(u) => {
                    let mut b = Bitmapset::empty();
                    let mut x = u.next_member(-1);
                    // bounded("key-columns: one member per updated column of the statement, at most MaxHeapAttributeNumber (1600) columns")
                    while x >= 0 {
                        b.add_member(cache, x)?; // alloc: Exec
                        x = u.next_member(x);
                    }
                    Some(b)
                }
                None => None,
            };
            self.generic.as_mut().expect("checked").updated = updated;
            return Ok(());
        }
        const FLIHAN: i32 = types_tuple::htup::FirstLowInvalidHeapAttributeNumber;
        // bounded("catalog-sized: one per index of the relation (its pg_index rows), its key columns (at most INDEX_MAX_KEYS 32) tested against the updated set")
        for s in self.slots.iter_mut() {
            let Some(u) = all_updated else {
                s.unchanged = false;
                continue;
            };
            let keys = &s.recipe.key_attnos[..s.recipe.nkeys];
            s.unchanged = if keys.iter().any(|&a| a > 0 && u.is_member(a as i32 - FLIHAN)) {
                false
            } else if !keys.iter().any(|&a| a <= 0) {
                true
            } else if let Some(rel) = s.rel.as_ref() {
                let mut info = BuildIndexInfo(mcx, rel)?; // alloc: Exec
                execindexing::index_unchanged_by_update(u, &mut info)?
            } else {
                false
            };
        }
        Ok(())
    }

    /// `ExecCloseIndices`: the AMs' per-statement insert state released (BRIN's holds a pinned revmap buffer) before the park,
    /// then the lock `open` took ("Drop lock acquired by ExecOpenIndices"). An execution that opened nothing (no row, a
    /// DELETE) has nothing to release and keeps what an earlier one parked.
    pub fn unbind(&mut self) -> PgResult<()> {
        let mut first = Ok(());
        // bounded("catalog-sized: one per index of the relation (its pg_index rows), closed and unlocked as ExecCloseIndices does; teardown runs whole")
        for s in self.slots.iter_mut() {
            if let Some(rel) = s.rel.as_ref() {
                if let Err(e) = indexam::index_insert_cleanup(rel, &mut s.am_cache) {
                    first = first.and(Err(e));
                }
            }
            s.am_cache = None;
            s.exclusion = None;
            s.general = None;
            if s.rel.is_some() {
                match park(s.rel.take()) {
                    Ok(p) => s.parked = p,
                    Err(e) => first = first.and(Err(e)),
                }
            }
            if core::mem::take(&mut s.locked) {
                if let Err(e) = lmgr_seams::unlock_relation_oid::call(s.recipe.oid, RowExclusiveLock) {
                    first = first.and(Err(e));
                }
            }
        }
        if let Some(g) = self.generic.take() {
            if let Err(e) = execindexing::ExecCloseIndices(g.state) {
                first = first.and(Err(e));
            }
        }
        self.opened = false;
        self.slot = None;
        self.cache.reset();
        first
    }

    /// The new row's entry in every index (`only_summarizing`: in the summarizing ones alone — a HOT update
    /// that answered `TU_Summarizing`; `update`: an UPDATE's new version, which passes each index's `indexUnchanged`).
    pub fn insert(&mut self, mcx: Mcx<'static>, heap: &Relation<'static>, tid: &ItemPointerData, values: &[Word], isnull: &[u8], only_summarizing: bool, update: bool) -> PgResult<()> {
        let cache = self.cache_mcx();
        if let Some(g) = self.generic.as_mut() {
            // D765: ExecInsertIndexTuples over the row as the executor holds it (its TID, its table)
            if self.slot.is_none() {
                self.slot = Some(tableam::table_slot_create(cache, heap)?); // alloc: Exec
            }
            let slot = self.slot.as_mut().expect("just made");
            exectuples::exec_clear_tuple(slot, mcx);
            {
                let base = slot.base_mut();
                let n = base.tts_values.len().min(values.len());
                // bounded("key-columns: one cell per column of the row, at most MaxTupleAttributeNumber (1664) columns")
                for a in 0..n {
                    base.tts_values[a] = Datum::from_u64(values[a]);
                    base.tts_isnull[a] = isnull[a] != 0;
                }
            }
            exectuples::exec_store_virtual_tuple(slot);
            slot.base_mut().tts_tableOid = heap.rd_id;
            slot.base_mut().tts_tid = *tid;
            let upd = if update { g.updated.as_ref() } else { None };
            let state_mcx = state_mcx(&mut g.state, cache, mcx)?; // D783: bind, not row: the IndexInfo's cached state outlives the row
            let recheck = execindexing::ExecInsertIndexTuples(state_mcx, mcx, &mut g.state, heap, slot, upd, false, None, &[], only_summarizing)?;
            self.recheck.clear();
            self.recheck.extend(recheck.iter().copied()); // alloc: Exec
            return Ok(());
        }
        let (keys, nulls, recheck, general) = (&mut self.keys, &mut self.nulls, &mut self.recheck, &mut self.slot);
        recheck.clear();
        // bounded("catalog-sized: one entry per index of the relation (its pg_index rows) for the row, as ExecInsertIndexTuples loops them; the kernel's row loop polls per row")
        for s in self.slots.iter_mut().filter(|s| !only_summarizing || s.summarizing) {
            let rel = s.rel.as_ref().expect("gravity: index maintenance before ExecOpenIndices (IndexMaint::open)");
            if let Some(info) = s.general.as_deref_mut() {
                // ExecInsertIndexTuples over the row in a virtual slot: no entry when the predicate fails, the expression
                // keys by FormIndexDatum (the per-row context `mcx` holds their results until the entry is in)
                if general.is_none() {
                    *general = Some(tableam::table_slot_create(cache, heap)?); // alloc: Exec
                }
                let slot = general.as_mut().expect("just made");
                exectuples::exec_clear_tuple(slot, mcx);
                {
                    let base = slot.base_mut();
                    let n = base.tts_values.len().min(values.len());
                    // bounded("key-columns: one cell per column of the row, at most MaxTupleAttributeNumber (1664) columns")
                    for a in 0..n {
                        base.tts_values[a] = Datum::from_u64(values[a]);
                        base.tts_isnull[a] = isnull[a] != 0;
                    }
                }
                exectuples::exec_store_virtual_tuple(slot);
                slot.base_mut().tts_tableOid = heap.rd_id;
                if !info.ii_Predicate.is_nil() && !execindexing::index_predicate_passes(cache, mcx, info, slot)? {
                    continue;
                }
                execindexing::FormIndexDatum(cache, mcx, info, slot, &mut keys[..s.recipe.nkeys], &mut nulls[..s.recipe.nkeys])?;
            } else {
                // bounded("small-fixed: one key datum per index key, at most INDEX_MAX_KEYS")
                for k in 0..s.recipe.nkeys {
                    let a = s.recipe.key_attnos[k] as usize - 1;
                    keys[k] = Datum::from_u64(values[a]);
                    nulls[k] = isnull[a] != 0;
                }
            }
            let check = match (s.recipe.unique, s.deferred) {
                (false, _) => IndexUniqueCheck::UNIQUE_CHECK_NO,
                (true, false) => IndexUniqueCheck::UNIQUE_CHECK_YES,
                (true, true) => IndexUniqueCheck::UNIQUE_CHECK_PARTIAL,
            };
            let n = s.recipe.nkeys;
            let mut satisfied = indexam::index_insert(if s.cached { cache } else { mcx }, rel, &keys[..n], &nulls[..n], tid, heap, check, update && s.unchanged, &mut s.am_cache)?;
            // ExecInsertIndexTuples: the exclusion scan after the entry is in (an immediate constraint waits on a conflicting insert)
            if let Some(info) = s.exclusion.as_deref_mut() {
                // D786 (D783's class): the recheck's FormIndexDatum over each neighbour caches this IndexInfo's expression
                // state; built here once per bind in the AM cache context (over the new row), never in the per-row `mcx`
                if let (false, true, Some(slot)) = (info.ii_Expressions.is_nil(), info.ii_ExpressionsState.is_empty(), general.as_mut()) {
                    let (mut v, mut z) = ([Datum::null(); 32], [false; 32]); // INDEX_MAX_KEYS
                    execindexing::FormIndexDatum(cache, mcx, info, slot, &mut v, &mut z)?;
                }
                if s.deferred {
                    // V4R-297: ExecInsertIndexTuples' !indimmediate arm: CEOUC_NOWAIT, violationOK (the recheck decides at commit)
                    satisfied = check_exclusion_or_unique_constraint(mcx, mcx, heap, rel, info, tid, &keys[..n], &nulls[..n], false, CeoucWaitMode::NoWait, true, None)?;
                } else {
                    check_exclusion_constraint(mcx, mcx, heap, rel, info, tid, &keys[..n], &nulls[..n], false)?;
                }
            }
            // ExecInsertIndexTuples: (checkUnique == UNIQUE_CHECK_PARTIAL || ii_ExclusionOps) && !satisfiesConstraint
            if s.deferred && !satisfied {
                recheck.push(s.recipe.oid); // alloc: Exec
            }
        }
        Ok(())
    }
}

/// D783 / D785 (the class of D778's partition check): the context ExecInsertIndexTuples / ExecCheckIndexConstraints are handed for the state they cache on the
/// IndexInfo (ii_PredicateState, ii_ExpressionsState, a brin / gist / spgist ii_AmCache): C builds those in the
/// executor's query context (ExecPrepareQual / ExecPrepareExprList, ii_Context), never the per-tuple one. The predicates are
/// built here in `bind` (per bind, reset only at unbind); the call gets `bind` while an expression state is still unbuilt
/// (FormIndexDatum builds it at the first row that passes the predicate) or the AM caches per statement (as a recipe's
/// `cached` does), else `row`, the per-row context, for what is the row's alone.
pub fn state_mcx(state: &mut ResultRelIndexState<'static>, bind: Mcx<'static>, row: Mcx<'static>) -> PgResult<Mcx<'static>> {
    let mut built = true;
    // bounded("catalog-sized: one per index of the relation (the executor's opened index list), its predicate prepared once per bind")
    for (d, i) in state.descs.iter().zip(state.infos.iter_mut()) {
        execindexing::prepare_index_predicate(bind, i)?;
        built &= (i.ii_Expressions.is_nil() || !i.ii_ExpressionsState.is_empty())
            && !matches!(IndexAmKind::from_relam(d.rd_rel.relam), IndexAmKind::Brin | IndexAmKind::Gist | IndexAmKind::Spgist);
    }
    Ok(if built { row } else { bind })
}

impl Drop for IndexMaint {
    fn drop(&mut self) {
        let _ = self.unbind();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recipe(oid: Oid, keys: &[i16]) -> IndexRecipe {
        IndexRecipe { oid, nkeys: keys.len(), key_attnos: keys.to_vec(), unique: false } // alloc: Cold
    }

    /// The cache context stays where its `Mcx`s point when the IndexMaint moves: a routed INSERT's leaf host binds (its
    /// expression index's IndexInfo in `cache`), then moves into `RouteHost::hosts` and again when that Vec grows; the
    /// drop at unbind deallocates through the context the ExprStates were made in (SIGSEGV in m4_idx_route, 2026-09-26).
    /// D4572 follow-up: a write host made under `scratch::small_roots` (a kept point write's) has an AM cache context
    /// that holds a small first block once used and none before, where the bump context keeps an 8 KiB one; a btree-only
    /// write never uses it.
    #[test]
    fn small_roots_make_a_small_am_cache() {
        let small = {
            let _g = crate::scratch::small_roots();
            IndexMaint::new(&[])
        };
        let bump = IndexMaint::new(&[]);
        let unused = small.cache_bytes();
        small.cache_mcx().alloc_bytes(64).unwrap();
        bump.cache_mcx().alloc_bytes(64).unwrap();
        assert!(unused <= 1024 && small.cache_bytes() <= 2048, "small AM cache: {unused} unused, {} used", small.cache_bytes());
        assert!(bump.cache_bytes() >= 8192, "the bump AM cache's keeper: {}", bump.cache_bytes());
    }

    #[test]
    fn the_cache_context_stays_put_when_the_index_maintenance_moves() {
        let at = |m: &IndexMaint| core::ptr::from_ref::<MemoryContext>(&m.cache);
        let mut hosts = vec![Some(IndexMaint::from_recipes(vec![recipe(1, &[1])]))]; // alloc: Cold
        let before = at(hosts[0].as_ref().expect("built"));
        let moved = Box::new(hosts.pop().flatten().expect("built")); // alloc: Cold
        assert_eq!(at(&moved), before, "the IndexMaint moved from the Vec into a Box; its cache context must not");
    }

    /// H-a (D235): an index is unchanged by an UPDATE when none of its key columns is updated (index_unchanged_by_update);
    /// no column list (an INSERT) gives no hint; an expression key over an unbound index gives none either.
    #[test]
    fn an_index_is_unchanged_when_no_key_column_is_updated() {
        let ctx: &'static MemoryContext = Box::leak(Box::new(MemoryContext::new("t"))); // alloc: Cold
        let m = ctx.mcx();
        let mut m2 = IndexMaint::from_recipes(vec![recipe(1, &[1]), recipe(2, &[2, 3]), recipe(3, &[3]), recipe(4, &[0])]); // alloc: Cold
        let mut upd = Bitmapset::empty();
        upd.add_member(m, 2 - types_tuple::htup::FirstLowInvalidHeapAttributeNumber).unwrap();
        m2.set_updated(m, Some(&upd)).unwrap();
        assert_eq!(m2.slots.iter().map(|s| s.unchanged).collect::<Vec<_>>(), vec![true, false, true, false]); // alloc: Cold
        m2.set_updated(m, None).unwrap();
        assert!(m2.slots.iter().all(|s| !s.unchanged));
    }
}
