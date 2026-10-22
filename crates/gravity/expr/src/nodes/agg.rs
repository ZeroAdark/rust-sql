//! `Aggref`, `WindowFunc`, `GroupingFunc` (the tail lane's file, expr-compiler.md §4.6, §9):
//! their values are the owning node's cells, not the row's. `Aggref` binds its `AggPlan` through
//! the aggregate library (an unsupported aggregate is refused by name) and reads
//! `Row::aggs[aggno]`, the Agg's finalized per-group value; a `WindowFunc` reads `Row::aggs[cell]`,
//! numbered in compile order (no `Aggref` shares its program: §9.5); grouping sets are refused.

use gravity_fn::NullableDatum;
use types_core::Oid;
use types_error::{PgError, PgResult};
use types_nodes::primnodes::{Aggref as AggrefTree, GroupingFunc as GroupingFuncTree, WindowFunc as WindowFuncTree};

use crate::eval::Cx;
use crate::nodes::{unsupported, Builder, ExprNode, Kind, NodeId, Prog, St};
use crate::CompileResult;

#[derive(Clone, Copy, Debug)]
pub struct Aggref {
    pub aggno: i32,
    pub typ: Oid,
}

#[derive(Clone, Copy, Debug)]
pub struct WindowFunc {
    pub cell: i32,
    pub winref: u32,
    pub typ: Oid,
}

#[derive(Clone, Copy, Debug)]
pub struct GroupingFunc {
    pub nargs: u16,
}

pub fn compile_aggref<'mcx>(b: &mut Builder<'_, 'mcx>, a: &AggrefTree<'mcx>) -> CompileResult<NodeId> {
    match b.folded {
        true => crate::agg::Agg::folded_gate(a, b.aggorder)?,
        false => drop(crate::agg::Agg::compile_with(a, b.catalog, b.aggorder)?),
    }
    if a.aggno < 0 {
        return Err(no_cell(a.aggno));
    }
    b.prog.naggs = b.prog.naggs.max(a.aggno as u32 + 1);
    Ok(b.push(ExprNode::Aggref(Aggref { aggno: a.aggno, typ: a.aggtype })))
}

pub fn compile_window_func<'mcx>(b: &mut Builder<'_, 'mcx>, w: &WindowFuncTree<'mcx>) -> CompileResult<NodeId> {
    let cell = b.prog.naggs as i32;
    b.prog.naggs += 1;
    Ok(b.push(ExprNode::WindowFunc(WindowFunc { cell, winref: w.winref, typ: w.wintype })))
}

pub fn compile_grouping_func<'mcx>(_b: &mut Builder<'_, 'mcx>, _g: &GroupingFuncTree<'mcx>) -> CompileResult<NodeId> {
    unsupported(Kind::GroupingFunc)
}

pub fn eval_aggref(_p: &Prog<'_, '_>, _st: &mut St<'_>, n: &Aggref, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    match cx.row.aggs.get(n.aggno as usize) {
        Some(v) => Ok(*v),
        None => Err(no_cell(n.aggno).into()),
    }
}

pub fn eval_window_func(_p: &Prog<'_, '_>, _st: &mut St<'_>, n: &WindowFunc, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    match cx.row.aggs.get(n.cell as usize) {
        Some(v) => Ok(*v),
        None => Err(no_cell(n.cell).into()),
    }
}

pub fn eval_grouping_func(_p: &Prog<'_, '_>, _st: &mut St<'_>, _n: &GroupingFunc, _cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    unreachable!("GroupingFunc is refused at compile")
}

#[cold]
#[inline(never)]
fn no_cell(aggno: i32) -> crate::CompileError {
    crate::CompileError::Pg(Box::new(PgError::error(format!("gravity_expr: aggregate cell {aggno} is not bound to the row")).with_sqlstate(types_error::ERRCODE_INTERNAL_ERROR))) // alloc: Cold
}
