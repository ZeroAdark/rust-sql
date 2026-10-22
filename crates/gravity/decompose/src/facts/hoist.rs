//! D2013 slice 2, Road A (main 2026-10-04T20:45:39Z): a SubPlan PostgreSQL evaluates inside a nest inner's index key over
//! the nest's parameters (an index scan's runtime key: ExecIndexEvalRuntimeKeys at the scan's rescan, an Append arm's at
//! its first read after ExecReScanAppend) or inside the run-time pruning step of the union that is the nest's inner
//! (ExecFindMatchingSubPlans at the Append's first read after the rescan). `attach` maps no SubPlan from an index qual or a
//! pruning step, so no pipeline evaluates one there. The value holding it (the key's comparison value or `= ANY` array, a
//! recheck clause's, where the planner may have chosen another of an AlternativeSubPlan's plans, a hashed one; the step's
//! expression) is hoisted onto the nest's outer row: each SubPlan in it becomes an `Op::SubPlan` (`SubSite::Row`,
//! unconditional) right before the Nest op (ops/subplan `map_hoisted`), and the value a NestLoopParam of the nest with a
//! synthetic PARAM_EXEC id past the statement's (`paramExecTypes.len() + plan_id - 1`), its nest parameters read as their
//! outer values; the key and the step read that parameter. Trunk's roads then serve the inner: a bare executor parameter
//! key, a pruning step over a nest parameter (lowerv4 rerun/inner_arms.rs `source`: quiet, so every arm runs and an arm
//! PostgreSQL prunes passes no row by its own key).
//! When (the effects home's `hoist_ok`): the hoisted value runs once per outer row that reaches the Nest op. A plain inner
//! index scan's runtime keys PostgreSQL also evaluates once per outer row, at every rescan (`exact`): same count, same row,
//! any value. An arm's key, a recheck (a btree walk never rechecks) or a pruning step PostgreSQL may evaluate fewer times
//! (a pruned arm, an arm past a LIMIT, a pruning that stops at a NULL key), so there the value must raise nothing
//! (VOLATILE counts are free: user ruling 10-04). Not hoisted (left in the key, which lowerv4 then refuses by its name): a
//! hashed SubPlan with parameters, an InitPlan-form, CTE or MULTIEXPR SubPlan, an AlternativeSubPlan, a SubPlan inside another's arguments or test, one a plan node outside the nest's inner
//! reads or an op of the inner evaluates, a value under a row comparison or an initial pruning step, a synthetic id
//! already taken. A pruning whose steps the hoist rewrote is no longer `partPruneInfos[index]`: its desc's index becomes
//! `HOISTED`, so no protocol runs it by index (rerun/inner_arms.rs `nest_prune` refuses it by D2013's name). Owner M8-NEST.
use gravity_program::ArenaVec;
use types_error::PgResult;
use types_nodes::list::NodeList;
use types_nodes::plannodes::{NestLoop, NestLoopParam, PartitionPruneInfo, PartitionedRelPruneInfo, PlannedStmt};
use types_nodes::primnodes::{Param, ParamKind, SubLinkType, SubPlan};
use types_nodes::{Node, NodeTag};

use crate::admit::Refusal;
use crate::graph::{IndexKey, KeyForm, Op, PipelineNode, Source};
use crate::join::JoinDesc;
use crate::ledger::Kind;
use crate::ops::shared::append::AppendDesc;
use crate::ops::shared::scan::BitmapPlan;
use crate::ops::shared::subplan::{SubForm, SubPlanDesc};
use crate::walk::{Builder, Open};

/// The values a NestLoop's parameters (`nestParams`) take from its outer row, in plan order: lowerv4 ops/subplan/per_row.rs
/// `order` reads a hoisted SubPlan's reader there.
pub fn nest_values<'a, 'p>(params: &'a NodeList<'p>) -> impl Iterator<Item = Node<'p>> + 'a {
    nest_params(params).map(|(_, v)| v)
}

/// A NestLoop's parameters (`nestParams`) in plan order, each as (the PARAM_EXEC id it sets, the outer row's value):
/// lowerv4 ops/nestloop/lower.rs compiles each value per outer row.
pub fn nest_params<'a, 'p>(params: &'a NodeList<'p>) -> impl Iterator<Item = (i32, Node<'p>)> + 'a {
    params.iter().filter_map(|n| n.as_nest_loop_param()).map(|p| (p.paramno, p.paramval))
}

/// The index of a pruning desc whose steps `plan` rewrote: its info is the hoist's, not the statement's `partPruneInfos`
/// entry, so nothing may run it by index.
pub const HOISTED: u32 = u32::MAX;

const NO_ROOM: &str = "a nest inner's hoisted key value the node allocator could not make (D2013)";

/// What `plan` hoisted: the nest's parameters with one NestLoopParam per hoisted value appended, and the SubPlans those
/// values hold (their arguments the outer row's values), each to map onto the outer row before the Nest op, with the
/// licence its value's evaluation there holds (construct C2: `SubPlanDesc::ahead`; `None` where it is denied).
pub(crate) struct Hoisted<'p> {
    pub params: &'p NodeList<'p>,
    pub subs: ArenaVec<(&'p SubPlan<'p>, Option<crate::effects::Licence>)>,
}

/// The index keys of source `s`: an index walk's, a bitmap tree's leaves'.
fn keys_mut<'p>(s: &mut Source<'p>, f: &mut dyn FnMut(&mut IndexKey<'p>)) {
    fn leaves<'p>(p: &mut BitmapPlan<'p>, f: &mut dyn FnMut(&mut IndexKey<'p>)) {
        crate::walk::stack_check(); // D3748
        match p {
            BitmapPlan::Index { keys, .. } => {
                for k in keys.iter_mut() {
                    f(k);
                }
            }
            BitmapPlan::And { plans, .. } | BitmapPlan::Or { plans, .. } => {
                for q in plans.iter_mut() {
                    leaves(q, f);
                }
            }
        }
    }
    match s {
        Source::Index { keys, .. } => {
            for k in keys.iter_mut() {
                f(k);
            }
        }
        Source::Bitmap { plan, .. } => leaves(plan, f),
        _ => {}
    }
}

fn recheck_mut<'a, 'p>(s: &'a mut Source<'p>) -> Option<&'a mut &'p NodeList<'p>> {
    match s {
        Source::Index { recheck, .. } | Source::Bitmap { recheck, .. } => Some(recheck),
        _ => None,
    }
}

fn rel_infos<'p>(info: &'p PartitionPruneInfo<'p>) -> impl Iterator<Item = &'p PartitionedRelPruneInfo<'p>> {
    info.prune_infos.iter().filter_map(|l| l.as_list()).flat_map(|l| l.iter()).filter_map(|r| r.as_partitioned_rel_prune_info())
}

fn step_exprs<'p>(s: Node<'p>) -> impl Iterator<Item = Node<'p>> {
    s.as_partition_prune_step_op().into_iter().flat_map(|o| o.exprs.iter())
}

/// `v` into `values` beside its `bare` form (`key`: an index key reads it, at the scan's rescan) unless one whose bare
/// form is `equal` is there, which then takes `key` too.
fn add<'p>(mcx: mcx::Mcx<'p>, values: &mut ArenaVec<(Node<'p>, Node<'p>, bool)>, v: Node<'p>, key: bool) -> PgResult<()> {
    let vb = bare(mcx, v)?.unwrap_or(v);
    match values.iter_mut().find(|w| types_nodes::equal(w.1, vb)) {
        Some(w) => w.2 |= key,
        None => values.push((v, vb, key)),
    }
    Ok(())
}

/// `n` with every SubPlan's estimates (startup_cost, per_call_cost) zeroed, `None` when it holds no SubPlan: the form `add`
/// and `swap` compare. One value's copies (an arm's key, a pruning step) may differ only there, and no estimate may change
/// the graph (exec tests inputs_front_census).
fn bare<'p>(mcx: mcx::Mcx<'p>, n: Node<'p>) -> PgResult<Option<Node<'p>>> {
    crate::walk::stack_check(); // D3748
    let Some(s) = n.as_sub_plan() else { return nodes_core::expression_tree_mutator_dyn(mcx, n, &mut |y| bare(mcx, y)) };
    let testexpr = match s.testexpr {
        Some(t) => bare(mcx, t)?.or(Some(t)),
        None => None,
    };
    let mut args = NodeList::nil();
    for a in s.args.iter() {
        args.lappend(mcx, bare(mcx, a)?.unwrap_or(a))?;
    }
    let sp = SubPlan {
        subLinkType: s.subLinkType,
        testexpr,
        paramIds: s.paramIds.clone_in(mcx)?,
        plan_id: s.plan_id,
        plan_name: s.plan_name,
        firstColType: s.firstColType,
        firstColTypmod: s.firstColTypmod,
        firstColCollation: s.firstColCollation,
        useHashTable: s.useHashTable,
        unknownEqFalse: s.unknownEqFalse,
        parallel_safe: s.parallel_safe,
        setParam: s.setParam.clone_in(mcx)?,
        parParam: s.parParam.clone_in(mcx)?,
        args,
        startup_cost: 0.0,
        per_call_cost: 0.0,
    };
    Ok(Some(Node::mk(mcx, sp)?))
}

/// A recheck clause's side holding a SubPlan (an index clause's comparison value or `= ANY` array; `Err`: a clause of
/// another form holds one, or both sides do); `Ok(None)`: none.
fn recheck_value(c: Node<'_>) -> Result<Option<Node<'_>>, ()> {
    if !crate::expr::has_subplan(c) {
        return Ok(None);
    }
    let args = c.as_op_expr().map(|o| &o.args).or_else(|| c.as_scalar_array_op_expr().map(|o| &o.args)).filter(|a| a.len() == 2).ok_or(())?;
    match (crate::expr::has_subplan(args.nth(0)), crate::expr::has_subplan(args.nth(1))) {
        (false, true) => Ok(Some(args.nth(1))),
        (true, false) => Ok(Some(args.nth(0))),
        _ => Err(()),
    }
}

/// Plan subtree `n`, each node once (rerun.rs `each_plan_node`'s kids).
fn under<'p>(n: Node<'p>, f: &mut dyn FnMut(Node<'p>)) {
    crate::walk::stack_check(); // D3748
    f(n);
    let Some(p) = n.as_plan() else { return };
    for k in p.lefttree.into_iter().chain(p.righttree).chain(n.as_subquery_scan().and_then(|s| s.subplan)) {
        under(k, f);
    }
    let lists = [n.as_append().map(|a| &a.appendplans), n.as_merge_append().map(|a| &a.mergeplans), n.as_bitmap_and().map(|a| &a.bitmapplans), n.as_bitmap_or().map(|a| &a.bitmapplans), n.as_agg().map(|a| &a.chain)];
    for k in lists.into_iter().flatten().flat_map(|l| l.iter()) {
        under(k, f);
    }
}

/// Plan node `n`'s own expressions read SubPlan `id`: the lists `attach` maps from (`own_lists`), an index or bitmap scan's
/// clauses, an Append's or MergeAppend's pruning steps.
fn refers_node<'p>(stmt: &'p PlannedStmt<'p>, n: Node<'p>, id: i32) -> bool {
    let mut hit = false;
    crate::ops::subplan::own_lists(n, &mut |e| hit |= crate::ops::shared::subplan::refers(e, id));
    let list = |l: &NodeList<'_>| crate::ops::shared::subplan::refers_list(l, id);
    hit |= n.as_index_scan().is_some_and(|s| list(&s.indexqual) || list(&s.indexqualorig) || list(&s.indexorderby));
    hit |= n.as_index_only_scan().is_some_and(|s| list(&s.indexqual) || list(&s.recheckqual) || list(&s.indexorderby));
    hit |= n.as_bitmap_index_scan().is_some_and(|s| list(&s.indexqual) || list(&s.indexqualorig));
    hit |= n.as_bitmap_heap_scan().is_some_and(|s| list(&s.bitmapqualorig));
    let prune = n.as_append().map(|a| a.part_prune_index).or_else(|| n.as_merge_append().map(|a| a.part_prune_index));
    let info = prune.and_then(|i| usize::try_from(i).ok()).filter(|&i| i < stmt.partPruneInfos.len()).and_then(|i| stmt.partPruneInfos.nth(i).as_partition_prune_info());
    hit || info.is_some_and(|info| rel_infos(info).any(|r| r.initial_pruning_steps.iter().chain(r.exec_pruning_steps.iter()).flat_map(step_exprs).any(|e| crate::ops::shared::subplan::refers(e, id))))
}

/// Every plan node of the statement that reads SubPlan `id` is in subtree `inner`: no other nest hoists it, so its synthetic
/// id is the statement's one.
fn only_under<'p>(stmt: &'p PlannedStmt<'p>, inner: Node<'p>, id: i32) -> bool {
    let (mut all, mut mine) = (0usize, 0usize);
    crate::rerun::each_plan_node(stmt, &mut |x| all += usize::from(refers_node(stmt, x, id)));
    under(inner, &mut |x| mine += usize::from(refers_node(stmt, x, id)));
    all == mine
}

/// An op of pipeline `p` (an inner under it too) evaluates SubPlan `id`.
fn op_of(p: &PipelineNode<'_>, id: i32) -> bool {
    crate::walk::stack_check(); // D3748
    p.ops.iter().any(|op| match op {
        Op::SubPlan(d) if d.plan_id == id => true,
        Op::SubPlan(SubPlanDesc { form: SubForm::Nested { inner }, .. }) | Op::Join(JoinDesc::Nest { inner, .. }) => op_of(inner, id),
        _ => false,
    })
}

/// A nest of pipeline `p` (or of an inner under it) sets parameter `id`.
fn sets(p: &PipelineNode<'_>, id: i32) -> bool {
    crate::walk::stack_check(); // D3748
    p.ops.iter().any(|op| match op {
        Op::Join(JoinDesc::Nest { params, inner, .. }) => params.iter().any(|x| x.as_nest_loop_param().is_some_and(|n| n.paramno == id)) || sets(inner, id),
        Op::SubPlan(SubPlanDesc { form: SubForm::Nested { inner }, .. }) => sets(inner, id),
        _ => false,
    })
}

/// `n` with each sub-expression `equal` to a hoisted value as its parameter.
fn swap<'p>(mcx: mcx::Mcx<'p>, n: Node<'p>, lifts: &[(Node<'p>, Node<'p>)]) -> PgResult<Option<Node<'p>>> {
    crate::walk::stack_check(); // D3748
    if crate::expr::has_subplan(n) {
        let nb = bare(mcx, n)?.unwrap_or(n);
        if let Some(l) = lifts.iter().find(|l| types_nodes::equal(nb, l.0)) {
            return Ok(Some(l.1));
        }
    }
    nodes_core::expression_tree_mutator_dyn(mcx, n, &mut |y| swap(mcx, y, lifts))
}

fn swap_list<'p>(mcx: mcx::Mcx<'p>, l: &'p NodeList<'p>, lifts: &[(Node<'p>, Node<'p>)]) -> PgResult<Option<&'p NodeList<'p>>> {
    if l.is_nil() {
        return Ok(Some(l));
    }
    let mut out = NodeList::nil();
    for n in l.iter() {
        out.lappend(mcx, swap(mcx, n, lifts)?.unwrap_or(n))?;
    }
    Ok(Node::mk_list(mcx, out)?.as_list())
}

/// `n` with each of the nest's parameters read (`nest`: paramno, the outer value) as its outer value.
fn outer_of<'p>(mcx: mcx::Mcx<'p>, n: Node<'p>, nest: &[(i32, Node<'p>)]) -> PgResult<Option<Node<'p>>> {
    crate::walk::stack_check(); // D3748
    if let Some(p) = n.as_param().filter(|p| p.paramkind == ParamKind::PARAM_EXEC) {
        return Ok(nest.iter().find(|x| x.0 == p.paramid).map(|x| x.1));
    }
    nodes_core::expression_tree_mutator_dyn(mcx, n, &mut |y| outer_of(mcx, y, nest))
}

/// Pruning `info` with its exec steps' expressions swapped (`swap`); the rest as it was.
fn swap_info<'p>(mcx: mcx::Mcx<'p>, info: &'p PartitionPruneInfo<'p>, lifts: &[(Node<'p>, Node<'p>)]) -> PgResult<Option<&'p PartitionPruneInfo<'p>>> {
    let mut outer = NodeList::nil();
    for l in info.prune_infos.iter() {
        let Some(rl) = l.as_list() else {
            outer.lappend(mcx, l)?;
            continue;
        };
        let mut rels = NodeList::nil();
        for r in rl.iter() {
            let Some(ri) = r.as_partitioned_rel_prune_info() else {
                rels.lappend(mcx, r)?;
                continue;
            };
            let mut steps = NodeList::nil();
            for s in ri.exec_pruning_steps.iter() {
                steps.lappend(mcx, swap(mcx, s, lifts)?.unwrap_or(s))?;
            }
            let ri = PartitionedRelPruneInfo {
                rtindex: ri.rtindex,
                present_parts: ri.present_parts.clone_in(mcx)?,
                nparts: ri.nparts,
                subplan_map: ri.subplan_map,
                subpart_map: ri.subpart_map,
                leafpart_rti_map: ri.leafpart_rti_map,
                relid_map: ri.relid_map,
                initial_pruning_steps: ri.initial_pruning_steps.clone_in(mcx)?,
                exec_pruning_steps: steps,
                execparamids: ri.execparamids.clone_in(mcx)?,
            };
            rels.lappend(mcx, Node::mk(mcx, ri)?)?;
        }
        outer.lappend(mcx, Node::mk_list(mcx, rels)?)?;
    }
    let n = Node::mk(mcx, PartitionPruneInfo { relids: info.relids.clone_in(mcx)?, prune_infos: outer, other_subplans: info.other_subplans.clone_in(mcx)? })?;
    Ok(n.as_partition_prune_info())
}

/// The hoist of nest `nl`'s inner (`inner`, mapped since pipeline `p0`; its parameters `params`, its outer `outer`): the
/// inner's keys and pruning rewritten in place, or `None` when nothing is hoisted (module doc).
pub(crate) fn plan<'p>(b: &mut Builder<'p, '_>, nl: &'p NestLoop<'p>, params: &'p NodeList<'p>, p0: usize, inner: &mut Open<'p>, outer: &Open<'p>) -> Result<Option<Hoisted<'p>>, Refusal> {
    let (stmt, facts, arena) = (b.stmt, b.facts, b.arena);
    let (Some(mcx), Some(inner_plan)) = (b.alloc, nl.join.plan.righttree) else { return Ok(None) };
    let id = nl.join.plan.plan_node_id;
    let room = |_| Refusal::shape(Kind::NestLoop, id, NO_ROOM);
    // the union this inner emits (settled: its first op) and its feeds, else the inner's own source
    let slot = match inner.ops.first() {
        Some(Op::Append(AppendDesc::Union { slot, .. })) => Some(*slot),
        _ => None,
    };
    let mut feeds: ArenaVec<usize> = ArenaVec::new_in(arena); // alloc: Cold
    if let Some(s) = slot {
        feeds.extend((p0..b.graph.pipelines.len()).filter(|&f| matches!(b.graph.pipelines[f].ops.last(), Some(Op::Append(AppendDesc::Feed { slot, .. })) if *slot == s)));
    }
    let exact = slot.is_none() && matches!(inner.source, Source::Index { .. }) && matches!(inner_plan.node_tag(), NodeTag::T_IndexScan | NodeTag::T_IndexOnlyScan);
    // the values holding a SubPlan: a key's comparison value or `= ANY` array, a recheck clause's (the planner's choice of
    // an AlternativeSubPlan may differ there: a hashed one), an exec pruning step's expression
    let mut values: ArenaVec<(Node<'p>, Node<'p>, bool)> = ArenaVec::new_in(arena); // alloc: Cold
    let mut bad = false;
    let mut oom = None;
    {
        let mut scan = |s: &mut Source<'p>| {
            keys_mut(s, &mut |k| match k.form {
                _ if !crate::expr::has_subplan(k.value) => {}
                KeyForm::Op | KeyForm::Any => oom = add(mcx, &mut values, k.value, true).err().or(oom.take()),
                _ => bad = true,
            });
            if let Some(r) = recheck_mut(s) {
                let r: &'p NodeList<'p> = *r;
                for c in r.iter() {
                    match recheck_value(c) {
                        Ok(Some(v)) => oom = add(mcx, &mut values, v, false).err().or(oom.take()),
                        Ok(None) => {}
                        Err(()) => bad = true,
                    }
                }
            }
        };
        match slot {
            None => scan(&mut inner.source),
            Some(_) => {
                for &f in feeds.iter() {
                    scan(&mut b.graph.pipelines[f].source);
                }
            }
        }
    }
    let prune = match inner.ops.first() {
        Some(Op::Append(AppendDesc::Union { prune: Some(d), .. })) => Some(*d),
        _ => None,
    };
    let mut in_steps = false;
    for r in prune.into_iter().flat_map(|d| rel_infos(d.info)) {
        bad |= r.initial_pruning_steps.iter().flat_map(step_exprs).any(crate::expr::has_subplan);
        for e in r.exec_pruning_steps.iter().flat_map(step_exprs).filter(|&e| crate::expr::has_subplan(e)) {
            in_steps = true;
            oom = add(mcx, &mut values, e, false).err().or(oom.take());
        }
    }
    if let Some(e) = oom {
        return Err(room(e));
    }
    if bad || values.is_empty() {
        return Ok(None);
    }
    // each value's SubPlans, its synthetic id (its first SubPlan's), its effects
    let base = stmt.paramExecTypes.len() as i32;
    let mut lifts: ArenaVec<(Node<'p>, Node<'p>, i32, Option<crate::effects::Licence>)> = ArenaVec::new_in(arena); // alloc: Cold
    for &(v, vb, key) in values.iter() {
        let (mut ok, mut first) = (true, None);
        crate::expr::walk(v, &mut |x| match x.node_tag() {
            NodeTag::T_SubPlan => {
                let Some(s) = x.as_sub_plan() else { return false };
                let form = (!s.useHashTable || s.parParam.is_nil()) && s.setParam.is_nil() && !matches!(s.subLinkType, SubLinkType::CTE_SUBLINK | SubLinkType::MULTIEXPR_SUBLINK);
                let nested = s.testexpr.is_some_and(crate::expr::has_subplan) || s.args.iter().any(crate::expr::has_subplan);
                let own = !op_of(inner, s.plan_id) && !feeds.iter().any(|&f| op_of(&b.graph.pipelines[f], s.plan_id)) && only_under(stmt, inner_plan, s.plan_id);
                ok &= form && !nested && own;
                first.get_or_insert(s.plan_id);
                false
            }
            NodeTag::T_AlternativeSubPlan => {
                ok = false;
                false
            }
            _ => ok,
        });
        let Some(first) = first.filter(|_| ok) else { return Ok(None) };
        let pid = base + first - 1;
        let e = crate::effects::Effects::of_found(crate::rerun::of_expr(stmt, facts, arena, v));
        let taken = lifts.iter().any(|l| l.2 == pid) || sets(outer, pid) || sets(inner, pid) || b.graph.pipelines.iter().any(|p| sets(p, pid));
        if !crate::effects::hoist_ok(e, exact && key) || taken || crate::catalog::expr_type::type_of(v).is_none() {
            return Ok(None);
        }
        // construct C2: the licence of the value's evaluation on the outer row, once for each row that reaches the Nest
        // op, where PostgreSQL evaluates it at the inner's rescan (`exact`: the same calls) or only for a row that
        // reaches its arm (more calls here, which the user's VOLATILE-count ruling frees): the whole-read licence
        // (`effects::whole_read_local`), granted to acting code only through call-local builtins, with `call_local`'s
        // proof over the value in a statement that locks no row (`call_local` denies a write, not a row mark). No
        // reader guard (`Px::whole_read`) is asked: none would add the calls an unreached arm skips. Asked once the
        // hoist is decided, which it does not change: a value denied it is hoisted as before, with none
        let cx = crate::effects::Cx::new(stmt, facts, arena);
        let local = (e.acts && stmt.rowMarks.iter().next().is_none()).then(|| crate::effects::call_local(&cx, core::iter::once(v))).flatten();
        lifts.push((v, vb, pid, crate::effects::whole_read_local(e, local).ok()));
    }
    // the parameters: each value as the outer row computes it (the nest's parameters read as their outer values)
    let nest: ArenaVec<(i32, Node<'p>)> = {
        let mut n = ArenaVec::new_in(arena); // alloc: Cold
        n.extend(params.iter().filter_map(|p| p.as_nest_loop_param()).map(|p| (p.paramno, p.paramval)));
        n
    };
    let mut all = NodeList::nil();
    for p in params.iter() {
        all.lappend(mcx, p).map_err(room)?;
    }
    let mut swaps: ArenaVec<(Node<'p>, Node<'p>)> = ArenaVec::new_in(arena); // alloc: Cold
    let mut subs: ArenaVec<(&'p SubPlan<'p>, Option<crate::effects::Licence>)> = ArenaVec::new_in(arena); // alloc: Cold
    for &(v, vb, pid, ahead) in lifts.iter() {
        let val = outer_of(mcx, v, &nest).map_err(room)?.unwrap_or(v);
        crate::expr::walk(val, &mut |x| {
            if let Some(s) = x.as_sub_plan() {
                subs.push((s, ahead));
            }
            x.as_sub_plan().is_none()
        });
        let ty = crate::catalog::expr_type::type_of(v).unwrap_or(0);
        let param = Node::mk(mcx, Param { paramkind: ParamKind::PARAM_EXEC, paramid: pid, paramtype: ty, paramtypmod: nodes_core::expr_typmod(v), paramcollid: crate::catalog::expr_type::collation_of(v), location: -1 }).map_err(room)?;
        all.lappend(mcx, Node::mk(mcx, NestLoopParam { paramno: pid, paramval: val }).map_err(room)?).map_err(room)?;
        swaps.push((vb, param));
    }
    // the inner reads the parameters: its keys, its rechecks, its pruning
    let mut err = None;
    let mut put = |s: &mut Source<'p>| {
        keys_mut(s, &mut |k| match swap(mcx, k.value, &swaps) {
            Ok(v) => k.value = v.unwrap_or(k.value),
            Err(e) => err = Some(e),
        });
        if let Some(r) = recheck_mut(s) {
            match swap_list(mcx, r, &swaps) {
                Ok(l) => *r = l.unwrap_or(*r),
                Err(e) => err = Some(e),
            }
        }
    };
    match slot {
        None => put(&mut inner.source),
        Some(_) => {
            for &f in feeds.iter() {
                put(&mut b.graph.pipelines[f].source);
            }
        }
    }
    if let Some(e) = err {
        return Err(room(e));
    }
    if let (true, Some(Op::Append(AppendDesc::Union { prune: Some(d), .. }))) = (in_steps, inner.ops.first_mut()) {
        d.info = swap_info(mcx, d.info, &swaps).map_err(room)?.ok_or(Refusal::shape(Kind::NestLoop, id, NO_ROOM))?;
        d.index = HOISTED;
    }
    let params = Node::mk_list(mcx, all).map_err(room)?.as_list().ok_or(Refusal::shape(Kind::NestLoop, id, NO_ROOM))?;
    Ok(Some(Hoisted { params, subs }))
}
