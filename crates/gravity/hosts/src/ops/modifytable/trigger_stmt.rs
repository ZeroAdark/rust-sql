use std::cell::RefCell;
use std::rc::Rc;

use gravity_program::Cmd;
use mcx::Mcx;
use trigger::{TransitionCaptureState, TrigTargetDesc, TriggerFmgrCache, TriggerWhenCache, TriggerWhenEval};
use types_core::Oid;
use types_error::{PgError, PgResult, ERRCODE_E_R_I_E_TRIGGER_PROTOCOL_VIOLATED};
use types_nodes::nodes_enums::CmdType;
use types_rel::Relation;
use types_storage::lock::NoLock;
use types_trigger::{TriggerDesc, TRIGGER_EVENT_BEFORE, TRIGGER_EVENT_DELETE, TRIGGER_EVENT_INSERT, TRIGGER_EVENT_UPDATE, TRIGGER_TYPE_BEFORE, TRIGGER_TYPE_DELETE, TRIGGER_TYPE_INSERT, TRIGGER_TYPE_LEVEL_MASK, TRIGGER_TYPE_STATEMENT, TRIGGER_TYPE_TIMING_MASK, TRIGGER_TYPE_UPDATE};
use types_trigger_call::TriggerData;

use crate::scratch::NodeScratch;
use crate::ops::modifytable::write_constraints::ModifiedCols;

/// The statement half of a served write's triggers (`fireBSTriggers` / `fireASTriggers`, one statement's
/// `ExecSetupTransitionCaptureState`): the BEFORE STATEMENT twins run at the host's bind, once per (relation,
/// command) and query level (`before_stmt_triggers_fired`); the AFTER STATEMENT events queue at the statement's
/// end after every row's — `after_query::end` fires the level's pending entries before `AfterTriggerEndQuery`,
/// so an Append's arms and a cached program's hosts all queue under the statement that bound them. The
/// transition capture is made at bind and handed to the AR twins per row (the DO UPDATE arm's is the UPDATE's).
pub struct StmtTriggers {
    relid: Oid,
    desc: Rc<TriggerDesc<'static>>,
    ops: (bool, bool, bool),
    modified: Option<ModifiedCols>,
    capture: Option<TransitionCaptureState>,
    oc_capture: Option<TransitionCaptureState>,
    /// MOVE: the statement is a MERGE (its moved rows are no UPDATE's in the transition tables: `merge`)
    merge: bool,
    fmgr: TriggerFmgrCache,
    when: TriggerWhenCache<'static>,
    scratch: NodeScratch,
}

struct Pending {
    depth: i32,
    stmt: Rc<RefCell<StmtTriggers>>,
    /// The writer whose bind made the entry (`tag_since`): 0 for the statement's own writers, else a writer whose AS events
    /// queue at its ModifyTable's end (`fire_as_tagged`).
    tag: u32,
}

leader_tls::leader_tls! {
    thread_local! {
        static PENDING: RefCell<Vec<Pending>> = const { RefCell::new(Vec::new()) }; // alloc: Program
        /// D832: exec `after_query` registered this thread's transaction-end callback (`arm_eoxact`).
        static EOXACT_ARMED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        /// D2605 EARLY (`early`): per query level, the relations whose AS events `fire_as` queues first.
        static EARLY: RefCell<Vec<(i32, Vec<Oid>)>> = const { RefCell::new(Vec::new()) }; // alloc: Exec
        /// D2830 (`target`): per query level, the served writes' root-less targets with the descriptor each bind took.
        static TARGETS: RefCell<Vec<(i32, TrigTargetDesc)>> = const { RefCell::new(Vec::new()) }; // alloc: Exec
        /// D3293 (`am_written`): per query level, the routed leaves written through their table AM (write.rs `insert_via_am`).
        static AM_WRITTEN: RefCell<Vec<(i32, Oid)>> = const { RefCell::new(Vec::new()) }; // alloc: Exec
        /// D3361: the running statement's crosscheck snapshot (write.rs `Crosscheck`), `None` outside one that carries it.
        pub(crate) static CROSSCHECK: RefCell<Option<Rc<types_snapshot::SnapshotData<'static>>>> = const { RefCell::new(None) };
    }
}

/// MERGE's subcommands (`mt_merge_subcommands`, H-f): the INSERT / UPDATE / DELETE its actions hold (DO NOTHING is
/// none), read from the statement's `mergeActionLists` per admission (exec `v4/m4.rs` `merge_ops`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MergeOps {
    pub insert: bool,
    pub update: bool,
    pub delete: bool,
}

impl MergeOps {
    pub const NONE: MergeOps = MergeOps { insert: false, update: false, delete: false };
}

/// `stmt_trigger_ops`: (insert, update, delete) statement events the command fires; ON CONFLICT DO UPDATE fires
/// both INSERT's and UPDATE's. A MERGE target's set is its actions' (`fireBSTriggers`' `mt_merge_subcommands`).
fn ops_of(cmd: Cmd, conflict_update: bool, merge: MergeOps) -> (bool, bool, bool) {
    match cmd {
        Cmd::Insert => (true, conflict_update, false),
        Cmd::Update => (false, true, false),
        Cmd::Delete => (false, false, true),
        Cmd::Merge => (merge.insert, merge.update, merge.delete),
        Cmd::Select => (false, false, false),
    }
}

fn cmd_type(cmd: Cmd) -> CmdType {
    match cmd {
        Cmd::Insert => CmdType::CMD_INSERT,
        Cmd::Update => CmdType::CMD_UPDATE,
        Cmd::Delete => CmdType::CMD_DELETE,
        Cmd::Merge => CmdType::CMD_MERGE,
        Cmd::Select => CmdType::CMD_SELECT,
    }
}

fn wants(desc: &TriggerDesc<'_>, ops: (bool, bool, bool)) -> bool {
    (ops.0 && (desc.trig_insert_before_statement || desc.trig_insert_after_statement)) || (ops.1 && (desc.trig_update_before_statement || desc.trig_update_after_statement)) || (ops.2 && (desc.trig_delete_before_statement || desc.trig_delete_after_statement))
}

impl StmtTriggers {
    /// The write host's bind: the captures made, the BS twins run in op order (INSERT, its conflict UPDATE, UPDATE,
    /// DELETE), the entry left for the statement's end; `None` when the command fires no statement-level
    /// trigger and no transition table is wanted.
    /// A MERGE's captures are CMD_MERGE's (MakeTransitionCaptureState: each action's tables), its BS / AS twins its
    /// subcommands' (`merge`), in fireBSTriggers' / fireASTriggers' orders.
    pub fn bind(heap: &Relation<'static>, desc: &Rc<TriggerDesc<'static>>, cmd: Cmd, conflict_update: bool, merge: MergeOps, modified: Option<ModifiedCols>) -> PgResult<Option<Rc<RefCell<StmtTriggers>>>> {
        let ops = ops_of(cmd, conflict_update, merge);
        if ops == (false, false, false) {
            return Ok(None);
        }
        let capture = trigger::MakeTransitionCaptureState(desc, heap.rd_id, cmd_type(cmd))?;
        let oc_capture = if conflict_update { trigger::MakeTransitionCaptureState(desc, heap.rd_id, CmdType::CMD_UPDATE)? } else { None };
        if !wants(desc, ops) && capture.is_none() && oc_capture.is_none() {
            return Ok(None);
        }
        let mut s = StmtTriggers { relid: heap.rd_id, desc: Rc::clone(desc), ops, modified, capture, oc_capture, merge: cmd == Cmd::Merge, fmgr: TriggerFmgrCache::default(), when: TriggerWhenCache::default(), scratch: NodeScratch::new("GravityTriggerStmt") };
        if ops.0 {
            s.before(heap, TRIGGER_TYPE_INSERT, TRIGGER_EVENT_INSERT)?;
        }
        if ops.1 {
            s.before(heap, TRIGGER_TYPE_UPDATE, TRIGGER_EVENT_UPDATE)?;
        }
        if ops.2 {
            s.before(heap, TRIGGER_TYPE_DELETE, TRIGGER_EVENT_DELETE)?;
        }
        let stmt = Rc::new(RefCell::new(s)); // alloc: Node
        PENDING.with(|p| p.borrow_mut().push(Pending { depth: trigger::query_depth(), stmt: Rc::clone(&stmt), tag: 0 })); // alloc: Node
        Ok(Some(stmt))
    }

    /// The capture the row's AR twin takes: the conflict arm's UPDATE has its own (`mt_oc_transition_capture`).
    pub fn capture(&self, conflict_update: bool) -> Option<&TransitionCaptureState> {
        if conflict_update { self.oc_capture.as_ref() } else { self.capture.as_ref() }
    }

    /// MOVE: a MERGE's row moved across partitions is captured as a DELETE's old row and an INSERT's new row,
    /// never as an UPDATE's: PostgreSQL 18.6's ExecDelete / ExecInsert take the UPDATE capture only when
    /// `mtstate->operation == CMD_UPDATE`.
    pub fn merge(&self) -> bool {
        self.merge
    }

    pub fn captures(&self) -> bool {
        self.capture.is_some() || self.oc_capture.is_some()
    }

    fn before(&mut self, rel: &Relation<'static>, tgtype_event: i16, event_op: u32) -> PgResult<()> {
        let has = match event_op {
            TRIGGER_EVENT_INSERT => self.desc.trig_insert_before_statement,
            TRIGGER_EVENT_UPDATE => self.desc.trig_update_before_statement,
            _ => self.desc.trig_delete_before_statement,
        };
        if !has || trigger::before_stmt_triggers_fired(rel.rd_id, event_op)? {
            return Ok(());
        }
        let tg_event = event_op | TRIGGER_EVENT_BEFORE;
        let desc = Rc::clone(&self.desc);
        let cols = if event_op == TRIGGER_EVENT_UPDATE { self.modified.map(|m| m.1) } else { None };
        let StmtTriggers { fmgr, when, scratch, .. } = self;
        let mcx: Mcx<'static> = scratch.exec_mcx();
        let mut w = TriggerWhenEval { mcx: scratch.mcx(), cache: when, modified_cols: cols };
        let r = (|| {
            // bounded("catalog-sized: one per trigger of the relation (its pg_trigger rows); a matching BEFORE STATEMENT trigger's function is called once, as ExecBSInsertTriggers loops them")
            for (i, t) in desc.triggers.iter().enumerate() {
                if t.tgtype & (TRIGGER_TYPE_LEVEL_MASK | TRIGGER_TYPE_TIMING_MASK | tgtype_event) != TRIGGER_TYPE_STATEMENT | TRIGGER_TYPE_BEFORE | tgtype_event || !trigger::TriggerEnabled(t) || !w.check(i, t, rel, tg_event, None, None)? {
                    continue;
                }
                let finfo = fmgr.get(i, t.tgfoid)?;
                let mut tdata = TriggerData::new(tg_event, rel, None, None, t);
                if trigger::ExecCallTriggerFunc(mcx, &mut tdata, finfo, None)?.is_some() {
                    return Err(Box::new(PgError::error("BEFORE STATEMENT trigger cannot return a value".to_string()).with_sqlstate(ERRCODE_E_R_I_E_TRIGGER_PROTOCOL_VIOLATED))); // alloc: Cold
                }
            }
            Ok(())
        })();
        scratch.reset_exec();
        r
    }

    /// `fireASTriggers`' order: DELETE, UPDATE (the conflict arm's capture when it has one), INSERT.
    fn after(&mut self) -> PgResult<()> {
        let rel = table::table_open(self.scratch.mcx(), self.relid, NoLock)?;
        let r = self.after_on(&rel);
        rel.close(NoLock)?;
        r
    }

    fn after_on(&mut self, rel: &Relation<'static>) -> PgResult<()> {
        let StmtTriggers { desc, ops, modified, capture, oc_capture, when, scratch, .. } = self;
        let mcx = scratch.mcx();
        if ops.2 {
            let mut w = TriggerWhenEval { mcx, cache: when, modified_cols: None };
            trigger::ExecASDeleteTriggers(rel, desc, capture.as_ref(), Some(&mut w))?;
        }
        if ops.1 {
            let cols = modified.map(|m| m.1);
            let mut w = TriggerWhenEval { mcx, cache: when, modified_cols: cols };
            trigger::ExecASUpdateTriggers(rel, desc, oc_capture.as_ref().or(capture.as_ref()), Some(&mut w), cols)?;
        }
        if ops.0 {
            let mut w = TriggerWhenEval { mcx, cache: when, modified_cols: None };
            trigger::ExecASInsertTriggers(rel, desc, capture.as_ref(), Some(&mut w))?;
        }
        Ok(())
    }
}

/// `AfterTriggerBeginQuery`'s companion: entries a statement that ended in error left at this level or deeper
/// are stale (the queue's own state was reset with the transaction's).
pub fn begin() {
    let d = trigger::query_depth();
    PENDING.with(|p| p.borrow_mut().retain(|e| e.depth < d));
    EARLY.with(|x| x.borrow_mut().retain(|e| e.0 < d));
    drop_targets_from(d);
    AM_WRITTEN.with(|w| w.borrow_mut().retain(|e| e.0 < d));
}

/// D3293: a routed leaf of another table AM (given it after the write's admission: write.rs `insert_via_am`) took a row
/// at this query level through `table_tuple_insert`. An AM may buffer such rows until `table_finish_bulk_insert`
/// (pgrcolumnar publishes them only there, and refuses a commit that still holds them), so the statement's end
/// finishes each one (`finish_am`).
pub fn am_written(relid: Oid) {
    let d = trigger::query_depth();
    AM_WRITTEN.with(|w| {
        let mut w = w.borrow_mut();
        if !w.contains(&(d, relid)) {
            w.push((d, relid)); // alloc: Exec
        }
    });
}

/// D3293 (`after_query::finish`, before the level's AFTER STATEMENT events): `table_finish_bulk_insert` over every leaf
/// this query level wrote through its table AM, then its placed rows' index entries (`drain_placed`), so its rows stand as
/// PostgreSQL's for the statement's triggers, the transaction's next statements and its commit. A statement that errored
/// left its entries (dropped by the level's next `begin` or the transaction's end): its rows go with the abort.
pub fn finish_am() -> PgResult<()> {
    let d = trigger::query_depth();
    let mine: Vec<Oid> = AM_WRITTEN.with(|w| {
        let mut w = w.borrow_mut();
        let mine = w.iter().filter(|e| e.0 == d).map(|e| e.1).collect(); // alloc: Exec
        w.retain(|e| e.0 != d);
        mine
    });
    if mine.is_empty() {
        return Ok(());
    }
    let scratch = NodeScratch::new("GravityAmFinish");
    // bounded("catalog-sized: one table_finish_bulk_insert per leaf partition this query level wrote through its table AM (am_written files a relation once), at most the target's partitions; the statement's end, outside every kernel call")
    for relid in mine {
        let rel = table::table_open(scratch.mcx(), relid, NoLock)?;
        let r = tableam::table_finish_bulk_insert(&rel, 0).and_then(|()| drain_placed(scratch.mcx(), &rel));
        rel.close(NoLock)?;
        r?;
    }
    Ok(())
}

pub fn drain_placed<'mcx>(mcx: Mcx<'mcx>, rel: &Relation<'mcx>) -> PgResult<()> {
    if !rel.rd_rel.relhasindex {
        return Ok(());
    }
    let mut state: Option<execindexing::ResultRelIndexState<'mcx>> = None;
    let mut eval: Option<mcx::MemoryContext> = None;
    let r = tableam::table_take_placed(mcx, rel, &mut |s| {
        let st = match state.as_mut() {
            Some(st) => st,
            None => state.insert(execindexing::ExecOpenIndices(mcx, rel, false)?),
        };
        let cx = eval.get_or_insert_with(|| mcx::MemoryContext::new_bump("GravityIndexDrain"));
        let out = execindexing::ExecInsertIndexTuples(mcx, cx.mcx(), st, rel, s, None, false, None, &[], false).map(|_| ());
        cx.reset();
        out
    });
    match state {
        Some(st) => r.and(execindexing::ExecCloseIndices(st)),
        None => r,
    }
}

/// D2830 (B39, C ExecGetTriggerResultRel; pgrust main 0d08dd059ab): a served write's root-less target, its own relation
/// (`StmtOf::Own`) or the partitioned root it routes or prunes under, with the descriptor its bind read (`None` = the
/// relation had none), filed under the query level. `after_query::finish` hands the level's targets to
/// `AfterTriggerEndQuery` as the ending query's result relations: their AFTER events fire with this executor-start
/// copy. Every other relation's events (a routed or pruned leaf, an RI-cascade target) fire with the descriptor the
/// firing resolves. The first bind of a relation at a level wins, as C's first `es_opened_result_relations` match does.
/// A target without `relhastriggers` is not filed: this statement queues no event on it.
pub fn target(relid: Oid, desc: Option<&Rc<TriggerDesc<'static>>>) {
    let d = trigger::query_depth();
    TARGETS.with(|t| {
        let mut t = t.borrow_mut();
        if !t.iter().any(|(at, (r, _))| *at == d && *r == relid) {
            t.push((d, (relid, desc.cloned()))); // alloc: Exec
        }
    });
}

/// D2830: the current level's targets (`target`), taken by `after_query::finish` before `AfterTriggerEndQuery`.
pub fn take_targets() -> Vec<TrigTargetDesc> {
    let d = trigger::query_depth();
    TARGETS.with(|t| {
        let mut t = t.borrow_mut();
        if !t.iter().any(|e| e.0 == d) {
            return Vec::new(); // alloc: Exec
        }
        let (mine, keep): (Vec<(i32, TrigTargetDesc)>, Vec<(i32, TrigTargetDesc)>) = t.drain(..).partition(|e| e.0 == d); // alloc: Exec
        *t = keep;
        mine.into_iter().map(|e| e.1).collect() // alloc: Exec
    })
}

/// D2830: level `d` and deeper forgotten: by `begin` (an errored statement's targets), and by `after_query::finish` once
/// level `d`'s events fired (an RI cascade's statement runs with EXEC_FLAG_SKIP_TRIGGERS, without a level of its own, so
/// the targets it bound while they fired were filed at `d`; they are not the ending query's).
pub fn drop_targets_from(d: i32) {
    TARGETS.with(|t| t.borrow_mut().retain(|e| e.0 < d));
}

/// D2605 EARLY (exec ops/modifytable/early_as.rs, regress triggers#499; once per execution, at its protocols' prepare,
/// inside the statement's query level): the level's entries of relations `rels` (a data-modifying CTE's writers whose end
/// is the statement's own write pipeline's) queue their AS events first at the statement's end (`fire_as`), after every
/// row's events and before the statement's own, as PostgreSQL's CTE ModifyTable queues them when its reader ends.
pub fn early(rels: &[Oid]) {
    let d = trigger::query_depth();
    EARLY.with(|x| {
        let mut x = x.borrow_mut();
        match x.iter_mut().find(|e| e.0 == d) {
            Some(e) => {
                let more: Vec<Oid> = rels.iter().copied().filter(|r| !e.1.contains(r)).collect(); // alloc: Exec
                e.1.extend(more); // alloc: Exec
            }
            None => x.push((d, rels.to_vec())), // alloc: Exec
        }
    });
}

/// D2605 (a writing CTE's ModifyTable ends where its reader reads past its last row): the entries made so far, a mark
/// `tag_since` takes. The driver takes it before it binds a writer whose AS events queue at that writer's end.
pub fn pending_mark() -> usize {
    PENDING.with(|p| p.borrow().len())
}

/// D2605: the current level's untagged entries made since `mark` (by the writer the driver just bound) take `tag`.
pub fn tag_since(mark: usize, tag: u32) {
    let d = trigger::query_depth();
    PENDING.with(|p| {
        // bounded("other: one per statement-trigger entry of this query level made since the mark, an entry per result relation a writer bound with statement triggers (the plan's own size); a store each")
        for e in p.borrow_mut().iter_mut().skip(mark).filter(|e| e.depth == d && e.tag == 0) {
            e.tag = tag;
        }
    });
}

/// D2605: PostgreSQL's fireASTriggers at one ModifyTable's end: the current level's entries tagged `tag` (0: the
/// statement's own writers') queue their AS events now, in bind order, and leave the level's pending set. A writer that
/// ended already, or never bound, queues nothing.
pub fn fire_as_tagged(tag: u32) -> PgResult<()> {
    let d = trigger::query_depth();
    let mine: Vec<Pending> = PENDING.with(|p| {
        let mut p = p.borrow_mut();
        if !p.iter().any(|e| e.depth == d && e.tag == tag) {
            return Vec::new(); // alloc: Exec
        }
        let (mine, keep): (Vec<Pending>, Vec<Pending>) = p.drain(..).partition(|e| e.depth == d && e.tag == tag); // alloc: Exec
        *p = keep;
        mine
    });
    // bounded("other: one per statement-trigger entry of the query level with this tag, an entry per result relation a writer bound (the plan's own size); each queues its AFTER STATEMENT events, which AfterTriggerEndQuery fires")
    for e in mine {
        e.stmt.borrow_mut().after()?;
    }
    Ok(())
}

/// D832: true exactly once per thread, the first time exec `after_query` asks: it then registers the callback that
/// calls `at_eoxact`.
pub fn arm_eoxact() -> bool {
    EOXACT_ARMED.with(|c| !c.replace(true))
}

/// D832: the transaction's end (exec `after_query`'s XactCallback, COMMIT / ABORT / PREPARE): every entry is dropped
/// while its contexts still live. A statement that ERRORed left its entry here; dropped only by the next served write's
/// `begin`, a session whose last served write failed kept it to thread exit, whose TLS destructor freed it into an
/// already torn-down Mcx (SIGSEGV at backend exit).
pub fn at_eoxact() {
    let stale = PENDING.with(|p| std::mem::take(&mut *p.borrow_mut()));
    drop(stale);
    EARLY.with(|x| x.borrow_mut().clear());
    let targets = TARGETS.with(|t| std::mem::take(&mut *t.borrow_mut()));
    drop(targets);
    AM_WRITTEN.with(|w| w.borrow_mut().clear());
}

/// The statement's end, before `AfterTriggerEndQuery`: the level's AS events queued after its rows' events, the
/// `early` relations' first (stable otherwise: bind order).
pub fn fire_as() -> PgResult<()> {
    let d = trigger::query_depth();
    let mut mine: Vec<Pending> = PENDING.with(|p| {
        let mut p = p.borrow_mut();
        let (mine, keep): (Vec<Pending>, Vec<Pending>) = p.drain(..).partition(|e| e.depth == d); // alloc: Exec
        *p = keep;
        mine
    });
    let early = EARLY.with(|x| {
        let mut x = x.borrow_mut();
        x.iter().position(|e| e.0 == d).map(|i| x.swap_remove(i).1)
    });
    if let Some(rels) = early {
        // bounded("other: a sort of the query level's statement-trigger entries, an entry per result relation a writer bound (the plan's own size)")
        mine.sort_by_key(|e| !rels.contains(&e.stmt.borrow().relid));
    }
    // bounded("other: one per statement-trigger entry of the query level, an entry per result relation a writer bound (the plan's own size); each queues its AFTER STATEMENT events, which AfterTriggerEndQuery fires")
    for e in mine {
        e.stmt.borrow_mut().after()?;
    }
    Ok(())
}
