//! D4779 (`pgrust.gravity_cte_pushdown` = fill; D3342's rows, PostgreSQL's plan): the readers' filters of a CTE read
//! more than once, applied inside Gravity's fill of it. PostgreSQL plans the CTE's body as written (planner cte_or.rs
//! `fill_admits` only marks its CteScans `gravity_fill`), so no pushed filter moves its estimates or join order (under
//! contained a smaller estimate can turn the body into a deep hash chain whose one large build does not spill). Here the
//! fill computes only the rows some reader keeps:
//! - the OR, over every CteScan of the CTE in the statement, of the AND of its own filters (its plan quals that read its
//!   columns alone, raise-free and acting nothing: `effects::read::pure`, no SubPlan, no PARAM_EXEC). A row no arm passes
//!   is read by no CteScan, so dropping it from the CTE changes no reader's rows or their order. One CteScan without
//!   such a filter leaves the CTE whole.
//! - lowered down the CTE's plan (`lower`) node by node, each column read through the node's target entry while that
//!   entry is a bare column of its input: a Sort, Material, Hash, Gather, a Result over an input, a SubqueryScan; an
//!   Append's every arm; a plain or hashed grouping Agg (no grouping sets) and a Unique over their key columns alone, each
//!   a key whose equal values are one image under the group's own equality (`keysem::one_image`, `own_equivalence`: a
//!   filter then keeps or drops a group's every row); a join's preserved sides (both of an inner join, the outer of a
//!   left, semi or anti join, the inner of a right one). An AND keeps the conjuncts that pass (each implied by it), an OR
//!   only when every arm passes, so what reaches a node is implied by the OR above it.
//! - at a SeqScan it reaches, a filter over the scan's own columns, after the scan's own quals (`filters`, from
//!   ops/scan `map_seq_scan`): the hash builds over it shrink, their runtime filters skip the probe's granules and the
//!   groups above shrink. Any other node stops the walk there (a Limit, a WindowAgg, a set operation, an index scan ...).
//! The errors a body raises over a row no reader keeps do not arise (`contained`'s ruled residual). Fires
//! `cte.fill` where a scan takes one. PostgreSQL's EXPLAIN is the plan's; the front key puts a marked CteScan's quals in
//! the zone class, as a SeqScan's own (exec inputs/front.rs).
use core::cell::Cell;

use gravity_program::ArenaVec;
use mcx::{GrowIn, PgVec};
use types_nodes::plannodes::Plan;
use types_nodes::primnodes::{BoolExpr, BoolExprType, ParamKind, INNER_VAR, OUTER_VAR};
use types_nodes::{JoinType, Node, NodeList, NodeTag};

use crate::catalog::Facts;
use crate::graph::Op;
use crate::ops::shared::scan::Atts;
use crate::walk::{Builder, Open};

/// The shapecheck fire signal of a scan that takes a reader filter.
pub const FIRE: &str = "cte.fill";

/// The fill filters of every fill-marked CTE of the statement, into `Builder::fill_quals` (by SeqScan node id). Nothing
/// without a node allocator (`decompose_in`) or a marked CteScan.
pub fn plan<'p>(b: &mut Builder<'p, '_>) {
    let Some(alloc) = b.alloc else { return };
    if b.stmt.subplans.is_nil() {
        return;
    }
    let mut ids: ArenaVec<i32> = ArenaVec::new_in(b.arena); // alloc: Cold (a marked statement's CTEs)
    crate::rerun::each_plan_node(b.stmt, &mut |n| {
        if let Some(cs) = n.as_cte_scan().filter(|c| c.gravity_fill) {
            if !ids.contains(&cs.ctePlanId) {
                ids.push(cs.ctePlanId);
            }
        }
    });
    let mut out: ArenaVec<(i32, Node<'p>)> = ArenaVec::new_in(b.arena); // alloc: Cold
    for &id in ids.iter() {
        let n = (id as usize).checked_sub(1).filter(|&n| n < b.stmt.subplans.len());
        let Some(root) = n.and_then(|n| b.stmt.subplans.nth(n)).filter(|n| n.as_plan().is_some()) else { continue };
        if let Some(q) = readers_or(b, alloc, id) {
            lower(b.facts, alloc, root, q, &mut out);
        }
    }
    for &f in out.iter() {
        b.fill_quals.push(f);
    }
}

/// The filters `plan` (a SeqScan, node `id`) takes: each after the scan's own quals, when every column it reads has a
/// lane (`atts`).
pub fn filters<'p>(b: &Builder<'p, '_>, id: i32, varno: i32, atts: &Atts, open: &mut Open<'p>) {
    for &(n, q) in b.fill_quals.iter() {
        if n != id {
            continue;
        }
        let mut laned = true;
        let known = crate::expr::walk(q, &mut |x| {
            if let Some(v) = x.as_var() {
                laned &= v.varno == varno && v.varlevelsup == 0 && v.varattno > 0 && atts.lane(v.varattno).is_some();
            }
            laned
        });
        if known && laned {
            open.push(Op::Filter(q));
            fires::fire(FIRE); // shapecheck fire signal (D4779): a CTE's fill scan takes its readers' OR
        }
    }
}

/// The OR over every CteScan of CTE `plan_id` of the AND of its filters (`reader_filter`), its columns as the CTE's (a
/// Var's varattno; every reader's varno its own scan's); none when a CteScan has none or one is unmarked.
fn readers_or<'p>(b: &Builder<'p, '_>, alloc: mcx::Mcx<'p>, plan_id: i32) -> Option<Node<'p>> {
    let mut scans: ArenaVec<Node<'p>> = ArenaVec::new_in(b.arena); // alloc: Cold
    crate::rerun::each_plan_node(b.stmt, &mut |n| {
        if n.as_cte_scan().is_some_and(|c| c.ctePlanId == plan_id) {
            scans.push(n);
        }
    });
    let mut arms: ArenaVec<Node<'p>> = ArenaVec::new_in(b.arena); // alloc: Cold
    for &n in scans.iter() {
        let cs = n.as_cte_scan()?;
        if !cs.gravity_fill {
            return None;
        }
        let varno = cs.scan.scanrelid as i32;
        let mut conj: ArenaVec<Node<'p>> = ArenaVec::new_in(b.arena); // alloc: Cold
        for q in cs.scan.plan.qual.iter() {
            each_conjunct(q, &mut |c| {
                if reader_filter(b, c, varno) {
                    conj.push(c);
                }
            });
        }
        arms.push(bool_of(alloc, BoolExprType::AND_EXPR, &conj[..])?);
    }
    bool_of(alloc, BoolExprType::OR_EXPR, &arms[..])
}

/// `q`'s conjuncts (an AND tree flattened).
fn each_conjunct<'p>(q: Node<'p>, f: &mut dyn FnMut(Node<'p>)) {
    crate::walk::stack_check(); // D3748
    match q.as_bool_expr() {
        Some(b) if b.boolop == BoolExprType::AND_EXPR => b.args.iter().for_each(|a| each_conjunct(a, f)),
        _ => f(q),
    }
}

/// Conjunct `c` of a CteScan's quals (scan `varno`) filters that reader alone: it reads one of its columns at least,
/// none of another relation's (or a system or whole-row column), no SubPlan, aggregate, window function or PARAM_EXEC
/// (a correlated or nest-loop parameter: another value per outer row), and it neither acts nor may raise.
fn reader_filter<'p>(b: &Builder<'p, '_>, c: Node<'p>, varno: i32) -> bool {
    let mut cols = 0usize;
    let mut ok = true;
    let known = crate::expr::walk(c, &mut |x| {
        match x.node_tag() {
            NodeTag::T_Var => {
                let v = x.as_var().expect("a Var");
                ok &= v.varno == varno && v.varlevelsup == 0 && v.varattno > 0;
                cols += 1;
            }
            NodeTag::T_Param => ok &= x.as_param().is_some_and(|p| p.paramkind == ParamKind::PARAM_EXTERN),
            NodeTag::T_SubPlan | NodeTag::T_AlternativeSubPlan | NodeTag::T_Aggref | NodeTag::T_WindowFunc | NodeTag::T_GroupingFunc | NodeTag::T_PlaceHolderVar => ok = false,
            _ => {}
        }
        ok
    });
    known && ok && cols > 0 && crate::effects::pure(&crate::effects::Cx::new(b.stmt, b.facts, b.arena), c, &crate::effects::PLAIN)
}

/// `args` as one boolean: none for no argument, the argument itself for one, else a new `boolop` node.
fn bool_of<'p>(alloc: mcx::Mcx<'p>, boolop: BoolExprType, args: &[Node<'p>]) -> Option<Node<'p>> {
    match args {
        [] => None,
        [one] => Some(*one),
        _ => Node::mk(alloc, BoolExpr { boolop, args: NodeList::from_slice(alloc, args).ok()?, location: -1 }).ok(), // alloc: Cold (a fill filter's node)
    }
}

/// Filter `q` over `node`'s output columns (a Var's varattno, whatever its varno) taken as far down `node`'s plan as it
/// passes; each SeqScan it reaches files it over the scan's own columns.
fn lower<'p>(facts: &dyn Facts, alloc: mcx::Mcx<'p>, node: Node<'p>, q: Node<'p>, out: &mut ArenaVec<(i32, Node<'p>)>) {
    crate::walk::stack_check(); // D3748
    let Some(plan) = node.as_plan() else { return };
    let tl = &plan.targetlist;
    let any = |_: i16| true;
    use NodeTag::*;
    match node.node_tag() {
        T_SeqScan => {
            let varno = node.as_seq_scan().map_or(0, |s| s.scan.scanrelid as i32);
            if let Some(f) = through(alloc, q, tl, varno, &any, true) {
                out.push((plan.plan_node_id, f));
            }
        }
        T_Sort | T_IncrementalSort if sort_rewrite(node) => {}
        T_Sort | T_IncrementalSort | T_Material | T_Hash | T_Gather | T_GatherMerge | T_Result => single(facts, alloc, plan, q, &any, out),
        T_SubqueryScan => {
            let s = node.as_subquery_scan().expect("a SubqueryScan");
            if let (Some(sub), Some(f)) = (s.subplan, through(alloc, q, tl, s.scan.scanrelid as i32, &any, false)) {
                lower(facts, alloc, sub, f, out);
            }
        }
        T_Agg => {
            let a = node.as_agg().expect("an Agg");
            let plain = a.aggstrategy == crate::ops::shared::agg::AGG_PLAIN;
            if !plain && a.numCols > 0 && a.groupingSets.is_nil() && a.chain.is_nil() && a.gravity_rewrite.is_none() {
                let key = |j: i16| one_image_key(facts, plan, a.grpColIdx, a.grpOperators, a.grpCollations, j);
                single(facts, alloc, plan, q, &key, out);
            }
        }
        T_Unique => {
            let u = node.as_unique().expect("a Unique");
            let key = |j: i16| one_image_key(facts, plan, u.uniqColIdx, u.uniqOperators, u.uniqCollations, j);
            single(facts, alloc, plan, q, &key, out);
        }
        T_Append | T_MergeAppend => {
            let arms = match node.as_append() {
                Some(a) => &a.appendplans,
                None => &node.as_merge_append().expect("a MergeAppend").mergeplans,
            };
            if let Some(f) = through(alloc, q, tl, OUTER_VAR, &any, false) {
                for arm in arms.iter() {
                    lower(facts, alloc, arm, f, out);
                }
            }
        }
        T_HashJoin | T_NestLoop | T_MergeJoin => {
            let jt = join_type(node);
            let (outer, inner) = match jt {
                Some(JoinType::JOIN_INNER) => (true, true),
                Some(JoinType::JOIN_LEFT | JoinType::JOIN_SEMI | JoinType::JOIN_ANTI) => (true, false),
                Some(JoinType::JOIN_RIGHT | JoinType::JOIN_RIGHT_SEMI | JoinType::JOIN_RIGHT_ANTI) => (false, true),
                _ => (false, false),
            };
            for (on, side, child) in [(outer, OUTER_VAR, plan.lefttree), (inner, INNER_VAR, plan.righttree)] {
                if let (true, Some(child)) = (on, child) {
                    if let Some(f) = through(alloc, q, tl, side, &any, false) {
                        lower(facts, alloc, child, f, out);
                    }
                }
            }
        }
        _ => {}
    }
}

/// A one-input node whose target entries read its input by `OUTER_VAR` (a Result only over an input): `q` read through
/// them, each input column `key` admits.
fn single<'p>(facts: &dyn Facts, alloc: mcx::Mcx<'p>, plan: &'p Plan<'p>, q: Node<'p>, key: &dyn Fn(i16) -> bool, out: &mut ArenaVec<(i32, Node<'p>)>) {
    if let (Some(child), Some(f)) = (plan.lefttree, through(alloc, q, &plan.targetlist, OUTER_VAR, key, false)) {
        lower(facts, alloc, child, f, out);
    }
}

/// A Sort the planner marked for one of Gravity's own rewrites (`GravityRewrite`): left as it is.
fn sort_rewrite(node: Node<'_>) -> bool {
    node.as_sort().map(|s| s.gravity_rewrite.is_some()).or_else(|| node.as_incremental_sort().map(|s| s.sort.gravity_rewrite.is_some())).unwrap_or(true)
}

fn join_type(node: Node<'_>) -> Option<JoinType> {
    node.as_hash_join().map(|j| j.join.jointype).or_else(|| node.as_nest_loop().map(|j| j.join.jointype)).or_else(|| node.as_merge_join().map(|j| j.join.jointype))
}

/// Input column `j` (1-based) of grouping node `plan` is one of its keys `cols` whose equal values are one image under
/// the key's own equality (`ops`, `colls` parallel to `cols`): every row of a group then holds the same value.
fn one_image_key(facts: &dyn Facts, plan: &Plan<'_>, cols: &[i16], ops: &[u32], colls: &[u32], j: i16) -> bool {
    let Some(i) = cols.iter().position(|&c| c == j) else { return false };
    let (Some(&op), Some(&coll)) = (ops.get(i), colls.get(i)) else { return false };
    let Some(ty) = plan.lefttree.and_then(|c| c.as_plan()).and_then(|c| c.targetlist.iter().filter_map(|t| t.as_target_entry()).find(|t| t.resno == j)).and_then(|t| crate::catalog::expr_type::type_of(t.expr)) else {
        return false;
    };
    crate::catalog::keysem::own_equivalence(facts, op, ty) && crate::catalog::keysem::one_image(facts, ty, coll)
}

/// `q` read through target list `tl`: each Var (at this level) is the entry of its varattno, which must be a bare column
/// of input `side` (`key` admitting its column); `last`: the scan's own column, copied, else the entry's Var itself
/// (read as its input's column by varattno one level down). An AND keeps the conjuncts that pass, an OR every arm or
/// none, anything else whole or none.
fn through<'p>(alloc: mcx::Mcx<'p>, q: Node<'p>, tl: &NodeList<'p>, side: i32, key: &dyn Fn(i16) -> bool, last: bool) -> Option<Node<'p>> {
    crate::walk::stack_check(); // D3748
    if let Some(be) = q.as_bool_expr().filter(|b| b.boolop != BoolExprType::NOT_EXPR) {
        let mut out: PgVec<'_, Node<'p>> = mcx::reserved_in(be.args.len(), alloc); // alloc: Cold
        for a in be.args.iter() {
            match through(alloc, a, tl, side, key, last) {
                Some(x) => out.push_in(x),
                None if be.boolop == BoolExprType::AND_EXPR => {}
                None => return None,
            }
        }
        return bool_of(alloc, be.boolop, &out[..]);
    }
    let failed = Cell::new(false);
    let r = remap(alloc, q, tl, side, key, last, &failed);
    match (r, failed.get()) {
        (Ok(x), false) => Some(x.unwrap_or(q)),
        _ => None,
    }
}

fn remap<'p>(alloc: mcx::Mcx<'p>, x: Node<'p>, tl: &NodeList<'p>, side: i32, key: &dyn Fn(i16) -> bool, last: bool, failed: &Cell<bool>) -> types_error::PgResult<Option<Node<'p>>> {
    crate::walk::stack_check(); // D3748
    if failed.get() {
        return Ok(None);
    }
    if let Some(v) = x.as_var().filter(|v| v.varlevelsup == 0) {
        let entry = tl.iter().filter_map(|t| t.as_target_entry()).find(|t| t.resno == v.varattno).map(|t| t.expr);
        let col = entry.and_then(|e| e.as_var()).filter(|c| c.varno == side && c.varlevelsup == 0 && c.varattno > 0 && c.vartype == v.vartype && key(c.varattno));
        return match col {
            None => {
                failed.set(true);
                Ok(None)
            }
            Some(c) if last => Node::mk_var(alloc, c.varno, c.varattno, c.vartype, c.vartypmod, c.varcollid, 0).map(Some), // alloc: Cold
            Some(_) => Ok(entry),
        };
    }
    nodes_core::expression_tree_mutator_dyn(alloc, x, &mut |y| remap(alloc, y, tl, side, key, last, failed))
}
