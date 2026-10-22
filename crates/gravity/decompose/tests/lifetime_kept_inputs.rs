// The keep rule's recorded answers (`graph.kept_by`, lifetime::KeptRec) over every plan fixture: one answer per owner and
// pipeline, and an answer that says PG's rescan reaches the pipeline (kept by PG's rule, by its usual answer, or as a quiet
// re-run) names a pipeline that runs a node of the owner's inner, or of a CTE a CteScan of the inner reads. The counts per answer are printed: they are what a
// lowering guard that reads `kept_input` sees.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use gravity_decompose::graph::PipelineGraph;
use gravity_decompose::lifetime::{self, KeptBy};
use gravity_decompose::{corpus, decompose_in};
use gravity_program::Arena;
use mcx::MemoryContext;
use types_nodes::plannodes::PlannedStmt;
use types_nodes::Node;

mod harness;

fn subplan_tree<'p>(stmt: &PlannedStmt<'p>, plan_id: i32) -> Option<Node<'p>> {
    (plan_id as usize).checked_sub(1).filter(|&i| i < stmt.subplans.len()).and_then(|i| stmt.subplans.nth(i))
}

/// The plan nodes under `n` (its children, Append and BitmapAnd / BitmapOr lists, a SubqueryScan's subplan, an Agg chain).
fn subtree(n: Node<'_>, out: &mut BTreeSet<i32>) {
    let Some(p) = n.as_plan() else { return };
    out.insert(p.plan_node_id);
    let mut kids: Vec<Node<'_>> = [p.lefttree, p.righttree].into_iter().flatten().collect();
    let lists = [n.as_append().map(|a| &a.appendplans), n.as_merge_append().map(|m| &m.mergeplans), n.as_bitmap_and().map(|b| &b.bitmapplans), n.as_bitmap_or().map(|b| &b.bitmapplans), n.as_agg().map(|a| &a.chain)];
    for l in lists.into_iter().flatten() {
        kids.extend(l.iter());
    }
    kids.extend(n.as_subquery_scan().and_then(|s| s.subplan));
    kids.into_iter().for_each(|k| subtree(k, out));
}

/// The CTEs (their plan ids) a CteScan under `n` reads.
fn cte_ids(n: Node<'_>, out: &mut BTreeSet<i32>) {
    let Some(p) = n.as_plan() else { return };
    out.extend(n.as_cte_scan().map(|c| c.ctePlanId));
    let mut kids: Vec<Node<'_>> = [p.lefttree, p.righttree].into_iter().flatten().collect();
    let lists = [n.as_append().map(|a| &a.appendplans), n.as_merge_append().map(|m| &m.mergeplans), n.as_bitmap_and().map(|b| &b.bitmapplans), n.as_bitmap_or().map(|b| &b.bitmapplans), n.as_agg().map(|a| &a.chain)];
    for l in lists.into_iter().flatten() {
        kids.extend(l.iter());
    }
    kids.extend(n.as_subquery_scan().and_then(|s| s.subplan));
    kids.into_iter().for_each(|k| cte_ids(k, out));
}

/// The inner a re-open of owner (`node_id`, `plan_id`) rescans: a nest's or a recursive union's righttree (its term), a
/// SubPlan's plan.
fn inner<'p>(stmt: &'p PlannedStmt<'p>, node_id: i32, plan_id: i32) -> Option<Node<'p>> {
    match plan_id {
        0 | gravity_decompose::ops::shared::setop::RECURSIVE_SET => lifetime::node_of(stmt, node_id).and_then(|(n, _)| n.as_plan()?.righttree),
        _ => subplan_tree(stmt, plan_id),
    }
}

fn kind(by: KeptBy) -> &'static str {
    match by {
        KeptBy::Pg { marks: false } => "pg",
        KeptBy::Pg { marks: true } => "pg-marks",
        KeptBy::Usual { .. } => "usual",
        KeptBy::Quiet => "quiet",
        KeptBy::Unreached => "unreached",
        KeptBy::Open => "open",
    }
}

fn faults<'p>(g: &PipelineGraph<'_>, stmt: &'p PlannedStmt<'p>, counts: &mut BTreeMap<&'static str, usize>) -> Vec<String> {
    let mut bad = Vec::new();
    let mut seen = BTreeSet::new();
    for r in g.kept_by.iter() {
        let p = r.pipeline as usize;
        if p >= g.pipelines.len() {
            bad.push(format!("owner n{} s{}: p{p} past the graph's {} pipelines", r.node_id, r.plan_id, g.pipelines.len()));
            continue;
        }
        if !seen.insert((r.node_id, r.plan_id, r.pipeline)) {
            bad.push(format!("owner n{} s{}: p{p} answered twice", r.node_id, r.plan_id));
        }
        let member = g.reruns.iter().any(|s| s.node_id == r.node_id && s.plan_id == r.plan_id && s.pipelines.contains(&r.pipeline));
        *counts.entry(if member { "joined-later" } else { kind(r.by) }).or_default() += 1;
        if member || !matches!(r.by, KeptBy::Pg { .. } | KeptBy::Usual { .. } | KeptBy::Quiet) {
            continue;
        }
        let mut under = BTreeSet::new();
        if let Some(i) = inner(stmt, r.node_id, r.plan_id) {
            subtree(i, &mut under);
            // a CTE a CteScan of the inner reads: its plan is a separate tree whose store the CteScan keeps
            let mut ctes = BTreeSet::new();
            cte_ids(i, &mut ctes);
            for t in ctes.into_iter().filter_map(|id| subplan_tree(stmt, id)) {
                subtree(t, &mut under);
            }
        }
        if !g.pipelines[p].node_ops.iter().any(|n| under.contains(&n.node_id)) {
            bad.push(format!("owner n{} s{}: p{p} answered {:?}, a rescan of the inner reaching it, but runs no node of the inner or of a CTE it reads", r.node_id, r.plan_id, r.by));
        }
    }
    bad
}

#[test]
fn every_kept_answer_names_a_pipeline_of_its_owners_inner() {
    let mut files = harness::fixtures_in(&harness::plans_root());
    files.extend(harness::fixtures_in(&harness::plans_root().join("regress-full")));
    let res = harness::par_map(&files, |f| {
        let text = fs::read_to_string(f).unwrap();
        let ctx = MemoryContext::new("fixture");
        let Ok(fx) = corpus::load(ctx.mcx(), &text) else { return None };
        let arena = Arena::new(1 << 20);
        let Ok(g) = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx) else { return None };
        let mut counts = BTreeMap::new();
        let bad = faults(&g, fx.stmt, &mut counts);
        Some((harness::fixture_name(f), counts, bad))
    });
    let (mut graphs, mut with, mut all, mut bad) = (0, 0, BTreeMap::new(), Vec::new());
    for (f, c, b) in res.into_iter().flatten() {
        graphs += 1;
        with += usize::from(!c.is_empty());
        for (k, n) in c {
            *all.entry(k).or_insert(0usize) += n;
        }
        bad.extend(b.into_iter().map(|x| format!("{f}: {x}")));
    }
    println!("kept answers: {graphs} graphs, {with} with answers, by kind {all:?}");
    for x in bad.iter() {
        println!("fault: {x}");
    }
    assert!(bad.is_empty(), "{} kept answers faulted", bad.len());
    assert!(graphs > 5000 || harness::filter().is_some(), "only {graphs} graphs");
}
