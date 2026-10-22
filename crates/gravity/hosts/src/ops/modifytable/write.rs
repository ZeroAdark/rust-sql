use std::rc::Weak;

use datum::Datum;
use gravity_abi::Datum as Word;
use gravity_abi::ScanPos;
use std::rc::Rc;

use execindexing::ResultRelIndexState;
use gravity_driver::{BindingRef, Outcome, Result, WriteOp, Writer};
use gravity_program::{Cmd, Conflict, WritePlan};
use mcx::Mcx;
use tableam_vocab::{LockTupleMode, LockWaitPolicy, TM_FailureData, TM_Result, TU_UpdateIndexes, TUPLE_LOCK_FLAG_FIND_LAST_VERSION};
use types_core::{CommandId, Oid};
use types_nodes::nodes_enums::CmdType;
use types_nodes::parsenodes::WCOKind;
use types_error::{PgError, PgResult, ERRCODE_CARDINALITY_VIOLATION, ERRCODE_TRIGGERED_DATA_CHANGE_VIOLATION, ERRCODE_T_R_SERIALIZATION_FAILURE, ERRCODE_WRONG_OBJECT_TYPE};
use types_rel::{Relation, RelationData, RELKIND_MATVIEW, RELKIND_PARTITIONED_TABLE, RELKIND_RELATION, RELKIND_TOASTVALUE, RELKIND_VIEW};
use types_slot::SlotData;
use types_snapshot::SnapshotData;
use types_storage::lock::{NoLock, RowExclusiveLock};
use types_tuple::ItemPointerData;

use crate::ops::modifytable::index_maint::{IndexMaint, IndexRecipe};
use crate::ops::modifytable::trigger::{Bound, StmtOf, TriggerHost};
use crate::ops::modifytable::trigger_stmt::{MergeOps, StmtTriggers};
use std::cell::RefCell;
use crate::ops::modifytable::trigger_before::Pre;
use crate::ops::modifytable::write_constraints::{ModifiedCols, WriteConstraints};

use crate::ops::modifytable::write_partition::PartitionCheck;
use crate::ops::shared::rel::{open_heap, park, pos_of, snapshot_of, tid_of};
use crate::scratch::NodeScratch;

use crate::ops::modifytable::trigger_stmt::CROSSCHECK;

/// The crosscheck snapshot of the statement whose run binds write hosts now (an executor run of a query desc that carries
/// one), the previous statement's back when it drops: a nested statement's run sets and restores its own.
pub struct Crosscheck(Option<Rc<SnapshotData<'static>>>);

impl Crosscheck {
    pub fn set(snapshot: Option<Rc<SnapshotData<'static>>>) -> Crosscheck {
        Crosscheck(CROSSCHECK.with(|c| c.replace(snapshot)))
    }
}

impl Drop for Crosscheck {
    fn drop(&mut self) {
        let prev = self.0.take();
        let replaced = CROSSCHECK.with(|c| c.replace(prev));
        drop(replaced);
    }
}

fn crosscheck_now() -> Option<Rc<SnapshotData<'static>>> {
    CROSSCHECK.with(|c| c.borrow().clone())
}

#[derive(Clone, Debug)]
pub struct WriteRecipe {
    pub heap: Oid,
    pub cmd: Cmd,
    pub conflict: Conflict,
    pub arbiters: Vec<Oid>,
    pub defer: bool,
}

impl WriteRecipe {
    pub fn from_plan(p: &WritePlan) -> WriteRecipe {
        WriteRecipe { heap: p.target_rel, cmd: p.cmd, conflict: p.conflict, arbiters: p.arbiters.to_vec(), defer: p.defer } // alloc: Node
    }
}

pub struct WriteHost {
    recipe: WriteRecipe,
    heap: Option<Relation<'static>>,
    parked: Option<Weak<RelationData<'static>>>,
    maint: IndexMaint,
    slot: Option<SlotData<'static>>,
    cid: CommandId,
    recheck_pos: ScanPos,
    values: Vec<Datum>,
    nulls: Vec<bool>,
    /// The written row's CHECK / NOT NULL constraints, through the incumbent (entry-flip,
    /// lower.md §14.12): `write_constraints.rs`, called once per formed row.
    constraints: WriteConstraints,
    generated: crate::ops::modifytable::write_generated::StoredGenerated,
    /// A directly written partition's constraint (`write_partition.rs`): ExecInsert / ExecUpdateAct's order.
    partition: PartitionCheck,
    /// The statement's insertedCols / updatedCols: its plan's at the derive, the host's own copies (node context) from its first bind on,
    /// so a parked Node never reads a plan that is gone (H16 (b): the simple protocol parks writers; exec `park_key` salts the words).
    modified: Option<ModifiedCols>,
    cols_owned: bool,
    triggers: Option<TriggerHost>,
    /// A view target (`write_view.rs`): the INSTEAD OF INSERT row trigger is the write.
    view: Option<crate::ops::modifytable::write_view::ViewInsert>,
    /// H-d: a view's UPDATE / DELETE through its INSTEAD OF row triggers (write_view_rows.rs; Decision 8).
    view_rows: Option<crate::ops::modifytable::write_view_rows::ViewRows>,
    /// H-f: a MERGE into a view: its DELETE actions' INSTEAD OF DELETE host (`view_rows` holds the UPDATE's, `view` the
    /// INSERT's); `merge` the statement's subcommands (the registry's, per admission).
    view_del: Option<crate::ops::modifytable::write_view_rows::ViewRows>,
    merge: MergeOps,
    /// THIS statement's check options (`write_check.rs`), bound over the heap at bind.
    checks: Option<Rc<[crate::ops::modifytable::write_check::CheckSpec]>>,
    /// INHERIT-ROOT (`WritePlan.checks_of`): the relation whose attnos `checks` carry when not the heap's (0: the heap's).
    checks_of: Oid,
    wco: Option<crate::ops::modifytable::write_check::WriteChecks>,
    /// W2: this execution's parameter words (the binding's; a leaf's from its route host), for the checks' InitPlan params.
    words: crate::ops::modifytable::write_check::ParamWords,
    bound: bool,
    /// A leaf under a routed INSERT (`write_route.rs`): the row was routed here, so its partition constraint is not checked (ExecInsert with `ri_RootResultRelInfo`).
    routed: bool,
    /// The routed row's root relation (an alias the route host holds open), or the pruned write's parent (`under_root`,
    /// opened here under RowExclusiveLock): the constraints' DETAIL relation; statement triggers are its, never the leaf's.
    root: Option<Relation<'static>>,
    under_root: Option<Oid>,
    /// The root's statement triggers under a routed INSERT (bound by the route host, handed in at `for_leaf`).
    root_stmt: Option<Rc<RefCell<StmtTriggers>>>,
    /// ON CONFLICT (ExecInsert's speculative arm): the executor's index state with the arbiters' unique
    /// info (opened per bind in `oc_scratch`'s exec context, closed at unbind), the conflicting row's
    /// slot, and the statement's snapshot for DO NOTHING's visibility check under a transaction snapshot.
    /// Declared before the scratch contexts they live in: fields drop in order. `oc_scratch`'s exec context is the
    /// per-bind context (reset at unbind): the check states (`wco`) live there too, never in the node context (D263).
    oc: Option<ResultRelIndexState<'static>>,
    existing: Option<SlotData<'static>>,
    snapshot: Option<Rc<SnapshotData<'static>>>,
    /// The statement's crosscheck snapshot (PG's es_crosscheck_snapshot: an RI action under a transaction snapshot), read at
    /// bind (`crosscheck_now`) and given to heap_update / heap_delete, which report a row it cannot see as TM_Updated.
    crosscheck: Option<Rc<SnapshotData<'static>>>,
    /// DO UPDATE's row lock mode (ExecUpdateLockMode: exclusive when a SET column is a key column), per bind.
    lockmode: Option<LockTupleMode>,
    /// A DELETE .. RETURNING's deleted version (ExecDelete's fetch under SnapshotAny, M4-RET (iii)): filled when the row's
    /// site has cells, handed back by `written`, its buffer pin held until the next row op (the cells point into it).
    deleted: Option<SlotData<'static>>,
    deleted_filled: bool,
    /// D655 Row C (V4R-534): the newest version a Recheck locked, fetched by `epq_row` (`epq_ready` until then) into its own
    /// slot, pinned until the next fetch or unbind: the retried row's by-reference cells and RETURNING point into it.
    epq: Option<SlotData<'static>>,
    epq_ready: bool,
    /// H-e (c): a cross-partition UPDATE's move host (`write_move.rs`), this host's own (a pruned write under its root)
    /// or its multi-target host's (shared by every target); `moved` = the last row moved (`written` is the move's).
    mover: Option<Rc<RefCell<crate::ops::modifytable::write_move::Mover>>>,
    mover_owned: bool,
    moved: bool,
    /// A moved row's destination (`write_move.rs`): its AFTER ROW half is a moved row's; `inserted` its last tid.
    moved_in: bool,
    inserted: ItemPointerData,
    /// M4-RET R4a (`WritePlan.old`): an UPDATE fetches the version it replaced into the slot above after Done (RETURNING
    /// OLD), `old_filled` until the next row op, handed back by `old`; a leaf host under a router never does.
    old: bool,
    old_filled: bool,
    /// D774 (`WritePlan.sys`): the last row op's system columns for RETURNING (write_sys.rs).
    sys: crate::ops::modifytable::write_sys::SysCols,
    /// W6-D1112: the row op parked at its check's SubPlan read, and where its resume enters (`Writer::resume`).
    park: Park,
    /// D1112 row B: a move parked at its destination's check (`Park::Move`): the deleted source row and whether the
    /// foreign-key leg follows the insert half.
    move_fk: Option<(ItemPointerData, bool)>,
    oc_scratch: NodeScratch,
    scratch: NodeScratch,
    defer: bool,
    refetch: bool,
}

/// W6-D1112: where a row op parked at a check's SubPlan read resumes: the RLS check (before the constraints) or the view
/// check (after the heap, the indexes and the AFTER ROW queue).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Park {
    None,
    Rls,
    View,
    /// D1112 row B (update#164 / #165 / #171 / #176): a moved row's destination parked at its check's SubPlan read
    /// (the source's delete half done); the resume enters the move host (write_move.rs `resume`), then the foreign-key leg.
    Move,
}

pub fn writable(heap: &Relation<'_>) -> bool {
    !heap.rd_hastriggers && writable_target(heap)
}

/// `writable` less the trigger condition: the write host's, whose trigger host takes the served kinds.
fn writable_target(heap: &Relation<'_>) -> bool {
    // a STORED generated column is `write_generated`'s; a VIRTUAL one has no storage (the planner's NULL const is the cell).
    // relhassubclass is no condition: the planner names every result relation (an inheritance UPDATE / DELETE is the
    // decomposer's Targets member), an INSERT / ONLY write touches the named relation alone, and the flag outlives the children.
    // relhasrules is no condition either: the rewriter applied the rules before planning, so the plan's ModifyTable writes
    // exactly as PostgreSQL's does, and a rule's extra queries are statements of their own
    // a materialized view is written by REFRESH ... CONCURRENTLY's diff (result_rel_error admits it only then)
    matches!(heap.rd_rel.relkind, RELKIND_RELATION | RELKIND_MATVIEW) && tableam_vocab::TableAm::of(heap) == Some(tableam_vocab::TableAm::Heap)
}

fn writable_am(heap: &Relation<'_>) -> bool {
    heap.rd_rel.relkind == RELKIND_RELATION && tableam_vocab::TableAm::of(heap).is_some_and(|a| a != tableam_vocab::TableAm::Heap)
}

fn heap_rel(heap: &Relation<'_>) -> bool {
    tableam_vocab::TableAm::of(heap) == Some(tableam_vocab::TableAm::Heap)
}

pub fn admit_target(relid: Oid, op: CmdType, on_conflict: bool) -> PgResult<()> {
    let scratch = NodeScratch::new("GravityAmAdmit");
    let rel = table::table_open(scratch.mcx(), relid, NoLock)?;
    let r = if rel.rd_rel.relkind == RELKIND_RELATION { tableam::table_modify_admit(&rel, op, on_conflict) } else { Ok(()) };
    rel.close(NoLock)?;
    r
}

const fn cmd_type(cmd: Cmd) -> Option<CmdType> {
    match cmd {
        Cmd::Insert => Some(CmdType::CMD_INSERT),
        Cmd::Update => Some(CmdType::CMD_UPDATE),
        Cmd::Delete => Some(CmdType::CMD_DELETE),
        Cmd::Merge => Some(CmdType::CMD_MERGE),
        Cmd::Select => None,
    }
}

/// CheckValidResultRel's TOAST and materialized view arms (execMain.c): PostgreSQL raises these at ExecInitModifyTable,
/// before any row; a matview is a result relation only under REFRESH ... CONCURRENTLY's incremental maintenance.
fn result_rel_error(heap: &Relation<'_>) -> Option<Box<PgError>> {
    let what = match heap.rd_rel.relkind {
        RELKIND_TOASTVALUE => "TOAST relation",
        RELKIND_MATVIEW if !matview_seams::matview_maintenance_is_enabled::call() => "materialized view",
        _ => return None,
    };
    Some(Box::new(PgError::error(format!("cannot change {what} \"{}\"", String::from_utf8_lossy(heap.rd_rel.relname.name_str()))).with_sqlstate(ERRCODE_WRONG_OBJECT_TYPE))) // alloc: Cold
}

/// Every relkind arm of CheckValidResultRel that raises before its replica identity check, for an EXPLAIN that never
/// binds a host: a sequence, a TOAST relation, a materialized view outside its maintenance and any relation but a table,
/// a partitioned table, a view or a foreign table. A view's INSTEAD OF triggers are the rewriter's (it keeps a view as
/// the result relation only with them), and a foreign table's EXPLAIN is its FDW's (entry explain.rs).
pub fn result_kind_error(heap: &Relation<'_>) -> Option<Box<PgError>> {
    use types_rel::{RELKIND_FOREIGN_TABLE, RELKIND_SEQUENCE};
    let what = match heap.rd_rel.relkind {
        RELKIND_RELATION | RELKIND_PARTITIONED_TABLE | RELKIND_VIEW | RELKIND_FOREIGN_TABLE => return None,
        RELKIND_TOASTVALUE | RELKIND_MATVIEW => return result_rel_error(heap),
        RELKIND_SEQUENCE => "sequence",
        _ => "relation",
    };
    Some(Box::new(PgError::error(format!("cannot change {what} \"{}\"", String::from_utf8_lossy(heap.rd_rel.relname.name_str()))).with_sqlstate(ERRCODE_WRONG_OBJECT_TYPE))) // alloc: Cold
}

/// MERGE's `TM_SelfModified` (`ExecMergeMatched`): a row the command already affected is the
/// cardinality violation, one a trigger of the command modified the 27000 error.
pub(crate) fn merge_self_modified(tmfd: &TM_FailureData, cid: CommandId) -> Box<PgError> {
    let e = if tmfd.cmax != cid {
        PgError::error("tuple to be updated or deleted was already modified by an operation triggered by the current command".to_string()).with_sqlstate(ERRCODE_TRIGGERED_DATA_CHANGE_VIOLATION).with_hint("Consider using an AFTER trigger instead of a BEFORE trigger to propagate changes to other rows.") // alloc: Cold
    } else if xact_seams::transaction_id_is_current_transaction_id::call(tmfd.xmax) {
        PgError::error("MERGE command cannot affect row a second time".to_string()).with_sqlstate(ERRCODE_CARDINALITY_VIOLATION).with_hint("Ensure that not more than one source row matches any one target row.") // alloc: Cold
    } else {
        PgError::error("attempted to update or delete invisible tuple".to_string()) // alloc: Cold
    };
    Box::<PgError>::new(e) // alloc: Cold
}

/// TM_Deleted under a transaction snapshot (ExecDelete / ExecUpdate / ExecOnConflictUpdate): 40001 naming the delete;
/// TM_Updated's twin is `crate::serialization_failure()` (D246).
fn concurrent_delete() -> Box<PgError> {
    Box::new(PgError::error("could not serialize access due to concurrent delete".to_string()).with_sqlstate(ERRCODE_T_R_SERIALIZATION_FAILURE)) // alloc: Cold
}

fn tm_error(what: &'static str, r: TM_Result, tmfd: &TM_FailureData, cid: CommandId) -> Box<PgError> {
    let e = match r {
        TM_Result::TM_SelfModified => PgError::error(format!("tuple to be {what}d was already modified by an operation triggered by the current command")).with_sqlstate(ERRCODE_TRIGGERED_DATA_CHANGE_VIOLATION).with_hint("Consider using an AFTER trigger instead of a BEFORE trigger to propagate changes to other rows."), // alloc: Cold
        TM_Result::TM_Invisible => PgError::error(format!("attempted to {what} invisible tuple")), // alloc: Cold
        other => PgError::error(format!("unrecognized table_tuple_{what} status: {other:?} (cid {cid}, cmax {})", tmfd.cmax)), // alloc: Cold
    };
    Box::<PgError>::new(e) // alloc: Cold
}

impl WriteHost {
    pub fn new(plan: &WritePlan) -> WriteHost {
        WriteHost { recipe: WriteRecipe::from_plan(plan), heap: None, parked: None, maint: IndexMaint::new(plan.indexes()), slot: None, cid: 0, recheck_pos: ScanPos::NONE, values: Vec::new(), nulls: Vec::new(), constraints: WriteConstraints::new(), generated: crate::ops::modifytable::write_generated::StoredGenerated::new(), partition: PartitionCheck::new(), modified: None, cols_owned: false, triggers: None, view: None, view_rows: None, view_del: None, merge: MergeOps::NONE, checks: None, checks_of: plan.checks_of, wco: None, words: crate::ops::modifytable::write_check::ParamWords::NONE, bound: false, routed: false, root: None, root_stmt: None, under_root: plan.route.as_ref().filter(|_| plan.cmd != Cmd::Insert).map(|r| r.root), scratch: NodeScratch::new("GravityWriteHost"), oc: None, existing: None, snapshot: None, crosscheck: None, lockmode: None, deleted: None, deleted_filled: false, epq: None, epq_ready: false, mover: None, mover_owned: false, moved: false, moved_in: false, inserted: ItemPointerData::default(), old: plan.old, old_filled: false, sys: crate::ops::modifytable::write_sys::SysCols::new(plan), park: Park::None, move_fk: None, oc_scratch: NodeScratch::new("GravityWriteConflict"), defer: false, refetch: false }.with_plan_mover(plan) // alloc: Node
    }

    /// H-f: THIS statement's MERGE subcommands (`fireBSTriggers`' set; a view's per-action INSTEAD OF hosts).
    pub fn with_merge(mut self, merge: MergeOps) -> WriteHost {
        self.merge = merge;
        self
    }

    /// H-e (c): a pruned cross-partition UPDATE's own move host (`RoutePlan.moves`).
    fn with_plan_mover(mut self, plan: &WritePlan) -> WriteHost {
        self.mover = crate::ops::modifytable::write_move::Mover::of(plan);
        self.mover_owned = self.mover.is_some();
        self
    }

    /// H-e (c): a target of a multi-target cross-partition UPDATE: the move host its multi-target host owns.
    pub(crate) fn with_mover(mut self, mover: Option<Rc<RefCell<crate::ops::modifytable::write_move::Mover>>>) -> WriteHost {
        self.mover = mover;
        self.mover_owned = false;
        self
    }

    /// H-e (c): a moved row's destination leaf (`write_move.rs`).
    pub(crate) fn moved_in(mut self) -> WriteHost {
        self.moved_in = true;
        self
    }

    /// The tid of the last row this host inserted (a moved row's, for the foreign-key leg).
    pub(crate) fn inserted_tid(&self) -> ItemPointerData {
        self.inserted
    }

    pub fn with_checks(mut self, checks: Option<Rc<[crate::ops::modifytable::write_check::CheckSpec]>>) -> WriteHost {
        self.checks = checks.filter(|c| !c.is_empty());
        // V4R-329: a pruned cross-partition UPDATE's own move host maps this target's list onto each destination
        if let Some(m) = self.mover.as_ref().filter(|_| self.mover_owned) {
            m.borrow_mut().with_checks(self.recipe.heap, self.checks.clone());
        }
        self
    }

    /// A routed INSERT's leaf host (`write_route.rs`): its recipes from the leaf's relcache under the leaf's lock.
    pub(crate) fn for_leaf(recipe: WriteRecipe, indexes: Vec<IndexRecipe>, root: Relation<'static>, root_stmt: Option<Rc<RefCell<StmtTriggers>>>) -> WriteHost {
        let mut h = WriteHost::new(&WritePlan::SELECT);
        h.recipe = recipe;
        h.maint = IndexMaint::from_recipes(indexes);
        h.routed = true;
        h.root = Some(root);
        h.root_stmt = root_stmt;
        h
    }

    /// G2(d) (`write_route.rs` RouteHost `checks_of`): the relation whose attnos a routed leaf's checks carry (a MERGE's
    /// first result relation); `checks_onto` maps them onto the leaf at the bind. 0: the leaf's own.
    pub(crate) fn with_checks_of(mut self, of: Oid) -> WriteHost {
        self.checks_of = of;
        self
    }

    /// The leaf host's bind at its first routed row (`write_route.rs`; the route host holds the binding's parts).
    pub(crate) fn bind_leaf(&mut self, cid: CommandId, snapshot: Option<Rc<SnapshotData<'static>>>, words: crate::ops::modifytable::write_check::ParamWords) -> PgResult<bool> {
        self.words = words;
        // D675: ON CONFLICT's transaction-snapshot checks read the leaf's es_snapshot too (ExecCheckTIDVisible / ExecCheckTupleVisible)
        self.snapshot = snapshot.clone();
        self.bind_pg(cid, snapshot)
    }

    /// The statement's modified columns (the registry's, per admission: two statements sharing one program differ here).
    pub fn with_modified(mut self, cols: Option<ModifiedCols>) -> WriteHost {
        self.modified = cols;
        self
    }

    /// D829: an UPDATE / DELETE (D3367: a MERGE) of a partitioned root pruned to no leaf: the root itself is the result
    /// relation (its own rootRelation, CheckValidResultRel's RELKIND_PARTITIONED_TABLE arm) over the planner's dummy
    /// Result, which the lowerer alone admits it by (lowerv4 dummy.rs `pruned_root`): its statement triggers fire, no row
    /// reaches it.
    fn pruned_root(&self, heap: &Relation<'_>) -> bool {
        heap.rd_rel.relkind == RELKIND_PARTITIONED_TABLE && self.under_root == Some(heap.rd_id) && matches!(self.recipe.cmd, Cmd::Update | Cmd::Delete | Cmd::Merge)
    }

    /// R4c (V4R-350): a target's host under a multi-target host fetches the version each UPDATE replaced (`WritePlan.old`).
    pub(crate) fn with_old(mut self, old: bool) -> WriteHost {
        self.old = old;
        self
    }

    /// D774: a routed leaf's host takes the route host's system-column flags (write_route.rs).
    pub(crate) fn with_sys(mut self, on: bool, header: bool) -> WriteHost {
        self.sys = crate::ops::modifytable::write_sys::SysCols::with(on, header);
        self
    }

    /// V4R-501: a target's host under a multi-target host fills the replaced version's system columns (`WritePlan.sys_old`).
    pub(crate) fn with_sys_old(mut self, on: bool) -> WriteHost {
        self.sys.set_old(on);
        self
    }

    /// D774: the leaf's last row's system columns, through its route host (`virtual_row`: PG's root slot was virtual).
    /// V4R-349: a moved row's are its destination's, the move host's insert (`Mover::sys`).
    pub(crate) fn sys_fill(&self, values: &mut [Word], isnull: &mut [u8], virtual_row: bool) -> PgResult<bool> {
        match self.mover.as_ref().filter(|_| self.moved) {
            Some(m) => m.borrow().sys(values, isnull),
            None => self.sys.fill(values, isnull, virtual_row),
        }
    }

    pub(crate) fn heap_oid(&self) -> Oid {
        self.recipe.heap
    }

    pub fn scratch_used(&self) -> usize {
        self.scratch.used()
    }

    /// The per-bind context's bytes (the constraint / generated / check states; reset at unbind).
    pub fn bind_scratch_used(&self) -> usize {
        self.oc_scratch.used()
    }

    /// The two scratches' bytes by root (node, exec, bind): the host's, then the per-bind one.
    pub fn scratch_parts(&self) -> [[usize; 3]; 2] {
        [self.scratch.used_parts(), self.oc_scratch.used_parts()]
    }

    /// Both scratches and the index maintenance's AM cache context (and every context under them) on `to` from now, their
    /// bytes moving with them (`NodeScratch::home`): a host a holder keeps past the statement that made it (the point
    /// path's kept pieces) is that holder's.
    pub fn home(&mut self, to: memledger::Owner) {
        self.scratch.home(to);
        self.oc_scratch.home(to);
        self.maint.home(to);
    }

    /// The index maintenance's AM cache context's bytes (`scratch_parts` counts the scratches).
    pub fn maint_bytes(&self) -> usize {
        self.maint.cache_bytes()
    }

    /// The modified columns the host reads (its own copies after its first bind).
    pub fn modified_cols(&self) -> Option<ModifiedCols> {
        self.modified
    }

    fn bind_pg(&mut self, cid: CommandId, snapshot: Option<Rc<SnapshotData<'static>>>) -> PgResult<bool> {
        let mcx = self.scratch.mcx();
        if !self.cols_owned {
            // the statement's bitmaps into the node context, once per host: the first bind is the deriving statement's, whose plan
            // `modified` points into; a parked Node's later binds are other statements' (the park key carries the words, H16 (b))
            if let Some((ins, upd)) = self.modified {
                self.modified = Some((mcx::alloc_leak_in(mcx, ins.clone_in(mcx)?)?, mcx::alloc_leak_in(mcx, upd.clone_in(mcx)?)?)); // alloc: Node
            }
            self.cols_owned = true;
        }
        // the result relation's lock is the executor's (ExecInitModifyTable opens it at the RTE's RowExclusiveLock):
        // a sourceless INSERT has no scan host that would have taken one, and the plan cache's re-execution none either
        lmgr_seams::lock_relation_oid::call(self.recipe.heap, RowExclusiveLock)?;
        if let Some(root) = self.under_root {
            // ExecInitModifyTable's rootRelation: locked at the range table's mode (the plan cache's re-execution has none)
            lmgr_seams::lock_relation_oid::call(root, RowExclusiveLock)?;
        }
        let heap = open_heap(mcx, &self.parked, self.recipe.heap)?;
        if let Some(e) = result_rel_error(&heap) {
            heap.close(NoLock)?;
            return Err(e);
        }
        // PGRCSERVE M1 (tableam T4, ExecInitModifyTable's): the table AM's own refusal of the statement, before any row
        if let Some(op) = cmd_type(self.recipe.cmd).filter(|_| heap.rd_rel.relkind == RELKIND_RELATION) {
            if let Err(e) = tableam::table_modify_admit(&heap, op, self.recipe.conflict != Conflict::None) {
                heap.close(NoLock)?;
                return Err(e);
            }
        }
        if heap.rd_rel.relkind == RELKIND_VIEW {
            // member TargetView: the INSTEAD OF row trigger is the write (INSERT: write_view.rs; UPDATE / DELETE:
            // write_view_rows.rs, H-d; MERGE: each subcommand's, H-f); ON CONFLICT into a view is the lowerer's refusal
            if self.recipe.cmd == Cmd::Select || self.recipe.conflict != Conflict::None {
                heap.close(NoLock)?;
                return Ok(false);
            }
            let bound = match self.recipe.cmd {
                Cmd::Insert => crate::ops::modifytable::write_view::ViewInsert::bind(&heap, false).map(|v| self.view = Some(v)),
                Cmd::Merge => self.bind_merge_view(&heap),
                cmd => crate::ops::modifytable::write_view_rows::ViewRows::bind(&heap, cmd, self.modified.map(|m| m.1), false).map(|v| self.view_rows = Some(v)),
            };
            if let Err(e) = bound {
                heap.close(NoLock)?;
                return Err(e);
            }
        } else if !(writable_target(&heap) || (matches!(self.recipe.cmd, Cmd::Insert | Cmd::Update | Cmd::Delete) && writable_am(&heap)) || (self.routed && heap.rd_rel.relkind == RELKIND_RELATION) || self.pruned_root(&heap)) {
            // V4R-303: a routed leaf is written whatever its table AM (ExecInitPartitionInfo re-admits it at its first row: a
            // leaf given another AM since admission goes through tableam, as ExecInsert's table_tuple_insert)
            heap.close(NoLock)?;
            return Ok(false);
        }
        if let Some(root) = self.under_root {
            // ExecInitModifyTable's rootRelation (the pruned write's parent, or the inheritance parent of a write whose
            // one result relation is a child or the parent itself, V4R-214): opened for the statement, its statement
            // triggers bound here (`fireBSTriggers` on the result relation), the leaf's never
            let r = match table::table_open(mcx, root, NoLock) {
                Ok(r) => r,
                Err(e) => {
                    heap.close(NoLock)?;
                    return Err(e);
                }
            };
            // under a partitioned root the leaf is still a partition; an inheritance child's link is not re-read
            // (write_route.rs revalidate_target: the plan's locks hold the tree)
            if r.rd_id != heap.rd_id && r.rd_rel.relkind == RELKIND_PARTITIONED_TABLE && !heap.rd_rel.relispartition {
                heap.close(NoLock)?;
                r.close(NoLock)?;
                return Ok(false);
            }
            self.root = Some(r);
        }
        // CheckValidResultRel's publication check (ExecInitModifyTable): an UPDATE / DELETE of a published table
        // without the replica identity its publication needs is an ERROR before any row, and before fireBSTriggers
        // (D978: bind_triggers fires the BEFORE STATEMENT triggers)
        let cmd = match self.recipe.cmd {
            Cmd::Update => Some(CmdType::CMD_UPDATE),
            Cmd::Delete => Some(CmdType::CMD_DELETE),
            Cmd::Insert if self.recipe.conflict == Conflict::Update => Some(CmdType::CMD_UPDATE), // ON CONFLICT DO UPDATE updates
            _ => None,
        };
        // (a view is never published: CheckValidResultRel's view arm has no replica identity check)
        if let Some(cmd) = cmd.filter(|_| heap.rd_rel.relkind != RELKIND_VIEW && execreplication_seams::check_cmd_replica_identity::is_installed()) {
            if let Err(e) = execreplication_seams::check_cmd_replica_identity::call(mcx, &heap, cmd) {
                heap.close(NoLock)?;
                self.close_root()?;
                return Err(e);
            }
        }
        match self.bind_triggers(&heap, snapshot) {
            Ok(true) => {}
            other => {
                heap.close(NoLock)?;
                self.close_root()?;
                return other;
            }
        }
        self.defer = self.defer_rows(&heap);
        // a view's UPDATE / DELETE row carries the wholerow junk after its cells (H-d)
        let natts = heap.rd_att.natts as usize + (self.view_rows.is_some() || self.view_del.is_some()) as usize;
        if self.values.len() < natts {
            self.values.resize(natts, Datum::null()); // alloc: Node
            self.nulls.resize(natts, false); // alloc: Node
        }
        // D613: the indexes (and ON CONFLICT's arbiter state) open at the first row that inserts or updates (`open_indexes`),
        // as ExecInsert / ExecUpdatePrologue open them, never here
        if self.slot.is_none() {
            self.slot = Some(tableam::table_slot_create(mcx, &heap)?);
        }
        if let Some(c) = self.checks.as_ref() {
            // the check states are this execution's (ExecInitModifyTable's per execution): in the per-bind context,
            // dropped at unbind before its reset, never in the node context a parked Node keeps (D263)
            let c = match self.checks_onto(&heap, c) {
                Ok(c) => c,
                Err(e) => {
                    heap.close(NoLock)?;
                    return Err(e);
                }
            };
            match crate::ops::modifytable::write_check::WriteChecks::bind(self.oc_scratch.exec_mcx(), &heap, c, self.words) {
                Ok(w) => self.wco = Some(w),
                Err(e) => {
                    heap.close(NoLock)?;
                    return Err(e);
                }
            }
        }
        if self.recipe.conflict != Conflict::None && self.existing.is_none() {
            self.existing = Some(tableam::table_slot_create(mcx, &heap)?);
        }
        self.cid = cid;
        self.crosscheck = crosscheck_now();
        if matches!(self.recipe.cmd, Cmd::Update | Cmd::Delete) && heap.rd_rel.relkind == RELKIND_RELATION && !heap_rel(&heap) {
            tableam::table_modify_will_refetch(&heap);
            self.refetch = true;
        }
        self.heap = Some(heap);
        self.recheck_pos = ScanPos::NONE;
        self.bound = true;
        Ok(true)
    }

    /// PGRCSERVE M1: the plan's `defer` on another table AM, unless a row trigger reads each row, ON CONFLICT or a moved row.
    fn defer_rows(&self, heap: &Relation<'_>) -> bool {
        self.recipe.defer && self.recipe.conflict == Conflict::None && !self.moved_in && writable_am(heap) && !self.triggers.as_ref().is_some_and(|t| t.reads_rows())
    }

    /// The row-trigger bind with the statement half it takes: a routed leaf's is the route host's root's, a
    /// pruned leaf's the parent's (bound now, the BEFORE STATEMENT twins run before any row), a plain target's
    /// its own. `Ok(false)` = a refusal by name (nothing fired yet).
    fn bind_triggers(&mut self, heap: &Relation<'static>, snapshot: Option<Rc<SnapshotData<'static>>>) -> PgResult<bool> {
        let conflict_update = self.recipe.conflict == Conflict::Update;
        // a partition's triggers read the root's modified columns in the leaf's numbering (write_route.rs; bind-lived)
        let trigger_cols = crate::ops::modifytable::write_route::leaf_modified(self.oc_scratch.exec_mcx(), self.modified, heap, self.root.as_ref(), self.under_root)?;
        let stmt_of = if self.routed {
            StmtOf::Root(self.root_stmt.clone(), self.root.as_ref().expect("the route host's root"))
        } else if self.under_root.is_some() {
            let root = self.root.as_ref().expect("opened");
            match crate::ops::modifytable::write_route::root_stmt(root, self.recipe.cmd, conflict_update, self.merge, self.modified)? {
                Ok(s) => StmtOf::Root(s, root),
                Err(_) => return Ok(false),
            }
        } else {
            StmtOf::Own
        };
        self.triggers = match TriggerHost::bind_moving(heap, self.recipe.cmd, conflict_update, self.merge, self.mover.is_some(), trigger_cols, snapshot, stmt_of)? {
            Bound::None => None,
            Bound::Host(t) => Some(t),
            Bound::Refused(_) => return Ok(false),
        };
        Ok(true)
    }

    /// H-f: MERGE into a view (Decision 8): CheckValidResultRel per subcommand, in INSERT / UPDATE / DELETE order, each
    /// with MERGE's hint. A MERGE UPDATE's site carries the FULL new image (the view's natts cells, then the wholerow),
    /// since each action has its own SET list: NEW is the cells, not OLD with the statement's updatedCols replaced.
    fn bind_merge_view(&mut self, heap: &Relation<'static>) -> PgResult<()> {
        if self.merge.insert {
            self.view = Some(crate::ops::modifytable::write_view::ViewInsert::bind(heap, true)?);
        }
        if self.merge.update {
            self.view_rows = Some(crate::ops::modifytable::write_view_rows::ViewRows::bind(heap, Cmd::Update, None, true)?);
        }
        if self.merge.delete {
            self.view_del = Some(crate::ops::modifytable::write_view_rows::ViewRows::bind(heap, Cmd::Delete, None, true)?);
        }
        Ok(())
    }

    fn close_root(&mut self) -> PgResult<()> {
        if self.under_root.is_some() {
            if let Some(root) = self.root.take() {
                root.close(NoLock)?;
            }
        }
        Ok(())
    }

    fn unbind_pg(&mut self) -> PgResult<()> {
        if !self.bound {
            return Ok(());
        }
        self.bound = false;
        if std::mem::replace(&mut self.refetch, false) {
            tableam::table_modify_refetch_end();
        }
        if let Some(slot) = self.slot.as_mut() {
            exectuples::exec_clear_tuple(slot, self.scratch.mcx());
        }
        for slot in [self.existing.as_mut(), self.epq.as_mut()].into_iter().flatten() {
            exectuples::exec_clear_tuple(slot, self.scratch.mcx());
        }
        self.epq_ready = false;
        self.clear_deleted();
        self.snapshot = None;
        self.lockmode = None;
        self.triggers = None;
        self.view = None;
        self.view_rows = None;
        self.view_del = None;
        self.wco = None;
        self.park = Park::None;
        self.words = crate::ops::modifytable::write_check::ParamWords::NONE;
        self.partition.reset();
        // H-i (D585) / D608: the constraint and generated states are this execution's (ExecInitModifyTable's per
        // execution): built in the per-bind context (oc_scratch's exec context, reset only here below, never per row: the
        // write's own exec context is reset after every row), dropped here before its reset, rebuilt at the next bind
        self.constraints.reset();
        self.generated.reset(self.oc_scratch.exec_mcx());
        let closed = self.oc.take().map_or(Ok(()), execindexing::ExecCloseIndices);
        self.oc_scratch.reset_exec();
        closed?;
        self.maint.unbind()?;
        self.parked = park(self.heap.take())?;
        self.moved = false;
        if self.mover_owned {
            // the move host's destinations and router go before the root it routes through
            if let Some(m) = self.mover.as_ref() {
                m.borrow_mut().unbind()?;
            }
        }
        self.close_root()?;
        self.scratch.reset_exec();
        Ok(())
    }

    /// ExecUpdate / ExecDelete's outcome mapping. TM_Updated under read
    /// committed: lock the newest version of the row through the update chain
    /// (table_tuple_lock, FIND_LAST_VERSION, blocking) — it becomes the EPQ
    /// test tuple (`Recheck`, position in `recheck_pos`), a deleted chain end
    /// is a skip; under a transaction snapshot it is a serialization failure
    /// whose message names the concurrent change (update / delete, D246).
    fn outcome(&mut self, heap: &Relation<'static>, what: &'static str, r: TM_Result, tmfd: &mut TM_FailureData, otid: &types_tuple::ItemPointerData, lockmode: LockTupleMode) -> PgResult<Outcome> {
        let xact_snapshot = crate::xact_snapshot();
        match r {
            TM_Result::TM_Ok => Ok(Outcome::Done),
            TM_Result::TM_Updated if xact_snapshot => Err(crate::serialization_failure()),
            TM_Result::TM_Deleted if xact_snapshot => Err(concurrent_delete()),
            // W6: a MERGE's MATCHED action over a deleted row: the row is NOT MATCHED now (ExecMergeMatched)
            TM_Result::TM_Deleted if self.recipe.cmd == Cmd::Merge => Ok(Outcome::NotMatched),
            TM_Result::TM_Deleted => Ok(Outcome::Skipped),
            TM_Result::TM_SelfModified if self.recipe.cmd == Cmd::Merge => Err(merge_self_modified(tmfd, self.cid)),
            TM_Result::TM_SelfModified if tmfd.cmax == self.cid => Ok(Outcome::Skipped),
            TM_Result::TM_Updated => {
                let slot = self.slot.as_mut().expect("bound");
                let r = tableam::table_tuple_lock(self.scratch.exec_mcx(), heap, otid, &None, slot, self.cid, lockmode, LockWaitPolicy::LockWaitBlock, TUPLE_LOCK_FLAG_FIND_LAST_VERSION, tmfd)?;
                let out = match r {
                    TM_Result::TM_Ok => {
                        self.recheck_pos = pos_of(self.recipe.heap, &slot.base().tts_tid);
                        Ok(Outcome::Recheck)
                    }
                    TM_Result::TM_Deleted if self.recipe.cmd == Cmd::Merge => Ok(Outcome::NotMatched),
                    TM_Result::TM_Deleted => Ok(Outcome::Skipped),
                    TM_Result::TM_SelfModified if self.recipe.cmd == Cmd::Merge => Err(merge_self_modified(tmfd, self.cid)),
                    TM_Result::TM_SelfModified if tmfd.cmax == self.cid => Ok(Outcome::Skipped),
                    other => Err(tm_error(what, other, tmfd, self.cid)),
                };
                exectuples::exec_clear_tuple(slot, self.scratch.mcx());
                out
            }
            other => Err(tm_error(what, other, tmfd, self.cid)),
        }
    }

    /// ExecInsert / ExecUpdatePrologue's first step (D613): the target's indexes opened at the first row that inserts or
    /// updates, before its BEFORE ROW triggers, and ON CONFLICT's arbiter state (`ExecOpenIndices(.., true)`) with them;
    /// a DELETE never opens them. Both are released at `unbind` (ExecCloseIndices). `Ok(false)`: an index the plan's recipe
    /// no longer describes (the bind's refusal before D613, now at the first such row).
    fn open_indexes(&mut self, op: WriteOp) -> PgResult<bool> {
        if op == WriteOp::Delete {
            return Ok(true);
        }
        let heap = self.heap.as_ref().expect("gravity: write on an unbound writer");
        let fresh = !self.maint.is_open();
        if !self.maint.open(self.scratch.mcx(), heap)? {
            return Ok(false);
        }
        if fresh && self.recipe.cmd == Cmd::Update {
            // H-a (D235): the indexUnchanged hints over ExecGetAllUpdatedCols (the stored generated columns an UPDATE
            // recomputes join its updatedCols), once per execution with the indexes open (an expression key's walk reads
            // its index); computed into the row's context, nothing kept from it
            let emcx = self.scratch.exec_mcx();
            let all = self.all_updated(emcx, heap)?;
            self.maint.set_updated(emcx, all.as_ref())?;
        }
        if self.recipe.conflict != Conflict::None && self.oc.is_none() {
            self.oc = Some(execindexing::ExecOpenIndices(self.oc_scratch.exec_mcx(), heap, true)?);
        }
        Ok(true)
    }

    fn write_pg(&mut self, op: WriteOp, pos: ScanPos, values: &[Word], isnull: &[u8]) -> PgResult<Outcome> {
        let heap = self.heap.take().expect("gravity: write on an unbound writer");
        let r = self.write_with(&heap, op, pos, values, isnull);
        // W6-D1112: a parked row keeps its BEFORE ROW row and the view host's row context
        let done = self.park == Park::None;
        if let Some(t) = self.triggers.as_mut().filter(|_| done) {
            t.row_done();
        }
        if let Some(v) = self.view.as_mut().filter(|_| done) {
            v.row_done();
        }
        self.heap = Some(heap);
        r
    }

    /// V4R-303: a routed leaf of another table AM (given it since admission): ExecInsert's table_tuple_insert over the
    /// row in the leaf's slot; the AM's TID for the indexes and the AR triggers. D3293: the leaf is filed for the
    /// statement's end, which finishes the AM's bulk insert (trigger_stmt.rs `finish_am`: an AM that buffers the row
    /// publishes it there).
    fn insert_via_am(&mut self, mcx: Mcx<'static>, heap: &Relation<'static>, n: usize) -> PgResult<ItemPointerData> {
        let slot = self.slot.as_mut().expect("gravity: write before bind (the leaf's slot)");
        exectuples::exec_clear_tuple(slot, mcx);
        // The leaf's slot is virtual (table_slot_callbacks) and lives in `node`, but `mcx` is the per-row exec context
        // (row_op resets it after each row). A virtual slot keeps its materialize buffer across clears (clear_virtual),
        // so the one an earlier row's table_tuple_insert allocated here now points into bytes the reset handed back out:
        // reused, the next materialize copies the row into memory heap_form_tuple then allocates over, and the stored
        // tuple is corrupt (isowrite-am-routed-leaf). Drop it, so this row's materialize allocates in the live `mcx`.
        exectuples::exec_drop_slot_scratch(slot, mcx);
        {
            let base = slot.base_mut();
            base.tts_values[..n].copy_from_slice(&self.values[..n]);
            base.tts_isnull[..n].copy_from_slice(&self.nulls[..n]);
        }
        exectuples::exec_store_virtual_tuple(slot);
        let options = if self.defer { tableam_vocab::TABLE_INSERT_DEFER } else { 0 };
        tableam::table_tuple_insert(mcx, heap, slot, self.cid, options, None)?;
        crate::ops::modifytable::trigger_stmt::am_written(heap.rd_id);
        Ok(slot.base().tts_tid)
    }

    /// PGRCSERVE M3: ExecUpdateAct's table_tuple_update over another table AM (`otid` = the rows road's TID); the new
    /// version's TID is left in the slot.
    #[allow(clippy::too_many_arguments)]
    fn update_via_am(&mut self, heap: &Relation<'static>, otid: &ItemPointerData, n: usize, tmfd: &mut TM_FailureData, lockmode: &mut LockTupleMode, update_indexes: &mut TU_UpdateIndexes) -> PgResult<TM_Result> {
        let mcx = self.scratch.exec_mcx();
        let slot = self.slot.as_mut().expect("gravity: write before bind (the target's slot)");
        exectuples::exec_clear_tuple(slot, mcx);
        exectuples::exec_drop_slot_scratch(slot, mcx);
        {
            let base = slot.base_mut();
            base.tts_values[..n].copy_from_slice(&self.values[..n]);
            base.tts_isnull[..n].copy_from_slice(&self.nulls[..n]);
        }
        exectuples::exec_store_virtual_tuple(slot);
        let r = tableam::table_tuple_update(mcx, heap, otid, slot, self.cid, &self.snapshot, &self.crosscheck, true, tmfd, lockmode, update_indexes)?;
        crate::ops::modifytable::trigger_stmt::am_written(heap.rd_id);
        Ok(r)
    }

    /// ExecDeleteAct: heap_delete, or (PGRCSERVE M3) table_tuple_delete over another table AM.
    fn delete_tid(&mut self, heap: &Relation<'static>, tid: &ItemPointerData, tmfd: &mut TM_FailureData, changing_part: bool) -> PgResult<TM_Result> {
        if heap_rel(heap) {
            return heapam::heap_delete(heap, tid, self.cid, self.crosscheck.as_deref(), true, tmfd, changing_part);
        }
        let r = tableam::table_tuple_delete(self.scratch.exec_mcx(), heap, tid, self.cid, &self.snapshot, &self.crosscheck, true, tmfd, changing_part)?;
        crate::ops::modifytable::trigger_stmt::am_written(heap.rd_id);
        Ok(r)
    }

    /// ExecInsert / ExecUpdate / ExecDelete's first step, the BEFORE ROW prologue: `Some(outcome)` ends the row
    /// (skipped, or the old row moved and the runner rechecks), `None` writes it with the cells `before.words` gives.
    fn before_row(&mut self, before: &mut Option<crate::ops::modifytable::trigger_before::BeforeRow>, op: WriteOp, pos: ScanPos, mcx: Mcx<'static>, heap: &Relation<'static>, n: usize) -> PgResult<Option<Outcome>> {
        let Some(b) = before.as_mut() else { return Ok(None) };
        match op {
            WriteOp::Insert => Ok((!b.insert(mcx, heap, &mut self.values[..n], &mut self.nulls[..n])?).then_some(Outcome::Skipped)),
            // D822: an INSERT's update is ON CONFLICT DO UPDATE's: its EXCLUDED (the INSERT trigger's row) stays alive
            WriteOp::Update => match b.update(mcx, heap, &tid_of(pos), self.cid, &mut self.values[..n], &mut self.nulls[..n], self.recipe.cmd == Cmd::Insert)? {
                Pre::Go => Ok(None),
                Pre::Skip => Ok(Some(Outcome::Skipped)),
                Pre::NotMatched => Ok(Some(Outcome::NotMatched)),
                Pre::Recheck(p) => {
                    self.recheck_pos = p;
                    Ok(Some(Outcome::Recheck))
                }
            },
            WriteOp::Delete => match b.delete(mcx, heap, &tid_of(pos), self.cid)? {
                Pre::Go => Ok(None),
                Pre::Skip => Ok(Some(Outcome::Skipped)),
                Pre::NotMatched => Ok(Some(Outcome::NotMatched)),
                Pre::Recheck(p) => {
                    self.recheck_pos = p;
                    Ok(Some(Outcome::Recheck))
                }
            },
        }
    }

    fn write_with(&mut self, heap: &Relation<'static>, op: WriteOp, pos: ScanPos, values: &[Word], isnull: &[u8]) -> PgResult<Outcome> {
        // W6-D1112: a resumed row op continues past its prologue (the row's cells, its BEFORE ROW row and generated values stand)
        if self.park == Park::None {
            if let Some(o) = self.prologue(heap, op, pos, values, isnull)? {
                return Ok(o);
            }
        }
        let n = values.len();
        let mut tmfd = TM_FailureData::default();
        let taken = self.triggers.as_mut().and_then(|t| t.before.as_mut()).and_then(|b| b.take_words());
        let computed = self.generated.take_words();
        let (values, isnull) = match (&computed, &taken) {
            (Some((v, n)), _) | (None, Some((v, n))) => (&v[..], &n[..]),
            (None, None) => (values, isnull),
        };
        let r = self.write_op(heap, op, pos, values, isnull, n, &mut tmfd);
        if let (Some(w), Some(b)) = (taken, self.triggers.as_mut().and_then(|t| t.before.as_mut())) {
            b.keep(w);
        }
        if let Some(w) = computed {
            self.generated.keep(w);
        }
        r
    }

    /// The row op's prologue: the row's cells, H-f's MERGE check, the BEFORE ROW triggers (`Some` ends the row) and the
    /// stored generated columns.
    fn prologue(&mut self, heap: &Relation<'static>, op: WriteOp, pos: ScanPos, values: &[Word], isnull: &[u8]) -> PgResult<Option<Outcome>> {
        let mcx = self.scratch.exec_mcx();
        let n = values.len();
        self.clear_deleted();
        self.moved = false;
        // bounded("key-columns: one cell per column of the row, at most MaxTupleAttributeNumber (1664) columns")
        for k in 0..n {
            self.values[k] = Datum::from_u64(values[k]);
            self.nulls[k] = isnull[k] != 0;
        }
        if self.recipe.cmd == Cmd::Merge && op != WriteOp::Insert && self.view_rows.is_none() && self.view_del.is_none() {
            // H-f: ExecMergeMatched's RLS USING check of the target's current version, after the action's WHEN (the IR's)
            // and before the action's prologue; after a recheck the call carries the newer version's position
            self.merge_check(mcx, heap, op, pos)?;
        }
        // the BEFORE ROW prologue (ExecInsert / ExecUpdate / ExecDelete's first step): the row's cells may be the trigger's
        let mut before = self.triggers.as_mut().and_then(|t| t.before.take());
        let pre = self.before_row(&mut before, op, pos, mcx, heap, values.len());
        if let Some(t) = self.triggers.as_mut() {
            t.before = before;
        }
        if let Some(o) = pre? {
            return Ok(Some(o));
        }
        // ExecComputeStoredGenerated: after the BEFORE ROW triggers, before the constraints (the cells are the host's row)
        if op != WriteOp::Delete {
            self.generated.compute(self.oc_scratch.exec_mcx(), mcx, heap, &mut self.values[..n], &mut self.nulls[..n])?;
        } else {
            self.generated.forget();
        }
        Ok(None)
    }

    /// ExecDelete's RETURNING (M4-RET (iii)): the deleted version, the one this call deleted (after an EPQ retry the newer
    /// version the recheck locked, whose position this call carried), fetched under SnapshotAny and deformed whole; a
    /// dropped column reads NULL. Only for a site with cells or RETURNING's system columns (D870): M1's DELETE end has
    /// neither and never fetches.
    fn fetch_deleted(&mut self, heap: &Relation<'static>, tid: &types_tuple::ItemPointerData) -> PgResult<()> {
        self.fetch_version(heap, tid, "failed to fetch deleted tuple for DELETE RETURNING")?;
        self.deleted_filled = true;
        Ok(())
    }

    /// The version at `tid` under SnapshotAny into the `deleted` slot, deformed whole; a dropped column reads NULL. M4-RET
    /// R4a: an UPDATE's replaced version (`otid`, after an EPQ retry the version the recheck locked) for RETURNING OLD.
    fn fetch_version(&mut self, heap: &Relation<'static>, tid: &types_tuple::ItemPointerData, what: &str) -> PgResult<()> {
        let mcx = self.scratch.mcx();
        if self.deleted.is_none() {
            self.deleted = Some(tableam::table_slot_create(mcx, heap)?);
        }
        let slot = self.deleted.as_mut().expect("created");
        if !tableam::table_tuple_fetch_row_version(mcx, heap, tid, &None, slot)? {
            return Err(Box::new(PgError::error(what.to_string()))); // alloc: Cold
        }
        exectuples::slot_getallattrs(slot);
        Ok(())
    }

    /// The last row's deleted version released (its buffer pin): at the next row op and at unbind.
    fn clear_deleted(&mut self) {
        self.sys.clear();
        if core::mem::take(&mut self.deleted_filled) | core::mem::take(&mut self.old_filled) {
            if let Some(slot) = self.deleted.as_mut() {
                exectuples::exec_clear_tuple(slot, self.scratch.mcx());
            }
        }
    }

    /// H-f: WCO_RLS_MERGE_UPDATE_CHECK / WCO_RLS_MERGE_DELETE_CHECK over `ri_oldTupleSlot`, the version at `pos` fetched
    /// under SnapshotAny (ExecMergeMatched's `table_tuple_fetch_row_version`); nothing to fetch without such a policy.
    fn merge_check(&mut self, mcx: Mcx<'static>, heap: &Relation<'static>, op: WriteOp, pos: ScanPos) -> PgResult<()> {
        let kind = if op == WriteOp::Update { WCOKind::WCO_RLS_MERGE_UPDATE_CHECK } else { WCOKind::WCO_RLS_MERGE_DELETE_CHECK };
        let (Some(w), Some(slot)) = (self.wco.as_mut(), self.slot.as_mut()) else { return Ok(()) };
        if !w.has(kind) {
            return Ok(());
        }
        let hmcx = self.scratch.mcx();
        if !tableam::table_tuple_fetch_row_version(hmcx, heap, &tid_of(pos), &None, slot)? {
            return Err(Box::new(PgError::error("failed to fetch the target tuple".to_string()))); // alloc: Cold
        }
        exectuples::slot_getallattrs(slot);
        let n = heap.rd_att.natts as usize;
        let r = {
            let b = slot.base();
            w.check(hmcx, mcx, heap, &b.tts_values[..n], &b.tts_isnull[..n], kind, self.modified, self.root.as_ref())
        };
        exectuples::exec_clear_tuple(slot, hmcx);
        r
    }

    fn wco_check(&mut self, mcx: Mcx<'static>, heap: &Relation<'static>, n: usize, kind: WCOKind) -> PgResult<()> {
        match self.wco.as_mut() {
            Some(w) => w.check(self.scratch.mcx(), mcx, heap, &self.values[..n], &self.nulls[..n], kind, self.modified, self.root.as_ref()),
            None => Ok(()),
        }
    }

    /// W6-D1112: `wco_check` at a plain row op's check points, parking at a SubPlan read (`Ok(true)`; `at` = where the
    /// resume enters). A routed INSERT's leaf parks too (updatable_views#978: the route host hands the parked call and the
    /// resume to it, write_route.rs), and a moved row's destination (subplan D1112 row B: its move host hands them to the
    /// source's host, `Park::Move`, write_move.rs).
    fn wco_park(&mut self, mcx: Mcx<'static>, heap: &Relation<'static>, n: usize, kind: WCOKind, at: Park) -> PgResult<bool> {
        let r = match self.wco.as_mut() {
            Some(w) => w.check_parked(self.scratch.mcx(), mcx, heap, &self.values[..n], &self.nulls[..n], kind, self.modified, self.root.as_ref()),
            _ => self.wco_check(mcx, heap, n, kind).map(|()| false),
        };
        self.park = if matches!(r, Ok(true)) { at } else { Park::None };
        r
    }

    /// The row op's last step (ExecInsert / ExecUpdateEpilogue): the view check after the heap, the indexes and the AFTER
    /// ROW queue, then DO UPDATE's locked row released (its pin held through the update: the cells point into its page).
    /// W6-D1112: a park at the view check's SubPlan read keeps the row; its resume enters here.
    fn row_end(&mut self, mcx: Mcx<'static>, heap: &Relation<'static>, n: usize, o: Outcome) -> PgResult<Outcome> {
        if o == Outcome::Done && self.wco_park(mcx, heap, n, WCOKind::WCO_VIEW_CHECK, Park::View)? {
            return Ok(Outcome::SubPlan);
        }
        if let Some(existing) = self.existing.as_mut() {
            exectuples::exec_clear_tuple(existing, self.scratch.mcx());
        }
        Ok(o)
    }

    /// W6-D1112: a park no resume took (a new row op starts clean).
    fn unpark(&mut self) {
        self.park = Park::None;
        self.move_fk = None;
        if let Some(w) = self.wco.as_mut() {
            w.unpark(self.scratch.mcx());
        }
    }

    fn write_op(&mut self, heap: &Relation<'static>, op: WriteOp, pos: ScanPos, values: &[Word], isnull: &[u8], n: usize, tmfd: &mut TM_FailureData) -> PgResult<Outcome> {
        let mcx = self.scratch.exec_mcx();
        if self.view.is_some() || self.view_rows.is_some() || self.view_del.is_some() {
            return self.view_write(mcx, heap, op, n);
        }
        match op {
            // W6-D1112: a row op resumed at its view check's SubPlan read (everything before the check ran)
            WriteOp::Insert | WriteOp::Update if self.park == Park::View => self.row_end(mcx, heap, n, Outcome::Done),
            // ExecInsert's `onconflict != ONCONFLICT_NONE && ri_NumIndices > 0`: with no index open at this row (D765: the
            // statement's own first row dropped the arbiter, the relation's last index) the row goes in as a plain insert
            WriteOp::Insert if self.recipe.conflict != Conflict::None && self.oc.as_ref().is_some_and(|o| o.num_indices() > 0) => {
                // H-b: ExecInsert's RLS insert check runs before the constraints on the ON CONFLICT path too. D1112 (rxnested):
                // parked at a SubPlan read, the resume re-enters this arm and the check goes on where it parked
                if self.wco_park(mcx, heap, n, WCOKind::WCO_RLS_INSERT_CHECK, Park::Rls)? {
                    return Ok(Outcome::SubPlan);
                }
                self.constraints.check(self.oc_scratch.exec_mcx(), mcx, heap, &self.values[..n], &self.nulls[..n], self.modified, self.root.as_ref())?;
                self.partition.check(self.scratch.mcx(), self.oc_scratch.exec_mcx(), mcx, heap, &self.values[..n], &self.nulls[..n], self.modified, false, self.root.as_ref())?;
                self.insert_on_conflict(mcx, heap, n)
            }
            WriteOp::Insert => {
                // ExecInsert's wco_kind: a moved row (the insert half of an UPDATE's move) checks the UPDATE policies (V4R-329)
                let rls = if self.moved_in { WCOKind::WCO_RLS_UPDATE_CHECK } else { WCOKind::WCO_RLS_INSERT_CHECK };
                if self.wco_park(mcx, heap, n, rls, Park::Rls)? {
                    return Ok(Outcome::SubPlan);
                }
                self.constraints.check(self.oc_scratch.exec_mcx(), mcx, heap, &self.values[..n], &self.nulls[..n], self.modified, self.root.as_ref())?;
                // ExecInsert: a routed row's partition is rechecked only when a BEFORE ROW trigger could have moved it
                if !self.routed || self.triggers.as_ref().is_some_and(|t| t.before_insert_row()) {
                    self.partition.check(self.scratch.mcx(), self.oc_scratch.exec_mcx(), mcx, heap, &self.values[..n], &self.nulls[..n], self.modified, false, self.root.as_ref())?;
                }
                let tid = if tableam_vocab::TableAm::of(heap) == Some(tableam_vocab::TableAm::Heap) {
                    let mut tup = heaptuple::heap_form_tuple(mcx, &heap.rd_att, &self.values[..n], &self.nulls[..n])?;
                    heapam::heap_insert(heap, tup.as_tuple_mut(), self.cid, 0, None)?;
                    self.sys.from_tuple(tup.as_tuple(), heap.rd_id);
                    tup.as_tuple().t_self
                } else {
                    let tid = self.insert_via_am(mcx, heap, n)?;
                    self.sys.from_slot(self.slot.as_ref().expect("the leaf's slot"), heap.rd_id)?;
                    tid
                };
                self.inserted = tid;
                // PGRCSERVE M1: a deferred row has no position yet; its entries come once its AM places it (the drain)
                if !self.defer || types_tuple::itemptr::ItemPointerIsValid(&tid) {
                    self.maint.insert(mcx, heap, &tid, values, isnull, false, false)?;
                }
                if self.defer {
                    crate::ops::modifytable::trigger_stmt::drain_placed(self.oc_scratch.exec_mcx(), heap)?;
                }
                if let Some(t) = self.triggers.as_mut() {
                    if self.moved_in {
                        t.after_insert_moved(mcx, heap, self.root.as_ref(), tid, self.maint.rechecks())?;
                    } else {
                        t.after_insert(mcx, heap, self.root.as_ref(), tid, self.maint.rechecks())?;
                    }
                }
                self.row_end(mcx, heap, n, Outcome::Done)
            }
            WriteOp::Update => {
                let otid = tid_of(pos);
                // H-e (c): ExecUpdateAct's partition_constraint_failed under a root that routes: the row moves (no RLS UPDATE
                // check or constraints on the source, as ExecUpdateAct skips them)
                match self.mover.clone() {
                    // D1112 row B: resumed at the moved row's destination check (the delete half ran before it parked)
                    Some(m) if self.park == Park::Move => return self.move_resume(&m, heap),
                    // W6-D1112: resumed at the RLS check (the partition check passed before it parked)
                    _ if self.park == Park::Rls => {}
                    Some(m) if !self.partition.passes(self.scratch.mcx(), self.oc_scratch.exec_mcx(), mcx, heap, &self.values[..n], &self.nulls[..n])? => return self.move_row(&m, heap, pos, n, tmfd),
                    Some(_) => {}
                    None => self.partition.check(self.scratch.mcx(), self.oc_scratch.exec_mcx(), mcx, heap, &self.values[..n], &self.nulls[..n], self.modified, self.recipe.cmd == Cmd::Insert, self.root.as_ref())?,
                }
                if self.wco_park(mcx, heap, n, WCOKind::WCO_RLS_UPDATE_CHECK, Park::Rls)? {
                    return Ok(Outcome::SubPlan);
                }
                self.constraints.check(self.oc_scratch.exec_mcx(), mcx, heap, &self.values[..n], &self.nulls[..n], self.modified, self.root.as_ref())?;
                let mut lockmode = LockTupleMode::LockTupleNoKeyExclusive;
                let mut update_indexes = TU_UpdateIndexes::TU_None;
                let (r, tup) = if heap_rel(heap) {
                    let mut tup = heaptuple::heap_form_tuple(mcx, &heap.rd_att, &self.values[..n], &self.nulls[..n])?;
                    let r = heapam::heap_update(heap, &otid, tup.as_tuple_mut(), self.cid, self.crosscheck.as_deref(), true, tmfd, &mut lockmode, &mut update_indexes)?;
                    (r, Some(tup))
                } else {
                    (self.update_via_am(heap, &otid, n, tmfd, &mut lockmode, &mut update_indexes)?, None)
                };
                let o = self.outcome(heap, "update", r, tmfd, &otid, lockmode)?;
                let new_tid = match &tup {
                    Some(t) => t.as_tuple().t_self,
                    None => self.slot.as_ref().map_or(ItemPointerData::invalid(), |s| s.base().tts_tid),
                };
                let indexed = o == Outcome::Done && update_indexes != TU_UpdateIndexes::TU_None;
                if indexed {
                    self.maint.insert(mcx, heap, &new_tid, values, isnull, update_indexes == TU_UpdateIndexes::TU_Summarizing, true)?;
                }
                if o == Outcome::Done {
                    if let Some(t) = self.triggers.as_mut() {
                        // a HOT update inserted no index entry: no recheck (ExecUpdateEpilogue's empty recheckIndexes)
                        let recheck = if indexed { self.maint.rechecks() } else { &[] };
                        t.after_update(mcx, heap, self.root.as_ref(), otid, new_tid, self.recipe.cmd == Cmd::Insert, recheck)?;
                    }
                    match &tup {
                        Some(t) => self.sys.from_tuple(t.as_tuple(), heap.rd_id),
                        None => self.sys.from_slot(self.slot.as_ref().expect("the target's slot"), heap.rd_id)?,
                    }
                    if self.old {
                        // M4-RET R4a: RETURNING OLD reads the version this call replaced (PG 18 ExecUpdate's oldSlot)
                        self.fetch_version(heap, &otid, "failed to fetch the old tuple for UPDATE RETURNING OLD")?;
                        self.old_filled = true;
                    }
                }
                // the view check last, after RETURNING's version and system columns are kept (W6-D1112: it may park)
                self.row_end(mcx, heap, n, o)
            }
            WriteOp::Delete => {
                let tid = tid_of(pos);
                let r = self.delete_tid(heap, &tid, tmfd, false)?;
                let o = self.outcome(heap, "delete", r, tmfd, &tid, LockTupleMode::LockTupleExclusive)?;
                if o == Outcome::Done {
                    if let Some(t) = self.triggers.as_mut() {
                        t.after_delete(mcx, heap, self.root.as_ref(), tid)?;
                    }
                    // D870: a table without columns has no cell, but RETURNING's system columns (WritePlan.sys) read the
                    // deleted version too
                    if n > 0 || self.sys.on {
                        self.fetch_deleted(heap, &tid)?;
                        self.sys.from_slot(self.deleted.as_ref().expect("fetched"), heap.rd_id)?;
                    }
                }
                Ok(o)
            }
        }
    }
}

impl WriteHost {
    /// A view target's row: its INSTEAD OF row trigger is the write (no constraint, heap or index entry; H-d UPDATE /
    /// DELETE); a NULL return skips the row. A MERGE's op picks its action's host (H-f); the other hosts' last rows are
    /// forgotten, so RETURNING reads this row's.
    fn view_write(&mut self, mcx: Mcx<'static>, heap: &Relation<'static>, op: WriteOp, n: usize) -> PgResult<Outcome> {
        let merge = self.recipe.cmd == Cmd::Merge;
        let done = match op {
            WriteOp::Insert => {
                let natts = (heap.rd_att.natts as usize).min(n);
                // H-f: the other actions' last rows are no longer RETURNING's (D667: nor is this host's, until it is done)
                for o in [self.view_rows.as_mut(), self.view_del.as_mut()].into_iter().flatten() {
                    o.forget();
                }
                let Some(v) = self.view.as_mut() else { return Err(no_view_host("INSERT")) };
                let done = v.insert(mcx, heap, &self.values[..natts], &self.nulls[..natts])?;
                // D636: ExecInsert's view check (WITH CHECK OPTION of an outer view) over the row the trigger returned
                if done && self.wco.is_some() {
                    v.take_row(&mut self.values[..natts], &mut self.nulls[..natts]);
                    self.wco_check(mcx, heap, natts, WCOKind::WCO_VIEW_CHECK)?;
                }
                done
            }
            WriteOp::Update | WriteOp::Delete => {
                let (this, other) = if op == WriteOp::Delete && merge { (&mut self.view_del, &mut self.view_rows) } else { (&mut self.view_rows, &mut self.view_del) };
                if let Some(o) = other.as_mut() {
                    o.forget();
                }
                if let Some(o) = self.view.as_mut() {
                    o.forget();
                }
                let Some(v) = this.as_mut() else { return Err(no_view_host(if op == WriteOp::Update { "UPDATE" } else { "DELETE" })) };
                let done = v.write(heap, &self.values[..n], &self.nulls[..n])?;
                // D636: ExecUpdate's view check over the final NEW the INSTEAD OF UPDATE triggers returned (a DELETE has none)
                if done && op == WriteOp::Update && self.wco.is_some() {
                    if let Some((words, nulls)) = v.written() {
                        // bounded("key-columns: one cell per column of the row, at most MaxTupleAttributeNumber (1664) columns")
                        for k in 0..words.len().min(n) {
                            self.values[k] = Datum::from_u64(words[k]);
                            self.nulls[k] = nulls[k] != 0;
                        }
                    }
                    self.wco_check(mcx, heap, heap.rd_att.natts as usize, WCOKind::WCO_VIEW_CHECK)?;
                }
                done
            }
        };
        if merge && op == WriteOp::Insert {
            for v in [self.view_rows.as_mut(), self.view_del.as_mut()].into_iter().flatten() {
                v.forget();
            }
        }
        Ok(if done { Outcome::Done } else { Outcome::Skipped })
    }

    /// H-e (c): ExecCrossPartitionUpdate over the row at `pos` whose new values (`self.values[..n]`) left this leaf. Part 1
    /// is ExecDelete with changingPart: the BEFORE ROW DELETE triggers (NULL: the row stays, `Skipped`; a newer version:
    /// `Recheck`), `heap_delete` marking the tuple moved, its outcome as a DELETE's (READ COMMITTED's TM_Updated locks the
    /// newest version and the runner re-projects the SET over it and writes again: the retry that may not move; a version
    /// moved by another transaction is the lock's 40001; a transaction snapshot's 40001), the moved row's AFTER ROW half.
    /// Part 2 is the move host's insert through the root, then the foreign-key leg; the count is the insert's.
    fn move_row(&mut self, m: &Rc<RefCell<crate::ops::modifytable::write_move::Mover>>, heap: &Relation<'static>, pos: ScanPos, n: usize, tmfd: &mut TM_FailureData) -> PgResult<Outcome> {
        let mcx = self.scratch.exec_mcx();
        let tid = tid_of(pos);
        let mut before = self.triggers.as_mut().and_then(|t| t.before.take());
        let pre = match before.as_mut() {
            Some(b) => b.delete(mcx, heap, &tid, self.cid),
            None => Ok(Pre::Go),
        };
        if let Some(t) = self.triggers.as_mut() {
            t.before = before;
        }
        match pre? {
            Pre::Go => {}
            Pre::Skip => return Ok(Outcome::Skipped),
            Pre::NotMatched => return Ok(Outcome::NotMatched),
            Pre::Recheck(p) => {
                self.recheck_pos = p;
                return Ok(Outcome::Recheck);
            }
        }
        let r = self.delete_tid(heap, &tid, tmfd, true)?;
        // MOVE: under a MERGE ExecDelete's own test of a row a trigger of this command changed runs first (its
        // DELETE wording); one this command changed is left to ExecMergeMatched (`outcome`'s MERGE form)
        if matches!(r, TM_Result::TM_SelfModified) && self.recipe.cmd == Cmd::Merge && tmfd.cmax != self.cid {
            return Err(tm_error("delete", r, tmfd, self.cid));
        }
        let o = self.outcome(heap, "delete", r, tmfd, &tid, LockTupleMode::LockTupleExclusive)?;
        if o != Outcome::Done {
            return Ok(o);
        }
        if let Some(t) = self.triggers.as_mut() {
            t.after_delete_moved(mcx, heap, self.root.as_ref(), tid)?;
        }
        if self.old {
            // R4c (V4R-350): ExecDelete's saveOld: the version the move deleted, RETURNING OLD's (context->cpDeletedSlot)
            self.fetch_version(heap, &tid, "failed to fetch deleted tuple for DELETE RETURNING")?;
            self.old_filled = true;
        }
        let stmt = self.triggers.as_ref().and_then(|t| t.stmt());
        let fk = self.triggers.as_ref().is_some_and(|t| t.update_after_row());
        let o = {
            let mut mover = m.borrow_mut();
            mover.bind(self.cid, self.snapshot.clone(), self.modified, stmt, self.words)?;
            mover.insert(heap, &self.values[..n], &self.nulls[..n])?
        };
        self.moved_end(m, heap, tid, fk, o)
    }

    /// D1112 row B: a move parked at its destination's check option SubPlan read, resumed (the destination's row op
    /// continues at its RLS or view check); `Park::Move` again when it parks at the next one.
    fn move_resume(&mut self, m: &Rc<RefCell<crate::ops::modifytable::write_move::Mover>>, heap: &Relation<'static>) -> PgResult<Outcome> {
        let Some((tid, fk)) = self.move_fk else {
            return Err(Box::new(PgError::error("gravity: a moved row's resume with no parked move".to_string()))); // alloc: Cold
        };
        let o = m.borrow_mut().resume()?;
        self.moved_end(m, heap, tid, fk, o)
    }

    /// The move's end after its insert half's outcome `o`: parked at the destination's check (`Park::Move`, the source row
    /// `tid` and the foreign-key flag kept for the resume), else the foreign-key leg after a written row.
    fn moved_end(&mut self, m: &Rc<RefCell<crate::ops::modifytable::write_move::Mover>>, heap: &Relation<'static>, tid: ItemPointerData, fk: bool, o: Outcome) -> PgResult<Outcome> {
        if o == Outcome::SubPlan {
            self.park = Park::Move;
            self.move_fk = Some((tid, fk));
            return Ok(o);
        }
        self.park = Park::None;
        self.move_fk = None;
        if o == Outcome::Done {
            if fk {
                m.borrow_mut().foreign_key(self.scratch.exec_mcx(), heap, tid)?;
            }
            self.moved = true;
        }
        Ok(o)
    }
}

impl WriteHost {
    /// ExecInsert's ON CONFLICT arm, in PostgreSQL's order: the arbiters' pre-check
    /// (`ExecCheckIndexConstraints`; a committed conflict → DO NOTHING skips the row, its visibility
    /// checked under a transaction snapshot), then the speculative insertion under its token, the
    /// index inserts with the arbiters in deferred-check mode, the tuple confirmed or killed; a
    /// concurrent inserter winning the race restarts from the pre-check (`vlock`).
    fn insert_on_conflict(&mut self, mcx: Mcx<'static>, heap: &Relation<'static>, n: usize) -> PgResult<Outcome> {
        let host_mcx = self.scratch.mcx();
        let lockmode = if self.recipe.conflict == Conflict::Update { self.update_lockmode(heap)? } else { LockTupleMode::LockTupleNoKeyExclusive };
        let oc = self.oc.as_mut().expect("bound");
        let slot = self.slot.as_mut().expect("bound");
        let existing = self.existing.as_mut().expect("bound");
        let bind_mcx = self.oc_scratch.exec_mcx(); // D785 (D778's class): bind, not row: oc's IndexInfo caches (expression / predicate / AM state) live per bind, as oc does
        exectuples::exec_clear_tuple(slot, host_mcx);
        exectuples::exec_clear_tuple(existing, host_mcx);
        {
            let base = slot.base_mut();
            base.tts_values[..n].copy_from_slice(&self.values[..n]);
            base.tts_isnull[..n].copy_from_slice(&self.nulls[..n]);
        }
        exectuples::exec_store_virtual_tuple(slot);
        slot.base_mut().tts_tableOid = heap.rd_id;
        let mut invalid = ItemPointerData::default();
        types_tuple::itemptr::ItemPointerSetInvalid(&mut invalid);
        let mut recheck = mcx::PgVec::new_in(mcx);
        let r = loop {
            cancelpoll::interrupts::check()?;
            let mut conflict_tid = ItemPointerData::default();
            let state_mcx = crate::ops::modifytable::index_maint::state_mcx(oc, bind_mcx, mcx)?;
            if !execindexing::ExecCheckIndexConstraints(state_mcx, mcx, oc, heap, slot, existing, &invalid, &self.recipe.arbiters, &mut conflict_tid)? {
                match self.recipe.conflict {
                    Conflict::Update => {
                        // ExecOnConflictUpdate: the row locked (the executor's mode), a concurrent update or delete
                        // under read committed restarts from the pre-check, the tuple-visible rule, its cells deformed
                        let mut tmfd = TM_FailureData::default();
                        match tableam::table_tuple_lock(mcx, heap, &conflict_tid, &self.snapshot, existing, self.cid, lockmode, LockWaitPolicy::LockWaitBlock, 0, &mut tmfd)? {
                            TM_Result::TM_Ok => {}
                            TM_Result::TM_Invisible => {
                                if !tableam::table_tuple_fetch_row_version(mcx, heap, &conflict_tid, &None, existing)? {
                                    return Err(Box::new(PgError::error("failed to fetch invisible conflicting tuple".to_string()))); // alloc: Cold
                                }
                                let xmin = slot_xmin(existing)?;
                                exectuples::exec_clear_tuple(existing, host_mcx);
                                if xact_seams::transaction_id_is_current_transaction_id::call(xmin) {
                                    return Err(cardinality_violation());
                                }
                                return Err(Box::new(PgError::error("attempted to lock invisible tuple".to_string()))); // alloc: Cold
                            }
                            TM_Result::TM_SelfModified => return Err(Box::new(PgError::error("unexpected self-updated tuple".to_string()))), // alloc: Cold
                            r @ (TM_Result::TM_Updated | TM_Result::TM_Deleted) => {
                                if crate::xact_snapshot() {
                                    return Err(if r == TM_Result::TM_Updated { crate::serialization_failure() } else { concurrent_delete() });
                                }
                                exectuples::exec_clear_tuple(existing, host_mcx);
                                continue;
                            }
                            other => return Err(Box::new(PgError::error(format!("unrecognized table_tuple_lock status: {other:?}")))), // alloc: Cold
                        }
                        if crate::xact_snapshot() && !tableam::table_tuple_satisfies_snapshot(heap, existing, &self.snapshot)? && !xact_seams::transaction_id_is_current_transaction_id::call(slot_xmin(existing)?) {
                            exectuples::exec_clear_tuple(existing, host_mcx);
                            return Err(crate::serialization_failure());
                        }
                        exectuples::slot_getallattrs(existing);
                        break Outcome::Conflict;
                    }
                    _ => {
                        if crate::xact_snapshot() {
                            let snapshot = self.snapshot.clone();
                            check_tid_visible(mcx, heap, &conflict_tid, existing, &snapshot)?;
                        }
                        break Outcome::Skipped;
                    }
                }
            }
            let xid = xact_seams::get_current_transaction_id::call()?;
            let token = lmgr::SpeculativeInsertionLockAcquire(xid)?;
            let mut spec_conflict = false;
            tableam::table_tuple_insert_speculative(mcx, heap, slot, self.cid, 0, None, token)?;
            // H-a2: the non-arbiter deferrable indexes' recheckIndexes, for the AR triggers (ExecInsert's recheckIndexes)
            let state_mcx = crate::ops::modifytable::index_maint::state_mcx(oc, bind_mcx, mcx)?;
            recheck = execindexing::ExecInsertIndexTuples(state_mcx, mcx, oc, heap, slot, None, true, Some(&mut spec_conflict), &self.recipe.arbiters, false)?;
            tableam::table_tuple_complete_speculative(mcx, heap, slot, token, !spec_conflict)?;
            lmgr::SpeculativeInsertionLockRelease(xid)?;
            if !spec_conflict {
                break Outcome::Done;
            }
        };
        if r == Outcome::Done {
            if let Some(t) = self.triggers.as_mut() {
                t.after_insert(mcx, heap, self.root.as_ref(), slot.base().tts_tid, &recheck)?;
            }
            // V4R-501: RETURNING's system columns of the inserted row (the slot table_tuple_insert_speculative stamped)
            self.sys.from_slot(slot, heap.rd_id)?;
        }
        // the speculative insert materialized the row in `mcx` (the exec context): the image is freed there
        exectuples::exec_clear_tuple(slot, mcx);
        // H-b: ExecInsert's view check after the row is in (a DO NOTHING skip or a conflict writes nothing). D1112
        // (rxnested): it may park at a SubPlan read, as the plain INSERT's does; the resume enters `row_end` (write_op's
        // Park::View arm: everything before the check ran)
        if r == Outcome::Done && self.wco_park(mcx, heap, n, WCOKind::WCO_VIEW_CHECK, Park::View)? {
            return Ok(Outcome::SubPlan);
        }
        Ok(r)
    }

    /// H-b: WCO_RLS_CONFLICT_CHECK over the existing row DO UPDATE locked (`existing`, filled by `insert_on_conflict`).
    /// ExecOnConflictUpdate runs it after the DO UPDATE's WHERE passed and before the SET projection and ExecUpdate, so the
    /// IR's arm calls it there (`WRITE_CONFLICT_CHECK`, `Writer::conflict_check`): a filtered row is never checked, and the
    /// SET never runs (nor errs) ahead of PG's 42501.
    fn conflict_check_pg(&mut self, mcx: Mcx<'static>, heap: &Relation<'static>) -> PgResult<()> {
        let (Some(w), Some(ex)) = (self.wco.as_mut(), self.existing.as_mut()) else { return Ok(()) };
        exectuples::slot_getallattrs(ex);
        let n = heap.rd_att.natts as usize;
        let b = ex.base();
        w.check(self.scratch.mcx(), mcx, heap, &b.tts_values[..n], &b.tts_isnull[..n], WCOKind::WCO_RLS_CONFLICT_CHECK, self.modified, self.root.as_ref())
    }
}

impl WriteHost {
    /// ExecGetAllUpdatedCols: the statement's updated columns in THIS relation's numbering (D724: a routed leaf's or a
    /// pruned partition's renumbered from the root's through the root→leaf map by name, ri_RootToChildMap) with the
    /// stored generated columns an UPDATE recomputes; built in `mcx`.
    fn all_updated(&self, mcx: Mcx<'static>, heap: &Relation<'static>) -> PgResult<Option<types_nodes::Bitmapset<'static>>> {
        let Some((_, u)) = crate::ops::modifytable::write_route::leaf_modified(mcx, self.modified, heap, self.root.as_ref(), self.under_root)? else { return Ok(None) };
        let mut all = u.union(&types_nodes::Bitmapset::empty(), mcx)?;
        if heap.rd_att.constr.as_deref().is_some_and(|c| c.has_generated_stored) {
            let before = self.triggers.as_ref().is_some_and(|t| t.before.is_some());
            nodemodifytable::add_generated_extra_updated_cols(mcx, heap, before, &mut all)?;
        }
        Ok(Some(all))
    }

    /// ExecUpdateLockMode: exclusive when an updated column (`all_updated`, in this leaf's numbering) is one of the
    /// relation's key columns, else no-key exclusive; computed once per bind of this host (a routed leaf has its own).
    fn update_lockmode(&mut self, heap: &Relation<'static>) -> PgResult<LockTupleMode> {
        if let Some(m) = self.lockmode {
            return Ok(m);
        }
        const FLIHAN: i32 = types_tuple::htup::FirstLowInvalidHeapAttributeNumber;
        let key_updated = match self.all_updated(self.oc_scratch.exec_mcx(), heap)? {
            Some(updated) => relcache_seams::relation_get_index_attr_bitmap::call(heap.rd_id)?.key.iter().any(|&a| updated.is_member(a as i32 - FLIHAN)),
            None => false,
        };
        let m = if key_updated { LockTupleMode::LockTupleExclusive } else { LockTupleMode::LockTupleNoKeyExclusive };
        self.lockmode = Some(m);
        Ok(m)
    }
}

/// The slot's xmin (the port's `slot_xmin`).
fn slot_xmin(slot: &mut SlotData<'static>) -> PgResult<types_core::TransactionId> {
    let mut isnull = false;
    Ok(exectuples::slot_getsysattr(slot, types_tuple::htup::MinTransactionIdAttributeNumber, &mut isnull)?.as_usize() as types_core::TransactionId)
}

/// ExecOnConflictUpdate's cardinality violation: a row this command already inserted or updated.
/// H-f: a MERGE row op whose action the statement's subcommands do not hold (no view host bound for it).
#[cold]
fn no_view_host(what: &str) -> Box<PgError> {
    Box::new(PgError::error(format!("gravity: Reason::Bug: a view row op {what} without its INSTEAD OF host (the MERGE subcommands)"))) // alloc: Cold
}

fn cardinality_violation() -> Box<PgError> {
    Box::new(PgError::error("ON CONFLICT DO UPDATE command cannot affect row a second time".to_string()).with_sqlstate(ERRCODE_CARDINALITY_VIOLATION).with_hint("Ensure that no rows proposed for insertion within the same command have duplicate constrained values.")) // alloc: Cold
}

/// ExecCheckTIDVisible: under a transaction snapshot a DO NOTHING conflict against a row the snapshot
/// does not see is a serialization failure, unless the row is this transaction's own.
fn check_tid_visible(mcx: Mcx<'static>, heap: &Relation<'static>, tid: &ItemPointerData, slot: &mut SlotData<'static>, snapshot: &tableam_vocab::Snapshot<'static>) -> PgResult<()> {
    if !tableam::table_tuple_fetch_row_version(mcx, heap, tid, &None, slot)? {
        return Err(Box::new(PgError::error("failed to fetch conflicting tuple for ON CONFLICT".to_string()))); // alloc: Cold
    }
    let visible = tableam::table_tuple_satisfies_snapshot(heap, slot, snapshot)?;
    let r = if visible {
        Ok(())
    } else {
        let mut isnull = false;
        let xmin = exectuples::slot_getsysattr(slot, types_tuple::htup::MinTransactionIdAttributeNumber, &mut isnull)?.as_usize() as types_core::TransactionId;
        if xact_seams::transaction_id_is_current_transaction_id::call(xmin) { Ok(()) } else { Err(crate::serialization_failure()) }
    };
    exectuples::exec_clear_tuple(slot, mcx);
    r
}

impl WriteHost {
    /// One row op (or its resume after a check's SubPlan read, W6-D1112); the row's context is reset after it unless it
    /// parked (the cells, the check's arguments and a formed version live there until the resume).
    fn row_op(&mut self, op: WriteOp, pos: ScanPos, values: &[Word], isnull: &[u8]) -> Result<Outcome> {
        let r = match self.open_indexes(op) {
            Ok(true) => self.write_pg(op, pos, values, isnull).map_err(crate::stash),
            Ok(false) => Err(crate::unsupported()),
            Err(e) => Err(crate::stash(e)),
        };
        self.epq_ready = matches!(r, Ok(Outcome::Recheck));
        if self.park == Park::None {
            self.scratch.reset_exec();
            #[cfg(debug_assertions)]
            clobber_row_context(&mut self.scratch);
        }
        r
    }
}

impl WriteHost {
    /// INHERIT-ROOT: the check options in `heap`'s attnos: the list as given, or, when `checks_of` is another
    /// relation (a MERGE's first result relation under an INSERT into the inheritance root), its list mapped onto the
    /// heap's attnos by name as ExecInitMerge maps it for rootRelInfo (write_route.rs `leaf_checks`: map_variable_attnos,
    /// whole-row Vars to the heap's row type), in the per-bind context the check states live in.
    fn checks_onto(&self, heap: &Relation<'static>, c: &Rc<[crate::ops::modifytable::write_check::CheckSpec]>) -> PgResult<Rc<[crate::ops::modifytable::write_check::CheckSpec]>> {
        let first = self.checks_of;
        if first == 0 || first == heap.rd_id {
            return Ok(Rc::clone(c));
        }
        let mcx = self.oc_scratch.exec_mcx();
        // the first result relation is the plan's (locked by the executor as a result relation; taken again, as a no-op)
        lmgr_seams::lock_relation_oid::call(first, RowExclusiveLock)?;
        let f = table::table_open(mcx, first, NoLock)?;
        let mapped = crate::ops::modifytable::write_route::leaf_checks(mcx, &Some(Rc::clone(c)), &f, heap);
        f.close(NoLock)?;
        Ok(mapped?.unwrap_or_else(|| Rc::clone(c)))
    }
}

impl Writer for WriteHost {
    fn bind(&mut self, b: &BindingRef<'_>) -> Result<()> {
        self.words = crate::ops::modifytable::write_check::ParamWords::of(b.params);
        // ON CONFLICT's visibility checks; a move's destination hosts bind under it too (H-e (c))
        if self.recipe.conflict != Conflict::None || self.mover.is_some() {
            self.snapshot = Some(snapshot_of(b));
        }
        match self.bind_pg(b.command_id.0, Some(snapshot_of(b))) {
            Ok(true) => Ok(()),
            Ok(false) => Err(crate::unsupported()),
            Err(e) => Err(crate::stash(e)),
        }
    }

    fn write(&mut self, op: WriteOp, pos: ScanPos, values: &[Word], isnull: &[u8]) -> Result<Outcome> {
        self.sys.op(op == WriteOp::Insert);
        if self.park != Park::None {
            self.unpark();
        }
        self.row_op(op, pos, values, isnull)
    }

    fn sub_call(&mut self) -> Result<*mut u64> {
        // D1112 row B: a moved row's destination parked (its move host hands the call)
        if self.park == Park::Move {
            return match self.mover.as_ref() {
                Some(m) => m.borrow_mut().sub_call(),
                None => Err(crate::stash(Box::new(PgError::error("gravity: a SubPlan call with no move host")))), // alloc: Cold
            };
        }
        match self.wco.as_mut().and_then(|w| w.sub_call()) {
            Some(p) if self.park != Park::None => Ok(p),
            _ => Err(crate::stash(Box::new(PgError::error("gravity: a SubPlan call with no parked check")))), // alloc: Cold
        }
    }

    fn resume(&mut self, op: WriteOp, pos: ScanPos, values: &[Word], isnull: &[u8]) -> Result<Outcome> {
        if self.park == Park::None {
            return Err(crate::stash(Box::new(PgError::error("gravity: a SubPlan resume with no parked check")))); // alloc: Cold
        }
        self.row_op(op, pos, values, isnull)
    }

    fn recheck_pos(&self) -> ScanPos {
        self.recheck_pos
    }

    /// D655 Row C (V4R-534): the version the last Recheck locked (`recheck_pos`, table_tuple_lock's FIND_LAST_VERSION),
    /// fetched under SnapshotAny as ExecUpdate refetches its oldSlot, whole; a dropped column reads NULL. Once per Recheck.
    fn epq_row(&mut self, values: &mut [Word], isnull: &mut [u8]) -> Result<ScanPos> {
        let Some(heap) = self.heap.as_ref().filter(|_| core::mem::take(&mut self.epq_ready)) else {
            return Err(gravity_driver::Error::host("gravity: an EvalPlanQual row without a locked newest version"));
        };
        let mcx = self.scratch.mcx();
        if self.epq.is_none() {
            self.epq = Some(tableam::table_slot_create(mcx, heap).map_err(crate::stash)?);
        }
        let slot = self.epq.as_mut().expect("created");
        if !tableam::table_tuple_fetch_row_version(mcx, heap, &tid_of(self.recheck_pos), &None, slot).map_err(crate::stash)? {
            return Err(crate::stash(Box::new(PgError::error("failed to fetch tuple for EvalPlanQual recheck".to_string())))); // alloc: Cold
        }
        exectuples::slot_getallattrs(slot);
        let base = slot.base();
        let n = values.len().min(base.tts_nvalid as usize);
        // bounded("key-columns: one cell per column of the row, at most MaxTupleAttributeNumber (1664) columns")
        for k in 0..n {
            values[k] = base.tts_values[k].as_u64();
            isnull[k] = base.tts_isnull[k] as u8;
        }
        Ok(self.recheck_pos)
    }

    fn conflict_check(&mut self) -> Result<()> {
        let Some(heap) = self.heap.take() else { return Err(gravity_driver::Error::host("gravity: a conflict check on an unbound writer")) };
        let r = self.conflict_check_pg(self.scratch.exec_mcx(), &heap).map_err(crate::stash);
        self.heap = Some(heap);
        self.scratch.reset_exec();
        r
    }

    fn existing(&mut self, values: &mut [Word], isnull: &mut [u8]) -> Result<ScanPos> {
        let base = self.existing.as_ref().expect("bound").base();
        let n = values.len().min(base.tts_nvalid as usize);
        // bounded("key-columns: one cell per column of the row, at most MaxTupleAttributeNumber (1664) columns")
        for k in 0..n {
            values[k] = base.tts_values[k].as_u64();
            isnull[k] = base.tts_isnull[k] as u8;
        }
        Ok(pos_of(self.recipe.heap, &base.tts_tid))
    }

    fn old(&mut self, values: &mut [Word], isnull: &mut [u8]) -> Result<bool> {
        if !self.old_filled {
            // D667: a view UPDATE's OLD is its wholerow row (hosts write_view_rows.rs), under RETURNING OLD (WritePlan.old)
            let (Some(v), true) = (self.view_rows.as_mut(), self.old) else { return Ok(false) };
            return Ok(v.old_row(values, isnull));
        }
        let base = self.deleted.as_ref().expect("filled").base();
        let n = self.sys.old_row(values.len()).min(base.tts_nvalid as usize);
        // bounded("key-columns: one cell per column of the row, at most MaxTupleAttributeNumber (1664) columns")
        for k in 0..n {
            values[k] = base.tts_values[k].as_u64();
            isnull[k] = base.tts_isnull[k] as u8;
        }
        // V4R-501: the replaced version's system columns, the old site's last six cells (write_sys.rs)
        let rel = self.heap.as_ref().map_or(0, |h| h.rd_id);
        self.sys.fill_old(self.deleted.as_ref().expect("filled"), rel, values, isnull).map_err(crate::stash)?;
        Ok(true)
    }

    fn sys(&mut self, values: &mut [Word], isnull: &mut [u8]) -> Result<bool> {
        self.sys_fill(values, isnull, false).map_err(crate::stash)
    }

    fn sys_off(&self) -> usize {
        self.sys.off(self.old, self.recipe.conflict == Conflict::Update)
    }

    fn written(&mut self, values: &mut [Word], isnull: &mut [u8]) -> bool {
        if self.moved {
            // H-e (c): the moved row as its destination wrote it, in this target's layout (the insert half's slot)
            let (Some(m), Some(heap)) = (self.mover.as_ref(), self.heap.as_ref()) else { return false };
            return m.borrow_mut().written(heap, values, isnull);
        }
        if self.deleted_filled {
            // M4-RET (iii): a DELETE's written row is the version it deleted (RETURNING projects over it)
            let base = self.deleted.as_ref().expect("filled").base();
            let n = values.len().min(base.tts_nvalid as usize);
            // bounded("key-columns: one cell per column of the row, at most MaxTupleAttributeNumber (1664) columns")
            for k in 0..n {
                values[k] = base.tts_values[k].as_u64();
                isnull[k] = base.tts_isnull[k] as u8;
            }
            return true;
        }
        // H-c (D236): a row with stored generated columns is handed back as computed (over the BEFORE ROW trigger's row,
        // when one returned it), so RETURNING and the checks read the row as written; else the trigger's row
        let view = self.view_rows.as_ref().and_then(|v| v.written()).or_else(|| self.view_del.as_ref().and_then(|v| v.written()));
        // D667: an INSERT into a view returns the row its INSTEAD OF trigger returned
        let view = view.or_else(|| self.view.as_ref().and_then(|v| v.written()));
        let computed = view.or_else(|| self.generated.written());
        let Some((w, wn)) = computed.or_else(|| self.triggers.as_ref().and_then(|t| t.before.as_ref()).and_then(|b| b.returned())) else { return false };
        let n = values.len().min(w.len());
        values[..n].copy_from_slice(&w[..n]);
        isnull[..n].copy_from_slice(&wn[..n]);
        true
    }

    fn unbind(&mut self) {
        self.unbind_pg().expect("gravity: write host unbind");
    }
}

/// D608 (debug builds): PG's CLOBBER_FREED_MEMORY for the per-row context. After the reset the rewound window is
/// allocated over in 1 KiB chunks while they stay contiguous (the arena's first block), each filled with 0x7F, then reset
/// again: a state left in the row's context from an earlier row reads poison, every time, instead of passing on stale
/// bytes the next row happened not to overwrite.
#[cfg(debug_assertions)]
fn clobber_row_context(scratch: &mut NodeScratch) {
    use gravity_program::cost::runtime::CLOBBER_CHUNK as CHUNK;
    let m = scratch.exec_mcx();
    let mut prev: Option<*const u8> = None;
    for _ in 0..1024 {
        let Ok(mut v) = mcx::vec_with_capacity_in::<u8>(m, CHUNK) else { break };
        v.resize(CHUNK, 0x7F);
        let p = v.as_ptr();
        core::mem::forget(v);
        if prev.is_some_and(|q| q.wrapping_add(CHUNK) != p) {
            break;
        }
        prev = Some(p);
    }
    scratch.reset_exec();
}

impl Drop for WriteHost {
    fn drop(&mut self) {
        let _ = self.unbind_pg();
    }
}
