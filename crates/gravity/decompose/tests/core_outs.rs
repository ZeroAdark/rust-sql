// Every expression node's output (Rep + life) across the plan corpus: a bare Var over a lane keeps the lane's Rep, a
// by-value result is a word, an fmgr by-ref result lives until the next call of its builtin (deny-by-default).
use std::fs;

use gravity_decompose::{corpus, decompose_in, Op};
use gravity_program::{Arena, ExprLife, LaneLife, Rep};
use mcx::MemoryContext;
use types_nodes::NodeTag;

mod harness;

#[test]
fn projection_entries_carry_their_output() {
    let mut files = harness::fixtures_in(&harness::plans_root());
    files.extend(harness::fixtures_in(&harness::plans_root().join("regress-full")));
    let (mut entries, mut with, mut codes, mut calls) = (0usize, 0usize, 0usize, 0usize);
    for f in files {
        let text = fs::read_to_string(&f).unwrap();
        let ctx = MemoryContext::new("fixture");
        let Ok(fx) = corpus::load(ctx.mcx(), &text) else { continue };
        let arena = Arena::new(1 << 20);
        let Ok(g) = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx) else { continue };
        let name = &fx.name;
        for p in g.pipelines.iter() {
            for op in p.ops.iter() {
                let Op::Project(l) = op else { continue };
                for e in l.iter() {
                    entries += 1;
                    let Some(o) = g.out_of(e) else { continue };
                    with += 1;
                    let x = e.as_target_entry().map_or(e, |t| t.expr);
                    if let Some(t) = gravity_decompose::catalog::typefacts::row_of(o.typid) {
                        if t.byval {
                            assert!(matches!(o.rep, Rep::Word { .. }), "{name}: a by-value output is a word");
                            assert_eq!(o.life, ExprLife::Lane(LaneLife::VALUE), "{name}: a by-value output needs no bytes");
                        }
                    }
                    if matches!(o.rep, Rep::Code { .. }) {
                        codes += 1;
                        assert!(matches!(x.node_tag(), NodeTag::T_Var | NodeTag::T_RelabelType), "{name}: only a bare Var or Relabel over a code lane is a code (D207)");
                        assert_eq!(o.life, ExprLife::Lane(LaneLife::DICTIONARY));
                    }
                    if let (Some(f), false) = (x.as_func_expr(), matches!(o.rep, Rep::Word { .. })) {
                        calls += 1;
                        assert_eq!(o.life, ExprLife::Call { proc: f.funcid }, "{name}: an unlisted builtin's by-ref result is its scratch (deny-by-default)");
                    }
                }
            }
        }
    }
    println!("outs: {with} of {entries} projection entries, {codes} code outputs, {calls} by-ref calls");
    assert!(with * 100 >= entries * 90, "most projection entries carry an output ({with} of {entries})");
    assert!(codes > 0 && calls > 0);
}

#[test]
fn a_case_lives_as_long_as_its_shortest_branch() {
    let call = ExprLife::Call { proc: 67 };
    assert_eq!(ExprLife::Lane(LaneLife::PAGE).min(call), call);
    assert_eq!(call.min(ExprLife::Row), call);
    assert_eq!(ExprLife::Row.min(ExprLife::Lane(LaneLife::VALUE)), ExprLife::Row);
    assert_eq!(call.min(ExprLife::Call { proc: 68 }), ExprLife::Call { proc: 0 }, "two scratches: any builtin's next call");
    assert_eq!(ExprLife::Lane(LaneLife::PAGE).min(ExprLife::Lane(LaneLife::DICTIONARY)), ExprLife::Lane(LaneLife::PAGE));
}

/// D261: `collation_of` is PG 18.6's `exprCollation` for the forms a served CREATE TABLE AS column reads (a domain
/// literal, a role SQLValueFunction, an array coercion, a field, a subscript): each carries its node's collation.
mod d261_collation_of {
    use gravity_decompose::catalog::expr_type::collation_of;
    use mcx::{Mcx, MemoryContext};
    use types_nodes::primnodes::{ArrayCoerceExpr, CaseTestExpr, CoerceToDomain, CoerceToDomainValue, CoercionForm, FieldSelect, SQLValueFunction, SQLValueFunctionOp, SubscriptingRef};
    use types_nodes::{Node, OptNodeList};

    fn text(m: Mcx<'_>) -> Node<'_> {
        Node::mk_const(m, 25, -1, 100, -1, datum::Datum::null(), true, false).unwrap()
    }

    #[test]
    fn coerce_to_domain() {
        let ctx = MemoryContext::new("d261");
        let m = ctx.mcx();
        let d = Node::mk(m, CoerceToDomain { arg: text(m), resulttype: 13_000, resulttypmod: -1, resultcollid: 100, coercionformat: CoercionForm::COERCE_EXPLICIT_CAST, location: -1 }).unwrap();
        assert_eq!(collation_of(d), 100);
    }

    #[test]
    fn coerce_to_domain_value() {
        let ctx = MemoryContext::new("d261");
        let v = Node::mk(ctx.mcx(), CoerceToDomainValue { typeId: 25, typeMod: -1, collation: 950, location: -1 }).unwrap();
        assert_eq!(collation_of(v), 950);
    }

    #[test]
    fn case_test_expr() {
        let ctx = MemoryContext::new("d261");
        let c = Node::mk(ctx.mcx(), CaseTestExpr { typeId: 25, typeMod: -1, collation: 100 }).unwrap();
        assert_eq!(collation_of(c), 100);
    }

    #[test]
    fn sql_value_function_is_c_for_name_only() {
        let ctx = MemoryContext::new("d261");
        let m = ctx.mcx();
        let user = Node::mk(m, SQLValueFunction { op: SQLValueFunctionOp::SVFOP_CURRENT_USER, r#type: 19, typmod: -1, location: -1 }).unwrap();
        let date = Node::mk(m, SQLValueFunction { op: SQLValueFunctionOp::SVFOP_CURRENT_DATE, r#type: 1082, typmod: -1, location: -1 }).unwrap();
        assert_eq!((collation_of(user), collation_of(date)), (950, 0));
    }

    #[test]
    fn array_coerce_expr() {
        let ctx = MemoryContext::new("d261");
        let m = ctx.mcx();
        let a = Node::mk(m, ArrayCoerceExpr { arg: text(m), elemexpr: None, resulttype: 1009, resulttypmod: -1, resultcollid: 100, coerceformat: CoercionForm::COERCE_EXPLICIT_CAST, location: -1 }).unwrap();
        assert_eq!(collation_of(a), 100);
    }

    #[test]
    fn field_select() {
        let ctx = MemoryContext::new("d261");
        let m = ctx.mcx();
        let f = Node::mk(m, FieldSelect { arg: text(m), fieldnum: 2, resulttype: 25, resulttypmod: -1, resultcollid: 100 }).unwrap();
        assert_eq!(collation_of(f), 100);
    }

    #[test]
    fn subscripting_ref() {
        let ctx = MemoryContext::new("d261");
        let m = ctx.mcx();
        let upper = OptNodeList::from_slice(m, &[Some(text(m))]).unwrap();
        let s = Node::mk(m, SubscriptingRef { refcontainertype: 1009, refelemtype: 25, refrestype: 25, reftypmod: -1, refcollid: 100, refupperindexpr: upper, reflowerindexpr: OptNodeList::from_slice(m, &[]).unwrap(), refexpr: Some(text(m)), refassgnexpr: None }).unwrap();
        assert_eq!(collation_of(s), 100);
    }
}
