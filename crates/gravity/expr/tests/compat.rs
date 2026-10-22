//! `compat` (expr-compiler.md §4.8): a call through the re-export and a direct `execexpr` call
//! agree on a corpus expression, so a caller changes only its `use` line.

use datum::Datum;
use gravity_expr::compat;
use mcx::MemoryContext;
use types_nodes::list::NodeList;
use types_nodes::node_tree::Node;
use types_nodes::primnodes::OpExpr;

const INT4OID: u32 = 23;

fn expr(mcx: mcx::Mcx<'_>) -> Node<'_> {
    let a = Node::mk_const(mcx, INT4OID, -1, 0, 4, Datum::from_i32(40), false, true).unwrap();
    let b = Node::mk_const(mcx, INT4OID, -1, 0, 4, Datum::from_i32(2), false, true).unwrap();
    Node::mk(mcx, OpExpr { opno: 551, opfuncid: 177, opresulttype: INT4OID, opretset: false, opcollid: 0, inputcollid: 0, args: NodeList::from_slice(mcx, &[a, b]).unwrap(), location: -1 }).unwrap()
}

fn install_seams() {
    syscache_seams::lookup_pg_type_shape::set(|typid| Ok((typid == INT4OID).then_some(types_tuple::PgTypeShape { typlen: 4, typbyval: true, typalign: b'i' as i8, typstorage: b'p' as i8, typcollation: 0 })));
    syscache_seams::pg_type_typnamespace::set(|_| Ok(Some(11)));
    namespace_seams::is_temp_namespace::set(|_| false);
    miscinit_seams::get_user_id::set(|| 10);
    aclchk_seams::object_aclcheck::set(|_, _, _, _| Ok(0));
    syscache_seams::pg_proc_proname::set(|_| Ok(None));
}

#[test]
fn compat_and_execexpr_agree() {
    install_seams();
    let ctx = MemoryContext::new("compat");
    let mcx = ctx.mcx();
    let via_compat = {
        let mut state = compat::exec_init_expr(mcx, Some(expr(mcx)), compat::ParamBind::NONE).unwrap().unwrap();
        state.arm_result_mcx(mcx);
        compat::exec_eval_expr(&mut state, &mut compat::EvalSlots::default()).unwrap()
    };
    let direct = {
        let mut state = execexpr::exec_init_expr(mcx, Some(expr(mcx)), execexpr::ParamBind::NONE).unwrap().unwrap();
        state.arm_result_mcx(mcx);
        execexpr::exec_eval_expr(&mut state, &mut execexpr::EvalSlots::default()).unwrap()
    };
    assert_eq!((via_compat.isnull, via_compat.value.as_i32()), (direct.isnull, direct.value.as_i32()));
    assert_eq!(via_compat.value.as_i32(), 42);
    let folded = compat::evaluate_expr(mcx, expr(mcx), INT4OID, -1, 0).unwrap();
    assert_eq!(folded.as_const().unwrap().constvalue.as_i32(), 42);
    let _: fn(Datum, bool, u32, Option<&mut types_error::SoftErrorContext>) -> types_error::PgResult<()> = compat::domain::domain_check_input;
}
