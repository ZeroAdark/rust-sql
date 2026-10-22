//! `BoolExpr`: three-valued AND / OR with short circuit in argument order, NOT.

use gravity_fn::{Datum, NullableDatum};
use types_error::PgResult;
use types_nodes::primnodes::{BoolExpr, BoolExprType};

use crate::eval::{eval, Cx};
use crate::nodes::{Builder, ExprNode, Kids, NodeId, Prog, St};
use crate::CompileResult;

#[derive(Clone, Copy, Debug)]
pub struct BoolNode {
    pub op: BoolExprType,
    pub args: Kids,
}

pub fn compile<'mcx>(b: &mut Builder<'_, 'mcx>, e: &BoolExpr<'mcx>) -> CompileResult<NodeId> {
    let args = b.kids(&e.args)?;
    Ok(b.push(ExprNode::Bool(BoolNode { op: e.boolop, args })))
}

pub fn eval_bool(p: &Prog<'_, '_>, st: &mut St<'_>, n: &BoolNode, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    let kids = &p.kids[n.args.start as usize..n.args.start as usize + n.args.len as usize];
    match n.op {
        BoolExprType::NOT_EXPR => {
            let v = eval(p, st, kids[0], cx)?;
            Ok(if v.isnull { NullableDatum::null() } else { NullableDatum::value(Datum::from_bool(!v.value.as_bool())) })
        }
        BoolExprType::AND_EXPR => {
            let mut any_null = false;
            for &k in kids {
                let v = eval(p, st, k, cx)?;
                if v.isnull {
                    any_null = true;
                } else if !v.value.as_bool() {
                    return Ok(NullableDatum::value(Datum::from_bool(false)));
                }
            }
            Ok(if any_null { NullableDatum::null() } else { NullableDatum::value(Datum::from_bool(true)) })
        }
        BoolExprType::OR_EXPR => {
            let mut any_null = false;
            for &k in kids {
                let v = eval(p, st, k, cx)?;
                if v.isnull {
                    any_null = true;
                } else if v.value.as_bool() {
                    return Ok(NullableDatum::value(Datum::from_bool(true)));
                }
            }
            Ok(if any_null { NullableDatum::null() } else { NullableDatum::value(Datum::from_bool(false)) })
        }
    }
}

