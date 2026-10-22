//! Construct C2 (b), the run-ahead tracer's plan half: decompose `pg_demand::never_run` names the plan nodes PostgreSQL
//! 18.6 may never run. Over every plan fixture: the top always runs, and so does a data-modifying CTE's plan; every
//! SubPlan's root (no InitPlan), and every NestLoop's inner, may never run; a node that may never run gates its whole
//! subtree; the list is sorted with no repeat.
use std::fs;

use gravity_decompose::{corpus, pg_demand, rerun};
use mcx::MemoryContext;
use types_nodes::NodeTag;

mod harness;

#[test]
fn never_run_gates_inners_subplans_and_their_subtrees() {
    let (mut checked, mut nests) = (0, 0);
    for path in harness::fixtures_in(&harness::plans_root()) {
        let raw = fs::read_to_string(&path).unwrap();
        let ctx = MemoryContext::new("fixture");
        let Ok(fx) = corpus::load(ctx.mcx(), &raw) else { continue };
        let Some(top) = fx.stmt.planTree else { continue };
        let never = pg_demand::never_run(fx.stmt, ctx.mcx());
        let gated = |id: i32| never.binary_search(&id).is_ok();
        assert!(never.windows(2).all(|w| w[0] < w[1]), "{}: not sorted, or a repeat", path.display());
        let top_id = top.as_plan().map(|p| p.plan_node_id).unwrap();
        assert!(!gated(top_id), "{}: the top always runs", path.display());
        // a SubPlan no node lists among its InitPlans runs where its expression is evaluated: only when asked (an
        // InitPlan a gating Result's one-time qual reads runs with that Result)
        let mut inits = Vec::new();
        rerun::each_plan_node(fx.stmt, &mut |n| inits.extend(n.as_plan().into_iter().flat_map(|p| p.initPlan.iter().filter_map(|s| s.as_sub_plan()).map(|s| s.plan_id))));
        for (i, sub) in fx.stmt.subplans.iter().enumerate() {
            let Some(sub) = sub else { continue };
            let id = sub.as_plan().map(|p| p.plan_node_id).unwrap();
            assert!(inits.contains(&(i as i32 + 1)) || gated(id), "{}: subplan root {id} runs only when asked", path.display());
            // ExecPostprocessPlan runs a data-modifying CTE's plan to its end whatever reads it
            assert!(sub.node_tag() != NodeTag::T_ModifyTable || !gated(id), "{}: data-modifying CTE plan {id} always runs", path.display());
        }
        rerun::each_plan_node(fx.stmt, &mut |n| {
            let Some(p) = n.as_plan() else { return };
            if n.node_tag() == NodeTag::T_NestLoop {
                let inner = p.righttree.and_then(|i| i.as_plan()).map(|i| i.plan_node_id).unwrap();
                assert!(gated(inner), "{}: NestLoop {}'s inner {inner} runs at its outer's first row", path.display(), p.plan_node_id);
                nests += 1;
            }
            if gated(p.plan_node_id) {
                for c in p.lefttree.into_iter().chain(p.righttree) {
                    let id = c.as_plan().map(|x| x.plan_node_id).unwrap();
                    assert!(gated(id), "{}: node {}'s child {id} runs only when it does", path.display(), p.plan_node_id);
                }
            }
        });
        checked += 1;
    }
    assert!(checked > 100 && nests > 10, "only {checked} fixtures, {nests} nest loops checked");
}

/// D4191: `regions` holds what `never_run` names, each with the root of its innermost gate region: a region's root is in
/// that region itself, every node's region is a recorded region, each chain climbs to a region whose parent is none, a
/// NestLoop's inner opens its own region, and a subplan's root opens one with no parent unless every CteScan reading it
/// sits in one region, which then holds it.
#[test]
fn regions_open_at_each_gate_and_chain_to_a_root() {
    let (mut checked, mut nested, mut joined) = (0, 0, 0);
    for path in harness::fixtures_in(&harness::plans_root()) {
        let raw = fs::read_to_string(&path).unwrap();
        let ctx = MemoryContext::new("fixture");
        let Ok(fx) = corpus::load(ctx.mcx(), &raw) else { continue };
        if fx.stmt.planTree.is_none() {
            continue;
        }
        let (never, r) = (pg_demand::never_run(fx.stmt, ctx.mcx()), pg_demand::regions(fx.stmt, ctx.mcx()));
        assert_eq!(never[..], r.of.iter().map(|x| x.0).collect::<Vec<_>>()[..], "{}: never_run is regions' nodes", path.display()); // alloc: Cold
        let region_of = |id: i32| r.of.binary_search_by_key(&id, |x| x.0).ok().map(|i| r.of[i].1);
        for &(root, _) in r.parent.iter() {
            assert_eq!(region_of(root), Some(root), "{}: region {root}'s root is in it", path.display());
        }
        for &(id, reg) in r.of.iter() {
            assert!(r.parent.binary_search_by_key(&reg, |x| x.0).is_ok(), "{}: node {id}'s region {reg} is recorded", path.display());
            let chain: Vec<i32> = r.chain(id).collect(); // alloc: Cold
            assert!(chain.first() == Some(&reg) && chain.len() <= r.parent.len(), "{}: node {id}'s chain {chain:?}", path.display());
            let last = *chain.last().unwrap();
            assert!(r.parent.iter().any(|&(x, p)| x == last && p < 0), "{}: node {id}'s chain ends below another region", path.display());
            nested += usize::from(chain.len() > 1);
        }
        rerun::each_plan_node(fx.stmt, &mut |n| {
            let Some(p) = n.as_plan() else { return };
            if n.node_tag() == NodeTag::T_NestLoop {
                let inner = p.righttree.and_then(|i| i.as_plan()).map(|i| i.plan_node_id).unwrap();
                assert_eq!(region_of(inner), Some(inner), "{}: NestLoop {}'s inner opens its region", path.display(), p.plan_node_id);
            }
        });
        // each CteScan's (CTE plan id, region): a CTE read in one region only is in that region
        let mut scans: Vec<(i32, i32)> = Vec::new(); // alloc: Cold
        rerun::each_plan_node(fx.stmt, &mut |n| {
            if let (Some(c), Some(p)) = (n.as_cte_scan(), n.as_plan()) {
                scans.push((c.ctePlanId, region_of(p.plan_node_id).unwrap_or(-1))); // alloc: Cold
            }
        });
        for (i, sub) in fx.stmt.subplans.iter().enumerate().filter_map(|(i, s)| s.map(|s| (i as i32 + 1, s))) {
            let id = sub.as_plan().map(|p| p.plan_node_id).unwrap();
            let Some(reg) = region_of(id) else { continue };
            if reg == id {
                assert!(r.parent.iter().any(|&(x, p)| x == id && p < 0), "{}: subplan region {id} has no parent", path.display());
                continue;
            }
            let read: Vec<i32> = scans.iter().filter(|x| x.0 == i).map(|x| x.1).collect(); // alloc: Cold
            assert!(read.is_empty() || read.iter().all(|&x| x == reg), "{}: CTE {i} (root {id}) in region {reg}, read in {read:?}", path.display());
            joined += 1;
        }
        checked += 1;
    }
    assert!(checked > 100 && nested > 0, "only {checked} fixtures, {nested} nodes in nested regions");
    eprintln!("{checked} fixtures, {nested} nodes in nested regions, {joined} subplans in a reader's region");
}

/// `result_gated` names, of the nodes `never_run` names, those only a Result's one-time qual gates: each listed Result is
/// such a Result and is not the node itself, a node under a nest's inner is never listed, and a listed node's child in
/// the statement's tree is listed too unless a join's waiting inner gates it. Sorted by node, no repeat.
#[test]
fn result_gated_names_only_nodes_under_one_time_results() {
    let (mut checked, mut listed) = (0, 0);
    for path in harness::fixtures_in(&harness::plans_root()) {
        let raw = fs::read_to_string(&path).unwrap();
        let ctx = MemoryContext::new("fixture");
        let Ok(fx) = corpus::load(ctx.mcx(), &raw) else { continue };
        if fx.stmt.planTree.is_none() {
            continue;
        }
        let (never, soft) = (pg_demand::never_run(fx.stmt, ctx.mcx()), pg_demand::result_gated(fx.stmt, ctx.mcx()));
        assert!(soft.windows(2).all(|w| w[0].0 < w[1].0), "{}: not sorted, or a repeat", path.display());
        let mut one_time = Vec::new();
        let mut nest_inners = Vec::new();
        rerun::each_plan_node(fx.stmt, &mut |n| {
            let Some(p) = n.as_plan() else { return };
            if n.as_result().is_some_and(|r| r.resconstantqual.is_some()) {
                one_time.push(p.plan_node_id);
            }
            if n.node_tag() == NodeTag::T_NestLoop {
                nest_inners.extend(p.righttree.and_then(|i| i.as_plan()).map(|i| i.plan_node_id));
            }
        });
        for (id, results) in soft.iter() {
            assert!(never.binary_search(id).is_ok(), "{}: node {id} is listed but may always run", path.display());
            assert!(!results.is_empty() && results.iter().all(|r| one_time.contains(r) && r != id), "{}: node {id} lists {results:?}", path.display());
            assert!(!nest_inners.contains(id), "{}: node {id} is a nest's inner", path.display());
            listed += 1;
        }
        checked += 1;
    }
    assert!(checked > 100 && listed > 0, "only {checked} fixtures, {listed} listed nodes");
}
