//! The planner's affine aggregate algebra (agg_algebra.rs): sum / avg / count / min / max of `x ± k`, `k ± x`, `k * x`
//! and their chains over int2 / int4 / int8 / numeric columns rewritten to one family of base aggregates per column, the
//! guards that keep PostgreSQL's raises, the shapes it declines, and its cost gate.
use super::*;
use types_nodes::primnodes::Aggref;

use super::plan_shapes::{
    assert_chain, chain, count_star, int_op, max_of, plan_of, plus_one, query, setup, sgc, sum_of, sum_ordered, var,
    INT4MI_OP, INT4MI_PROC, INT4OID, INT4PL_OP, INT4PL_PROC, INT8OID, MAX_INT4, NUMERICOID, SUM_INT4,
};

// sum(x ± k) over an integer column and an integer constant is rewritten to
// CASE WHEN (min(x) OP k) IS NOT NULL AND (max(x) OP k) IS NOT NULL THEN sum(x) ± k * count(x) END
// (M2-plan §9.4: the guard is the original operator, so an overflowing row still raises);
// the shared sum / count / min / max of x deduplicate to four aggregates.
const SUM_INT8: u32 = 2107;
const SUM_INT2: u32 = 2109;
const COUNT_ANY: u32 = 2147;
const MAX_INT8: u32 = 2115;
const MAX_INT2: u32 = 2117;
const MIN_INT8: u32 = 2131;
const MIN_INT4: u32 = 2132;
const MIN_INT2: u32 = 2133;
const INT8PL_OP: u32 = 684;
const INT8MI_OP: u32 = 685;
const INT8MUL_OP: u32 = 686;
const INT84PL_OP: u32 = 1276;
const INT84PL_PROC: u32 = 1274;
const INT48PL_OP: u32 = 1280;
const INT48PL_PROC: u32 = 1278;
const INT24PL_OP: u32 = 552;
const INT24PL_PROC: u32 = 178;
const NUMERIC_ADD_OP: u32 = 1758;
const NUMERIC_MUL_OP: u32 = 1760;
const NUMERIC_DIV_OP: u32 = 1761;
const NUMERIC_DIV_PROC: u32 = 1727;
const INT4MUL_OP: u32 = 514;
const INT4MUL_PROC: u32 = 141;
const AVG_INT4: u32 = 2101;
const INT8_NUMERIC_PROC: u32 = 1781;
const INT8LE_OP: u32 = 414;
const INT8LT_OP: u32 = 412;
const INT8GT_OP: u32 = 413;
const NUMERIC_INT8_PROC: u32 = 1779;
const INT2OID: u32 = 21;

fn int_const(mcx: Mcx<'_>, typ: u32, k: i64) -> Node<'_> {
    let (len, d) = match typ {
        INT2OID => (2, Datum::from_i16(k as i16)),
        INT4OID => (4, Datum::from_i32(k as i32)),
        _ => (8, Datum::from_i64(k)),
    };
    Node::mk_const(mcx, typ, -1, 0, len, d, false, true).unwrap()
}


/// `sum(<expr>)` with an explicit aggregate, argument type and result type; the argument entry
/// carries no resname, as the parser builds it (the rewrite's fresh sum(x) must match it).
fn sum_typed<'m>(mcx: Mcx<'m>, aggfnoid: u32, aggtype: u32, argtype: u32, expr: Node<'m>) -> Node<'m> {
    let mut aggargtypes = types_nodes::list::OidList::nil();
    aggargtypes.lappend(mcx, argtype).unwrap();
    let args = NodeList::make1(mcx, Node::mk_target_entry(mcx, expr, 1, None, false).unwrap()).unwrap();
    Node::mk(mcx, Aggref { aggfnoid, aggtype, aggargtypes, args, ..Aggref::default() }).unwrap()
}

fn aggrefs<'m>(node: Node<'m>, out: &mut Vec<Node<'m>>) {
    match node.node_tag() {
        NodeTag::T_Aggref => out.push(node),
        NodeTag::T_OpExpr => {
            for a in &node.as_op_expr().unwrap().args {
                aggrefs(a, out);
            }
        }
        NodeTag::T_FuncExpr => {
            for a in &node.as_func_expr().unwrap().args {
                aggrefs(a, out);
            }
        }
        NodeTag::T_BoolExpr => {
            for a in &node.as_bool_expr().unwrap().args {
                aggrefs(a, out);
            }
        }
        NodeTag::T_NullTest => aggrefs(node.as_null_test().unwrap().arg.unwrap(), out),
        NodeTag::T_CaseExpr => {
            let c = node.as_case_expr().unwrap();
            for w in &c.args {
                let w = w.as_case_when().unwrap();
                aggrefs(w.expr.unwrap(), out);
                aggrefs(w.result.unwrap(), out);
            }
        }
        _ => {}
    }
}

struct ForcedAffine;
fn forced_affine() -> ForcedAffine {
    crate::agg_algebra::FORCED_IN_TEST.with(|f| f.set(true));
    ForcedAffine
}
impl Drop for ForcedAffine {
    fn drop(&mut self) {
        crate::agg_algebra::FORCED_IN_TEST.with(|f| f.set(false));
    }
}

/// The guarded form: `CASE WHEN (min OP k) IS NOT NULL AND (max OP k) IS NOT NULL THEN <result> ELSE NULL END`;
/// returns (the min guard's OpExpr, the max guard's OpExpr, the result). The ELSE is a typed NULL Const.
fn guarded(expr: Node<'_>) -> (Node<'_>, Node<'_>, Node<'_>) {
    let case = expr.as_case_expr().expect("CASE over the rewrite");
    assert!(case.arg.is_none());
    assert_eq!(case.args.len(), 1);
    let els = case.defresult.expect("ELSE NULL").as_const().expect("typed NULL");
    assert!(els.constisnull && els.consttype == case.casetype);
    let when = case.args.nth(0).as_case_when().unwrap();
    let and = when.expr.unwrap().as_bool_expr().expect("AND of the two guards");
    assert_eq!((and.boolop, and.args.len()), (types_nodes::primnodes::BoolExprType::AND_EXPR, 2));
    let edge = |i: usize| {
        let nt = and.args.nth(i).as_null_test().expect("IS NOT NULL");
        assert_eq!(nt.nulltesttype, types_nodes::primnodes::NullTestType::IS_NOT_NULL);
        nt.arg.unwrap()
    };
    (edge(0), edge(1), when.result.unwrap())
}

fn edge_form(expr: Node<'_>) -> (u32, u32, u32, u32, Node<'_>) {
    let op = expr.as_op_expr().expect("the original operator");
    let (agg, k) = match (op.args.nth(0).node_tag(), op.args.nth(1).node_tag()) {
        (NodeTag::T_Aggref, NodeTag::T_Const) => (op.args.nth(0), op.args.nth(1)),
        (NodeTag::T_Const, NodeTag::T_Aggref) => (op.args.nth(1), op.args.nth(0)),
        other => panic!("guard operands {other:?}"),
    };
    (op.opno, op.opfuncid, op.opresulttype, agg.as_aggref().unwrap().aggfnoid, k)
}

fn agg_tlist_expr<'m>(plan: &Node<'m>, i: usize) -> Node<'m> {
    plan.as_agg().unwrap().plan.targetlist.nth(i).as_target_entry().unwrap().expr
}

/// An int8 form past the capacity cap: `CASE WHEN count(x) <= N THEN <int8 form> ELSE <numeric mod 2^64>::int8 END`,
/// or, for a one-shift `x ± k` (|k| < 2^16, D2862 0b), PostgreSQL's wrap in int8 operators:
/// `CASE WHEN a > MAX - b THEN (a + MIN) + (b + MIN) ELSE a + b END` (`a < MIN - b` when k < 0);
/// returns the int8 form (`a + b` for the wrap). The bare form (under the cap) is returned as is.
fn below_n(expr: Node<'_>) -> Node<'_> {
    let Some(case) = expr.as_case_expr() else { return expr };
    let when = case.args.nth(0).as_case_when().unwrap();
    if let Some(edge) = when.expr.unwrap().as_op_expr().filter(|o| o.opno == INT8GT_OP || o.opno == INT8LT_OP) {
        let lim = edge.args.nth(1).as_op_expr().expect("MAX - b / MIN - b");
        assert_eq!(lim.opno, INT8MI_OP);
        let want = if edge.opno == INT8GT_OP { i64::MAX } else { i64::MIN };
        assert_eq!(lim.args.nth(0).as_const().expect("the int8 edge").constvalue.as_i64(), want);
        assert_eq!(case.casetype, INT8OID);
        let plain = case.defresult.expect("ELSE a + b");
        assert_eq!(plain.as_op_expr().expect("a + b").opno, INT8PL_OP);
        return plain;
    }
    let Some(le) = when.expr.unwrap().as_op_expr().filter(|o| o.opno == INT8LE_OP) else { return expr };
    assert_eq!(le.args.nth(0).as_aggref().expect("count(x) <= N").aggfnoid, COUNT_ANY);
    let els = case.defresult.expect("ELSE the wrapped numeric S");
    if case.casetype == INT8OID {
        assert_eq!(els.as_func_expr().expect("numeric_int8").funcid, NUMERIC_INT8_PROC);
    }
    when.result.unwrap()
}

fn shifted_form(expr: Node<'_>) -> (u32, u32, Node<'_>, u32, u32) {
    let (_, _, expr) = guarded(expr);
    let expr = below_n(expr);
    let outer = expr.as_op_expr().expect("outer op");
    let sum = outer.args.nth(0).as_aggref().expect("sum(x) first");
    let mul = outer.args.nth(1).as_op_expr().expect("k * count(x)");
    let k = mul.args.nth(0);
    let mut cnt = Vec::new();
    aggrefs(mul.args.nth(1), &mut cnt);
    assert_eq!(cnt.len(), 1, "one count(x) under the product");
    (outer.opno, mul.opno, k, sum.aggfnoid, cnt[0].as_aggref().unwrap().aggfnoid)
}

#[test]
fn affine_sum_of_column_plus_constant_is_rewritten_and_deduplicated() {
    let _forced = forced_affine();
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let plus = |k| int_op(mcx, INT4PL_OP, INT4PL_PROC, INT4OID, var(mcx, 2), int_const(mcx, INT4OID, k));
    let parse = query(
        mcx,
        None,
        &[
            (sum_typed(mcx, SUM_INT4, INT8OID, INT4OID, var(mcx, 2)), "s0", 0),
            (sum_of(mcx, plus(1), false), "s1", 0),
            (sum_of(mcx, plus(2), false), "s2", 0),
            (sum_of(mcx, int_op(mcx, INT4PL_OP, INT4PL_PROC, INT4OID, int_const(mcx, INT4OID, 3), var(mcx, 2)), false), "s3", 0),
        ],
        &[],
        None,
    );
    let plan = plan_of(mcx, parse, "SELECT sum(val), sum(val + 1), sum(val + 2), sum(3 + val) FROM t");
    assert_chain(&plan, &["Agg", "Scan"]);
    assert_eq!(agg_tlist_expr(&plan, 0).as_aggref().unwrap().aggfnoid, SUM_INT4, "sum(val) as written");
    for (i, k) in [(1usize, 1i64), (2, 2), (3, 3)] {
        let (op, mul, kc, sum_fn, count_fn) = shifted_form(agg_tlist_expr(&plan, i));
        assert_eq!((op, mul, sum_fn, count_fn), (INT8PL_OP, INT8MUL_OP, SUM_INT4, COUNT_ANY), "entry {i}");
        let kc = kc.as_const().expect("k is a Const");
        assert_eq!((kc.consttype, kc.constvalue.as_i64()), (INT8OID, k), "k as int8, entry {i}");
        let (lo, hi, _) = guarded(agg_tlist_expr(&plan, i));
        for (edge, agg_fn) in [(lo, MIN_INT4), (hi, MAX_INT4)] {
            let (opno, opfuncid, restype, aggfnoid, kc) = edge_form(edge);
            assert_eq!((opno, opfuncid, restype, aggfnoid), (INT4PL_OP, INT4PL_PROC, INT4OID, agg_fn), "guard, entry {i}");
            let kc = kc.as_const().unwrap();
            assert_eq!((kc.consttype, i64::from(kc.constvalue.as_i32())), (INT4OID, k), "guard k, entry {i}");
        }
    }
    // Four aggregates for the four entries: every sum(val) shares aggno 0, every count(val) aggno 1,
    // and the three guarded entries share one min(val) and one max(val). An int4 x is past the capacity cap; a one-shift
    // member's past-N value is PostgreSQL's int8 wrap (D2862 0b): each entry reads min, max, and sum / count in the
    // wrap's edge test and in its wrapped arm (`aggrefs` does not walk a CASE's ELSE, the plain sum + k * count).
    let mut all = Vec::new();
    for i in 0..4 {
        aggrefs(agg_tlist_expr(&plan, i), &mut all);
    }
    assert_eq!(all.len(), 1 + 3 * 6, "min, max, (sum, count) x 2 per guarded entry");
    let mut aggnos: Vec<i32> = all.iter().map(|a| a.as_aggref().unwrap().aggno).collect();
    aggnos.sort_unstable();
    aggnos.dedup();
    assert_eq!(aggnos, vec![0, 1, 2, 3], "the Agg carries four aggregates");
    let by_fn = |f: u32| all.iter().filter(move |a| a.as_aggref().unwrap().aggfnoid == f).map(|a| a.as_aggref().unwrap().aggno);
    assert_eq!(by_fn(MIN_INT4).collect::<std::collections::HashSet<_>>().len(), 1, "one min(val)");
    assert_eq!(by_fn(MAX_INT4).collect::<std::collections::HashSet<_>>().len(), 1, "one max(val)");
}

/// The guard over no rows through the planner's own constant folder (the aggregates stand in as
/// Consts): min / max are NULL, int4pl is strict, the projection folds to the typed NULL that
/// `sum(x + 1)` is. The raise at the edge is the executor's (e2e/gravity/v4/gen_affine_r1.sql: the test world
/// pins the fold's evaluator to a panic and has no executor seams).
#[test]
fn affine_guard_is_the_typed_null_over_no_rows() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let chain = crate::agg_algebra::Chain::of(plus_one(mcx, var(mcx, 2))).unwrap().expect("x + 1 is a chain");
    let null = || Node::mk_const(mcx, INT4OID, -1, 0, 4, Datum::null(), true, true).unwrap();
    let result = int_const(mcx, INT8OID, 42);
    let else_null = Node::mk_const(mcx, INT8OID, -1, 0, 8, Datum::null(), true, true).unwrap();
    let case = crate::agg_algebra::guarded(mcx, &chain, null(), null(), result, INT8OID, else_null).unwrap();
    let folded = clauses::eval_const_expressions(mcx, case).unwrap();
    let c = folded.as_const().expect("folded to a Const");
    assert!(c.constisnull && c.consttype == INT8OID, "NULL::int8");
}

#[test]
fn affine_subtraction_forms_fold_the_sign() {
    let _forced = forced_affine();
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let parse = query(
        mcx,
        None,
        &[
            (sum_of(mcx, int_op(mcx, INT4MI_OP, INT4MI_PROC, INT4OID, var(mcx, 2), int_const(mcx, INT4OID, 5)), false), "a", 0),
            (sum_of(mcx, int_op(mcx, INT4MI_OP, INT4MI_PROC, INT4OID, int_const(mcx, INT4OID, 7), var(mcx, 2)), false), "b", 0),
            (sum_of(mcx, int_op(mcx, INT4MI_OP, INT4MI_PROC, INT4OID, var(mcx, 2), int_const(mcx, INT4OID, 6)), false), "c", 0),
        ],
        &[],
        None,
    );
    let plan = plan_of(mcx, parse, "SELECT sum(val - 5), sum(7 - val), sum(val - 6) FROM t");
    let (op, mul, kc, _, _) = shifted_form(agg_tlist_expr(&plan, 0));
    assert_eq!((op, mul, kc.as_const().unwrap().constvalue.as_i64()), (INT8PL_OP, INT8MUL_OP, -5));
    let (lo, _, _) = guarded(agg_tlist_expr(&plan, 0));
    let (opno, _, _, aggfnoid, kc) = edge_form(lo);
    assert_eq!((opno, aggfnoid, kc.as_const().unwrap().constvalue.as_i32()), (INT4MI_OP, MIN_INT4, 5));
    assert_eq!(lo.as_op_expr().unwrap().args.nth(0).node_tag(), NodeTag::T_Aggref, "var first");
    let (lo, hi, result) = guarded(agg_tlist_expr(&plan, 1));
    for (edge, agg_fn) in [(lo, MIN_INT4), (hi, MAX_INT4)] {
        let (opno, _, _, aggfnoid, kc) = edge_form(edge);
        assert_eq!((opno, aggfnoid, kc.as_const().unwrap().constvalue.as_i32()), (INT4MI_OP, agg_fn, 7));
        assert_eq!(edge.as_op_expr().unwrap().args.nth(0).node_tag(), NodeTag::T_Const, "const first");
    }
    let outer = below_n(result).as_op_expr().unwrap();
    assert_eq!(outer.opno, INT8MI_OP);
    let mul = outer.args.nth(0).as_op_expr().expect("k * count(x) first");
    assert_eq!((mul.opno, mul.args.nth(0).as_const().unwrap().constvalue.as_i64()), (INT8MUL_OP, 7));
    assert_eq!(outer.args.nth(1).as_aggref().unwrap().aggfnoid, SUM_INT4);
}

#[test]
fn affine_int2_column_uses_sum_int2() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    // A SMALLINT column: int24pl → int4, sum(int4) → int8; the rewrite uses sum(int2) → int8. Two A70 sums
    // (finals.cse's threshold) carry the guard although smallint + 89 cannot leave int4.
    let x = || Node::mk_var(mcx, 1, 2, INT2OID, -1, 0, 0).unwrap();
    let shift = |k| sum_typed(mcx, SUM_INT4, INT8OID, INT4OID, int_op(mcx, INT24PL_OP, INT24PL_PROC, INT4OID, x(), int_const(mcx, INT4OID, k)));
    let parse = query(mcx, None, &[(shift(89), "s", 0), (shift(90), "t", 0)], &[], None);
    let plan = plan_of(mcx, parse, "SELECT sum(w + 89), sum(w + 90) FROM t");
    let (op, _, _, sum_fn, count_fn) = shifted_form(agg_tlist_expr(&plan, 0));
    assert_eq!((op, sum_fn, count_fn), (INT8PL_OP, SUM_INT2, COUNT_ANY));
    let (lo, hi, _) = guarded(agg_tlist_expr(&plan, 0));
    let (opno, opfuncid, restype, aggfnoid, kc) = edge_form(lo);
    assert_eq!((opno, opfuncid, restype, aggfnoid), (INT24PL_OP, INT24PL_PROC, INT4OID, MIN_INT2));
    assert_eq!(edge_form(hi).3, MAX_INT2);
    assert_eq!(kc.as_const().unwrap().consttype, INT4OID);
}

#[test]
fn affine_agg_marks_its_finals() {
    let _forced = forced_affine();
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let no = |n: Node<'_>| n.as_aggref().unwrap().aggno;
    let check = |plan: &Node<'_>, want: &[i16]| {
        let finals = plan.as_agg().unwrap().gravity_finals;
        assert_eq!(finals.iter().map(|f| f.resno).collect::<Vec<_>>(), want);
        for f in finals {
            let e = agg_tlist_expr(plan, f.resno as usize - 1);
            let (lo, hi, value) = guarded(e);
            let (mut l, mut h) = (Vec::new(), Vec::new());
            aggrefs(lo, &mut l);
            aggrefs(hi, &mut h);
            assert_eq!((no(l[0]), no(h[0])), (f.min, f.max), "the guard's min / max, entry {}", f.resno);
            let outer = below_n(value).as_op_expr().expect("the value's int8 form");
            let (sum, prod) = match outer.args.nth(0).as_aggref() {
                Some(_) => (outer.args.nth(0), outer.args.nth(1)),
                None => (outer.args.nth(1), outer.args.nth(0)),
            };
            let mut c = Vec::new();
            aggrefs(prod, &mut c);
            assert_eq!((no(sum), no(c[0])), (f.sum, f.count), "the value's sum / count, entry {}", f.resno);
        }
    };
    let plus = |k| int_op(mcx, INT4PL_OP, INT4PL_PROC, INT4OID, var(mcx, 2), int_const(mcx, INT4OID, k));
    let parse = query(
        mcx,
        None,
        &[
            (sum_typed(mcx, SUM_INT4, INT8OID, INT4OID, var(mcx, 2)), "s0", 0),
            (sum_of(mcx, plus(1), false), "s1", 0),
            (sum_of(mcx, int_op(mcx, INT4MI_OP, INT4MI_PROC, INT4OID, int_const(mcx, INT4OID, 7), var(mcx, 2)), false), "s2", 0),
            (sum_of(mcx, plus(3), false), "s3", 0),
        ],
        &[],
        None,
    );
    let plan = plan_of(mcx, parse, "SELECT sum(val), sum(val + 1), sum(7 - val), sum(val + 3) FROM t");
    check(&plan, &[2, 3, 4]);
    let x = || Node::mk_var(mcx, 1, 2, INT2OID, -1, 0, 0).unwrap();
    let shift = |k| sum_typed(mcx, SUM_INT4, INT8OID, INT4OID, int_op(mcx, INT24PL_OP, INT24PL_PROC, INT4OID, x(), int_const(mcx, INT4OID, k)));
    let parse = query(mcx, None, &[(shift(89), "s", 0), (shift(90), "t", 0)], &[], None);
    check(&plan_of(mcx, parse, "SELECT sum(w + 89), sum(w + 90) FROM t"), &[1, 2]);
}

#[test]
fn affine_int8_column_rewrites_in_numeric() {
    let _forced = forced_affine();
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let x = || Node::mk_var(mcx, 1, 2, INT8OID, -1, 0, 0).unwrap();
    let shift = |k| sum_typed(mcx, SUM_INT8, NUMERICOID, INT8OID, int_op(mcx, INT84PL_OP, INT84PL_PROC, INT8OID, x(), int_const(mcx, INT4OID, k)));
    let parse = query(mcx, None, &[(shift(1), "s", 0), (shift(2), "t", 0), (shift(3), "u", 0)], &[], None);
    let plan = plan_of(mcx, parse, "SELECT sum(x8 + 1), sum(x8 + 2), sum(x8 + 3) FROM t");
    let expr = agg_tlist_expr(&plan, 0);
    let (op, mul, kc, sum_fn, count_fn) = shifted_form(expr);
    assert_eq!((op, mul, sum_fn, count_fn), (NUMERIC_ADD_OP, NUMERIC_MUL_OP, SUM_INT8, COUNT_ANY));
    let kc = kc.as_func_expr().expect("k::numeric");
    assert_eq!((kc.funcid, kc.funcresulttype), (INT8_NUMERIC_PROC, NUMERICOID));
    let k8 = kc.args.nth(0).as_const().expect("k, an int8 literal");
    assert_eq!((k8.consttype, k8.constbyval, k8.constvalue.as_i64()), (INT8OID, true, 1));
    let (lo, hi, result) = guarded(expr);
    let cast = result.as_op_expr().unwrap().args.nth(1).as_op_expr().unwrap().args.nth(1).as_func_expr().unwrap();
    assert_eq!((cast.funcid, cast.funcresulttype), (INT8_NUMERIC_PROC, NUMERICOID));
    // A row at the int8 edge raises in PostgreSQL too: the guard is int84pl over min(x8) / max(x8).
    let (opno, opfuncid, restype, aggfnoid, _) = edge_form(lo);
    assert_eq!((opno, opfuncid, restype, aggfnoid), (INT84PL_OP, INT84PL_PROC, INT8OID, MIN_INT8));
    assert_eq!(edge_form(hi).3, MAX_INT8);
    assert_eq!(expr.as_case_expr().unwrap().casetype, NUMERICOID);
}

#[test]
fn affine_rewrite_declines_the_other_shapes() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let is_sum_as_written = |plan: &Node<'_>, i: usize, aggfnoid: u32| {
        let a = agg_tlist_expr(plan, i).as_aggref().expect("Aggref as written");
        assert_eq!(a.aggfnoid, aggfnoid, "entry {i} keeps its aggregate");
    };
    let non_const = int_op(mcx, INT4PL_OP, INT4PL_PROC, INT4OID, var(mcx, 2), var(mcx, 1));
    let distinct = sum_of(mcx, plus_one(mcx, var(mcx, 2)), true);
    let parse = query(mcx, None, &[(sum_of(mcx, non_const, false), "a", 0), (distinct, "d", 1)], &[], None);
    let plan = plan_of(mcx, parse, "SELECT sum(val + pk), sum(DISTINCT val + 1) FROM t");
    assert_chain(&plan, &["Agg", "Sort", "Scan"]);
    is_sum_as_written(&plan, 0, SUM_INT4);
    is_sum_as_written(&plan, 1, SUM_INT4);
}

// GENERALIZE R1 (PL-01 / FOLD-06 / SRC-40), PSP2-06: a raise-free member under a FILTER that raises on no row and calls
// only builtins is rewritten, each base aggregate carrying the FILTER (PostgreSQL runs it once per transition, so a
// second run is unobservable); three raising ones stay as written (with the guard's min / max: four FILTERed bases); an
// int8 k widens the sum to numeric (sum(val) as numeric + k * count(val)); int2 / int4 levels need no guard where they
// cannot raise.
#[test]
fn affine_filters_stay_and_widened_constant_rewrite() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let filtered = |k| {
        let a = sum_of(mcx, int_op(mcx, INT4PL_OP, INT4PL_PROC, INT4OID, var(mcx, 2), int_const(mcx, INT4OID, k)), false);
        let tru = Node::mk_const(mcx, 16, -1, 0, 1, Datum::from_bool(true), false, true).unwrap();
        // SAFETY: freshly built node.
        unsafe { a.with_mut::<Aggref, _>(|a| a.aggfilter = Some(tru)) }.unwrap();
        a
    };
    let int8_k = |k| sum_typed(mcx, SUM_INT8, NUMERICOID, INT8OID, int_op(mcx, INT48PL_OP, INT48PL_PROC, INT8OID, var(mcx, 2), int_const(mcx, INT8OID, k)));
    let w = || Node::mk_var(mcx, 1, 2, INT2OID, -1, 0, 0).unwrap();
    let quiet_filtered = |k| {
        let a = sum_typed(mcx, SUM_INT4, INT8OID, INT4OID, int_op(mcx, INT24PL_OP, INT24PL_PROC, INT4OID, w(), int_const(mcx, INT4OID, k)));
        let tru = Node::mk_const(mcx, 16, -1, 0, 1, Datum::from_bool(true), false, true).unwrap();
        // SAFETY: freshly built node.
        unsafe { a.with_mut::<Aggref, _>(|a| a.aggfilter = Some(tru)) }.unwrap();
        a
    };
    let parse = query(
        mcx,
        None,
        &[
            (filtered(1), "c", 0),
            (filtered(2), "d", 0),
            (filtered(3), "e", 0),
            (int8_k(1), "b", 0),
            (int8_k(2), "f", 0),
            (quiet_filtered(1), "g", 0),
            (quiet_filtered(2), "h", 0),
            (quiet_filtered(3), "i", 0),
        ],
        &[],
        None,
    );
    let plan = plan_of(
        mcx,
        parse,
        "SELECT sum(val + 1) FILTER (WHERE true), sum(val + 2) FILTER (WHERE true), sum(val + 3) FILTER (WHERE true), \
         sum(val + 1::int8), sum(val + 2::int8), sum(w + 1) FILTER (WHERE true), sum(w + 2) FILTER (WHERE true), \
         sum(w + 3) FILTER (WHERE true) FROM t",
    );
    for i in 0..3 {
        let a = agg_tlist_expr(&plan, i).as_aggref().expect("a raising FILTER member: as written");
        assert_eq!(a.aggfnoid, SUM_INT4, "entry {i}");
        assert!(a.aggfilter.is_some(), "entry {i}");
    }
    for i in 5..8 {
        let add = agg_tlist_expr(&plan, i).as_op_expr().expect("no guard: sum(w) + k * count(w)");
        assert_eq!(add.opno, INT8PL_OP, "entry {i}");
        let sum = add.args.nth(0).as_aggref().expect("sum(w)");
        assert_eq!(sum.aggfnoid, SUM_INT2, "entry {i}");
        assert!(sum.aggfilter.is_some(), "entry {i}: the base aggregates carry the member's FILTER");
    }
    // val + 1::int8 cannot leave int8, and PostgreSQL sums it exactly: sum(val::int8) + 1 * count(val)::numeric, with
    // no guard (not A70's shape, which finals.cse reads).
    let add = agg_tlist_expr(&plan, 3).as_op_expr().expect("numeric +");
    assert_eq!(add.opno, NUMERIC_ADD_OP);
    let sum8 = add.args.nth(0).as_aggref().expect("sum(val::int8), numeric");
    assert_eq!(sum8.aggfnoid, SUM_INT8);
    let cast = sum8.args.nth(0).as_target_entry().unwrap().expr.as_func_expr().expect("val::int8");
    assert_eq!(cast.funcresulttype, INT8OID);
}

#[test]
fn affine_raising_filter_family_from_four_members() {
    let _forced = forced_affine();
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let filtered = |k| {
        let a = sum_of(mcx, int_op(mcx, INT4PL_OP, INT4PL_PROC, INT4OID, var(mcx, 2), int_const(mcx, INT4OID, k)), false);
        let tru = Node::mk_const(mcx, 16, -1, 0, 1, Datum::from_bool(true), false, true).unwrap();
        // SAFETY: freshly built node.
        unsafe { a.with_mut::<Aggref, _>(|a| a.aggfilter = Some(tru)) }.unwrap();
        a
    };
    let names = ["a", "b", "c", "d"];
    let three: Vec<_> = (0..3).map(|i| (filtered(i as i64 + 1), names[i], 0)).collect();
    let plan = plan_of(
        mcx,
        query(mcx, None, &three, &[], None),
        "SELECT sum(val + 1) FILTER (WHERE true), sum(val + 2) FILTER (WHERE true), sum(val + 3) FILTER (WHERE true) \
         FROM t",
    );
    for i in 0..3 {
        let a = agg_tlist_expr(&plan, i).as_aggref().expect("three raising FILTER members: as written");
        assert_eq!(a.aggfnoid, SUM_INT4, "entry {i}");
    }
    let four: Vec<_> = (0..4).map(|i| (filtered(i as i64 + 1), names[i], 0)).collect();
    let plan = plan_of(
        mcx,
        query(mcx, None, &four, &[], None),
        "SELECT sum(val + 1) FILTER (WHERE true), sum(val + 2) FILTER (WHERE true), sum(val + 3) FILTER (WHERE true), \
         sum(val + 4) FILTER (WHERE true) FROM t",
    );
    for i in 0..4 {
        let (lo, hi, _) = guarded(agg_tlist_expr(&plan, i));
        for (edge, f) in [(lo, MIN_INT4), (hi, MAX_INT4)] {
            let (opno, _, _, aggfnoid, _) = edge_form(edge);
            assert_eq!((opno, aggfnoid), (INT4PL_OP, f), "entry {i}");
        }
        let edge = lo.as_op_expr().unwrap().args.iter().find_map(|n| n.as_aggref()).unwrap();
        assert!(edge.aggfilter.is_some(), "entry {i}: the guard's min carries the FILTER");
    }
}

#[test]
fn affine_raising_family_beside_a_filtered_count() {
    let _forced = forced_affine();
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let shifted = |k| {
        let e = int_op(mcx, INT4PL_OP, INT4PL_PROC, INT4OID, var(mcx, 2), int_const(mcx, INT4OID, k));
        sum_of(mcx, e, false)
    };
    let counted = |f| {
        let c = count_star(mcx);
        // SAFETY: freshly built node.
        unsafe { c.with_mut::<Aggref, _>(|a| a.aggfilter = Some(f)) }.unwrap();
        c
    };
    let tru = Node::mk_const(mcx, 16, -1, 0, 1, Datum::from_bool(true), false, true).unwrap();
    let raising = int_op(mcx, INT4_GT_OP, 147, 16, plus_one(mcx, var(mcx, 2)), int_const(mcx, INT4OID, 0));
    let random = Node::mk(
        mcx,
        types_nodes::primnodes::FuncExpr {
            funcid: 1598,
            funcresulttype: 701,
            funcretset: false,
            funcvariadic: false,
            funcformat: types_nodes::primnodes::CoercionForm::COERCE_EXPLICIT_CALL,
            funccollid: 0,
            inputcollid: 0,
            args: NodeList::nil(),
            location: -1,
        },
    )
    .unwrap();
    let half = Node::mk_const(mcx, 701, -1, 0, 8, Datum::from_f64(0.5), false, true).unwrap();
    let draw = int_op(mcx, 672, 295, 16, random, half);
    let cases = [(tru, true, "true"), (raising, false, "val + 1 > 0"), (draw, false, "random() < 0.5")];
    for (filter, rewritten, what) in cases {
        let tlist = [(counted(filter), "n", 0), (shifted(1), "a", 0), (shifted(2), "b", 0), (shifted(3), "c", 0)];
        let parse = query(mcx, None, &tlist, &[], None);
        let sql = format!("SELECT count(*) FILTER (WHERE {what}), sum(val + 1), sum(val + 2), sum(val + 3) FROM t");
        let plan = plan_of(mcx, parse, &sql);
        let n = agg_tlist_expr(&plan, 0).as_aggref().expect("the count as written");
        assert!(n.aggfilter.is_some(), "{what}");
        for i in 1..4 {
            let e = agg_tlist_expr(&plan, i);
            if rewritten {
                let _ = guarded(e);
            } else {
                assert_eq!(e.as_aggref().expect("as written").aggfnoid, SUM_INT4, "{what}: entry {i}");
            }
        }
    }
}

// avg(e) = numeric_div(S(e), count(x)) (never a * avg(x) + b: select_div_scale reads the dividend's weight);
// sum(k * x) = k * sum(x). Both raising: under the guard.
#[test]
fn affine_avg_and_product_forms() {
    let _forced = forced_affine();
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let avg = |k| sum_typed(mcx, AVG_INT4, NUMERICOID, INT4OID, int_op(mcx, INT4PL_OP, INT4PL_PROC, INT4OID, var(mcx, 2), int_const(mcx, INT4OID, k)));
    let times = |k| sum_of(mcx, int_op(mcx, INT4MUL_OP, INT4MUL_PROC, INT4OID, int_const(mcx, INT4OID, k), var(mcx, 2)), false);
    let parse = query(mcx, None, &[(avg(1), "a", 0), (times(3), "m", 0), (avg(2), "b", 0), (times(4), "n", 0)], &[], None);
    let plan = plan_of(mcx, parse, "SELECT avg(val + 1), sum(3 * val), avg(val + 2), sum(4 * val) FROM t");
    let (_, _, result) = guarded(agg_tlist_expr(&plan, 0));
    let div = result.as_op_expr().expect("numeric_div");
    assert_eq!((div.opno, div.opfuncid), (NUMERIC_DIV_OP, NUMERIC_DIV_PROC));
    // the dividend is int8_avg's: S(e) in int8, converted once. An int4 x is past the capacity cap; for a
    // one-shift x + k (|k| < 2^16, 0b) S(e) is PostgreSQL's wrapping int8 accumulator spelled in int8 operators that
    // cannot overflow (agg_algebra::wrap_add), whose plain arm is sum(val) + k * count(val).
    let s = div.args.nth(0).as_func_expr().expect("S(e)::numeric");
    assert_eq!((s.funcid, s.funcresulttype), (INT8_NUMERIC_PROC, NUMERICOID));
    assert_eq!(s.args.nth(0).as_case_expr().expect("a > MAX - b").casetype, INT8OID);
    let s8 = below_n(s.args.nth(0)).as_op_expr().expect("S(e) in int8");
    assert_eq!(s8.opresulttype, INT8OID);
    assert_eq!(s8.args.nth(0).as_aggref().expect("sum(val)").aggfnoid, SUM_INT4);
    let count = div.args.nth(1).as_func_expr().expect("count(val) as numeric");
    assert_eq!(count.args.nth(0).as_aggref().unwrap().aggfnoid, COUNT_ANY);
    let (lo, _, result) = guarded(agg_tlist_expr(&plan, 1));
    assert_eq!(edge_form(lo).0, INT4MUL_OP, "the guard is the original int4mul");
    let mul = below_n(result).as_op_expr().expect("k * sum(val)");
    assert_eq!(mul.opno, INT8MUL_OP);
    assert_eq!(mul.args.nth(0).as_const().unwrap().constvalue.as_i64(), 3);
    assert_eq!(mul.args.nth(1).as_aggref().unwrap().aggfnoid, SUM_INT4);
    // A shift past 2^16 keeps below_n's CASE: count(val) <= N, else PostgreSQL's accumulator in numeric mod 2^64.
    let parse = query(mcx, None, &[(avg(70000), "a", 0), (times(3), "m", 0), (avg(70001), "b", 0), (times(4), "n", 0)], &[], None);
    let plan = plan_of(mcx, parse, "SELECT avg(val + 70000), sum(3 * val), avg(val + 70001), sum(4 * val) FROM t");
    let (_, _, result) = guarded(agg_tlist_expr(&plan, 0));
    let div = result.as_op_expr().expect("numeric_div");
    assert_eq!(div.args.nth(0).as_case_expr().expect("count(val) <= N").casetype, NUMERICOID);
    let s = below_n(div.args.nth(0)).as_func_expr().expect("S(e)::numeric");
    assert_eq!((s.funcid, s.funcresulttype), (INT8_NUMERIC_PROC, NUMERICOID));
}

/// D2862 0b: agg_algebra::wrap_add is PostgreSQL's wrapping int8 total a + b (int2_sum / int4_sum / int4_avg_accum),
/// spelled in int8 operators. Evaluated with PostgreSQL's own int8 operators (adt_int8: they raise past the int8 range)
/// over a and b at the int8 edges and b up to |k| < 2^16 times ROW_CAP (2^47) rows, it never raises and equals the exact
/// total modulo 2^64 (wrap64's formula; int8_form_cap_and_wrap ties that formula to int4_sum).
#[test]
fn wrap_add_is_the_int8_wrap_at_the_edges() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    fn eval(n: Node<'_>) -> types_error::PgResult<i64> {
        if let Some(c) = n.as_const() {
            assert!(!c.constisnull && c.consttype == INT8OID && c.constbyval);
            return Ok(c.constvalue.as_i64());
        }
        if let Some(case) = n.as_case_expr() {
            assert_eq!((case.casetype, case.args.len()), (INT8OID, 1));
            let when = case.args.nth(0).as_case_when().unwrap();
            let test = when.expr.unwrap().as_op_expr().expect("a > MAX - b / a < MIN - b");
            let (l, r) = (eval(test.args.nth(0))?, eval(test.args.nth(1))?);
            let hit = match test.opno {
                INT8GT_OP => l > r,
                INT8LT_OP => l < r,
                other => panic!("edge test {other}"),
            };
            return eval(if hit { when.result.unwrap() } else { case.defresult.unwrap() });
        }
        let op = n.as_op_expr().expect("an int8 operator");
        assert_eq!(op.opresulttype, INT8OID);
        let (l, r) = (eval(op.args.nth(0))?, eval(op.args.nth(1))?);
        match op.opno {
            INT8PL_OP => adt_int8::int8pl(l, r),
            INT8MI_OP => adt_int8::int8mi(l, r),
            INT8MUL_OP => adt_int8::int8mul(l, r),
            other => panic!("operator {other}"),
        }
    }
    let wrap = |v: i128| ((v + (1 << 63)) % (1 << 64) + (1 << 64)) % (1 << 64) - (1 << 63);
    let (max, min) = (i64::MAX, i64::MIN);
    let b_max = ((1i64 << 16) - 1) << 47;
    let mut cases = 0;
    for k in [1i64, -1, (1 << 16) - 1, -((1 << 16) - 1)] {
        let bs: Vec<i64> = [0, 1, 7, 1 << 47, b_max].iter().map(|b| if k < 0 { -b } else { *b }).collect();
        for &b in &bs {
            let edge = if k < 0 { min - b } else { max - b };
            let edges = [Some(min), min.checked_add(1), Some(-1), Some(0), Some(1), max.checked_sub(1), Some(max)];
            let near = [edge.checked_sub(1), Some(edge), edge.checked_add(1), Some(b), b.checked_neg()];
            for a in edges.into_iter().chain(near).flatten() {
                let v = crate::agg_algebra::wrap_add(mcx, k, &|| Ok(int_const(mcx, INT8OID, a)), &|| Ok(int_const(mcx, INT8OID, b))).unwrap();
                let got = eval(v).unwrap_or_else(|e| panic!("k {k} a {a} b {b}: an int8 operator raised: {e:?}"));
                assert_eq!(got as i128, wrap(a as i128 + b as i128), "k {k} a {a} b {b}");
                cases += 1;
            }
        }
    }
    assert!(cases >= 4 * 5 * 10, "{cases} cases");
}

// a HAVING could drop a group and hide the raise PostgreSQL makes in the scan: the guards run first in it
// (PSP2-03); a lone raising member stays as written (it does not pay). A statement whose aggregates are all min / max
// keeps planagg's form.
#[test]
fn affine_raising_members_decline_under_having_and_minmax_only() {
    let _forced = forced_affine();
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let mut parse = query(mcx, None, &[(sum_of(mcx, plus_one(mcx, var(mcx, 2)), false), "s", 0)], &[], None);
    let gt = int_op(mcx, INT4_GT_OP, 147, 16, max_of(mcx, var(mcx, 1)), int_const(mcx, INT4OID, 0));
    parse.havingQual = Some(gt);
    let plan = plan_of(mcx, parse, "SELECT sum(val + 1) FROM t HAVING max(pk) > 0");
    assert_eq!(agg_tlist_expr(&plan, 0).as_aggref().expect("as written").aggfnoid, SUM_INT4);
    let shifted = |k: i64| sum_of(mcx, int_op(mcx, INT4PL_OP, INT4PL_PROC, INT4OID, var(mcx, 2), int_const(mcx, INT4OID, k)), false);
    let mut parse = query(mcx, None, &[(shifted(1), "a", 0), (shifted(2), "b", 0), (shifted(3), "c", 0)], &[], None);
    parse.havingQual = Some(int_op(mcx, INT4_GT_OP, 147, 16, max_of(mcx, var(mcx, 1)), int_const(mcx, INT4OID, 0)));
    let plan = plan_of(mcx, parse, "SELECT sum(val + 1), sum(val + 2), sum(val + 3) FROM t HAVING max(pk) > 0");
    let (lo, _, _) = guarded(agg_tlist_expr(&plan, 0));
    assert_eq!(lo.as_op_expr().expect("guard").opno, INT4PL_OP);
    let qual = &plan.as_agg().unwrap().plan.qual;
    assert_eq!(qual.len(), 1);
    let case = qual.nth(0).as_case_expr().expect("CASE WHEN <guards> THEN <HAVING> ELSE false END");
    let when = case.args.nth(0).as_case_when().unwrap();
    assert_eq!(when.expr.unwrap().as_bool_expr().expect("the three guards").args.len(), 3);
    assert_eq!(when.result.unwrap().as_op_expr().expect("the HAVING qual").opno, INT4_GT_OP);
    let min_plus = sum_typed(mcx, MIN_INT4, INT4OID, INT4OID, plus_one(mcx, var(mcx, 2)));
    let parse = query(mcx, None, &[(min_plus, "m", 0)], &[], None);
    let plan = plan_of(mcx, parse, "SELECT min(val + 1) FROM t");
    assert_eq!(agg_tlist_expr(&plan, 0).as_aggref().expect("as written").aggfnoid, MIN_INT4);
}

// Under GROUP BY a raise-free family (smallint + int4 k cannot leave int4) is rewritten when it lowers the per-row
// evaluations (three shifted sums: six against sum(w) + count(w)); one shifted sum alone stays. A raising family is
// rewritten with its guard (order_safe holds under GROUP BY without HAVING).
// The same measure holds in a plain aggregation (R1-REVIEW F2: a lone member keeps PostgreSQL's one transition).
#[test]
fn affine_group_by_rewrites_raise_free_families_that_pay() {
    let _forced = forced_affine();
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let w = || Node::mk_var(mcx, 1, 2, INT2OID, -1, 0, 0).unwrap();
    let shift = |k| sum_typed(mcx, SUM_INT4, INT8OID, INT4OID, int_op(mcx, INT24PL_OP, INT24PL_PROC, INT4OID, w(), int_const(mcx, INT4OID, k)));
    // Grouped by `val` (no pg_statistic: PostgreSQL's 200 groups over 10 000 rows) the finals are paid once per 50 rows.
    let entries = |n: i64, key: i16| {
        let mut e = vec![(var(mcx, key), "key", 1)];
        for k in 1..=n {
            e.push((shift(k), "s", 0));
        }
        e
    };
    let parse = query(mcx, None, &entries(3, 2), &[1], None);
    let plan = plan_of(mcx, parse, "SELECT val, sum(w + 1), sum(w + 2), sum(w + 3) FROM t GROUP BY val");
    let top = if plan.node_tag() == NodeTag::T_Agg { plan } else { plan.as_plan().unwrap().lefttree.unwrap() };
    for i in 1..4 {
        let add = agg_tlist_expr(&top, i).as_op_expr().expect("no guard: sum(w) + k * count(w)");
        assert_eq!(add.opno, INT8PL_OP);
        assert_eq!(add.args.nth(0).as_aggref().unwrap().aggfnoid, SUM_INT2);
    }
    let parse = query(mcx, None, &entries(1, 2), &[1], None);
    let plan = plan_of(mcx, parse, "SELECT val, sum(w + 1) FROM t GROUP BY val");
    let top = if plan.node_tag() == NodeTag::T_Agg { plan } else { plan.as_plan().unwrap().lefttree.unwrap() };
    assert_eq!(agg_tlist_expr(&top, 1).as_aggref().expect("as written").aggfnoid, SUM_INT4);
    let parse = query(mcx, None, &entries(3, 1), &[1], None);
    let plan = plan_of(mcx, parse, "SELECT pk, sum(w + 1), sum(w + 2), sum(w + 3) FROM t GROUP BY pk");
    let top = if plan.node_tag() == NodeTag::T_Agg { plan } else { plan.as_plan().unwrap().lefttree.unwrap() };
    for i in 1..4 {
        assert_eq!(agg_tlist_expr(&top, i).as_aggref().expect("as written").aggfnoid, SUM_INT4);
    }
    let parse = query(mcx, None, &[(var(mcx, 2), "val", 1), (shift(2147483000), "a", 0), (shift(2147483001), "b", 0), (shift(2147483002), "c", 0)], &[1], None);
    let plan = plan_of(mcx, parse, "SELECT val, sum(w + 2147483000), sum(w + 2147483001), sum(w + 2147483002) FROM t GROUP BY val");
    let top = if plan.node_tag() == NodeTag::T_Agg { plan } else { plan.as_plan().unwrap().lefttree.unwrap() };
    let (lo, _, _) = guarded(agg_tlist_expr(&top, 1));
    assert_eq!(lo.as_op_expr().expect("guard").opno, INT24PL_OP);
    let raising = |k| sum_of(mcx, int_op(mcx, INT4PL_OP, INT4PL_PROC, INT4OID, var(mcx, 2), int_const(mcx, INT4OID, k)), false);
    let parse = query(mcx, None, &[(var(mcx, 2), "val", 1), (raising(1), "a", 0), (raising(2), "b", 0), (raising(3), "c", 0)], &[1], None);
    let plan = plan_of(mcx, parse, "SELECT val, sum(val + 1), sum(val + 2), sum(val + 3) FROM t GROUP BY val");
    let top = if plan.node_tag() == NodeTag::T_Agg { plan } else { plan.as_plan().unwrap().lefttree.unwrap() };
    let (lo, _, _) = guarded(agg_tlist_expr(&top, 1));
    assert_eq!(lo.as_op_expr().expect("guard").opno, INT4PL_OP);
}

// D2862 0c: the rewrite is settled after query_planner at the scan's row estimate. Three raise-free shifted sums over
// t's 10 000 rows pay; behind `pk = 5` (one row: pk's unique index) the nodes they add cost more per execution than the
// transitions they save, so the entries are as written again, numbered as if never rewritten, with no final marked.
#[test]
fn affine_settles_at_the_scan_rows() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let w = || Node::mk_var(mcx, 1, 2, INT2OID, -1, 0, 0).unwrap();
    let shift = |k| sum_typed(mcx, SUM_INT4, INT8OID, INT4OID, int_op(mcx, INT24PL_OP, INT24PL_PROC, INT4OID, w(), int_const(mcx, INT4OID, k)));
    let entries = || vec![(var(mcx, 2), "val", 1), (shift(1), "a", 0), (shift(2), "b", 0), (shift(3), "c", 0)];
    fn agg<'m>(plan: Node<'m>) -> Node<'m> {
        let mut p = plan;
        while p.node_tag() != NodeTag::T_Agg {
            p = p.as_plan().unwrap().lefttree.expect("an Agg under the plan");
        }
        p
    }
    let parse = query(mcx, None, &entries(), &[1], None);
    let top = agg(plan_of(mcx, parse, "SELECT val, sum(w + 1), sum(w + 2), sum(w + 3) FROM t GROUP BY val"));
    for i in 1..4 {
        let add = agg_tlist_expr(&top, i).as_op_expr().expect("rewritten at 10 000 rows: sum(w) + k * count(w)");
        assert_eq!(add.opno, INT8PL_OP);
    }
    let parse = query(mcx, Some(eq_qual(mcx, 1, 5)), &entries(), &[1], None);
    let top = agg(plan_of(mcx, parse, "SELECT val, sum(w + 1), sum(w + 2), sum(w + 3) FROM t WHERE pk = 5 GROUP BY val"));
    let mut aggnos = Vec::new();
    for i in 1..4 {
        let a = agg_tlist_expr(&top, i).as_aggref().expect("undone at one row: as written");
        assert_eq!(a.aggfnoid, SUM_INT4, "entry {i}");
        aggnos.push(a.aggno);
    }
    assert_eq!(aggnos, vec![0, 1, 2], "numbered as never rewritten");
    assert!(top.as_agg().unwrap().gravity_finals.is_empty(), "no final marked");
}

// D2862 0d: the split between deciding after query_planner and rewriting before it, read from the fire record. A
// grouped family that query_planner never reads waits for the scan's rows (`agg_algebra.affine.deferred`): behind
// `pk = 5` nothing is built (no `agg_algebra.affine`, no `.undone`: 0c built it and undid it), over t's 10 000 rows it
// is built there and the aggregates are numbered again. Every case query_planner reads, or that is ungrouped, is
// rewritten before it, as in 0c, and kept under `pk = 5` where settle cannot undo it: an entry ORDER BY names, an entry
// DISTINCT names, an ordered aggregate beside the family, a window or a set-returning target above the Agg, an
// ungrouped one-relation family (finals.cse), a min / max-only family (planagg's statement). An ungrouped family over
// one relation that finals.cse does not serve is built before it and undone.
#[test]
fn affine_decides_after_query_planner_only_where_it_reads_no_aggregate() {
    use types_nodes::parsenodes::WindowClause;
    use types_nodes::primnodes::{CoercionForm, FuncExpr, WindowFunc};
    const INT8EQ_OP: u32 = 410;
    const ROW_NUMBER: u32 = 3100;
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let w = || Node::mk_var(mcx, 1, 2, INT2OID, -1, 0, 0).unwrap();
    let w_plus = |k| int_op(mcx, INT24PL_OP, INT24PL_PROC, INT4OID, w(), int_const(mcx, INT4OID, k));
    let shift = |k| sum_typed(mcx, SUM_INT4, INT8OID, INT4OID, w_plus(k));
    let one_row = || Some(eq_qual(mcx, 1, 5));
    let planned = |parse, sql: &str| {
        fires::set(true);
        let _ = fires::drain();
        let plan = plan_of(mcx, parse, sql);
        let f = fires::drain();
        fires::set(false);
        (plan, f.split(',').map(str::to_owned).collect::<Vec<_>>())
    };
    fn agg<'m>(plan: Node<'m>) -> Node<'m> {
        let mut p = plan;
        while p.node_tag() != NodeTag::T_Agg {
            p = p.as_plan().unwrap().lefttree.expect("an Agg under the plan");
        }
        p
    }
    let has = |f: &[String], name: &str| f.iter().any(|n| n == name);
    let grouped = |r: u32| vec![(var(mcx, 2), "val", 1), (shift(1), "a", r), (shift(2), "b", 0), (shift(3), "c", 0)];

    // After query_planner: behind pk = 5 never built.
    let (plan, f) = planned(query(mcx, one_row(), &grouped(0), &[1], None), "SELECT val, sum(w + 1), sum(w + 2), sum(w + 3) FROM t WHERE pk = 5 GROUP BY val");
    assert!(has(&f, "agg_algebra.affine.deferred"), "{f:?}");
    assert!(!has(&f, "agg_algebra.affine") && !has(&f, "agg_algebra.affine.undone"), "never built: {f:?}");
    let top = agg(plan);
    let aggnos: Vec<i32> = (1..4).map(|i| agg_tlist_expr(&top, i).as_aggref().expect("as written").aggno).collect();
    assert_eq!(aggnos, vec![0, 1, 2], "numbered as written");
    let (plan, f) = planned(query(mcx, None, &grouped(0), &[1], None), "SELECT val, sum(w + 1), sum(w + 2), sum(w + 3) FROM t GROUP BY val");
    assert!(has(&f, "agg_algebra.affine.deferred") && has(&f, "agg_algebra.affine"), "{f:?}");
    let top = agg(plan);
    let mut nos = std::collections::BTreeMap::new();
    for i in 1..4 {
        let add = agg_tlist_expr(&top, i).as_op_expr().expect("rewritten after query_planner");
        assert_eq!(add.opno, INT8PL_OP);
        let sum = add.args.nth(0).as_aggref().expect("sum(w)");
        let mul = add.args.nth(1).as_op_expr().expect("k * count(w)");
        let count = (0..2).find_map(|j| mul.args.nth(j).as_aggref()).expect("count(w)");
        nos.entry(sum.aggfnoid).or_insert_with(Vec::new).push(sum.aggno);
        nos.entry(count.aggfnoid).or_insert_with(Vec::new).push(count.aggno);
    }
    let mut seen: Vec<i32> = nos.values().map(|v| {
        assert!(v.iter().all(|n| *n == v[0]), "one agginfo per base aggregate: {nos:?}");
        v[0]
    }).collect();
    seen.sort_unstable();
    assert_eq!(seen, vec![0, 1], "numbered again: {nos:?}");

    let before = |parse, sql: &str, what: &str| {
        let (plan, f) = planned(parse, sql);
        assert!(!has(&f, "agg_algebra.affine.deferred"), "{what}: {f:?}");
        assert!(has(&f, "agg_algebra.affine") && !has(&f, "agg_algebra.affine.undone"), "{what}: kept: {f:?}");
        plan
    };
    before(
        query(mcx, one_row(), &grouped(2), &[1], Some((2, INT8EQ_OP, INT8LT_OP, false))),
        "SELECT val, sum(w + 1) AS a, sum(w + 2), sum(w + 3) FROM t WHERE pk = 5 GROUP BY val ORDER BY a",
        "ORDER BY entry",
    );
    let mut parse = query(
        mcx,
        one_row(),
        &[(var(mcx, 2), "val", 1), (shift(1), "a", 2), (shift(2), "b", 3), (shift(3), "c", 4)],
        &[1],
        None,
    );
    let mut dl = NodeList::make1(mcx, sgc(mcx, 1, INT4EQ_OP, INT4_LT_OP, false)).unwrap();
    for r in 2..5 {
        dl.lappend(mcx, sgc(mcx, r, INT8EQ_OP, INT8LT_OP, false)).unwrap();
    }
    parse.distinctClause = dl;
    before(parse, "SELECT DISTINCT val, sum(w + 1), sum(w + 2), sum(w + 3) FROM t WHERE pk = 5 GROUP BY val", "DISTINCT entry");
    let mut e = grouped(0);
    e.push((sum_ordered(mcx, var(mcx, 1)), "o", 0));
    before(
        query(mcx, one_row(), &e, &[1], None),
        "SELECT val, sum(w + 1), sum(w + 2), sum(w + 3), sum(pk ORDER BY pk) FROM t WHERE pk = 5 GROUP BY val",
        "ordered aggregate",
    );
    let mut e = grouped(0);
    let wf = Node::mk(mcx, WindowFunc { winfnoid: ROW_NUMBER, wintype: INT8OID, winref: 1, ..WindowFunc::default() }).unwrap();
    e.push((wf, "rn", 0));
    let mut parse = query(mcx, one_row(), &e, &[1], None);
    let wc = Node::mk(
        mcx,
        WindowClause { frameOptions: types_nodes::rawnodes::FRAMEOPTION_DEFAULTS, winref: 1, ..WindowClause::default() },
    )
    .unwrap();
    parse.windowClause = NodeList::make1(mcx, wc).unwrap();
    parse.hasWindowFuncs = true;
    before(parse, "SELECT val, sum(w + 1), sum(w + 2), sum(w + 3), row_number() OVER () FROM t WHERE pk = 5 GROUP BY val", "window");
    let mut e = grouped(0);
    let gs = Node::mk(
        mcx,
        FuncExpr {
            funcid: 1067,
            funcresulttype: INT4OID,
            funcretset: true,
            funcvariadic: false,
            funcformat: CoercionForm::COERCE_EXPLICIT_CALL,
            funccollid: 0,
            inputcollid: 0,
            args: NodeList::make2(mcx, int_const(mcx, INT4OID, 1), int_const(mcx, INT4OID, 2)).unwrap(),
            location: -1,
        },
    )
    .unwrap();
    e.push((gs, "g", 0));
    let mut parse = query(mcx, one_row(), &e, &[1], None);
    parse.hasTargetSRFs = true;
    before(parse, "SELECT val, sum(w + 1), sum(w + 2), sum(w + 3), generate_series(1, 2) FROM t WHERE pk = 5 GROUP BY val", "SRF");
    let (_, f) = planned(
        query(mcx, one_row(), &[(shift(1), "a", 0), (shift(2), "b", 0), (shift(3), "c", 0)], &[], None),
        "SELECT sum(w + 1), sum(w + 2), sum(w + 3) FROM t WHERE pk = 5",
    );
    assert!(!has(&f, "agg_algebra.affine.deferred") && has(&f, "agg_algebra.cse") && has(&f, "agg_algebra.affine"), "finals.cse: {f:?}");
    let mm = |agg: u32, k| sum_typed(mcx, agg, INT4OID, INT4OID, w_plus(k));
    before(
        query(mcx, one_row(), &[(mm(MAX_INT4, 1), "a", 0), (mm(MAX_INT4, 2), "b", 0), (mm(MIN_INT4, 3), "c", 0)], &[], None),
        "SELECT max(w + 1), max(w + 2), min(w + 3) FROM t WHERE pk = 5",
        "min / max only",
    );
    let avg = |k| sum_typed(mcx, AVG_INT4, NUMERICOID, INT4OID, w_plus(k));
    let (_, f) = planned(
        query(mcx, one_row(), &[(avg(1), "a", 0), (avg(2), "b", 0), (avg(3), "c", 0)], &[], None),
        "SELECT avg(w + 1), avg(w + 2), avg(w + 3) FROM t WHERE pk = 5",
    );
    assert!(!has(&f, "agg_algebra.affine.deferred") && has(&f, "agg_algebra.affine.undone"), "ungrouped avg: {f:?}");
}

#[test]
fn affine_member_over_a_raise_free_expression() {
    let _forced = forced_affine();
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let coalesce = |a| {
        let args = NodeList::make2(mcx, a, int_const(mcx, INT4OID, 0)).unwrap();
        Node::mk(mcx, types_nodes::primnodes::CoalesceExpr { coalescetype: INT4OID, coalescecollid: 0, args, location: -1 }).unwrap()
    };
    let over = |a, k| int_op(mcx, INT4PL_OP, INT4PL_PROC, INT4OID, coalesce(a), int_const(mcx, INT4OID, k));
    assert!(crate::agg_algebra::Chain::of(over(var(mcx, 2), 1)).unwrap().is_some(), "coalesce(val, 0) + 1");
    let outer = Node::mk_var(mcx, 1, 2, INT4OID, -1, 0, 1).unwrap();
    assert!(crate::agg_algebra::Chain::of(over(outer, 1)).unwrap().is_none(), "an outer column");
    let exec = types_nodes::primnodes::Param { paramkind: types_nodes::primnodes::ParamKind::PARAM_EXEC, paramid: 1, paramtype: INT4OID, paramtypmod: -1, paramcollid: 0, location: -1 };
    let both = |p| {
        let args = NodeList::make2(mcx, var(mcx, 2), Node::mk(mcx, p).unwrap()).unwrap();
        let c = Node::mk(mcx, types_nodes::primnodes::CoalesceExpr { coalescetype: INT4OID, coalescecollid: 0, args, location: -1 }).unwrap();
        int_op(mcx, INT4PL_OP, INT4PL_PROC, INT4OID, c, int_const(mcx, INT4OID, 1))
    };
    assert!(crate::agg_algebra::Chain::of(both(exec)).unwrap().is_none(), "coalesce(val, <initplan output>)");

    let member = |k| sum_of(mcx, over(var(mcx, 2), k), false);
    let parse = query(mcx, None, &[(var(mcx, 2), "val", 1), (member(1), "a", 0), (member(2), "b", 0), (member(3), "c", 0)], &[1], None);
    let plan = plan_of(mcx, parse, "SELECT val, sum(coalesce(val, 0) + 1), sum(coalesce(val, 0) + 2), sum(coalesce(val, 0) + 3) FROM t GROUP BY val");
    let top = if plan.node_tag() == NodeTag::T_Agg { plan } else { plan.as_plan().unwrap().lefttree.unwrap() };
    for i in 1..4 {
        let (lo, _, _) = guarded(agg_tlist_expr(&top, i));
        let min = lo.as_op_expr().expect("guard").args.nth(0).as_aggref().expect("min(coalesce(val, 0))");
        assert_eq!(min.aggfnoid, MIN_INT4, "entry {i}");
        assert_eq!(min.args.nth(0).as_target_entry().unwrap().expr.node_tag(), NodeTag::T_CoalesceExpr, "entry {i}");
    }
}

// R1-REVIEW F2 / S1 / S4: a lone member keeps PostgreSQL's one transition; a `c` that is an initplan's output
// (PARAM_EXEC) is not a chain constant (the initplan runs only where a row reaches it); a member with a FILTER, a
// volatile one here, stays as written (each base aggregate would draw its own FILTER).
#[test]
fn affine_lone_members_initplan_params_and_volatile_filters_stay() {
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let w = || Node::mk_var(mcx, 1, 2, INT2OID, -1, 0, 0).unwrap();
    let w_plus = |k| int_op(mcx, INT24PL_OP, INT24PL_PROC, INT4OID, w(), int_const(mcx, INT4OID, k));
    let parse = query(mcx, None, &[(sum_typed(mcx, SUM_INT4, INT8OID, INT4OID, w_plus(1)), "s", 0)], &[], None);
    let plan = plan_of(mcx, parse, "SELECT sum(w + 1) FROM t");
    assert_eq!(agg_tlist_expr(&plan, 0).as_aggref().expect("as written").aggfnoid, SUM_INT4);
    let avg = sum_typed(mcx, AVG_INT4, NUMERICOID, INT4OID, plus_one(mcx, var(mcx, 2)));
    let parse = query(mcx, None, &[(avg, "a", 0)], &[], None);
    let plan = plan_of(mcx, parse, "SELECT avg(val + 1) FROM t");
    assert_eq!(agg_tlist_expr(&plan, 0).as_aggref().expect("as written").aggfnoid, AVG_INT4);

    let param = |kind| {
        let p = types_nodes::primnodes::Param { paramkind: kind, paramid: 1, paramtype: INT4OID, paramtypmod: -1, paramcollid: 0, location: -1 };
        Node::mk(mcx, p).unwrap()
    };
    let over = |kind| int_op(mcx, INT4PL_OP, INT4PL_PROC, INT4OID, var(mcx, 2), param(kind));
    assert!(crate::agg_algebra::Chain::of(over(types_nodes::primnodes::ParamKind::PARAM_EXEC)).unwrap().is_none(), "initplan output");
    assert!(crate::agg_algebra::Chain::of(over(types_nodes::primnodes::ParamKind::PARAM_EXTERN)).unwrap().is_some(), "$1");

    let random = Node::mk(
        mcx,
        types_nodes::primnodes::FuncExpr {
            funcid: 1598,
            funcresulttype: 701,
            funcretset: false,
            funcvariadic: false,
            funcformat: types_nodes::primnodes::CoercionForm::COERCE_EXPLICIT_CALL,
            funccollid: 0,
            inputcollid: 0,
            args: NodeList::nil(),
            location: -1,
        },
    )
    .unwrap();
    let half = Node::mk_const(mcx, 701, -1, 0, 8, Datum::from_f64(0.5), false, true).unwrap();
    let draw = int_op(mcx, 672, 295, 16, random, half);
    let drawn = |k| {
        let a = sum_typed(mcx, AVG_INT4, NUMERICOID, INT4OID, w_plus(k));
        // SAFETY: freshly built node.
        unsafe { a.with_mut::<Aggref, _>(|a| a.aggfilter = Some(draw)) }.unwrap();
        a
    };
    let parse = query(mcx, None, &[(drawn(1), "a", 0), (drawn(2), "b", 0), (drawn(3), "c", 0)], &[], None);
    let plan = plan_of(mcx, parse, "SELECT avg(w + 1) FILTER (WHERE random() < 0.5), avg(w + 2) FILTER (..), avg(w + 3) FILTER (..) FROM t");
    for i in 0..3 {
        assert_eq!(agg_tlist_expr(&plan, i).as_aggref().expect("as written").aggfnoid, AVG_INT4, "entry {i}");
    }
}

// R1-REVIEW S2 / S3: no statistics input. A numeric-result sum computes every level in numeric over an exact base
// (smallint + int4 k, widened to int8: sum(w::int8) + k * count(w)::numeric), and an int4 x's int8 form holds up to
// N = (2^63 - 1) / M rows (M = 2^31 here: N = 2^32 - 1), past them PostgreSQL's wrapped accumulator.
#[test]
fn affine_numeric_levels_and_the_row_bound_are_statistics_free() {
    let _forced = forced_affine();
    let _guc = GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let cx = setup();
    let mcx = cx.mcx();
    let w = || Node::mk_var(mcx, 1, 2, INT2OID, -1, 0, 0).unwrap();
    let widened = |k| {
        let level = int_op(mcx, INT24PL_OP, INT24PL_PROC, INT4OID, w(), int_const(mcx, INT4OID, k));
        let cast = Node::mk(
            mcx,
            types_nodes::primnodes::FuncExpr {
                funcid: 481,
                funcresulttype: INT8OID,
                funcretset: false,
                funcvariadic: false,
                funcformat: types_nodes::primnodes::CoercionForm::COERCE_EXPLICIT_CAST,
                funccollid: 0,
                inputcollid: 0,
                args: NodeList::make1(mcx, level).unwrap(),
                location: -1,
            },
        )
        .unwrap();
        sum_typed(mcx, SUM_INT8, NUMERICOID, INT8OID, cast)
    };
    let parse = query(mcx, None, &[(widened(2000000000), "a", 0), (widened(2000000001), "b", 0)], &[], None);
    let plan = plan_of(mcx, parse, "SELECT sum((w + 2000000000)::int8), sum((w + 2000000001)::int8) FROM t");
    let add = agg_tlist_expr(&plan, 0).as_op_expr().expect("numeric +, no guard (raise-free)");
    assert_eq!(add.opno, NUMERIC_ADD_OP);
    let sum8 = add.args.nth(0).as_aggref().expect("sum(w::int8)");
    assert_eq!((sum8.aggfnoid, sum8.aggtype), (SUM_INT8, NUMERICOID));
    assert_eq!(sum8.args.nth(0).as_target_entry().unwrap().expr.as_func_expr().expect("w::int8").funcid, 754);
    let mul = add.args.nth(1).as_op_expr().expect("k * count(w), numeric");
    assert_eq!(mul.opno, NUMERIC_MUL_OP, "the int4 level in numeric");
    assert_eq!(mul.args.nth(1).as_func_expr().expect("count(w)::numeric").funcid, INT8_NUMERIC_PROC);
    // profit is PostgreSQL's per-row cost (get_agg_clause_costs: argument + transition). A lone widened member
    // costs int24pl + int48 + its transition; sum(w::int8) + count(w) costs int28 + two transitions: no cheaper, so it
    // stays as written (an evaluation count missed the cast the base sum needs).
    let parse = query(mcx, None, &[(widened(2000000000), "a", 0)], &[], None);
    let plan = plan_of(mcx, parse, "SELECT sum((w + 2000000000)::int8) FROM t");
    assert_eq!(agg_tlist_expr(&plan, 0).as_aggref().expect("as written").aggfnoid, SUM_INT8);

    let shift = |k| sum_of(mcx, int_op(mcx, INT4PL_OP, INT4PL_PROC, INT4OID, var(mcx, 2), int_const(mcx, INT4OID, k)), false);
    let parse = query(mcx, None, &[(shift(2000000000), "a", 0), (shift(2000000001), "b", 0), (shift(2000000002), "c", 0)], &[], None);
    let plan = plan_of(mcx, parse, "SELECT sum(val + 2000000000), sum(val + 2000000001), sum(val + 2000000002) FROM t");
    let (_, _, result) = guarded(agg_tlist_expr(&plan, 0));
    let case = result.as_case_expr().expect("count(val) <= N");
    let le = case.args.nth(0).as_case_when().unwrap().expr.unwrap().as_op_expr().unwrap();
    assert_eq!((le.opno, le.args.nth(1).as_const().unwrap().constvalue.as_i64()), (INT8LE_OP, (1i64 << 32) - 1));
    below_n(result);
    // Every literal of the member, its ELSE (PostgreSQL's wrapped accumulator) included, is by-value: the lowerer refuses
    // a by-reference literal past its parameter-word budget (a 90-member `sum(i + k)` over an int4 column).
    fn consts<'m>(n: Node<'m>, out: &mut Vec<Node<'m>>) {
        let each = |l: &NodeList<'m>, out: &mut Vec<Node<'m>>| l.iter().for_each(|a| consts(a, out));
        match n.node_tag() {
            NodeTag::T_Const => out.push(n),
            NodeTag::T_OpExpr => each(&n.as_op_expr().unwrap().args, out),
            NodeTag::T_FuncExpr => each(&n.as_func_expr().unwrap().args, out),
            NodeTag::T_BoolExpr => each(&n.as_bool_expr().unwrap().args, out),
            NodeTag::T_NullTest => consts(n.as_null_test().unwrap().arg.unwrap(), out),
            NodeTag::T_CaseExpr => {
                let c = n.as_case_expr().unwrap();
                for w in &c.args {
                    let w = w.as_case_when().unwrap();
                    consts(w.expr.unwrap(), out);
                    consts(w.result.unwrap(), out);
                }
                consts(c.defresult.unwrap(), out);
            }
            _ => {}
        }
    }
    let mut lits = Vec::new();
    consts(agg_tlist_expr(&plan, 0), &mut lits);
    assert!(lits.len() >= 8, "guard, bound, k and wrap64's literals: {}", lits.len());
    assert!(lits.iter().all(|c| c.as_const().unwrap().constbyval), "a by-reference literal in the rewritten member");
}
