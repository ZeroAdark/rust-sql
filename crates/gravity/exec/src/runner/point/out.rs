//! The wire's out columns of a point statement's target list (the lowering's out row, lowerv4 end/output.rs `col_of`).
use gravity_program::OutCol;
use types_error::PgResult;
use types_nodes::primnodes::TargetEntry;

/// A cstring column: the wire's out row has no form for one (as the lowering's `col_of` refuses it).
pub(super) fn cstring(te: &TargetEntry<'_>) -> bool {
    gravity_expr::nodes::func::expr_type(te.expr) == types_core::CSTRINGOID
}

/// ExecTypeFromTL's column, as the lowering's out row (lowerv4 end/output.rs `col_of`): exprType / exprTypmod /
/// exprCollation, the name, the origin.
pub(super) fn out_col(te: &TargetEntry<'_>) -> PgResult<OutCol> {
    let typ = gravity_expr::nodes::func::expr_type(te.expr);
    let (typlen, byval) = gravity_fn::typesem::storage::len_byval(typ)?;
    let c = OutCol::new(te.resname.unwrap_or("?column?"), typ, gravity_expr::nodes::func::expr_typmod(te.expr), typlen, byval);
    Ok(c.collation(gravity_expr::nodes::func::expr_collation(te.expr)).origin(te.resorigtbl, te.resorigcol))
}
