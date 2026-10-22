//! `SubPlan` (the tail lane's file; lower.md §8): a per-row subplan's value is the owning
//! pipeline's cell — `Row::aggs[cell]` / `RowRefs::aggs[cell]`, numbered after the program's
//! aggregate cells as a `WindowFunc` is; the pipeline fills it before the expression reads it.
//! A set-returning `FuncExpr` (P6, lower.md §12) is the same cell face: the SRF loop the
//! pipeline drives writes the function's current value before the projection reads it.

use gravity_fn::NullableDatum;
use types_core::Oid;
use types_error::{PgError, PgResult};
use types_nodes::primnodes::{FuncExpr, SubLinkType, SubPlan as SubPlanTree};

use crate::eval::Cx;
use crate::nodes::{Builder, ExprNode, NodeId, Prog, St};
use crate::CompileResult;

#[derive(Clone, Copy, Debug)]
pub struct SubPlan {
    pub plan_id: i32,
    pub cell: i32,
    pub typ: Oid,
}

pub fn compile_subplan<'mcx>(b: &mut Builder<'_, 'mcx>, s: &SubPlanTree<'mcx>) -> CompileResult<NodeId> {
    let typ = match s.subLinkType {
        SubLinkType::EXPR_SUBLINK => s.firstColType,
        // the ARRAY value's type as PG's exprType names it (the element's array type; an array element keeps its own type)
        SubLinkType::ARRAY_SUBLINK => nodes_core::node_funcs::promoted_array_type(s.firstColType),
        // PG's exprType: a MULTIEXPR is RECORD; its value is the dummy NULL (the step writes its setParams)
        SubLinkType::MULTIEXPR_SUBLINK => types_core::catalog::RECORDOID,
        _ => types_core::catalog::BOOLOID,
    };
    let cell = b.prog.naggs as i32;
    b.prog.naggs += 1;
    Ok(b.push(ExprNode::SubPlan(SubPlan { plan_id: s.plan_id, cell, typ })))
}

/// D3374: the word of a NULL SubPlan cell whose SubPlan made a second row it holds (emit/mod.rs `emit_subplan_held`): the
/// island raises PG's cardinality error where it reads the cell, never where it skips it.
pub const HELD_ROWS: u64 = 0x4845_4c44_5f32_3130;

pub fn eval_subplan(_p: &Prog<'_, '_>, _st: &mut St<'_>, n: &SubPlan, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    match cx.row.aggs.get(n.cell as usize) {
        Some(v) if v.isnull && v.value.as_u64() == HELD_ROWS => Err(more_rows()),
        Some(v) => Ok(*v),
        None => Err(no_cell(n.plan_id)),
    }
}

/// ExecScanSubPlan's error at an EXPR sublink's second row.
#[cold]
#[inline(never)]
fn more_rows() -> Box<PgError> {
    Box::new(PgError::error("more than one row returned by a subquery used as an expression".to_owned()).with_sqlstate(types_error::ERRCODE_CARDINALITY_VIOLATION)) // alloc: Cold
}

#[cold]
#[inline(never)]
fn no_cell(plan_id: i32) -> Box<PgError> {
    let _ = plan_id;
    Box::new(PgError::error("SubPlan found with no parent plan".to_owned()).with_sqlstate(types_error::ERRCODE_INTERNAL_ERROR)) // alloc: Cold
}

#[derive(Clone, Copy, Debug)]
pub struct Srf {
    pub funcid: Oid,
    pub cell: i32,
    pub typ: Oid,
}

pub fn compile_srf<'mcx>(b: &mut Builder<'_, 'mcx>, f: &FuncExpr<'mcx>) -> CompileResult<NodeId> {
    Ok(srf_cell(b, f.funcid, f.funcresulttype))
}

/// A set-returning operator (`OpExpr.opretset`, D780: `|@| ARRAY[..]` over unnest) is the same cell face: ProjectSet's
/// `ExecInitFunctionResultSet` takes a FuncExpr or an OpExpr alike, and the decomposer maps both (`srf_of`).
pub fn srf_cell(b: &mut Builder<'_, '_>, funcid: Oid, typ: Oid) -> NodeId {
    let cell = b.prog.naggs as i32;
    b.prog.naggs += 1;
    b.push(ExprNode::Srf(Srf { funcid, cell, typ }))
}

pub fn eval_srf(_p: &Prog<'_, '_>, _st: &mut St<'_>, n: &Srf, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    match cx.row.aggs.get(n.cell as usize) {
        Some(v) => Ok(*v),
        None => Err(no_srf_cell(n.funcid)),
    }
}

#[cold]
#[inline(never)]
fn no_srf_cell(funcid: Oid) -> Box<PgError> {
    Box::new(PgError::error(format!("set-valued function {funcid} called in context that cannot accept a set")).with_sqlstate(types_error::ERRCODE_FEATURE_NOT_SUPPORTED)) // alloc: Cold
}
