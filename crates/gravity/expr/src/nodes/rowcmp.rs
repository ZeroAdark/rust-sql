//! `RowCompareExpr`: the btree comparison support function per column (C `ExecEvalRowCompare`):
//! the first non-zero column decides; a null column makes the result null.

use gravity_fn::{Datum, NullableDatum};
use types_error::PgResult;
use types_nodes::primnodes::RowCompareExpr;

use crate::eval::{eval, Cx};
use crate::nodes::func::{bind_typed, invoke, MAX_ARGS};
use crate::nodes::{unsupported, Builder, ExprNode, Kids, Kind, NodeId, Prog, St};
use crate::CompileResult;

pub const COMPARE_LT: i32 = 1;
pub const COMPARE_LE: i32 = 2;
pub const COMPARE_GE: i32 = 4;
pub const COMPARE_GT: i32 = 5;

#[derive(Clone, Copy, Debug)]
pub struct RowCompare {
    pub cmptype: i32,
    pub largs: Kids,
    pub rargs: Kids,
    /// The first of `largs.len` comparison sites, one per column.
    pub sites: u32,
}

pub fn compile<'mcx>(b: &mut Builder<'_, 'mcx>, r: &RowCompareExpr<'mcx>) -> CompileResult<NodeId> {
    let n = r.largs.len();
    let planner_cmptype = matches!(r.cmptype, COMPARE_LT | COMPARE_LE | COMPARE_GE | COMPARE_GT);
    if !planner_cmptype || n == 0 || r.rargs.len() != n || r.opnos.len() != n || r.opfamilies.len() != n {
        return unsupported(Kind::RowCompareExpr);
    }
    let largs = b.kids(&r.largs)?;
    let rargs = b.kids(&r.rargs)?;
    let sites = b.prog.sites.len() as u32;
    for i in 0..n {
        let opno = r.opnos.nth(i);
        let (lt, rt) = b.catalog.op_input_types(opno)?;
        let proc_oid = b.catalog.btree_cmp_proc(r.opfamilies.nth(i), lt, rt)?;
        if proc_oid == 0 {
            return unsupported(Kind::RowCompareExpr);
        }
        let coll = if r.inputcollids.len() > i { r.inputcollids.nth(i) } else { 0 };
        let call = bind_typed(b, proc_oid, Kids::NONE, &[lt, rt], coll)?;
        b.site(call);
    }
    Ok(b.push(ExprNode::RowCompare(RowCompare { cmptype: r.cmptype, largs, rargs, sites })))
}

pub fn eval_row_compare(p: &Prog<'_, '_>, st: &mut St<'_>, n: &RowCompare, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    let mut args = [Datum::null(); MAX_ARGS];
    let nulls = [false; MAX_ARGS];
    let mut cmp = 0i32;
    for i in 0..n.largs.len as usize {
        let l = eval(p, st, p.kids[n.largs.start as usize + i], cx)?;
        let r = eval(p, st, p.kids[n.rargs.start as usize + i], cx)?;
        if l.isnull || r.isnull {
            return Ok(NullableDatum::null());
        }
        args[0] = l.value;
        args[1] = r.value;
        cmp = invoke(p, st, &p.sites[n.sites as usize + i], cx, &args, &nulls, false)?.value.as_i32();
        if cmp != 0 {
            break;
        }
    }
    let r = match n.cmptype {
        COMPARE_LT => cmp < 0,
        COMPARE_LE => cmp <= 0,
        COMPARE_GE => cmp >= 0,
        _ => cmp > 0,
    };
    Ok(NullableDatum::value(Datum::from_bool(r)))
}

