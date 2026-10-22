//! gravity §3 #4: dependent grouping-key trim. A GROUP BY item that is a
//! function of another GROUP BY item (`x - 1` next to `x` for an integer
//! column `x`, `t || '-x'` next to a text `t`, `(k + 0) - 1` next to
//! `k + 0`, `s::int4 + 5` next to an int2 `s`, `k + $1` in a generic plan),
//! or a second copy of one (`GROUP BY 1, 2` over two entries of
//! the same column), is dropped from `processed_groupClause`, the mechanism
//! `remove_useless_groupby_columns` uses for PK-dependent columns and the
//! pathkey pass uses for constants: the target list keeps the expression and
//! the Agg projects it from the group's representative row, whose key value
//! determines it. The premise is functional dependence: `f` is an
//! immutable built-in over the key and constants (a `$n` is one value for the
//! whole execution), the key is not volatile, and its group equality is image equality (equal
//! keys are one value, so every row of a group gives the same `f(x)`), so the
//! rows are PostgreSQL's. Its errors move: PostgreSQL computes a key below
//! the sort or hash, on every input row, before its first group; the trim
//! computes `f(x)` at the Agg's projection of each group. A dropped `f` that
//! may raise (all but a widening cast and `||`) is dropped only where that
//! move cannot change the statement's outcome (`order_safe`, D2850). At
//! Gravity width 8 and up, the int forms past the direct one stay keys
//! (`INT_FORM_HOLD_WIDTH`): the lone int key they leave is slower there.
use types_error::PgResult;
use types_nodes::parsenodes::{Query, RTEKind};
use types_nodes::{Node, NodeTag};

use gravity_fn::typesem::keysem::image::{dependent_form, Form};

use crate::run::PlannerRun;

/// The Gravity width (`gravity_paths::gravity_width`, as the walk's width term reads it) from which the trim keeps the
/// int forms past the direct one as keys: every form but `||` (`text_form`) and the direct one (`direct_int_form`).
/// Measured basis (a quiet box, 3 takes, A/A 1.003; untrimmed vs trimmed over a high-cardinality int8 key `hk`): at W8
/// a trimmed statement reads at its lone-key twin (`GROUP BY hk, hk::int4 + 5` trimmed 10.84 ms, `GROUP BY hk` 10.88),
/// and that lone int key is 3-8 % slower than the untrimmed two-key form (10.70 vs 9.93 ms; with `-hk` 9.87 vs 9.57;
/// `hk + 0` with `(hk + 0) - 1` 10.21 vs 9.87), so the trim reads 1.092 there. At W0 every int form wins (0.78-0.91),
/// and `||` wins at W0 and W8 (0.64-0.74). Widths 1-7 follow W0. The hold goes when the lone int key's W8 path reads at
/// or under the two-key form.
const INT_FORM_HOLD_WIDTH: u32 = 8;

fn text_form(e: Node<'_>) -> PgResult<bool> {
    let Some(op) = e.as_op_expr() else {
        return Ok(false);
    };
    Ok(dependent_form(if op.opfuncid != 0 { op.opfuncid } else { lsyscache::get_opcode(op.opno)? }) == Some(Form::TextConcat))
}

fn direct_int_form(e: Node<'_>) -> PgResult<bool> {
    let column = |n: Node<'_>| n.as_var().is_some_and(|v| v.varlevelsup == 0);
    if let Some(f) = e.as_func_expr() {
        return Ok(f.args.len() == 1 && matches!(dependent_form(f.funcid), Some(Form::IntCast { .. })) && column(f.args.nth(0)));
    }
    let Some(op) = e.as_op_expr().filter(|o| o.args.len() == 2) else {
        return Ok(false);
    };
    let f = if op.opfuncid != 0 { op.opfuncid } else { lsyscache::get_opcode(op.opno)? };
    if !matches!(dependent_form(f), Some(Form::IntOffset | Form::IntScale)) {
        return Ok(false);
    }
    let (a, b) = (op.args.nth(0), op.args.nth(1));
    Ok(column(a) && b.as_const().is_some() || a.as_const().is_some() && column(b))
}

fn held_at_width(e: Node<'_>, width: u32) -> PgResult<bool> {
    Ok(width >= INT_FORM_HOLD_WIDTH && !text_form(e)? && !direct_int_form(e)?)
}

/// The `prosrc` of a built-in internal function (`None` for anything else: SQL / user functions, aggregates).
pub(crate) fn builtin_src(f: types_core::Oid) -> Option<&'static str> {
    let t = fmgr_core::CANONICAL;
    t.binary_search_by_key(&f, |e| e.0).ok().map(|i| t[i].1)
}

pub fn remove_dependent_groupby_items<'mcx>(run: &mut PlannerRun<'mcx>) -> PgResult<()> {
    let parse = run.parse();
    if run.root.processed_groupClause.len() < 2 || !parse.groupingSets.is_nil() {
        return Ok(());
    }
    let clauses = run.root.processed_groupClause;
    let exprs: Vec<Node<'mcx>> = clauses
        .iter()
        .map(|&id| {
            let sgref = run.root.expr_node(id).as_sort_group_clause().expect("SortGroupClause cell").tleSortGroupRef;
            crate::initsplan::get_sortgroupclause_tle(sgref, &parse.targetList).expr
        })
        .collect();
    let mut verdict = ::mcx::stdvec::try_filled(clauses.len(), 0u8, "groupkey_trim")?;
    let mut class = ::mcx::stdvec::try_filled(clauses.len(), types_core::Oid::default(), "groupkey_trim")?;
    let mut kept_vars: Vec<(i32, i16)> = Vec::new();
    let width = crate::gravity_paths::gravity_width(run);
    for i in 0..exprs.len() {
        if let Some(var) = exprs[i].as_var().filter(|v| v.varlevelsup == 0).map(|v| (v.varno, v.varattno)) {
            if !kept_vars.contains(&var) {
                kept_vars.push(var);
            } else {
                verdict[i] = 1;
            }
            continue;
        }
        // a key: another GROUP BY item, under its relabels (a domain column's `dk - 1` reads `dk` through one); an item
        // is a strict subterm of the one that names it, so no two drop each other. Never a volatile one: PostgreSQL
        // computes each target's copy on every row, so `nextval(..)::text || '-x'` is no function of `nextval(..)::text`.
        let key = |n: Node<'mcx>| -> PgResult<bool> {
            let n = strip_relabel(n);
            for (j, &g) in exprs.iter().enumerate() {
                if j != i && types_nodes::equal(strip_relabel(g), n) {
                    return Ok(image_key(g)? && !clauses::contain_volatile_functions(g)?);
                }
            }
            Ok(false)
        };
        match dependent_of_key(exprs[i], &key)? {
            Some(_) if held_at_width(exprs[i], width)? => {}
            Some(None) => verdict[i] = 2,
            Some(Some(t)) => (verdict[i], class[i]) = (3, t),
            None => {}
        }
    }
    let raising: Vec<(u32, types_core::Oid)> = (0..clauses.len())
        .filter(|&i| verdict[i] == 3)
        .map(|i| (run.root.expr_node(clauses[i]).as_sort_group_clause().expect("SortGroupClause cell").tleSortGroupRef, class[i]))
        .collect();
    let ref_of = |i: usize| run.root.expr_node(clauses[i]).as_sort_group_clause().expect("SortGroupClause cell").tleSortGroupRef;
    let kept_keys: Vec<u32> = (0..clauses.len()).filter(|&i| verdict[i] < 2).map(ref_of).collect();
    if !raising.is_empty() && !order_safe(parse, &raising, &kept_keys)? {
        verdict.iter_mut().filter(|v| **v == 3).for_each(|v| *v = 0);
    }
    let mut kept = mcx::PgVec::new_in(run.mcx);
    kept.extend(clauses.iter().zip(&verdict).filter(|(_, &v)| v == 0).map(|(&id, _)| id));
    let (dropped_duplicate, dropped_injective) = (verdict.contains(&1), verdict.iter().any(|&v| v >= 2));
    if kept.len() != clauses.len() {
        if dropped_duplicate {
            fires::fire("groupkey_trim.duplicate");
        }
        if dropped_injective {
            fires::fire("groupkey_trim.injective");
        }
        run.root.processed_groupClause = ::mcx::freeze_fit(kept);
    }
    Ok(())
}

/// whether dropping the keys `raising` (`(ressortgroupref, expression)`, each an injective function of a key
/// that may raise) leaves the statement's outcome PostgreSQL's. PostgreSQL computes a key below its sort or hash, on
/// every row that passes the quals, and raises at the first that overflows, before its first group; the trim computes
/// it at the Agg's projection, once per projected group. The outcome holds when:
/// - there is no HAVING: a group it rejects is never projected, so its key never raises (`GROUP BY x, x + k HAVING
///   count(*) > 1` returns rows where PostgreSQL raises 22003);
/// - the first ORDER BY key holds an aggregate: no Agg order provides it, so a full sort above the Agg pulls and
///   projects every group before its first row leaves, as PostgreSQL's sort or hash reads every input row before its
///   first group (without one, a LIMIT, a cursor's fetch or an outer query's early stop leaves groups unprojected);
/// - nothing else can raise (`agg_algebra::quiet`: every other target, the aggregates, the jointree's quals; plain
///   tables only; a raise-free form such as `t || c` by `raise_class`), so the first error either way is a dropped
///   key's; a kept key (`kept`, by ressortgroupref) may raise that one message too (`raise_class`: `k + 0` next to
///   `(k + 0) - 1`), as it is computed on every row either way;
/// - the dropped keys raise one type's out of range (`dependent_of_key`'s class), so that error is one message.
///
/// `GROUP BY c, c - 1, .. ORDER BY count(*) DESC LIMIT 10` over a plain table holds all four.
fn order_safe(parse: &Query<'_>, raising: &[(u32, types_core::Oid)], kept: &[u32]) -> PgResult<bool> {
    if parse.havingQual.is_some() {
        return Ok(false);
    }
    let Some(first) = parse.sortClause.iter().next() else { return Ok(false) };
    let sgref = first.as_sort_group_clause().expect("SortGroupClause cell").tleSortGroupRef;
    if !clauses::contain_agg_clause(crate::initsplan::get_sortgroupclause_tle(sgref, &parse.targetList).expr)? {
        return Ok(false);
    }
    let ty = raising[0].1;
    if raising.iter().any(|r| r.1 != ty) {
        return Ok(false);
    }
    for t in parse.targetList.iter() {
        let te = t.as_target_entry().expect("tlist cell");
        let r = te.ressortgroupref;
        if r != 0 && raising.iter().any(|d| d.0 == r) || crate::agg_algebra::quiet(t)? {
            continue;
        }
        match raise_class(te.expr)? {
            Some(None) => {}
            Some(Some(t)) if t == ty && r != 0 && kept.contains(&r) => {}
            _ => return Ok(false),
        }
    }
    fn item_quiet(q: &Query<'_>, n: Node<'_>) -> PgResult<bool> {
        if let Some(r) = n.as_range_tbl_ref() {
            let rte = q.rtable.nth(r.rtindex as usize - 1).as_range_tbl_entry().expect("rtable cell");
            return Ok(matches!(rte.rtekind, RTEKind::RTE_RELATION)
                && rte.relkind != types_rel::RELKIND_FOREIGN_TABLE
                && rte.tablesample.is_none()
                && rte.securityQuals.is_nil());
        }
        if let Some(j) = n.as_join_expr() {
            return Ok(j.quals.map_or(Ok(true), crate::agg_algebra::quiet)? && item_quiet(q, j.larg)? && item_quiet(q, j.rarg)?);
        }
        n.as_from_expr().map_or(Ok(false), |f| from_quiet(q, f))
    }
    fn from_quiet(q: &Query<'_>, f: &types_nodes::primnodes::FromExpr<'_>) -> PgResult<bool> {
        if !f.quals.map_or(Ok(true), crate::agg_algebra::quiet)? {
            return Ok(false);
        }
        for i in f.fromlist.iter() {
            if !item_quiet(q, i)? {
                return Ok(false);
            }
        }
        Ok(true)
    }
    parse.jointree.map_or(Ok(false), |jt| from_quiet(parse, jt))
}

/// The one error a tree of the trim's forms (and int / date arithmetic of any operands) over Vars and Consts can raise:
/// `Some(None)` none (widening casts, `||`), `Some(Some(t))` only result type `t`'s out of range (every call's result is
/// `t`), None for anything else or two types.
fn raise_class(e: Node<'_>) -> PgResult<Option<Option<types_core::Oid>>> {
    let (f, args) = match e.node_tag() {
        NodeTag::T_Var | NodeTag::T_Const => return Ok(Some(None)),
        NodeTag::T_Param if e.as_param().is_some_and(|p| p.paramkind == types_nodes::primnodes::ParamKind::PARAM_EXTERN) => return Ok(Some(None)),
        NodeTag::T_RelabelType => return raise_class(e.as_relabel_type().expect("RelabelType").arg),
        NodeTag::T_FuncExpr => (e.as_func_expr().expect("FuncExpr").funcid, &e.as_func_expr().expect("FuncExpr").args),
        NodeTag::T_OpExpr => {
            let o = e.as_op_expr().expect("OpExpr");
            (if o.opfuncid != 0 { o.opfuncid } else { lsyscache::get_opcode(o.opno)? }, &o.args)
        }
        _ => return Ok(None),
    };
    let mut class = match dependent_form(f) {
        Some(form) if raise_free(f, form) => None,
        Some(_) => Some(nodes_core::expr_type(e)),
        None => return Ok(None),
    };
    for a in args.iter() {
        match raise_class(a)? {
            None => return Ok(None),
            Some(None) => {}
            Some(Some(t)) if class.is_none_or(|c| c == t) => class = Some(t),
            Some(Some(_)) => return Ok(None),
        }
    }
    Ok(Some(class))
}

fn strip_relabel(mut n: Node<'_>) -> Node<'_> {
    while let Some(r) = n.as_relabel_type() {
        n = r.arg;
    }
    n
}

fn image_key(k: Node<'_>) -> PgResult<bool> {
    gravity_fn::typesem::keysem::eq::group_image(nodes_core::expr_type(k), nodes_core::expr_collation(k))
}

pub(crate) fn dependent_of_key<'m>(expr: Node<'m>, is_key: &dyn Fn(Node<'m>) -> PgResult<bool>) -> PgResult<Option<Option<types_core::Oid>>> {
    let over = |x: Node<'m>, own: Option<types_core::Oid>| -> PgResult<Option<Option<types_core::Oid>>> {
        let inner = if is_key(x)? { Some(None) } else { dependent_of_key(strip_relabel(x), is_key)? };
        Ok(match (own, inner) {
            (_, None) => None,
            (o, Some(None)) => Some(o),
            (None, Some(t)) => Some(t),
            (Some(o), Some(Some(t))) => (o == t).then_some(Some(o)),
        })
    };
    let ty = nodes_core::expr_type(expr);
    if let Some(f) = expr.as_func_expr() {
        if let (1, Some(Form::IntCast { widening })) = (f.args.len(), dependent_form(f.funcid)) {
            return over(f.args.nth(0), (!widening).then_some(ty));
        }
        return Ok(None);
    }
    let Some(op) = expr.as_op_expr() else {
        return Ok(None);
    };
    let f = if op.opfuncid != 0 { op.opfuncid } else { lsyscache::get_opcode(op.opno)? };
    let form = dependent_form(f);
    if op.args.len() == 1 {
        return if form == Some(Form::IntNegate) { over(op.args.nth(0), Some(ty)) } else { Ok(None) };
    }
    if op.args.len() != 2 {
        return Ok(None);
    }
    let Some(form @ (Form::IntOffset | Form::IntScale | Form::DateOffset | Form::TextConcat)) = form else {
        return Ok(None);
    };
    let (mul, date) = (form == Form::IntScale, form == Form::DateOffset);
    let constant = |n: Node<'_>| match n.as_const() {
        Some(c) => !c.constisnull && !(mul && c.constvalue.as_i64() == 0),
        None => n.as_param().is_some_and(|p| p.paramkind == types_nodes::primnodes::ParamKind::PARAM_EXTERN),
    };
    let (a, b) = (op.args.nth(0), op.args.nth(1));
    let operand = match (constant(a), constant(b)) {
        (false, true) => a,
        (true, false) if !date => b,
        _ => return Ok(None),
    };
    over(operand, (!raise_free(f, form)).then_some(ty))
}

fn raise_free(f: types_core::Oid, form: Form) -> bool {
    matches!(form, Form::IntCast { widening: true }) || types_core::quiet::call(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn among<'a, 'm>(keys: &'a [Node<'m>]) -> impl Fn(Node<'m>) -> PgResult<bool> + 'a {
        move |n| Ok(keys.iter().any(|&g| types_nodes::equal(g, n)))
    }

    fn op<'m>(mcx: mcx::Mcx<'m>, f: u32, ty: u32, args: &[Node<'m>]) -> Node<'m> {
        let mut l = types_nodes::NodeList::nil();
        for a in args {
            l.lappend(mcx, *a).unwrap();
        }
        Node::mk(mcx, types_nodes::primnodes::OpExpr { opno: 1, opfuncid: f, opresulttype: ty, opretset: false, opcollid: 0, inputcollid: 0, args: l, location: -1 }).unwrap()
    }

    #[test]
    fn dependent_forms_and_their_raises() {
        use types_nodes::primnodes::{Param, ParamKind};
        let cx = mcx::MemoryContext::new("groupkey_trim");
        let mcx = cx.mcx();
        let (int2, int4, date, text) = (21, 23, 1082, 25);
        let k = Node::mk_var(mcx, 1, 1, int4, -1, 0, 0).unwrap();
        let d = Node::mk_var(mcx, 1, 2, date, -1, 0, 0).unwrap();
        let t = Node::mk_var(mcx, 1, 3, text, -1, 100, 0).unwrap();
        let s = Node::mk_var(mcx, 1, 4, int2, -1, 0, 0).unwrap();
        let i4 = |v: i32| Node::mk_const(mcx, int4, -1, 0, 4, datum::Datum::from_i32(v), false, true).unwrap();
        let txt = Node::mk_const(mcx, text, -1, 100, -1, datum::Datum::from_usize(0), false, false).unwrap();
        let param = |kind| Node::mk(mcx, Param { paramkind: kind, paramid: 1, paramtype: int4, paramtypmod: -1, paramcollid: 0, location: -1 }).unwrap();
        let cast = |f: u32, ty: u32, x| Node::mk(mcx, types_nodes::primnodes::FuncExpr { funcid: f, funcresulttype: ty, args: types_nodes::NodeList::make1(mcx, x).unwrap(), ..Default::default() }).unwrap();
        let k0 = op(mcx, 177, int4, &[k, i4(0)]);
        let col_keys = [k, d, t, s];
        let cols = among(&col_keys);
        let raises = |ty| Some(Some(ty));
        assert_eq!(dependent_of_key(op(mcx, 212, int4, &[k]), &cols).unwrap(), raises(int4), "-k");
        assert_eq!(dependent_of_key(op(mcx, 1141, date, &[d, i4(1)]), &cols).unwrap(), raises(date), "d + 1");
        assert_eq!(dependent_of_key(op(mcx, 1142, date, &[d, i4(1)]), &cols).unwrap(), raises(date), "d - 1");
        assert_eq!(dependent_of_key(op(mcx, 1141, date, &[i4(1), d]), &cols).unwrap(), None, "a date's int4 is its right argument");
        assert_eq!(dependent_of_key(op(mcx, 1258, text, &[t, txt]), &cols).unwrap(), Some(None), "t || '-x' raises nothing");
        assert_eq!(dependent_of_key(op(mcx, 1258, text, &[txt, t]), &cols).unwrap(), Some(None), "'-x' || t");
        assert_eq!(dependent_of_key(op(mcx, 181, int4, &[k0, i4(1)]), &cols).unwrap(), raises(int4), "(k + 0) - 1: a form over a form over k");
        assert_eq!(dependent_of_key(op(mcx, 177, int4, &[cast(313, int4, s), i4(5)]), &cols).unwrap(), raises(int4), "s::int4 + 5 (PS4-12)");
        assert_eq!(dependent_of_key(cast(314, int2, op(mcx, 181, int4, &[k, i4(1)])), &cols).unwrap(), None, "(k - 1)::int2: two messages");
        assert_eq!(dependent_of_key(op(mcx, 177, int4, &[k, param(ParamKind::PARAM_EXTERN)]), &cols).unwrap(), raises(int4), "k + $1 (PS4-12)");
        assert_eq!(dependent_of_key(op(mcx, 141, int4, &[k, param(ParamKind::PARAM_EXTERN)]), &cols).unwrap(), raises(int4), "k * $1");
        assert_eq!(dependent_of_key(op(mcx, 177, int4, &[k, param(ParamKind::PARAM_EXEC)]), &cols).unwrap(), None, "an initplan's output is no constant here");
        assert_eq!(dependent_of_key(op(mcx, 141, int4, &[k, i4(0)]), &cols).unwrap(), None, "k * 0");
        assert_eq!(dependent_of_key(op(mcx, 177, int4, &[i4(1), i4(2)]), &cols).unwrap(), None, "no key");
        let k1 = op(mcx, 177, int4, &[k, i4(1)]);
        assert_eq!(dependent_of_key(op(mcx, 177, int4, &[k1, k1]), &cols).unwrap(), None, "no constant side");
        let expr_keys = [k0];
        let exprs = among(&expr_keys);
        assert_eq!(dependent_of_key(op(mcx, 181, int4, &[k0, i4(1)]), &exprs).unwrap(), raises(int4), "(k + 0) - 1 next to k + 0");
        assert_eq!(raise_class(op(mcx, 177, int4, &[k, param(ParamKind::PARAM_EXTERN)])).unwrap(), raises(int4), "a $n reads nothing that raises");
        assert_eq!(raise_class(k0).unwrap(), Some(Some(int4)));
        assert_eq!(raise_class(op(mcx, 181, int4, &[k0, i4(1)])).unwrap(), Some(Some(int4)));
        let k8 = Node::mk(mcx, types_nodes::primnodes::FuncExpr { funcid: 481, funcresulttype: 20, args: types_nodes::NodeList::make1(mcx, k).unwrap(), ..Default::default() }).unwrap();
        assert_eq!(raise_class(k8).unwrap(), Some(None), "a widening cast raises nothing");
        assert_eq!(raise_class(op(mcx, 463, 20, &[k8, k8])).unwrap(), Some(Some(20)));
        let k2 = Node::mk(mcx, types_nodes::primnodes::FuncExpr { funcid: 314, funcresulttype: 21, args: types_nodes::NodeList::make1(mcx, k).unwrap(), ..Default::default() }).unwrap();
        assert_eq!(raise_class(op(mcx, 178, int4, &[k2, i4(1)])).unwrap(), None, "smallint and integer out of range: two messages");
        assert_eq!(raise_class(op(mcx, 1724, 1700, &[k, k])).unwrap(), None, "numeric_add is not a form");
    }

    #[test]
    fn int_forms_past_the_direct_one_are_held_at_width_8() {
        let cx = mcx::MemoryContext::new("groupkey_trim");
        let mcx = cx.mcx();
        let (int2, int4, int8, date, text) = (21, 23, 20, 1082, 25);
        let k = Node::mk_var(mcx, 1, 1, int8, -1, 0, 0).unwrap();
        let d = Node::mk_var(mcx, 1, 2, date, -1, 0, 0).unwrap();
        let t = Node::mk_var(mcx, 1, 3, text, -1, 100, 0).unwrap();
        let s = Node::mk_var(mcx, 1, 4, int2, -1, 0, 0).unwrap();
        let i4 = |v: i32| Node::mk_const(mcx, int4, -1, 0, 4, datum::Datum::from_i32(v), false, true).unwrap();
        let i8 = |v: i64| Node::mk_const(mcx, int8, -1, 0, 8, datum::Datum::from_i64(v), false, true).unwrap();
        let txt = Node::mk_const(mcx, text, -1, 100, -1, datum::Datum::from_usize(0), false, false).unwrap();
        let cast = |f: u32, ty: u32, x| Node::mk(mcx, types_nodes::primnodes::FuncExpr { funcid: f, funcresulttype: ty, args: types_nodes::NodeList::make1(mcx, x).unwrap(), ..Default::default() }).unwrap();
        let direct = [
            ("k - 1", op(mcx, 464, int8, &[k, i8(1)])),
            ("1 - k", op(mcx, 464, int8, &[i8(1), k])),
            ("k * 2", op(mcx, 465, int8, &[k, i8(2)])),
            ("k::int4", cast(480, int4, k)),
            ("s::int4", cast(313, int4, s)),
        ];
        let text_forms = [("t || '-x'", op(mcx, 1258, text, &[t, txt])), ("'p-' || t", op(mcx, 1258, text, &[txt, t]))];
        let past_direct = [
            ("-k", op(mcx, 462, int8, &[k])),
            ("k::int4 + 5", op(mcx, 177, int4, &[cast(480, int4, k), i4(5)])),
            ("(k + 0) - 1", op(mcx, 464, int8, &[op(mcx, 463, int8, &[k, i8(0)]), i8(1)])),
            ("d + 1", op(mcx, 1141, date, &[d, i4(1)])),
        ];
        for (w, held) in [(0, false), (2, false), (7, false), (8, true), (16, true)] {
            for (name, e) in direct.iter().chain(&text_forms) {
                assert!(!held_at_width(*e, w).unwrap(), "{name} at width {w}");
            }
            for (name, e) in &past_direct {
                assert_eq!(held_at_width(*e, w).unwrap(), held, "{name} at width {w}");
            }
        }
    }
}
