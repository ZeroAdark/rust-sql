//! D3307 (setop_recurse line 99 at W8: a recursive walk with LIMIT 9499 over a hash join whose projection divides by an
//! edge's weight): a hash build whose within-key match order PostgreSQL's reader can see stays serial (`visible`).
//! PostgreSQL's serial Hash chains each key's tuples newest first (ExecHashTableInsert pushes at the bucket's head), and
//! a serial build here keeps that order; a parallel build interleaves its workers' inserts, so a probe row's matches
//! come in an order that varies run to run. Where the join's per-match work may raise and a reader above may stop before
//! the join's last row (a LIMIT, a first-match nest, a merge join, a SubPlan, a cursor), that order decides whether the
//! raising match is reached: an error where PostgreSQL answers. Such a build stays serial only there: every other
//! parallel build keeps its width (no deterministic order is paid for where no reader can see it).
//! - Per-match work: the join qual runs on each candidate that passes the hash clauses, the qual and the projection on
//!   each joined row. The hash clauses' sides are no such work: each side's key runs over every row at the build or the
//!   probe anyway, in any order. A SEMI or ANTI join stops at the first candidate whose join qual passes, so it shows
//!   its order through the join qual alone, whatever reads it; an inner-unique join has at most one candidate that
//!   passes the hash clauses, so its work runs once per probe row, in the probe's order, and shows none.
//! - A reader that may stop (`stops_above`), walked up from the join: a Limit, a merge join, the inner side of a
//!   first-match nest loop, a SubPlan's or InitPlan's root, a cursor's root, or a CTE's root one of whose CteScans has
//!   such a reader (a recursive union's term reaches its CTE's readers through the union). A node that reads its whole
//!   input before its first row ends the walk (a Sort, a Hash, a plain or hashed aggregate, a hashed SetOp, a write):
//!   every match runs before anything leaves, in any order. The statement's root ends it too (the client reads every
//!   row). Anything else hands rows on as it pulls them, and the walk goes on.
use types_nodes::jointype::JoinType;
use types_nodes::plannodes::{HashJoin, Plan};
use types_nodes::Node;

use gravity_program::ArenaVec;

use crate::ops::shared::agg::{AGG_HASHED, AGG_PLAIN};
use crate::walk::Builder;

/// pathnodes.h `SETOP_HASHED`.
const SETOP_HASHED: u32 = 1;

/// The walk's bound: plan depth plus CTE hops (a deeper plan answers "may stop", failing closed).
const DEPTH: usize = 256;

/// Whether hash join `hj`'s build must stay serial: its within-key match order is visible.
pub fn visible<'p>(b: &Builder<'p, '_>, hj: &'p HashJoin<'p>) -> bool {
    let raises = |l: &'p types_nodes::list::NodeList<'p>| {
        l.iter().any(|n| crate::effects::Effects::of_found(crate::rerun::of_expr(b.stmt, b.facts, b.arena, n.as_target_entry().map_or(n, |t| t.expr))).may_raise)
    };
    let (j, plan) = (&hj.join, &hj.join.plan);
    if j.inner_unique {
        return false;
    }
    let quals = raises(&j.joinqual);
    if matches!(j.jointype, JoinType::JOIN_SEMI | JoinType::JOIN_ANTI) {
        return quals;
    }
    (quals || raises(&plan.qual) || raises(&plan.targetlist)) && stops_above(b, plan, DEPTH)
}

/// Whether a reader above plan node `p` may stop before `p`'s last row (the module doc's walk).
fn stops_above<'p>(b: &Builder<'p, '_>, p: &'p Plan<'p>, depth: usize) -> bool {
    crate::walk::stack_check(); // D3748
    let Some(depth) = depth.checked_sub(1) else { return true };
    let mut path = ArenaVec::new_in(b.arena); // alloc: Cold
    let roots = b.stmt.planTree.into_iter().map(|r| (r, None)).chain(b.stmt.subplans.iter().enumerate().filter_map(|(i, r)| r.map(|r| (r, Some(i)))));
    let Some(sub) = roots.into_iter().find_map(|(r, sub)| path_to(r, p, &mut path).then_some(sub)) else { return true };
    // `path`: the ancestors, nearest first, each with whether the walk came up its inner side
    for &(n, inner) in path.iter() {
        if n.as_limit().is_some() || n.as_merge_join().is_some() {
            return true;
        }
        if inner && n.as_nest_loop().is_some_and(|nl| nl.join.inner_unique || matches!(nl.join.jointype, JoinType::JOIN_SEMI | JoinType::JOIN_ANTI)) {
            return true;
        }
        let whole = n.as_sort().is_some()
            || n.as_hash().is_some()
            || n.as_agg().is_some_and(|a| a.aggstrategy == AGG_PLAIN || a.aggstrategy == AGG_HASHED)
            || n.as_set_op().is_some_and(|s| s.strategy == SETOP_HASHED)
            || n.as_modify_table().is_some();
        if whole {
            return false;
        }
    }
    match sub {
        // the client reads every row, unless a cursor's FETCH stops it
        None => b.graph.partial,
        // a CTE: its readers decide; a SubPlan or InitPlan (no CteScan reads it) may stop at its first row
        Some(i) => {
            let id = i as i32 + 1;
            let mut scans = ArenaVec::new_in(b.arena); // alloc: Cold
            for r in b.stmt.planTree.into_iter().chain(b.stmt.subplans.iter().flatten()) {
                cte_scans(r, id, &mut scans);
            }
            scans.is_empty() || scans.iter().any(|&s| stops_above(b, s, depth))
        }
    }
}

/// The children of plan node `n`, each with whether it is the inner side: the outer, the inner, a SubqueryScan's plan,
/// an Append's or MergeAppend's members.
fn children<'p>(n: Node<'p>) -> impl Iterator<Item = (Node<'p>, bool)> {
    let p = n.as_plan();
    let sides = p.into_iter().flat_map(|p| [(p.lefttree, false), (p.righttree, true)]);
    let sub = n.as_subquery_scan().and_then(|s| s.subplan).map(|s| (Some(s), false));
    let members = n.as_append().map(|a| &a.appendplans).or_else(|| n.as_merge_append().map(|m| &m.mergeplans));
    sides.chain(sub).filter_map(|(c, i)| Some((c?, i))).chain(members.into_iter().flat_map(|l| l.iter().map(|c| (c, false))))
}

/// Whether plan node `p` is under `n` (or is `n`); `path` then holds `p`'s ancestors up to `n`, nearest first.
fn path_to<'p>(n: Node<'p>, p: &'p Plan<'p>, path: &mut ArenaVec<(Node<'p>, bool)>) -> bool {
    crate::walk::stack_check(); // D3748
    if n.as_plan().is_some_and(|q| core::ptr::eq(q, p)) {
        return true;
    }
    for (c, inner) in children(n) {
        if path_to(c, p, path) {
            path.push((n, inner));
            return true;
        }
    }
    false
}

/// The CteScans under `n` that read CTE subplan `id` (1-based, `ctePlanId`).
fn cte_scans<'p>(n: Node<'p>, id: i32, out: &mut ArenaVec<&'p Plan<'p>>) {
    crate::walk::stack_check(); // D3748
    if let Some(c) = n.as_cte_scan().filter(|c| c.ctePlanId == id) {
        out.push(&c.scan.plan);
    }
    for (c, _) in children(n) {
        cte_scans(c, id, out);
    }
}
