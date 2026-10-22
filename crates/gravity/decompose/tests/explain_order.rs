//! `DECISIONS.md` 4 (D) b (the run-ahead tag, program `AheadTag`): decompose `explain::explain_position` numbers each plan node
//! EXPLAIN prints once, in one walk. Over every plan fixture: the indices it gives are 0..n with no gap and no repeat, the
//! node EXPLAIN prints first (the top, or an invisible Gather's child) is 0, and each has EXPLAIN's Node Type.
use std::fs;

use gravity_decompose::{corpus, explain, rerun};
use mcx::MemoryContext;

mod harness;

#[test]
fn explain_order_numbers_each_printed_node_once() {
    let mut checked = 0;
    for path in harness::fixtures_in(&harness::plans_root()) {
        let raw = fs::read_to_string(&path).unwrap();
        let ctx = MemoryContext::new("fixture");
        let Ok(fx) = corpus::load(ctx.mcx(), &raw) else { continue };
        let mut ids = Vec::new();
        rerun::each_plan_node(fx.stmt, &mut |n| ids.extend(n.as_plan().map(|p| p.plan_node_id)));
        // a grouping-sets Agg's chain (planner Agg nodes EXPLAIN does not print) carries plan_node_id 0
        ids.sort_unstable();
        ids.dedup();
        let mut at = Vec::new();
        for &id in ids.iter() {
            if let Some((i, t)) = explain::explain_position(fx.stmt, id) {
                assert_ne!(t, "???", "{}: node {id} has no EXPLAIN Node Type", path.display());
                at.push(i);
            }
        }
        at.sort_unstable();
        assert_eq!(at, (0..at.len() as u32).collect::<Vec<_>>(), "{}: EXPLAIN's order has a gap or a repeat", path.display());
        let Some(top) = fx.stmt.planTree else { continue };
        let first = match top.as_gather() {
            Some(g) if g.invisible => g.plan.lefttree.unwrap_or(top),
            _ => top,
        };
        let id = first.as_plan().map(|p| p.plan_node_id).unwrap();
        assert_eq!(explain::explain_position(fx.stmt, id).map(|x| x.0), Some(0), "{}: the first node printed", path.display());
        checked += 1;
    }
    assert!(checked > 100, "only {checked} fixtures checked");
}
