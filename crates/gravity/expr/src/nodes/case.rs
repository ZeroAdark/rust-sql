//! `CaseExpr` (both forms, the test value in `Cx::casetest`), `CoalesceExpr`, `MinMaxExpr` (the
//! type's btree comparison function as a two-argument call site).

use gravity_fn::{Datum, NullableDatum};
use types_error::PgResult;
use types_nodes::primnodes::{CaseExpr, CoalesceExpr, MinMaxExpr, MinMaxOp};

use crate::eval::{eval, Cx};
use crate::nodes::func::{bind_typed, invoke, MAX_ARGS};
use crate::nodes::{unsupported, Builder, ExprNode, Kids, Kind, NodeId, Prog, SiteId, St};
use crate::CompileResult;

#[derive(Clone, Copy, Debug)]
pub struct Case {
    pub arg: Option<NodeId>,
    /// `2n` children: condition, result, condition, result, ...
    pub whens: Kids,
    pub default: Option<NodeId>,
}

#[derive(Clone, Copy, Debug)]
pub struct Coalesce {
    pub args: Kids,
}

#[derive(Clone, Copy, Debug)]
pub struct MinMax {
    pub args: Kids,
    pub least: bool,
    pub cmp: SiteId,
}

pub fn compile_case<'mcx>(b: &mut Builder<'_, 'mcx>, c: &CaseExpr<'mcx>) -> CompileResult<NodeId> {
    let arg = match c.arg {
        Some(a) => Some(b.compile(a)?),
        None => None,
    };
    let mut ids = Vec::with_capacity(c.args.len() * 2); // alloc: Program
    for w in c.args.iter() {
        let w = w.as_case_when().ok_or(crate::CompileError::Unsupported(Kind::CaseExpr))?;
        let (Some(e), Some(r)) = (w.expr, w.result) else { return unsupported(Kind::CaseExpr) };
        ids.push(b.compile(e)?);
        ids.push(b.compile(r)?);
    }
    let whens = b.kids_of(&ids);
    let default = match c.defresult {
        Some(d) => Some(b.compile(d)?),
        None => None,
    };
    Ok(b.push(ExprNode::Case(Case { arg, whens, default })))
}

pub fn compile_coalesce<'mcx>(b: &mut Builder<'_, 'mcx>, c: &CoalesceExpr<'mcx>) -> CompileResult<NodeId> {
    let args = b.kids(&c.args)?;
    Ok(b.push(ExprNode::Coalesce(Coalesce { args })))
}

pub fn compile_minmax<'mcx>(b: &mut Builder<'_, 'mcx>, m: &MinMaxExpr<'mcx>) -> CompileResult<NodeId> {
    let cmp_fn = b.catalog.type_cmp_proc(m.minmaxtype)?;
    if cmp_fn == 0 {
        return unsupported(Kind::MinMaxExpr);
    }
    let args = b.kids(&m.args)?;
    let call = bind_typed(b, cmp_fn, Kids::NONE, &[m.minmaxtype, m.minmaxtype], m.inputcollid)?;
    let cmp = b.site(call);
    Ok(b.push(ExprNode::MinMax(MinMax { args, least: m.op == MinMaxOp::IS_LEAST, cmp })))
}

pub fn eval_case(p: &Prog<'_, '_>, st: &mut St<'_>, n: &Case, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    let saved = cx.casetest;
    if let Some(a) = n.arg {
        cx.casetest = eval(p, st, a, cx)?;
    }
    let r = eval_whens(p, st, n, cx);
    cx.casetest = saved;
    r
}

fn eval_whens(p: &Prog<'_, '_>, st: &mut St<'_>, n: &Case, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    let kids = &p.kids[n.whens.start as usize..n.whens.start as usize + n.whens.len as usize];
    for pair in kids.chunks_exact(2) {
        let c = eval(p, st, pair[0], cx)?;
        if !c.isnull && c.value.as_bool() {
            return eval(p, st, pair[1], cx);
        }
    }
    match n.default {
        Some(d) => eval(p, st, d, cx),
        None => Ok(NullableDatum::null()),
    }
}

pub fn eval_coalesce(p: &Prog<'_, '_>, st: &mut St<'_>, n: &Coalesce, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    let kids = &p.kids[n.args.start as usize..n.args.start as usize + n.args.len as usize];
    for &k in kids {
        let v = eval(p, st, k, cx)?;
        if !v.isnull {
            return Ok(v);
        }
    }
    Ok(NullableDatum::null())
}

pub fn eval_minmax(p: &Prog<'_, '_>, st: &mut St<'_>, n: &MinMax, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    let kids = &p.kids[n.args.start as usize..n.args.start as usize + n.args.len as usize];
    let call = &p.sites[n.cmp.0 as usize];
    let mut best = NullableDatum::null();
    let mut args = [Datum::null(); MAX_ARGS];
    let nulls = [false; MAX_ARGS];
    for &k in kids {
        let v = eval(p, st, k, cx)?;
        if v.isnull {
            continue;
        }
        if best.isnull {
            best = v;
            continue;
        }
        args[0] = v.value;
        args[1] = best.value;
        let c = invoke(p, st, call, cx, &args, &nulls, false)?.value.as_i32();
        if (n.least && c < 0) || (!n.least && c > 0) {
            best = v;
        }
    }
    Ok(best)
}
