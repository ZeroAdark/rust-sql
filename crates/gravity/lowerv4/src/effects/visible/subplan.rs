//! What evaluating a per-row SubPlan's neighbours may do, asked of the effects home. per_row.rs `before` walks the nodes
//! PostgreSQL's expression interpreter evaluates ahead of a SubPlan's read (children first, in order) and asks, per node,
//! `effects::of_node` (decompose rerun.rs `Walk::expr`'s per-node rules, where a node the walk does not know acts and may
//! raise). Lowering holds the graph, not the plan, so two nodes are read from the graph instead: another SubPlan by its
//! step's facts, and a PARAM_EXEC also by those of the InitPlan that sets it (PostgreSQL runs the InitPlan at the
//! parameter's first read).
//! - `acts`: evaluating the node may call a VOLATILE function;
//! - `own`: it may call a VOLATILE function or raise;
//! - `per_row_transition`: an aggregate's transition runs as each row arrives, not at the final call;
//! - `step` / `whole`: the one SubPlan answer (decompose `SubPlanDesc::answer`) for a SubPlan, or over every SubPlan an
//!   expression holds, read from the graph's steps (fail closed: `SubEffects::UNKNOWN` where no step names one);
//! - `calls_whole`: an expression calls user code or a VOLATILE function in whole scope, its own parts and each SubPlan's
//!   plan, test and args (D4138).
use gravity_decompose::effects::{of_node, Cx, Effects, SubEffects};
use gravity_decompose::{Op, PipelineNode};
use gravity_program::ArenaRef;
use types_nodes::Node;

/// Node `n` itself, its children apart: a SubPlan by the one answer of pipeline `p`'s step for it (`SubPlanDesc::answer`;
/// unknown without one), any other node by `of_node`; a PARAM_EXEC adds the InitPlans that set it.
fn effects(gx: &crate::graph::Gx<'_, '_>, arena: ArenaRef, p: &PipelineNode<'_>, n: Node<'_>) -> Effects {
    let own = match n.as_sub_plan() {
        Some(s) => p.ops.iter().find_map(|o| if let Op::SubPlan(t) = o { (t.plan_id == s.plan_id).then(|| t.answer().effects) } else { None }).unwrap_or(Effects::UNKNOWN),
        None => of_node(&Cx::without_stmt(gx.facts, arena), n),
    };
    let exec = n.as_param().filter(|q| q.paramkind == types_nodes::ParamKind::PARAM_EXEC);
    let sets = |i: &&gravity_decompose::graph::InitPlan<'_>| exec.is_some_and(|q| i.params.iter().any(|x| x == q.paramid));
    gx.graph.init_plans.iter().filter(sets).fold(own, |e, i| e.and(Effects::of_init_plan(i)))
}

/// The one SubPlan answer (decompose `SubPlanDesc::answer`) for SubPlan `plan_id` as its step in pipeline `k` holds it,
/// read across the graph's pipelines and embedded inners when `k` has no step for it; `SubEffects::UNKNOWN` when no step
/// names it (fail closed).
pub(crate) fn step(gx: &crate::graph::Gx<'_, '_>, k: usize, plan_id: i32) -> SubEffects {
    let ps = gx.pipelines();
    let of = |p: &PipelineNode<'_>| p.ops.iter().find_map(|o| if let Op::SubPlan(t) = o { (t.plan_id == plan_id).then(|| t.answer()) } else { None });
    ps.get(k).and_then(of).or_else(|| ps.iter().chain(ps.inners().into_iter().map(|x| x.1)).find_map(of)).unwrap_or(SubEffects::UNKNOWN)
}

/// The one SubPlan answer over every SubPlan expression `n` holds (`step` of each, in pipeline `k`): what the plans an
/// evaluation of `n` runs may do, which the per-expression readings (own scope: a SubPlan by its own parts) do not see.
/// `SubEffects::NONE` for an expression without a SubPlan.
pub(crate) fn whole(gx: &crate::graph::Gx<'_, '_>, k: usize, n: Node<'_>) -> SubEffects {
    let mut out = SubEffects::NONE;
    gravity_decompose::expr::walk(n, &mut |x| {
        if let Some(s) = x.as_sub_plan() {
            out = out.and(step(gx, k, s.plan_id));
        }
        true
    });
    out
}

/// D4138: expression `n` calls user code or a VOLATILE function in whole scope: in its own parts (`Fx::user_or_volatile`,
/// which reads a SubPlan by its test and args only) or in the plan, test and args of a SubPlan it holds (`whole`, read
/// across the graph's steps: `usize::MAX` names no pipeline first).
pub(crate) fn calls_whole(gx: &crate::graph::Gx<'_, '_>, n: Node<'_>) -> bool {
    let w = whole(gx, usize::MAX, n);
    gx.fx(n).user_or_volatile() || w.user_code || w.effects.acts
}

/// Whether evaluating node `n` itself may call a VOLATILE function.
pub(crate) fn acts(gx: &crate::graph::Gx<'_, '_>, arena: ArenaRef, p: &PipelineNode<'_>, n: Node<'_>) -> bool {
    effects(gx, arena, p, n).acts
}

/// Whether evaluating node `n` itself may call a VOLATILE function or raise.
pub(crate) fn own(gx: &crate::graph::Gx<'_, '_>, arena: ArenaRef, p: &PipelineNode<'_>, n: Node<'_>) -> bool {
    let e = effects(gx, arena, p, n);
    e.acts || e.may_raise
}

/// Whether aggregate `a`'s transition (and its direct arguments) runs as each row arrives: a plain aggregate's does, a
/// DISTINCT or ORDER BY aggregate's runs at the final call unless its input comes sorted (`aggpresorted`).
pub(crate) fn per_row_transition(a: &types_nodes::primnodes::Aggref<'_>) -> bool {
    a.aggdistinct.is_nil() && a.aggorder.is_nil() || a.aggpresorted
}

/// D3274 (rerun/epq/sub_lock.rs `fresh_ok`): whether a second run of InitPlan `i`, whose plan runs pipelines `ps`, shows
/// nothing: no VOLATILE call, no call of user code or of a function the facts cannot name (decompose's parallel hazard
/// counts those unsafe), and no TABLESAMPLE scan (its sample is drawn per scan). Its value is then the same under the
/// statement's snapshot, as is any error it raises.
pub(crate) fn init_rerun_alike(gx: &crate::graph::Gx<'_, '_>, i: &gravity_decompose::InitPlan<'_>, ps: &[usize]) -> bool {
    let sample = ps.iter().any(|&q| matches!(gx.pipelines()[q].source, gravity_decompose::Source::Sample { .. }));
    i.volatility != gravity_decompose::effects::Volatility::Volatile && i.parallel_safe && !sample
}

#[cfg(test)]
mod tests {
    use gravity_decompose::effects::found::Found;
    use gravity_decompose::effects::{SubEffects, Volatility};
    use gravity_decompose::graph::Source;
    use gravity_decompose::{NoFacts, PipelineGraph, PipelineNode};
    use gravity_program::{Arena, End, Pipeline, Reason};
    use types_nodes::list::NodeList;
    use types_nodes::primnodes::{FuncExpr, OpExpr, Param, SubLinkType, SubPlan, TargetEntry};
    use types_nodes::{Node, ParamKind};

    /// The one SubPlan answer fails closed: a SubPlan no step of the graph names reads `SubEffects::UNKNOWN` (acts, may
    /// raise, user code, a call whose place shows), so D4138's whole-scope reading calls; an expression without a SubPlan
    /// reads `SubEffects::NONE`, its own-scope reading alone.
    #[test]
    fn a_subplan_no_step_names_reads_unknown() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let m = Box::leak(Box::new(mcx::MemoryContext::new("subplan-answer"))).mcx(); // alloc: Cold
        let arena: &'static Arena = Box::leak(Box::new(Arena::new(1 << 16))); // alloc: Cold
        let a = arena.handle();
        let mut g = PipelineGraph::new(a);
        g.pipelines.push(PipelineNode::new(a, 1, Source::Const, Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite)));
        let gx = crate::graph::Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
        let sub = Node::mk(m, SubPlan { plan_id: 7, ..Default::default() }).unwrap();
        let abs = Node::mk(m, FuncExpr { funcid: 1397, funcresulttype: 23, args: NodeList::from_slice(m, &[sub]).unwrap(), ..Default::default() }).unwrap();
        assert_eq!(super::step(&gx, 0, 7), SubEffects::UNKNOWN, "no step names SubPlan 7");
        assert_eq!(super::whole(&gx, 0, abs), SubEffects::UNKNOWN, "an expression over SubPlan 7");
        assert!(super::calls_whole(&gx, abs), "D4138: abs((SubPlan 7)) calls in whole scope (fail closed)");
        let plain = Node::mk(m, FuncExpr { funcid: 1397, funcresulttype: 23, args: NodeList::nil(), ..Default::default() }).unwrap();
        assert_eq!(super::whole(&gx, 0, plain), SubEffects::NONE, "no SubPlan: the own-scope readings answer");
    }

    /// D4138: a graph whose one pipeline holds SubPlan `plan_id`'s step, its call facts `call` (`SubPlanDesc::call`).
    fn with_step(a: gravity_program::ArenaRef, plan_id: i32, call: Found) -> PipelineGraph<'static> {
        with_walk(a, plan_id, call.volatility, call)
    }

    /// `with_step` whose plan walk reads `volatility` apart from its call's facts (a data-modifying CTE the plan reads is
    /// VOLATILE in the walk and the call's store, `SubPlanDesc::call`).
    fn with_walk(a: gravity_program::ArenaRef, plan_id: i32, volatility: Volatility, call: Found) -> PipelineGraph<'static> {
        use gravity_decompose::ops::shared::subplan::{SubForm, SubLink, SubPlanDesc, SubSite};
        use types_nodes::list::IntList;
        let nil: &NodeList<'_> = Box::leak(Box::new(NodeList::nil())); // alloc: Cold
        let ints: &IntList<'_> = Box::leak(Box::new(IntList::nil())); // alloc: Cold
        let d = SubPlanDesc { plan_id, link: SubLink::All, test: None, args: nil, params: ints, sets: ints, ids: ints, site: SubSite::Row, conditional: false, volatility, raise_free: call.raise_free, quiet_but_rows: false, invariant: false, unknown_eq_false: false, alternative: false, set_spool: false, stops: false, ahead: None, call, form: SubForm::Build };
        let mut g = PipelineGraph::new(a);
        let mut p = PipelineNode::new(a, 1, Source::Const, Pipeline::serial(-1, End::Sink, Reason::NoDrivingSite));
        p.push(gravity_decompose::Op::SubPlan(d));
        g.pipelines.push(p);
        g
    }

    /// A VOLATILE PL/pgSQL function's call facts (`f(y)`): user code that acts.
    const USER_F: Found = Found { volatility: Volatility::Volatile, raise_free: false, user_call: true, acts_past_call: true, order_call: true, acts: true, ..Found::CLEAN };

    /// D4138: a gate's qual `WHERE 0 <> ALL (SELECT f(y) FROM u)` (the qual is the SubPlan, its test `int4ne(0, $0)`, a
    /// builtin over parameters) calls user code by its step's facts: result.rs `qual_calls` (Gate::make's verdict, limit.rs)
    /// reads it in whole scope. Over a clean step the same qual calls nothing.
    #[test]
    fn a_gate_qual_reads_its_subplans_user_call() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let m = Box::leak(Box::new(mcx::MemoryContext::new("d4138-gate"))).mcx(); // alloc: Cold
        let arena: &'static Arena = Box::leak(Box::new(Arena::new(1 << 16))); // alloc: Cold
        let param = |id| Node::mk(m, Param { paramkind: ParamKind::PARAM_EXEC, paramid: id, paramtype: 23, ..Default::default() }).unwrap();
        let ne = Node::mk(m, OpExpr { opno: 518, opfuncid: 144, opresulttype: 16, args: NodeList::make2(m, param(0), param(1)).unwrap(), ..Default::default() }).unwrap();
        let qual = Node::mk(m, SubPlan { subLinkType: SubLinkType::ALL_SUBLINK, testexpr: Some(ne), plan_id: 1, ..Default::default() }).unwrap();
        for (call, want) in [(USER_F, true), (Found::CLEAN, false)] {
            let g = with_step(arena.handle(), 1, call);
            let gx = crate::graph::Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
            assert_eq!(crate::effects::visible::result::qual_calls(&gx, qual), want, "0 <> ALL (SubPlan 1) over {call:?}");
        }
    }

    /// D4138: an aggregate's argument `sum((SELECT f(id)))` (a TargetEntry over SubPlan 2) calls user code by its step's
    /// facts: group.rs `acts` (D2824's call-order refusal) reads each argument through `calls_whole`. Over a clean step it
    /// calls nothing.
    #[test]
    fn an_aggregate_argument_reads_its_subplans_user_call() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let m = Box::leak(Box::new(mcx::MemoryContext::new("d4138-agg"))).mcx(); // alloc: Cold
        let arena: &'static Arena = Box::leak(Box::new(Arena::new(1 << 16))); // alloc: Cold
        let sub = Node::mk(m, SubPlan { subLinkType: SubLinkType::EXPR_SUBLINK, plan_id: 2, ..Default::default() }).unwrap();
        let arg = Node::mk(m, TargetEntry { expr: sub, resno: 1, resname: None, ressortgroupref: 0, resorigtbl: 0, resorigcol: 0, resjunk: false }).unwrap();
        for (call, want) in [(USER_F, true), (Found::CLEAN, false)] {
            let g = with_step(arena.handle(), 2, call);
            let gx = crate::graph::Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
            assert_eq!(super::calls_whole(&gx, arg), want, "sum((SubPlan 2)) over {call:?}");
        }
    }

    /// D4143 read through the one SubPlan answer: a claim operand holding a SubPlan whose plan reads a data-modifying CTE
    /// (its walk VOLATILE, its call clean: the CTE is the call's store, D4137) is no acting operand, as PostgreSQL runs
    /// that CTE to its end whether or not the SubPlan runs; a SubPlan whose call acts (a VOLATILE PL/pgSQL function) is.
    /// The per-row neighbour reading (`effects`, per_row.rs `before`) reads the same answer.
    #[test]
    fn an_operand_reads_its_subplan_by_the_one_answer() {
        let rig = mcx::MemoryContext::new("GravityPlanRig");
        let m = Box::leak(Box::new(mcx::MemoryContext::new("d4143-operand"))).mcx(); // alloc: Cold
        let arena: &'static Arena = Box::leak(Box::new(Arena::new(1 << 16))); // alloc: Cold
        let sub = Node::mk(m, SubPlan { subLinkType: SubLinkType::EXPR_SUBLINK, plan_id: 3, ..Default::default() }).unwrap();
        for (vol, call, want) in [(Volatility::Volatile, Found::CLEAN, false), (Volatility::Volatile, USER_F, true), (Volatility::Immutable, Found::CLEAN, false)] {
            let g = with_walk(arena.handle(), 3, vol, call);
            let gx = crate::graph::Gx { graph: &g, facts: &NoFacts, catalog: &gravity_expr::PgCatalog, over: None, mcx: rig.mcx() };
            assert_eq!(crate::effects::visible::expr::operand(&gx, 0, sub), want, "(SubPlan 3), walk {vol:?}, call {call:?}");
            assert_eq!(super::acts(&gx, arena.handle(), &g.pipelines[0], sub), want, "per-row neighbour (SubPlan 3), walk {vol:?}, call {call:?}");
        }
    }
}
