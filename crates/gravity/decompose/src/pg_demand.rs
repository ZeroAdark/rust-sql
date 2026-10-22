//! Construct C2 (b), the run-ahead tracer's plan half: the plan nodes PostgreSQL 18.6's executor may never run, however
//! the statement's rows fall. A Gravity pipeline whose source is such a node and which runs in program order (before any
//! row could ask for it) runs ahead of PostgreSQL's first demand: lowerv4 stamps it (program `Pipeline::pg_gate`) and
//! the driver's tracer (gravity_driver `ahead_trace`) checks that it holds a licence, or that its code cannot be seen.
//!
//! PostgreSQL's demand, node by node (PG 18.6 src/backend/executor):
//! - every subplan (`PlannedStmt::subplans`): an InitPlan runs at its param's first read (ExecSetParamPlan), a SubPlan
//!   where its expression is evaluated, a CTE's plan as its CteScan readers pull it. Except an InitPlan a gating
//!   Result's one-time qual reads (`resconstantqual`, the planner's pseudoconstant quals): it runs at that Result's first
//!   call, so exactly when the Result runs; a CTE's plan, which its first CteScan reader's first pull runs: it may never
//!   run only when every reader may never run; and a data-modifying CTE's plan (a subplan whose root is a ModifyTable),
//!   which ExecPostprocessPlan runs to its end whatever reads it (`es_auxmodifytables`): it always runs, whether the
//!   program runs it last (program `Pipeline::post`) or first (lowerv4 modifytable post.rs, the fail-closed order);
//! - a NestLoop's inner: at its outer's first row (an empty outer never runs it);
//! - a MergeJoin's inner: after its outer's first row (EXEC_MJ_INITIALIZE_OUTER), unless the join fills the inner (RIGHT,
//!   FULL, RIGHT ANTI: it reads the inner after an empty outer too);
//! - a HashJoin's Hash on PostgreSQL's outer-first path (`join::outer_first`): built after the outer's first row;
//! - a Result's one-time qual (`resconstantqual`): its child only when the qual holds;
//! - anything else: its children when it runs (a Sort, an Agg, a Hash, a Material, an Append's arms, a MergeAppend's arms,
//!   a Gather, a SubqueryScan). A Limit's count is not modelled (a LIMIT 0 never pulls its child): its child counts as run.
//!
//! Not here: an input PostgreSQL skips on the emptiness of another input it read first (a build-first HashJoin's outer
//! over an empty table, a SetOp's right input after an empty left). Gravity's program order reads that other input
//! first too, so whether it then skips the first is a skip (effects `Departure::Skip`), not a run ahead of demand.
use mcx::{CollectIn, GrowIn, Mcx, PgVec};
use types_nodes::jointype::JoinType;
use types_nodes::list::IntList;
use types_nodes::plannodes::PlannedStmt;
use types_nodes::primnodes::{ParamKind, SubLinkType};
use types_nodes::{Node, NodeTag};

/// The plan node ids of `stmt` (its plan tree and its subplans) PostgreSQL 18.6 may never run, sorted (see the module doc).
pub fn never_run<'m>(stmt: &PlannedStmt<'_>, m: Mcx<'m>) -> PgVec<'m, i32> {
    regions(stmt, m).of.iter().map(|x| x.0).collect_in(m)
}

/// D4191, the gate regions of a statement: a gate region is a subtree PostgreSQL 18.6 may skip whole, rooted at a gated
/// subplan's plan, a waiting join's inner (`inner_waits`) or a one-time-qual Result's child (the module doc's gates).
/// What one demand runs (a CTE's pull, an InitPlan's first read, a rescan, a build at its probe's first row) PostgreSQL
/// runs inside that demand's region; a node in a region nested in it may still never run.
pub struct Regions<'m> {
    /// Each node `never_run` names with the root of the innermost region holding it, sorted by node.
    pub of: PgVec<'m, (i32, i32)>,
    /// Each region's parent: the region holding its root, -1 for none (a subplan's own region: what runs a subplan is
    /// its reader; a CTE whose readers all sit in one region is in that region instead), sorted by region.
    pub parent: PgVec<'m, (i32, i32)>,
}

impl<'m> Regions<'m> {
    /// No regions, the lists in `m`.
    pub fn new_in(m: Mcx<'m>) -> Self {
        Regions { of: PgVec::new_in(m), parent: PgVec::new_in(m) }
    }

    /// The regions holding node `id`, innermost first; none when PostgreSQL runs it whenever its statement runs.
    pub fn chain(&self, id: i32) -> impl Iterator<Item = i32> + '_ {
        let up = |r: &i32| self.parent.binary_search_by_key(r, |x| x.0).ok().map(|i| self.parent[i].1).filter(|&p| p >= 0);
        let first = self.of.binary_search_by_key(&id, |x| x.0).ok().map(|i| self.of[i].1);
        core::iter::successors(first, up).take(self.parent.len() + 1)
    }
}

/// `stmt`'s gate regions (`Regions`).
pub fn regions<'m>(stmt: &PlannedStmt<'_>, m: Mcx<'m>) -> Regions<'m> {
    let mut r = Regions::new_in(m);
    // (plan id, its reader's region) of each InitPlan a one-time qual reads and each CTE a CteScan reads, met on the walk
    let mut at_start: PgVec<'_, (i32, i32)> = PgVec::new_in(m);
    if let Some(root) = stmt.planTree {
        walk(root, -1, &mut r, &mut at_start);
    }
    // a subplan's own InitPlans and CTEs come before it in `subplans` (the planner plans them first): walked last, they
    // find their readers; a CTE runs with its first reader that always runs, an InitPlan with its one-time qual's Result,
    // anything else on demand. A CTE or such an InitPlan whose readers all sit in one region runs whenever that region
    // runs, so it joins it (a MATERIALIZED CTE inside a SubPlan: the SubPlan's rescan runs it); readers in several
    // regions open its own
    for (id, sub) in stmt.subplans.iter().enumerate().filter_map(|(i, s)| s.map(|s| (i as i32 + 1, s))).rev() {
        let readers: PgVec<'_, i32> = at_start.iter().filter(|x| x.0 == id).map(|x| x.1).collect_in(m);
        let region = match readers.first() {
            // a data-modifying CTE's plan always runs (the module doc)
            _ if sub.node_tag() == NodeTag::T_ModifyTable => -1,
            Some(_) if readers.contains(&-1) => -1,
            Some(&g) if readers.iter().all(|&x| x == g) => g,
            _ => open(sub, -1, &mut r),
        };
        walk(sub, region, &mut r, &mut at_start);
    }
    r.of.sort_unstable();
    r.of.dedup_by_key(|x| x.0);
    r.parent.sort_unstable();
    r.parent.dedup_by_key(|x| x.0);
    r
}

/// The region rooted at plan node `n`, opened inside `region` (-1: none): its root's id, its parent recorded.
fn open(n: Node<'_>, region: i32, r: &mut Regions<'_>) -> i32 {
    let id = n.as_plan().map_or(-1, |p| p.plan_node_id);
    r.parent.push_in((id, region));
    id
}

/// `n` and its subtree in `region` (-1: a node PostgreSQL runs whenever its statement runs).
fn walk(n: Node<'_>, region: i32, r: &mut Regions<'_>, at_start: &mut PgVec<'_, (i32, i32)>) {
    crate::walk::stack_check(); // D3748
    let Some(p) = n.as_plan() else { return };
    if region >= 0 {
        r.of.push_in((p.plan_node_id, region));
    }
    if let Some(c) = n.as_cte_scan() {
        at_start.push_in((c.ctePlanId, region));
    }
    if let Some(q) = n.as_result().and_then(|r| r.resconstantqual) {
        for s in p.initPlan.iter().filter_map(|s| s.as_sub_plan()).filter(|s| s.subLinkType != SubLinkType::CTE_SUBLINK && reads_param(q, &s.setParam)) {
            at_start.push_in((s.plan_id, region));
        }
    }
    let (outer, inner) = (p.lefttree, p.righttree);
    let (once, waits) = (one_time(n), inner_waits(n));
    if let Some(o) = outer {
        let at = if once { open(o, region, r) } else { region };
        walk(o, at, r, at_start);
    }
    if let Some(i) = inner {
        let at = if once || waits { open(i, region, r) } else { region };
        walk(i, at, r, at_start);
    }
    // an Append's or MergeAppend's arms, a SubqueryScan's subplan, a BitmapAnd's or BitmapOr's inputs (`explain::children`
    // lists the two trees first)
    for (c, _) in crate::explain::children(n).skip(usize::from(outer.is_some()) + usize::from(inner.is_some())) {
        walk(c, region, r, at_start);
    }
}

/// Join `n` runs its inner (right) input only after its outer's first row (the module doc): a NestLoop, a MergeJoin that
/// does not fill its inner, a HashJoin on the outer-first path.
fn inner_waits(n: Node<'_>) -> bool {
    let Some(p) = n.as_plan() else { return false };
    match n.node_tag() {
        NodeTag::T_NestLoop => true,
        NodeTag::T_MergeJoin => {
            let jt = n.as_merge_join().map_or(JoinType::JOIN_INNER, |m| m.join.jointype);
            !matches!(jt, JoinType::JOIN_RIGHT | JoinType::JOIN_FULL | JoinType::JOIN_RIGHT_ANTI)
        }
        NodeTag::T_HashJoin => {
            let jt = n.as_hash_join().map_or(JoinType::JOIN_INNER, |h| h.join.jointype);
            let hash_total = p.righttree.and_then(|h| h.as_plan()).map_or(0.0, |h| h.total_cost);
            let outer_startup = p.lefttree.and_then(|o| o.as_plan()).map_or(0.0, |o| o.startup_cost);
            crate::join::outer_first(jt, p.parallel_aware, outer_startup, hash_total)
        }
        _ => false,
    }
}

/// `n` is a Result with a one-time qual (`resconstantqual`).
fn one_time(n: Node<'_>) -> bool {
    n.as_result().is_some_and(|r| r.resconstantqual.is_some())
}

/// The plan nodes `never_run` names for a Result's one-time qual alone: nodes of the statement's own tree with such a
/// Result above them and no join's waiting inner (`inner_waits`) between the root and them; each with those Results' node
/// ids, outermost first. Sorted by node id. PostgreSQL's ExecResult evaluates the qual at its first call and never pulls
/// its outer when it fails, so a pipeline that makes each of those checks before it pulls a row (its own gate step or
/// its feeder's check) starts where PostgreSQL's demand does: lowerv4's stamp leaves such a pipeline ungated. A subplan's
/// nodes are never listed (what runs a subplan is its reader).
pub fn result_gated<'m>(stmt: &PlannedStmt<'_>, m: Mcx<'m>) -> PgVec<'m, (i32, PgVec<'m, i32>)> {
    let (mut out, mut gates) = (PgVec::new_in(m), PgVec::new_in(m));
    if let Some(root) = stmt.planTree {
        walk_results(root, &mut gates, &mut out, m);
    }
    out.sort_unstable_by_key(|x| x.0);
    out
}

fn walk_results<'m>(n: Node<'_>, gates: &mut PgVec<'m, i32>, out: &mut PgVec<'m, (i32, PgVec<'m, i32>)>, m: Mcx<'m>) {
    crate::walk::stack_check();
    let Some(p) = n.as_plan() else { return };
    if !gates.is_empty() {
        out.push_in((p.plan_node_id, mcx::to_vec_in(gates, m)));
    }
    let own = one_time(n);
    if own {
        gates.push_in(p.plan_node_id);
    }
    let (outer, inner) = (p.lefttree, p.righttree);
    if let Some(o) = outer {
        walk_results(o, gates, out, m);
    }
    if let Some(i) = inner.filter(|_| !inner_waits(n)) {
        walk_results(i, gates, out, m);
    }
    for (c, _) in crate::explain::children(n).skip(usize::from(outer.is_some()) + usize::from(inner.is_some())) {
        walk_results(c, gates, out, m);
    }
    if own {
        gates.pop();
    }
}

/// Expression `n` reads one of exec parameters `ids`.
fn reads_param(n: Node<'_>, ids: &IntList<'_>) -> bool {
    crate::walk::stack_check(); // D3748
    if n.as_param().is_some_and(|p| p.paramkind == ParamKind::PARAM_EXEC && ids.iter().any(|i| i == p.paramid)) {
        return true;
    }
    let mut hit = false;
    crate::expr::kids(n, &mut |c| hit |= reads_param(c, ids));
    hit
}
