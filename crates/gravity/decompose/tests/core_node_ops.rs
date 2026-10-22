// ENTRY E1 (v1's `Pipeline::node_ops`): every plan node the executed tree holds is listed by some pipeline's `node_ops`
// (a pushed pipeline or an embedded inner), `producer_top` entries aside; each list starts with the pipeline's own node and
// its `first_op`s never go back or past the ops. The executed tree: the plan tree and every subplan an InitPlan or a
// SubPlan op runs. A node listed by several pipelines outside a streamed union's arms is reported, not failed: a
// breaker's producer / consumer pair is the one expected repeat, and it is marked `producer_top`.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use gravity_decompose::ops::shared::append::AppendDesc;
use gravity_decompose::ops::shared::agg::AGG_PLAIN;
use gravity_decompose::graph::{Op, PipelineGraph, PipelineNode};
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::subplan::SubForm;
use gravity_decompose::{corpus, decompose_in};
use gravity_program::Arena;
use mcx::MemoryContext;
use types_nodes::Node;

mod harness;

const OPTIONAL: &str = "optional";

/// normalize's `distinct_count` (landing 7, DC-21): a plain Agg whose every output is a bare `count(*)`, over (a
/// SubqueryScan over) a grouping (a grouped Agg, a Group, a Unique), may count the distinct keys of the grouping's input
/// below its Sorts, so the SubqueryScan, the grouping and its Sorts may never run (such a plan's EXPLAIN ANALYZE delegates:
/// admit explain.rs counts an Aggregate only at the root over a leaf). Those nodes, and the node the rewrite reads.
fn counted_grouping(n: Node<'_>) -> Option<(Vec<i32>, Node<'_>)> {
    let a = n.as_agg().filter(|a| a.aggstrategy == AGG_PLAIN && a.numCols == 0)?;
    let mut outs = a.plan.targetlist.iter().filter_map(|t| t.as_target_entry());
    if a.plan.targetlist.is_nil() || !outs.all(|t| t.expr.as_aggref().is_some_and(|r| r.aggstar)) {
        return None;
    }
    let mut skipped = Vec::new();
    let mut c = a.plan.lefttree?;
    if let Some(s) = c.as_subquery_scan() {
        skipped.push(s.scan.plan.plan_node_id);
        c = s.subplan?;
    }
    let grouping = c.as_agg().is_some_and(|g| g.numCols >= 1 && g.groupingSets.is_nil()) || c.as_group().is_some() || c.as_unique().is_some();
    let inner = c.as_plan().filter(|_| grouping)?;
    skipped.push(inner.plan_node_id);
    let mut below = inner.lefttree?;
    while let Some(s) = below.as_sort() {
        skipped.push(s.plan.plan_node_id);
        below = s.plan.lefttree?;
    }
    Some((skipped, below))
}

fn plan_nodes(n: Node<'_>, out: &mut BTreeMap<i32, String>) {
    let Some(p) = n.as_plan() else { return };
    out.insert(p.plan_node_id, format!("{:?}", n.node_tag()));
    if let Some((skipped, below)) = counted_grouping(n) {
        for id in skipped {
            out.insert(id, OPTIONAL.into());
        }
        return plan_nodes(below, out);
    }
    for c in [p.lefttree, p.righttree].into_iter().flatten() {
        // the grouping-sets rewrite reads a rollup's input below its Sort: the Sort never runs, but D1061's spool of a
        // VOLATILE input reads it through the Sort (PostgreSQL's first phase), so the Sort may be listed
        let skip = n.as_agg().is_some_and(|a| !a.groupingSets.is_nil()) && c.as_sort().is_some();
        match c.as_sort().filter(|_| skip).and_then(|s| s.plan.lefttree.map(|below| (s, below))) {
            Some((s, below)) => {
                out.insert(s.plan.plan_node_id, OPTIONAL.into());
                plan_nodes(below, out)
            }
            None => plan_nodes(c, out),
        }
    }
    let lists = [n.as_append().map(|a| &a.appendplans), n.as_merge_append().map(|m| &m.mergeplans), n.as_bitmap_and().map(|b| &b.bitmapplans), n.as_bitmap_or().map(|b| &b.bitmapplans)];
    for l in lists.into_iter().flatten() {
        for c in l.iter() {
            plan_nodes(c, out);
        }
    }
    if let Some(s) = n.as_subquery_scan().and_then(|s| s.subplan) {
        plan_nodes(s, out);
    }
}

/// Every pipeline, embedded inners included, with whether it is a streamed union's arm.
fn walk<'a, 'p>(p: &'a PipelineNode<'p>, out: &mut Vec<&'a PipelineNode<'p>>, subplans: &mut BTreeSet<i32>) {
    out.push(p);
    for op in p.ops.iter() {
        match op {
            Op::Join(JoinDesc::Nest { inner, .. }) => walk(inner, out, subplans),
            Op::SubPlan(d) => {
                subplans.insert(d.plan_id);
                if let SubForm::Nested { inner } = &d.form {
                    walk(inner, out, subplans);
                }
            }
            _ => {}
        }
    }
}

fn check(g: &PipelineGraph<'_>, stmt: &types_nodes::plannodes::PlannedStmt<'_>) -> (Vec<String>, Vec<String>) {
    let (mut bad, mut repeats) = (Vec::new(), Vec::new());
    let mut all = Vec::new();
    let mut subplans: BTreeSet<i32> = g.init_plans.iter().map(|i| i.plan_id).collect();
    for p in g.pipelines.iter() {
        walk(p, &mut all, &mut subplans);
    }
    let mut want = BTreeMap::new();
    if let Some(t) = stmt.planTree {
        plan_nodes(t, &mut want);
    }
    for id in subplans.iter() {
        if let Some(t) = (*id as usize).checked_sub(1).filter(|&i| i < stmt.subplans.len()).and_then(|i| stmt.subplans.nth(i)) {
            plan_nodes(t, &mut want);
        }
    }
    let (mut plain, mut armed): (BTreeMap<i32, usize>, BTreeSet<i32>) = (BTreeMap::new(), BTreeSet::new());
    for (k, p) in all.iter().enumerate() {
        let Some(first) = p.node_ops.first() else {
            bad.push(format!("pipeline {k} (node {}) has no node_ops", p.node_id));
            continue;
        };
        // a bitmap heap scan's subtree nodes come before it (they run inside the source)
        let own = p.node_ops.iter().find(|n| !matches!(p.source, gravity_decompose::graph::Source::Bitmap { .. }) || n.node_id == p.node_id).unwrap_or(first);
        if own.node_id != p.node_id {
            bad.push(format!("pipeline {k}: node_ops[0] is n{}, the pipeline's node is n{}", own.node_id, p.node_id));
        }
        let mut at = 0;
        for n in p.node_ops.iter() {
            if n.first_op < at || n.first_op as usize > p.ops.len() {
                bad.push(format!("pipeline {k}: n{} first_op {} out of order (ops {})", n.node_id, n.first_op, p.ops.len()));
            }
            at = n.first_op;
        }
        let arm = p.ops.iter().any(|o| matches!(o, Op::Append(AppendDesc::Arm { .. })));
        for n in p.node_ops.iter().filter(|n| !n.producer_top) {
            if arm {
                armed.insert(n.node_id);
            } else {
                *plain.entry(n.node_id).or_default() += 1;
            }
        }
    }
    let mut seen = plain.clone();
    for id in armed.iter() {
        *seen.entry(*id).or_default() += 1;
    }
    for (id, tag) in want.iter().filter(|(_, t)| t.as_str() != OPTIONAL) {
        match seen.get(id) {
            None => bad.push(format!("n{id} ({tag}) is in no pipeline's node_ops")),
            Some(&c) if c > 1 => repeats.push(format!("n{id} ({tag}) listed by {c} pipelines")),
            _ => {}
        }
    }
    for id in seen.keys().filter(|id| !want.contains_key(id)) {
        bad.push(format!("n{id} is listed but not in the executed tree"));
    }
    (bad, repeats)
}

#[test]
fn every_executed_plan_node_is_in_a_pipelines_node_ops() {
    let mut files = harness::fixtures_in(&harness::plans_root());
    files.extend(harness::fixtures_in(&harness::plans_root().join("regress-full")));
    let res = harness::par_map(&files, |f| {
        let text = fs::read_to_string(f).unwrap();
        let ctx = MemoryContext::new("fixture");
        let Ok(fx) = corpus::load(ctx.mcx(), &text) else { return None };
        let arena = Arena::new(1 << 20);
        let Ok(g) = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx) else { return None };
        let (bad, rep) = check(&g, fx.stmt);
        Some((fx.name.clone(), bad, rep))
    });
    let (mut graphs, mut bad, mut rep) = (0, Vec::new(), Vec::new());
    for (f, b, r) in res.into_iter().flatten() {
        graphs += 1;
        bad.extend(b.into_iter().map(|x| format!("{f}: {x}")));
        rep.extend(r.into_iter().map(|x| format!("{f}: {x}")));
    }
    println!("node_ops: {graphs} graphs, {} violations, {} repeats outside arms", bad.len(), rep.len());
    for x in bad.iter() {
        println!("violation: {x}");
    }
    for x in rep.iter() {
        println!("repeat: {x}");
    }
    assert!(bad.is_empty(), "{} node_ops violations", bad.len());
    assert!(graphs > 5000, "only {graphs} graphs");
}
