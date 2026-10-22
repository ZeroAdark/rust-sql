//! Plan facts: an anonymous `record` key's columns. A `record` value carries its row type in the datum (the blessed typmod
//! of the ROW() that formed it), so its type id alone names no columns; PostgreSQL's `record_eq` / `record_cmp` read them
//! per value. A key's columns are known before any row when every producer of the key's values is one ROW() (a
//! `RowExpr` of type `record`, or the `record` constant the planner folded a constant ROW() to, whose blessed typmod the
//! backend's record cache answers) and all of them agree on each column's type and collation: through the plan's
//! pass-through columns (a Var of an outer or inner child's target list), a subquery scan's subplan, a VALUES list's
//! rows, a CTE's plan, every arm of an Append / MergeAppend and both inputs of a SetOp. Anything else (a function's
//! record, a whole-row reference, a recursive work table, rows of different column types) answers `None`, and the key
//! keeps its refusal: two producers that disagree are values `record_eq` raises on, never two equal images.
use types_nodes::primnodes::{INNER_VAR, OUTER_VAR};
use types_nodes::{Node, NodeTag};

use crate::catalog::{anonymous_record, Facts};
use crate::walk::Builder;

/// How deep the walk follows pass-through columns before it gives up (a plan deeper than this keeps the refusal).
const DEPTH: u8 = 32;

/// The ROW() standing for the columns of anonymous `record` key `e` of group node `id` (an Agg, Group, Unique or SetOp;
/// its keys' `resno` index its input's target list, 0 for a DISTINCT aggregate's argument): `e` itself when it is one
/// ROW(); else the input column `resno` resolved through the plan (the node's left input, and its right one for a
/// SetOp, which must agree); else, for an argument still over the node's own input (`agg`: its Vars are the node's),
/// that Var's input column. `None` when the columns are not known before a row.
pub fn key_row<'p>(b: &Builder<'p, '_>, id: i32, resno: i16, e: Node<'p>, agg: bool) -> Option<Node<'p>> {
    if is_row(b.facts, e) {
        return Some(e);
    }
    let node = plan_node(b, id)?;
    let col = match () {
        _ if resno > 0 => resno,
        _ if agg => e.as_var().filter(|v| v.varno == OUTER_VAR && v.varlevelsup == 0)?.varattno,
        _ => return None,
    };
    let p = node.as_plan()?;
    let left = column(b, p.lefttree?, col, DEPTH)?;
    match node.node_tag() {
        NodeTag::T_SetOp => agree(b.facts, left, column(b, p.righttree?, col, DEPTH)?),
        _ => Some(left),
    }
}

/// D3374 (e) (record keys E2): whether anonymous `record` key `e` of node `id` (as `key_row` reads it) may hold rows of
/// more than one column layout: no ROW() every row agrees on (`key_row`), and not a whole-row reference to a scanned
/// relation (a table's, a subquery's or a CTE's rows are one row type), however many pass-through columns lie between.
pub fn per_row<'p>(b: &Builder<'p, '_>, id: i32, resno: i16, e: Node<'p>, agg: bool) -> bool {
    if key_row(b, id, resno, e, agg).is_some() || whole_row_var(e) {
        return false;
    }
    let Some(p) = plan_node(b, id).and_then(|n| n.as_plan()) else { return true };
    let col = match () {
        _ if resno > 0 => resno,
        _ if agg => match e.as_var().filter(|v| v.varno == OUTER_VAR && v.varlevelsup == 0) {
            Some(v) => v.varattno,
            None => return true,
        },
        _ => return true,
    };
    !p.lefttree.is_some_and(|l| whole_col(b, l, col, DEPTH))
}

/// D3374 (e) (record keys E2): whether column `col` of plan node `id` itself (a MergeAppend's merge key) may hold
/// anonymous records of more than one column layout: no ROW() every arm agrees on (`column`'s walk), and no whole-row
/// reference to a scanned relation.
pub fn node_per_row(b: &Builder<'_, '_>, id: i32, col: i16) -> bool {
    plan_node(b, id).is_none_or(|n| column(b, n, col, DEPTH).is_none() && !whole_col(b, n, col, DEPTH))
}

/// `e` (through relabels) is a whole-row Var of a scanned relation (`varattno` 0).
fn whole_row_var(e: Node<'_>) -> bool {
    relabels(e).as_var().is_some_and(|v| v.varlevelsup == 0 && v.varattno == 0 && v.varno != OUTER_VAR && v.varno != INNER_VAR)
}

/// Column `col` of plan node `n` is a whole-row Var of a scanned relation, through pass-through columns (`column`'s walk).
fn whole_col<'p>(b: &Builder<'p, '_>, n: Node<'p>, col: i16, depth: u8) -> bool {
    crate::walk::stack_check(); // D3748
    let Some(depth) = depth.checked_sub(1) else { return false };
    let Some(p) = n.as_plan() else { return false };
    let Some(te) = p.targetlist.iter().filter_map(|t| t.as_target_entry()).find(|t| t.resno == col) else { return false };
    let e = relabels(te.expr);
    if whole_row_var(e) {
        return true;
    }
    let Some(v) = e.as_var().filter(|v| v.varlevelsup == 0) else { return false };
    match v.varno {
        OUTER_VAR => p.lefttree.is_some_and(|l| whole_col(b, l, v.varattno, depth)),
        INNER_VAR => p.righttree.is_some_and(|r| whole_col(b, r, v.varattno, depth)),
        scan => match n.node_tag() {
            NodeTag::T_SubqueryScan => n.as_subquery_scan().is_some_and(|s| s.scan.scanrelid as i32 == scan && s.subplan.is_some_and(|sp| whole_col(b, sp, v.varattno, depth))),
            NodeTag::T_CteScan => n.as_cte_scan().is_some_and(|cs| cs.scan.scanrelid as i32 == scan && crate::rerun::subplan_root(b.stmt, cs.ctePlanId).is_some_and(|r| whole_col(b, r, v.varattno, depth))),
            _ => false,
        },
    }
}

/// The columns of ROW() `row` (`key_row`'s answer): each column's type and collation (`ExecTypeFromExprList`'s, the
/// blessed descriptor's `atttypid` / `attcollation`); empty when `row` is not one. Read where the key's form is built
/// (lowerv4), never on decompose's steady-state path.
pub fn columns(f: &dyn Facts, row: Node<'_>) -> Vec<(u32, u32)> {
    match row_args(row) {
        Some(args) => args.iter().map(col).collect(), // alloc: Program
        None => desc_cols(f, row).unwrap_or_default(),
    }
}

/// D1292 inside a record: a column of ROW() `row` over a collatable type with no collation (conflicting implicit
/// collations, `ROW(a || b)` over a "C" and a "POSIX" column): `record_eq` / `hash_record` raise 42P22 at the first
/// compare, so the key refuses by sort's name, as a scalar key does.
pub fn indeterminate(f: &dyn Facts, row: Node<'_>) -> bool {
    let loose = |(t, c): (u32, u32)| crate::ops::shared::sort::indeterminate(f, t, c);
    match row_args(row) {
        Some(args) => args.iter().map(col).any(loose),
        None => desc_cols(f, row).is_some_and(|cols| cols.into_iter().any(loose)),
    }
}

/// Whether `e` (through relabels) is one ROW() of type `record`, or a non-NULL `record` constant whose blessed row type
/// the record cache answers.
pub(super) fn is_row(f: &dyn Facts, e: Node<'_>) -> bool {
    row_args(e).is_some() || desc_cols(f, e).is_some()
}

/// The arguments of `e` when it is a ROW() of type `record`, through relabels.
pub(super) fn row_args<'p>(e: Node<'p>) -> Option<&'p types_nodes::list::NodeList<'p>> {
    relabels(e).as_row_expr().filter(|r| anonymous_record(r.row_typeid)).map(|r| &r.args)
}

/// A ROW() argument's column: its type (0 when untyped: the key's form refuses it) and collation.
pub(super) fn col(a: Node<'_>) -> (u32, u32) {
    (crate::catalog::expr_type::type_of(a).unwrap_or(0), crate::catalog::expr_type::collation_of(a))
}

/// The columns of `e` when it is a non-NULL `record` constant: the row type its datum's typmod names
/// (`Facts::record_cols`, the backend's record cache).
fn desc_cols(f: &dyn Facts, e: Node<'_>) -> Option<Vec<(u32, u32)>> {
    let c = relabels(e).as_const().filter(|c| anonymous_record(c.consttype) && !c.constisnull)?;
    // a composite datum's payload starts with HeapTupleHeaderData's t_datum: datum_typmod, then datum_typeid
    let typmod = i32::from_le_bytes(crate::expr::const_bytes(c)?.get(..4)?.try_into().ok()?);
    f.record_cols(typmod)
}

/// `e` through the relabels that leave its value as it is (`expr::through_relabel`).
pub(super) fn relabels(mut e: Node<'_>) -> Node<'_> {
    while let Some(a) = crate::expr::through_relabel(e) {
        e = a;
    }
    e
}

/// The ROW() column `col` of plan node `n` delivers: its target list's entry, resolved; an Append's or MergeAppend's
/// arms, which must agree.
fn column<'p>(b: &Builder<'p, '_>, n: Node<'p>, col: i16, depth: u8) -> Option<Node<'p>> {
    crate::walk::stack_check(); // D3748
    let depth = depth.checked_sub(1)?;
    if let Some(arms) = n.as_append().map(|a| &a.appendplans).or_else(|| n.as_merge_append().map(|m| &m.mergeplans)) {
        let mut rows = arms.iter().map(|arm| column(b, arm, col, depth));
        let first = rows.next()??;
        return rows.try_fold(first, |acc, r| agree(b.facts, acc, r?));
    }
    let p = n.as_plan()?;
    let te = p.targetlist.iter().filter_map(|t| t.as_target_entry()).find(|t| t.resno == col)?;
    expr(b, n, te.expr, depth)
}

/// The ROW() expression `e`, evaluated by plan node `n` over its inputs, produces.
fn expr<'p>(b: &Builder<'p, '_>, n: Node<'p>, e: Node<'p>, depth: u8) -> Option<Node<'p>> {
    let e = relabels(e);
    if is_row(b.facts, e) {
        return Some(e);
    }
    let v = e.as_var().filter(|v| v.varlevelsup == 0)?;
    let p = n.as_plan()?;
    match v.varno {
        OUTER_VAR => column(b, p.lefttree?, v.varattno, depth),
        INNER_VAR => column(b, p.righttree?, v.varattno, depth),
        scan => scanned(b, n, scan, v.varattno, depth),
    }
}

/// Column `col` of the relation scan `n` reads as range table entry `scan`: a subquery scan's subplan column, a VALUES
/// list's rows (each row's entry, all agreeing), a CTE's plan column.
fn scanned<'p>(b: &Builder<'p, '_>, n: Node<'p>, scan: i32, col: i16, depth: u8) -> Option<Node<'p>> {
    let depth = depth.checked_sub(1)?;
    match n.node_tag() {
        NodeTag::T_SubqueryScan => {
            let s = n.as_subquery_scan()?;
            (s.scan.scanrelid as i32 == scan).then_some(())?;
            column(b, s.subplan?, col, depth)
        }
        NodeTag::T_ValuesScan => {
            let vs = n.as_values_scan()?;
            (vs.scan.scanrelid as i32 == scan).then_some(())?;
            let k = usize::try_from(col).ok()?.checked_sub(1)?;
            let mut rows = vs.values_lists.iter().map(|l| l.as_list().and_then(|l| (k < l.len()).then(|| l.nth(k))).filter(|e| is_row(b.facts, *e)));
            let first = rows.next()??;
            rows.try_fold(first, |acc, r| agree(b.facts, acc, r?))
        }
        NodeTag::T_CteScan => {
            let cs = n.as_cte_scan()?;
            (cs.scan.scanrelid as i32 == scan).then_some(())?;
            column(b, crate::rerun::subplan_root(b.stmt, cs.ctePlanId)?, col, depth)
        }
        _ => None,
    }
}

/// `a` when ROW()s `a` and `b` have the same columns (count, each type and collation), else `None`. Two ROW()s compare
/// argument by argument (no allocation: decompose's steady state allocates nothing); a record constant's descriptor is
/// read from the record cache.
pub(super) fn agree<'p>(f: &dyn Facts, a: Node<'p>, b: Node<'p>) -> Option<Node<'p>> {
    let same = match (row_args(a), row_args(b)) {
        (Some(x), Some(y)) => x.len() == y.len() && x.iter().zip(y.iter()).all(|(p, q)| col(p) == col(q)),
        _ => is_row(f, a) && is_row(f, b) && columns(f, a) == columns(f, b),
    };
    same.then_some(a)
}

/// The plan node numbered `id` (numbers are statement-wide: its main tree and its subplans).
fn plan_node<'p>(b: &Builder<'p, '_>, id: i32) -> Option<Node<'p>> {
    let mut found = None;
    crate::rerun::each_plan_node(b.stmt, &mut |n| {
        if found.is_none() && n.as_plan().is_some_and(|p| p.plan_node_id == id) {
            found = Some(n);
        }
    });
    found
}
