//! `Var`: a column of one of the row's sources; a whole-row reference (attno 0) forms its tuple
//! through the tail lane's tuple face (`tuple.rs`); a system column (attno < 0) reads the slot
//! the row carries. OLD/NEW returning vars read the sources `Builder.returning` names (a RETURNING list the lowerer lays
//! out, M4-RET R4a), else are refused with the kind named.

use gravity_fn::NullableDatum;
use types_core::Oid;
use types_error::{PgError, PgResult};
use types_nodes::primnodes::{VarReturningType, INNER_VAR, OUTER_VAR};

use crate::eval::{Cols, Cx, Source};
use crate::nodes::{unsupported, Builder, ExprNode, Kind, NodeId};
use crate::CompileResult;

#[derive(Clone, Copy, Debug)]
pub struct Var {
    pub src: Source,
    pub attno: i16,
    pub typ: Oid,
    /// The whole-row var's descriptor in `State::descs`.
    pub desc: u32,
}

pub fn compile(b: &mut Builder<'_, '_>, v: &types_nodes::primnodes::Var<'_>) -> CompileResult<NodeId> {
    if v.varreturningtype != VarReturningType::VAR_RETURNING_DEFAULT {
        let Some(r) = b.returning.filter(|_| v.varlevelsup == 0) else { return unsupported(Kind::Var) };
        let old = v.varreturningtype == VarReturningType::VAR_RETURNING_OLD;
        let ((src, off), exists) = if old { (r.old, r.old_exists) } else { (r.new, r.new_exists) };
        if v.varattno <= 0 && exists == Some(false) {
            // D774: the whole row or a system column of an image the command never has is NULL (ExecEvalWholeRowVar /
            // ExecEvalSysVar under EEO_FLAG_OLD_IS_NULL / NEW_IS_NULL), not a row of NULLs
            let arg = b.push(ExprNode::Imm(crate::nodes::misc::Imm { value: 0, typ: v.vartype }));
            return Ok(b.push(ExprNode::Returning(crate::nodes::misc::Returning { arg, old })));
        }
        if v.varattno == 0 {
            // D774: a whole-row OLD / NEW is its image's row; D2729: an image laid at an offset (MERGE's NEW after its OLD)
            // is the ROW of its columns there (`image_row`)
            // an image that exists per row (ON CONFLICT DO UPDATE's OLD, MERGE's): NULL where its flag says it does not,
            // as ExecEvalWholeRowVar under EEO_FLAG_OLD_IS_NULL / NEW_IS_NULL, never a row of NULLs
            let Some((fsrc, fattno)) = exists.is_none().then(|| if old { r.old_flag } else { r.new_flag }).flatten() else {
                return image_row(b, src, off, v.vartype);
            };
            let cond = b.push(ExprNode::Var(Var { src: fsrc, attno: fattno, typ: crate::percode::BOOLOID, desc: 0 }));
            let then = image_row(b, src, off, v.vartype)?;
            let whens = b.kids_of(&[cond, then]);
            return Ok(b.push(ExprNode::Case(crate::nodes::case::Case { arg: None, whens, default: None })));
        }
        if v.varattno > 0 {
            return Ok(b.push(ExprNode::Var(Var { src, attno: off + v.varattno, typ: v.vartype, desc: 0 })));
        }
        // D774: a system column of the image that is the written row (an INSERT's / UPDATE's NEW, a DELETE's OLD) is the
        // written row's, as the default reference's below; of the other image only tableoid (an UPDATE's OLD: the row stays
        // in its relation; under a root, R4c, the target the row was in, which a moved row left), or V4R-501's lanes
        if let Some(relid) = r.old_tableoid.filter(|_| old && v.varattno == -6) {
            return Ok(b.push(ExprNode::Imm(crate::nodes::misc::Imm { value: relid as i64, typ: 26 })));
        }
        // V4R-501: the replaced version's system columns, laid in the OLD image's row at `base - attno`
        if let Some(base) = r.old_sys.filter(|_| old && (-6..=-1).contains(&v.varattno)) {
            return Ok(b.push(ExprNode::Var(Var { src, attno: off + base - v.varattno, typ: v.vartype, desc: 0 })));
        }
        if (src, off) != (Source::Scan, 0) && v.varattno != -6 {
            return unsupported(Kind::Var);
        }
    }
    // D669: a RETURNING list's column of another relation, the child's row's column at its resno, where the lowerer lays it
    if let Some((src, off)) = b.returning.and_then(|r| r.outer).filter(|_| v.varno == OUTER_VAR && v.varattno > 0 && v.varlevelsup == 0) {
        return Ok(b.push(ExprNode::Var(Var { src, attno: off + v.varattno, typ: v.vartype, desc: 0 })));
    }
    if let Some((varno, outer)) = b.outer {
        if v.varno == varno && v.varattno > 0 {
            let e = outer.nth(v.varattno as usize - 1);
            let e = e.as_target_entry().map_or(e, |t| t.expr);
            return b.compile(e);
        }
    }
    // PostgreSQL's TableOidAttributeNumber: the scan's relation OID, a plan-time constant when the builder names it
    if let Some((varno, tableoid)) = b.tableoid {
        if v.varno == varno && v.varattno == -6 {
            return Ok(match tableoid {
                // an immediate, not a Const: a Const is a parameter word the lowerer matches to a literal of the tree
                // (tables.rs `needs_of`), and the tree has a Var here
                crate::nodes::TableOid::Rel(relid) => b.push(ExprNode::Imm(crate::nodes::misc::Imm { value: relid as i64, typ: 26 })),
                // D3467: the row's own relation, a column of one of its sources
                crate::nodes::TableOid::Col(src, attno) => b.push(ExprNode::Var(Var { src, attno, typ: 26, desc: 0 })),
            });
        }
    }
    let src = match v.varno {
        INNER_VAR => Source::Inner,
        OUTER_VAR => Source::Outer,
        _ => Source::Scan,
    };
    if v.varattno == 0 {
        return crate::nodes::tuple::compile_whole_row(b, src, v.vartype);
    }
    Ok(b.push(ExprNode::Var(Var { src, attno: v.varattno, typ: v.vartype, desc: 0 })))
}

/// D774: the whole row of an OLD / NEW image laid at `off` past its source's first column: at 0 the whole-row `Var` over the
/// source (lowerv4's expr.wholerow forms it, step/expr_row.rs `rewrite`); D2729: at an offset (MERGE's NEW, after its OLD
/// in the inner row: lowerv4 merge/ret.rs) the ROW of the image's columns `off + 1 ..` under the rowtype's descriptor, as
/// that rewrite forms one: trailing dropped columns take no argument, a dropped one before a live one reads the first live
/// column, whose value the row site skips (`attisdropped`).
fn image_row(b: &mut Builder<'_, '_>, src: Source, off: i16, typ: Oid) -> CompileResult<NodeId> {
    let id = crate::nodes::tuple::compile_whole_row(b, src, typ)?;
    if off == 0 {
        return Ok(id);
    }
    let ExprNode::Var(w) = b.prog.nodes[id.0 as usize] else { return unsupported(Kind::Var) };
    // a named rowtype's descriptor (a `record` one has none: no image is a `record`)
    let Some(desc) = b.state.descs.get(w.desc as usize).and_then(|d| d.desc.as_ref()) else { return unsupported(Kind::Var) };
    let cols: Vec<(bool, Oid)> = desc.attrs.iter().take(desc.natts.max(0) as usize).map(|a| (a.attisdropped, a.atttypid)).collect(); // alloc: Program
    let n = cols.iter().rposition(|c| !c.0).map_or(0, |p| p + 1);
    let first = cols.iter().position(|c| !c.0).unwrap_or(0);
    let args: Vec<NodeId> = (0..n).map(|a| {
        let a = if cols[a].0 { first } else { a };
        b.push(ExprNode::Var(Var { src, attno: off + a as i16 + 1, typ: cols[a].1, desc: 0 }))
    }).collect(); // alloc: Program
    let kids = b.kids_of(&args);
    let site = crate::nodes::row::row_site(b, n);
    b.prog.nodes[id.0 as usize] = ExprNode::Row(crate::nodes::row::RowExpr { args: kids, row_type: typ, desc: w.desc, natts: cols.len() as u32, site });
    Ok(id)
}

#[inline(always)]
pub fn eval(n: &Var, cx: &mut Cx<'_, '_>) -> PgResult<NullableDatum> {
    let src = cx.row.source(n.src);
    if n.attno < 0 {
        return sysattr(n, src);
    }
    if let Cols::Slot { types, tdtypeid, .. } = *src {
        if !types.is_empty() {
            check_slot(n, types, tdtypeid)?;
        }
    }
    match src.get(n.attno) {
        Some(v) => Ok(v),
        None => Err(missing(n)),
    }
}

/// C `CheckVarSlotCompatibility`: the column exists, is not dropped, and has the Var's type.
#[inline(always)]
fn check_slot(n: &Var, types: &[Oid], tdtypeid: Oid) -> PgResult<()> {
    let i = n.attno as usize - 1;
    if i >= types.len() {
        return Err(out_of_range(n.attno, types.len()));
    }
    if types[i] == 0 {
        return Err(dropped(n.attno, tdtypeid));
    }
    if types[i] != n.typ {
        return Err(wrong_type(n.attno, tdtypeid, types[i], n.typ));
    }
    Ok(())
}

#[cold]
#[inline(never)]
fn out_of_range(attno: i16, natts: usize) -> Box<PgError> {
    Box::new(PgError::error(format!("attribute number {attno} exceeds number of columns {natts}")).with_sqlstate(types_error::ERRCODE_INTERNAL_ERROR)) // alloc: Cold
}

#[cold]
#[inline(never)]
fn dropped(attno: i16, tdtypeid: Oid) -> Box<PgError> {
    let t = format_type::format_type_be(tdtypeid).unwrap_or_else(|_| tdtypeid.to_string()); // alloc: Cold
    Box::new(PgError::error(format!("attribute {attno} of type {t} has been dropped")).with_sqlstate(types_error::ERRCODE_UNDEFINED_COLUMN)) // alloc: Cold
}

#[cold]
#[inline(never)]
fn wrong_type(attno: i16, tdtypeid: Oid, tabletype: Oid, vartype: Oid) -> Box<PgError> {
    let f = |o: Oid| format_type::format_type_be(o).unwrap_or_else(|_| o.to_string()); // alloc: Cold
    Box::new( // alloc: Cold
        PgError::error(format!("attribute {attno} of type {} has wrong type", f(tdtypeid))) // alloc: Cold
            .with_sqlstate(types_error::ERRCODE_DATATYPE_MISMATCH)
            .with_detail(format!("Table has type {}, but query expects {}.", f(tabletype), f(vartype))), // alloc: Cold
    )
}

#[cold]
#[inline(never)]
fn missing(n: &Var) -> Box<PgError> {
    Box::new(PgError::error(format!("gravity_expr: attribute {} of the {:?} row is not present", n.attno, n.src))) // alloc: Cold
}

/// C `ExecEvalSysVar`: the slot answers (`slot_getsysattr`); a row without one cannot.
#[cold]
#[inline(never)]
fn sysattr(n: &Var, src: &Cols<'_>) -> PgResult<NullableDatum> {
    let Cols::Slot { slot: Some(slot), .. } = *src else {
        return Err(Box::new(PgError::error("cannot retrieve a system column in this context").with_sqlstate(types_error::ERRCODE_FEATURE_NOT_SUPPORTED))); // alloc: Cold
    };
    let mut isnull = false;
    let value = exectuples::slot_getsysattr(slot, n.attno as i32, &mut isnull)?;
    Ok(NullableDatum { value, isnull })
}
