//! A one-time gate (a Result's `resconstantqual`, PG's `ExecResult` checks it once before the first row): the test
//! that it passes no row, read by the MERGE source's dummy Result (ops/modifytable) and a recursive union's term the
//! planner proved empty (ops/setop); `no_rows`, a plan subtree that returns no row, by a WindowAgg's order check
//! (ops/window, D4020).
use types_nodes::{Node, NodeTag};

/// A one-time gate constant false or NULL (an implicit-AND list holding one such constant): no row passes it (lowerv4
/// shared/dummy.rs `never`, the same test).
pub fn never(n: Node<'_>) -> bool {
    crate::walk::stack_check();
    if let Some(c) = n.as_const() {
        return c.constisnull || !c.constvalue.as_bool();
    }
    n.as_list().is_some_and(|l| l.iter().any(never))
}

/// D4020: plan node `n` returns no row. A Result whose one-time gate is `never` passes none (the planner's empty
/// relation, or a WHERE folded to false over its input), and a node that only orders, buffers, filters, deduplicates or
/// caps its one input, or adds columns to its rows (Sort, Incremental Sort, Material, a Result over an input, Unique,
/// Limit, a WindowAgg: one row per input row) returns none over such an input. A consumer of its rows may then take any
/// input order: no row comes to be out of order (rnd-0473: PG drops the pinned `em_hired` from the Sort under a
/// WindowAgg because the WHERE pins it, then folds the WHERE to `One-Time Filter: false`, so no Filter op is left to read
/// that pin from).
pub fn no_rows(n: Node<'_>) -> bool {
    crate::walk::stack_check();
    if let Some(r) = n.as_result() {
        return r.resconstantqual.is_some_and(never) || r.plan.lefttree.is_some_and(no_rows);
    }
    match n.node_tag() {
        NodeTag::T_Sort | NodeTag::T_IncrementalSort | NodeTag::T_Material | NodeTag::T_Unique | NodeTag::T_Limit | NodeTag::T_WindowAgg => {
            n.as_plan().and_then(|p| p.lefttree).is_some_and(no_rows)
        }
        _ => false,
    }
}
