//! Plan facts: a recursive UNION's anonymous `record` or `record[]` dedup key's columns (with#62 / #65 / #82 / #86: the
//! SEARCH / CYCLE columns PostgreSQL's rewriteSearchAndCycle adds, `ROW(depth, cols)` breadth first, `ARRAY[ROW(cols)]`
//! then `array_cat(seq, ARRAY[ROW(cols)])` depth first and for CYCLE's path, or a query's own `path || ROW(..)`). The
//! key's values are the seed's rows and every generation's term rows, and a generation reads the rows before it through
//! the work table. The columns are known before any row when, through both terms, every value of the column (a `record`
//! key) or every element of it (a `record[]` key) is one ROW() (facts::record's producers: a `RowExpr` of type `record`,
//! or a folded record constant) and all of them agree on each column's type and collation. An array builder's elements
//! are its arguments' (`ARRAY[..]` over ROW()s or over such arrays; catalog `array_build`: array_cat, array_append,
//! array_prepend), and the union's own column read back through its work table holds only what the union produced
//! before: the seed's values, then each generation's (by induction, those ROW()s). Anything else (a function's record, a
//! CASE, a NULL constant, another column of the work table, rows of different column types) answers no columns, and the
//! key keeps its refusal: two records `record_eq` raises on are never one key's values here.
use gravity_program::ArenaVec;
use types_nodes::plannodes::{PlannedStmt, RecursiveUnion};
use types_nodes::primnodes::{INNER_VAR, OUTER_VAR};
use types_nodes::{Node, NodeTag};

use super::record::{agree, col, is_row, relabels, row_args};
use crate::catalog::arrays::{array_build, record_array};
use crate::catalog::{anonymous_record, Facts};
use crate::ops::shared::sort::EqKey;
use crate::walk::Builder;

/// How deep the walk follows pass-through columns before it gives up (a plan deeper than this keeps the refusal).
const DEPTH: u8 = 32;

/// What a column delivers: values (or elements) that are each one ROW() agreeing with `Row`'s, or the union's own column
/// as the generations before produced it (`Same`).
#[derive(Clone, Copy)]
enum Prod<'p> {
    Row(Node<'p>),
    Same,
}

/// The walk over one dedup key: the statement, the catalog, the union's work table (`wtParam`) and the key's column (its
/// resno in the union's target list, its attno in the work table).
struct Walk<'p, 'f> {
    stmt: &'p PlannedStmt<'p>,
    f: &'f dyn Facts,
    wt: i32,
    col: i16,
}

/// Each dedup key's columns (type and collation) of RecursiveUnion `ru`, in key order (`keys`: decompose's `eq_keys` of
/// its `dupColIdx`): for a `record` or `record[]` key whose values (elements) are known before any row, its ROW()'s
/// columns; empty for every other key. Allocates in the plan's arena only (a folded record constant's descriptor comes
/// from the record cache, on the server).
pub fn dedup_rows<'p>(b: &Builder<'p, '_>, ru: &'p RecursiveUnion<'p>, keys: &[EqKey]) -> ArenaVec<ArenaVec<(u32, u32)>> {
    let mut out = ArenaVec::with_capacity_in(keys.len(), b.arena);
    for k in keys {
        let mut cols = ArenaVec::new_in(b.arena);
        let elems = record_array(k.typid);
        if elems || anonymous_record(k.typid) {
            let w = Walk { stmt: b.stmt, f: b.facts, wt: ru.wtParam, col: k.resno };
            if let Some(row) = key_row(&w, ru, elems) {
                match row_args(row) {
                    Some(args) => args.iter().for_each(|a| cols.push(col(a))),
                    None => super::record::columns(b.facts, row).into_iter().for_each(|c| cols.push(c)),
                }
            }
        }
        out.push(cols);
    }
    out
}

/// The ROW() every value (`elems` false) or every element (`elems` true) of the walk's column agrees with, through union
/// `ru`'s seed and its recursive term; `None` when the columns are not known before a row.
fn key_row<'p>(w: &Walk<'p, '_>, ru: &'p RecursiveUnion<'p>, elems: bool) -> Option<Node<'p>> {
    let seed = column(w, ru.plan.lefttree?, w.col, elems, DEPTH)?;
    let term = column(w, ru.plan.righttree?, w.col, elems, DEPTH)?;
    match merge(w.f, seed, term)? {
        Prod::Row(r) => Some(r),
        Prod::Same => None,
    }
}

/// Two producers of one column: both ROW()s must agree (facts::record `agree`); the union's own column agrees with any.
fn merge<'p>(f: &dyn Facts, a: Prod<'p>, b: Prod<'p>) -> Option<Prod<'p>> {
    match (a, b) {
        (Prod::Row(x), Prod::Row(y)) => agree(f, x, y).map(Prod::Row),
        (Prod::Row(x), Prod::Same) | (Prod::Same, Prod::Row(x)) => Some(Prod::Row(x)),
        (Prod::Same, Prod::Same) => Some(Prod::Same),
    }
}

/// `merge` over every producer `ps` yields; `None` when one is unknown or there is none.
fn fold<'p>(f: &dyn Facts, mut ps: impl Iterator<Item = Option<Prod<'p>>>) -> Option<Prod<'p>> {
    let first = ps.next()??;
    ps.try_fold(first, |acc, p| merge(f, acc, p?))
}

/// What column `col` of plan node `n` delivers: its target list's entry, resolved; an Append's or MergeAppend's arms.
fn column<'p>(w: &Walk<'p, '_>, n: Node<'p>, col: i16, elems: bool, depth: u8) -> Option<Prod<'p>> {
    crate::walk::stack_check(); // D3748
    let depth = depth.checked_sub(1)?;
    if let Some(arms) = n.as_append().map(|a| &a.appendplans).or_else(|| n.as_merge_append().map(|m| &m.mergeplans)) {
        return fold(w.f, arms.iter().map(|arm| column(w, arm, col, elems, depth)));
    }
    let p = n.as_plan()?;
    let te = p.targetlist.iter().filter_map(|t| t.as_target_entry()).find(|t| t.resno == col)?;
    expr(w, n, te.expr, elems, depth)
}

/// What expression `e`, evaluated by plan node `n` over its inputs, delivers: its values (`elems` false: one ROW(), or a
/// column passed through) or its elements (`elems` true: an `ARRAY[..]`, an array builder over its arguments, or a column
/// passed through).
fn expr<'p>(w: &Walk<'p, '_>, n: Node<'p>, e: Node<'p>, elems: bool, depth: u8) -> Option<Prod<'p>> {
    crate::walk::stack_check(); // D3748
    let e = relabels(e);
    if let Some(v) = e.as_var().filter(|v| v.varlevelsup == 0) {
        let p = n.as_plan()?;
        return match v.varno {
            OUTER_VAR => column(w, p.lefttree?, v.varattno, elems, depth),
            INNER_VAR => column(w, p.righttree?, v.varattno, elems, depth),
            scan => scanned(w, n, scan, v.varattno, elems, depth),
        };
    }
    if !elems {
        return is_row(w.f, e).then_some(Prod::Row(e));
    }
    if let Some(a) = e.as_array_expr() {
        // ARRAY[ROW(..), ..]: each element a value; ARRAY[ARRAY[..], ..] (multidims): each element's elements
        return fold(w.f, a.elements.iter().map(|x| expr(w, n, x, a.multidims, depth)));
    }
    let (funcid, args) = match (e.as_func_expr(), e.as_op_expr()) {
        (Some(f), _) => (f.funcid, &f.args),
        (_, Some(o)) => (o.opfuncid, &o.args),
        _ => return None,
    };
    if !array_build(funcid) {
        return None;
    }
    // an argument of type record is one element; any other (the array) gives its elements
    fold(w.f, args.iter().map(|x| expr(w, n, x, !crate::catalog::expr_type::type_of(x).is_some_and(anonymous_record), depth)))
}

/// What column `col` of the relation scan `n` reads as range table entry `scan` delivers: the union's own column through
/// its work table (`Same`), a subquery scan's subplan column, a CTE's plan column, a VALUES list's rows.
fn scanned<'p>(w: &Walk<'p, '_>, n: Node<'p>, scan: i32, col: i16, elems: bool, depth: u8) -> Option<Prod<'p>> {
    let depth = depth.checked_sub(1)?;
    match n.node_tag() {
        NodeTag::T_WorkTableScan => {
            let ws = n.as_work_table_scan()?;
            (ws.scan.scanrelid as i32 == scan && ws.wtParam == w.wt && col == w.col).then_some(Prod::Same)
        }
        NodeTag::T_SubqueryScan => {
            let s = n.as_subquery_scan()?;
            (s.scan.scanrelid as i32 == scan).then_some(())?;
            column(w, s.subplan?, col, elems, depth)
        }
        NodeTag::T_CteScan => {
            let cs = n.as_cte_scan()?;
            (cs.scan.scanrelid as i32 == scan).then_some(())?;
            column(w, crate::rerun::subplan_root(w.stmt, cs.ctePlanId)?, col, elems, depth)
        }
        NodeTag::T_ValuesScan => {
            let vs = n.as_values_scan()?;
            (vs.scan.scanrelid as i32 == scan).then_some(())?;
            let k = usize::try_from(col).ok()?.checked_sub(1)?;
            fold(w.f, vs.values_lists.iter().map(|l| l.as_list().and_then(|l| (k < l.len()).then(|| l.nth(k))).and_then(|e| expr(w, n, e, elems, depth))))
        }
        _ => None,
    }
}
