//! S5 (pgrust.point_noplan, D4550): a point statement's plan reused from the session's memo of the plans the planner
//! built for its shape. Hook A (`plan_skip`, after nofrom's) takes one Query of a point shape (`candidate`: a SELECT,
//! SELECT FOR <lock>, UPDATE or DELETE of one table whose WHERE is `column = expression` clauses ANDed), folds its target
//! list and WHERE as subquery_planner's preprocess_expression folds them (the same function, in the same order, at the
//! same point: a folding error or side effect is the planner's own), takes the locks get_relation_info takes (every index
//! of the table in OID order at the RTE's lock mode, invalid ones too), and then:
//! - hit: the memo holds the shape's plan for this role, the catalog reads the planner made for it stand (gravity_deps
//!   `Held`, asked after the locks as plancache asks after AcquirePlannerLocks: a new lock reads the invalidations), no
//!   setting moved since (`store_mutation_count`), and the folded statement equals the filed one but for its constants:
//!   the plan is the filed plan with this statement's own folded target entries and WHERE clauses in its expressions'
//!   places (`build`), as setrefs places them (its scan-level fix is the operator function ids);
//! - miss: the planner plans the folded statement (its own folding then evaluates nothing again) under a recording of its
//!   catalog reads, and the plan is filed when it is a point plan (`plan_gate`, `fill_entry`): an IndexScan over a unique
//!   btree with equality on every key column against a constant and no other clause, under LockRows or ModifyTable at most.
//!   The filing rule (D4550): at once when no other index of the table has a key column the
//!   WHERE tests (`alone`: the planner has nothing else to choose), else once the planner chose the same plan (index and
//!   node) for the shape at `CONFIRM_MISSES` misses in a row (the slot counts them, `Slot::seen`); another choice starts
//!   the count again.
//!
//! A statement's start also lets go the slots whose reads can never stand again (`sweep`: a dropped or rewritten table, a
//! changed catalog row), at most once a second and only when a committed generation moved; and the whole memo when the
//! process's memory-short epoch moved (REACH, `mcx::memory_short_epoch`). The idle release drops it whole too.
//!
//! The plan then goes where a planner plan goes (hook B, Gravity, a portal). A reused plan keeps the filed statement's
//! estimates and index: for other constants the planner may estimate otherwise, or pick another index or scan node.
//! `strict` and PGRUST_GRAVITY_POINT_PLANCHECK plan each reused statement again and classify the difference (`compare`);
//! a difference past estimates and that choice is an error under `strict`.
//!
//! A hit, and a miss whose plan was filed, also names its slot to hook B (`Hit`): a point statement's hook B keeps what
//! it builds per statement in the slot (keep.rs `Kept`), under the slot's reads (its build's own added), settings, role
//! and the memo's cap. A point UPDATE is served only from a slot (update.rs).
use std::cell::RefCell;
use std::hash::Hasher;

use mcx::{Mcx, McxOwned, PgVec};
use nodes_core::NodeWalker;
use types_core::Oid;
use types_error::{PgError, PgResult};
use types_nodes::equal::equal;
use types_nodes::list::{IntList, NodeList, OidList};
use types_nodes::nodes_enums::CmdType;
use types_nodes::parsenodes::{Query, QuerySource, RTEKind, CURSOR_OPT_PARALLEL_OK};
use types_nodes::plannodes::{IndexScan, LockRows, ModifyTable, Plan, PlannedStmt, Scan};
use types_nodes::primnodes::{BoolExprType, Const, OpExpr, TargetEntry, OUTER_VAR};
use types_nodes::{Bitmapset, Node, NodeTag};
use types_portal::ParamListHandle;
use types_rel::{NoLock, Relation, LOCKMODE, RELKIND_MATVIEW, RELKIND_RELATION};

use super::keep::{self, KeptBox};
use super::{env_once, probe, rel, report, reporting, PLANCHECK};
use crate::inputs::knobs::seam::{noplan_setting, planning_observed, point_noplan, settings_at, POINT_NOPLAN_OFF, POINT_NOPLAN_STRICT};

/// The memo's holder: a per-backend cache, charged whole to it by construction (its root context and every entry's child).
pub(super) const MEMO_OWNER: memledger::Owner = memledger::Owner::Cache(memledger::Holder::Of(memledger::Consumer::PerBackend));

leader_tls::leader_tls! {
    thread_local! {
        /// The session's memo of point plans: made at its first filing, dropped when memory runs short (`sweep`), at the
        /// idle release and at the session's teardown.
        static MEMO: RefCell<Option<McxOwned<MemoTy>>> = const { RefCell::new(None) };
        /// The slot hook A served this statement's plan from (`hit`), for hook B: set at a hit, cleared at every hook A,
        /// taken by hook B.
        static HIT: core::cell::Cell<Option<Hit>> = const { core::cell::Cell::new(None) };
    }
}

mcx::bind!(MemoTy => Memo<'m>);
mcx::bind!(EntryTy => Entry<'e>);

/// 'GravityPointPlanMemo': the slots, least recently used first out past mempolicy's caps.
struct Memo<'m> {
    slots: PgVec<'m, Slot>,
    tick: u64,
    /// `l2cache::committed_moves` at the last `sweep`, and the earliest next one (`pg_clock::mono_ns`).
    swept: u64,
    sweep_at: u64,
    /// REACH: `mcx::memory_short_epoch` when the memo was made; the memo goes whole at the first `sweep` that reads another.
    short: u64,
    /// The slots' bytes, plans and kept pieces (`Slot::all_bytes` summed), kept current so a statement's put reads one
    /// number instead of walking every slot.
    bytes: usize,
    /// D4572: moves whenever bytes leave the memo (a slot or a slot's pieces dropped); a slot whose pieces found no room
    /// (`Slot::refused`) looks for room again only after it moved.
    freed: u64,
}

/// Misses of one shape whose planner chose the same plan, in a row, before it is filed when another index of the table
/// has a key column the WHERE tests (`file`, `alone`).
const CONFIRM_MISSES: u8 = 5;

/// At most one `sweep` a second.
const SWEEP_EVERY_NS: u64 = 1_000_000_000;

/// The planner's choice for a shape (`choice_of`): the scan's index and the plan's top node.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Choice {
    index: Oid,
    top: NodeTag,
}

/// One shape for one role: its plan (`entry`), or none (`entry` None) while what the planner read for it stands: with
/// `seen` the plan is being confirmed (the planner's choice and how many misses in a row made it: the shape's statements
/// are planned and counted), without it the planner's plan of the shape is no point plan (later statements go to the
/// planner untouched).
struct Slot {
    k1: u64,
    user: Oid,
    /// `store_mutation_count` when filed: any SET, RESET, SET LOCAL, function SET clause, rollback of one or reload since
    /// makes the slot stale.
    guc_at: u64,
    tick: u64,
    /// The entry context's bytes and the recorded reads' (a std vector in gravity_deps, counted here by its length).
    bytes: usize,
    held: gravity_deps::Held,
    entry: Option<McxOwned<EntryTy>>,
    seen: Option<(Choice, u8)>,
    /// What hook B keeps for the slot's statements (keep.rs), and its bytes (`Kept::bytes`, at its last return).
    kept: Option<KeptBox>,
    kept_bytes: usize,
    /// D4572: the bytes of the pieces the slot's statement made that found no room in the memo (`put_kept`; 0: none) and
    /// `Memo::freed` when they did not: while no room comes back, the slot's statements run as with pgrust.point_keep off
    /// (nothing made, nothing evicted for them).
    refused: usize,
    refused_at: u64,
}

impl Slot {
    /// The slot's bytes in the memo: its plan's and its kept pieces' (the bytes the REPORT lines of a sweep, a shed and
    /// the idle drop name).
    fn all_bytes(&self) -> usize {
        self.bytes + self.kept_bytes
    }
}

/// 'GravityPointPlan' (a child of the memo's root): the folded statement the plan was made from, its constants in
/// `holes_of_query` order, and the filed plan once it passed `plan_gate` and `fill_entry`.
struct Entry<'e> {
    qf: Query<'e>,
    holes: PgVec<'e, Node<'e>>,
    filed: Option<Filed<'e>>,
}

struct Filed<'e> {
    /// The PlannedStmt with no plan tree: its range table, permissions, relation OIDs, row marks, result relations and
    /// exec parameters are the shape's.
    shell: PlannedStmt<'e>,
    root: Node<'e>,
    /// indexqualorig[i]'s clause in the folded WHERE's implicit-AND list.
    orig_of: PgVec<'e, u16>,
}

/// A hit's slot (or the slot a miss just filed), for hook B: the slot (its key and the tick the hit or filing gave it: a
/// slot filed again since has another) and the plan hook A built (its root's address: hook B serves that plan, no other).
#[derive(Clone, Copy)]
pub(super) struct Hit {
    k1: u64,
    user: Oid,
    tick: u64,
    plan: usize,
}

#[derive(Clone, Copy)]
struct Key {
    k1: u64,
    user: Oid,
    guc_at: u64,
}

/// Hook A after S0's: `Err` hands the queries back to the planner untouched; once a statement is folded here it is planned
/// here too (the planner's own call, as pg_plan_query makes it; its log and debug taps decline S5 first).
pub(super) fn plan_skip<'mcx>(mcx: Mcx<'mcx>, querytrees: PgVec<'mcx, Query<'mcx>>) -> PgResult<Result<PgVec<'mcx, PlannedStmt<'mcx>>, PgVec<'mcx, Query<'mcx>>>> {
    let mode = point_noplan();
    if mode == POINT_NOPLAN_OFF || querytrees.len() != 1 {
        return Ok(Err(querytrees));
    }
    sweep();
    let Some(k1) = candidate(&querytrees[0]) else {
        return Ok(Err(querytrees));
    };
    if let Some(why) = tap_refusal() {
        if reporting() {
            report(&format!("noplan declined ({why})")); // alloc: Cold
        }
        return Ok(Err(querytrees));
    }
    let key = Key { k1, user: miscinit_seams::get_user_id::call(), guc_at: settings_at() };
    if negative_stands(&key) {
        if reporting() {
            report("noplan declined (no point plan)");
        }
        return Ok(Err(querytrees));
    }
    // the table's own tests come before anything is evaluated: past the fold the statement is planned here, and the
    // planner expands a virtual generated column before it folds (expand_virtual_generated_columns), so the order of the
    // fold's evaluations would differ from its
    let relid = querytrees[0].rtable.nth(0).as_range_tbl_entry().expect("checked by candidate").relid;
    if let Some(why) = table_check(mcx, relid)? {
        if reporting() {
            report(&format!("noplan declined ({why})")); // alloc: Cold
        }
        return Ok(Err(querytrees));
    }
    // standard_planner judges a SELECT's parallel safety on the statement as written (max_parallel_hazard, before
    // preprocess_expression): a fold that drops a function not parallel safe would open parallel plans the planner does
    // not consider for it
    if parallel_hazard(&querytrees[0])? {
        if reporting() {
            report("noplan declined (parallel hazard)");
        }
        return Ok(Err(querytrees));
    }
    let qs: &'mcx mut [Query<'mcx>] = querytrees.leak();
    let q: &'mcx mut Query<'mcx> = &mut qs[0];
    fold(mcx, q)?;
    let stmt = point_road(mcx, q, &key, mode)?;
    let mut out: PgVec<'mcx, PlannedStmt<'mcx>> = PgVec::new_in(mcx);
    out.try_reserve_exact(1).map_err(|_| mcx.oom(core::mem::size_of::<PlannedStmt<'_>>()))?;
    out.push(stmt);
    Ok(Ok(out))
}

/// A SELECT without row marks (the planner's parallel assessment: a row mark is unsafe as written and folded alike) whose
/// expressions are not all parallel safe.
fn parallel_hazard(q: &Query<'_>) -> PgResult<bool> {
    if q.commandType != CmdType::CMD_SELECT || !q.rowMarks.is_nil() {
        return Ok(false);
    }
    Ok(clauses::max_parallel_hazard(q)? != clauses::classify::PROPARALLEL_SAFE)
}

/// What keeps S5 off whatever the statement: an executor hook (a module that may print or count the plan it runs),
/// a planner tap or planner statistics / plan printing, or a planner setting the memo does not follow.
fn tap_refusal() -> Option<&'static str> {
    if super::super::execute::executor_hooks_active() {
        return Some("executor hooks");
    }
    if planning_observed() {
        return Some("planning observed");
    }
    noplan_setting()
}

/// The point shape's test on the analyzed Query, before anything is evaluated, and its shape key K1 (constants as typed
/// holes; operators, functions, columns and the table in): `None` for anything else.
fn candidate(q: &Query<'_>) -> Option<u64> {
    let cmd = q.commandType;
    let write = matches!(cmd, CmdType::CMD_UPDATE | CmdType::CMD_DELETE);
    if !(write || cmd == CmdType::CMD_SELECT) || q.querySource != QuerySource::QSRC_ORIGINAL || !q.canSetTag || q.utilityStmt.is_some() {
        return None;
    }
    let flags = q.hasAggs || q.hasWindowFuncs || q.hasTargetSRFs || q.hasSubLinks || q.hasDistinctOn || q.hasRecursive || q.hasModifyingCTE || q.hasRowSecurity || q.hasGroupRTE || q.isReturn;
    let clauses = !q.cteList.is_nil()
        || !q.mergeActionList.is_nil()
        || q.mergeJoinCondition.is_some()
        || q.onConflict.is_some()
        || !q.returningList.is_nil()
        || !q.groupClause.is_nil()
        || !q.groupingSets.is_nil()
        || q.havingQual.is_some()
        || !q.windowClause.is_nil()
        || !q.distinctClause.is_nil()
        || !q.sortClause.is_nil()
        || q.limitOffset.is_some()
        || q.limitCount.is_some()
        || q.setOperations.is_some()
        || !q.constraintDeps.is_nil()
        || !q.withCheckOptions.is_nil();
    if flags || clauses || q.rtable.len() != 1 || q.rteperminfos.len() != 1 {
        return None;
    }
    let rte = q.rtable.nth(0).as_range_tbl_entry()?;
    let kind = rte.relkind == RELKIND_RELATION || (rte.relkind == RELKIND_MATVIEW && !write);
    if rte.rtekind != RTEKind::RTE_RELATION || !kind || rte.tablesample.is_some() || !rte.securityQuals.is_nil() {
        return None;
    }
    if q.resultRelation != i32::from(write) {
        return None;
    }
    let marks = q.rowMarks.len();
    if (write && marks != 0) || marks > 1 || q.hasForUpdate != (marks == 1) {
        return None;
    }
    let mark = match q.rowMarks.first() {
        Some(m) => Some(m.as_row_mark_clause().filter(|m| m.rti == 1 && !m.pushedDown)?),
        None => None,
    };
    let f = q.jointree?;
    if f.fromlist.len() != 1 || f.fromlist.nth(0).as_range_tbl_ref().is_none_or(|r| r.rtindex != 1) {
        return None;
    }
    let mut h = std::hash::DefaultHasher::new();
    h.write_u32(cmd as u32);
    h.write_u32(rte.relid);
    h.write_i32(rte.rellockmode);
    if let Some(m) = mark {
        h.write_u32(m.strength as u32);
        h.write_u32(m.waitPolicy as u32);
    }
    let mut w = Shape { h, ok: true };
    for t in q.targetList.iter() {
        let te = t.as_target_entry().filter(|te| !te.resjunk)?;
        w.h.write_i16(te.resno);
        if w.visit(te.expr).ok()? || !w.ok {
            return None;
        }
    }
    // the WHERE: `column = expression` clauses (the column first), ANDed, each column at most once (a = 1 AND a = 2 is
    // the planner's equivalence classes' to judge, by value)
    let quals = f.quals?;
    let and = quals.as_bool_expr();
    let n = match and {
        Some(b) if b.boolop == BoolExprType::AND_EXPR => b.args.len(),
        Some(_) => return None,
        None => 1,
    };
    let mut seen = [0u64; 32];
    for i in 0..n {
        let c = and.map_or(quals, |b| b.args.nth(i));
        let op = c.as_op_expr().filter(|op| op.args.len() == 2)?;
        let v = column(op.args.nth(0))?;
        let (word, bit) = (v.varattno as usize / 64, 1u64 << (v.varattno as usize % 64));
        if word >= seen.len() || seen[word] & bit != 0 {
            return None;
        }
        seen[word] |= bit;
        if w.visit(c).ok()? || !w.ok {
            return None;
        }
    }
    Some(w.h.finish())
}

/// The table's column a WHERE clause tests (a binary-compatible relabeling over it allowed).
fn column<'p>(n: Node<'p>) -> Option<&'p types_nodes::primnodes::Var<'p>> {
    let n = n.as_relabel_type().map_or(n, |r| r.arg);
    n.as_var().filter(|v| v.varno == 1 && v.varlevelsup == 0 && v.varattno > 0)
}

/// K1's walk: every node's tag, a constant's type (never its value), a column, an operator, a function; `ok` false (and the
/// walk stopped) at a node S5 leaves to the planner: a sublink or subplan, a parameter, an aggregate or window function, a
/// hashed-array operator (the planner marks it in place), a column of another level or table.
struct Shape {
    h: std::hash::DefaultHasher,
    ok: bool,
}

impl<'mcx> NodeWalker<'mcx> for Shape {
    fn visit(&mut self, n: Node<'mcx>) -> PgResult<bool> {
        let tag = n.node_tag();
        self.h.write_u16(tag as u16);
        if let Some(c) = n.as_const() {
            self.h.write_u32(c.consttype);
            self.h.write_i32(c.consttypmod);
            self.h.write_u32(c.constcollid);
            self.h.write_u8(u8::from(c.constisnull));
            return Ok(false);
        }
        if let Some(v) = n.as_var() {
            if v.varno != 1 || v.varlevelsup != 0 {
                self.ok = false;
                return Ok(true);
            }
            self.h.write_i16(v.varattno);
            self.h.write_u32(v.vartype);
            self.h.write_i32(v.vartypmod);
            self.h.write_u32(v.varcollid);
            return Ok(false);
        }
        let refused = matches!(
            tag,
            NodeTag::T_SubLink
                | NodeTag::T_SubPlan
                | NodeTag::T_AlternativeSubPlan
                | NodeTag::T_Param
                | NodeTag::T_Aggref
                | NodeTag::T_WindowFunc
                | NodeTag::T_WindowFuncRunCondition
                | NodeTag::T_GroupingFunc
                | NodeTag::T_ScalarArrayOpExpr
                | NodeTag::T_PlaceHolderVar
                | NodeTag::T_CurrentOfExpr
                | NodeTag::T_SetToDefault
                | NodeTag::T_MergeSupportFunc
                | NodeTag::T_NextValueExpr
                | NodeTag::T_ReturningExpr
        );
        if refused {
            self.ok = false;
            return Ok(true);
        }
        if let Some(o) = n.as_op_expr() {
            self.h.write_u32(o.opno);
        }
        if let Some(f) = n.as_func_expr() {
            self.h.write_u32(f.funcid);
        }
        nodes_core::expression_tree_walker(n, self)
    }
}

/// subquery_planner's preprocess_expression folding of the target list (EXPRKIND_TARGET: the list as one node), then of
/// the WHERE (EXPRKIND_QUAL), with eval_const_expressions; the WHERE stays an expression (canonicalize_qual and
/// make_ands_implicit are the planner's, and the identity on a flat AND of clauses).
fn fold<'mcx>(mcx: Mcx<'mcx>, q: &mut Query<'mcx>) -> PgResult<()> {
    if !q.targetList.is_nil() {
        let tl = core::mem::replace(&mut q.targetList, NodeList::nil());
        let folded = clauses::fold::eval_const_expressions(mcx, Node::mk_list(mcx, tl)?)?;
        q.targetList = folded.as_list().expect("a list folds to a list").clone_in(mcx)?;
    }
    let f = q.jointree.expect("checked by candidate");
    if let Some(w) = f.quals {
        let quals = Some(clauses::fold::eval_const_expressions(mcx, w)?);
        q.jointree = Some(mcx::alloc_leak_in(mcx, types_nodes::primnodes::FromExpr { fromlist: f.fromlist.clone_in(mcx)?, quals })?);
    }
    Ok(())
}

/// The folded WHERE as the planner's implicit-AND list, when every clause is `column = constant` under an operator of
/// initdb's (its selectivity estimator and function run at planning; a user's could raise or print there, and a reused
/// plan would skip it): `None` otherwise (folding made the shape value-dependent: a NULL, a CASE).
fn point_quals<'m>(mcx: Mcx<'m>, q: &Query<'m>) -> PgResult<Option<NodeList<'m>>> {
    let Some(w) = q.jointree.and_then(|f| f.quals) else {
        return Ok(None);
    };
    let list = match w.as_bool_expr() {
        Some(b) if b.boolop == BoolExprType::AND_EXPR => b.args.clone_in(mcx)?,
        Some(_) => return Ok(None),
        None => NodeList::make1(mcx, w)?,
    };
    let point = list.iter().all(|c| {
        c.as_op_expr().is_some_and(|op| op.args.len() == 2 && column(op.args.nth(0)).is_some() && op.args.nth(1).as_const().is_some() && gravity_expr::catalog::FuncFacts::builtin(op.opno))
    });
    Ok(point.then_some(list))
}

fn point_road<'mcx>(mcx: Mcx<'mcx>, q: &'mcx mut Query<'mcx>, key: &Key, mode: i32) -> PgResult<PlannedStmt<'mcx>> {
    let rte = q.rtable.nth(0).as_range_tbl_entry().expect("checked by candidate");
    let (relid, lockmode) = (rte.relid, rte.rellockmode);
    let quals = point_quals(mcx, q)?;
    let why = match quals {
        None => Some("folded where"),
        Some(_) => lock_indexes(mcx, relid, lockmode)?,
    };
    if let Some(why) = why {
        if reporting() {
            report(&format!("noplan declined ({why})")); // alloc: Cold
        }
        return plan_now(mcx, q);
    }
    let quals = quals.expect("checked above");
    match hit(mcx, q, &quals, key)? {
        Look::Hit(stmt, checked, tick) => {
            check(mcx, q, &stmt, checked, mode)?;
            if reporting() {
                report("noplan hit");
            }
            let plan = stmt.planTree.map_or(0, |n| n.as_raw().as_ptr() as usize);
            HIT.with(|h| h.set(Some(Hit { k1: key.k1, user: key.user, tick, plan })));
            Ok(stmt)
        }
        Look::Miss(why) => miss(mcx, q, relid, key, why, None),
        Look::Confirm(seen) => miss(mcx, q, relid, key, "confirming", Some(seen)),
    }
}

/// The planner, as pg_plan_query calls it (its log_planner_stats and debug_print_plan wrappers decline S5 first).
fn plan_now<'mcx>(mcx: Mcx<'mcx>, q: &'mcx mut Query<'mcx>) -> PgResult<PlannedStmt<'mcx>> {
    planner_seams::planner::call(mcx, q, "", CURSOR_OPT_PARALLEL_OK, ParamListHandle::NULL)
}

/// The table's tests (`Some`: the planner plans the statement untouched), under the parser's lock (NoLock here, as
/// plancat's table_open).
fn table_check(mcx: Mcx<'_>, relid: Oid) -> PgResult<Option<&'static str>> {
    let heap = table::table_open(mcx, relid, NoLock)?;
    let why = table_facts(&heap);
    table::table_close(heap, NoLock)?;
    Ok(why)
}

fn table_facts(heap: &Relation<'_>) -> Option<&'static str> {
    if let Some(why) = rel::table_refusal(heap) {
        return Some(why);
    }
    if heap.rd_rel.relhassubclass || heap.rd_rel.relispartition {
        return Some("inheritance or partition");
    }
    if heap.rd_att.constr.as_ref().is_some_and(|c| c.has_generated_virtual) {
        return Some("virtual generated column");
    }
    if !heap.rd_rel.relhasindex {
        return Some("no index");
    }
    None
}

/// get_relation_info's locks, after the fold as the planner takes them: every index of RelationGetIndexList's, in its
/// order, opened at the RTE's lock mode and closed keeping it, an invalid one too. A table with a partial index declines:
/// whether the planner may use it is a proof over the constants.
fn lock_indexes(mcx: Mcx<'_>, relid: Oid, lockmode: LOCKMODE) -> PgResult<Option<&'static str>> {
    let list = relcache_seams::relation_get_index_list::call(mcx, relid)?;
    let mut partial = false;
    for &oid in list.iter() {
        let index = indexam::index_open(mcx, oid, lockmode)?;
        partial |= index.rd_index.as_ref().is_some_and(|f| f.has_indpred);
        indexam::index_close(index, NoLock)?;
    }
    Ok(partial.then_some("partial index"))
}

enum Look<'m> {
    /// The plan, its class against the planner's when C3's check mode checked the slot's reads (`Held::check`), and the
    /// slot's tick.
    Hit(PlannedStmt<'m>, Option<Class>, u64),
    Miss(&'static str),
    /// The shape's plan is being confirmed (`Slot::seen`): planned again, counted.
    Confirm((Choice, u8)),
}

/// A negative slot of `key` whose reads and settings stand (`true`: the planner, untouched); a stale one goes.
fn negative_stands(key: &Key) -> bool {
    MEMO.with(|cell| {
        let Ok(mut g) = cell.try_borrow_mut() else {
            return false;
        };
        let Some(memo) = g.as_mut() else {
            return false;
        };
        memo.with_mut(|m| {
            let Some(i) = find(m, key) else {
                return false;
            };
            if m.slots[i].entry.is_some() || m.slots[i].seen.is_some() {
                return false;
            }
            if m.slots[i].guc_at == key.guc_at && m.slots[i].held.stands() {
                m.tick += 1;
                m.slots[i].tick = m.tick;
                return true;
            }
            out(m, i);
            false
        })
    })
}

fn find(m: &Memo<'_>, key: &Key) -> Option<usize> {
    m.slots.iter().position(|s| s.k1 == key.k1 && s.user == key.user)
}

/// The memo's plan for the folded statement `q` (its WHERE as `quals`), after the locks: the slot's settings and reads
/// stand and `q` is the filed statement but for its constants.
fn hit<'m>(mcx: Mcx<'m>, q: &Query<'m>, quals: &NodeList<'m>, key: &Key) -> PgResult<Look<'m>> {
    MEMO.with(|cell| {
        let Ok(mut g) = cell.try_borrow_mut() else {
            return Ok(Look::Miss("memo busy"));
        };
        let Some(memo) = g.as_mut() else {
            return Ok(Look::Miss("new shape"));
        };
        memo.with_mut(|m| -> PgResult<Look<'m>> {
            let Some(i) = find(m, key) else {
                return Ok(Look::Miss("new shape"));
            };
            m.tick += 1;
            let tick = m.tick;
            let s = &mut m.slots[i];
            if s.guc_at != key.guc_at {
                return Ok(Look::Miss("setting"));
            }
            if !s.held.stands() {
                return Ok(Look::Miss("stale"));
            }
            let built = match s.entry.as_ref() {
                None => return Ok(s.seen.map_or(Look::Miss("no point plan"), Look::Confirm)),
                Some(e) => e.with(|e| -> PgResult<Option<PlannedStmt<'m>>> {
                    let Some(f) = e.filed.as_ref() else {
                        return Ok(None);
                    };
                    if !same_shape(mcx, q, &e.qf, &e.holes)? {
                        return Ok(None);
                    }
                    build(mcx, q, quals, f).map(Some)
                })?,
            };
            let Some(stmt) = built else {
                return Ok(Look::Miss("shape"));
            };
            s.tick = tick;
            s.held.replay();
            // C3's check mode: the slot's fill done again now (the planner over the statement) must read nothing the
            // slot's reads miss, and answer the same plan but for estimates and choice
            let checked = if gravity_deps::checking() {
                let mut class = Ok(Class::Same);
                s.held.check("point noplan memo", || {
                    class = plancheck(mcx, q, &stmt);
                    !matches!(class, Ok(Class::Other(_)) | Err(_))
                });
                Some(class?)
            } else {
                None
            };
            Ok(Look::Hit(stmt, checked, tick))
        })
    })
}

/// `stmt` is `filed` but for its constants (each of the same type, typmod, collation and nullness): the same command,
/// range table, permissions, row marks, join tree, target entries' labels, and target and WHERE expressions once
/// `filed`'s constants (`holes`, in `holes_of_query` order) stand in for `stmt`'s.
fn same_shape<'m>(mcx: Mcx<'m>, stmt: &Query<'m>, filed: &Query<'_>, holes: &PgVec<'_, Node<'_>>) -> PgResult<bool> {
    let scalars = stmt.commandType == filed.commandType
        && stmt.querySource == filed.querySource
        && stmt.canSetTag == filed.canSetTag
        && stmt.resultRelation == filed.resultRelation
        && stmt.hasForUpdate == filed.hasForUpdate
        && stmt.r#override == filed.r#override;
    if !scalars || !lists_equal(&stmt.rtable, &filed.rtable) || !lists_equal(&stmt.rteperminfos, &filed.rteperminfos) || !lists_equal(&stmt.rowMarks, &filed.rowMarks) {
        return Ok(false);
    }
    let (Some(f1), Some(f0)) = (stmt.jointree, filed.jointree) else {
        return Ok(false);
    };
    if !lists_equal(&f1.fromlist, &f0.fromlist) || stmt.targetList.len() != filed.targetList.len() {
        return Ok(false);
    }
    let mut k = 0;
    for (a, b) in stmt.targetList.iter().zip(filed.targetList.iter()) {
        let (Some(a), Some(b)) = (a.as_target_entry(), b.as_target_entry()) else {
            return Ok(false);
        };
        let labels = a.resno == b.resno && a.resname == b.resname && a.ressortgroupref == b.ressortgroupref && a.resorigtbl == b.resorigtbl && a.resorigcol == b.resorigcol && a.resjunk == b.resjunk;
        if !labels || !expr_same(mcx, a.expr, b.expr, holes, &mut k)? {
            return Ok(false);
        }
    }
    let wheres = match (f1.quals, f0.quals) {
        (Some(a), Some(b)) => expr_same(mcx, a, b, holes, &mut k)?,
        (None, None) => true,
        _ => false,
    };
    Ok(wheres && k == holes.len())
}

fn expr_same<'m>(mcx: Mcx<'m>, a: Node<'m>, b: Node<'_>, holes: &PgVec<'_, Node<'_>>, k: &mut usize) -> PgResult<bool> {
    let mut same = true;
    let swapped = swap(mcx, a, holes, k, &mut same)?.unwrap_or(a);
    Ok(same && equal(swapped, b))
}

/// `n` with its constants replaced, in expression_tree_mutator's order, by `holes[k..]` (only for the comparison: a
/// stand-in's value may point into the memo's context); `same` false at a constant of another kind or past the holes.
fn swap<'m>(mcx: Mcx<'m>, n: Node<'m>, holes: &PgVec<'_, Node<'_>>, k: &mut usize, same: &mut bool) -> PgResult<Option<Node<'m>>> {
    stack_depth_core::check_stack_depth()?;
    if let Some(c) = n.as_const() {
        let h = holes.get(*k).and_then(|h| h.as_const());
        *k += 1;
        return match h {
            Some(h) if const_kind_eq(c, h) => Ok(Some(Node::mk(mcx, *h)?)),
            _ => {
                *same = false;
                Ok(None)
            }
        };
    }
    nodes_core::expression_tree_mutator(mcx, n, &mut |c| swap(mcx, c, holes, k, same))
}

fn const_kind_eq(a: &Const, b: &Const) -> bool {
    a.consttype == b.consttype && a.consttypmod == b.consttypmod && a.constcollid == b.constcollid && a.constlen == b.constlen && a.constbyval == b.constbyval && a.constisnull == b.constisnull
}

/// The constants of `q`'s target entries' expressions, then of its WHERE, in expression_tree_mutator's order.
fn holes_of_query<'e>(m: Mcx<'e>, q: &Query<'e>) -> PgResult<PgVec<'e, Node<'e>>> {
    let mut out = PgVec::new_in(m);
    for t in q.targetList.iter() {
        if let Some(te) = t.as_target_entry() {
            holes_of(m, te.expr, &mut out)?;
        }
    }
    if let Some(w) = q.jointree.and_then(|f| f.quals) {
        holes_of(m, w, &mut out)?;
    }
    Ok(out)
}

fn holes_of<'e>(m: Mcx<'e>, n: Node<'e>, out: &mut PgVec<'e, Node<'e>>) -> PgResult<()> {
    stack_depth_core::check_stack_depth()?;
    if n.as_const().is_some() {
        out.try_reserve(1).map_err(|_| m.oom(core::mem::size_of::<Node<'_>>()))?;
        out.push(n);
        return Ok(());
    }
    nodes_core::expression_tree_mutator(m, n, &mut |c| holes_of(m, c, out).map(|()| None))?;
    Ok(())
}

/// The plan of folded statement `q` from filed plan `f`: the IndexScan's target list is `q`'s entries (the filed entries'
/// labels; a junk entry, a row identity column, the filed one), its indexqualorig `q`'s WHERE clauses in the filed order,
/// its indexqual the filed clauses over the index's columns with `q`'s constants; LockRows' list is the scan's entries by
/// position (set_dummy_tlist_references); the statement's identity is `q`'s.
fn build<'m>(mcx: Mcx<'m>, q: &Query<'m>, quals: &NodeList<'m>, f: &Filed<'_>) -> PgResult<PlannedStmt<'m>> {
    let s0 = scan_of(f.root).expect("checked at filing");
    let scan = Node::mk(mcx, build_scan(mcx, q, quals, s0, &f.orig_of)?)?;
    let root = if let Some(lr0) = f.root.as_lock_rows() {
        let stl = &scan.as_index_scan().expect("built above").scan.plan.targetlist;
        let mut tl = NodeList::with_capacity(mcx, lr0.plan.targetlist.len())?;
        for (i, t) in lr0.plan.targetlist.iter().enumerate() {
            let constant = t.as_target_entry().is_some_and(|te| te.expr.as_const().is_some());
            tl.lappend(mcx, if constant { stl.nth(i) } else { copyfuncs::copy_object(mcx, t)? })?;
        }
        Node::mk(mcx, LockRows { plan: plan_like(mcx, &lr0.plan, tl, Some(scan))?, rowMarks: nodes_in(mcx, &lr0.rowMarks)?, epqParam: lr0.epqParam })?
    } else if let Some(mt0) = f.root.as_modify_table() {
        Node::mk(mcx, modify_like(mcx, mt0, scan)?)?
    } else {
        scan
    };
    let shell = copyfuncs::copy_planned_stmt(mcx, &f.shell)?;
    Ok(PlannedStmt {
        planTree: Some(root),
        queryId: types_nodes::SyncCell::new(q.queryId),
        canSetTag: q.canSetTag,
        stmt_location: q.stmt_location,
        stmt_len: q.stmt_len,
        ..shell
    })
}

fn build_scan<'m>(mcx: Mcx<'m>, q: &Query<'m>, quals: &NodeList<'m>, s0: &IndexScan<'_>, orig_of: &[u16]) -> PgResult<IndexScan<'m>> {
    let mut tl = NodeList::with_capacity(mcx, s0.scan.plan.targetlist.len())?;
    let mut entries = q.targetList.iter();
    for t in s0.scan.plan.targetlist.iter() {
        let te0 = t.as_target_entry().expect("checked at filing");
        if te0.resjunk {
            tl.lappend(mcx, copyfuncs::copy_object(mcx, t)?)?;
            continue;
        }
        let te1 = entries.next().and_then(|n| n.as_target_entry()).expect("checked by same_shape");
        nodes_core::fix_opfuncids(te1.expr)?;
        let te = TargetEntry {
            expr: te1.expr,
            resno: te0.resno,
            resname: te0.resname.and(te1.resname),
            ressortgroupref: te0.ressortgroupref,
            resorigtbl: te0.resorigtbl,
            resorigcol: te0.resorigcol,
            resjunk: false,
        };
        tl.lappend(mcx, Node::mk(mcx, te)?)?;
    }
    let n = s0.indexqual.len();
    let mut indexqual = NodeList::with_capacity(mcx, n)?;
    let mut indexqualorig = NodeList::with_capacity(mcx, n)?;
    for (i, c0) in s0.indexqual.iter().enumerate() {
        let clause = quals.nth(usize::from(orig_of[i]));
        nodes_core::fix_opfuncids(clause)?;
        indexqualorig.lappend(mcx, clause)?;
        let op0 = c0.as_op_expr().expect("checked at filing");
        let op1 = clause.as_op_expr().expect("checked by point_quals");
        let args = NodeList::make2(mcx, copyfuncs::copy_object(mcx, op0.args.nth(0))?, op1.args.nth(1))?;
        let op = OpExpr {
            opno: op0.opno,
            opfuncid: op0.opfuncid,
            opresulttype: op0.opresulttype,
            opretset: op0.opretset,
            opcollid: op0.opcollid,
            inputcollid: op0.inputcollid,
            args,
            location: op1.location,
        };
        indexqual.lappend(mcx, Node::mk(mcx, op)?)?;
    }
    Ok(IndexScan {
        scan: Scan { plan: plan_like(mcx, &s0.scan.plan, tl, None)?, scanrelid: s0.scan.scanrelid },
        indexid: s0.indexid,
        indexqual,
        indexqualorig,
        indexorderby: NodeList::nil(),
        indexorderbyorig: NodeList::nil(),
        indexorderbyops: OidList::nil(),
        indexorderdir: s0.indexorderdir,
    })
}

/// A filed node's Plan fields over a new target list and child (no qual, no initplan, no right child: `plan_gate`).
fn plan_like<'m>(mcx: Mcx<'m>, p: &Plan<'_>, targetlist: NodeList<'m>, lefttree: Option<Node<'m>>) -> PgResult<Plan<'m>> {
    Ok(Plan {
        disabled_nodes: p.disabled_nodes,
        startup_cost: p.startup_cost,
        total_cost: p.total_cost,
        plan_rows: p.plan_rows,
        plan_width: p.plan_width,
        parallel_aware: p.parallel_aware,
        parallel_safe: p.parallel_safe,
        async_capable: p.async_capable,
        plan_node_id: p.plan_node_id,
        targetlist,
        qual: NodeList::nil(),
        lefttree,
        righttree: None,
        initPlan: NodeList::nil(),
        extParam: bms_in(mcx, &p.extParam)?,
        allParam: bms_in(mcx, &p.allParam)?,
    })
}

/// The filed ModifyTable over a new scan (no RETURNING, WITH CHECK, ON CONFLICT or MERGE part: `plan_gate`).
fn modify_like<'m>(mcx: Mcx<'m>, mt0: &ModifyTable<'_>, scan: Node<'m>) -> PgResult<ModifyTable<'m>> {
    Ok(ModifyTable {
        plan: plan_like(mcx, &mt0.plan, NodeList::nil(), Some(scan))?,
        operation: mt0.operation,
        canSetTag: mt0.canSetTag,
        nominalRelation: mt0.nominalRelation,
        rootRelation: mt0.rootRelation,
        partColsUpdated: mt0.partColsUpdated,
        resultRelations: IntList::from_slice(mcx, mt0.resultRelations.as_slice())?,
        updateColnosLists: nodes_in(mcx, &mt0.updateColnosLists)?,
        withCheckOptionLists: NodeList::nil(),
        returningOldAlias: None,
        returningNewAlias: None,
        returningLists: NodeList::nil(),
        fdwPrivLists: nodes_in(mcx, &mt0.fdwPrivLists)?,
        fdwDirectModifyPlans: bms_in(mcx, &mt0.fdwDirectModifyPlans)?,
        rowMarks: NodeList::nil(),
        epqParam: mt0.epqParam,
        onConflictAction: mt0.onConflictAction,
        arbiterIndexes: OidList::nil(),
        onConflictSet: NodeList::nil(),
        onConflictCols: IntList::nil(),
        onConflictWhere: None,
        exclRelRTI: mt0.exclRelRTI,
        exclRelTlist: NodeList::nil(),
        mergeActionLists: NodeList::nil(),
        mergeJoinConditions: NodeList::nil(),
    })
}

fn nodes_in<'m>(mcx: Mcx<'m>, l: &NodeList<'_>) -> PgResult<NodeList<'m>> {
    let mut out = NodeList::with_capacity(mcx, l.len())?;
    for n in l.iter() {
        out.lappend(mcx, copyfuncs::copy_object(mcx, n)?)?;
    }
    Ok(out)
}

fn bms_in<'m>(mcx: Mcx<'m>, b: &Bitmapset<'_>) -> PgResult<Bitmapset<'m>> {
    let mut out = Bitmapset::empty();
    let mut x = b.next_member(-1);
    while x >= 0 {
        out.add_member(mcx, x)?;
        x = b.next_member(x);
    }
    Ok(out)
}

/// The IndexScan of a point plan: the root, or LockRows' or ModifyTable's child.
fn scan_of<'p>(root: Node<'p>) -> Option<&'p IndexScan<'p>> {
    let child = root.as_lock_rows().map(|l| l.plan.lefttree).or_else(|| root.as_modify_table().map(|m| m.plan.lefttree));
    match child {
        Some(c) => c?.as_index_scan(),
        None => root.as_index_scan(),
    }
}

/// A miss: the planner plans the folded statement under a recording of what it reads (and of the table's and its indexes'
/// storage statistics, which estimate_rel_size reads past the relcache's own invalidation: a TRUNCATE, a REINDEX), and
/// the plan is filed, counted toward its filing (`seen`: the slot's count so far), or the shape marked as planning to no
/// point plan.
fn miss<'mcx>(mcx: Mcx<'mcx>, q: &'mcx mut Query<'mcx>, relid: Oid, key: &Key, why: &'static str, seen: Option<(Choice, u8)>) -> PgResult<PlannedStmt<'mcx>> {
    let cmd = q.commandType;
    let marks = !q.rowMarks.is_nil();
    let entry = new_entry(q)?;
    let (planned, held) = gravity_deps::Held::fill(|| -> PgResult<PlannedStmt<'mcx>> {
        gravity_deps::note_stats(relid);
        for &index in relcache_seams::relation_get_index_list::call(mcx, relid)?.iter() {
            gravity_deps::note_stats(index);
        }
        plan_now(mcx, q)
    });
    let p = planned?;
    let filed = match entry {
        Some(e) => file(mcx, &p, cmd, marks, relid, e, held, key, seen)?,
        None => Err("memo busy"),
    };
    if reporting() {
        report(&match filed {
            Ok(Filing::Slot(_)) => format!("noplan miss ({why}) filed"),                            // alloc: Cold
            Ok(Filing::Confirming(n)) => format!("noplan miss ({why}) confirming {n}/{CONFIRM_MISSES}"), // alloc: Cold
            Err(no) => format!("noplan miss ({why}) not filed ({no})"),                              // alloc: Cold
        });
    }
    // the plan just filed is the slot's: hook B makes the slot's pieces at this statement (keep.rs)
    if let Ok(Filing::Slot(Some(tick))) = filed {
        let plan = p.planTree.map_or(0, |n| n.as_raw().as_ptr() as usize);
        HIT.with(|h| h.set(Some(Hit { k1: key.k1, user: key.user, tick, plan })));
    }
    Ok(p)
}

/// The memo (made at its first use) and an entry context under it holding the folded statement, copied before the
/// planner rewrites it in place; `None` while the memo is borrowed (a nested statement).
fn new_entry(q: &Query<'_>) -> PgResult<Option<McxOwned<EntryTy>>> {
    let ctx = MEMO.with(|cell| -> PgResult<Option<mcx::MemoryContext>> {
        let Ok(mut g) = cell.try_borrow_mut() else {
            return Ok(None);
        };
        if g.is_none() {
            *g = Some(memo_new()?);
        }
        Ok(g.as_ref().map(|memo| memo.context().new_child_small("GravityPointPlan")))
    })?;
    let Some(ctx) = ctx else {
        return Ok(None);
    };
    McxOwned::<EntryTy>::try_new(ctx, |m| {
        let qf = copyfuncs::copy_query(m, q)?;
        let holes = holes_of_query(m, &qf)?;
        Ok(Entry { qf, holes, filed: None })
    })
    .map(Some)
}

/// The memo's root, charged to MEMO_OWNER, its drop armed at the session's teardown.
fn memo_new() -> PgResult<McxOwned<MemoTy>> {
    let memo = McxOwned::<MemoTy>::try_new(gravity_driver::mem::root::context("GravityPointPlanMemo", true), |m| Ok(Memo { slots: PgVec::new_in(m), tick: 0, swept: l2cache::committed_moves(), sweep_at: 0, short: mcx::memory_short_epoch(), bytes: 0, freed: 0 }))?;
    memo.context().set_owner(MEMO_OWNER);
    super::super::execute::arm_teardown();
    Ok(memo)
}

/// What `file` did with a point plan.
enum Filing {
    /// Filed: the new slot's tick (`Hit::tick`), `None` when the memo had no room for it.
    Slot(Option<u64>),
    /// The n-th miss in a row whose planner made the same choice, not filed yet.
    Confirming(u8),
}

/// The planner's plan `p` of the entry's statement, when it is a point plan: filed at once when no other index of the
/// table has a key column the WHERE tests (`alone`), else at the `CONFIRM_MISSES`th miss in a row whose planner made the
/// same choice (`seen`: the slot's choice and count). A transient plan (an index this transaction cannot use yet) files
/// nothing and drops the key's slot.
#[allow(clippy::too_many_arguments)]
fn file(mcx: Mcx<'_>, p: &PlannedStmt<'_>, cmd: CmdType, marks: bool, relid: Oid, mut e: McxOwned<EntryTy>, held: gravity_deps::Held, key: &Key, seen: Option<(Choice, u8)>) -> PgResult<Result<Filing, &'static str>> {
    if p.transientPlan {
        remove(key);
        return Ok(Err("transient plan"));
    }
    let verdict = match plan_gate(mcx, p, cmd, marks, relid)? {
        Err(why) => Err(why),
        Ok(()) => e.with_mut_mcx(|m, e| fill_entry(m, e, p))?,
    };
    if let Err(why) = verdict {
        drop(e);
        put(key, held, None, None);
        return Ok(Err(why));
    }
    let root = p.planTree.expect("checked by plan_gate");
    let scan = scan_of(root).expect("checked by plan_gate");
    let choice = Choice { index: scan.indexid, top: root.node_tag() };
    let n = match seen {
        Some((c, n)) if c == choice => n.saturating_add(1),
        _ => 1,
    };
    if n < CONFIRM_MISSES && !alone(mcx, relid, scan)? {
        drop(e);
        put(key, held, None, Some((choice, n)));
        return Ok(Ok(Filing::Confirming(n)));
    }
    Ok(Ok(Filing::Slot(put(key, held, Some(e), None))))
}

/// No index of table `relid` but `scan`'s has a key column the WHERE tests (`scan`'s index clauses' columns: the WHERE's
/// every clause, `fill_entry`), so the planner has no other index to choose for the shape. An expression key column counts
/// as one the WHERE may test. The indexes are locked (`lock_indexes`).
fn alone(mcx: Mcx<'_>, relid: Oid, scan: &IndexScan<'_>) -> PgResult<bool> {
    let mut cols = [0i16; types_core::fmgr::INDEX_MAX_KEYS as usize];
    let mut n = 0;
    for c in scan.indexqualorig.iter() {
        let Some(v) = c.as_op_expr().and_then(|op| column(op.args.nth(0))) else {
            return Ok(false);
        };
        if n == cols.len() {
            return Ok(false);
        }
        cols[n] = v.varattno;
        n += 1;
    }
    let tested = &cols[..n];
    for &index in relcache_seams::relation_get_index_list::call(mcx, relid)?.iter() {
        if index == scan.indexid {
            continue;
        }
        let r = indexam::index_open(mcx, index, NoLock)?;
        let other = r.rd_index.as_ref().is_none_or(|f| f.indkey.iter().take(f.indnkeyatts as usize).any(|&a| a <= 0 || has(tested, a)));
        indexam::index_close(r, NoLock)?;
        if other {
            return Ok(false);
        }
    }
    Ok(true)
}

/// wasm32 compares element by element: `[i16]::contains`'s chunked fold lowers to a v8i16 -> v16i8 setcc that LLVM's
/// simd128 backend cannot select (D4700).
#[inline]
fn has(tested: &[i16], a: i16) -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        tested.iter().any(|&t| core::hint::black_box(t) == a)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        tested.contains(&a)
    }
}

/// The plan-level test of a point plan (S1's shape over every command): the statement's flags, one relation, an
/// IndexScan of the table's unique btree with equality on every key column against a constant (`probe::index_refusal`)
/// and nothing else, under LockRows (one row mark) or a bare ModifyTable at most.
fn plan_gate(mcx: Mcx<'_>, p: &PlannedStmt<'_>, cmd: CmdType, marks: bool, relid: Oid) -> PgResult<Result<(), &'static str>> {
    let stmt = p.commandType == cmd
        && p.utilityStmt.is_none()
        && !p.hasReturning
        && !p.hasModifyingCTE
        && !p.transientPlan
        && !p.dependsOnRole
        && !p.parallelModeNeeded
        && p.jitFlags == 0
        && p.partPruneInfos.is_nil()
        && p.appendRelations.is_nil()
        && p.subplans.is_nil()
        && p.rewindPlanIDs.is_empty()
        && p.rtable.len() == 1
        && p.relationOids.as_slice() == [relid];
    if !stmt {
        return Ok(Err("statement"));
    }
    let Some(root) = p.planTree else {
        return Ok(Err("plan"));
    };
    let wrapper = match (cmd, marks) {
        (CmdType::CMD_SELECT, false) => root.as_index_scan().map(|_| true),
        (CmdType::CMD_SELECT, true) => root.as_lock_rows().map(|l| l.plan.righttree.is_none() && l.plan.qual.is_nil() && l.plan.initPlan.is_nil() && l.rowMarks.len() == 1),
        _ => root.as_modify_table().map(|m| bare_modify(m, cmd)),
    };
    if wrapper != Some(true) {
        return Ok(Err("plan node"));
    }
    let Some(scan) = scan_of(root) else {
        return Ok(Err("scan node"));
    };
    let plan = &scan.scan.plan;
    if !plan.qual.is_nil() {
        return Ok(Err("filter clause"));
    }
    let bare = scan.scan.scanrelid == 1
        && plan.initPlan.is_nil()
        && plan.lefttree.is_none()
        && plan.righttree.is_none()
        && scan.indexorderby.is_nil()
        && scan.indexorderbyorig.is_nil()
        && scan.indexorderbyops.is_nil()
        && !scan.indexqual.is_nil()
        && scan.indexqual.len() == scan.indexqualorig.len();
    if !bare {
        return Ok(Err("scan"));
    }
    let index = indexam::index_open(mcx, scan.indexid, NoLock)?;
    let mut ops: probe::KeyOps = [(0, 0); types_core::fmgr::INDEX_MAX_KEYS as usize];
    let why = if index.rd_index.as_ref().is_some_and(|f| f.indrelid == relid) { probe::index_refusal(&index, &scan.indexqual, &[], &mut ops) } else { Ok(Some("index of another table")) };
    let closed = indexam::index_close(index, NoLock);
    let why = why?;
    closed?;
    Ok(why.map_or(Ok(()), Err))
}

fn bare_modify(m: &ModifyTable<'_>, cmd: CmdType) -> bool {
    m.operation == cmd
        && m.plan.targetlist.is_nil()
        && m.plan.qual.is_nil()
        && m.plan.initPlan.is_nil()
        && m.plan.righttree.is_none()
        && m.withCheckOptionLists.is_nil()
        && m.returningLists.is_nil()
        && m.returningOldAlias.is_none()
        && m.returningNewAlias.is_none()
        && m.rowMarks.is_nil()
        && m.onConflictAction == 0
        && m.arbiterIndexes.is_nil()
        && m.onConflictSet.is_nil()
        && m.onConflictCols.is_nil()
        && m.onConflictWhere.is_none()
        && m.exclRelTlist.is_nil()
        && m.mergeActionLists.is_nil()
        && m.mergeJoinConditions.is_nil()
        && !m.partColsUpdated
        && m.rootRelation == 0
        && m.resultRelations.as_slice() == [1]
}

/// The statement-level half of filing, against the entry's folded statement: each indexqualorig clause is one of its
/// WHERE clauses (each once, all of them) and its indexqual clause carries that clause's constant; the scan's non-junk
/// entries are its target entries in order (expression and name), its junk entries hold no constant; LockRows' entries
/// are the scan's by position. Then the plan is copied in.
fn fill_entry<'e>(m: Mcx<'e>, e: &mut Entry<'e>, p: &PlannedStmt<'_>) -> PgResult<Result<(), &'static str>> {
    let Some(quals) = point_quals(m, &e.qf)? else {
        return Ok(Err("folded where"));
    };
    let root = p.planTree.expect("checked by plan_gate");
    let scan = scan_of(root).expect("checked by plan_gate");
    let n = scan.indexqualorig.len();
    let mut used = [false; types_core::fmgr::INDEX_MAX_KEYS as usize];
    if quals.len() != n || n > used.len() {
        return Ok(Err("where clauses"));
    }
    let mut orig_of: PgVec<'e, u16> = PgVec::new_in(m);
    orig_of.try_reserve_exact(n).map_err(|_| m.oom(n * 2))?;
    for (i, c) in scan.indexqualorig.iter().enumerate() {
        let Some(j) = (0..n).find(|&j| !used[j] && equal(c, quals.nth(j))) else {
            return Ok(Err("index clause"));
        };
        used[j] = true;
        if !matches!((const_arg(scan.indexqual.nth(i)), const_arg(c)), (Some(a), Some(b)) if equal(a, b)) {
            return Ok(Err("index clause value"));
        }
        orig_of.push(j as u16);
    }
    let stl = &scan.scan.plan.targetlist;
    let mut entries = e.qf.targetList.iter();
    for t in stl.iter() {
        let Some(te) = t.as_target_entry() else {
            return Ok(Err("target list"));
        };
        if te.resjunk {
            if HasConst.visit(te.expr)? {
                return Ok(Err("junk entry"));
            }
            continue;
        }
        let Some(qte) = entries.next().and_then(|x| x.as_target_entry()) else {
            return Ok(Err("target list"));
        };
        if !equal(te.expr, qte.expr) || te.resname != qte.resname {
            return Ok(Err("target list"));
        }
    }
    if entries.next().is_some() {
        return Ok(Err("target list"));
    }
    if let Some(lr) = root.as_lock_rows() {
        for (i, t) in lr.plan.targetlist.iter().enumerate() {
            let ok = t.as_target_entry().is_some_and(|te| match (te.expr.as_var(), te.expr.as_const()) {
                (Some(v), _) => v.varno == OUTER_VAR,
                (None, Some(_)) => i < stl.len() && stl.nth(i).as_target_entry().is_some_and(|s| equal(te.expr, s.expr)),
                _ => false,
            });
            if !ok {
                return Ok(Err("lock rows target list"));
            }
        }
    }
    let c = copyfuncs::copy_planned_stmt(m, p)?;
    let root = c.planTree.expect("checked by plan_gate");
    e.filed = Some(Filed { shell: PlannedStmt { planTree: None, ..c }, root, orig_of });
    Ok(Ok(()))
}

/// A binary clause's constant right-hand side.
fn const_arg(x: Node<'_>) -> Option<Node<'_>> {
    x.as_op_expr().filter(|o| o.args.len() == 2).map(|o| o.args.nth(1)).filter(|a| a.as_const().is_some())
}

/// A constant anywhere under a node.
struct HasConst;

impl<'mcx> NodeWalker<'mcx> for HasConst {
    fn visit(&mut self, n: Node<'mcx>) -> PgResult<bool> {
        if n.as_const().is_some() {
            return Ok(true);
        }
        nodes_core::expression_tree_walker(n, self)
    }
}

/// The key's slot replaced by (or a new slot of) `entry` (None: no point plan, or with `seen` one being confirmed) with
/// `held`, then the least recently used slots out while the memo is past mempolicy's caps. The slot's tick (`Hit::tick`),
/// `None` when it was not placed.
fn put(key: &Key, held: gravity_deps::Held, entry: Option<McxOwned<EntryTy>>, seen: Option<(Choice, u8)>) -> Option<u64> {
    let bytes = entry.as_ref().map_or(0, |e| e.context().used()) + held.deps().len() * core::mem::size_of::<(gravity_deps::Dep, u64)>();
    MEMO.with(|cell| {
        let Ok(mut g) = cell.try_borrow_mut() else {
            return None;
        };
        let memo = g.as_mut()?;
        memo.with_mut(move |m| {
            m.tick += 1;
            let tick = m.tick;
            let slot = Slot { k1: key.k1, user: key.user, guc_at: key.guc_at, tick, bytes, held, entry, seen, kept: None, kept_bytes: 0, refused: 0, refused_at: 0 };
            match find(m, key) {
                Some(i) => {
                    let old = core::mem::replace(&mut m.slots[i], slot);
                    m.bytes = m.bytes.saturating_sub(old.all_bytes()) + bytes;
                    m.freed += 1;
                    drop(old);
                }
                None => {
                    if m.slots.try_reserve(1).is_err() {
                        return None;
                    }
                    m.slots.push(slot);
                    m.bytes += bytes;
                }
            }
            evict(m);
            Some(tick)
        })
    })
}

fn remove(key: &Key) {
    MEMO.with(|cell| {
        if let Ok(mut g) = cell.try_borrow_mut() {
            if let Some(memo) = g.as_mut() {
                memo.with_mut(|m| {
                    if let Some(i) = find(m, key) {
                        out(m, i);
                    }
                });
            }
        }
    });
}

/// Hook A's start: no hit for this statement yet.
pub(super) fn clear_hit() {
    HIT.with(|h| h.set(None));
}

/// Hook B's read, once per statement: the hit hook A made for `pstmt` (its plan the one hook A built).
pub(super) fn take_hit(pstmt: &PlannedStmt<'_>) -> Option<Hit> {
    let hit = HIT.with(|h| h.take())?;
    (pstmt.planTree.map(|n| n.as_raw().as_ptr() as usize) == Some(hit.plan)).then_some(hit)
}

fn find_hit(m: &Memo<'_>, hit: &Hit) -> Option<usize> {
    m.slots.iter().position(|s| s.k1 == hit.k1 && s.user == hit.user && s.tick == hit.tick)
}

/// What hook B runs a hit's statement over (`take_kept`).
pub(super) enum Taken {
    /// The slot's kept pieces, moved out for the statement's run (no borrow of the memo held while it runs).
    Kept(KeptBox),
    /// None kept: the run makes them for the slot (`put_kept` files them when they fit).
    Make,
    /// D4572: the pieces the slot's statement made last found no room and none came back since: the statement runs as
    /// with pgrust.point_keep off (its plain run, or Gravity's for an UPDATE), making nothing.
    NoRoom,
}

/// The hit's slot's kept pieces for the statement's run, or whether the run makes them (`Taken`).
pub(super) fn take_kept(hit: &Hit) -> Taken {
    let t = MEMO.with(|cell| {
        let Ok(mut g) = cell.try_borrow_mut() else {
            return Taken::Make;
        };
        let Some(memo) = g.as_mut() else {
            return Taken::Make;
        };
        memo.with_mut(|m| {
            let Some(i) = find_hit(m, hit) else {
                return Taken::Make;
            };
            if let Some(kb) = m.slots[i].kept.take() {
                let b = core::mem::take(&mut m.slots[i].kept_bytes);
                m.bytes = m.bytes.saturating_sub(b);
                return Taken::Kept(kb);
            }
            let want = m.slots[i].refused;
            if want == 0 {
                return Taken::Make;
            }
            // one look a move of `freed`: the bytes it frees are a slot's or a piece's, so the memo's total is read again
            if m.slots[i].refused_at == m.freed || !room(m, want) {
                m.slots[i].refused_at = m.freed;
                return Taken::NoRoom;
            }
            Taken::Make
        })
    });
    if matches!(t, Taken::NoRoom) && reporting() {
        report("keep no room");
    }
    t
}

/// D4572: whether `want` more bytes fit under the memo's byte cap as it is.
fn room(m: &Memo<'_>, want: usize) -> bool {
    // pgrust.point_portal_keep's kept probes count in the same byte cap
    !mempolicy::cache::point_memo_over(0, m.bytes.saturating_add(super::pkeep::bytes()).saturating_add(want))
}

/// Slot `i` out of the memo, its bytes with it.
fn out(m: &mut Memo<'_>, i: usize) {
    let s = m.slots.swap_remove(i);
    m.bytes = m.bytes.saturating_sub(s.all_bytes());
    m.freed += 1;
    drop(s);
}

/// The pieces back into the hit's slot after a run that returned, charged to the memo's holder; `deps`: what building
/// them read (pieces built by this run), added to the slot's reads, so a change to any of it drops the slot with them.
/// The least recently used slots go while the memo is past its caps. Dropped when the slot is gone or filed again since.
/// D4572: pieces made by this run (`deps`) take only room the memo has: past the byte cap with them they are dropped and
/// the slot remembers it (`Slot::refused`), so its statements make none until room comes back, and no other slot's
/// pieces go for them (an LRU over more statements than fit made pieces again at nearly every statement: TPC-C over 20
/// tables at t8 made 332,455 pieces in 30 s and used 95,616).
pub(super) fn put_kept(hit: &Hit, mut kept: KeptBox, deps: Option<&gravity_deps::Deps>) {
    kept.with_mut(|k| k.kept.settle(MEMO_OWNER));
    let bytes = keep::bytes(&kept);
    let parts = (deps.is_some() && super::reporting()).then(|| keep::parts(&kept));
    let mut entry = None;
    let mut refused = None;
    let left = MEMO.with(|cell| {
        let Ok(mut g) = cell.try_borrow_mut() else {
            return Some(kept);
        };
        let Some(memo) = g.as_mut() else {
            return Some(kept);
        };
        memo.with_mut(|m| {
            let Some(i) = find_hit(m, hit) else {
                return Some(kept);
            };
            if let Some(d) = deps {
                let s = &mut m.slots[i];
                let before = s.bytes;
                s.held.absorb(d);
                let reads = d.len() * core::mem::size_of::<(gravity_deps::Dep, u64)>();
                s.bytes += reads;
                m.bytes += reads;
                entry = Some((before, s.bytes));
                if !room(m, bytes) {
                    let s = &mut m.slots[i];
                    s.refused = bytes;
                    s.refused_at = m.freed;
                    refused = Some((m.bytes, m.slots.len()));
                    evict(m);
                    return Some(kept);
                }
            }
            let s = &mut m.slots[i];
            s.refused = 0;
            let was = core::mem::replace(&mut s.kept_bytes, bytes);
            let old = s.kept.replace(kept);
            m.bytes = m.bytes.saturating_sub(was) + bytes;
            evict(m);
            old
        })
    });
    drop(left);
    if let Some((memo, n)) = refused.filter(|_| super::reporting()) {
        super::report(&format!("keep refused {bytes} bytes (memo {memo} bytes, {n} slots)")); // alloc: Cold
    }
    if let Some((before, after)) = entry.filter(|_| super::reporting()) {
        // The entry's bytes without the pieces, then with them (the reads their making added, and the pieces).
        let parts = parts.unwrap_or_default();
        super::report(&format!("keep bytes {before} -> {} (reads {after}, pieces {bytes} = {parts})", after + bytes)); // alloc: Cold
    }
}

/// While the memo is past its caps: when only its bytes are (not its entry count), the least recently used slot's kept
/// pieces go first (the slot keeps its plan; its next statement builds them again), and a whole slot, plan included, goes
/// only when no slot keeps pieces any more; past the entry count, the least recently used slot goes.
fn evict(m: &mut Memo<'_>) {
    debug_assert_eq!(m.bytes, m.slots.iter().map(Slot::all_bytes).sum::<usize>(), "the memo's byte count");
    loop {
        // pgrust.point_portal_keep's kept probes count in the same byte cap (pkeep.rs: at most `pkeep::ENTRIES` of them)
        let bytes = m.bytes + super::pkeep::bytes();
        if !mempolicy::cache::point_memo_over(m.slots.len(), bytes) {
            return;
        }
        let count_over = mempolicy::cache::point_memo_over(m.slots.len(), 0);
        match out_of(count_over, || m.slots.iter().map(|s| (s.tick, s.kept.is_some()))) {
            Some(Out::Pieces(i)) => {
                let s = &mut m.slots[i];
                let b = core::mem::take(&mut s.kept_bytes);
                drop(s.kept.take());
                m.bytes = m.bytes.saturating_sub(b);
                m.freed += 1;
                if reporting() {
                    report(&format!("keep evicted {b} bytes (memo {bytes} bytes, {} slots)", m.slots.len())); // alloc: Cold
                }
            }
            Some(Out::Slot(i)) => {
                out(m, i);
                if reporting() {
                    report(&format!("noplan evicted a slot (memo {bytes} bytes, {} slots left)", m.slots.len())); // alloc: Cold
                }
            }
            None => return,
        }
    }
}

/// The memo trimmed to its caps after pkeep.rs filed a kept probe (its bytes count in the byte cap); nothing without a memo.
pub(super) fn trim() {
    let _ = MEMO.try_with(|cell| {
        if let Ok(mut g) = cell.try_borrow_mut() {
            if let Some(memo) = g.as_mut() {
                memo.with_mut(|m| evict(m));
            }
        }
    });
}

/// What `evict` drops next.
#[derive(Debug, PartialEq, Eq)]
enum Out {
    /// The slot's kept pieces (it keeps its plan).
    Pieces(usize),
    /// The whole slot.
    Slot(usize),
}

/// `evict`'s choice over the slots' (tick, keeps pieces): past the bytes cap only (`count_over` false), the least recently
/// used slot that keeps pieces gives them up; with none left, or past the entry count, the least recently used slot goes.
/// `slots` walks the slots afresh at each call.
fn out_of<I: Iterator<Item = (u64, bool)>>(count_over: bool, slots: impl Fn() -> I) -> Option<Out> {
    if !count_over {
        let lru = slots().enumerate().filter(|&(_, (_, kept))| kept).min_by_key(|&(_, (t, _))| t).map(|(i, _)| i);
        if let Some(i) = lru {
            return Some(Out::Pieces(i));
        }
    }
    victim(slots().map(|(t, _)| t)).map(Out::Slot)
}

/// At a statement's start (Memory Accounting's DDL check, D4550): when a committed catalog generation moved since the
/// last sweep (`l2cache::committed_moves`, one load) and at most once a second, the slots whose reads can never stand
/// again in this database go (`Deps::superseded_in`: a dropped or rewritten table, a truncated one's storage, a changed
/// catalog row), as the program tier's D4579 sweep: their shapes would plan again anyway, and a dropped table's are
/// never looked up again. A dropped database's memo goes with its sessions.
/// REACH (D4550): first, when the process's memory-short epoch moved since the memo was made
/// (`mcx::memory_short_epoch`, one relaxed load: the reclaimer below its low mark), the memo goes whole, as at the idle
/// release; the next filing makes it again.
fn sweep() {
    let moves = l2cache::committed_moves();
    let short = mcx::memory_short_epoch();
    let (out, shed) = MEMO.with(|cell| {
        let Ok(mut g) = cell.try_borrow_mut() else {
            return (None, None);
        };
        let Some(memo) = g.as_mut() else {
            return (None, None);
        };
        if memo.with(|m| m.short) != short {
            return (None, g.take());
        }
        let out = memo.with_mut(|m| -> Option<(usize, usize, usize, usize)> {
            if m.swept == moves {
                return None;
            }
            let now = pg_clock::mono_ns();
            if now < m.sweep_at {
                return None;
            }
            m.swept = moves;
            m.sweep_at = now.saturating_add(SWEEP_EVERY_NS);
            let db = init_small::globals::MyDatabaseId();
            let (n0, b0) = (m.slots.len(), m.bytes);
            m.slots.retain(|s| !s.held.deps().superseded_in(db));
            let (n1, b1) = (m.slots.len(), m.slots.iter().map(Slot::all_bytes).sum::<usize>());
            m.bytes = b1;
            if n1 != n0 {
                m.freed += 1;
            }
            Some((n0 - n1, b0 - b1, n1, b1))
        });
        (out, None)
    });
    if let Some(memo) = shed {
        let (n, b) = memo.with(|m| (m.slots.len(), m.slots.iter().map(Slot::all_bytes).sum::<usize>()));
        drop(memo);
        if reporting() {
            report(&format!("noplan short shed {n} slots ({b} bytes)")); // alloc: Cold
        }
    }
    if let Some((n, b, left, lb)) = out.filter(|&(n, ..)| n > 0 && reporting()) {
        report(&format!("noplan sweep {n} slots ({b} bytes) out, {left} slots ({lb} bytes) left")); // alloc: Cold
    }
}

/// The least recently used slot.
fn victim(ticks: impl Iterator<Item = u64>) -> Option<usize> {
    ticks.enumerate().min_by_key(|&(_, t)| t).map(|(i, _)| i)
}

/// The memo's drop at the session's teardown (`execute::arm_teardown`'s registration, through `drop_state_at_teardown`):
/// every slot and the root context go; a borrowed memo (never at the teardown) stays.
pub(super) fn drop_at_teardown() {
    let _ = MEMO.try_with(|m| m.try_borrow_mut().map(|mut g| g.take()));
}

/// The memo's drop at the idle release (`super::idle_release`), as at the teardown: its slots and bytes when there was one
/// (`None`: no memo, or a borrowed one, never the case at the idle release).
pub(super) fn drop_at_idle() -> Option<(usize, usize)> {
    let memo = MEMO.try_with(|m| m.try_borrow_mut().ok().and_then(|mut g| g.take())).ok().flatten()?;
    Some(memo.with(|m| (m.slots.len(), m.slots.iter().map(Slot::all_bytes).sum::<usize>())))
}

/// How a reused plan differs from the planner's for the same statement.
enum Class {
    Same,
    /// Only the costs, row or width estimates or the JIT flags.
    Estimates,
    /// Another index, or another scan node (the planner's choice by the constants' statistics).
    Choice,
    /// Anything else: a substitution bug.
    Other(&'static str),
}

/// `strict`, PGRUST_GRAVITY_POINT_PLANCHECK (proof runs) or C3's check mode (`checked`, from `hit`): the planner plans a
/// copy of the folded statement too (its folding evaluates nothing again) and the plans are compared. Other is an error
/// under `strict` and a LOG line otherwise; Estimates and Choice are LOG lines (their count is the reuse's effect).
fn check<'mcx>(mcx: Mcx<'mcx>, q: &Query<'mcx>, ours: &PlannedStmt<'mcx>, checked: Option<Class>, mode: i32) -> PgResult<()> {
    let class = if checked.is_some() {
        checked
    } else if mode == POINT_NOPLAN_STRICT || PLANCHECK.with(|c| env_once(c, "PGRUST_GRAVITY_POINT_PLANCHECK")) {
        Some(plancheck(mcx, q, ours)?)
    } else {
        None
    };
    match class {
        Some(Class::Other(why)) if mode == POINT_NOPLAN_STRICT => Err(mismatch_error(why)),
        Some(Class::Other(why)) => {
            report(&format!("noplan plancheck mismatch ({why})")); // alloc: Cold
            Ok(())
        }
        Some(Class::Estimates) => {
            report("noplan plancheck estimates");
            Ok(())
        }
        Some(Class::Choice) => {
            report("noplan plancheck choice");
            Ok(())
        }
        Some(Class::Same) => {
            if reporting() {
                report("noplan plancheck same");
            }
            Ok(())
        }
        None => Ok(()),
    }
}

#[cold]
fn plancheck<'mcx>(mcx: Mcx<'mcx>, q: &Query<'mcx>, ours: &PlannedStmt<'mcx>) -> PgResult<Class> {
    let copy = copyfuncs::copy_query(mcx, q)?;
    let mut v: PgVec<'mcx, Query<'mcx>> = PgVec::new_in(mcx);
    v.try_reserve_exact(1).map_err(|_| mcx.oom(core::mem::size_of::<Query<'_>>()))?;
    v.push(copy);
    let qs: &'mcx mut [Query<'mcx>] = v.leak();
    let copy: &'mcx mut Query<'mcx> = &mut qs[0];
    let theirs = plan_now(mcx, copy)?;
    Ok(compare(ours, &theirs))
}

fn compare(ours: &PlannedStmt<'_>, theirs: &PlannedStmt<'_>) -> Class {
    let stmt = ours.commandType == theirs.commandType
        && ours.canSetTag == theirs.canSetTag
        && ours.hasReturning == theirs.hasReturning
        && ours.hasModifyingCTE == theirs.hasModifyingCTE
        && ours.transientPlan == theirs.transientPlan
        && ours.dependsOnRole == theirs.dependsOnRole
        && ours.parallelModeNeeded == theirs.parallelModeNeeded
        && theirs.subplans.is_nil()
        && theirs.partPruneInfos.is_nil()
        && lists_equal(&ours.rtable, &theirs.rtable)
        && lists_equal(&ours.permInfos, &theirs.permInfos)
        && ours.resultRelations.as_slice() == theirs.resultRelations.as_slice()
        && ours.relationOids.as_slice() == theirs.relationOids.as_slice()
        && ours.paramExecTypes.as_slice() == theirs.paramExecTypes.as_slice()
        && marks_equal(&ours.rowMarks, &theirs.rowMarks)
        && ours.stmt_location == theirs.stmt_location
        && ours.stmt_len == theirs.stmt_len;
    if !stmt {
        return Class::Other("statement");
    }
    let (Some(a), Some(b)) = (ours.planTree, theirs.planTree) else {
        return Class::Other("plan");
    };
    let Some(sa) = scan_of(a) else {
        return Class::Other("our plan");
    };
    let Some(sb) = scan_of(b) else {
        return Class::Choice;
    };
    if a.node_tag() != b.node_tag() {
        return Class::Other("plan node");
    }
    if sa.indexid != sb.indexid {
        return Class::Choice;
    }
    let scan = sa.scan.scanrelid == sb.scan.scanrelid
        && sa.indexorderdir == sb.indexorderdir
        && lists_equal(&sa.scan.plan.targetlist, &sb.scan.plan.targetlist)
        && lists_equal(&sa.scan.plan.qual, &sb.scan.plan.qual)
        && lists_equal(&sa.indexqual, &sb.indexqual)
        && lists_equal(&sa.indexqualorig, &sb.indexqualorig)
        && sb.indexorderby.is_nil()
        && sb.scan.plan.initPlan.is_nil();
    if !scan {
        return Class::Other("index scan");
    }
    let wrapper = match (a.as_lock_rows(), b.as_lock_rows(), a.as_modify_table(), b.as_modify_table()) {
        (Some(x), Some(y), _, _) => lists_equal(&x.plan.targetlist, &y.plan.targetlist) && marks_equal(&x.rowMarks, &y.rowMarks) && x.epqParam == y.epqParam,
        (_, _, Some(x), Some(y)) => {
            x.operation == y.operation
                && x.canSetTag == y.canSetTag
                && x.nominalRelation == y.nominalRelation
                && x.rootRelation == y.rootRelation
                && x.resultRelations.as_slice() == y.resultRelations.as_slice()
                && lists_equal(&x.updateColnosLists, &y.updateColnosLists)
                && x.epqParam == y.epqParam
                && x.onConflictAction == y.onConflictAction
                && x.fdwPrivLists.len() == y.fdwPrivLists.len()
                && y.returningLists.is_nil()
                && y.withCheckOptionLists.is_nil()
        }
        _ => true,
    };
    if !wrapper {
        return Class::Other("lock or modify node");
    }
    let est = |x: &Plan<'_>, y: &Plan<'_>| x.startup_cost == y.startup_cost && x.total_cost == y.total_cost && x.plan_rows == y.plan_rows && x.plan_width == y.plan_width;
    let top = |n: Node<'_>| n.as_lock_rows().map(|l| (l.plan.startup_cost, l.plan.total_cost, l.plan.plan_rows, l.plan.plan_width)).or_else(|| n.as_modify_table().map(|m| (m.plan.startup_cost, m.plan.total_cost, m.plan.plan_rows, m.plan.plan_width)));
    if est(&sa.scan.plan, &sb.scan.plan) && top(a) == top(b) && ours.jitFlags == theirs.jitFlags {
        Class::Same
    } else {
        Class::Estimates
    }
}

fn lists_equal(a: &NodeList<'_>, b: &NodeList<'_>) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| equal(x, y))
}

fn marks_equal(a: &NodeList<'_>, b: &NodeList<'_>) -> bool {
    a.len() == b.len()
        && a.iter().zip(b.iter()).all(|(x, y)| match (x.as_plan_row_mark(), y.as_plan_row_mark()) {
            (Some(x), Some(y)) => {
                x.rti == y.rti
                    && x.prti == y.prti
                    && x.rowmarkId == y.rowmarkId
                    && x.markType == y.markType
                    && x.allMarkTypes == y.allMarkTypes
                    && x.strength == y.strength
                    && x.waitPolicy == y.waitPolicy
                    && x.isParent == y.isParent
            }
            _ => false,
        })
}

#[cold]
fn mismatch_error(why: &str) -> Box<PgError> {
    Box::new(PgError::error(format!("point path: a reused plan differs from the planner's ({why})")).with_sqlstate(types_error::ERRCODE_INTERNAL_ERROR)) // alloc: Cold
}

#[cfg(test)]
mod tests {
    use super::*;
    use types_nodes::parsenodes::{RTEPermissionInfo, RangeTblEntry};
    use types_nodes::primnodes::{FromExpr, RangeTblRef, Var};

    const T: Oid = 70_000;
    const INT4: Oid = 23;
    const INT8: Oid = 20;
    const INT4EQ: Oid = 96;

    fn konst(mcx: Mcx<'_>, typ: Oid, v: i32) -> Node<'_> {
        let len = if typ == INT8 { 8 } else { 4 };
        Node::mk(mcx, Const { consttype: typ, consttypmod: -1, constcollid: 0, constlen: len, constvalue: datum::Datum::from_i32(v), constisnull: false, constbyval: true, location: -1 }).unwrap()
    }

    fn col(mcx: Mcx<'_>, attno: i16) -> Node<'_> {
        Node::mk(mcx, Var { varno: 1, varattno: attno, vartype: INT4, vartypmod: -1, ..Var::default() }).unwrap()
    }

    fn eq<'m>(mcx: Mcx<'m>, l: Node<'m>, r: Node<'m>) -> Node<'m> {
        let op = OpExpr { opno: INT4EQ, opfuncid: 65, opresulttype: 16, opretset: false, opcollid: 0, inputcollid: 0, args: NodeList::make2(mcx, l, r).unwrap(), location: -1 };
        Node::mk(mcx, op).unwrap()
    }

    fn and<'m>(mcx: Mcx<'m>, a: Node<'m>, b: Node<'m>) -> Node<'m> {
        let b = types_nodes::primnodes::BoolExpr { boolop: BoolExprType::AND_EXPR, args: NodeList::make2(mcx, a, b).unwrap(), location: -1 };
        Node::mk(mcx, b).unwrap()
    }

    /// `SELECT c1 FROM t WHERE <quals>`.
    fn select<'m>(mcx: Mcx<'m>, quals: Node<'m>) -> Query<'m> {
        let mut rte = Node::build::<RangeTblEntry<'m>>(mcx).unwrap();
        rte.rtekind = RTEKind::RTE_RELATION;
        rte.relid = T;
        rte.relkind = RELKIND_RELATION;
        rte.rellockmode = 1;
        rte.perminfoindex = 1;
        let perm = Node::mk(mcx, RTEPermissionInfo { relid: T, ..RTEPermissionInfo::default() }).unwrap();
        let te = TargetEntry { expr: col(mcx, 1), resno: 1, resname: Some("c1"), ressortgroupref: 0, resorigtbl: T, resorigcol: 1, resjunk: false };
        let from = FromExpr { fromlist: NodeList::make1(mcx, Node::mk(mcx, RangeTblRef { rtindex: 1 }).unwrap()).unwrap(), quals: Some(quals) };
        Query {
            commandType: CmdType::CMD_SELECT,
            canSetTag: true,
            rtable: NodeList::make1(mcx, rte.seal()).unwrap(),
            rteperminfos: NodeList::make1(mcx, perm).unwrap(),
            jointree: Some(mcx::alloc_leak_in(mcx, from).unwrap()),
            targetList: NodeList::make1(mcx, Node::mk(mcx, te).unwrap()).unwrap(),
            ..Query::default()
        }
    }

    #[test]
    fn candidate_takes_point_shapes_only() {
        let ctx = gravity_driver::mem::root::context("PointNoplanTest", true);
        let mcx = ctx.mcx();
        assert!(candidate(&select(mcx, eq(mcx, col(mcx, 1), konst(mcx, INT4, 7)))).is_some(), "column = constant");
        let two = and(mcx, eq(mcx, col(mcx, 1), konst(mcx, INT4, 7)), eq(mcx, col(mcx, 2), konst(mcx, INT4, 8)));
        assert!(candidate(&select(mcx, two)).is_some(), "two key columns");
        assert!(candidate(&select(mcx, eq(mcx, konst(mcx, INT4, 7), col(mcx, 1)))).is_none(), "the constant first");
        let twice = and(mcx, eq(mcx, col(mcx, 1), konst(mcx, INT4, 7)), eq(mcx, col(mcx, 1), konst(mcx, INT4, 8)));
        assert!(candidate(&select(mcx, twice)).is_none(), "one column twice: the equivalence classes judge by value");
        let mut sorted = select(mcx, eq(mcx, col(mcx, 1), konst(mcx, INT4, 7)));
        sorted.sortClause = NodeList::make1(mcx, konst(mcx, INT4, 1)).unwrap();
        assert!(candidate(&sorted).is_none(), "ORDER BY");
        let mut sub = select(mcx, eq(mcx, col(mcx, 1), konst(mcx, INT4, 7)));
        sub.hasSubLinks = true;
        assert!(candidate(&sub).is_none(), "a sublink");
    }

    #[test]
    fn the_shape_key_holds_constants_as_typed_holes() {
        let ctx = gravity_driver::mem::root::context("PointNoplanTest", true);
        let mcx = ctx.mcx();
        let k = |c, attno: i16| candidate(&select(mcx, eq(mcx, col(mcx, attno), c))).unwrap();
        assert_eq!(k(konst(mcx, INT4, 7), 1), k(konst(mcx, INT4, 9000), 1), "another value, the same shape");
        assert_ne!(k(konst(mcx, INT4, 7), 1), k(konst(mcx, INT8, 7), 1), "another type");
        assert_ne!(k(konst(mcx, INT4, 7), 1), k(konst(mcx, INT4, 7), 2), "another column");
    }

    #[test]
    fn the_shape_compare_swaps_constants_by_position() {
        let ctx = gravity_driver::mem::root::context("PointNoplanTest", true);
        let mcx = ctx.mcx();
        let filed = select(mcx, and(mcx, eq(mcx, col(mcx, 1), konst(mcx, INT4, 5)), eq(mcx, col(mcx, 2), konst(mcx, INT4, 7))));
        let holes = holes_of_query(mcx, &filed).unwrap();
        assert_eq!(holes.len(), 2);
        let same = select(mcx, and(mcx, eq(mcx, col(mcx, 1), konst(mcx, INT4, 1)), eq(mcx, col(mcx, 2), konst(mcx, INT4, 1))));
        assert!(same_shape(mcx, &same, &filed, &holes).unwrap(), "other values, equal ones too");
        let typed = select(mcx, and(mcx, eq(mcx, col(mcx, 1), konst(mcx, INT8, 5)), eq(mcx, col(mcx, 2), konst(mcx, INT4, 7))));
        assert!(!same_shape(mcx, &typed, &filed, &holes).unwrap(), "another constant type");
        let swapped = select(mcx, and(mcx, eq(mcx, col(mcx, 2), konst(mcx, INT4, 5)), eq(mcx, col(mcx, 1), konst(mcx, INT4, 7))));
        assert!(!same_shape(mcx, &swapped, &filed, &holes).unwrap(), "the columns in another order");
        let one = select(mcx, eq(mcx, col(mcx, 1), konst(mcx, INT4, 5)));
        assert!(!same_shape(mcx, &one, &filed, &holes).unwrap(), "fewer clauses");
    }

    #[test]
    fn the_victim_is_the_least_recently_used() {
        assert_eq!(victim([5u64, 2, 9].into_iter()), Some(1));
        assert_eq!(victim(core::iter::empty()), None);
    }

    #[test]
    fn past_the_bytes_cap_the_least_recently_used_pieces_go_before_any_slot() {
        let slots = [(5u64, true), (2, false), (9, true), (7, true)];
        assert_eq!(out_of(false, || slots.into_iter()), Some(Out::Pieces(0)), "the oldest slot that keeps pieces, not the oldest slot");
        let bare = [(5u64, false), (2, false), (9, false)];
        assert_eq!(out_of(false, || bare.into_iter()), Some(Out::Slot(1)), "no pieces left: the least recently used slot");
        assert_eq!(out_of(true, || slots.into_iter()), Some(Out::Slot(1)), "past the entry count: a whole slot");
        assert_eq!(out_of(false, core::iter::empty::<(u64, bool)>), None);
    }
}
