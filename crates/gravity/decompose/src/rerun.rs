//! FACTS-1b (M8-plan §7.1): F5 (volatility / stability / raise-free) over a plan subtree or an expression, and F1 (rescan
//! invariance) over a nest's inner or a nested SubPlan's plan. Both are read from PostgreSQL's plan tree and the catalog
//! (`Facts::func` / `Facts::oper`), never from a query shape. Every unknown answers the safe way: volatile, may raise, rows
//! that may differ on a rescan. A road that caches on F1 or runs early on F5 changes what PostgreSQL computes when a fact is
//! wrongly true, so each rule below errs towards false.
//!
//! The walk covers everything the subtree evaluates: every plan node's target list and quals, its node-specific
//! expressions (index quals and order-bys, a nest's params, hash / merge clauses and keys, LIMIT counts, window frame
//! offsets, function / VALUES scans, a Memoize's keys), the operators its sorts / groups / hashes compare with, the
//! InitPlans attached inside it, the plans of the SubPlans it calls and the CTE plans its CteScans read.
use gravity_program::{ArenaRef, ArenaVec};
use types_nodes::list::NodeList;
use types_nodes::plannodes::PlannedStmt;
use types_nodes::primnodes::{SubLinkType, SubPlan};
use types_nodes::{Node, NodeTag};

use crate::admit::Refusal;
use crate::catalog::Facts;
use crate::effects::Parallel;
pub use crate::effects::found::{dump_f5, Found};
use crate::graph::{slot_has, slot_set, Bound, Rerun, SlotSet};
use crate::lifetime;
use crate::ledger::Kind;
use crate::walk::Builder;


/// D3796 (`Walk::domain`): how many domains' CHECK expressions a walk reads inside one another before it fails closed.
const DOMAIN_DEPTH: usize = 8;

struct Walk<'a, 'p> {
    /// None: a reader with no statement (lowering's): every SubPlan and scanned relation is then unknown.
    stmt: Option<&'p PlannedStmt<'p>>,
    facts: &'a dyn Facts,
    found: Found,
    /// The `wtParam` of each RecursiveUnion inside the subtree: a WorkTableScan of one reads rows the subtree makes itself.
    wt: ArenaVec<i32>,
    /// The subplan ids already walked (a CTE read by several scans, a SubPlan met twice): each is walked once.
    seen: ArenaVec<i32>,
    /// What the walk leaves out of the subtree (`Cut`).
    cut: Option<Cut<'a, 'p>>,
    /// D1920 (`of_rescan`): a rescan changing these params walks only what PostgreSQL runs again: a FunctionScan's call whose
    /// funcparams it does not change (its tuplestore kept), a CTE's plan (filled once), an InitPlan or a hashed SubPlan whose
    /// plan reads none of them (its cell or table kept) and a BERNOULLI / SYSTEM sample's seed (drawn once per PlanState)
    /// are not run again.
    kept: Option<&'a [i32]>,
    /// D3615 (a): the last function `call` read, its `Facts::func` row, `effects::quiet_call` and `effects::call_acts` (an
    /// Append's arms call one comparison each).
    last_call: Option<(u32, Option<crate::effects::FuncProps>, bool, bool)>,
    arena: ArenaRef,
    /// D3796 (`domain`): the domains whose CHECK expressions this walk is inside, outermost first.
    domains: &'a [u32],
}

/// What a walk leaves out of the subtree it reads.
#[derive(Clone, Copy)]
enum Cut<'a, 'p> {
    /// `of_rerun`'s stop: a child `cut(parent, child)` names is not walked.
    Kids(&'a dyn Fn(Node<'p>, Node<'p>) -> bool),
    /// D3749 (`of_inner_after`): the CTEs whose plans a nest's outer row has run to their end (`whole`) or up to their
    /// first row (`started`, `first_row`), and whether the walk is inside a started CTE's plan above its first Sort (`top`).
    After { whole: &'a [i32], started: &'a [i32], top: bool },
}

/// The facts of plan subtree `root`.
pub fn of_plan<'p>(stmt: &'p PlannedStmt<'p>, facts: &dyn Facts, arena: ArenaRef, root: Node<'p>) -> Found {
    let mut w = Walk::new(Some(stmt), facts, arena);
    w.plan(root);
    w.found
}

/// The facts of the whole statement in one walk: its tree, then each subplan the tree did not reach. A SubPlan's or a
/// CTE's plan is walked once, through its call or alone.
pub fn of_all_plans<'p>(stmt: &'p PlannedStmt<'p>, facts: &dyn Facts, arena: ArenaRef) -> Found {
    let mut w = Walk::new(Some(stmt), facts, arena);
    if let Some(r) = stmt.planTree {
        w.plan(r);
    }
    for id in 1..=stmt.subplans.len() as i32 {
        if w.seen.contains(&id) {
            continue;
        }
        w.seen.push(id); // alloc: Cold
        if let Some(r) = subplan_root(stmt, id) {
            w.plan(r);
        }
    }
    w.found
}

/// D3749 (River's JobSchedule with few due jobs: a Nested Loop Left Join whose inner is a Materialize of a join that
/// reads the CTE jobs_with_rownum, its outer the CTE Scan of that CTE or a Hash Join that hashes it): the facts of a nest's
/// inner `root` read after its outer handed its first row, the CTEs that row ran (`first_row` of the outer) apart.
/// PostgreSQL runs the inner only once the outer handed a row (ExecNestLoop), so by then a `whole` CTE's store holds every
/// row (the inner's scan of it runs nothing of its plan), and a `started` one's plan ran below its first Sort (the inner's
/// scan runs only the nodes above it). Their stores stay for the statement, so the rows are the same at every rescan. A
/// LockRows in those plans (River's FOR UPDATE claim) is then no part of the inner.
pub fn of_inner_after<'p>(stmt: &'p PlannedStmt<'p>, facts: &dyn Facts, arena: ArenaRef, root: Node<'p>, (whole, started): (&[i32], &[i32])) -> Found {
    let mut w = Walk::new(Some(stmt), facts, arena);
    w.cut = Some(Cut::After { whole, started, top: false });
    w.plan(root);
    w.found
}

/// D3749: the CTEs plan subtree `n` has run once it handed its first row, into `whole` (its plan read to its end) and
/// `started` (its plan ran until it made its first row). A CTE Scan starts its CTE; a Sort, a Hash, a plain or hashed
/// Agg reads its child to the end before its first row (`drained`); a filter, projection, limit, lock or window node hands
/// on its child's row; a Hash Join's row comes after its Hash read its child whole, and an INNER / LEFT / SEMI / ANTI one's
/// after its outer handed a row; a Nested Loop's or a Merge Join's of those types after its outer handed a row. Anything
/// else adds nothing (an Append's arm, a Gather's worker: not known to have run).
pub fn first_row<'p>(stmt: &'p PlannedStmt<'p>, n: Node<'p>, whole: &mut ArenaVec<i32>, started: &mut ArenaVec<i32>) {
    crate::walk::stack_check();
    use types_nodes::jointype::JoinType;
    let Some(p) = n.as_plan() else { return };
    let outer = |t: JoinType| matches!(t, JoinType::JOIN_INNER | JoinType::JOIN_LEFT | JoinType::JOIN_SEMI | JoinType::JOIN_ANTI);
    match n.node_tag() {
        NodeTag::T_CteScan => {
            let id = n.as_cte_scan().unwrap().ctePlanId;
            if !started.contains(&id) {
                started.push(id); // alloc: Cold
                if let Some(r) = subplan_root(stmt, id) {
                    first_row(stmt, r, whole, started);
                }
            }
        }
        NodeTag::T_Sort | NodeTag::T_Hash => p.lefttree.into_iter().for_each(|c| drained(stmt, c, whole)),
        NodeTag::T_Agg if n.as_agg().is_some_and(|a| a.groupingSets.is_nil() && matches!(a.aggstrategy, crate::ops::shared::agg::AGG_PLAIN | crate::ops::shared::agg::AGG_HASHED)) => {
            p.lefttree.into_iter().for_each(|c| drained(stmt, c, whole))
        }
        NodeTag::T_Result if n.as_result().is_some_and(|r| r.resconstantqual.is_none()) => p.lefttree.into_iter().for_each(|c| first_row(stmt, c, whole, started)),
        NodeTag::T_Agg | NodeTag::T_Limit | NodeTag::T_LockRows | NodeTag::T_Unique | NodeTag::T_WindowAgg | NodeTag::T_Material | NodeTag::T_Group | NodeTag::T_IncrementalSort => {
            p.lefttree.into_iter().for_each(|c| first_row(stmt, c, whole, started))
        }
        NodeTag::T_SubqueryScan => n.as_subquery_scan().and_then(|s| s.subplan).into_iter().for_each(|c| first_row(stmt, c, whole, started)),
        NodeTag::T_HashJoin => {
            p.righttree.and_then(|h| h.as_plan()).and_then(|h| h.lefttree).into_iter().for_each(|c| drained(stmt, c, whole));
            if outer(n.as_hash_join().unwrap().join.jointype) {
                p.lefttree.into_iter().for_each(|c| first_row(stmt, c, whole, started));
            }
        }
        NodeTag::T_NestLoop | NodeTag::T_MergeJoin => {
            let t = n.as_nest_loop().map(|j| j.join.jointype).or(n.as_merge_join().map(|j| j.join.jointype));
            if t.is_some_and(outer) {
                p.lefttree.into_iter().for_each(|c| first_row(stmt, c, whole, started));
            }
        }
        _ => {}
    }
}

/// D3749 (`first_row`): the CTEs plan subtree `n` has run to their end once it was read to its end: a CTE Scan's (and
/// what its plan read whole); through nodes that read their child to its end (a filter or projection with no one-time
/// qual, a sort, a hash, an aggregate, a lock, a window node with no run condition, which stops early), a Nested Loop's
/// outer and every Append arm. Anything else adds nothing (a Limit, a Hash or Merge Join may stop before an input's end).
fn drained<'p>(stmt: &'p PlannedStmt<'p>, n: Node<'p>, whole: &mut ArenaVec<i32>) {
    crate::walk::stack_check();
    let Some(p) = n.as_plan() else { return };
    match n.node_tag() {
        NodeTag::T_CteScan => {
            let id = n.as_cte_scan().unwrap().ctePlanId;
            if !whole.contains(&id) {
                whole.push(id); // alloc: Cold
                if let Some(r) = subplan_root(stmt, id) {
                    drained(stmt, r, whole);
                }
            }
        }
        NodeTag::T_Result if n.as_result().is_some_and(|r| r.resconstantqual.is_none()) => p.lefttree.into_iter().for_each(|c| drained(stmt, c, whole)),
        NodeTag::T_WindowAgg if n.as_window_agg().is_some_and(|w| w.runCondition.is_nil()) => p.lefttree.into_iter().for_each(|c| drained(stmt, c, whole)),
        NodeTag::T_Sort | NodeTag::T_Hash | NodeTag::T_Agg | NodeTag::T_LockRows | NodeTag::T_Unique | NodeTag::T_Material | NodeTag::T_Group | NodeTag::T_IncrementalSort | NodeTag::T_NestLoop => {
            p.lefttree.into_iter().for_each(|c| drained(stmt, c, whole))
        }
        NodeTag::T_SubqueryScan => n.as_subquery_scan().and_then(|s| s.subplan).into_iter().for_each(|c| drained(stmt, c, whole)),
        NodeTag::T_Append => n.as_append().unwrap().appendplans.iter().for_each(|c| drained(stmt, c, whole)),
        _ => {}
    }
}

/// D4051 (join.rs `map_hash_join`, lowerv4 hashjoin build_class.rs `nonbuiltin_build`): the facts of a Hash's build,
/// `of_plan` over its subtree and hash keys, with a data-modifying CTE it reads taken as its store: PostgreSQL runs such a
/// CTE to its end whether or not the build runs (ExecPostprocessPlan), so its write is no code of the build's.
pub fn of_build<'p>(stmt: &'p PlannedStmt<'p>, facts: &dyn Facts, arena: ArenaRef, root: Node<'p>) -> Found {
    let mut w = Walk::new(Some(stmt), facts, arena);
    w.seen.extend(writes(stmt)); // alloc: Cold
    w.plan(root);
    w.found
}

/// The plan ids of the statement's data-modifying CTEs (a subplan whose root is a ModifyTable).
fn writes<'p>(stmt: &'p PlannedStmt<'p>) -> impl Iterator<Item = i32> + 'p {
    (1..=stmt.subplans.len() as i32).filter(move |&id| subplan_root(stmt, id).is_some_and(|r| r.node_tag() == NodeTag::T_ModifyTable))
}

/// The facts of the part of subtree `root` a rescan runs again: the walk stops at a child `cut(parent, child)` names (a
/// node that keeps its state and does not rescan its children, the lifetime home's `shields_in`).
pub fn of_rerun<'p>(stmt: &'p PlannedStmt<'p>, facts: &dyn Facts, arena: ArenaRef, root: Node<'p>, cut: &dyn Fn(Node<'p>, Node<'p>) -> bool) -> Found {
    let mut w = Walk::new(Some(stmt), facts, arena);
    w.cut = Some(Cut::Kids(cut));
    w.plan(root);
    w.found
}

/// D1920: the facts of what a rescan of plan subtree `root` changing `changed` runs again (ExecReScan, a hashed SubPlan's
/// rebuild for another calling node: buildSubPlanHash): `of_rerun`'s walk stopped where PostgreSQL surely keeps state
/// (`lifetime::keeps_in`), less the stores its nodes keep (`Walk::kept`). `same_rows` then says the rescan makes the rows
/// the first run made, and `acts` whether it calls a VOLATILE function again.
pub fn of_rescan<'p>(stmt: &'p PlannedStmt<'p>, facts: &dyn Facts, arena: ArenaRef, root: Node<'p>, changed: &[i32]) -> Found {
    let cut = |up: Node<'p>, k: Node<'p>| lifetime::keeps_in(stmt, changed, up, k);
    let mut w = Walk::new(Some(stmt), facts, arena);
    w.cut = Some(Cut::Kids(&cut));
    w.kept = Some(changed);
    w.plan(root);
    w.found
}

/// The facts of expression `n` (a SubPlan in it counts its plan).
pub fn of_expr<'p>(stmt: &'p PlannedStmt<'p>, facts: &dyn Facts, arena: ArenaRef, n: Node<'p>) -> Found {
    let mut w = Walk::new(Some(stmt), facts, arena);
    w.expr(n);
    w.found
}

/// The facts of expression `n` in the lowering's per-expression scope: a SubPlan in it counts its own parts (its
/// arguments and test, and the raise of a sublink that may return more than one row), not its plan, which runs as the
/// plan's own pipelines and is read there (C1's lowering-end agreement check, lowerv4 effects/end_check.rs).
pub fn of_expr_own<'p>(stmt: &'p PlannedStmt<'p>, facts: &dyn Facts, arena: ArenaRef, n: Node<'p>) -> Found {
    let mut w = Walk::new(Some(stmt), facts, arena);
    crate::expr::walk(n, &mut |x| {
        if let Some(s) = x.as_sub_plan() {
            w.seen.push(s.plan_id); // alloc: Cold
        }
        true
    });
    w.expr(n);
    w.found
}

/// The facts of expression `n` read with no statement (lowering's readers): a SubPlan in it, and a scan, are unknown.
pub fn of_expr_alone<'p>(facts: &dyn Facts, arena: ArenaRef, n: Node<'p>) -> Found {
    let mut w = Walk::new(None, facts, arena);
    w.expr(n);
    w.found
}

/// The facts of node `n` alone, its children apart (the walk's per-node rules: its call or operator, an aggregate's
/// support functions, an I/O coercion's functions, a node that raises by itself; a SubPlan's sublink raise and plan, unknown
/// with no statement). A list's members are its children. A node the walk does not know is unknown: volatile, raising,
/// unstable. For a reader that visits the children itself, in PostgreSQL's evaluation order.
pub fn of_node<'p>(stmt: Option<&'p PlannedStmt<'p>>, facts: &dyn Facts, arena: ArenaRef, n: Node<'p>) -> Found {
    let mut w = Walk::new(stmt, facts, arena);
    if n.as_list().is_none() && matches!(w.node(n), Children::Kids) && !crate::expr::kids(n, &mut |_| {}) {
        w.unknown();
    }
    w.found
}

/// How the walk reaches a node's children once the node itself is judged.
#[derive(Clone, Copy)]
enum Children<'p> {
    /// Every child `expr::kids` names; a tag it does not know is unknown.
    Kids,
    /// This one expression (a ReturningExpr's, a run condition's argument).
    One(Node<'p>),
    /// None: a sort / group clause names operators only.
    Done,
}

/// The facts of a SubPlan's call: its test, its args and its plan (with the sublink's own raise: a second row).
pub fn of_subplan<'p>(stmt: &'p PlannedStmt<'p>, facts: &dyn Facts, arena: ArenaRef, s: &'p SubPlan<'p>) -> Found {
    let mut w = Walk::new(Some(stmt), facts, arena);
    w.opt(s.testexpr);
    w.list(&s.args);
    w.subplan(s);
    w.found
}

/// D4137 (`of_build`'s reading for a SubPlan's call): `of_subplan` with a data-modifying CTE its plan reads taken as its
/// store. PostgreSQL runs such a CTE to its end whether or not the call runs (ExecPostprocessPlan), so its write is no act
/// of the call's.
pub fn of_subplan_stored<'p>(stmt: &'p PlannedStmt<'p>, facts: &dyn Facts, arena: ArenaRef, s: &'p SubPlan<'p>) -> Found {
    let mut w = Walk::new(Some(stmt), facts, arena);
    w.seen.extend(writes(stmt).filter(|&id| id != s.plan_id)); // alloc: Cold
    w.opt(s.testexpr);
    w.list(&s.args);
    w.subplan(s);
    w.found
}

/// F1: PostgreSQL re-running subtree `root` with the same params returns the same rows. Its `allParam` holds only params
/// that stay fixed for the statement (`fixed_params`: so none of a nest's, a SubPlan's, a recursion's or an enclosing
/// query level's), and the effects home reads the walk's facts as returning the same rows (`Effects::same_rows`: nothing
/// in it acts, nothing in it makes different rows per run).
pub fn invariant<'p>(stmt: &'p PlannedStmt<'p>, arena: ArenaRef, root: Node<'p>, found: Found) -> bool {
    let Some(p) = root.as_plan() else { return false };
    if !crate::effects::Effects::of_found(found).same_rows {
        return false;
    }
    let fixed = fixed_params(stmt, arena);
    let mut epq: ArenaVec<i32> = ArenaVec::new_in(arena); // alloc: Cold
    each_plan_node(stmt, &mut |n| {
        if let Some(m) = n.as_modify_table() {
            epq.push(m.epqParam);
        }
        if let Some(l) = n.as_lock_rows() {
            epq.push(l.epqParam);
        }
    });
    p.allParam.iter().all(|x| fixed.contains(&x) || epq.contains(&x))
}

/// The statement's params that never change once set: those an InitPlan binds whose plan reads no param but such ones.
/// PostgreSQL re-runs an InitPlan only when a param its plan reads changed (`ExecReScan`: `ExecReScanSetParamPlan` under an
/// initPlan whose `extParam` meets `chgParam`), so its values stay the first ones for the statement.
pub fn fixed_params<'p>(stmt: &'p PlannedStmt<'p>, arena: ArenaRef) -> ArenaVec<i32> {
    let mut inits: ArenaVec<&'p SubPlan<'p>> = ArenaVec::new_in(arena); // alloc: Cold
    let mut take = |n: Node<'p>| {
        if let Some(p) = n.as_plan() {
            inits.extend(p.initPlan.iter().filter_map(|s| s.as_sub_plan()));
        }
    };
    if let Some(t) = stmt.planTree {
        each_node(t, &mut take);
    }
    for r in stmt.subplans.iter().flatten() {
        each_node(r, &mut take);
    }
    let mut fixed: ArenaVec<i32> = ArenaVec::new_in(arena); // alloc: Cold
    loop {
        let mut grew = false;
        for s in inits.iter() {
            if s.setParam.iter().all(|x| fixed.contains(&x)) {
                continue;
            }
            if subplan_root(stmt, s.plan_id).and_then(|r| r.as_plan()).is_some_and(|p| p.extParam.iter().all(|x| fixed.contains(&x))) {
                fixed.extend(s.setParam.iter());
                grew = true;
            }
        }
        if !grew {
            return fixed;
        }
    }
}

/// D4236 / D4249 / D3736: whether plan node `plan` may run after a row left the statement, as mapping sees it from the plan,
/// where a recheck licence's plan-order run cannot answer its error (exec admit `recheck`: `sent == 0`):
/// - it reads a parameter that changes per outer row (`allParam` past `fixed_params`: a correlated SubPlan, a LATERAL or
///   parameterised nest inner): every run after the first comes after the outer's earlier rows may have left;
/// - it sits in the plan of a SubPlan that is no CTE's (an InitPlan or a SubPlan, run on demand at its first read, which
///   may come after a UNION ALL arm sent its row).
/// A node in a CTE's plan is lowerv4's to answer (D4203: ops/ctescan/lower.rs runs the producers first or refuses).
pub(crate) fn runs_late<'p>(stmt: &'p PlannedStmt<'p>, arena: ArenaRef, plan: &types_nodes::plannodes::Plan<'p>) -> bool {
    let me = plan.plan_node_id;
    let mut ctes: ArenaVec<i32> = ArenaVec::new_in(arena); // alloc: Cold
    each_plan_node(stmt, &mut |n| {
        if let Some(c) = n.as_cte_scan() {
            ctes.push(c.ctePlanId);
        }
    });
    let holds = |r: Node<'p>| {
        let mut found = false;
        each_node(r, &mut |n| found |= n.as_plan().is_some_and(|p| p.plan_node_id == me));
        found
    };
    let sub = stmt.subplans.iter().enumerate().find(|(_, r)| r.is_some_and(holds)).map(|(i, _)| i as i32 + 1);
    if sub.is_some_and(|id| ctes.contains(&id)) {
        return false;
    }
    let fixed = fixed_params(stmt, arena);
    sub.is_some() || plan.allParam.iter().any(|x| !fixed.contains(&x))
}

pub fn subplan_root<'p>(stmt: &'p PlannedStmt<'p>, plan_id: i32) -> Option<Node<'p>> {
    let n = usize::try_from(plan_id).ok()?.checked_sub(1)?;
    (n < stmt.subplans.len()).then(|| stmt.subplans.nth(n)).flatten().filter(|r| r.as_plan().is_some())
}

/// Every plan node of the tree under `n` (its plan children; the SubPlans its expressions call are separate trees).
/// Every plan node of the statement: its tree and each subplan's, parents first.
pub fn each_plan_node<'p>(stmt: &'p PlannedStmt<'p>, f: &mut dyn FnMut(Node<'p>)) {
    for r in stmt.planTree.into_iter().chain(stmt.subplans.iter().flatten()) {
        each_node(r, f);
    }
}

pub(crate) fn each_node<'p>(n: Node<'p>, f: &mut dyn FnMut(Node<'p>)) {
    crate::walk::stack_check(); // D3748
    f(n);
    plan_kids(n, &mut |k| each_node(k, f));
}

/// D3271: the first LockRows in the tree under `n` (parents first).
pub(crate) fn find_lock<'p>(n: Node<'p>) -> Option<Node<'p>> {
    crate::walk::stack_check(); // D3748
    if n.node_tag() == NodeTag::T_LockRows {
        return Some(n);
    }
    let mut found = None;
    plan_kids(n, &mut |k| {
        if found.is_none() {
            found = find_lock(k);
        }
    });
    found
}

/// D3271: whether plan node `lock` is in the tree under `n`, and then whether plan node `id` is on the path from `n` down
/// to it (both ends included): None when `lock` is not there.
pub(crate) fn path_has(n: Node<'_>, lock: i32, id: i32) -> Option<bool> {
    crate::walk::stack_check(); // D3748
    let p = n.as_plan()?;
    if p.plan_node_id == lock {
        return Some(p.plan_node_id == id);
    }
    let mut found = None;
    plan_kids(n, &mut |k| {
        if found.is_none() {
            found = path_has(k, lock, id);
        }
    });
    found.map(|below| below || p.plan_node_id == id)
}

/// D3271: whether plan node `id` is in the tree under `n`.
pub(crate) fn holds_node(n: Node<'_>, id: i32) -> bool {
    let mut hit = false;
    each_node(n, &mut |x| hit |= x.as_plan().is_some_and(|p| p.plan_node_id == id));
    hit
}

fn plan_kids<'p>(n: Node<'p>, f: &mut dyn FnMut(Node<'p>)) {
    let Some(p) = n.as_plan() else { return };
    p.lefttree.into_iter().chain(p.righttree).for_each(&mut *f);
    let lists: [Option<&'p NodeList<'p>>; 5] = [
        n.as_append().map(|a| &a.appendplans),
        n.as_merge_append().map(|a| &a.mergeplans),
        n.as_bitmap_and().map(|a| &a.bitmapplans),
        n.as_bitmap_or().map(|a| &a.bitmapplans),
        n.as_agg().map(|a| &a.chain),
    ];
    for l in lists.into_iter().flatten() {
        l.iter().for_each(&mut *f);
    }
    if let Some(s) = n.as_subquery_scan().and_then(|s| s.subplan) {
        f(s);
    }
}

impl<'a, 'p> Walk<'a, 'p> {
    fn new(stmt: Option<&'p PlannedStmt<'p>>, facts: &'a dyn Facts, arena: ArenaRef) -> Walk<'a, 'p> {
        Walk { stmt, facts, found: Found::CLEAN, wt: ArenaVec::new_in(arena), seen: ArenaVec::new_in(arena), cut: None, kept: None, last_call: None, arena, domains: &[] } // alloc: Cold
    }

    fn unknown(&mut self) {
        self.found.add(Found::UNKNOWN);
    }

    fn raises(&mut self) {
        self.found.raise_free = false;
    }

    /// D3749 (`Cut::After`): whether the walk is inside a started CTE's plan above its first Sort.
    fn at_top(&self) -> bool {
        matches!(self.cut, Some(Cut::After { top: true, .. }))
    }

    /// D3749: sets `at_top` to `now` (under `Cut::After` only) and returns what it was.
    fn top(&mut self, now: bool) -> bool {
        match &mut self.cut {
            Some(Cut::After { top, .. }) => core::mem::replace(top, now),
            _ => false,
        }
    }

    fn func(&mut self, oid: u32) {
        self.call(oid, None);
    }

    /// A call of `oid`, over `args` and the node's input collation where the node has them (a FuncExpr's, an OpExpr's).
    fn call(&mut self, oid: u32, args: Option<(&NodeList<'_>, u32)>) {
        self.call_in(oid, args, false);
    }

    /// `call`; D3788: `list`, `args` are a ScalarArrayOpExpr's (`effects::found::call_raise_free`).
    fn call_in(&mut self, oid: u32, args: Option<(&NodeList<'_>, u32)>, list: bool) {
        self.found.user_call |= crate::effects::runs_user_code(self.facts, oid);
        self.found.may_write |= crate::effects::user_code_may_write(self.facts, oid);
        let (row, quiet, acts) = match self.last_call {
            Some((o, row, quiet, acts)) if o == oid => (row, quiet, acts),
            _ => {
                let row = self.facts.func(oid);
                let quiet = row.is_some() && crate::effects::quiet_call(self.facts, oid);
                let acts = row.is_some() && crate::effects::call_acts(self.facts, oid);
                self.last_call = Some((oid, row, quiet, acts));
                (row, quiet, acts)
            }
        };
        match row {
            Some(p) => {
                self.found.volatility = self.found.volatility.max(p.volatility);
                self.found.local_marks(oid, &p);
                self.found.acts |= acts;
                self.found.parallel = self.found.parallel.max(p.pool(oid));
                // F5 reads the effects home's raise-free answer (effects/found.rs `call_raise_free`)
                self.found.raise_free &= crate::effects::found::call_raise_free(self.facts, quiet, &p, oid, args, list);
            }
            None => self.unknown(),
        }
    }

    /// An aggregate (or window function) call: its own pg_proc row, and for a user aggregate its pg_aggregate support
    /// functions, since CREATE AGGREGATE stores the aggregate itself as IMMUTABLE whatever they are. A user aggregate the
    /// catalog has no pg_aggregate row for is unknown; a window function with none is not an aggregate.
    fn agg(&mut self, oid: u32, aggref: bool) {
        self.func(oid);
        match crate::effects::agg_calls(self.facts, oid) {
            crate::effects::AggCalls::Support(s) => s.each().for_each(|f| self.func(f)),
            crate::effects::AggCalls::NoRow if aggref => self.unknown(),
            _ => {}
        }
    }

    /// D3796: a coercion to domain `typid` checks each constraint of its chain (ExecEvalConstraintNotNull /
    /// ExecEvalConstraintCheck), any of which may raise (23502 / 23514), and a CHECK calls what its expression calls: each
    /// is walked as an expression of no statement (a CHECK holds no SubPlan; its CoerceToDomainValue is the coerced value),
    /// inside the callback, while the facts hold the list. A domain the facts cannot read, or one met again inside its own
    /// CHECK (PostgreSQL recurses until its stack-depth error), fails closed (`Found::DOMAIN_CHECK`).
    fn domain(&mut self, typid: u32) {
        let (facts, arena) = (self.facts, self.arena);
        if self.domains.contains(&typid) || self.domains.len() >= DOMAIN_DEPTH {
            return self.found.add(Found::DOMAIN_CHECK);
        }
        let mut inside: ArenaVec<u32> = ArenaVec::new_in(arena); // alloc: Cold
        inside.extend(self.domains.iter().copied().chain([typid]));
        let mut found = Found::CLEAN;
        let read = facts.domain_checks(typid, &mut |c| {
            found.raise_free = false;
            if let Some(e) = c {
                let mut w = Walk::new(None, facts, arena);
                w.domains = &inside;
                w.expr(e);
                found.add(w.found);
            }
        });
        self.found.add(if read { found } else { Found::DOMAIN_CHECK });
    }

    /// An operator call: its function as the node names it, else the operator's `oprcode`.
    fn oper(&mut self, opno: u32, funcid: u32) {
        self.oper_over(opno, funcid, None);
    }

    /// `oper` over the OpExpr's `args` and input collation (`call`).
    fn oper_over(&mut self, opno: u32, funcid: u32, args: Option<(&NodeList<'_>, u32)>) {
        match if funcid != 0 { Some(funcid) } else { self.facts.oper(opno).map(|o| o.code).filter(|&c| c != 0) } {
            Some(f) => self.call(f, args),
            None => self.op_alone(opno),
        }
    }

    /// KeySem (LANGUAGE internal; equivclass#53): an operator a node names alone (a Sort's `USING <`, a Group's `=`) that
    /// the builtin facts do not hold: the support functions the node runs for it (`Facts::op_calls`: its `oprcode`, an
    /// ordering operator's sortsupport and comparator, a hashable one's hash procs), each a call, when every one runs a
    /// builtin (`effects::body`: a builtin, or a user function `LANGUAGE internal` over one); an ordering operator whose
    /// family has neither proc for its type raises at the sort's first call ("missing support function"). Any other
    /// operator (a SQL or PL procedure, a C function, an unknown one) stays unknown.
    fn op_alone(&mut self, opno: u32) {
        match self.facts.op_calls(opno) {
            Some(c) if c.each().all(|p| crate::effects::body(self.facts, p).is_some()) => {
                c.each().for_each(|p| self.call(p, None));
                if c.no_order() {
                    self.raises();
                }
            }
            _ => self.unknown(),
        }
    }

    fn opers(&mut self, ops: &[u32]) {
        for &o in ops {
            self.oper(o, 0);
        }
    }

    fn list(&mut self, l: &'p NodeList<'p>) {
        // D4449: a Const calls, raises and writes nothing and has no child (a literal table's cells: one test each)
        for n in l.iter().filter(|n| n.as_const().is_none()) {
            self.expr(n);
        }
    }

    fn opt(&mut self, n: Option<Node<'p>>) {
        if let Some(n) = n {
            self.expr(n);
        }
    }

    /// A LIMIT / OFFSET bound: none, a null (no bound) or a non-negative constant never raises; anything else may.
    fn bound(&mut self, n: Option<Node<'p>>) {
        match crate::ops::bound(n) {
            Bound::All => {}
            Bound::Const(c) if c >= 0 => {}
            _ => self.raises(),
        }
        self.opt(n);
    }

    fn expr(&mut self, n: Node<'p>) {
        crate::walk::stack_check(); // D3748
        if let Some(l) = n.as_list() {
            return self.list(l); // a VALUES row
        }
        match self.node(n) {
            // NextValueExpr, CurrentOfExpr and any tag the walk does not know: volatile, raising, unstable
            Children::Kids => {
                if !crate::expr::kids(n, &mut |k| self.expr(k)) {
                    self.unknown();
                }
            }
            Children::One(k) => self.expr(k),
            Children::Done => {}
        }
    }

    /// Node `n` itself, its children apart: its calls and raises (a SubPlan's also its plan's), and how `expr` reaches
    /// its children.
    fn node(&mut self, n: Node<'p>) -> Children<'p> {
        self.found.may_write |= crate::effects::node_may_write(self.facts, n);
        match n.node_tag() {
            NodeTag::T_FuncExpr => {
                let f = n.as_func_expr().unwrap();
                self.call(f.funcid, Some((&f.args, f.inputcollid)))
            }
            NodeTag::T_OpExpr => {
                let o = n.as_op_expr().unwrap();
                self.oper_over(o.opno, o.opfuncid, Some((&o.args, o.inputcollid)))
            }
            // over their arguments and input collation, as an OpExpr (the home's collation bound, `call_raise_free`)
            NodeTag::T_DistinctExpr => {
                let o = n.as_distinct_expr().unwrap();
                self.oper_over(o.opno, o.opfuncid, Some((&o.args, o.inputcollid)))
            }
            NodeTag::T_NullIfExpr => {
                let o = n.as_null_if_expr().unwrap();
                self.oper_over(o.opno, o.opfuncid, Some((&o.args, o.inputcollid)))
            }
            NodeTag::T_ScalarArrayOpExpr => {
                let o = n.as_scalar_array_op_expr().unwrap();
                // D3788: over its arguments, so a pattern match over a constant list of good patterns reads raise-free
                // (`effects::quiet_pattern_list`), as the effects home's `quiet_node` reads it
                match if o.opfuncid != 0 { Some(o.opfuncid) } else { self.facts.oper(o.opno).map(|x| x.code).filter(|&c| c != 0) } {
                    Some(f) => self.call_in(f, Some((&o.args, o.inputcollid)), true),
                    None => self.op_alone(o.opno),
                }
                for f in [o.hashfuncid, o.negfuncid].into_iter().filter(|&f| f != 0) {
                    self.func(f);
                }
            }
            NodeTag::T_RowCompareExpr => {
                let r = n.as_row_compare_expr().unwrap();
                for op in r.opnos.iter() {
                    self.oper(op, 0);
                }
                // the home's rule (`effects::row_compare_quiet`): each column's comparison is its family's FUNCTION 1
                // (a user family's is user code) under the column's collation (an indeterminate one raises 42P22)
                if !crate::effects::row_compare_quiet(self.facts, r) {
                    self.raises();
                }
            }
            // D1108 (material): an aggregate's pg_proc row is never strict, so `func` reads every one as raising; the
            // effects home's answer for a one-phase builtin aggregate whose transitions and final raise on no input
            // (`effects::agg_quiet`) keeps raise-free. Its volatility still counts; its arguments, FILTER and sorts are
            // walked as before
            NodeTag::T_Aggref => {
                let a = n.as_aggref().unwrap();
                let before = self.found.raise_free;
                self.agg(a.aggfnoid, true);
                if a.aggsplit == types_nodes::primnodes::AGGSPLIT_SIMPLE && crate::effects::agg_quiet(a) {
                    self.found.raise_free = before;
                }
            }
            // D3749: a window function's pg_proc row may be not strict too (row_number's, which reads no argument): one
            // that raises on no input (`types_core::quiet::window_quiet`) keeps raise-free. Its volatility still counts;
            // its arguments and FILTER are walked as before
            NodeTag::T_WindowFunc => {
                let w = n.as_window_func().unwrap();
                let before = self.found.raise_free;
                self.agg(w.winfnoid, false);
                if types_core::quiet::window_quiet(w.winfnoid, w.args.len()) {
                    self.found.raise_free = before;
                }
            }
            // the argument type's output function and the result type's input function (`check_functions_in_node`:
            // getTypeOutputInfo / getTypeInputInfo, then each function's provolatile; D1071: a user type's through the
            // source's `Facts::type_io`, unknown without one); an input function raises on text it cannot read
            NodeTag::T_CoerceViaIO => {
                let c = n.as_coerce_via_io().unwrap();
                let out = crate::catalog::expr_type::type_of(c.arg).and_then(|t| self.facts.type_io(t)).map(|t| t.1);
                let input = self.facts.type_io(c.resulttype).map(|t| t.0);
                let before = self.found.raise_free;
                for f in [out, input] {
                    match f {
                        Some(f) => self.func(f),
                        None => self.unknown(),
                    }
                }
                // AGC-30: to a text-like type from a listed source neither function raises (the effects home's rule)
                match crate::catalog::expr_type::type_of(c.arg) {
                    Some(t) if types_core::quiet::io_quiet(t, c.resulttype) => self.found.raise_free = before,
                    _ => self.raises(),
                }
            }
            NodeTag::T_CoerceToDomain => self.domain(n.as_coerce_to_domain().unwrap().resulttype),
            NodeTag::T_ArrayCoerceExpr | NodeTag::T_MinMaxExpr | NodeTag::T_SubscriptingRef => self.raises(),
            // AGC-31: IS JSON by the effects home's rule (`effects::json_is_quiet`)
            NodeTag::T_JsonIsPredicate if crate::effects::json_is_quiet(n.as_json_is_predicate().unwrap()) => {}
            // AGC-03: XMLCONCAT / XMLELEMENT / XMLFOREST by the effects home's rule (`effects::xml_quiet`)
            NodeTag::T_XmlExpr if crate::effects::xml_quiet(n.as_xml_expr().unwrap()) => {}
            NodeTag::T_XmlExpr | NodeTag::T_JsonExpr | NodeTag::T_JsonConstructorExpr | NodeTag::T_JsonIsPredicate | NodeTag::T_JsonValueExpr | NodeTag::T_JsonBehavior => self.raises(),
            NodeTag::T_ArrayExpr if n.as_array_expr().unwrap().multidims => self.raises(),
            // CURRENT_DATE, CURRENT_USER, ..: stable (`contain_mutable_functions`); the role ones may raise (D3920:
            // `quiet::svf_raise_free`, the expression reader's answer too)
            NodeTag::T_SQLValueFunction => {
                self.found.stable_node();
                if !types_core::quiet::svf_raise_free(n.as_sql_value_function().unwrap().op as u32) {
                    self.raises();
                }
            }
            NodeTag::T_SubPlan => self.subplan(n.as_sub_plan().unwrap()),
            // D1112 (writesubplan): RETURNING's OLD / NEW image of a view column (PG 18's ReturningExpr) evaluates its
            // expression, or yields NULL with no such image (EEOP_RETURNINGEXPR): the expression's facts (a `(SELECT old)`
            // over a view passes that ROW(..) image as the SubPlan's argument)
            NodeTag::T_ReturningExpr => return Children::One(n.as_returning_expr().unwrap().retexpr),
            // an ordered / DISTINCT aggregate's clause: the operators it compares with, no expression under it
            NodeTag::T_SortGroupClause => {
                let c = n.as_sort_group_clause().unwrap();
                for o in [c.eqop, c.sortop].into_iter().filter(|&o| o != 0) {
                    self.oper(o, 0);
                }
                return Children::Done;
            }
            // a WindowAgg's run condition: `<op> arg` against the window function's value
            NodeTag::T_WindowFuncRunCondition => {
                let c = n.as_window_func_run_condition().unwrap();
                self.oper(c.opno, 0);
                return Children::One(c.arg);
            }
            _ => {}
        }
        Children::Kids
    }

    fn subplan(&mut self, s: &'p SubPlan<'p>) {
        let root = self.stmt.and_then(|st| subplan_root(st, s.plan_id));
        // `more than one row returned by a subquery used as an expression` (ExecScanSubPlan / ExecSetParamPlan): none
        // where the plan returns at most one row each run (the facts home's row bound)
        let one = root.zip(self.stmt).is_some_and(|(r, st)| crate::catalog::rows::max_rows(self.facts, st, r).is_some_and(|m| m <= 1));
        if matches!(s.subLinkType, SubLinkType::EXPR_SUBLINK | SubLinkType::ROWCOMPARE_SUBLINK | SubLinkType::MULTIEXPR_SUBLINK) && !one {
            self.raises();
        }
        if self.seen.contains(&s.plan_id) {
            return;
        }
        self.seen.push(s.plan_id);
        // a rescan keeps a hashed SubPlan's table, an InitPlan's cell and a CTE's rows while their plan reads no changed param
        let read = |r: Node<'p>| r.as_plan().is_some_and(|p| p.allParam.iter().any(|q| self.kept.is_some_and(|c| c.contains(&q))));
        let holds = s.useHashTable || !s.setParam.is_nil() || s.subLinkType == SubLinkType::CTE_SUBLINK;
        if self.kept.is_some() && holds && root.is_some_and(|r| !read(r) && s.parParam.iter().all(|q| !self.kept.is_some_and(|c| c.contains(&q)))) {
            return;
        }
        let top = self.top(false);
        match root {
            Some(r) => self.plan(r),
            None => self.unknown(),
        }
        self.top(top);
    }

    /// D1035 step 2: a scanned relation's hazard: a temporary relation is parallel-restricted (PostgreSQL's
    /// `set_rel_consider_parallel`: RELPERSISTENCE_TEMP), one the facts do not know unsafe.
    fn scanned(&mut self, scanrelid: u32) {
        let n = (scanrelid as usize).checked_sub(1);
        let rtable = self.stmt.map(|st| &st.rtable);
        let relid = rtable.and_then(|t| n.filter(|&n| n < t.len()).and_then(|n| t.nth(n).as_range_tbl_entry())).map(|r| r.relid);
        let hazard = match relid.map(|r| self.facts.rel_ext(r)) {
            Some(e) if e.known && !e.temp => Parallel::Safe,
            Some(e) if e.known => Parallel::Restricted,
            _ => Parallel::Unsafe,
        };
        self.found.parallel = self.found.parallel.max(hazard);
        // a sequence's row is the state nextval moves (SELECT last_value FROM s): a statement that reads one holds no
        // call-local proof; a relation the facts cannot name fails closed
        self.found.local_state |= relid.and_then(|r| self.facts.rel(r)).is_none_or(|r| r.relkind == b'S');
    }

    /// Range-table entry `scanrelid` is a heap relation by the facts.
    fn heap(&self, scanrelid: u32) -> bool {
        let n = (scanrelid as usize).checked_sub(1);
        let rte = self.stmt.and_then(|st| n.filter(|&n| n < st.rtable.len()).and_then(|n| st.rtable.nth(n).as_range_tbl_entry()));
        rte.and_then(|r| self.facts.rel(r.relid)).is_some_and(|r| r.am == crate::catalog::Am::Heap)
    }

    fn plan(&mut self, n: Node<'p>) {
        crate::walk::stack_check(); // D3748: one level per plan node of the subtree
        let Some(p) = n.as_plan() else { return self.unknown() };
        if self.at_top() && n.node_tag() == NodeTag::T_Sort {
            return;
        }
        let scan = match n.node_tag() {
            NodeTag::T_SeqScan => n.as_seq_scan().map(|s| s.scan.scanrelid),
            NodeTag::T_IndexScan => n.as_index_scan().map(|s| s.scan.scanrelid),
            NodeTag::T_IndexOnlyScan => n.as_index_only_scan().map(|s| s.scan.scanrelid),
            NodeTag::T_BitmapHeapScan => n.as_bitmap_heap_scan().map(|s| s.scan.scanrelid),
            NodeTag::T_TidScan => n.as_tid_scan().map(|s| s.scan.scanrelid),
            NodeTag::T_TidRangeScan => n.as_tid_range_scan().map(|s| s.scan.scanrelid),
            NodeTag::T_SampleScan => n.as_sample_scan().map(|s| s.scan.scanrelid),
            _ => None,
        };
        if let Some(r) = scan {
            self.scanned(r);
        }
        self.list(&p.targetlist);
        self.list(&p.qual);
        self.list(&p.initPlan);
        match n.node_tag() {
            NodeTag::T_SeqScan | NodeTag::T_Material | NodeTag::T_ProjectSet | NodeTag::T_BitmapAnd | NodeTag::T_BitmapOr | NodeTag::T_SubqueryScan => {}
            NodeTag::T_Result => self.opt(n.as_result().unwrap().resconstantqual),
            NodeTag::T_IndexScan => {
                let s = n.as_index_scan().unwrap();
                for l in [&s.indexqual, &s.indexqualorig, &s.indexorderby, &s.indexorderbyorig] {
                    self.list(l);
                }
            }
            NodeTag::T_IndexOnlyScan => {
                let s = n.as_index_only_scan().unwrap();
                for l in [&s.indexqual, &s.recheckqual, &s.indexorderby] {
                    self.list(l);
                }
            }
            NodeTag::T_BitmapIndexScan => {
                let s = n.as_bitmap_index_scan().unwrap();
                self.list(&s.indexqual);
                self.list(&s.indexqualorig);
            }
            NodeTag::T_BitmapHeapScan => self.list(&n.as_bitmap_heap_scan().unwrap().bitmapqualorig),
            NodeTag::T_TidScan => self.list(&n.as_tid_scan().unwrap().tidquals),
            NodeTag::T_TidRangeScan => self.list(&n.as_tid_range_scan().unwrap().tidrangequals),
            NodeTag::T_SampleScan => {
                // D1920 (`kept`): BERNOULLI and SYSTEM draw their seed once per PlanState (ExecInitSampleScan), so a rescan
                // over the same arguments samples the same rows; over a heap relation every run of the plan node in an
                // execution does too (D1920 / D1644: its sources share the node's one seed, hosts HostRegistry::sample_seed)
                let s = n.as_sample_scan().unwrap();
                let clause = s.tablesample.and_then(|t| t.as_table_sample_clause());
                let seeded = clause.is_some_and(|t| matches!(t.tsmhandler, crate::ops::shared::scan::TSM_BERNOULLI | crate::ops::shared::scan::TSM_SYSTEM));
                if !seeded || self.kept.is_none() && !self.heap(s.scan.scanrelid) {
                    self.found.rescan_stable = false;
                }
                // the clause's own raise at each scan's start (the effects home's `sample_class`; its arguments below)
                if !clause.is_some_and(|t| crate::effects::sample_class(t.tsmhandler, &t.args, t.repeatable, &crate::bake::read) == Some(true)) {
                    self.raises();
                }
                if let Some(t) = clause {
                    self.list(&t.args);
                    self.opt(t.repeatable);
                }
            }
            NodeTag::T_FunctionScan => {
                for f in n.as_function_scan().unwrap().functions.iter() {
                    match f.as_range_tbl_function() {
                        // `kept`: ExecReScanFunctionScan keeps the tuplestore while none of its funcparams changed
                        Some(r) if self.kept.is_some_and(|c| !r.funcparams.iter().any(|q| c.contains(&q))) => {}
                        Some(r) => self.opt(r.funcexpr),
                        None => self.unknown(),
                    }
                }
            }
            NodeTag::T_ValuesScan => self.list(&n.as_values_scan().unwrap().values_lists),
            NodeTag::T_CteScan => {
                let id = n.as_cte_scan().unwrap().ctePlanId;
                // `kept`: the CTE's rows are filled once per statement (its InitPlan, `subplan`'s rule)
                if self.kept.is_none() && !self.seen.contains(&id) {
                    self.seen.push(id);
                    // D3749 (`of_inner_after`): a CTE the nest's outer row ran whole runs nothing here; one it started
                    // ran below its first Sort then
                    let (whole, now) = match self.cut {
                        Some(Cut::After { whole, started, .. }) => (whole.contains(&id), started.contains(&id)),
                        _ => (false, false),
                    };
                    let top = self.top(now);
                    match self.stmt.and_then(|st| subplan_root(st, id)) {
                        _ if whole => {}
                        Some(r) => self.plan(r),
                        None => self.unknown(),
                    }
                    self.top(top);
                }
            }
            NodeTag::T_WorkTableScan => {
                if !self.wt.contains(&n.as_work_table_scan().unwrap().wtParam) {
                    self.found.rescan_stable = false;
                }
            }
            NodeTag::T_NamedTuplestoreScan => self.found.rescan_stable = false,
            NodeTag::T_RecursiveUnion => {
                let r = n.as_recursive_union().unwrap();
                self.wt.push(r.wtParam);
                self.opers(r.dupOperators);
            }
            NodeTag::T_NestLoop => {
                let j = n.as_nest_loop().unwrap();
                self.list(&j.join.joinqual);
                for np in j.nestParams.iter() {
                    match np.as_nest_loop_param() {
                        Some(np) => self.expr(np.paramval),
                        None => self.unknown(),
                    }
                }
            }
            NodeTag::T_MergeJoin => {
                let j = n.as_merge_join().unwrap();
                self.list(&j.join.joinqual);
                self.list(&j.mergeclauses);
            }
            NodeTag::T_HashJoin => {
                let j = n.as_hash_join().unwrap();
                self.list(&j.join.joinqual);
                self.list(&j.hashclauses);
                self.list(&j.hashkeys);
            }
            NodeTag::T_Hash => self.list(&n.as_hash().unwrap().hashkeys),
            NodeTag::T_Sort => self.opers(n.as_sort().unwrap().sortOperators),
            NodeTag::T_IncrementalSort => self.opers(n.as_incremental_sort().unwrap().sort.sortOperators),
            NodeTag::T_Group => self.opers(n.as_group().unwrap().grpOperators),
            NodeTag::T_Unique => self.opers(n.as_unique().unwrap().uniqOperators),
            NodeTag::T_SetOp => self.opers(n.as_set_op().unwrap().cmpOperators),
            NodeTag::T_Agg => self.opers(n.as_agg().unwrap().grpOperators),
            NodeTag::T_WindowAgg => {
                let w = n.as_window_agg().unwrap();
                self.opers(w.partOperators);
                self.opers(w.ordOperators);
                // a frame offset raises when negative or null
                if w.startOffset.is_some() || w.endOffset.is_some() {
                    self.raises();
                }
                self.opt(w.startOffset);
                self.opt(w.endOffset);
                self.list(&w.runCondition);
                for f in [w.startInRangeFunc, w.endInRangeFunc].into_iter().filter(|&f| f != 0) {
                    self.func(f);
                }
            }
            NodeTag::T_Limit => {
                let l = n.as_limit().unwrap();
                // WITH TIES raises on a null count
                if l.limitOption == types_nodes::nodes_enums::LimitOption::LIMIT_OPTION_WITH_TIES && !matches!(crate::ops::limit_bounds(l).0, Bound::Const(_)) {
                    self.raises();
                }
                self.bound(l.limitCount);
                self.bound(l.limitOffset);
                self.opers(l.uniqOperators);
            }
            NodeTag::T_Memoize => {
                let m = n.as_memoize().unwrap();
                self.list(&m.param_exprs);
                self.opers(m.hashOperators);
            }
            NodeTag::T_Append => {
                let a = n.as_append().unwrap();
                self.opers(a.gravity_sort_operators);
                if a.part_prune_index >= 0 {
                    self.raises(); // the run-time pruning steps evaluate their comparisons
                }
            }
            NodeTag::T_MergeAppend => {
                let a = n.as_merge_append().unwrap();
                self.opers(a.sortOperators);
                if a.part_prune_index >= 0 {
                    self.raises();
                }
            }
            NodeTag::T_LockRows => {
                self.found.rescan_stable = false;
                self.raises();
            }
            // a write (a data-modifying CTE), a foreign / custom / table-function scan, Gather, any other node
            _ => self.unknown(),
        }
        let cut = match self.cut {
            Some(Cut::Kids(c)) => Some(c),
            _ => None,
        };
        // D3749: only a chain of one-child nodes reaches the first Sort at the first fetch (an Append's later arm, a
        // join's inner may run after it)
        let mut kids = 0;
        plan_kids(n, &mut |_| kids += 1);
        let top = self.top(self.at_top() && kids == 1);
        plan_kids(n, &mut |k| {
            if !cut.is_some_and(|c| c(n, k)) {
                self.plan(k)
            }
        });
        self.top(top);
    }
}

// The rerun set (v5 rerun home, decompose half; interfaces.md §2.11 as corrected by proto2 §6): PG's chgParam and
// ExecReScan for an owner's re-open (a nest loop's inner, a nested SubPlan, a recursive union's generation): which
// pipelines of the inner re-run when the owner re-opens it, and which keep their rows. The set is fixed here, before
// lowering, so a family's answer is an arm of `on_rescan` by the closing kind, never a lowering method.

/// PG's chgParam: the parameters changed since a node's last run. PG adds every owner param to the inner's chgParam at
/// every re-open, whatever its value. Borrowed: decompose allocates nothing in steady state (core_zero_alloc).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ParamSet<'a>(pub &'a [i32]);

impl<'a> ParamSet<'a> {
    pub fn of(params: &'a [i32]) -> ParamSet<'a> {
        ParamSet(params)
    }
}

/// The owner whose re-open runs a rerun set: plan node `node_id` rescanning its righttree (a nest, a recursive union), or
/// with a `plan_id`, a SubPlan's plan.
#[derive(Clone, Copy, Debug)]
pub struct Owner {
    pub node_id: i32,
    pub plan_id: i32,
}

impl From<i32> for Owner {
    fn from(node_id: i32) -> Owner {
        Owner { node_id, plan_id: 0 }
    }
}

/// The inner a re-open of owner `node_id` rescans: a nest's or a recursive union's righttree, a SubPlan's plan.
pub(crate) fn owner_inner<'p>(b: &Builder<'p, '_>, node_id: i32, plan_id: i32) -> Option<Node<'p>> {
    match plan_id {
        0 => lifetime::node_of(b.stmt, node_id).and_then(|(n, _)| n.as_plan()?.righttree),
        _ => subplan_root(b.stmt, plan_id),
    }
}

/// The rerun set of an inner mapped since pipeline `p0` (graph.rs `Rerun`): the pipelines whose answer is `Rebuild`,
/// closed over the slots they fill. The slots a rerun pipeline fills or reads are leased for the statement
/// (`breakers::leased`): they are filled and read again per outer row, so none is reassigned. A slot only the embedded
/// inner reads stays leased by that read (the inner is never pushed).
pub fn declare_rerun(b: &mut Builder<'_, '_>, p0: usize, node_id: i32, plan_id: i32, params: &[i32]) -> Result<(), Refusal> {
    let k = if plan_id == 0 { Kind::NestLoop } else { Kind::SubPlan };
    let (set, slots, kept_by) = lifetime::rerun_set(b, p0, params, k, Owner { node_id, plan_id })?;
    b.graph.kept_by.extend(kept_by.iter().copied());
    if !set.is_empty() {
        let (keep, _) = lifetime::kept_hashed(b, p0, &set, params, false).map_err(|why| Refusal::shape(k, node_id, why))?;
        b.graph.kept.extend(keep.iter().copied());
        let changed = lifetime::changed_params(b, &set, params);
        b.graph.reruns.push(Rerun { node_id, plan_id, pipelines: set, slots });
        lifetime::claim_refills(b, p0, (b.graph.reruns.len() - 1) as u16, &changed);
    }
    Ok(())
}

/// D2711: the effects of the fill of the hashed Agg closing pipeline `q`, from the effects home's primitives: its input
/// subtree and its aggregates (arguments, FILTER, ORDER BY, transition calls). One that acts and may raise must not run
/// again where PG keeps its table: a call run again could raise anew.
pub(crate) fn fill_effects(b: &Builder<'_, '_>, q: usize) -> crate::effects::Effects {
    let Some(agg) = b.closers.get(q).and_then(|c| c.as_agg()) else { return crate::effects::Effects::UNKNOWN };
    let cx = crate::effects::Cx::new(b.stmt, b.facts, b.arena);
    let mut e = agg.plan.lefttree.map_or(crate::effects::Effects::UNKNOWN, |n| crate::effects::of_plan(&cx, n));
    for x in agg.plan.targetlist.iter().chain(agg.plan.qual.iter()) {
        crate::expr::run_aggrefs(x, &mut |a| e = e.and(crate::effects::of_expr(&cx, a, &crate::effects::PLAIN)));
    }
    e
}

/// D2612: whether pipeline `q` closes at a hashed Agg with one hash table (AGG_HASHED, no grouping sets), the one form
/// `kept_hashed` keeps (a grouping-sets table runs again at every re-open, as PG's spilled one does).
pub(crate) fn hashed_closer(b: &Builder<'_, '_>, q: usize) -> bool {
    b.closers.get(q).and_then(|c| c.as_agg()).is_some_and(|a| a.aggstrategy == crate::ops::shared::agg::AGG_HASHED && a.groupingSets.is_nil())
}

/// D4104: whether pipeline `q` closes at a hashed GROUPING SETS Agg in its tag form (ops/agg/gsets_tag.rs, D3029): one
/// pipeline whose one HashAgg breaker holds every set's groups (its group is no arm's `set`). PostgreSQL keeps every one of
/// that node's tables at a rescan that changes no param they read unless one spilled (ExecReScanAgg over all of
/// `perhash`), as it keeps a one-table one (`hashed_closer`), and lowerv4 reads its table again as it reads that one's.
/// The arm form (a spool and one HashAgg per set) is not this form.
pub(crate) fn tag_sets_closer(b: &Builder<'_, '_>, q: usize) -> bool {
    use gravity_abi::breaker::BreakerKind;
    b.closers.get(q).and_then(|c| c.as_agg()).is_some_and(|a| a.aggstrategy == crate::ops::shared::agg::AGG_HASHED && !a.groupingSets.is_nil())
        && matches!(b.graph.pipelines[q].desc.ends_in, gravity_program::End::Breaker(br) if matches!(br.kind, BreakerKind::HashAgg))
        && b.graph.group_of(gravity_program::LifeId(q as u16)).is_some_and(|g| g.set.is_none())
}

/// D1018 (nestloop item 9, main's option (a1)): a nest whose embedded inner reads breaker slot `slot` that no rerun set
/// of the nest refills (no NestLoopParam reaches the breaker's input). When PG re-executes every node that fills it at a
/// rescan (`reexecuted`), the pipelines that fill `slot` (a union's arms each feed it) and every pipeline since `p0` they
/// read from (through the slots those fill) join the nest's rerun set and run again per outer row. An InitPlan's
/// pipeline stays out: PG runs it once. When one of them is a node PG keeps (a Sort, a hashed aggregate, a hash build,
/// a Material, a CteScan: rewound or re-read, never re-run) there is no set and lowering refuses the inner by D1018's
/// name until reuse lands (item 9 (b)). An inner whose code acts (`acts`) gets no such set: a sorted aggregate's input
/// Sort, absorbed into its group here, is rewound by PG and would not repeat a VOLATILE call.
/// Item 9 (b), reuse: when the node closing the slot itself is one PG keeps at a rescan that changes no param (`kept_root`:
/// a Sort or a Limit's top-k rewinds, a hashed Agg or hashed SetOp keeps its table), PG re-executes nothing below it, so
/// the whole fill is one kept set: its pipelines join `graph.kept` too, and the set runs at its first run only, the
/// inner's first re-open (PG fills it at the first rescan, never under an empty outer); later re-opens rewind the emit
/// (lowerv4 `RERUN_KEPT`). A nest with a parameter-driven set already, a fill holding a hashed SubPlan's build (its own
/// road builds it once) or a Material root (nest.spool's road) gets no kept set; nor does a root the lifetime home's keep
/// table re-runs over an acting fill (a Sort without EXEC_FLAG_REWIND, which ExecInitNestLoop gives the inner only when
/// the nest has no NestLoopParam, D2616; an IncrementalSort, D2626) or keeps only while it never spilled (a hashed Agg
/// whose fill acts): D1018 refuses those. A call above the root (PG's postponed volatile target list under a LIMIT, the
/// inner's own ops) runs per re-open either way; a hashed SetOp, which PG always keeps, runs its fill's calls once, at the
/// first re-open, as PG does. D3978: a nested SubPlan's inner (`plan_id`) takes the re-executed roads alone (a plain Agg
/// over a constant-false Result, folded again at every evaluation as ExecReScanAgg does); its kept fills are D2117's.
/// Part 2: a re-executed root over a node PG keeps (a plain or sorted aggregate over a Sort, a top-k or a hashed group):
/// the set holds both, the kept node and its fill in `graph.kept` (run at the first re-open), the re-executed members run
/// at every re-open over its rewound rows; a hash build a re-executed member probes stays outside (built once, kept).
/// D3803 (DBeaver's lock tree: a nest inner HashSetOp Except over two CTE Scans): `also`, a hashed or sorted SetOp
/// emit's right group slot (the inner's op 0, `SetDesc::Set`), is walked as `slot` is, so both group fills join the set
/// (lowerv4 rerun/inner.rs `set_op` reads both as members; PG's ExecReScanSetOp keeps or rebuilds the table from both).
/// D4101 (n10): a Unique (or a Group whose own code is quiet, D1018) directly over a Sort the keep table keeps at this
/// rescan re-reads the rewound run and nothing else (`lifetime::rereads_run`), so under an inner that acts its pipeline, which holds both nodes, is a kept member and
/// a kept root as the Sort alone would be: the fill's VOLATILE calls run once, at the first re-open, and never under an
/// empty outer. A quiet inner keeps the (a1) set: its re-run shows nothing.
pub fn declare_breaker_rerun(b: &mut Builder<'_, '_>, p0: usize, (node_id, plan_id): (i32, i32), slot: u8, also: Option<u8>, acts: bool) {
    let n = b.graph.pipelines.len();
    let inner = owner_inner(b, node_id, plan_id);
    // a SubPlan mapped once per reference (D1337's copies, one key) owns the set its own mapping (since `p0`) declared
    let own = b.graph.reruns.iter().position(|r| r.node_id == node_id && r.plan_id == plan_id && (plan_id == 0 || r.pipelines.iter().all(|&q| q as usize >= p0)));
    if own.is_some_and(|i| slot_has(&b.graph.reruns[i].slots, slot)) {
        return;
    }
    let ps = &b.graph.pipelines;
    let init = |q: usize| b.graph.init_plans.iter().any(|i| i.pipeline as usize == q);
    let mut set: ArenaVec<u16> = ArenaVec::new_in(b.arena); // alloc: Cold
    let mut keep: ArenaVec<u16> = ArenaVec::new_in(b.arena); // alloc: Cold
    let mut edge: ArenaVec<u16> = ArenaVec::new_in(b.arena); // alloc: Cold
    let mut roots: ArenaVec<u16> = ArenaVec::new_in(b.arena); // alloc: Cold
    let mut todo: ArenaVec<(u8, usize, bool)> = ArenaVec::new_in(b.arena); // alloc: Cold
    let mut torn = false;
    todo.push((slot, n, false));
    todo.extend(also.map(|r| (r, n, false)));
    // every pipeline of the life of slot `s` its reader before `at` reads: the producers met walking back from the reader
    // (a union's arms all feed one slot; other readers of the same life may sit between), up to the first reader before
    // them (an earlier life of the slot number). `under`: the reader is one PG keeps, so it never rescans what it reads
    // (item 9 (b)); a hash build a re-executed member probes stays out of the set, as `rerun_set` leaves a one-batch Hash no
    // param reaches (built once, kept for the statement; ops/hashjoin/probe.rs judges such a kept table, D1904 / D1906)
    while let Some((s, at, under)) = todo.pop() {
        let mut met = false;
        for q in (p0..at).rev() {
            let d = ps[q].desc;
            if d.produces() == Some(s) {
                met = true;
                // D4101: a Unique or a Group that only re-reads its Sort's rewound run is kept with it (`lifetime::rereads_run`), asked
                // for an inner that acts alone: a quiet one keeps the (a1) road, where the re-run shows nothing
                let kq = under || !lifetime::reexecuted(b, q) || acts && lifetime::rereads_run(b, q);
                let hash = matches!(d.ends_in, gravity_program::End::Breaker(br) if br.kind == gravity_abi::breaker::BreakerKind::HashBuild);
                if init(q) || hash && !under && at != n {
                    continue;
                }
                if set.contains(&(q as u16)) {
                    torn |= keep.contains(&(q as u16)) != kq;
                    continue;
                }
                set.push(q as u16);
                if kq {
                    keep.push(q as u16);
                }
                if kq && !under && at != n {
                    edge.push(q as u16);
                }
                if at == n {
                    roots.push(q as u16);
                }
                todo.extend(d.consumes.iter().map(|&c| (c, q, kq)));
            } else if met && d.consumes.contains(&s) {
                break;
            }
        }
    }
    if set.is_empty() || torn {
        return;
    }
    if keep.len() == set.len() {
        // item 9 (b): kept whole when its root is a node PG keeps (the keep table: D2616's REWIND, D2626's IncrementalSort)
        if plan_id != 0 || own.is_some() || !roots.iter().all(|&q| kept_root(b, q as usize, inner)) || set.iter().any(|&q| sub_build(b, q as usize)) {
            return;
        }
    } else if acts || !keep.is_empty() && (own.is_some() || roots.iter().any(|q| keep.contains(q)) || keep.iter().any(|&q| sub_build(b, q as usize)) || !edge.iter().all(|&q| lifetime::keepable(b, q as usize))) {
        // a re-executed root: (a1) when nothing below is kept; item 9 (b) part 2 when what it reads includes a node PG
        // keeps (a Sort, a top-k, a hashed group under a plain or sorted aggregate): that node and its fill run at the first
        // re-open only, the re-executed members at every re-open over its rewound rows
        return;
    }
    b.graph.kept.extend(keep.iter().copied());
    let mut slots: SlotSet = own.map_or([0; 4], |i| b.graph.reruns[i].slots);
    for &q in set.iter() {
        for s in ps[q as usize].desc.consumes.iter().chain(ps[q as usize].desc.produces().iter()) {
            slot_set(&mut slots, *s);
        }
    }
    match own {
        Some(i) => {
            let r = &mut b.graph.reruns[i];
            for q in set {
                if !r.pipelines.contains(&q) {
                    r.pipelines.push(q);
                }
            }
            r.pipelines.sort_unstable();
            r.slots = slots;
        }
        None => {
            set.sort_unstable();
            b.graph.reruns.push(Rerun { node_id, plan_id, pipelines: set, slots });
        }
    }
}

/// D2117: nested SubPlan `plan_id` at node `node_id` (its plan mapped since pipeline `p0`, its embedded inner `inner`):
/// the fills the lifetime home keeps for it (`lifetime::sub_kept_fills`: run at the SubPlan's first evaluation, where
/// PostgreSQL 18.6's first scan of its plan runs them) join its rerun set as members PG keeps (`graph.kept`, lowerv4
/// `RERUN_KEPT`), the keep rule's earlier answers for them dropped; a SubPlan with no set gets one of those members alone.
pub fn declare_sub_kept(b: &mut Builder<'_, '_>, p0: usize, node_id: i32, plan_id: i32, inner: &crate::graph::PipelineNode<'_>) {
    let mut keep = lifetime::sub_kept_fills(b, p0, node_id, plan_id, inner);
    let own = b.graph.reruns.iter().position(|r| r.node_id == node_id && r.plan_id == plan_id);
    let ps = &b.graph.pipelines;
    if keep.is_empty() {
        return;
    }
    // the set runs them now: the keep rule's answers for them (`kept_by`, run once outside the set) no longer apply
    b.graph.kept_by.retain(|r| !(r.node_id == node_id && r.plan_id == plan_id && keep.contains(&r.pipeline)));
    b.graph.kept.extend(keep.iter().copied());
    let mut slots: SlotSet = own.map_or([0; 4], |i| b.graph.reruns[i].slots);
    for &q in keep.iter() {
        for s in ps[q as usize].desc.consumes.iter().chain(ps[q as usize].desc.produces().iter()) {
            slot_set(&mut slots, *s);
        }
    }
    match own {
        Some(i) => {
            let r = &mut b.graph.reruns[i];
            r.pipelines.extend(keep.iter().copied());
            r.pipelines.sort_unstable();
            r.slots = slots;
        }
        None => {
            keep.sort_unstable();
            b.graph.reruns.push(Rerun { node_id, plan_id, pipelines: keep, slots });
        }
    }
}

/// Whether pipeline `q` is a hashed SubPlan's build (its last op the build): built once by its own road, never a set's.
pub(crate) fn sub_build(b: &Builder<'_, '_>, q: usize) -> bool {
    matches!(b.graph.pipelines[q].ops.last(), Some(crate::graph::Op::SubPlan(d)) if matches!(d.form, crate::ops::shared::subplan::SubForm::Build))
}

/// Item 9 (b): whether pipeline `q`, a producer of a nest inner's slot, ends in a breaker lowering can rewind (a Sort, a
/// Limit's top-k, a hashed Agg or SetOp; not a re-executed closer, a Material, the spool road's, or a hash build) whose
/// closer the lifetime home's keep table keeps at a rescan that changes no param: by its PG node and flags, not the lowered
/// kind (a Sort rewinds only with randomAccess, which a nest inner gets from EXEC_FLAG_REWIND when the nest has no
/// NestLoopParam, D2616; an IncrementalSort always re-sorts, D2626; a hashed Agg keeps a table that never spilled), or
/// re-runs it over a quiet fill. An acting fill PG re-runs, or one whose spill decides it, is not kept (D1018 refuses).
/// D3908: a hashed Agg whose every key the planner dropped folds into one plain record (BreakerKind::PlainAgg); PG keeps
/// its one-entry table as any hashed Agg's, and the record's drain reads it again at each re-open, its aggregates
/// finished from the same transition values (none ordered, DISTINCT or ordered-set: those fold at the drain).
/// D4101 (n10): a re-executed closer is a kept root only as a Unique or a Group re-reading its kept Sort's run (`rereads_run`).
fn kept_root<'p>(b: &Builder<'p, '_>, q: usize, inner: Option<Node<'p>>) -> bool {
    use gravity_abi::breaker::BreakerKind;
    let gravity_program::End::Breaker(br) = b.graph.pipelines[q].desc.ends_in else { return false };
    let record = br.kind == BreakerKind::PlainAgg && hashed_closer(b, q) && plain_finals(b, q);
    let rewound = (matches!(br.kind, BreakerKind::Sort | BreakerKind::TopK | BreakerKind::HashAgg | BreakerKind::Distinct) || record) && (!lifetime::reexecuted(b, q) || lifetime::rereads_run(b, q)) && !sub_build(b, q);
    let site = lifetime::Site { keep_rerun: true, checks_unseen: false };
    rewound && b.closers.get(q).is_some_and(|&c| lifetime::keep_at(b, c, &[], inner, site).is_none_or(|k| matches!(k, lifetime::Kept::Keep { .. })))
}

/// D3908: every aggregate of the Agg closing pipeline `q` a plain one (no ORDER BY, no DISTINCT, not ordered-set); D4031's
/// quiet SubPlan fill too (lifetime/rerun.rs `member_reread`).
pub(crate) fn plain_finals(b: &Builder<'_, '_>, q: usize) -> bool {
    let Some(agg) = b.closers.get(q).and_then(|c| c.as_agg()) else { return false };
    let mut plain = true;
    for x in agg.plan.targetlist.iter().chain(agg.plan.qual.iter()) {
        crate::expr::run_aggrefs(x, &mut |a| plain &= a.as_aggref().is_some_and(|r| r.aggorder.is_nil() && r.aggdistinct.is_nil() && r.aggkind == types_nodes::primnodes::AGGKIND_NORMAL));
    }
    plain
}

/// D1920: a rescan of SubPlan `s`'s plan (its params and `changed` changed) keeps a VOLATILE call's rows: one of the stores
/// the lifetime home's walk finds (`lifetime::subplan_stores`) is filled by code that acts, by the effects home (an unknown
/// filler counts).
pub(crate) fn rescan_keeps<'p>(b: &Builder<'p, '_>, s: &'p SubPlan<'p>, changed: &[i32]) -> bool {
    let cx = crate::effects::Cx::new(b.stmt, b.facts, b.arena);
    lifetime::subplan_stores(b.stmt, b.arena, s, changed, &mut |st| match st {
        lifetime::Store::Plan(x) => crate::effects::of_plan(&cx, x).acts,
        lifetime::Store::Call(e) => crate::effects::of_expr(&cx, e, &crate::effects::PLAIN).acts,
        lifetime::Store::Unknown => true,
    })
}
