use core::fmt::{self, Write};

use gravity_abi::breaker::{BreakerKind, Budget, Spill};
use gravity_program::{ArenaBox, ArenaVec, Pipeline};
use types_nodes::list::{IntList, NodeList};
use types_nodes::plannodes::{CteScan, PlannedStmt, SubqueryScan};
use types_nodes::primnodes::{SubLinkType, SubPlan, OUTER_VAR};
use types_nodes::{Node, NodeTag};

use crate::expr;
use crate::graph::{InitPlan, Op, PipelineNode, Source};
use crate::ledger::Kind;
use crate::ops;
use crate::admit::Refusal;
use crate::ops::shared::sort::column_kind_of_expr;
use crate::ops::shared::sort::{keys_layout, ROW_LAYOUT};
use crate::walk::{Builder, Open, STMT_NODE};
use crate::ops::shared::subplan::{agg_kids, refers, walk_cond, walk_ret, SubForm, SubLink, SubPlanDesc, SubSite};
use crate::ops::shared::record_row::record_row;

pub const fn link_of(t: SubLinkType) -> SubLink {
    match t {
        SubLinkType::EXISTS_SUBLINK => SubLink::Exists,
        SubLinkType::ALL_SUBLINK => SubLink::All,
        SubLinkType::ANY_SUBLINK => SubLink::Any,
        SubLinkType::ROWCOMPARE_SUBLINK => SubLink::RowCompare,
        SubLinkType::EXPR_SUBLINK => SubLink::Expr,
        SubLinkType::MULTIEXPR_SUBLINK => SubLink::MultiExpr,
        SubLinkType::ARRAY_SUBLINK => SubLink::Array,
        SubLinkType::CTE_SUBLINK => SubLink::Cte,
    }
}

fn root_of<'p>(b: &Builder<'p, '_>, plan_id: i32, k: Kind, id: i32) -> Result<Node<'p>, Refusal> {
    let n = (plan_id as usize).checked_sub(1).filter(|&n| n < b.stmt.subplans.len()).ok_or(Refusal::shape(k, id, "a subplan id outside the statement's subplans"))?;
    b.stmt.subplans.nth(n).filter(|n| n.as_plan().is_some()).ok_or(Refusal::shape(k, id, "a subplan without a plan tree"))
}

fn first_param(l: &IntList<'_>) -> i32 {
    if l.is_nil() {
        -1
    } else {
        l.nth(0)
    }
}

pub fn map_stmt(b: &mut Builder<'_, '_>) -> Result<(), Refusal> {
    for n in b.stmt.subplans.iter().flatten() {
        if n.as_plan().is_none() {
            return Err(Refusal::shape(Kind::SubPlan, STMT_NODE, "a subplans entry that is not a plan node"));
        }
    }
    Ok(())
}

/// `Plan.initPlan`: a CTE's tree is mapped and spooled into a slot every `CteScan` reads; any other
/// InitPlan is a pipeline run once, its rows folded into the exec parameter `setParam` names.
pub fn map_init_plans<'p>(b: &mut Builder<'p, '_>, node: Node<'p>, k: Kind) -> Result<(), Refusal> {
    let plan = node.as_plan().unwrap();
    let id = plan.plan_node_id;
    for n in plan.initPlan.iter() {
        let s = n.as_sub_plan().ok_or(Refusal::shape(k, id, "an initPlan entry that is not a SubPlan"))?;
        if !s.parParam.is_nil() || !s.args.is_nil() {
            return Err(Refusal::shape(Kind::SubPlan, id, "an InitPlan with parameters"));
        }
        let link = link_of(s.subLinkType);
        let root = root_of(b, s.plan_id, Kind::SubPlan, id)?;
        match link {
            SubLink::Cte => {
                let cols = crate::facts::demand::cte::cte_cols(b, s.plan_id);
                b.graph.cte_reads.push(crate::facts::demand::cte::CteRead { plan_id: s.plan_id, cols });
                let open = b.plan(root)?;
                let open = crate::ops::shared::union::settle(&mut b.graph, open);
                let slot = match reprojects(b, &open) || read_on_demand(b, s.plan_id) {
                    true => crate::ops::shared::breaker::close(b, open, BreakerKind::Materialize, ROW_LAYOUT, Budget::Share, Spill::Runs, Kind::CteScan, id)?,
                    false => crate::ops::shared::breaker::spool(b, open, Kind::CteScan, id)?,
                };
                let producer = b.graph.breakers.iter().rev().find(|d| d.breaker.slot == slot).map_or(0, |d| d.producer);
                b.ctes.push((s.plan_id, slot));
                let f = crate::rerun::of_subplan(b.stmt, b.facts, b.arena, s);
                let i = InitPlan { plan_id: s.plan_id, pipeline: producer, link, param: first_param(&s.setParam), params: &s.setParam, slot: Some(slot), volatility: f.volatility, raise_free: f.raise_free, parallel_safe: f.parallel == crate::effects::Parallel::Safe, attach: id };
                // D1934: the CTE whose CteScan is the statement's first fetch, with the exact licence of its plan's run
                // from there (`Proof::first_fetch`; denied for a plan that acts)
                if let Some(l) = crate::effects::Proof::first_fetch(b.stmt, s.plan_id).and_then(|p| crate::effects::exact(crate::effects::Effects::of_init_plan(&i), p).ok()) {
                    b.graph.first_cte = Some((s.plan_id, l));
                }
                b.graph.init_plans.push(i);
            }
            _ => {
                let open = b.plan(root)?;
                let open = crate::ops::shared::union::settle(&mut b.graph, open); // an InitPlan is one pipeline
                if b.graph.pipelines.len() + 2 > gravity_program::limits::PIPELINES {
                    return Err(Refusal::shape(Kind::SubPlan, id, "more than 65535 pipelines (the u16 pipeline id)"));
                }
                let p = b.graph.pipelines.len() as u16;
                b.graph.push(open);
                let f = crate::rerun::of_subplan(b.stmt, b.facts, b.arena, s);
                b.graph.init_plans.push(InitPlan { plan_id: s.plan_id, pipeline: p, link, param: first_param(&s.setParam), params: &s.setParam, slot: None, volatility: f.volatility, raise_free: f.raise_free, parallel_safe: f.parallel == crate::effects::Parallel::Safe, attach: id });
                // PS1-05: its planner charge (graph `InitCharge::of`, a front hit's too)
                if let Some(c) = crate::graph::InitCharge::of(s).cost {
                    b.graph.init_costs.push((s.plan_id, c));
                }
                let site = recheck_site(b.stmt);
                b.graph.recheck_node = site.as_ref().map_or(-1, |r| r.id);
                if site.as_ref().is_some_and(|r| r.above(id)) {
                    b.graph.recheck_above.push(s.plan_id);
                }
                if recheck_reads(site.as_ref(), id, &s.setParam) {
                    b.graph.recheck_inits.push(s.plan_id);
                }
                if recheck_fresh(site.as_ref(), id) {
                    b.graph.epq_fresh.push(s.plan_id);
                }
            }
        }
        b.closed_by(root);
        b.graph.note(Kind::SubPlan);
    }
    Ok(())
}

/// D3271: where the statement's recheck is, the node `recheck_inits` and `epq_fresh` are measured from: the first
/// ModifyTable / LockRows down the top plan's outer chain; else, in a statement that writes nothing, its one LockRows
/// wherever it is (a FROM subquery's under a SubqueryScan, a CTE's, a SubPlan's: lockrows-d2112-initplan s2p / s2ctp),
/// which has the same EvalPlanQual. `root`: the tree it is in; `sub`: that tree is a subplan's. Nothing here allocates
/// (decompose runs allocation-free after warm-up: core_zero_alloc).
struct Recheck<'p> {
    id: i32,
    node: Node<'p>,
    root: Node<'p>,
    sub: bool,
    main: Option<Node<'p>>,
}

impl Recheck<'_> {
    /// Plan node `id` is the recheck's node or above it in its tree.
    fn above(&self, id: i32) -> bool {
        crate::rerun::path_has(self.root, self.id, id) == Some(true)
    }
}

fn recheck_site<'p>(stmt: &'p PlannedStmt<'p>) -> Option<Recheck<'p>> {
    let mut n = stmt.planTree;
    while let Some(x) = n {
        let p = x.as_plan()?;
        if matches!(x.node_tag(), NodeTag::T_ModifyTable | NodeTag::T_LockRows) {
            return Some(Recheck { id: p.plan_node_id, node: x, root: stmt.planTree?, sub: false, main: None });
        }
        n = p.lefttree;
    }
    let (mut locks, mut writes) = (0usize, 0usize);
    crate::rerun::each_plan_node(stmt, &mut |x| match x.node_tag() {
        NodeTag::T_LockRows => locks += 1,
        NodeTag::T_ModifyTable => writes += 1,
        _ => {}
    });
    if writes > 0 || locks != 1 {
        return None;
    }
    let found = |root: Node<'p>, sub: bool| crate::rerun::find_lock(root).and_then(|x| x.as_plan().map(|p| Recheck { id: p.plan_node_id, node: x, root, sub, main: stmt.planTree }));
    stmt.planTree.and_then(|t| found(t, false)).or_else(|| stmt.subplans.iter().flatten().find_map(|r| found(r, true)))
}

/// D2028 / D2112 phase B: whether an InitPlan attached at plan node `id` with setParams `set` is one PostgreSQL forces at
/// the statement's recheck (`recheck_site`): `id` at the recheck's node or above it in its tree, and one of `set` in that
/// node's subplan `extParam` (EvalPlanQualStart / EvalPlanQualBegin run ExecSetParamPlanMulti over the subplan's extParam).
fn recheck_reads(r: Option<&Recheck<'_>>, id: i32, set: &IntList<'_>) -> bool {
    let Some(r) = r else { return false };
    let sub = r.node.as_plan().and_then(|p| p.lefttree).and_then(|c| c.as_plan());
    r.above(id) && sub.is_some_and(|s| set.iter().any(|v| s.extParam.is_member(v.to_owned())))
}

/// D2673: whether an InitPlan attached at plan node `id` is one PostgreSQL's EvalPlanQualStart arms afresh in the recheck's
/// EState (`recheck_site`). Every subplan's tree is initialized again there (ExecInitSubPlan arms each InitPlan attached in
/// it), and the recheck's own plan below its node: so any InitPlan attached outside the statement's own plan tree (in a
/// SubPlan's or a CTE's tree, the recheck's own node and those above it too when the lock is in one, D3271), or lower in
/// the statement's tree than the recheck's node, runs again where the recheck reads it. One at the node or above it in the
/// statement's tree only has its value copied (`recheck_reads`).
fn recheck_fresh(r: Option<&Recheck<'_>>, id: i32) -> bool {
    r.is_some_and(|r| if r.sub { !r.main.is_some_and(|m| crate::rerun::holds_node(m, id)) } else { !r.above(id) })
}

/// D1497 (CT-2, V4R-474): whether `sort::spool` would read the CTE's `open` (a bare emit over a breaker: one Project, one
/// consumed slot) as that breaker's slot while the projection is not the breaker's row itself. The shortcut drops the
/// projection, so the CTE's readers would read the breaker's row by the CTE's column numbers (an Agg's group row: an
/// Aggref's column read as a key's). Only a projection no wider than the row whose every kept entry is the row's own column
/// at its own place (a bare OUTER Var, attno == resno, the column's type, typmod and collation) is dropped; any other closes
/// the CTE with a Materialize (nest.cte's store).
fn reprojects(b: &Builder<'_, '_>, open: &Open<'_>) -> bool {
    let (Source::Breaker { slot }, [Op::Project(tl)]) = (&open.source, &open.ops[..]) else { return false };
    if open.desc.nconsumed() != 1 {
        return false;
    }
    let row = crate::breakers::decl_of(&b.graph, *slot).and_then(|d| b.graph.pipelines[d.producer as usize].ops.iter().rev().find_map(|op| if let Op::Project(l) = op { Some(*l) } else { None }));
    let Some(row) = row else { return true };
    tl.len() > row.len() || !tl.iter().all(|e| {
        let Some(te) = e.as_target_entry() else { return false };
        if te.resjunk {
            return true;
        }
        let (Some(v), Some(i)) = (te.expr.as_var(), (te.resno as usize).checked_sub(1).filter(|&i| i < row.len())) else { return false };
        let Some(c) = row.nth(i).as_target_entry() else { return false };
        v.varno == OUTER_VAR && v.varlevelsup == 0 && v.varattno == te.resno && crate::catalog::expr_type::type_of(c.expr) == Some(v.vartype) && crate::catalog::expr_type::typmod_of(c.expr) == v.vartypmod && crate::catalog::expr_type::collation_of(c.expr) == v.varcollid
    })
}

/// D3373 (D3095's sorted form): CTE `plan_id` is read inside an InitPlan that is no CTE (a scalar subquery over it, run
/// on demand) or (D3746, pganalyze's collector: a NOT IN over a CTE that calls pg_lock_status(), then UNION ALL its
/// rows) inside a hashed SubPlan's plan, and by at least one more CteScan (outside it, or in another InitPlan). Its
/// store is then its own Materialize (nest.cte's store: the CTE object, pulled or its producers run ahead), never
/// `spool`'s shortcut onto the root breaker's slot (a CTE's ORDER BY Sort run): over the shortcut no CTE object exists,
/// the CTE's producers are plain eager pipelines both readers read, and the InitPlan's on-demand members would take
/// them in (lowerv4 frame/program.rs refuses "an InitPlan whose producer a pipeline outside it also reads"). Two
/// CteScans in one InitPlan's plan take it too (the Materialize is never wrong).
/// D1236 (Hibernate's DefaultMutation: `dml_cte_Person AS (SELECT e.id, row_number() OVER () FROM dml_cte_base_Person e
/// ORDER BY e.id)`, read by `HTE_Engineer`'s join and by the statement's `count(*)`): the same for a CTE read inside
/// another CTE's plan, a pulled set whose members would take in the shortcut's producers, and by a CteScan outside that
/// plan (the statement's tree, an InitPlan's or SubPlan's, or a third CTE's plan); lowerv4 refused "a CTE whose producer
/// a pipeline outside it also reads". CteScans all inside one other CTE's plan (a CTE that one other CTE's plan joins to
/// itself) keep the shortcut: that set's members take the producers and nothing outside reads them.
fn read_on_demand(b: &Builder<'_, '_>, plan_id: i32) -> bool {
    let stmt = b.stmt;
    let scan = |x: Node<'_>| usize::from(x.node_tag() == NodeTag::T_CteScan && x.as_cte_scan().is_some_and(|c| c.ctePlanId == plan_id));
    // walked in place (decompose allocates nothing once warm, core_zero_alloc): each InitPlan is listed by one node only
    let (mut inside, mut all, mut ctes, mut in_ctes) = (0, 0, 0, 0);
    crate::rerun::each_plan_node(stmt, &mut |n| {
        all += scan(n);
        let Some(p) = n.as_plan() else { return };
        for s in p.initPlan.iter().filter_map(|x| x.as_sub_plan()).filter(|s| link_of(s.subLinkType) != SubLink::Cte) {
            if let Some(r) = crate::rerun::subplan_root(stmt, s.plan_id) {
                crate::rerun::each_node(r, &mut |x| inside += scan(x));
            }
        }
        // D1236: each other CTE whose plan reads it, and how many of its CteScans are in those plans
        for s in p.initPlan.iter().filter_map(|x| x.as_sub_plan()).filter(|s| link_of(s.subLinkType) == SubLink::Cte && s.plan_id != plan_id) {
            if let Some(r) = crate::rerun::subplan_root(stmt, s.plan_id) {
                let mut here = 0;
                crate::rerun::each_node(r, &mut |x| here += scan(x));
                ctes += usize::from(here > 0);
                in_ctes += here;
            }
        }
        // D3746: a hashed SubPlan's plan in the node's quals or target list, whose set lowerv4 may build LAZY, at its first
        // probe (on demand, as PG's ExecHashSubPlan builds it at the first call: ops/subplan/hashed.rs `lazy_build`)
        let mut hashed = |s: &SubPlan<'_>| {
            if let Some(r) = crate::rerun::subplan_root(stmt, s.plan_id).filter(|_| s.useHashTable) {
                crate::rerun::each_node(r, &mut |x| inside += scan(x));
            }
        };
        for e in p.qual.iter().chain(p.targetlist.iter()) {
            each_sub(e, &mut hashed);
        }
    });
    (inside > 0 && all > 1) || ctes > 1 || (ctes == 1 && all > in_ctes)
}

/// Each SubPlan in expression tree `n` (`read_on_demand`'s hashed sets).
fn each_sub<'p>(n: Node<'p>, f: &mut dyn FnMut(&'p SubPlan<'p>)) {
    crate::walk::stack_check();
    if let Some(s) = n.as_sub_plan() {
        f(s);
    }
    expr::kids(n, &mut |c| each_sub(c, f));
}

fn map_subquery_scan<'p>(b: &mut Builder<'p, '_>, sq: &'p SubqueryScan<'p>) -> Result<Open<'p>, Refusal> {
    let (plan, k) = (&sq.scan.plan, Kind::SubqueryScan);
    let id = plan.plan_node_id;
    let root = sq.subplan.ok_or(Refusal::shape(k, id, "a SubqueryScan without a subplan"))?;
    record_row(b, &sq.scan, root);
    let mut open = b.plan(root)?;
    ops::filters(&mut open, &plan.qual);
    ops::project(&mut open, &plan.targetlist);
    b.graph.note(k);
    Ok(open)
}

fn map_cte_scan<'p>(b: &mut Builder<'p, '_>, cs: &'p CteScan<'p>) -> Result<Open<'p>, Refusal> {
    let (plan, k) = (&cs.scan.plan, Kind::CteScan);
    let id = plan.plan_node_id;
    let slot = b.ctes.iter().rev().find(|(p, _)| *p == cs.ctePlanId).map(|(_, s)| *s).ok_or(Refusal::shape(k, id, "a CteScan before its CTE's InitPlan"))?;
    if let Ok(root) = root_of(b, cs.ctePlanId, k, id) {
        record_row(b, &cs.scan, root);
    }
    let mut open = crate::ops::shared::breaker::emit(b, id, slot);
    ops::filters(&mut open, &plan.qual);
    ops::project(&mut open, &plan.targetlist);
    b.graph.note(k);
    Ok(open)
}

pub fn map<'p>(b: &mut Builder<'p, '_>, node: Node<'p>, k: Kind) -> Result<Open<'p>, Refusal> {
    match k {
        Kind::SubqueryScan => map_subquery_scan(b, node.as_subquery_scan().unwrap()),
        Kind::CteScan => map_cte_scan(b, node.as_cte_scan().unwrap()),
        _ => Err(Refusal::shape(k, STMT_NODE, "a SubPlan is an expression node, not a plan node")),
    }
}

/// The planner's cost choice among equivalent subplans (`fix_alternative_subplan`'s rule over the
/// node's row estimate), made here once, never at run time.
fn choose<'p>(alts: &NodeList<'p>, rows: f64) -> Option<&'p SubPlan<'p>> {
    let mut best: Option<(&'p SubPlan<'p>, f64)> = None;
    for a in alts.iter() {
        let s = a.as_sub_plan()?;
        let cost = s.startup_cost + s.per_call_cost * rows.max(1.0);
        if best.is_none_or(|(_, c)| cost < c) {
            best = Some((s, cost));
        }
    }
    best.map(|(s, _)| s)
}

/// Whether op `o` reads the SubPlan (`r`: `refers`, or `evaluates` for the check below): a filter, a projection, a
/// join's quals, a hash join's build or probe keys (`attach_keys`), a window's functions, a write's lists, a ProjectSet's
/// target list (its plain entries run per output row and in the call that finds every set ended, before the projection:
/// the SubPlan op sits before the Srf op, M8-SUB 09-27), a FROM function's arguments (evaluated once per scan, before its
/// first row).
/// A window op reads its functions' D1415 step arguments only (window.rs `step_arg`; a SubPlan of another argument or
/// FILTER is the partition buffer's fill's, `attach_window_args`, or refuses in lowerv4): a SubPlan elsewhere in the
/// WindowAgg's target list is PG's projection's, after the functions, so its op sits before the projection or qual that
/// reads it (D2252).
fn reads(o: &Op<'_>, plan_id: i32, r: fn(Node<'_>, i32) -> bool) -> bool {
    let list = |l: &NodeList<'_>| l.iter().any(|n| r(n, plan_id));
    match o {
        Op::Filter(n) | Op::Gate(n) => r(*n, plan_id),
        Op::Project(l) => list(l),
        // D2013 slice 2: a nest's NestLoopParam holding a SubPlan hoisted out of its inner's keys (facts/hoist.rs)
        Op::Join(j) => j.qual().is_some_and(list) || j.keys().is_some_and(list) || matches!(j, crate::join::JoinDesc::Nest { params, .. } if params.iter().any(|p| p.as_nest_loop_param().is_some_and(|n| r(n.paramval, plan_id)))),
        Op::Window(w) => w.functions.iter().any(|f| f.func.args.iter().enumerate().any(|(a, n)| crate::ops::window_step_arg(f.func, a) && r(n, plan_id))),
        Op::Write(d) => crate::ops::shared::modifytable::refers(d, plan_id),
        Op::Srf(s) if s.place == crate::ops::shared::srf::SrfPlace::TargetList => list(s.tlist),
        Op::Srf(s) => s.functions.iter().any(|f| r(f.node, plan_id)),
        _ => false,
    }
}

/// The first op of a pipeline that reads the SubPlan (`reads`); D4058 `own_reader`: the open node's own first, from its mark.
fn reader(ops: &[Op<'_>], plan_id: i32) -> Option<usize> {
    ops.iter().position(|o| reads(o, plan_id, refers))
}

fn own_reader(open: &Open<'_>, plan_id: i32) -> Option<usize> {
    let m = (open.mark as usize).min(open.ops.len());
    reader(&open.ops[m..], plan_id).map(|i| i + m).or_else(|| reader(&open.ops, plan_id))
}

/// `refers` outside aggregate calls, for `unplaced`: an Aggref's arguments (a SubPlan among them) run on the aggregate's
/// input side, where `attach` places that SubPlan's op (the agg's input pipeline); the emit side's projection holds the
/// Aggref node but reads only the aggregate's result, so a SubPlan under it is no read there.
fn evaluates(n: Node<'_>, plan_id: i32) -> bool {
    crate::walk::stack_check(); // D3748
    if let Some(l) = n.as_list() {
        return l.iter().any(|c| evaluates(c, plan_id));
    }
    let mut found = false;
    walk_ret(n, &mut |x| {
        if let Some(s) = x.as_sub_plan() {
            found |= s.plan_id == plan_id;
        }
        !found && x.node_tag() != NodeTag::T_Aggref
    });
    found
}

/// `expr::has_subplan` through `walk_ret`.
fn has_subplan_ret(node: Node<'_>) -> bool {
    let mut found = false;
    walk_ret(node, &mut |n| {
        found |= matches!(n.node_tag(), NodeTag::T_SubPlan | NodeTag::T_AlternativeSubPlan);
        !found
    });
    found
}

/// D1047 (interim): every pipeline (an embedded inner too) that reads a placed SubPlan holds its op before
/// the read. place() puts one op in one pipeline, so a SubPlan whose readers sit in several pipelines leaves the others
/// reading a result no step of their row made (a grouping set's arm places its own: `attach_lists`): a hashed SubPlan in a right join's probe qual (cte-049: place() puts a hashed op only
/// in the open pipeline, and its Distinct build comes after the probe's pipeline). That refuses by name until M8-SUB serves
/// the gap by the M8 exit: an op before every reader (each nested one its own inner), or a lazy SubHash built at the
/// first probe (V4R-369's form).
pub fn unplaced(g: &crate::graph::PipelineGraph<'_>) -> Option<Refusal> {
    fn ids(p: &PipelineNode<'_>, out: &mut ArenaVec<i32>) {
        crate::walk::stack_check(); // D3748
        for op in p.ops.iter() {
            match op {
                Op::SubPlan(d) => {
                    out.push(d.plan_id);
                    if let SubForm::Nested { inner } = &d.form {
                        ids(inner, out);
                    }
                }
                Op::Join(crate::join::JoinDesc::Nest { inner, .. }) => ids(inner, out),
                _ => {}
            }
        }
    }
    fn check(p: &PipelineNode<'_>, placed: &[i32]) -> bool {
        crate::walk::stack_check(); // D3748
        let before = |id: i32, r: usize| p.ops[..r].iter().any(|o| matches!(o, Op::SubPlan(d) if d.plan_id == id));
        placed.iter().all(|&id| p.ops.iter().position(|o| reads(o, id, evaluates)).is_none_or(|r| before(id, r)))
            && p.ops.iter().all(|op| match op {
                Op::SubPlan(SubPlanDesc { form: SubForm::Nested { inner }, .. }) | Op::Join(crate::join::JoinDesc::Nest { inner, .. }) => check(inner, placed),
                _ => true,
            })
    }
    let mut placed: ArenaVec<i32> = ArenaVec::new_in(*g.pipelines.allocator()); // alloc: Cold
    for p in g.pipelines.iter() {
        ids(p, &mut placed);
    }
    if placed.is_empty() || g.pipelines.iter().all(|p| check(p, &placed)) {
        return None;
    }
    Some(Refusal::shape(Kind::SubPlan, STMT_NODE, "a SubPlan read in a pipeline with no op of it before the read"))
}

/// `op` into the pipeline that reads its result, before its first reader: `open`, else (a nested one) the latest closed
/// pipeline that does (a right-driven hash join's probe the node already closed). That
/// pipeline also reads the slots a nested inner reads (`inner`, its descriptor: `breakers::embed`); a closed one only
/// lives filled before it (the program runs pipelines in order), else the statement refuses.
fn place<'p>(b: &mut Builder<'p, '_>, open: &mut Open<'p>, plan_id: i32, op: Op<'p>, inner: Option<Pipeline>) {
    let arena = b.arena;
    let refused = |later: bool| Refusal::shape(Kind::SubPlan, crate::walk::STMT_NODE, if later { "a nested SubPlan's inner reads a slot filled after its reader's pipeline" } else { "a nested SubPlan's inner reads a slot its reader reads from another life" });
    let into_open = |b: &mut Builder<'p, '_>, open: &mut Open<'p>| match inner.map(|d| crate::breakers::embed(arena, open.desc, &d, Some(&mut b.graph.reads))) {
        Some(Err(_)) => {
            b.graph.fault.get_or_insert(refused(false));
        }
        Some(Ok(d)) => open.desc = d,
        None => {}
    };
    if let Some(i) = own_reader(open, plan_id) {
        into_open(b, open);
        return open.insert(i, op);
    }
    let closed = if inner.is_some() { b.graph.pipelines.iter_mut().enumerate().rev().find_map(|(c, p)| reader(&p.ops, plan_id).map(|i| (c, p, i))) } else { None };
    let fault = match closed {
        // one arm of a streamed union cannot take the inner for all of them
        Some((_, p, i)) if p.ops[..i].iter().any(|o| matches!(o, Op::Append(crate::ops::shared::append::AppendDesc::Arm { .. }))) => {
            Some(Refusal::shape(Kind::SubPlan, crate::walk::STMT_NODE, "a nested SubPlan read past a streamed union's arm marker"))
        }
        Some((c, p, i)) => {
            // a join reader evaluates its qual's SubPlan over the pair it hands the qual (lowerv4 `site::Lend::Reader`),
            // after that pipeline filled the pair's slots (a right join's build row from its hash entry)
            let at_join = matches!(p.ops[i], Op::Join(_));
            p.insert(i, op);
            let d = inner.as_ref().map_or(&[][..], |d| d.consumes);
            let lives = inner.as_ref().map_or(&[][..], |d| d.consumes_from);
            // a slot the SubPlan's own rerun set fills is filled on demand, when the op runs the set (`rerun::declare_rerun`)
            let own = |l: u16| b.graph.reruns.iter().any(|r| r.plan_id == plan_id && r.pipelines.contains(&l));
            let before = lives.len() == d.len() && lives.iter().all(|l| (l.0 as usize) < c || (at_join && l.0 as usize == c) || own(l.0));
            match inner.map(|x| crate::breakers::embed(arena, p.desc, &x, None)) {
                Some(Ok(e)) if before => {
                    p.desc = e;
                    None
                }
                Some(Ok(_)) => Some(refused(true)),
                Some(Err(_)) => Some(refused(false)),
                None => None,
            }
        }
        None => {
            into_open(b, open);
            open.ops.push(op);
            None
        }
    };
    if let Some(f) = fault {
        b.graph.fault.get_or_insert(f);
    }
}

/// D1260: the SubPlan node its first reader (`own_reader`) holds (a Filter, Gate or projection), where the node's lists were
/// rewritten before the reader was built: an Agg's GROUPING() folded to its constant inside the SubPlan's arguments
/// (agg.rs `arm_lists`: PostgreSQL evaluates an outer-level GROUPING() at the Agg and passes it as the SubPlan's
/// parameter), so the op binds the arguments the reader reads, not the plan's.
fn read_as<'p>(open: &Open<'p>, plan_id: i32) -> Option<&'p SubPlan<'p>> {
    let mut hit: Option<&'p SubPlan<'p>> = None;
    let mut see = |n: Node<'p>| {
        expr::walk(n, &mut |x| {
            if let Some(s) = x.as_sub_plan().filter(|s| s.plan_id == plan_id) {
                hit.get_or_insert(s);
            }
            hit.is_none()
        });
    };
    match open.ops.get(own_reader(open, plan_id)?)? {
        Op::Filter(n) | Op::Gate(n) => see(*n),
        Op::Project(l) => l.iter().for_each(see),
        _ => {}
    }
    hit
}

/// D1920: PG keeps one PlanState per plan_id for every node that calls it (ExecInitSubPlan), so a store it keeps across a
/// rescan is filled once per statement; `map_subplan` maps the plan once per calling node, except a plan whose root is
/// that store (`one_store`), mapped once as the statement's store (`store_reader`).
const SHARED_STATE_KEPT_VOLATILE: &str = "a SubPlan called by several plan nodes whose plan keeps rows a VOLATILE call made across a rescan (PG shares one PlanState per plan_id; D1920)";
/// D1920: a TABLESAMPLE without REPEATABLE draws its seed once per PlanState and keeps it over every rescan, so every
/// calling node reads one sample; `map_subplan`'s copies of a heap BERNOULLI / SYSTEM sample share its plan node's seed
/// (`draws`), another method's or a pgrc relation's draw one each.
const SHARED_STATE_KEPT_SEED: &str = "a SubPlan called by several plan nodes whose plan keeps a TABLESAMPLE seed across a rescan (no REPEATABLE; PG shares one PlanState per plan_id; D1920)";

/// The lists of node `n` itself that `attach` / `attach_values` map SubPlans from.
pub(crate) fn own_lists<'p>(n: Node<'p>, f: &mut dyn FnMut(Node<'p>)) {
    let Some(p) = n.as_plan() else { return };
    p.targetlist.iter().chain(p.qual.iter()).for_each(&mut *f);
    n.as_result().and_then(|r| r.resconstantqual).into_iter().for_each(&mut *f);
    let jq = n.as_nest_loop().map(|j| &j.join.joinqual).or_else(|| n.as_merge_join().map(|j| &j.join.joinqual)).or_else(|| n.as_hash_join().map(|j| &j.join.joinqual));
    jq.into_iter().flat_map(|q| q.iter()).for_each(&mut *f);
    if let Some(v) = n.as_values_scan() {
        v.values_lists.iter().filter_map(|l| l.as_list()).flat_map(|l| l.iter()).for_each(&mut *f);
    }
    if let Some(mt) = n.as_modify_table() {
        crate::ops::shared::modifytable::expr_lists(mt, &mut |l| l.iter().for_each(&mut *f));
    }
}

/// D2115 (`SubPlanDesc::stops`): a Limit node of the statement (its subplans' too) whose count is neither absent nor a
/// constant of at least one: PG's ExecLimit never pulls its subplan at a zero count, and a parameter's may be zero.
fn stops(b: &Builder<'_, '_>) -> bool {
    use crate::graph::Bound;
    let mut hit = false;
    crate::rerun::each_plan_node(b.stmt, &mut |x| {
        hit |= x.as_limit().is_some_and(|l| !matches!(crate::ops::limit_bounds(l).0, Bound::All) && !matches!(crate::ops::limit_bounds(l).0, Bound::Const(c) if c > 0));
    });
    hit
}

/// D1920 serve: a hashed SubPlan of no parameters that several plan nodes call keeps its rows in a store a rescan keeps
/// (`rerun::rescan_keeps`) or draws a sample's seed (`draws`); PostgreSQL builds each caller's table from the one PlanState
/// (buildSubPlanHash: ExecReScan, then the plan's rows), so the store is filled once and every later build reads it again.
/// Where that rescan makes the first run's rows and calls no VOLATILE function again (`rerun::of_rescan`: what runs again
/// outside the kept stores, a BERNOULLI / SYSTEM seed kept), every caller's table holds the first build's rows: the set is
/// built once for the statement and each calling node probes it (`PipelineGraph::shared_sets`).
fn one_set<'p>(b: &Builder<'p, '_>, s: &'p SubPlan<'p>, root: Node<'p>) -> bool {
    s.useHashTable && s.parParam.is_nil() && crate::effects::Effects::of_found(crate::rerun::of_rescan(b.stmt, b.facts, b.arena, root, &[])).same_rows
}

/// D1920: more than one plan node of the statement calls SubPlan `plan_id`.
fn shared(b: &Builder<'_, '_>, plan_id: i32) -> bool {
    let mut n = 0;
    crate::rerun::each_plan_node(b.stmt, &mut |x| {
        let mut hit = false;
        own_lists(x, &mut |e| hit |= refers(e, plan_id));
        n += usize::from(hit);
    });
    n > 1
}

/// D1920's VOLATILE half: SubPlan `s`'s plan root is itself the store PG's rescan keeps whole (the lifetime home's
/// `root_store`: a Material or a Sort under REWIND, a FunctionScan, nothing below it run again), and the root's own lists act
/// on nothing (PG evaluates a FunctionScan's qual and target list at every read of its tuplestore; the store holds their
/// values once).
fn one_store<'p>(b: &Builder<'p, '_>, s: &'p SubPlan<'p>, root: Node<'p>) -> bool {
    if b.alloc.is_none() || !crate::lifetime::root_store(b.stmt, s, root) {
        return false;
    }
    let cx = crate::effects::Cx::new(b.stmt, b.facts, b.arena);
    let mut acts = false;
    own_lists(root, &mut |e| acts |= crate::effects::of_expr(&cx, e, &crate::effects::PLAIN).acts);
    !acts
}

/// D1920's VOLATILE half: a reader of SubPlan `s`'s root store (`one_store`). PG keeps one PlanState per plan_id
/// for every calling node, so the store is filled once for the statement, lazily, by whichever call reads past its rows.
/// The first calling node maps it as a CTE's store (an InitPlan of link `Cte`, lowerv4 nest.cte: one store, each reader's
/// read pointer reset at every re-open, the plan pulled a row at a time when it calls VOLATILE code): its fill is the
/// root's child for a Material (the store is the Material's), the root itself for a Sort or a FunctionScan. Every calling
/// node's inner reads that store and projects the stored row as the SubPlan's tuple.
fn store_reader<'p>(b: &mut Builder<'p, '_>, s: &'p SubPlan<'p>, root: Node<'p>, id: i32) -> Result<Open<'p>, Refusal> {
    let refuse = || Refusal::shape(Kind::SubPlan, id, SHARED_STATE_KEPT_VOLATILE);
    let plan = root.as_plan().ok_or_else(refuse)?;
    let cols = crate::self_spool::identity(b, &plan.targetlist).ok_or_else(refuse)?;
    let found = b.ctes.iter().rev().find(|&&(p, _)| p == s.plan_id).map(|&(_, slot)| slot);
    let slot = match found {
        Some(slot) => slot,
        None => {
            let fill = match root.as_material() {
                Some(m) => m.plan.lefttree.ok_or_else(refuse)?,
                None => root,
            };
            let open = b.plan(fill)?;
            let open = crate::ops::shared::union::settle(&mut b.graph, open);
            let slot = crate::ops::shared::breaker::close(b, open, BreakerKind::Materialize, ROW_LAYOUT, Budget::Share, Spill::Runs, Kind::SubPlan, id)?;
            let producer = b.graph.breakers.iter().rev().find(|d| d.breaker.slot == slot).map_or(0, |d| d.producer);
            b.closed_by(root);
            b.ctes.push((s.plan_id, slot));
            let f = crate::rerun::of_subplan(b.stmt, b.facts, b.arena, s);
            b.graph.init_plans.push(InitPlan { plan_id: s.plan_id, pipeline: producer, link: SubLink::Cte, param: first_param(&s.setParam), params: &s.setParam, slot: Some(slot), volatility: f.volatility, raise_free: f.raise_free, parallel_safe: f.parallel == crate::effects::Parallel::Safe, attach: id });
            slot
        }
    };
    let mut open = crate::ops::shared::breaker::emit(b, plan.plan_node_id, slot);
    ops::project(&mut open, cols);
    Ok(open)
}

/// D1920: plan subtree `n`, a CTE declared in it or a SubPlan it calls scans a TABLESAMPLE without REPEATABLE. A non-CTE
/// InitPlan is one parameter cell for every copy, one sample.
fn draws<'p>(b: &Builder<'p, '_>, n: Node<'p>) -> bool {
    crate::walk::stack_check(); // D3748
    let Some(p) = n.as_plan() else { return true };
    let sub = |s: &'p SubPlan<'p>| crate::rerun::subplan_root(b.stmt, s.plan_id).is_none_or(|r| draws(b, r));
    // D1920: a heap BERNOULLI / SYSTEM sample's copies share its plan node's one seed (hosts HostRegistry::sample_seed), as
    // PostgreSQL's one PlanState does; another method's or a pgrc relation's source draws its own
    let seeded = |s: &types_nodes::plannodes::SampleScan<'_>, t: &types_nodes::parsenodes::TableSampleClause<'_>| {
        matches!(t.tsmhandler, crate::ops::shared::scan::TSM_BERNOULLI | crate::ops::shared::scan::TSM_SYSTEM)
            && b.relation(s.scan.scanrelid, Kind::SampleScan, p.plan_node_id).is_ok_and(|r| r.am == crate::catalog::Am::Heap)
    };
    let mut hit = n.as_sample_scan().is_some_and(|s| s.tablesample.and_then(|t| t.as_table_sample_clause()).is_none_or(|t| t.repeatable.is_none() && !seeded(s, t)));
    hit |= p.initPlan.iter().filter_map(|s| s.as_sub_plan()).any(|s| s.subLinkType == SubLinkType::CTE_SUBLINK && sub(s));
    own_lists(n, &mut |e| {
        expr::walk(e, &mut |x| {
            hit |= x.as_sub_plan().is_some_and(sub);
            !hit
        });
    });
    let lists = [n.as_append().map(|a| &a.appendplans), n.as_merge_append().map(|a| &a.mergeplans), n.as_bitmap_and().map(|a| &a.bitmapplans), n.as_bitmap_or().map(|a| &a.bitmapplans), n.as_agg().map(|a| &a.chain)];
    let kids = p.lefttree.into_iter().chain(p.righttree).chain(n.as_subquery_scan().and_then(|s| s.subplan));
    hit || kids.chain(lists.into_iter().flatten().flat_map(|l| l.iter())).any(|k| draws(b, k))
}

fn map_subplan<'p>(b: &mut Builder<'p, '_>, f: Found1<'p>, k: Kind, id: i32, open: Open<'p>) -> Result<Open<'p>, Refusal> {
    map_subplan_held(b, f, k, id, open, None)
}

/// `map_subplan` for a SubPlan its place evaluates ahead of PostgreSQL's demand under licence `ahead`, carried on each of
/// its ops (`SubPlanDesc::ahead`).
fn map_subplan_held<'p>(b: &mut Builder<'p, '_>, f: Found1<'p>, k: Kind, id: i32, mut open: Open<'p>, ahead: Option<crate::effects::Licence>) -> Result<Open<'p>, Refusal> {
    let Found1 { s, alternative, site, conditional } = f;
    let s = if f.alternative { s } else { read_as(&open, s.plan_id).unwrap_or(s) };
    let link = link_of(s.subLinkType);
    if link == SubLink::Cte || (link != SubLink::MultiExpr && !s.setParam.is_nil()) {
        return Err(Refusal::shape(Kind::SubPlan, id, "an InitPlan referenced as a per-row SubPlan"));
    }
    let root = root_of(b, s.plan_id, Kind::SubPlan, id)?;
    let (mut share, mut store) = (false, false);
    if shared(b, s.plan_id) {
        let (keeps, seeds) = (crate::rerun::rescan_keeps(b, s, &[]), draws(b, root));
        share = (keeps || seeds) && one_set(b, s, root);
        // D1920's VOLATILE half: a plan whose root is the store its rescans keep is that one store for every calling node
        store = keeps && !share && one_store(b, s, root);
        if keeps && !share && !store {
            return Err(Refusal::shape(Kind::SubPlan, id, SHARED_STATE_KEPT_VOLATILE));
        }
        if seeds && !share && !store {
            return Err(Refusal::shape(Kind::SubPlan, id, SHARED_STATE_KEPT_SEED));
        }
    }
    let f = crate::rerun::of_subplan(b.stmt, b.facts, b.arena, s);
    let call = crate::rerun::of_subplan_stored(b.stmt, b.facts, b.arena, s);
    let quiet_but_rows = crate::effects::found::sub_plan_quiet_but_rows(b.stmt, b.facts, b.arena, s, f);
    let invariant = !s.useHashTable && crate::rerun::invariant(b.stmt, b.arena, root, crate::rerun::of_plan(b.stmt, b.facts, b.arena, root));
    let stops = site == SubSite::AggArg && stops(b);
    let desc = |form| SubPlanDesc { plan_id: s.plan_id, link, test: s.testexpr, args: &s.args, params: &s.parParam, sets: &s.setParam, ids: &s.paramIds, site, conditional, volatility: f.volatility, raise_free: f.raise_free, quiet_but_rows, invariant, unknown_eq_false: s.unknownEqFalse, alternative, set_spool: false, stops, ahead, call, form };
    if s.useHashTable {
        if !s.parParam.is_nil() {
            return Err(Refusal::shape(Kind::SubPlan, id, "a hashed SubPlan with parameters"));
        }
        // D1920: a later calling node probes the set the first one built
        if let Some(slot) = b.graph.shared_sets.iter().find(|&&(p, _)| p == s.plan_id).map(|&(_, slot)| slot).filter(|_| share) {
            crate::ops::shared::breaker::consume(b, &mut open, slot);
            place(b, &mut open, s.plan_id, Op::SubPlan(desc(SubForm::Hashed { slot })), None);
            b.graph.note(Kind::SubPlan);
            if alternative {
                b.graph.note(Kind::AlternativeSubPlan);
            }
            return Ok(open);
        }
        let mut sub = b.plan(root)?;
        let tlist = &root.as_plan().unwrap().targetlist;
        let kinds = tlist.iter().filter(|t| t.as_target_entry().is_some_and(|t| !t.resjunk)).map(|e| column_kind_of_expr(b.facts, e));
        let layout = keys_layout(b.arena, kinds);
        let rows = Builder::rows(root.as_plan().unwrap());
        sub.push(Op::SubPlan(desc(SubForm::Build)));
        let bits = crate::join::spill_bits(rows, layout.size());
        let slot = crate::ops::shared::breaker::close(b, sub, BreakerKind::Distinct, layout, Budget::Share, Spill::Partitioned { bits }, Kind::SubPlan, id)?;
        b.closed_by(root);
        if share {
            b.graph.shared_sets.push((s.plan_id, slot));
        }
        crate::ops::shared::breaker::consume(b, &mut open, slot);
        place(b, &mut open, s.plan_id, Op::SubPlan(desc(SubForm::Hashed { slot })), None);
    } else {
        // D1920: the store is mapped at the first calling node, before this caller's own pipelines (`p0`)
        let read = if store { Some(store_reader(b, s, root, id)?) } else { None };
        let p0 = b.graph.pipelines.len();
        // D3646: a correlated plan's slots stay held while it is mapped (`breakers::hold_range`)
        let held = (!s.parParam.is_nil()).then(|| crate::breakers::hold_range(&mut b.graph, p0));
        let inner = match read {
            Some(r) => Ok(r),
            None => b.plan(root),
        };
        let inner = inner.map(|i| crate::ops::shared::union::settle(&mut b.graph, i)); // embedded: one pipeline
        if let Some(h) = held {
            crate::breakers::release_range(&mut b.graph, p0, h);
        }
        let inner = inner?;
        b.closed_by(root); // a settled union's arms: the root closed them (declare_rerun reads its allParam)
        let mut params: ArenaVec<i32> = ArenaVec::new_in(b.arena); // alloc: Cold
        params.extend(s.parParam.iter());
        crate::rerun::declare_rerun(b, p0, id, s.plan_id, &params)?;
        // D2117: a fill the inner reads that may raise, run at the SubPlan's first evaluation as PG's first scan runs it
        crate::rerun::declare_sub_kept(b, p0, id, s.plan_id, &inner);
        // D3978: an inner over a breaker PG re-executes at every evaluation that no set refills (nest item 9's roads)
        if let crate::graph::Source::Breaker { slot } = &inner.source {
            let right = match inner.ops.first() {
                Some(Op::SetOp(crate::ops::shared::setop::SetDesc::Set { right, .. })) => Some(*right),
                _ => None,
            };
            let acts = crate::effects::Effects::of_found(crate::rerun::of_subplan(b.stmt, b.facts, b.arena, s)).acts;
            crate::rerun::declare_breaker_rerun(b, p0, (id, s.plan_id), *slot, right, acts);
        }
        b.nested_spans.push((s.plan_id, p0 as u16, b.graph.pipelines.len() as u16));
        let reads = inner.desc;
        let inner = ArenaBox::new_in(inner, b.arena);
        place(b, &mut open, s.plan_id, Op::SubPlan(desc(SubForm::Nested { inner })), Some(reads));
    }
    b.graph.note(Kind::SubPlan);
    if alternative {
        b.graph.note(Kind::AlternativeSubPlan);
    }
    Ok(open)
}

/// D2013 slice 2 (facts/hoist.rs): SubPlan `s`, hoisted out of nest `id`'s inner keys or pruning, mapped onto the nest's outer row
/// `open` before the Nest op whose NestLoopParam reads it (`reads`); unconditional: facts/hoist.rs `plan` hoists it only where it
/// runs for every outer row that reaches the Nest op; `ahead`, the licence of its value's evaluation there (`plan`).
pub(crate) fn map_hoisted<'p>(b: &mut Builder<'p, '_>, s: &'p SubPlan<'p>, k: Kind, id: i32, open: Open<'p>, ahead: Option<crate::effects::Licence>) -> Result<Open<'p>, Refusal> {
    map_subplan_held(b, Found1 { s, alternative: false, site: SubSite::Row, conditional: false }, k, id, open, ahead)
}

/// The per-row SubPlans of a mapped node's target list and quals become `Op::SubPlan`s placed before
/// the first op that reads them; one inside an aggregate's argument would run on the emit side of the
/// aggregate's breaker, so it refuses.
pub fn attach<'p>(b: &mut Builder<'p, '_>, node: Node<'p>, k: Kind, mut open: Open<'p>) -> Result<Open<'p>, Refusal> {
    let Some(plan) = node.as_plan() else { return Ok(open) };
    let id = plan.plan_node_id;
    let rows = crate::admit::plan_rows(plan);
    let mut found: Found<'p> = ArenaVec::new_in(b.arena);
    let mut err = None;
    let window = k == Kind::WindowAgg;
    // a grouping-sets aggregate's lists are each arm's (`ops::list_subplans` from agg's map_sets, after the arm's filters)
    let per_set = node.as_agg().is_some_and(crate::ops::per_set);
    // a qual conjunct is its own filter op, in order (`ops::filters`): unconditional at its place (`conditional`'s doc)
    for n in plan.targetlist.iter().chain(plan.qual.iter()).filter(|_| !per_set) {
        scan(n, SubSite::Row, false, window, &mut found, &mut err, rows, k, id);
    }
    // a Result's one-time qual is an implicit-AND list (`expr::walk` has no List arm): each clause, a later one only when
    // the earlier ones held (`ExecQual` stops at the first false)
    if let Some(r) = node.as_result().and_then(|r| r.resconstantqual) {
        match r.as_list() {
            Some(l) => l.iter().enumerate().for_each(|(i, c)| scan(c, SubSite::Row, i > 0, window, &mut found, &mut err, rows, k, id)),
            None => scan(r, SubSite::Row, false, window, &mut found, &mut err, rows, k, id),
        }
    }
    // a FROM function's arguments: the Srf op evaluates them once per (re)scan, before its first row (`reads`)
    if let Some(fs) = node.as_function_scan() {
        for e in fs.functions.iter().filter_map(|f| f.as_range_tbl_function()?.funcexpr) {
            scan(e, SubSite::Row, false, window, &mut found, &mut err, rows, k, id);
        }
    }
    let joinqual = match node.node_tag() {
        NodeTag::T_NestLoop => Some(&node.as_nest_loop().unwrap().join.joinqual),
        NodeTag::T_MergeJoin => Some(&node.as_merge_join().unwrap().join.joinqual),
        NodeTag::T_HashJoin => Some(&node.as_hash_join().unwrap().join.joinqual),
        _ => None,
    };
    // the join op evaluates its qual per candidate pair and stops at the first false conjunct (`ExecQual`); a draining hash
    // join's probe took its own (`attach_probe`)
    let drained = node.as_hash_join().is_some_and(|hj| crate::join::hash_drains(hj.join.jointype));
    for (i, n) in joinqual.into_iter().filter(|_| !drained).flat_map(|q| q.iter()).enumerate() {
        scan(n, SubSite::JoinQual, i > 0, window, &mut found, &mut err, rows, k, id);
    }
    // a hash join's outer keys, read by its probe (`attach_keys`; the Hash node's inner keys went on the build)
    if let Some(hj) = node.as_hash_join().filter(|_| !drained) {
        scan_keys(b, &hj.hashkeys, true, &mut found, &mut err, rows, k, id);
    }
    // a write's own lists (ON CONFLICT SET, the checks, RETURNING, the MERGE actions): write::Member::WriteSubPlan. A MERGE
    // action's WHEN qual and target list are conditional: ExecMergeMatched /
    // ExecMergeNotMatched reach them only for a row of the action's kind that no earlier action took, the target list only
    // once the qual held
    let mut write_hit = false;
    if let Some(mt) = node.as_modify_table() {
        let action = |l: &NodeList<'p>| {
            let mut acts = mt.mergeActionLists.iter().filter_map(|x| x.as_list()).flat_map(|x| x.iter()).filter_map(|a| a.as_merge_action());
            acts.any(|a| std::ptr::eq(&a.targetList, l) || a.qual.and_then(|q| q.as_list()).is_some_and(|q| std::ptr::eq(q, l)))
        };
        crate::ops::shared::modifytable::expr_lists(mt, &mut |l| {
            let cond = action(l);
            for n in l.iter() {
                write_hit |= has_subplan_ret(n);
                scan(n, SubSite::Row, cond, window, &mut found, &mut err, rows, k, id);
            }
        });
    }
    if let Some(e) = err {
        return Err(e);
    }
    if write_hit {
        for op in open.ops.iter_mut() {
            if let Op::Write(d) = op {
                d.members = d.members.with(crate::ops::shared::modifytable::Member::WriteSubPlan);
            }
        }
    }
    for f in found.iter() {
        open = map_subplan(b, *f, k, id, open)?;
    }
    Ok(open)
}

/// One SubPlan an expression holds: the SubPlan (or the chosen alternative), whether it was an alternative, its site and
/// whether PostgreSQL may skip it for a row that reaches its op (`SubPlanDesc::conditional`).
#[derive(Clone, Copy)]
struct Found1<'p> {
    s: &'p SubPlan<'p>,
    alternative: bool,
    site: SubSite,
    conditional: bool,
}

type Found<'p> = ArenaVec<Found1<'p>>;

/// The SubPlans of one expression, `cond` when the expression itself is reached only on some rows. A window function's
/// arguments are declared here as `AggArg`; an aggregate's were declared on the input side by `attach_agg_arg` (agg.rs,
/// before the fold), so its subtree is skipped.
fn scan<'p>(n: Node<'p>, site: SubSite, cond: bool, window: bool, found: &mut Found<'p>, err: &mut Option<Refusal>, rows: f64, k: Kind, id: i32) {
    crate::walk::stack_check(); // D3748
    walk_cond(n, cond, &mut |x, cond| match x.node_tag() {
        NodeTag::T_Aggref | NodeTag::T_WindowFunc if site == SubSite::Row && expr::has_subplan(x) => {
            // a window function's D1415 step arguments run before the window op; its other arguments' and FILTER's are
            // the fill's (`attach_window_args`) or refuse by name
            match x.as_window_func() {
                Some(w) if window => {
                    for (a, c) in w.args.iter().enumerate().filter(|&(a, _)| crate::ops::window_step_arg(w, a)) {
                        scan(c, SubSite::AggArg, cond || w.aggfilter.is_some(), window, found, err, rows, k, id);
                    }
                }
                _ if window => agg_kids(x, cond, &mut |c, cond| scan(c, SubSite::AggArg, cond, window, found, err, rows, k, id)),
                _ => {}
            }
            false
        }
        _ => visit(x, site, cond, found, err, rows, k, id),
    });
}

/// The SubPlans inside one aggregate call (its arguments, ORDER BY, direct arguments, FILTER) placed on the group's
/// INPUT pipeline `open` after its projection, before the fold reads them (`SubSite::AggArg`; decision 80). The
/// planner's hoisting into the input's projection is the steer; this is where they run.
pub fn attach_agg_arg<'p>(b: &mut Builder<'p, '_>, aggref: Node<'p>, k: Kind, id: i32, mut open: Open<'p>) -> Result<Open<'p>, Refusal> {
    let rows = open.desc.predicted_rows as f64;
    let mut found: Found<'p> = ArenaVec::new_in(b.arena);
    let mut err = None;
    agg_kids(aggref, false, &mut |c, cond| scan(c, SubSite::AggArg, cond, false, &mut found, &mut err, rows, k, id));
    if let Some(e) = err {
        return Err(e);
    }
    for f in found.iter() {
        open = map_subplan(b, *f, k, id, open)?;
    }
    Ok(open)
}

/// The SubPlans of a hash join's keys over one side's row, placed on that side's pipeline `open` before the op that reads
/// them (`reads`): the Hash node's inner keys (`outer` false) before the build's end, which keys each row it inserts; the
/// outer keys (`attach`) before the probe. PostgreSQL computes a side's hash value with ExecBuildHash32Expr: the keys in
/// order, one past the first only when the keys before it are not NULL (the strict hash stops at a NULL key unless the
/// side keeps NULLs), and an outer row's only over a non-empty table (ExecHashJoin ends an inner join at an empty table
/// before it hashes an outer row). So a key's SubPlan is conditional on the outer side and past the first key: lent to
/// the key's read (lowerv4 site.rs `Lend::Row`), where hashjoin/keys.rs `key_cells` stops as PG stops and the probe ends
/// at an empty table before its keys. PG also runs the hash clauses again on each candidate pair (ExecScanHashBucket),
/// the key's SubPlan with them, where the probe decides by the key images: a SubPlan that calls a VOLATILE function
/// refuses by name; any other gives that run's value again (its parameters are the row's).
pub fn attach_keys<'p>(b: &mut Builder<'p, '_>, keys: &'p NodeList<'p>, k: Kind, id: i32, mut open: Open<'p>) -> Result<Open<'p>, Refusal> {
    let rows = open.desc.predicted_rows as f64;
    let mut found: Found<'p> = ArenaVec::new_in(b.arena);
    let mut err = None;
    scan_keys(b, keys, false, &mut found, &mut err, rows, k, id);
    if let Some(e) = err {
        return Err(e);
    }
    for f in found.iter() {
        open = map_subplan(b, *f, k, id, open)?;
    }
    Ok(open)
}

/// The SubPlans a draining hash join's probe reads (a Right, Full, right semi or right anti join: join.rs `hash_drains`),
/// its join qual's (`SubSite::JoinQual`, past the first conjunct conditional) and its outer keys' (`scan_keys`), placed on
/// the probe pipeline `open` before the probe op: join.rs closes that pipeline into the drain's spool before the node's
/// `attach` runs, which takes them for any other join.
pub fn attach_probe<'p>(b: &mut Builder<'p, '_>, hj: &'p types_nodes::plannodes::HashJoin<'p>, k: Kind, id: i32, mut open: Open<'p>) -> Result<Open<'p>, Refusal> {
    let rows = open.desc.predicted_rows as f64;
    let mut found: Found<'p> = ArenaVec::new_in(b.arena);
    let mut err = None;
    for (i, n) in hj.join.joinqual.iter().enumerate() {
        scan(n, SubSite::JoinQual, i > 0, false, &mut found, &mut err, rows, k, id);
    }
    scan_keys(b, &hj.hashkeys, true, &mut found, &mut err, rows, k, id);
    if let Some(e) = err {
        return Err(e);
    }
    for f in found.iter() {
        open = map_subplan(b, *f, k, id, open)?;
    }
    Ok(open)
}

/// The SubPlans of window functions `funcs`' arguments past their D1415 step arguments and of their FILTERs that raise
/// nothing and call nothing VOLATILE, placed on the partition buffer's fill `open` (no op of it reads them: its end stores
/// each argument cell, lowerv4 window/rows.rs); a FILTERed one's arguments are conditional, so lent to the cell's read.
/// PG evaluates each where a frame, an offset row or a FILTER reads the row, as often as it reads it: for such a SubPlan
/// the count and the time show nothing (`DECISIONS.md` 3). One that may raise (and calls nothing VOLATILE) is placed there
/// too under a recheck licence, as ops/limit.rs `window_keeps_stop` reads the fill's own code ahead: the fill evaluates
/// rows PG may never read (a frame that skips a row, lag's last row, a LIMIT's or a run condition's stop), so an error it
/// raises before any row left re-runs the statement in plan order (exec runner/recheck.rs), where the licence is denied and
/// the window refuses by name. Only a window whose fill runs once, before any row leaves the statement (`fill_once`: not
/// a nested loop's inner, a later Append arm, a SubPlan's or a CTE's plan); a statement that acts, a cursor's partial
/// fetch (`deny_all_bounded`) and the plan-order run itself deny the licence, and the SubPlan is placed nowhere: it
/// refuses by name in lowerv4 (window/lower.rs `admit`). One that acts is placed nowhere (each cell lazy, the object
/// evaluates it: D1232).
/// D3374 (the window argument SubPlan): the plan-order run of a read taken to its end (`deny_all`, not bounded) places
/// it on the fill anyway, in a statement that does not act, where PostgreSQL 18.6 evaluates the argument at every row the
/// window stores before the statement ends (`read_at`, `frame_reads_every_row`): an error the fill raises there
/// PostgreSQL raises too, at its own first such row or at another error of the statement before it (`DECISIONS.md` 4
/// (D): where PostgreSQL raises, any genuine error passes).
pub fn attach_window_args<'p>(b: &mut Builder<'p, '_>, funcs: &[Node<'p>], k: Kind, id: i32, mut open: Open<'p>) -> Result<Open<'p>, Refusal> {
    let rows = open.desc.predicted_rows as f64;
    let mut found: Found<'p> = ArenaVec::new_in(b.arena);
    let mut err = None;
    let (stmt, facts, arena) = (b.stmt, b.facts, b.arena);
    let read = stmt.planTree.and_then(|r| read_at(r, id, Read::All));
    let every = read.filter(|&(r, w)| frame_reads_every_row(r, w));
    // D3374: the found SubPlans PostgreSQL evaluates at every stored row: an aggregate's arguments under no FILTER, and a
    // FILTER (evaluated for each row a frame reads, before the arguments); `exact`, an argument of an aggregate with no
    // FILTER PostgreSQL evaluates exactly once per stored row, in the buffer's order (`frame_reads_each_once`)
    let mut each: ArenaVec<bool> = ArenaVec::new_in(arena);
    let mut exact: ArenaVec<bool> = ArenaVec::new_in(arena);
    let once_each = read.is_some_and(|(r, w)| frame_reads_each_once(r, w));
    for w in funcs.iter().filter_map(|f| f.as_window_func()) {
        let fill = w.args.iter().enumerate().filter(|&(a, _)| !crate::ops::window_step_arg(w, a)).map(|(_, n)| (n, w.aggfilter.is_some()));
        for (n, cond) in fill.chain(w.aggfilter.map(|q| (q, false))) {
            scan(n, SubSite::Row, cond, false, &mut found, &mut err, rows, k, id);
            let v = every.is_some() && w.winagg && !cond;
            each.resize(found.len(), v);
            exact.resize(found.len(), once_each && w.winagg && w.aggfilter.is_none());
        }
    }
    if let Some(e) = err {
        return Err(e);
    }
    use crate::effects::{denying, denying_bounded, licence, of_statement, unobservable, whole_read, Cx, Departure, Effects};
    let once = stmt.planTree.and_then(|r| fill_once(r, id, true)).unwrap_or(false);
    let acts = |f: &Found1<'p>| Effects::of_found(crate::rerun::of_subplan(stmt, facts, arena, f.s)).acts;
    let acting = found.iter().filter(|&f| acts(f)).count();
    for (i, f) in found.iter().enumerate() {
        let e = Effects::of_found(crate::rerun::of_subplan(stmt, facts, arena, f.s));
        // D3374 (an acting window argument SubPlan): the window's one acting SubPlan, a correlated one PostgreSQL runs at
        // each evaluation (no hashed set, no parameterless plan) of an argument it evaluates once per stored row in the
        // buffer's order (`exact`), every row read, is the fill's too: it evaluates it per row in that order, so its
        // calls are PostgreSQL's (lowerv4 window/lower.rs `admit` asks the effects home whether anything else of the
        // statement acts or raises among them, else refuses by name; it holds the fill's whole-read licence)
        let per_row = !f.s.useHashTable && !f.s.parParam.is_nil() && !f.conditional;
        if e.acts && acting == 1 && per_row && exact.get(i).copied().unwrap_or(false) {
            open = map_subplan(b, *f, k, id, open)?;
            continue;
        }
        if e.acts || e.may_raise && !once {
            continue;
        }
        // construct C2: the licence the fill's evaluation ahead of the frame reads holds (`SubPlanDesc::ahead`): the
        // recheck licence; in the plan-order run of a read to its end, the whole-read licence (code that does not act,
        // every stored row's argument read by PostgreSQL too); a quiet SubPlan's, `unobservable` (evaluated once per
        // stored row where PostgreSQL evaluates it at every frame read of the row: fewer runs as well as ahead)
        let ahead = if e.may_raise {
            let ask = e.and(Effects { acts: of_statement(&Cx::new(stmt, facts, arena)).acts, ..Effects::NONE });
            match licence(ask, Departure::Ahead) {
                Ok(l) => {
                    b.graph.licensed(&l);
                    if l.recheck() {
                        b.graph.licensed_nodes.push(id); // C2 R3: the window's fill runs this departure
                    }
                    Some(l)
                }
                // D3374: the plan-order run over a read to its end, every stored row's argument read by PostgreSQL
                Err(_) if each.get(i).copied().unwrap_or(false) && !ask.acts && denying() && !denying_bounded() => whole_read(e).ok(),
                Err(_) => continue,
            }
        } else {
            unobservable(e)
        };
        open = map_subplan_held(b, *f, k, id, open, ahead)?;
    }
    Ok(open)
}

/// D3374: how far PostgreSQL 18.6 reads a plan node's rows when the statement is read to its end: `All` of them,
/// at least its first one (`Once`), or maybe none (`No`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Read {
    All,
    Once,
    No,
}

/// D3374: the reading of WindowAgg `id` under `n`, read `r`, with the node (`None`: not in this tree; a SubPlan's or a
/// CTE's plan is never searched). A Sort and a plain or hashed Agg read their whole input at their first fetch; a Limit
/// whose count is a constant above zero reads its child at least once (a count of zero, or one known only at the run:
/// maybe never); a Result without a one-time qual, a project set, a unique, a sorted aggregate or group, a WindowAgg with
/// no run condition, a material, a gather, a lock, a write, a subquery scan, a nested loop's outer side, every MergeAppend
/// arm and an Append's first arm read as they are read (an Append's later arm only when it is read to its end). Any other
/// node, a nested loop's inner and either side of a hash or merge join (an empty side ends the join) may read nothing.
fn read_at<'p>(n: Node<'p>, id: i32, r: Read) -> Option<(Read, &'p types_nodes::plannodes::WindowAgg<'p>)> {
    crate::walk::stack_check(); // D3748
    use crate::ops::shared::agg::{AGG_HASHED, AGG_PLAIN};
    let p = n.as_plan()?;
    if p.plan_node_id == id {
        return n.as_window_agg().map(|w| (r, w));
    }
    let (full, once) = (if r == Read::No { Read::No } else { Read::All }, if r == Read::No { Read::No } else { Read::Once });
    let left = match n.node_tag() {
        NodeTag::T_Sort => full,
        NodeTag::T_Agg if n.as_agg().is_some_and(|a| matches!(a.aggstrategy, AGG_PLAIN | AGG_HASHED)) => full,
        NodeTag::T_Limit => match n.as_limit().map(crate::ops::limit_bounds) {
            Some((crate::graph::Bound::All, _)) => r,
            Some((crate::graph::Bound::Const(c), _)) if c > 0 => once,
            _ => Read::No,
        },
        NodeTag::T_Result if n.as_result().is_some_and(|x| x.resconstantqual.is_none()) => r,
        NodeTag::T_WindowAgg if n.as_window_agg().is_some_and(|w| w.runCondition.is_nil()) => r,
        NodeTag::T_ProjectSet | NodeTag::T_Unique | NodeTag::T_Agg | NodeTag::T_Group | NodeTag::T_Material | NodeTag::T_Gather | NodeTag::T_GatherMerge | NodeTag::T_LockRows | NodeTag::T_ModifyTable | NodeTag::T_NestLoop => r,
        _ => Read::No,
    };
    if let Some(x) = p.lefttree.and_then(|l| read_at(l, id, left)) {
        return Some(x);
    }
    if let Some(x) = p.righttree.and_then(|l| read_at(l, id, Read::No)) {
        return Some(x);
    }
    if let Some(s) = n.as_subquery_scan().and_then(|s| s.subplan) {
        return read_at(s, id, r);
    }
    if let Some(m) = n.as_merge_append() {
        return m.mergeplans.iter().find_map(|c| read_at(c, id, r));
    }
    if let Some(a) = n.as_append() {
        return a.appendplans.iter().enumerate().find_map(|(i, c)| read_at(c, id, if i == 0 || r == Read::All { r } else { Read::No }));
    }
    None
}

/// D3374: whether PostgreSQL 18.6 evaluates an aggregate's argument at every row WindowAgg `w`, read `r`, stores. No run
/// condition (past it nodeWindowAgg stops advancing the aggregates). Read to its end: each frame holds its current row
/// (its start no FOLLOWING bound, its end no PRECEDING one, no EXCLUDE), so each row is advanced when a frame first
/// reaches it. Read at least once: one partition (no PARTITION BY) whose every frame is the whole partition (UNBOUNDED
/// PRECEDING to UNBOUNDED FOLLOWING, or RANGE / GROUPS to CURRENT ROW with no ORDER BY: every row a peer), all of which
/// the first row's frame advances.
fn frame_reads_every_row(r: Read, w: &types_nodes::plannodes::WindowAgg<'_>) -> bool {
    use types_nodes::rawnodes::*;
    let f = w.frameOptions;
    let current = f & (FRAMEOPTION_START_UNBOUNDED_FOLLOWING | FRAMEOPTION_START_OFFSET_FOLLOWING | FRAMEOPTION_END_UNBOUNDED_PRECEDING | FRAMEOPTION_END_OFFSET_PRECEDING | FRAMEOPTION_EXCLUSION) == 0;
    let to_end = f & FRAMEOPTION_END_UNBOUNDED_FOLLOWING != 0 || (w.ordNumCols == 0 && f & FRAMEOPTION_ROWS == 0 && f & FRAMEOPTION_END_CURRENT_ROW != 0);
    let whole = w.partNumCols == 0 && f & FRAMEOPTION_START_UNBOUNDED_PRECEDING != 0 && f & FRAMEOPTION_EXCLUSION == 0 && to_end;
    w.runCondition.is_nil() && ((r == Read::All && current) || (r == Read::Once && whole))
}

/// D3374: whether PostgreSQL 18.6 evaluates an aggregate's argument exactly once at every row WindowAgg `w`, read `r`,
/// stores, in the buffer's order. Read to its end, no run condition, each frame holding its current row
/// (`frame_reads_every_row`'s reading), and a frame head that never leaves the partition's first row (UNBOUNDED
/// PRECEDING): nodeWindowAgg.c `eval_windowaggregates` then restarts an aggregate only at a partition's first row and
/// advances it over each row once, as a frame end first reaches the row (a moving head or an EXCLUDE clause would restart
/// it and evaluate the rows again).
fn frame_reads_each_once(r: Read, w: &types_nodes::plannodes::WindowAgg<'_>) -> bool {
    use types_nodes::rawnodes::FRAMEOPTION_START_UNBOUNDED_PRECEDING;
    r == Read::All && frame_reads_every_row(r, w) && w.frameOptions & FRAMEOPTION_START_UNBOUNDED_PRECEDING != 0
}

/// `attach_window_args`' bound on a raising argument SubPlan: whether WindowAgg `id`, found under `n` (`ok`: every node
/// above reads it once, ahead of any row of its own), fills its buffer once per statement before any row left the
/// statement, so an error the fill raises reaches the plan-order re-run (exec runner/recheck.rs runs only before the first
/// row). A node above that reads it in one pass ahead of its first row keeps `ok`: a sort, a limit, a result or project
/// set, a unique, an aggregate or group, a window, a material, a gather, a subquery scan, a hash (the build runs before
/// the probe's first row), a join's outer side, a merge join's inner (read once, ahead of its first row), every arm of a
/// MergeAppend (each arm's first row is read before any row leaves it). A nested loop's inner (re-run per outer row), an
/// Append's later arm (run after the earlier arms' rows left) and any other node clear it; a SubPlan's or CTE's plan is
/// never searched (`None`: not in the main tree).
fn fill_once(n: Node<'_>, id: i32, ok: bool) -> Option<bool> {
    crate::walk::stack_check(); // D3748
    let p = n.as_plan()?;
    if p.plan_node_id == id {
        return Some(ok);
    }
    let pass = matches!(
        n.node_tag(),
        NodeTag::T_Sort | NodeTag::T_IncrementalSort | NodeTag::T_Limit | NodeTag::T_Result | NodeTag::T_ProjectSet | NodeTag::T_Unique | NodeTag::T_Agg | NodeTag::T_Group | NodeTag::T_WindowAgg | NodeTag::T_Material | NodeTag::T_Gather | NodeTag::T_GatherMerge | NodeTag::T_Hash
    );
    let join = matches!(n.node_tag(), NodeTag::T_NestLoop | NodeTag::T_HashJoin | NodeTag::T_MergeJoin);
    let inner_once = matches!(n.node_tag(), NodeTag::T_HashJoin | NodeTag::T_MergeJoin);
    if let Some(r) = p.lefttree.and_then(|l| fill_once(l, id, ok && (pass || join))) {
        return Some(r);
    }
    if let Some(r) = p.righttree.and_then(|l| fill_once(l, id, ok && inner_once)) {
        return Some(r);
    }
    if let Some(s) = n.as_subquery_scan().and_then(|s| s.subplan) {
        return fill_once(s, id, ok);
    }
    if let Some(m) = n.as_merge_append() {
        return m.mergeplans.iter().find_map(|c| fill_once(c, id, ok));
    }
    if let Some(a) = n.as_append() {
        return a.appendplans.iter().enumerate().find_map(|(i, c)| fill_once(c, id, ok && i == 0));
    }
    None
}

/// A SubPlan in a hash join's key that calls a VOLATILE function: PG runs it again per candidate pair in the hash
/// clause (ExecScanHashBucket) beside each row's key, so its calls follow the table's contents.
const KEY_VOLATILE: &str = "a SubPlan in a hash join's key that calls a VOLATILE function (PG runs it again in the hash clause on each candidate pair)";

/// `attach_keys`' scan of key list `keys` (`outer`: the probe's).
#[allow(clippy::too_many_arguments)]
fn scan_keys<'p>(b: &Builder<'p, '_>, keys: &'p NodeList<'p>, outer: bool, found: &mut Found<'p>, err: &mut Option<Refusal>, rows: f64, k: Kind, id: i32) {
    let n0 = found.len();
    for (i, n) in keys.iter().enumerate() {
        scan(n, SubSite::Row, outer || i > 0, false, found, err, rows, k, id);
    }
    if found[n0..].iter().any(|f| crate::effects::Effects::of_found(crate::rerun::of_subplan(b.stmt, b.facts, b.arena, f.s)).acts) {
        err.get_or_insert(Refusal::shape(k, id, KEY_VOLATILE));
    }
}

/// The per-row SubPlans of one grouping set's arm lists (`lists`: its qual and target list, with that set's projection
/// folded in), placed on the arm's emit `open` before the op that first reads each; aggregates' arguments were placed
/// on the arm's input (`attach_agg_arg`).
pub fn attach_lists<'p>(b: &mut Builder<'p, '_>, lists: &[&'p NodeList<'p>], k: Kind, id: i32, mut open: Open<'p>) -> Result<Open<'p>, Refusal> {
    let rows = open.desc.predicted_rows as f64;
    let mut found: Found<'p> = ArenaVec::new_in(b.arena);
    let mut err = None;
    for n in lists.iter().flat_map(|l| l.iter()) {
        scan(n, SubSite::Row, false, false, &mut found, &mut err, rows, k, id);
    }
    if let Some(e) = err {
        return Err(e);
    }
    for f in found.iter() {
        open = map_subplan(b, *f, k, id, open)?;
    }
    Ok(open)
}

/// The SubPlans inside a `VALUES` list's rows, placed first on the values pipeline (`SubSite::Values`: the source's
/// row expressions read them; decision 80). D4449: a Const cell holds none, so only the other cells are scanned, and the
/// walk counts them for the list's readers (`PipelineGraph::values_coded`).
pub fn attach_values<'p>(b: &mut Builder<'p, '_>, lists: &'p NodeList<'p>, k: Kind, id: i32, mut open: Open<'p>) -> Result<Open<'p>, Refusal> {
    let rows = lists.len() as f64;
    let mut found: Found<'p> = ArenaVec::new_in(b.arena);
    let mut err = None;
    let mut coded = crate::graph::ValuesCoded { coded_rows: 0, coded_cells: 0 };
    for l in lists.iter() {
        let Some(row) = l.as_list() else {
            coded.coded_rows += 1;
            continue;
        };
        let cells = coded.coded_cells;
        for e in row.iter().filter(|e| e.as_const().is_none()) {
            coded.coded_cells += 1;
            scan(e, SubSite::Values, false, false, &mut found, &mut err, rows, k, id);
        }
        coded.coded_rows += u32::from(coded.coded_cells != cells);
    }
    b.graph.values.push((lists as *const NodeList<'p> as usize, coded));
    if let Some(e) = err {
        return Err(e);
    }
    for f in found.iter() {
        open = map_subplan(b, *f, k, id, open)?;
    }
    Ok(open)
}

/// One node of a scanned expression: a SubPlan (or the chosen alternative) is recorded once with the site it was first met
/// at; it stays conditional only while every place it is met at is.
fn visit<'p>(x: Node<'p>, site: SubSite, cond: bool, found: &mut Found<'p>, err: &mut Option<Refusal>, rows: f64, k: Kind, id: i32) -> bool {
    let mut record = |s: &'p SubPlan<'p>, alternative: bool| match found.iter_mut().find(|f| f.s.plan_id == s.plan_id) {
        Some(f) => f.conditional &= cond,
        None => found.push(Found1 { s, alternative, site, conditional: cond }),
    };
    match x.node_tag() {
        NodeTag::T_SubPlan => {
            record(x.as_sub_plan().unwrap(), false);
            false
        }
        NodeTag::T_AlternativeSubPlan => {
            match choose(&x.as_alternative_sub_plan().unwrap().subplans, rows) {
                Some(s) => record(s, true),
                None => *err = Some(Refusal::shape(k, id, "an AlternativeSubPlan without SubPlans")),
            }
            false
        }
        _ => err.is_none(),
    }
}

impl SubPlanDesc<'_> {
    /// What running the SubPlan (its plan, test and args) may do, in the effects home's terms (`Effects::of_found`: a
    /// VOLATILE call acts). Readers decide eager or lazy through `effects::licence` on this, not on the two flags.
    pub fn effects(&self) -> crate::effects::Effects {
        crate::effects::Effects::of_sub_plan(self)
    }

    /// The one answer for what running the SubPlan (its plan, test and args) may do: its `effects`, whether it runs user
    /// code, and whether it makes a call whose place among the statement's calls shows (`Found::order_call`). Every reader
    /// of a SubPlan step asks this, not the step's separate fields.
    pub fn answer(&self) -> crate::effects::SubEffects {
        crate::effects::SubEffects::of_sub_plan(self)
    }

    pub fn dump(&self, out: &mut dyn Write) -> fmt::Result {
        let form = match self.form {
            SubForm::Nested { .. } => "nested",
            SubForm::Build => "build",
            SubForm::Hashed { .. } => "hashed",
        };
        write!(out, "  subplan {} {form} {:?}{}{}", self.plan_id, self.link, if self.unknown_eq_false { " unknown-eq-false" } else { "" }, if self.alternative { " alternative" } else { "" })?;
        if let SubForm::Hashed { slot } = self.form {
            write!(out, " slot={slot}")?;
        }
        if !self.params.is_nil() {
            write!(out, " params=(")?;
            for (i, (p, a)) in self.params.iter().zip(self.args.iter()).enumerate() {
                write!(out, "{}$exec{p}=", if i > 0 { ", " } else { "" })?;
                expr::print(a, out)?;
            }
            write!(out, ")")?;
        }
        if !self.sets.is_nil() {
            write!(out, " sets=(")?;
            for (i, p) in self.sets.iter().enumerate() {
                write!(out, "{}$exec{p}", if i > 0 { "," } else { "" })?;
            }
            write!(out, ")")?;
        }
        if !self.ids.is_nil() {
            write!(out, " ids=(")?;
            for (i, p) in self.ids.iter().enumerate() {
                write!(out, "{}$exec{p}", if i > 0 { "," } else { "" })?;
            }
            write!(out, ")")?;
        }
        match self.site {
            SubSite::Row => {}
            SubSite::JoinQual => write!(out, " site=joinqual")?,
            SubSite::AggArg => write!(out, " site=aggarg")?,
            SubSite::Values => write!(out, " site=values")?,
        }
        if self.conditional {
            write!(out, " conditional")?;
        }
        if self.invariant {
            write!(out, " invariant")?;
        }
        crate::rerun::dump_f5(out, self.volatility, self.raise_free)?;
        if let Some(t) = self.test {
            write!(out, " test=")?;
            expr::print(t, out)?;
        }
        if let SubForm::Nested { inner } = &self.form {
            writeln!(out)?;
            write!(out, "  inner ")?;
            crate::dump::nested(inner, out)?;
        }
        Ok(())
    }
}
