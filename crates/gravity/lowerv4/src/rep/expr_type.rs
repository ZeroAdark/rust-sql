//! An expression's type in the lowerer: PostgreSQL's `exprType` through the decomposer's catalog home, the compiled
//! expression's where that names none.
use types_core::Oid;
use types_nodes::Node;

use crate::graph::Gx;

/// PostgreSQL's `exprType` of an expression (nodes_core's port through decompose's `type_of`, which also answers
/// RETURNING's ReturningExpr); only a node `type_of` does not name (an ARRAY sublink of a type with no builtin array
/// type) takes the compiled expression's type.
pub(crate) fn result_type<'p>(gx: &Gx<'_, 'p>, node: Node<'p>, id: i32) -> Oid {
    crate::stack_check();
    gravity_decompose::catalog::expr_type::type_of(node).unwrap_or_else(|| crate::frame::raw::compiled_type(gx, node, id))
}
