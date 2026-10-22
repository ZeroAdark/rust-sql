//! `CollateExpr` (the tail lane's file, expr-compiler.md §4.3): the node is its argument at
//! evaluation (the collation is in the consuming call's `inputcollid`); `Path` is the
//! comparison path per collation: byte order, a sort key, or the function manager.

use gravity_fn::NullableDatum;
use types_core::Oid;
use types_error::PgResult;
use types_nodes::primnodes::CollateExpr;

use crate::catalog::Catalog;
use crate::eval::Cx;
use crate::nodes::{Builder, ExprNode, NodeId, Prog, St};
use crate::CompileResult;

#[derive(Clone, Copy, Debug)]
pub struct Collate {
    pub arg: NodeId,
    pub collation: Oid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Path {
    Bytes,
    SortKey,
    Fmgr,
}

pub fn path_of<'mcx>(cat: &dyn Catalog<'mcx>, collation: Oid) -> PgResult<Path> {
    let t = cat.text(collation)?;
    Ok(if !t.deterministic {
        Path::Fmgr
    } else if t.order_is_bytes {
        Path::Bytes
    } else {
        Path::SortKey
    })
}

pub fn compile<'mcx>(b: &mut Builder<'_, 'mcx>, c: &CollateExpr<'mcx>) -> CompileResult<NodeId> {
    let arg = b.compile(c.arg)?;
    Ok(b.push(ExprNode::Collate(Collate { arg, collation: c.collOid })))
}

pub fn eval(p: &Prog<'_, '_>, st: &mut St<'_>, n: &Collate, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    crate::eval::eval(p, st, n.arg, cx)
}
