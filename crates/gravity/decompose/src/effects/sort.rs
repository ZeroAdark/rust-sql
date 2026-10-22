//! Whether a Sort raises at its first call, read from its keys: the planning half of the sort family's raise decisions.

use crate::catalog::Facts;
use crate::graph::{Op, PipelineGraph, Source};
use crate::ops::shared::sort::indeterminate;
use crate::ops::shared::sort::SortDesc;
use crate::walk::Open;

/// D1292: a key of Sort `d` is indeterminate (`ops::sort::indeterminate` over its target entry's type), so PG's
/// `tuplesort_begin_heap` raises 42P22 at the Sort's first call (varlena.c `varstr_sortsupport`'s
/// `check_collation_set`).
pub fn sort_raises(facts: &dyn Facts, d: &SortDesc<'_>) -> bool {
    d.keys.iter().any(|k| {
        let e = (k.resno as usize).checked_sub(1).and_then(|j| (j < d.tlist.len()).then(|| d.tlist.nth(j))).and_then(|n| n.as_target_entry());
        e.and_then(|e| crate::catalog::expr_type::type_of(e.expr)).is_some_and(|t| indeterminate(facts, t, k.coll.oid))
    })
}

/// D1292: `open` (a WindowAgg's input, no union's arms) reads the emit of a Sort of graph `g` that raises at its first
/// call (`sort_raises`): PG's WindowAgg pulls that Sort at its first call, so no row reaches a partition or peer compare
/// (decompose ops/window.rs `map`); lowerv4 serves the raise or refuses it by D1292's name (ops/sort/sort_raise.rs).
pub fn reads_raising_sort(g: &PipelineGraph<'_>, facts: &dyn Facts, open: &Open<'_>) -> bool {
    let Source::Breaker { slot } = open.source else { return false };
    open.union.is_none() && crate::breakers::decl_of(g, slot).is_some_and(|d| matches!(g.pipelines[d.producer as usize].ops.last(), Some(Op::Sort(s)) if sort_raises(facts, s)))
}
