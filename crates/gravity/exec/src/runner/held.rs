//! D761: the relations PostgreSQL's executor opens at ExecutorStart, held by a served run from its start to its end.
//! PostgreSQL keeps a relcache reference per relation InitPlan / ExecInitNode opened (`ExecGetRangeTableRelation` to
//! `ExecCloseRangeTableRelations`, an index scan's `index_open`), so a body that runs ALTER TABLE /
//! TRUNCATE / DROP / CREATE INDEX on one meets CheckTableNotInUse's 55006. Gravity's sources open their relations
//! transiently at their own pull: a UNION ALL arm's relation was open only while that arm ran, and a body in the arm
//! before it altered or truncated it (error-vs-value). `plan` walks the plan once per program template (admit.rs
//! `GraphTpl`); a run only opens and drops the handles (`take`). Range-table relations only, never `relationOids` (D759).
//! D978: `take` also runs ExecInitModifyTable's CheckValidResultRel publication check (CheckCmdReplicaIdentity) over the
//! result relations initial pruning keeps, the first when it keeps none: the hosts see a target at its first row only.
use types_core::Oid;
use types_error::PgResult;
use types_nodes::parsenodes::RTEKind;
use types_nodes::nodes_enums::CmdType;
use types_nodes::plannodes::{PlannedStmt, RowMarkType};
use types_nodes::primnodes::OnConflictAction;
use types_nodes::Node;
use types_portal::ParamListHandle;
use types_rel::Relation;
use types_storage::lock::{LOCKMODE, NoLock};

use mcx::Mcx;
use std::rc::Rc;
use types_snapshot::SnapshotData;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum What {
    /// A scanned relation, a result relation, a row mark's: under the executor's lock already (NoLock). A result
    /// relation's indexes are not: PostgreSQL opens them at the first row written (ExecInsert / ExecUpdatePrologue).
    Rel,
    /// An index scan's index, at its relation's `rellockmode` (ExecInitIndexScan's `index_open`).
    Index(LOCKMODE),
    /// A partitioned table a PartitionedRelPruneInfo names (CreatePartitionPruneState opens it, pruned or not).
    Pruning,
    /// A row mark's relation (not a parent's, not ROW_MARK_COPY): InitPlan's CheckValidRowMarkRel runs over it (D976).
    Mark(RowMarkType),
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    rti: u32,
    oid: Oid,
    what: What,
}

/// D978: a result relation of a ModifyTable and the commands CheckValidResultRel checks its replica identity for
/// (CheckCmdReplicaIdentity's UPDATE / DELETE: the operation, a MERGE's actions, ON CONFLICT DO UPDATE's UPDATE).
#[derive(Debug)]
struct ResultCheck {
    rti: u32,
    oid: Oid,
    lock: LOCKMODE,
    cmds: Box<[CmdType]>,
}

/// A plan's relations as ExecutorStart opens them; `initial`: a pruning info has initial steps, so a run asks
/// ExecDoInitialPruning which range-table entries ExecInitNode initializes (`es_unpruned_relids`). `results`: per
/// ModifyTable its result relations in `resultRelations` order, each with the commands its publication check covers.
#[derive(Debug, Default)]
pub struct HeldPlan {
    entries: Box<[Entry]>,
    initial: bool,
    results: Box<[Box<[ResultCheck]>]>,
    /// An EXPLAIN without ANALYZE's (`explain_plan`): `Some(generic)`.
    explain: Option<bool>,
}

impl HeldPlan {
    pub fn heap_bytes(&self) -> u64 {
        let results: usize = self.results.iter().map(|r| core::mem::size_of_val(&**r) + r.iter().map(|c| core::mem::size_of_val(&*c.cmds)).sum::<usize>()).sum();
        (core::mem::size_of_val(&*self.entries) + core::mem::size_of_val(&*self.results) + results) as u64
    }
}

/// One run's handles: dropped at the run's end (`Relation`'s drop is `relation_close(rel, NoLock)`; locks stay to the
/// transaction's end, as PostgreSQL's).
/// D2578: also the run's hold on its initial pruning decision (`prune::decide`), read by its `src.prune` / rerun sets.
#[derive(Default)]
pub struct Held(Vec<Relation<'static>>, Option<crate::ops::append::prune::Decided>);

struct Walk<'a, 'p> {
    pstmt: &'a PlannedStmt<'p>,
    out: Vec<Entry>,
    /// D3585: `rel`'s entries of `out` by range-table index, so a relation is checked against its own entries only (an
    /// Append over thousands of partitions walked in linear time)
    at: std::collections::HashMap<u32, Vec<usize>>,
    results: Vec<Box<[ResultCheck]>>,
}

impl<'p> Walk<'_, 'p> {
    fn rte(&self, rti: u32) -> Option<(Oid, LOCKMODE)> {
        let r = self.pstmt.rtable.iter().nth((rti as usize).checked_sub(1)?)?.as_range_tbl_entry()?;
        (r.rtekind == RTEKind::RTE_RELATION).then_some((r.relid, r.rellockmode))
    }

    fn rel(&mut self, rti: u32, what: What) {
        if let Some((oid, _)) = self.rte(rti) {
            let Walk { out, at, .. } = self;
            let mine = at.entry(rti).or_default();
            // a row mark's relation already held as a scan's: one handle, checked as the mark's (D976)
            if let (What::Mark(_), Some(&i)) = (what, mine.iter().find(|&&i| out[i].oid == oid && out[i].what == What::Rel)) {
                out[i].what = what;
            } else if !mine.iter().any(|&i| out[i].oid == oid && out[i].what == what) {
                mine.push(out.len()); // alloc: Program
                out.push(Entry { rti, oid, what }); // alloc: Program
            }
        }
    }

    fn index(&mut self, rti: u32, indexid: Oid) {
        if let Some((_, lock)) = self.rte(rti) {
            self.out.push(Entry { rti, oid: indexid, what: What::Index(lock) }); // alloc: Program
        }
    }

    /// ExecInitNode's opens over `n` and its children.
    fn node(&mut self, n: Node<'p>) {
        stack_depth_core::check_stack_depth_or_panic();
        let scan = |rti: u32, w: &mut Self| {
            if rti > 0 {
                w.rel(rti, What::Rel);
            }
        };
        if let Some(s) = n.as_seq_scan() {
            scan(s.scan.scanrelid, self);
        } else if let Some(s) = n.as_sample_scan() {
            scan(s.scan.scanrelid, self);
        } else if let Some(s) = n.as_index_scan() {
            scan(s.scan.scanrelid, self);
            self.index(s.scan.scanrelid, s.indexid);
        } else if let Some(s) = n.as_index_only_scan() {
            scan(s.scan.scanrelid, self);
            self.index(s.scan.scanrelid, s.indexid);
        } else if let Some(s) = n.as_bitmap_index_scan() {
            self.index(s.scan.scanrelid, s.indexid);
        } else if let Some(s) = n.as_bitmap_heap_scan() {
            scan(s.scan.scanrelid, self);
        } else if let Some(s) = n.as_tid_scan() {
            scan(s.scan.scanrelid, self);
        } else if let Some(s) = n.as_tid_range_scan() {
            scan(s.scan.scanrelid, self);
        } else if let Some(s) = n.as_foreign_scan() {
            scan(s.scan.scanrelid, self);
        } else if let Some(m) = n.as_modify_table() {
            let mut group = Vec::new(); // alloc: Program
            for (i, rti) in m.resultRelations.iter().enumerate() {
                self.rel(rti as u32, What::Rel);
                // CheckValidResultRel: a MERGE's actions' commands, else the operation; ON CONFLICT DO UPDATE's UPDATE too
                let mut cmds: Vec<CmdType> = match m.operation {
                    CmdType::CMD_MERGE => m.mergeActionLists.iter().nth(i).and_then(|l| l.as_list()).into_iter().flat_map(|l| l.iter()).filter_map(|a| a.as_merge_action()).map(|a| a.commandType).collect(), // alloc: Program
                    op => vec![op], // alloc: Program
                };
                if m.onConflictAction == OnConflictAction::ONCONFLICT_UPDATE as u32 {
                    cmds.push(CmdType::CMD_UPDATE); // alloc: Program
                }
                cmds.retain(|c| matches!(c, CmdType::CMD_UPDATE | CmdType::CMD_DELETE));
                if let Some((oid, lock)) = self.rte(rti as u32) {
                    group.push(ResultCheck { rti: rti as u32, oid, lock, cmds: cmds.into_boxed_slice() }); // alloc: Program
                }
            }
            // every result relation: an EXPLAIN's start checks each one's relkind too (`explain_plan`)
            if !group.is_empty() {
                self.results.push(group.into_boxed_slice()); // alloc: Program
            }
            if m.rootRelation > 0 {
                self.rel(m.rootRelation, What::Rel);
            }
        }
        let children = n.as_append().map(|a| &a.appendplans).or(n.as_merge_append().map(|a| &a.mergeplans)).or(n.as_bitmap_and().map(|a| &a.bitmapplans)).or(n.as_bitmap_or().map(|a| &a.bitmapplans));
        for c in children.into_iter().flat_map(|l| l.iter()) {
            self.node(c);
        }
        if let Some(sub) = n.as_subquery_scan().and_then(|s| s.subplan) {
            self.node(sub);
        }
        if let Some(p) = n.as_plan() {
            for c in [p.lefttree, p.righttree].into_iter().flatten() {
                self.node(c);
            }
        }
    }
}

/// The relations `pstmt`'s ExecutorStart opens: every scan, index scan and result relation of the plan tree and of each
/// subplan (InitPlan initializes them all), the row marks' relations (not a parent's, not ROW_MARK_COPY) and the pruning
/// infos' partitioned tables. A view has no scan node: never held.
pub fn plan(pstmt: &PlannedStmt<'_>) -> HeldPlan {
    let mut w = Walk { pstmt, out: Vec::new(), at: std::collections::HashMap::new(), results: Vec::new() }; // alloc: Program
    if let Some(t) = pstmt.planTree {
        w.node(t);
    }
    for s in pstmt.subplans.iter().flatten() {
        w.node(s);
    }
    for m in pstmt.rowMarks.iter().filter_map(|n| n.as_plan_row_mark()) {
        if !m.isParent && m.markType != RowMarkType::ROW_MARK_COPY {
            w.rel(m.rti, What::Mark(m.markType));
        }
    }
    let mut initial = false;
    for info in pstmt.partPruneInfos.iter().filter_map(|n| n.as_partition_prune_info()) {
        for l in info.prune_infos.iter() {
            for r in l.as_list().into_iter().flat_map(|l| l.iter()).filter_map(|r| r.as_partitioned_rel_prune_info()) {
                initial |= !r.initial_pruning_steps.is_nil();
                w.rel(r.rtindex, What::Pruning);
            }
        }
    }
    HeldPlan { entries: w.out.into_boxed_slice(), initial, results: w.results.into_boxed_slice(), explain: None }
}

/// `plan` for an EXPLAIN without ANALYZE (`generic`: EXPLAIN (GENERIC_PLAN)), whose `take` opens what ExecutorStart opens
/// under EXEC_FLAG_EXPLAIN_ONLY: no index (ExecInitIndexScan, ExecInitIndexOnlyScan and ExecInitBitmapIndexScan return
/// before their `index_open`), no initial pruning under GENERIC_PLAN (its parameters are unbound: CreatePartitionPruneState
/// skips it, so every relation is initialized), and each result relation's relkind checked as CheckValidResultRel does
/// (the served write's host checks it at its bind, which an EXPLAIN never reaches).
pub fn explain_plan(pstmt: &PlannedStmt<'_>, generic: bool) -> HeldPlan {
    HeldPlan { explain: Some(generic), ..plan(pstmt) }
}

/// One run's handles over `plan`'s relations, taken before anything of the run executes (an InitPlan's body, a key
/// expression): an entry ExecDoInitialPruning pruned under `params` is skipped, as ExecInitNode never initializes it.
/// D2578: that pruning is the run's decision (`prune::decide`, held with the handles), made under `snap` pushed (the
/// statement's snapshot), or else the caller's active snapshot (the served ExecutorStart's, PortalStart pushed it), or a
/// transaction snapshot pushed for it when none is active: a step's function that runs SQL has its outer snapshot.
pub fn take(mcx: Mcx<'static>, pstmt: &'static PlannedStmt<'static>, plan: &HeldPlan, params: ParamListHandle, snap: Option<&Rc<SnapshotData<'static>>>) -> PgResult<Held> {
    let explain = plan.explain;
    if plan.entries.is_empty() {
        return Ok(Held::default());
    }
    let (decided, unpruned) = if plan.initial && explain != Some(true) {
        let pushed = match snap {
            Some(s) => snapmgr::PushActiveSnapshot(s).map(|()| true)?,
            None if !snapmgr::ActiveSnapshotSet() => snapmgr::PushActiveSnapshot(&snapmgr::GetTransactionSnapshot()?).map(|()| true)?,
            None => false,
        };
        let d = crate::ops::append::prune::decide(pstmt, params);
        if pushed {
            snapmgr::PopActiveSnapshot()?;
        }
        let (d, u) = d?;
        (Some(d), Some(u))
    } else {
        (None, None)
    };
    let mut out = Vec::with_capacity(plan.entries.len()); // alloc: Exec
    for e in plan.entries.iter() {
        if e.what != What::Pruning && unpruned.as_ref().is_some_and(|u| !u.contains(&e.rti)) {
            continue;
        }
        if let What::Index(lock) = e.what {
            if explain.is_none() {
                out.push(indexam::index_open(mcx, e.oid, lock)?); // alloc: Exec
            }
            continue;
        }
        let rel = table::table_open(mcx, e.oid, NoLock)?;
        if let What::Mark(mark) = e.what {
            valid_row_mark_rel(&rel, mark)?;
        }
        out.push(rel); // alloc: Exec
    }
    // D978: ExecInitModifyTable's CheckValidResultRel publication check over the result relations initial pruning keeps
    // (the first one when it keeps none), before any expression, trigger or row of the statement
    for group in plan.results.iter() {
        let kept = |r: &&ResultCheck| unpruned.as_ref().map_or(true, |u| u.contains(&r.rti));
        let first = if group.iter().any(|r| kept(&r)) { None } else { group.first() };
        for r in group.iter().filter(kept).chain(first).filter(|r| explain.is_some() || !r.cmds.is_empty()) {
            let rel = table::table_open(mcx, r.oid, r.lock)?;
            if explain.is_some() {
                if let Some(e) = gravity_hosts::ops::modifytable::write::result_kind_error(&rel) {
                    return Err(e);
                }
            }
            for &cmd in r.cmds.iter() {
                gravity_hosts::ops::modifytable::write_route::replica_identity(mcx, &rel, cmd)?;
            }
        }
    }
    Ok(Held(out, decided))
}

/// D976: InitPlan's CheckValidRowMarkRel (execMain.c) over a row mark's relation, at the served ExecutorStart as
/// PostgreSQL raises it: a sequence, a TOAST relation, a view, a materialized view under a locking clause (a
/// ROW_MARK_REFERENCE is allowed) and any other non-table relkind cannot be locked. A foreign table's mark never reaches
/// a served program (ForeignScan is an excluded node family, ledger.rs), so its RefetchForeignRow arm has no served case.
fn valid_row_mark_rel(rel: &Relation<'_>, mark: RowMarkType) -> PgResult<()> {
    use types_rel::{RELKIND_FOREIGN_TABLE, RELKIND_MATVIEW, RELKIND_PARTITIONED_TABLE, RELKIND_RELATION, RELKIND_SEQUENCE, RELKIND_TOASTVALUE, RELKIND_VIEW};
    let what = match rel.rd_rel.relkind {
        RELKIND_RELATION | RELKIND_PARTITIONED_TABLE | RELKIND_FOREIGN_TABLE => return Ok(()),
        RELKIND_MATVIEW if mark == RowMarkType::ROW_MARK_REFERENCE => return Ok(()),
        RELKIND_SEQUENCE => "sequence",
        RELKIND_TOASTVALUE => "TOAST relation",
        RELKIND_VIEW => "view",
        RELKIND_MATVIEW => "materialized view",
        _ => "relation",
    };
    let name = String::from_utf8_lossy(rel.rd_rel.relname.name_str()).to_string(); // alloc: Cold
    Err(Box::new(types_error::PgError::error(format!("cannot lock rows in {what} \"{name}\"")).with_sqlstate(types_error::ERRCODE_WRONG_OBJECT_TYPE))) // alloc: Cold
}
