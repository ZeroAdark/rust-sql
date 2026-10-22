//! D1920: the stores PostgreSQL 18.6's rescan of a subtree keeps, whose rows a re-run does not fill again (each node's
//! ExecReScan under the executor flags the rescan reaches it with). Which of them matter, e.g. one holding a VOLATILE call's
//! rows, is the caller's question to the effects home: this walk only names the stores and what fills them.
use gravity_program::{ArenaRef, ArenaVec};
use types_nodes::plannodes::PlannedStmt;
use types_nodes::primnodes::{SubLinkType, SubPlan};
use types_nodes::{Node, NodeTag};

use super::keep::{MARK, REWIND};
use crate::ops::shared::agg::{AGG_HASHED, AGG_MIXED};

/// A store a rescan keeps, by what fills it.
#[derive(Clone, Copy, Debug)]
pub enum Store<'p> {
    /// The rows of a plan subtree: a node PG keeps (a Material under REWIND, a Sort under REWIND or MARK, a hash join's
    /// table, a hashed Agg's or SetOp's table, a Memoize cache), a hashed SubPlan's table (built once per SubPlanState) or
    /// a CTE declared in the subtree.
    Plan(Node<'p>),
    /// A FunctionScan function's tuplestore, filled by its call expression (ExecReScanFunctionScan keeps it while none of
    /// its funcparams changed).
    Call(Node<'p>),
    /// A store whose filler the plan does not show: a node with no plan, a SubPlan whose plan is missing, a table function's
    /// tuplestore, a function entry that is no RangeTblFunction or has no expression.
    Unknown,
}

/// D1920: whether `visit` stops (returns true) at one of the stores a rescan of SubPlan `s`'s plan keeps, the SubPlan's own
/// params and `changed` changed (its plan under REWIND when it is in rewindPlanIDs), in the plan's walk order.
pub fn subplan_stores<'p>(stmt: &'p PlannedStmt<'p>, arena: ArenaRef, s: &'p SubPlan<'p>, changed: &[i32], visit: &mut dyn FnMut(Store<'p>) -> bool) -> bool {
    let Some(root) = crate::rerun::subplan_root(stmt, s.plan_id) else { return visit(Store::Unknown) };
    let mut ch: ArenaVec<i32> = ArenaVec::new_in(arena); // alloc: Cold
    ch.extend(changed.iter().copied().chain(s.parParam.iter()));
    walk(stmt, arena, root, if stmt.rewindPlanIDs.is_member(s.plan_id) { REWIND } else { 0 }, &ch, visit)
}

/// D1920's VOLATILE half: the root of SubPlan `s`'s plan is itself a store PG's rescan of the plan keeps whole, so nothing
/// below it runs again at any rescan: `s` is nested with no parameters, its plan reads none (no chgParam ever reaches it),
/// and the root is a Material or a Sort its REWIND keeps (the plan in rewindPlanIDs: ExecReScanMaterial / ExecReScanSort
/// rewind the store) or a FunctionScan (ExecReScanFunctionScan rewinds each tuplestore no funcparam changed).
pub fn root_store<'p>(stmt: &'p PlannedStmt<'p>, s: &'p SubPlan<'p>, root: Node<'p>) -> bool {
    let Some(p) = root.as_plan() else { return false };
    if s.useHashTable || !s.parParam.is_nil() || !s.args.is_nil() || p.allParam.iter().next().is_some() {
        return false;
    }
    match root.node_tag() {
        NodeTag::T_Material | NodeTag::T_Sort => stmt.rewindPlanIDs.is_member(s.plan_id),
        NodeTag::T_FunctionScan => true,
        _ => false,
    }
}

/// The stores a rescan of subtree `n` (executor flags `ef`: REWIND, MARK) changing only `changed` keeps. A store whose input
/// reads a changed param is rebuilt (chgParam), so it is not one; a non-CTE InitPlan is one parameter cell for every copy,
/// run once, so it is none either.
fn walk<'p>(stmt: &'p PlannedStmt<'p>, arena: ArenaRef, n: Node<'p>, ef: u8, changed: &[i32], visit: &mut dyn FnMut(Store<'p>) -> bool) -> bool {
    crate::walk::stack_check(); // D3748
    let Some(p) = n.as_plan() else { return visit(Store::Unknown) };
    let reads = |x: Option<Node<'p>>| x.and_then(|x| x.as_plan()).is_some_and(|c| c.allParam.iter().any(|q| changed.contains(&q)));
    for s in p.initPlan.iter().filter_map(|s| s.as_sub_plan()).filter(|s| s.subLinkType == SubLinkType::CTE_SUBLINK) {
        let hit = match crate::rerun::subplan_root(stmt, s.plan_id) {
            Some(r) => visit(Store::Plan(r)),
            None => visit(Store::Unknown),
        };
        if hit {
            return true;
        }
    }
    let mut hit = false;
    crate::ops::subplan::own_lists(n, &mut |e| {
        crate::expr::walk(e, &mut |x| {
            if !hit {
                hit = x.as_sub_plan().is_some_and(|s| called(stmt, arena, s, changed, &mut *visit));
            }
            !hit
        });
    });
    if hit {
        return true;
    }
    let (l, r) = (p.lefttree, p.righttree);
    let hashed = n.as_agg().is_some_and(|a| matches!(a.aggstrategy, AGG_HASHED | AGG_MIXED)) || n.as_set_op().is_some_and(|s| s.strategy == 1);
    let strip = n.as_agg().is_some_and(|a| a.aggstrategy == AGG_HASHED) || n.as_set_op().is_some_and(|s| s.strategy == 1);
    let kept = match n.node_tag() {
        NodeTag::T_FunctionScan => {
            let fresh = |f: &types_nodes::parsenodes::RangeTblFunction<'p>| f.funcparams.iter().any(|q| changed.contains(&q));
            for f in n.as_function_scan().unwrap().functions.iter() {
                let hit = match f.as_range_tbl_function() {
                    None => visit(Store::Unknown),
                    Some(f) if fresh(f) => false,
                    Some(f) => match f.funcexpr {
                        Some(e) => visit(Store::Call(e)),
                        None => visit(Store::Unknown),
                    },
                };
                if hit {
                    return true;
                }
            }
            return false;
        }
        NodeTag::T_TableFuncScan => return visit(Store::Unknown),
        NodeTag::T_Material => ef & REWIND != 0 && !reads(l),
        NodeTag::T_Sort => ef != 0 && !reads(l),
        NodeTag::T_Hash => !reads(l),
        NodeTag::T_Memoize => true,
        NodeTag::T_Agg | NodeTag::T_SetOp => hashed && !reads(l) && !reads(r),
        _ => false,
    };
    if kept {
        return visit(Store::Plan(n));
    }
    let mut kid = |c: Option<Node<'p>>, f: u8| c.is_some_and(|c| walk(stmt, arena, c, f, changed, &mut *visit));
    match n.node_tag() {
        NodeTag::T_Sort | NodeTag::T_Material => kid(l, 0),
        NodeTag::T_NestLoop => kid(l, ef) || kid(r, if n.as_nest_loop().unwrap().nestParams.is_nil() { ef | REWIND } else { ef & !REWIND }),
        NodeTag::T_MergeJoin => kid(l, ef) || kid(r, if n.as_merge_join().unwrap().skip_mark_restore { ef } else { ef | MARK }),
        _ if strip => kid(l, ef & !REWIND) || kid(r, ef & !REWIND),
        _ => {
            let lists = [n.as_append().map(|a| &a.appendplans), n.as_merge_append().map(|a| &a.mergeplans), n.as_bitmap_and().map(|a| &a.bitmapplans), n.as_bitmap_or().map(|a| &a.bitmapplans), n.as_agg().map(|a| &a.chain)];
            kid(l, ef) || kid(r, ef) || lists.into_iter().flatten().any(|ls| ls.iter().any(|c| kid(Some(c), ef))) || kid(n.as_subquery_scan().and_then(|s| s.subplan), ef)
        }
    }
}

/// SubPlan `s`, called inside a subtree the rescan reaches: a hashed one keeps its table (its whole plan fills it); a nested
/// one what its own plan's rescan keeps (`subplan_stores`).
fn called<'p>(stmt: &'p PlannedStmt<'p>, arena: ArenaRef, s: &'p SubPlan<'p>, changed: &[i32], visit: &mut dyn FnMut(Store<'p>) -> bool) -> bool {
    match crate::rerun::subplan_root(stmt, s.plan_id) {
        Some(root) if s.useHashTable => visit(Store::Plan(root)),
        _ => subplan_stores(stmt, arena, s, changed, visit),
    }
}
