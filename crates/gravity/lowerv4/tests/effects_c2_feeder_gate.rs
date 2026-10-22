//! Construct C2 (b), the run-ahead tracer's gate stamp (frame/program.rs `checks_results`, ops/result/gate.rs
//! `feeder_checks_gate`): a pipeline whose source only one-time Results gate is not stamped gated when its feeder makes
//! those checks before its first pull, by `limit_feeder`'s own arms. regress-full/merge-058 (MERGE INTO sq_target t USING
//! v ON tid = sid AND (SELECT count(*) > 0 FROM sq_target) ..) puts a one-time Result over an InitPlan's parameter above a
//! merge join. PostgreSQL's ExecResult takes that verdict before it pulls the join. Lowered with the stamps on: the
//! pipeline that fills the join's outer Sort takes the gate's verdict in its prologue (an InitPlan parameter's qual, no
//! mask may skip the gate's pipeline), so its scan is no pipeline's gated node. The scan that fills the inner Sort stays
//! gated: PostgreSQL's merge join pulls its inner only once its outer has a row, a demand the gate's check does not cover.
use gravity_lowerv4_it as common;

use gravity_decompose::corpus;
use mcx::MemoryContext;
use types_nodes::plannodes::PlannedStmt;
use types_nodes::{Node, NodeTag};

/// The plan node ids of the scans that fill the outer and the inner Sort of `stmt`'s merge join.
fn merge_fills<'p>(stmt: &'p PlannedStmt<'p>) -> (i32, i32) {
    let mut join = None;
    gravity_decompose::rerun::each_plan_node(stmt, &mut |n| {
        if n.node_tag() == NodeTag::T_MergeJoin {
            join = n.as_plan().map(|p| (p.lefttree, p.righttree));
        }
    });
    let (outer, inner) = join.expect("merge-058 plans a merge join");
    let fill = |side: Option<Node<'p>>| {
        let sort = side.expect("a merge join side");
        assert_eq!(sort.node_tag(), NodeTag::T_Sort, "merge-058's merge join reads a Sort on each side");
        sort.as_plan().and_then(|p| p.lefttree).and_then(|s| s.as_plan().map(|p| p.plan_node_id)).expect("the Sort's scan")
    };
    (fill(outer), fill(inner))
}

#[test]
fn a_feeder_that_checks_the_gate_is_not_gated() {
    gravity_lowerv4::frame::program::set_gate_stamps(true);
    let text = std::fs::read_to_string(common::plans_dir().join("regress-full/merge-058.plan")).expect("merge-058.plan");
    let ctx = MemoryContext::new("c2 feeder gate");
    let fx = corpus::load(ctx.mcx(), &text).unwrap_or_else(|e| panic!("merge-058: {e}"));
    let (outer, inner) = merge_fills(fx.stmt);
    let never = gravity_decompose::pg_demand::never_run(fx.stmt, ctx.mcx());
    assert!(never.binary_search(&outer).is_ok() && never.binary_search(&inner).is_ok(), "merge-058: the one-time Result does not cover the scans {outer} and {inner}: {never:?}");
    let l = match common::lower_fixture(&text) {
        Some(Ok(l)) => l,
        Some(Err(_)) => panic!("merge-058 is refused"),
        None => panic!("merge-058 does not decompose"),
    };
    let gated: Vec<i32> = l.descs.pipelines.iter().map(|p| p.pg_gate).filter(|&g| g >= 0).collect();
    assert!(!gated.contains(&outer), "merge-058: the outer Sort's scan {outer} is stamped gated, though the pipeline filling it takes the gate's verdict first: {gated:?}");
    assert!(gated.contains(&inner), "merge-058: the inner Sort's scan {inner} is not stamped gated, though the merge join may never pull its inner: {gated:?}");
}
