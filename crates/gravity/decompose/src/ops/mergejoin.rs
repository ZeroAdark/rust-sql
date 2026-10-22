//! A merge join's ask of its inner (the exact request, PLAN-v5 §2.5; jmerge-0012, the request's second consumer after the
//! window's fill). PostgreSQL's ExecMergeJoin fetches its inner one tuple at a time (ExecProcNode at INITIALIZE_INNER,
//! SKIPINNER_ADVANCE and NEXTINNER) and stops fetching where the join ends, so an inner scan whose code acts (a VOLATILE
//! or user function in its filter) runs that code on exactly the rows the merge reached. The merge asks Exact of such an
//! inner (`PipelineGraph::exact`): lowerv4's merge then pulls its spool one row per fetch past the stored rows
//! (ops/mergejoin/exact_inner.rs), or keeps D1236's name where its first form does not reach. A raise-only inner is not
//! asked (errorhold's run-ahead licence reads it), nor a Sort (PG's sort reads its whole input at the first fetch: the
//! merge's lazy inner, MERGE R3 (ii)). Owner M11-MERGE.
use types_nodes::Node;

use crate::effects::Cx;
use crate::walk::Builder;

/// PostgreSQL's AggStrategy AGG_SORTED (nodes.h).
const AGG_SORTED: u32 = 1;

/// The merge's inner plan `inner`: a relation scan, or a Material over one, or a sorted GroupAggregate over one (it
/// streams, D1071 / facts/stream.rs `merge_inner`: PG's merge fetches it one group at a time and the group reads one feed
/// row past each, whose arguments run with the next group), whose code acts, asked Exact.
pub fn ask_exact<'p>(b: &mut Builder<'p, '_>, inner: Node<'p>) {
    let scan = |n: Option<Node<'p>>| n.is_some_and(|n| n.as_seq_scan().is_some() || n.as_index_scan().is_some() || n.as_index_only_scan().is_some() || n.as_bitmap_heap_scan().is_some());
    let group = inner.as_agg().is_some_and(|a| a.aggstrategy == AGG_SORTED && scan(a.plan.lefttree));
    let shaped = scan(Some(inner)) || inner.as_material().is_some_and(|m| scan(m.plan.lefttree)) || group;
    let Some(id) = inner.as_plan().map(|p| p.plan_node_id).filter(|_| shaped) else { return };
    if crate::effects::of_plan(&Cx::new(b.stmt, b.facts, b.arena), inner).acts && !b.graph.exact.contains(&id) {
        b.graph.exact.push(id);
    }
}
