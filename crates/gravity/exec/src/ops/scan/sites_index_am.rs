//! M2's any-AM index admission (cards/M2.md L5 index_am, V4R-49): a hash / GiST / SP-GiST index walk or index-only walk,
//! with KNN ORDER BY keys, that M1's btree arm leaves (V4R-36). The recipe is hosts `SiteRecipe::from_plan_am` (null tests
//! as SK_SEARCHNULL keys) plus `AmRecipe` (the ORDER BY keys, their sort support and types); M1's BIND files it
//! (`RecipeTpl::Index`). The host rechecks a lossy row by its scan keys' operators on the key columns and recomputes a
//! lossy distance by its ORDER BY key's operator on the table column (an expression column's value formed by the index's
//! own expression, FormIndexDatum), so the site is admitted only where those are the plan's own expressions (`plain`):
//! otherwise it is refused by name.
use gravity_decompose::graph::KeyForm;
use gravity_decompose::{IndexAm, IndexKey, Source};
use gravity_hosts::ops::scan::index_window::AmRecipe;
use gravity_hosts::SiteRecipe;
use types_core::Oid;
use types_nodes::{Node, NodeList};
use types_storage::lock::NoLock;

use crate::ops::scan::sites_heap::plan_node;
use crate::admit::{refuse, AdmitCx, AdmitResult, RecipeTpl};
use crate::shapes;

pub(crate) fn any_am(cx: &AdmitCx<'_>) -> AdmitResult {
    let Source::Index { iam, keys, .. } = cx.source else { return Ok(Ok(None)) };
    if *iam == IndexAm::Btree || computed(keys) || gravity_lowerv4::rerun::params_key::hosts_bound(keys, &cx.site.param_slots) {
        return Ok(Ok(None));
    }
    Ok(am_recipe(cx)?.map(|s| Some(RecipeTpl::Index(s))).or_else(|why| refuse("hosts", why)))
}

/// D1016 row 4: an AM qual's operand or an ORDER BY argument computed from executor parameters is M8's host cell, which this
/// recipe never binds (v4/m8_param_key.rs builds that walk's recipe with `am_recipe`; m2_admit.rs `bound_array`'s rule).
fn computed(keys: &[IndexKey<'_>]) -> bool {
    keys.iter().any(|k| matches!(k.form, KeyForm::AmOp | KeyForm::OrderBy) && gravity_lowerv4::rerun::params_key::computed(k.value))
}

/// The any-AM walk's recipe (`from_plan_am`, `AmRecipe`), or why not: not the plan's own expressions (`plain`), or the host's.
pub(crate) fn am_recipe(cx: &AdmitCx<'_>) -> types_error::PgResult<Result<SiteRecipe, String>> {
    let Source::Index { scanrelid, indexid, backward, only, .. } = cx.source else { return Ok(Err("an any-AM recipe over another source".into())) };
    let Some(r) = shapes::rte(cx.pstmt, *scanrelid as u32) else { return Ok(Err("the index scan's range table entry".into())) };
    let Some(n) = plan_node(cx.pstmt, cx.node_id) else { return Ok(Err("the index scan's plan node".into())) };
    // (indexqual, what the host's recheck stands for, indexorderby, indexorderbyorig with its ordering operators)
    let (qual, orig, orderby, oborig, obops) = match (n.as_index_scan(), n.as_index_only_scan()) {
        (Some(s), _) => (&s.indexqual, Some(&s.indexqualorig), &s.indexorderby, Some(&s.indexorderbyorig), s.indexorderbyops.iter().collect::<Vec<Oid>>()), // alloc: Program
        (_, Some(s)) => (&s.indexqual, None, &s.indexorderby, None, Vec::new()), // alloc: Program
        _ => return Ok(Err("the index scan's clauses".into())),
    };
    let heap = table::table_open(cx.mcx, cx.site.relation as Oid, NoLock)?;
    let index = indexam::index_open(cx.mcx, *indexid as Oid, r.rellockmode)?;
    let cols = |attno: i16| {
        let heap = index.rd_index.as_ref().filter(|_| attno >= 1).and_then(|f| f.indkey.get(attno as usize - 1).copied()).filter(|&a| a >= 0)?;
        Some((heap, index.rd_indcollation.get(attno as usize - 1).copied().unwrap_or(0)))
    };
    // an expression column's original operand is the index's expression over this scan's range table entry
    let same = |attno: i16, e: Node<'_>| same_expression(cx.mcx, &index, attno, e, *scanrelid as i32).unwrap_or(false);
    // an index-only walk's recheckqual, over the table's columns
    let indkey = index.rd_index.as_ref().map_or(&[][..], |f| &f.indkey[..]);
    let recheck = match n.as_index_only_scan() {
        Some(s) => Some(NodeList::from_slice(cx.mcx, &s.recheckqual.iter().map(|c| table_form(cx.mcx, c, indkey, *scanrelid as i32).map(|m| m.unwrap_or(c))).collect::<Result<Vec<_>, _>>()?)?), // alloc: Program
        None => None,
    };
    let recipe = match plain(&cols, &same, qual, orig.or(recheck.as_ref()), orderby, oborig) {
        Some(why) => Err(why.to_string()), // alloc: Cold
        None => {
            let ops = obops.iter().zip(oborig.into_iter().flat_map(|o| o.iter())).filter_map(|(&op, e)| e.as_op_expr().map(|o| (op, o.opresulttype, o.opcollid))).collect::<Vec<_>>(); // alloc: Program
            SiteRecipe::from_plan_am(&heap, &index, qual, r.rellockmode, cx.attnos)
                .and_then(|mut s| {
                    s.am = Some(AmRecipe::new(&index, orderby, &ops, &mut s.words)?);
                    Ok(s.with_backward(*backward).with_only(*only))
                })
                .map_err(|e| e.message().to_string()) // alloc: Cold
        }
    };
    index.close(NoLock)?;
    heap.close(NoLock)?;
    Ok(recipe)
}

/// Why the host's recheck or distance would not be the plan's own expression, or `None` (`cols(n)`: index column `n`'s
/// table column, 0 for an expression column, and its collation; `same(n, e)`: `e` is expression column `n`'s expression):
/// each clause of `qual` is a null test, or an operator whose left side is an index column in the index column's
/// collation, and its `orig` clause is the same operator (or its commutator, arguments swapped) over that table column or
/// that expression in the same collation; each ORDER BY key is an index column and its `oborig` expression the same
/// operator over that column or expression.
fn plain(cols: &dyn Fn(i16) -> Option<(i16, Oid)>, same: &dyn Fn(i16, Node<'_>) -> bool, qual: &NodeList<'_>, orig: Option<&NodeList<'_>>, orderby: &NodeList<'_>, oborig: Option<&NodeList<'_>>) -> Option<&'static str> {
    let col = |n: Node<'_>| strip(n).as_var().map(|v| (v.varno, v.varattno));
    // `n` is index column `attno` (table column `h`, or 0: its expression, or an index-only walk's read of that column)
    let is = |n: Node<'_>, attno: i16, h: i16| if h > 0 { col(n).is_some_and(|(_, a)| a == h) } else { col(n) == Some((types_nodes::primnodes::INDEX_VAR, attno)) || same(attno, strip(n)) };
    if orig.is_some_and(|o| o.len() != qual.len()) || oborig.is_some_and(|o| o.len() != orderby.len()) {
        return Some("an index walk whose original clauses are not one per index key");
    }
    for (i, q) in qual.iter().enumerate() {
        if q.as_null_test().is_some() {
            continue;
        }
        let Some(op) = q.as_op_expr().filter(|o| o.args.len() == 2) else { return Some("an any-AM index qual that is not an operator") };
        let Some((types_nodes::primnodes::INDEX_VAR, attno)) = col(op.args.nth(0)) else { return Some("an any-AM index qual whose key side is not an index column") };
        let Some((h, icoll)) = cols(attno) else { return Some("an any-AM index qual on an index column that is neither a table column nor an expression") };
        if op.inputcollid != 0 && op.inputcollid != icoll {
            return Some("an any-AM index qual whose collation is not its index column's");
        }
        let Some(o) = orig.map(|l| l.nth(i).as_op_expr()) else { continue };
        let same = o.filter(|o| o.args.len() == 2 && o.inputcollid == op.inputcollid).is_some_and(|o| {
            (o.opno == op.opno && is(o.args.nth(0), attno, h)) || (is(o.args.nth(1), attno, h) && lsyscache::get_commutator(o.opno).is_ok_and(|c| c == op.opno))
        });
        if !same {
            return Some("an any-AM index qual whose original clause is not its operator on the table column");
        }
    }
    for (j, ob) in orderby.iter().enumerate() {
        let Some(op) = ob.as_op_expr().filter(|o| o.args.len() == 2) else { return Some("an ORDER BY index key that is not an operator") };
        let key = col(op.args.nth(0)).filter(|&(v, _)| v == types_nodes::primnodes::INDEX_VAR).and_then(|(_, a)| cols(a).map(|(h, _)| (a, h)));
        let Some((attno, h)) = key else { return Some("an ORDER BY index key that is not an index column") };
        let Some(o) = oborig.map(|l| l.nth(j).as_op_expr()) else { continue };
        if !o.is_some_and(|o| o.opno == op.opno && o.inputcollid == op.inputcollid && is(o.args.nth(0), attno, h)) {
            return Some("an ORDER BY distance that is not its operator on the table column");
        }
    }
    None
}

fn strip(n: Node<'_>) -> Node<'_> {
    stack_depth_core::check_stack_depth_or_panic();
    n.as_relabel_type().map_or(n, |r| strip(r.arg))
}

/// An index-only walk's recheckqual clause `n` over the table: index column `k`'s Var is its table column's Var of
/// `varno` (`indkey`); an expression column's stays (the AM returns that column).
fn table_form<'m>(mcx: mcx::Mcx<'m>, n: Node<'m>, indkey: &[i16], varno: i32) -> types_error::PgResult<Option<Node<'m>>> {
    stack_depth_core::check_stack_depth()?;
    if let Some(v) = n.as_var().filter(|v| v.varno == types_nodes::primnodes::INDEX_VAR && v.varlevelsup == 0) {
        if let Some(&h) = indkey.get((v.varattno as usize).wrapping_sub(1)).filter(|&&h| h > 0) {
            return Ok(Some(Node::mk(mcx, types_nodes::primnodes::Var { varno, varattno: h, varnullingrels: v.varnullingrels.clone_in(mcx)?, ..*v })?));
        }
    }
    clauses::walker::expression_tree_mutator(mcx, n, &mut |c| table_form(mcx, c, indkey, varno))
}

/// `e` (over range table entry `varno`) is index column `attno`'s expression (varno 1): equal once its Vars of `varno`
/// are moved to 1; a Var of another relation or level is not.
fn same_expression(mcx: mcx::Mcx<'_>, index: &types_rel::Relation<'_>, attno: i16, e: Node<'_>, varno: i32) -> types_error::PgResult<bool> {
    let Some(want) = gravity_hosts::ops::scan::index_window::index_expression(mcx, index, attno)? else { return Ok(false) };
    fn moved<'m>(mcx: mcx::Mcx<'m>, n: Node<'m>, varno: i32, other: &mut bool) -> types_error::PgResult<Option<Node<'m>>> {
        stack_depth_core::check_stack_depth()?;
        if let Some(v) = n.as_var() {
            *other |= v.varno != varno || v.varlevelsup != 0;
            let v = types_nodes::primnodes::Var { varno: 1, varnullingrels: v.varnullingrels.clone_in(mcx)?, ..*v };
            return Ok(Some(Node::mk(mcx, v)?));
        }
        clauses::walker::expression_tree_mutator(mcx, n, &mut |c| moved(mcx, c, varno, other))
    }
    let mut other = false;
    let Some(e) = moved(mcx, e, varno, &mut other)? else { return Ok(false) };
    Ok(!other && types_nodes::equal::equal(strip(e), strip(want)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use types_nodes::primnodes::{NullTest, NullTestType, OpExpr, INDEX_VAR};

    const RANGE_OVERLAPS: Oid = 3888;
    const POINT_DISTANCE: Oid = 517;

    fn op<'m>(mcx: mcx::Mcx<'m>, opno: Oid, coll: Oid, l: Node<'m>, r: Node<'m>) -> Node<'m> {
        Node::mk(mcx, OpExpr { opno, opfuncid: 0, opresulttype: 16, opretset: false, opcollid: 0, inputcollid: coll, args: NodeList::make2(mcx, l, r).unwrap(), location: -1 }).unwrap()
    }

    fn list<'m>(mcx: mcx::Mcx<'m>, n: Node<'m>) -> NodeList<'m> {
        NodeList::make1(mcx, n).unwrap()
    }

    /// G4 (main 09-24): the host rechecks a lossy row by its key operators on the key's own column: a table column, or an
    /// expression column's value formed by the index's expression. A plain GiST key, a plain KNN key and either over an
    /// expression column whose original operand is that expression admit, and so does an index-only walk's recheckqual
    /// (its index columns' Vars moved to the table's, `table_form`); an expression column whose original operand is
    /// another expression, a qual collation that is not the index column's, original clauses that are not one per key and
    /// an original clause that is not the key's operator on its column all decline by name.
    #[test]
    fn a_lossy_recheck_is_admitted_only_over_plain_columns() {
        let ctx = mcx::MemoryContext::new("v4r-49");
        let mcx = ctx.mcx();
        let ivar = Node::mk_var(mcx, INDEX_VAR, 1, 3904, -1, 0, 0).unwrap();
        let hvar = Node::mk_var(mcx, 1, 2, 3904, -1, 0, 0).unwrap();
        let other = Node::mk_var(mcx, 1, 3, 3904, -1, 0, 0).unwrap();
        let arg = Node::mk_const(mcx, 3904, -1, 0, -1, datum::Datum::from_u64(0), true, false).unwrap();
        let plain_col = |_: i16| Some((2i16, 0));
        let (no, yes) = (|_: i16, _: Node<'_>| false, |_: i16, _: Node<'_>| true);
        let (q, orig) = (list(mcx, op(mcx, RANGE_OVERLAPS, 0, ivar, arg)), list(mcx, op(mcx, RANGE_OVERLAPS, 0, hvar, arg)));
        let nil = NodeList::nil();
        assert_eq!(plain(&plain_col, &no, &q, Some(&orig), &nil, None), None, "a plain GiST key");
        let (ob, oborig) = (list(mcx, op(mcx, POINT_DISTANCE, 0, ivar, arg)), list(mcx, op(mcx, POINT_DISTANCE, 0, hvar, arg)));
        assert_eq!(plain(&plain_col, &no, &nil, None, &ob, Some(&oborig)), None, "a plain KNN key");
        let expr_col = |_: i16| Some((0i16, 0));
        assert!(plain(&expr_col, &no, &q, Some(&orig), &nil, None).is_some(), "an expression GiST column whose original operand is not its expression declines");
        assert!(plain(&expr_col, &no, &nil, None, &ob, Some(&oborig)).is_some(), "a KNN key whose original operand is not the column's expression declines");
        assert_eq!(plain(&expr_col, &yes, &q, Some(&orig), &nil, None), None, "an expression GiST column over its own expression");
        assert_eq!(plain(&expr_col, &yes, &nil, None, &ob, Some(&oborig)), None, "a KNN key over an expression column's own expression");
        assert_eq!(plain(&expr_col, &no, &q, Some(&q), &nil, None), None, "an index-only walk's recheck reading the expression column itself");
        assert!(plain(&plain_col, &no, &q, Some(&nil), &nil, None).is_some(), "original clauses that are not one per key decline");
        // an index-only walk's recheckqual over the table: index column 1 is table column 2, or an expression column
        let mapped = |indkey: &[i16]| {
            let c = op(mcx, RANGE_OVERLAPS, 0, ivar, arg);
            list(mcx, table_form(mcx, c, indkey, 1).unwrap().unwrap_or(c)) // None: unchanged (an expression column's Var stays)
        };
        assert_eq!(plain(&plain_col, &no, &q, Some(&mapped(&[2])), &nil, None), None, "an index-only recheck on a plain column, over the table");
        assert!(plain(&plain_col, &no, &q, Some(&mapped(&[3])), &nil, None).is_some(), "an index-only recheck on another column declines");
        assert_eq!(plain(&expr_col, &no, &q, Some(&mapped(&[0])), &nil, None), None, "an index-only recheck on a returned expression column");
        let c_coll = |_: i16| Some((2i16, 100));
        let (qc, origc) = (list(mcx, op(mcx, RANGE_OVERLAPS, 950, ivar, arg)), list(mcx, op(mcx, RANGE_OVERLAPS, 950, hvar, arg)));
        assert!(plain(&c_coll, &no, &qc, Some(&origc), &nil, None).is_some(), "a lossy recheck in a collation that is not the index column's declines");
        assert!(plain(&plain_col, &no, &q, Some(&list(mcx, op(mcx, RANGE_OVERLAPS, 0, other, arg))), &nil, None).is_some(), "an original clause on another column declines");
        assert!(plain(&plain_col, &no, &nil, None, &ob, Some(&list(mcx, op(mcx, POINT_DISTANCE, 0, other, arg)))).is_some(), "a distance over another column declines");
        let nt = Node::mk(mcx, NullTest { arg: Some(ivar), nulltesttype: NullTestType::IS_NULL, argisrow: false, location: -1 }).unwrap();
        assert_eq!(plain(&plain_col, &no, &list(mcx, nt), Some(&list(mcx, nt)), &nil, None), None, "a null test key");
    }

    /// D1016 row 4 (the 04:16Z review's F2): a walk with an AM qual's operand or an ORDER BY argument computed from executor
    /// parameters is M8's (`any_am` declines it: Ok(Ok(None))); a constant, a bare parameter (a key word) or an expression
    /// over extern parameters only (bound at bind) stays this arm's.
    #[test]
    fn a_computed_am_key_is_m8s() {
        use types_nodes::primnodes::{Param, ParamKind};
        const POINT_ADD: Oid = 731;
        let ctx = mcx::MemoryContext::new("d1016-row4");
        let mcx = ctx.mcx();
        let param = |paramkind, paramid| Node::mk(mcx, Param { paramkind, paramid, paramtype: 600, paramtypmod: -1, paramcollid: 0, location: -1 }).unwrap();
        let (e, x) = (param(ParamKind::PARAM_EXEC, 0), param(ParamKind::PARAM_EXTERN, 1));
        let c = Node::mk_const(mcx, 600, -1, 0, 16, datum::Datum::from_u64(0), true, false).unwrap();
        let key = |form, value| IndexKey { attno: 1, col: 1, opno: POINT_DISTANCE, value, form };
        assert!(!computed(&[key(KeyForm::AmOp, c), key(KeyForm::OrderBy, c)]), "constant keys");
        assert!(!computed(&[key(KeyForm::AmOp, e), key(KeyForm::OrderBy, x)]), "bare parameters are key words");
        assert!(!computed(&[key(KeyForm::OrderBy, op(mcx, POINT_ADD, 0, x, c))]), "an argument over extern parameters only");
        assert!(computed(&[key(KeyForm::AmOp, op(mcx, POINT_ADD, 0, e, c))]), "a computed qual operand");
        assert!(computed(&[key(KeyForm::AmOp, c), key(KeyForm::OrderBy, op(mcx, POINT_ADD, 0, c, e))]), "a computed ORDER BY argument");
        assert!(computed(&[key(KeyForm::OrderBy, op(mcx, POINT_ADD, 0, x, e))]), "beside an extern parameter (the lowerer refuses it: EXTERN)");
    }
}
