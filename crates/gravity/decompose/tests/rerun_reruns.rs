// D240: D4's rerun sets against PostgreSQL's rescan. When an outer row changes a parameter, ExecReScan's
// UpdateChangedParamSet marks every node whose `allParam` meets it (chgParam), and a marked node never keeps its rows:
// a scan restarts with the new value, a Material / Sort / Hash / Agg over it rebuilds, an Append rescans every child
// (ExecReScanAppend), an InitPlan whose extParam meets it is recomputed (a CTE's rows with it). So every pushed
// pipeline that runs a node of a nest's inner (a nested SubPlan's tree; the InitPlans attached in either) whose
// `allParam` meets the nest's `nestParams` (the SubPlan's `parParam`) must be in that owner's rerun set; outside it the
// pipeline runs once per statement and the embedded inner reads the first outer row's rows for every row. A union
// kept in its Materialize form lists the Append as each arm's `producer_top`: the Append's `allParam` holds every
// child's, so every arm reruns, as PG rescans every child.
//
// D241: a recursive union's generation is the same rule with `wtParam`: ExecRecursiveUnion adds `wtParam` to the
// recursive term's chgParam after every generation, so the term's pipelines that run a node whose `allParam` holds it
// (the WorkTableScan and everything above it), and the pipelines they feed, re-run per generation; the rest of the term
// (a Hash / Material / Sort over input with no chgParam: ExecReScanHashJoin keeps a one-batch table) runs once. The
// iteration's re-run set must be exactly that set, and a recursive union is never refused for having it.
//
// The model reads only the plan (`allParam`, `nestParams`, `parParam`, `wtParam`) and the finished graph (`node_ops`,
// `reruns`, the `Iter` op).
use std::collections::BTreeSet;
use std::fs;

use gravity_decompose::graph::{Op, PipelineGraph, PipelineNode};
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::setop::{IterDesc, SetDesc};
use gravity_decompose::ops::shared::subplan::SubForm;
use gravity_decompose::{corpus, decompose_in, Kind};
use gravity_program::Arena;
use mcx::MemoryContext;
use types_nodes::plannodes::PlannedStmt;
use types_nodes::Node;

mod harness;

/// The children PG's rescan reaches from `n`: lefttree, righttree, Append / MergeAppend / BitmapAnd / BitmapOr children,
/// a SubqueryScan's subplan, and the tree of every InitPlan attached at `n` (ExecReScan marks an InitPlan whose extParam
/// meets chgParam for recomputation; a CTE's rows are recomputed with it).
fn children<'p>(n: Node<'p>, stmt: &PlannedStmt<'p>) -> Vec<Node<'p>> {
    let Some(p) = n.as_plan() else { return Vec::new() };
    let mut out: Vec<Node<'p>> = [p.lefttree, p.righttree].into_iter().flatten().collect();
    let lists = [n.as_append().map(|a| &a.appendplans), n.as_merge_append().map(|m| &m.mergeplans), n.as_bitmap_and().map(|b| &b.bitmapplans), n.as_bitmap_or().map(|b| &b.bitmapplans)];
    for l in lists.into_iter().flatten() {
        out.extend(l.iter());
    }
    out.extend(n.as_subquery_scan().and_then(|s| s.subplan));
    out.extend(p.initPlan.iter().filter_map(|i| i.as_sub_plan()).filter_map(|s| subplan_tree(stmt, s.plan_id)));
    out
}

/// The nodes of the executed tree under `n` whose `allParam` meets `params`.
fn readers<'p>(n: Node<'p>, stmt: &PlannedStmt<'p>, params: &[i32], out: &mut BTreeSet<i32>) {
    let Some(p) = n.as_plan() else { return };
    if p.allParam.overlap_list(params) {
        out.insert(p.plan_node_id);
    }
    for c in children(n, stmt) {
        readers(c, stmt, params, out);
    }
}

/// An owner of a rerun set: a parameterised nest (`plan_id` 0, `node_id` the NestLoop) or a nested SubPlan
/// (`plan_id`), with the nodes of its inner that read the parameters it sets per outer row.
struct Owner {
    node_id: i32,
    plan_id: i32,
    readers: BTreeSet<i32>,
}

/// Every parameterised NestLoop of the executed tree under `n`.
fn nests<'p>(n: Node<'p>, stmt: &PlannedStmt<'p>, out: &mut Vec<Owner>) {
    let Some(p) = n.as_plan() else { return };
    if let Some(nl) = n.as_nest_loop() {
        let params: Vec<i32> = nl.nestParams.iter().filter_map(|x| x.as_nest_loop_param()).map(|x| x.paramno).collect();
        if let Some(inner) = nl.join.plan.righttree.filter(|_| !params.is_empty()) {
            let mut r = BTreeSet::new();
            readers(inner, stmt, &params, &mut r);
            out.push(Owner { node_id: p.plan_node_id, plan_id: 0, readers: r });
        }
    }
    for c in children(n, stmt) {
        nests(c, stmt, out);
    }
}

/// The nested SubPlans the graph runs (pushed pipelines and embedded inners): plan id and `parParam`.
fn nested(p: &PipelineNode<'_>, out: &mut Vec<(i32, Vec<i32>)>) {
    for op in p.ops.iter() {
        match op {
            Op::Join(JoinDesc::Nest { inner, .. }) => nested(inner, out),
            Op::SubPlan(d) => {
                if let SubForm::Nested { inner } = &d.form {
                    out.push((d.plan_id, d.params.iter().collect()));
                    nested(inner, out);
                }
            }
            _ => {}
        }
    }
}

fn subplan_tree<'p>(stmt: &PlannedStmt<'p>, plan_id: i32) -> Option<Node<'p>> {
    (plan_id as usize).checked_sub(1).filter(|&i| i < stmt.subplans.len()).and_then(|i| stmt.subplans.nth(i))
}

/// A recursive union: its `wtParam`, the nodes of its recursive term, and those whose `allParam` holds `wtParam`.
struct Recursion {
    node_id: i32,
    wt_param: i32,
    term: BTreeSet<i32>,
    readers: BTreeSet<i32>,
}

fn subtree<'p>(n: Node<'p>, stmt: &PlannedStmt<'p>, out: &mut BTreeSet<i32>) {
    let Some(p) = n.as_plan() else { return };
    out.insert(p.plan_node_id);
    for c in children(n, stmt) {
        subtree(c, stmt, out);
    }
}

/// Every recursive union of the executed tree under `n`.
fn recursions<'p>(n: Node<'p>, stmt: &PlannedStmt<'p>, out: &mut Vec<Recursion>) {
    let Some(p) = n.as_plan() else { return };
    if let Some(term) = n.as_recursive_union().and_then(|ru| ru.plan.righttree.map(|t| (ru.wtParam, t))) {
        let (mut t, mut r) = (BTreeSet::new(), BTreeSet::new());
        subtree(term.1, stmt, &mut t);
        readers(term.1, stmt, &[term.0], &mut r);
        out.push(Recursion { node_id: p.plan_node_id, wt_param: term.0, term: t, readers: r });
    }
    for c in children(n, stmt) {
        recursions(c, stmt, out);
    }
}

/// The iterations the graph runs (pushed pipelines and embedded inners).
fn iters<'a, 'p>(p: &'a PipelineNode<'p>, out: &mut Vec<&'a IterDesc>) {
    for op in p.ops.iter() {
        match op {
            Op::SetOp(SetDesc::Iter(it)) => out.push(it),
            Op::Join(JoinDesc::Nest { inner, .. }) => iters(inner, out),
            Op::SubPlan(d) => {
                if let SubForm::Nested { inner } = &d.form {
                    iters(inner, out);
                }
            }
            _ => {}
        }
    }
}

/// The pipelines the driver re-runs per generation.
fn reran(it: &IterDesc) -> Vec<u16> {
    it.reruns.to_vec()
}

/// PG's re-run set of a recursion: the pushed pipelines after the work table's first fill (the non-recursive term) up to
/// the body that run a node of its term, in index order, each in when it runs a node reading `wtParam`, reads a slot a
/// set pipeline filled last, or refills a slot the set uses. (A CTE over the bare work table is folded onto the work
/// slot: its WorkTableScan is listed by the slot's first producer, which runs once.)
fn generation_set(g: &PipelineGraph<'_>, r: &Recursion, it: &IterDesc) -> BTreeSet<usize> {
    let body = it.body as usize;
    let first = (0..body).rev().find(|&j| g.pipelines[j].desc.produces() == Some(it.work)).map_or(0, |j| j + 1);
    let mut set = BTreeSet::new();
    let mut slots = BTreeSet::new();
    for (i, p) in g.pipelines.iter().enumerate().take(body + 1).skip(first) {
        if !p.node_ops.iter().any(|n| r.term.contains(&n.node_id)) {
            continue;
        }
        let reads = p.node_ops.iter().any(|n| r.readers.contains(&n.node_id));
        let fed = p.desc.consumes.iter().any(|s| (0..i).rev().find(|&j| g.pipelines[j].desc.produces() == Some(*s)).is_some_and(|j| set.contains(&j)));
        let refills = p.desc.produces().is_some_and(|s| slots.contains(&s));
        if reads || fed || refills {
            set.insert(i);
            slots.extend(p.desc.consumes.iter().copied().chain(p.desc.produces()));
        }
    }
    set
}

/// The iteration faults of one graph: a recursion whose driver re-runs another set than PG's.
fn iteration_faults(g: &PipelineGraph<'_>, stmt: &PlannedStmt<'_>) -> (usize, Vec<String>) {
    let mut rs = Vec::new();
    if let Some(t) = stmt.planTree {
        recursions(t, stmt, &mut rs);
    }
    let mut subs = Vec::new();
    for p in g.pipelines.iter() {
        nested(p, &mut subs);
    }
    let mut trees: BTreeSet<i32> = g.init_plans.iter().map(|i| i.plan_id).collect();
    trees.extend(subs.iter().map(|s| s.0));
    for id in trees.iter() {
        if let Some(t) = subplan_tree(stmt, *id) {
            recursions(t, stmt, &mut rs);
        }
    }
    let mut seen = BTreeSet::new();
    rs.retain(|r| seen.insert(r.node_id));
    let mut its = Vec::new();
    for p in g.pipelines.iter() {
        iters(p, &mut its);
    }
    let mut bad = Vec::new();
    for r in rs.iter() {
        let Some(it) = its.iter().find(|it| it.wt_param == r.wt_param) else {
            bad.push(format!("recursive union n{} (wtParam {}) has no iteration", r.node_id, r.wt_param));
            continue;
        };
        let want = generation_set(g, r, it);
        let got: BTreeSet<usize> = reran(it).into_iter().map(usize::from).collect();
        for i in want.difference(&got) {
            bad.push(format!("p{i} re-runs per generation of n{} in PG (a node reading wtParam {}, or fed by one) but the driver runs it once", r.node_id, r.wt_param));
        }
        for i in got.difference(&want) {
            bad.push(format!("p{i} is re-run per generation of n{} but PG keeps its rows (no node of it reads wtParam {})", r.node_id, r.wt_param));
        }
    }
    (rs.len(), bad)
}

/// The pushed pipelines that run a node reading an owner's parameters but are in none of its rerun sets.
fn missing(g: &PipelineGraph<'_>, stmt: &PlannedStmt<'_>) -> (usize, Vec<String>) {
    let mut owners = Vec::new();
    if let Some(t) = stmt.planTree {
        nests(t, stmt, &mut owners);
    }
    let mut subs = Vec::new();
    for p in g.pipelines.iter() {
        nested(p, &mut subs);
    }
    // the trees no plan node's children reach: an InitPlan at the statement's root, a nested SubPlan's
    let mut trees: BTreeSet<i32> = g.init_plans.iter().map(|i| i.plan_id).collect();
    trees.extend(subs.iter().map(|s| s.0));
    for id in trees.iter() {
        if let Some(t) = subplan_tree(stmt, *id) {
            nests(t, stmt, &mut owners);
        }
    }
    // a tree both walks reached (an InitPlan attached below the root): its nests once
    let mut seen = BTreeSet::new();
    owners.retain(|o| seen.insert(o.node_id));
    for (plan_id, params) in subs.iter().filter(|s| !s.1.is_empty()) {
        if let Some(t) = subplan_tree(stmt, *plan_id) {
            let mut r = BTreeSet::new();
            readers(t, stmt, params, &mut r);
            owners.push(Owner { node_id: -1, plan_id: *plan_id, readers: r });
        }
    }
    let mut bad = Vec::new();
    for o in owners.iter() {
        let owns = |r: &&gravity_decompose::graph::Rerun| if o.plan_id == 0 { r.plan_id == 0 && r.node_id == o.node_id } else { r.plan_id == o.plan_id };
        for (i, p) in g.pipelines.iter().enumerate() {
            let Some(n) = p.node_ops.iter().find(|n| o.readers.contains(&n.node_id)) else { continue };
            if !g.reruns.iter().filter(owns).any(|r| r.pipelines.contains(&(i as u16))) {
                let who = if o.plan_id == 0 { format!("nest n{}", o.node_id) } else { format!("subplan {}", o.plan_id) };
                bad.push(format!("p{i} runs n{} (reads the parameters of {who}) but is in no rerun set of it", n.node_id));
            }
        }
    }
    (owners.len(), bad)
}

#[test]
fn every_pipeline_reading_an_outer_rows_parameter_reruns_per_outer_row() {
    let mut files = harness::fixtures_in(&harness::plans_root());
    files.extend(harness::fixtures_in(&harness::plans_root().join("regress-full")));
    let res = harness::par_map(&files, |f| {
        let text = fs::read_to_string(f).unwrap();
        let ctx = MemoryContext::new("fixture");
        let Ok(fx) = corpus::load(ctx.mcx(), &text) else { return None };
        let arena = Arena::new(1 << 20);
        let Ok(g) = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx) else { return None };
        let (owners, bad) = missing(&g, fx.stmt);
        Some((harness::fixture_name(f), owners, bad))
    });
    let (mut graphs, mut owners, mut hit, mut bad) = (0, 0, 0, Vec::new());
    for (f, o, b) in res.into_iter().flatten() {
        graphs += 1;
        owners += o;
        hit += usize::from(!b.is_empty());
        bad.extend(b.into_iter().map(|x| format!("{f}: {x}")));
    }
    println!("reruns: {graphs} graphs, {owners} parameterised owners, {hit} graphs with {} pipelines outside their rerun set", bad.len());
    for x in bad.iter() {
        println!("missing: {x}");
    }
    assert!(bad.is_empty(), "{} pipelines read an outer row's parameter but run once per statement", bad.len());
    assert!(graphs > 5000 || harness::filter().is_some(), "only {graphs} graphs");
}

#[test]
fn every_recursive_term_reruns_what_pg_reruns_per_generation() {
    let mut files = harness::fixtures_in(&harness::plans_root());
    files.extend(harness::fixtures_in(&harness::plans_root().join("regress-full")));
    let res = harness::par_map(&files, |f| {
        let text = fs::read_to_string(f).unwrap();
        let ctx = MemoryContext::new("fixture");
        let Ok(fx) = corpus::load(ctx.mcx(), &text) else { return None };
        let arena = Arena::new(1 << 20);
        let name = harness::fixture_name(f);
        let g = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx);
        let out = match &g {
            Ok(g) => {
                let (n, bad) = iteration_faults(g, fx.stmt);
                Some((name, n, bad))
            }
            // a recursive term the graph cannot re-run is the same fault, refused
            Err(r) if r.kind == Kind::RecursiveUnion => Some((name, 1, vec![format!("refused: {r}")])),
            Err(_) => None,
        };
        out
    });
    let (mut graphs, mut recursions, mut hit, mut bad) = (0, 0, 0, Vec::new());
    for (f, n, b) in res.into_iter().flatten() {
        graphs += 1;
        recursions += n;
        hit += usize::from(!b.is_empty());
        bad.extend(b.into_iter().map(|x| format!("{f}: {x}")));
    }
    println!("generations: {graphs} graphs, {recursions} recursive unions, {hit} graphs with {} faults", bad.len());
    for x in bad.iter() {
        println!("fault: {x}");
    }
    assert!(bad.is_empty(), "{} recursive-term faults", bad.len());
    assert!(graphs > 5000 || harness::filter().is_some(), "only {graphs} graphs");
}
