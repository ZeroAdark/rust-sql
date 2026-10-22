//! Which clauses a granule's [min, max] zone can decide: one mapping shared by plan-time costing and run-time pruning,
//! so the two read a clause the same way.

use datum::Datum;
use types_core::Oid;
use types_error::PgResult;
use types_nodes::{Node, NodeTag};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZoneOp {
    Eq,
    Lt,
    Le,
    Gt,
    Ge,
}

impl ZoneOp {
    pub fn commuted(self) -> ZoneOp {
        match self {
            ZoneOp::Eq => ZoneOp::Eq,
            ZoneOp::Lt => ZoneOp::Gt,
            ZoneOp::Le => ZoneOp::Ge,
            ZoneOp::Gt => ZoneOp::Lt,
            ZoneOp::Ge => ZoneOp::Le,
        }
    }
}

pub fn key_units(typid: Oid) -> Option<u8> {
    use types_core::catalog::*;
    Some(match typid {
        INT2OID | INT4OID | INT8OID | OIDOID => 1,
        DATEOID => 2,
        TIMESTAMPOID | TIMESTAMPTZOID => 3,
        BOOLOID => 4,
        TIMEOID => 5,
        CHAROID => 6,
        _ => return None,
    })
}

// A constant's zone key: the datum word as the footer embeds it
pub fn const_key(c: &types_nodes::primnodes::Const, vartype: Oid) -> Option<i64> {
    datum_key(c.consttype, c.constisnull, c.constvalue, vartype)
}

pub fn datum_key(consttype: Oid, isnull: bool, d: Datum, vartype: Oid) -> Option<i64> {
    use types_core::catalog::*;
    if isnull || key_units(consttype)? != key_units(vartype)? {
        return None;
    }
    Some(match consttype {
        INT2OID => i64::from(d.as_i16()),
        INT4OID | DATEOID => i64::from(d.as_i32()),
        INT8OID | TIMESTAMPOID | TIMESTAMPTZOID | TIMEOID => d.as_i64(),
        BOOLOID => i64::from(d.as_bool()),
        OIDOID => i64::from(d.as_u32()),
        CHAROID => i64::from(d.as_i8()),
        _ => return None,
    })
}

pub fn zone_op(opno: Oid, ty: Oid) -> PgResult<Option<ZoneOp>> {
    if lsyscache::op_mergejoinable(opno, ty)? {
        return Ok(Some(ZoneOp::Eq));
    }
    if let Some((_, _, cmp)) = lsyscache::get_ordering_op_properties(opno)? {
        return Ok(Some(if cmp == lsyscache::COMPARE_LT { ZoneOp::Lt } else { ZoneOp::Gt }));
    }
    let neg = lsyscache::get_negator(opno)?;
    if neg != 0 {
        if let Some((_, _, cmp)) = lsyscache::get_ordering_op_properties(neg)? {
            return Ok(Some(if cmp == lsyscache::COMPARE_LT { ZoneOp::Ge } else { ZoneOp::Le }));
        }
    }
    Ok(None)
}

#[derive(Clone, Copy)]
pub struct ColumnOperands<'m> {
    pub opno: Oid,
    pub attno: i16,
    pub vartype: Oid,
    pub other: Node<'m>,
    pub commuted: bool,
}

pub fn column_operands<'m>(clause: Node<'m>, varno: i32) -> Option<ColumnOperands<'m>> {
    let o = clause.as_op_expr()?;
    if o.args.len() != 2 {
        return None;
    }
    fn strip<'m>(mut e: Node<'m>) -> Node<'m> {
        while let Some(r) = e.as_relabel_type() {
            e = r.arg;
        }
        e
    }
    let mut args = o.args.iter();
    let (a, b) = (strip(args.next().unwrap()), strip(args.next().unwrap()));
    let (var, other, commuted) = match (a.as_var(), b.as_var()) {
        (Some(v), None) => (v, b, false),
        (None, Some(v)) => (v, a, true),
        _ => return None,
    };
    if var.varno != varno || var.varlevelsup != 0 || var.varattno <= 0 {
        return None;
    }
    if !matches!(other.node_tag(), NodeTag::T_Const | NodeTag::T_Param) {
        return None;
    }
    Some(ColumnOperands { opno: o.opno, attno: var.varattno, vartype: var.vartype, other, commuted })
}

// (attno, the zone comparison, the constant's key); the key is None for a
// Param or a constant of other units, the op None for a non-zone operator.
pub fn column_op(clause: Node<'_>, varno: i32) -> PgResult<Option<(i16, Option<ZoneOp>, Option<i64>)>> {
    let Some(c) = column_operands(clause, varno) else { return Ok(None) };
    let key = c.other.as_const().and_then(|k| const_key(k, c.vartype));
    let op = zone_op(c.opno, c.vartype)?.map(|z| if c.commuted { z.commuted() } else { z });
    Ok(Some((c.attno, op, key)))
}
