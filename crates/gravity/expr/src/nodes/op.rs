//! `DistinctExpr`, `NullIfExpr` and `ScalarArrayOpExpr`: the operator-shaped nodes with their own
//! null rules over one `Call` site. The array's internals (element walk) are `array.rs`'s face.

use gravity_fn::{Datum, NullableDatum};
use types_core::Oid;
use types_error::PgResult;
use types_nodes::primnodes::{DistinctExpr, NullIfExpr, ScalarArrayOpExpr};
use types_nodes::Node;

use crate::eval::{eval, Cx};
use crate::nodes::func::{bind, eval_args, invoke, set_fn_expr, Call, MAX_ARGS};
use crate::nodes::{array, unsupported, Builder, ExprNode, Kind, NodeId, Prog, SiteId, St};
use crate::CompileResult;

#[derive(Clone, Copy, Debug)]
pub struct ScalarArrayOp {
    pub site: SiteId,
    pub use_or: bool,
    pub elem: array::ElemFacts,
}

pub fn compile_distinct<'mcx>(b: &mut Builder<'_, 'mcx>, d: &DistinctExpr<'mcx>, node: Node<'mcx>) -> CompileResult<NodeId> {
    let funcid = if d.opfuncid != 0 { d.opfuncid } else { b.catalog.opcode(d.opno)? };
    let args = b.kids(&d.args)?;
    let call = bind(b, funcid, args, &d.args, d.inputcollid)?;
    set_fn_expr(b, &call, node);
    let site = b.site(call);
    Ok(b.push(ExprNode::Distinct(crate::nodes::func::FuncNode { site })))
}

pub fn compile_nullif<'mcx>(b: &mut Builder<'_, 'mcx>, d: &NullIfExpr<'mcx>, node: Node<'mcx>) -> CompileResult<NodeId> {
    let funcid = if d.opfuncid != 0 { d.opfuncid } else { b.catalog.opcode(d.opno)? };
    let args = b.kids(&d.args)?;
    let call = bind(b, funcid, args, &d.args, d.inputcollid)?;
    set_fn_expr(b, &call, node);
    let site = b.site(call);
    Ok(b.push(ExprNode::NullIf(crate::nodes::func::FuncNode { site })))
}

pub fn compile_scalar_array_op<'mcx>(b: &mut Builder<'_, 'mcx>, s: &ScalarArrayOpExpr<'mcx>, node: Node<'mcx>) -> CompileResult<NodeId> {
    if s.args.len() != 2 {
        return unsupported(Kind::ScalarArrayOpExpr);
    }
    let funcid = if s.opfuncid != 0 { s.opfuncid } else { b.catalog.opcode(s.opno)? };
    if s.hashfuncid != 0 {
        crate::nodes::func::execute_gate(s.hashfuncid)?;
    }
    // D2642: PostgreSQL hashed it (ExecEvalHashedScalarArrayOp): its table finds what the operator finds element by
    // element only when the hash procs agree with the operator (typesem's `linear`). =! over int4ne / hashint4 finds
    // nothing in the table where the walk finds every other element: such a node is not served
    if s.hashfuncid != 0 && s.useOr && matches!(b.catalog.hash_key(s.opno, s.inputcollid), Ok(Ok(k)) if !k.linear) {
        return unsupported(Kind::ScalarArrayOpExpr);
    }
    let array_type: Oid = nodes_core::expr_type(s.args.nth(1));
    let elem = array::elem_facts(b.catalog, array_type)?;
    let args = b.kids(&s.args)?;
    // the operator's second argument is one element (C get_call_expr_argtype: a ScalarArrayOpExpr's argument 1 is its
    // array's element type), so a shim views it by the element's typlen, never as the array's varlena (D123 / D419)
    let types = [nodes_core::expr_type(s.args.nth(0)), elem.typ];
    let call = crate::nodes::func::bind_typed(b, funcid, args, &types, s.inputcollid)?;
    set_fn_expr(b, &call, node);
    let site = b.site(call);
    Ok(b.push(ExprNode::ScalarArrayOp(ScalarArrayOp { site, use_or: s.useOr, elem })))
}

/// `a IS DISTINCT FROM b`: both null → false, one null → true, else `NOT (a = b)`.
pub fn eval_distinct(p: &Prog<'_, '_>, st: &mut St<'_>, call: &Call, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    let mut args = [Datum::null(); MAX_ARGS];
    let mut nulls = [false; MAX_ARGS];
    eval_args(p, st, call, cx, &mut args, &mut nulls)?;
    if nulls[0] && nulls[1] {
        return Ok(NullableDatum::value(Datum::from_bool(false)));
    }
    if nulls[0] || nulls[1] {
        return Ok(NullableDatum::value(Datum::from_bool(true)));
    }
    let r = invoke(p, st, call, cx, &args, &nulls, false)?;
    Ok(NullableDatum::value(Datum::from_bool(!r.value.as_bool())))
}

/// `NULLIF(a, b)`: null when `a = b` is true; `a` otherwise (a strict operator over a null
/// argument is not called).
pub fn eval_nullif(p: &Prog<'_, '_>, st: &mut St<'_>, call: &Call, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    let mut args = [Datum::null(); MAX_ARGS];
    let mut nulls = [false; MAX_ARGS];
    let any_null = eval_args(p, st, call, cx, &mut args, &mut nulls)?;
    let first = NullableDatum { value: args[0], isnull: nulls[0] };
    if any_null && call.strict {
        return Ok(first);
    }
    let r = invoke(p, st, call, cx, &args, &nulls, any_null)?;
    if !r.isnull && r.value.as_bool() {
        return Ok(NullableDatum::null());
    }
    Ok(first)
}

/// `scalar op ANY/ALL (array)` (C `ExecEvalScalarArrayOp`).
pub fn eval_scalar_array_op(p: &Prog<'_, '_>, st: &mut St<'_>, n: &ScalarArrayOp, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    let call = &p.sites[n.site.0 as usize];
    let scalar = eval(p, st, p.kids[call.args.start as usize], cx)?;
    let arr = eval(p, st, p.kids[call.args.start as usize + 1], cx)?;
    if arr.isnull {
        return Ok(NullableDatum::null());
    }
    // D1812: an empty array answers false (ANY) / true (ALL) before the scalar's NULL is read (ExecEvalScalarArrayOp)
    let mut walk = array::Walk::over(arr.value, n.elem, cx.region)?;
    if walk.is_empty() {
        return Ok(NullableDatum::value(Datum::from_bool(!n.use_or)));
    }
    if call.strict && scalar.isnull {
        return Ok(NullableDatum::null());
    }
    let mut args = [Datum::null(); MAX_ARGS];
    let mut nulls = [false; MAX_ARGS];
    args[0] = scalar.value;
    nulls[0] = scalar.isnull;
    let mut result = NullableDatum::value(Datum::from_bool(!n.use_or));
    while let Some(e) = walk.next() {
        args[1] = e.value;
        nulls[1] = e.isnull;
        let r = invoke(p, st, call, cx, &args, &nulls, scalar.isnull | e.isnull)?;
        if r.isnull {
            result = NullableDatum::null();
        } else if n.use_or {
            if r.value.as_bool() {
                return Ok(NullableDatum::value(Datum::from_bool(true)));
            }
        } else if !r.value.as_bool() {
            return Ok(NullableDatum::value(Datum::from_bool(false)));
        }
    }
    Ok(result)
}
