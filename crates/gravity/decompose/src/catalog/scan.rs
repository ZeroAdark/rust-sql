//! The scan sources' catalog names: the tid comparison operators a TID scan or a TID range scan accepts (tidpath.c
//! `IsTidEqualClause` / `IsTidRangeClause`), and the by-value types a pgrc lane lays down as a fixed-width value.

/// pg_operator.dat's `TIDEqualOperator` (`tid = tid`).
pub const TID_EQ: u32 = 387;
/// pg_operator.dat's `TIDLessOperator`, `TIDGreaterOperator`, `TIDLessEqOperator` and `TIDGreaterEqOperator`.
pub const TID_LT: u32 = 2799;
pub const TID_GT: u32 = 2800;
pub const TID_LE: u32 = 2801;
pub const TID_GE: u32 = 2802;

/// A tid comparison operator as the TID scans read it (nodeTidscan.c `TidExprListCreate`, nodeTidrangescan.c
/// `MakeTidOpExpr`), before any commute for a ctid on the right.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TidOp {
    /// `=`: a TID scan's point (`TIDEqualOperator`).
    Eq,
    /// `<` / `<=`: a TID range scan's upper bound when ctid is on the left (`TIDLessOperator`, `TIDLessEqOperator`).
    Less { inclusive: bool },
    /// `>` / `>=`: a TID range scan's lower bound when ctid is on the left (`TIDGreaterOperator`, `TIDGreaterEqOperator`).
    Greater { inclusive: bool },
}

/// Operator `opno` as a tid comparison, `None` for any other operator.
pub const fn tid_op(opno: u32) -> Option<TidOp> {
    match opno {
        TID_EQ => Some(TidOp::Eq),
        TID_LT => Some(TidOp::Less { inclusive: false }),
        TID_LE => Some(TidOp::Less { inclusive: true }),
        TID_GT => Some(TidOp::Greater { inclusive: false }),
        TID_GE => Some(TidOp::Greater { inclusive: true }),
        _ => None,
    }
}

/// Whether a by-value column of type `typid` is bool, float4 or float8 (pg_type.dat `BOOLOID` 16, `FLOAT4OID` 700,
/// `FLOAT8OID` 701): a pgrc lane holds those as a fixed-width value, not an integer.
pub const fn fixed_value_type(typid: u32) -> bool {
    matches!(typid, types_core::BOOLOID | types_core::FLOAT4OID | types_core::FLOAT8OID)
}

#[cfg(test)]
mod tests {
    use super::{fixed_value_type, tid_op, TidOp};

    /// Each tid comparison reads as its operator; no other operator does.
    #[test]
    fn tid_ops_by_oid() {
        assert_eq!(tid_op(387), Some(TidOp::Eq));
        assert_eq!(tid_op(2799), Some(TidOp::Less { inclusive: false }));
        assert_eq!(tid_op(2801), Some(TidOp::Less { inclusive: true }));
        assert_eq!(tid_op(2800), Some(TidOp::Greater { inclusive: false }));
        assert_eq!(tid_op(2802), Some(TidOp::Greater { inclusive: true }));
        assert_eq!(tid_op(96), None);
        assert_eq!(tid_op(0), None);
    }

    /// bool, float4 and float8 are the fixed-width value types; the integers and date are not.
    #[test]
    fn fixed_value_types() {
        assert!(fixed_value_type(16) && fixed_value_type(700) && fixed_value_type(701));
        assert!(!fixed_value_type(20) && !fixed_value_type(21) && !fixed_value_type(23) && !fixed_value_type(1082));
    }
}
