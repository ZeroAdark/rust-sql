//! D3437 (C10 move 4a-2): a union whose arms read breakers their own children filled runs each arm right after its own
//! child's pipelines (drain/arms.rs `arm_run`, frame/mechs.rs `arm_order`), so the driver frees each child's slot after its
//! arm and one child's finished breaker is resident at a time, as PostgreSQL's Append runs child `i` whole first. gsunion-022
//! (the planner's grouping-set rewrite: CUBE (a, b) as a UNION ALL of GROUP BYs under count(*)) and regress-full
//! partition_aggregate-002 (a partition-wise aggregate under a sort) take arm order; in graph order every child ran before
//! the first arm.
#[allow(unused_imports, dead_code)]
use gravity_lowerv4_it as common;

#[allow(dead_code)]
#[path = "common/write_rig.rs"]
mod write_rig;

/// Fixture `name`'s program order and each Append union's child starts and arm pipelines.
fn order_of(name: &str) -> (Vec<usize>, Vec<(Vec<usize>, std::ops::Range<usize>)>) {
    let rig = mcx::MemoryContext::new("GravityPlanRig");
    let path = common::plans_dir().join(format!("{name}.plan"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    write_rig::declared(&text, |a, graph, facts| {
        let unions = graph.unions.iter().filter(|u| !u.sets).map(|u| (u.starts.iter().map(|&s| s as usize).collect(), u.pushed.start as usize..u.pushed.end as usize)).collect();
        (gravity_lowerv4::program_order(a.handle(), rig.mcx(), graph, facts, common::RigCatalog::get()), unions)
    })
    .unwrap_or_else(|| panic!("{name}: decompose declined"))
}

#[test]
fn each_arm_runs_right_after_its_own_child() {
    for name in ["gsunion-022", "regress-full/partition_aggregate-002"] {
        let (order, unions) = order_of(name);
        let at = |k: usize| order.iter().position(|&p| p == k).unwrap_or_else(|| panic!("{name}: p{k} not in {order:?}"));
        let (starts, arms) = unions.iter().find(|(s, a)| s.len() == a.len() + 1 && a.len() >= 2).unwrap_or_else(|| panic!("{name}: no Append union"));
        for (i, q) in arms.clone().enumerate() {
            // child i's own pipelines, then its arm, then child i + 1's first pipeline
            if starts[i] < starts[i + 1] {
                assert_eq!(at(q), at(starts[i + 1] - 1) + 1, "{name}: arm p{q} not right after child {i}'s last pipeline: {order:?}");
            }
            if let Some(&next) = starts.get(i + 1).filter(|&&s| s < arms.start) {
                assert!(at(q) < at(next), "{name}: arm p{q} after child {}'s first pipeline: {order:?}", i + 1);
            }
        }
    }
}
