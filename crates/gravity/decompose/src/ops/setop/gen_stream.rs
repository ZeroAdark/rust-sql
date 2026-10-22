//! D2034 (with#45): the recursive union whose generations stream. PostgreSQL's RecursiveUnion pulls its term one row per
//! call (nodeRecursiveunion.c ExecRecursiveUnion), so a reader that stops inside a generation stops the term there too;
//! the lowering runs a generation whole, so a term that never ends under such a reader never returns. The streamed form
//! (lowerv4 ops/setop/recursive.rs, `IterDesc::stream`) pulls the term's body a budget at a time instead, through the term
//! set's pull (exec rerun `OP_RERUN_PULL`), for one shape whose set runs nothing but the body: the term reads another
//! recursive union's CTE through projections and quals only, that inner union reads the outer one's working table, and
//! its seed reads none of the outer union's params. The outer term set then re-arms the inner CTE at every generation
//! (its seed runs again in the CTE's next pull: decompose rerun.rs `union_seed`, PG's ExecReScanRecursiveUnion), and the
//! body's CteScan pulls the inner union as far as the outer reader reads, as PG's does.
use types_nodes::plannodes::{PlannedStmt, RecursiveUnion};
use types_nodes::{Node, NodeTag};

/// The plan id of the CTE whose rows union `ru`'s term streams (the shape above), if its term is that shape.
pub(crate) fn source<'p>(stmt: &'p PlannedStmt<'p>, ru: &RecursiveUnion<'p>) -> Option<i32> {
    let mut n = ru.plan.righttree?;
    let id = loop {
        match n.node_tag() {
            NodeTag::T_Result if n.as_result().is_some_and(|r| r.resconstantqual.is_none()) => n = n.as_plan()?.lefttree?,
            NodeTag::T_SubqueryScan => n = n.as_subquery_scan()?.subplan?,
            NodeTag::T_CteScan => break n.as_cte_scan()?.ctePlanId,
            _ => return None,
        }
    };
    let inner = crate::rerun::subplan_root(stmt, id)?;
    let reads = |p: Option<Node<'p>>| p.and_then(|p| p.as_plan()).is_some_and(|p| p.allParam.is_member(ru.wtParam));
    let seed = inner.as_recursive_union()?.plan.lefttree;
    (reads(Some(inner)) && !reads(seed)).then_some(id)
}
