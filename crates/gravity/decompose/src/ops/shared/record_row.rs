//! The `record` rows a whole-row `Var` forms over a subquery, CTE, VALUES, function or named tuplestore scan
//! (C `ExecEvalWholeRowVar`'s RECORD leg): built by the scan families, read by the lowerer's whole-row expressions.
use gravity_program::ArenaVec;
use types_nodes::list::NodeList;
use types_nodes::plannodes::{Plan, Scan};
use types_nodes::Node;

use crate::expr;
use crate::walk::Builder;

/// D437: the `record` row a whole-row `Var` of subquery scan `scanrelid` forms (C `ExecEvalWholeRowVar`'s RECORD leg):
/// the subplan's non-junk target entries (the junk filter `ExecInitWholeRowVar` puts over a SubqueryScan's input), each
/// its expression, its resno and its column name after `ExecTypeSetColNames` (the RTE's `eref` alias where it names one,
/// else the entry's `resname`). The lowerer blesses a descriptor of these columns (lowerv4 step/expr_row.rs).
#[derive(Debug)]
pub struct RecordRow<'p> {
    pub scanrelid: u32,
    pub cols: ArenaVec<RecordCol<'p>>,
    /// D732-rest: the scan node forming the row (a SubqueryScan's plan node id: its `NodeOps` entry's `first_op` is where
    /// EvalPlanQual hands a COPY mark's stored row in); a VALUES scan's is its own; a function row's -1 (no scan node).
    pub node_id: i32,
    /// A named tuplestore scan's row (`named_record_row`): the relation whose descriptor the ENR's is; 0 for the others.
    pub named: u32,
    /// D1685: a flattened UNION ALL's row (`union_record_row`): the Append output column (resno) holding the COPY mark's
    /// whole row (the arms' ROW(..) junk), which a recheck hands on as the stored record itself; 0 for the others.
    pub whole: i16,
}

#[derive(Clone, Copy, Debug)]
pub struct RecordCol<'p> {
    /// The column's expression (a subquery's target entry, a VALUES scan's first-list cell, a scalar FROM call): its type,
    /// typmod and collation are C `exprType` / `exprTypmod` / `exprCollation`'s; none where `ty` names them.
    pub expr: Option<Node<'p>>,
    /// `(type, typmod, collation)` of a column without an expression (a column definition list's, the ordinality's).
    pub ty: (u32, i32, u32),
    pub resno: i16,
    pub name: &'p str,
}

const RECORDOID: u32 = 2249;

/// The subquery scan's lists read its own row whole as a `record` (a level-0 `Var` of `scanrelid`, attribute 0).
fn reads_record_row<'p>(lists: [&'p NodeList<'p>; 2], scanrelid: u32) -> bool {
    reads_row(lists, scanrelid, false)
}

/// `reads_record_row`; `view` (writesubplan A2): a subquery expanded from a view, whose whole row C `makeWholeRowVar` types
/// as the view's rowtype (a write's COPY mark's `wholerow` junk over it too), any type. producerchain cluster A: or an
/// inlined SQL function's subquery or one function's FunctionScan, typed as the function's composite result (PG 18.6).
fn reads_row<'p>(lists: [&'p NodeList<'p>; 2], scanrelid: u32, view: bool) -> bool {
    let mut hit = false;
    for l in lists {
        for n in l.iter() {
            expr::walk(n, &mut |x| {
                hit |= x.as_var().is_some_and(|v| v.varattno == 0 && v.varlevelsup == 0 && v.varno == scanrelid as i32 && (view || v.vartype == RECORDOID));
                !hit
            });
        }
    }
    hit
}

/// A2 (EXPR-ODDS): a CteScan's the same way, over its CTE's plan tree: C `ExecInitWholeRowVar` puts the junk filter over a
/// `CteScanState`'s `cteplanstate` as over a subquery's, and nest.cte's source hands the CTE's stored row as scan columns
/// 1..n in its last Project's order (nest/cte.rs `CteRows::row`), the non-junk entries' resnos.
pub(crate) fn record_row<'p>(b: &mut Builder<'p, '_>, scan: &'p Scan<'p>, root: Node<'p>) {
    let scanrelid = scan.scanrelid;
    // makeWholeRowVar types a view's or an inlined SQL function's subquery row (its `functions`, which setrefs drops) by its
    // rowtype, any other subquery's `record`: every subquery's whole row is its subplan's row
    let view = b.rte(scanrelid).is_some_and(|r| r.rtekind == types_nodes::parsenodes::RTEKind::RTE_SUBQUERY);
    if !reads_row([&scan.plan.targetlist, &scan.plan.qual], scanrelid, view) {
        return;
    }
    let Some(tlist) = root.as_plan().map(|p| &p.targetlist) else { return };
    let aliases = b.rte(scanrelid).and_then(|r| r.eref).map(|e| &e.colnames);
    let mut cols: ArenaVec<RecordCol<'p>> = ArenaVec::new_in(b.arena); // alloc: Cold
    for te in tlist.iter().filter_map(|n| n.as_target_entry()).filter(|t| !t.resjunk) {
        let alias = aliases.filter(|a| cols.len() < a.len()).and_then(|a| a.nth(cols.len()).as_string()).map(|s| s.sval).filter(|s| !s.is_empty());
        cols.push(RecordCol { expr: Some(te.expr), ty: (0, -1, 0), resno: te.resno, name: alias.or(te.resname).unwrap_or("") });
    }
    b.graph.record_rows.push(RecordRow { scanrelid, cols, node_id: scan.plan.plan_node_id, named: 0, whole: 0 });
}

/// The RTE's `eref` name of column `i` (C `ExecTypeSetColNames`, which names a `record` whole row's columns); "" past it.
fn eref_name<'p>(b: &Builder<'p, '_>, scanrelid: u32, i: usize) -> &'p str {
    let names = b.rte(scanrelid).and_then(|r| r.eref).map(|e| &e.colnames).filter(|a| i < a.len());
    names.and_then(|a| a.nth(i).as_string()).map_or("", |s| s.sval)
}

/// D693: a FunctionScan's `record` row, C `ExecInitFunctionScan`'s descriptor: a scalar call's one column typed by its
/// expression, a column definition list's columns, the ordinality's bigint; D1073: a composite member's (OUT parameters,
/// a row type) from its call's result descriptor (`Facts::call_col`); D3374: a dropped attribute of that row type typed 0
/// (`Facts::call_col_dropped`), as `named_record_row`'s. A member none of them types (no catalog) leaves no record row (the
/// lowerer refuses the whole row by name).
pub fn function_record_row<'p>(b: &mut Builder<'p, '_>, scanrelid: u32, lists: [&'p NodeList<'p>; 2], functions: &'p NodeList<'p>, ordinality: bool) {
    // one function without ordinality: makeWholeRowVar types its whole row as a composite result's type, not `record`
    if !reads_row(lists, scanrelid, !ordinality && functions.len() == 1) {
        return;
    }
    let mut cols: ArenaVec<RecordCol<'p>> = ArenaVec::new_in(b.arena); // alloc: Cold
    for f in functions.iter().filter_map(|f| f.as_range_tbl_function()) {
        let n = f.funccolcount.max(1) as usize;
        let scalar = f.funcexpr.and_then(crate::catalog::expr_type::type_of).is_some_and(|t| t != RECORDOID && crate::catalog::typefacts::row_of(t).is_some());
        for i in 0..n {
            let (expr, ty) = match f.funccoltypes.len() == n {
                true => (None, (f.funccoltypes.nth(i), f.funccoltypmods.iter().nth(i).unwrap_or(-1), f.funccolcollations.iter().nth(i).unwrap_or(0))),
                false if scalar && n == 1 => (f.funcexpr, (0, -1, 0)),
                false => match f.funcexpr.and_then(|e| b.facts.call_col(e, i as i16 + 1)) {
                    Some(ty) => (None, ty),
                    // D3374: a dropped attribute of the call's row type holds no value (PG's scan slot leaves it NULL):
                    // typed 0, as `named_record_row`'s dropped one (the copy entry hands it NULL, lowerv4 copy_row.rs)
                    None if f.funcexpr.is_some_and(|e| b.facts.call_col_dropped(e, i as i16 + 1)) => (None, (0, -1, 0)),
                    None => return,
                },
            };
            cols.push(RecordCol { expr, ty, resno: cols.len() as i16 + 1, name: eref_name(b, scanrelid, cols.len()) });
        }
    }
    if ordinality {
        cols.push(RecordCol { expr: None, ty: (20, -1, 0), resno: cols.len() as i16 + 1, name: eref_name(b, scanrelid, cols.len()) });
    }
    // no SubqueryScan: a function row's COPY leg enters past its FROM step (D890, lowerv4 copy_row.rs `boundary`)
    b.graph.record_rows.push(RecordRow { scanrelid, cols, node_id: -1, named: 0, whole: 0 });
}

/// D3292: a TableFuncScan (XMLTABLE / JSON_TABLE in FROM) whose lists read its own row whole as a `record` (a COPY row
/// mark's `wholerow{id}`): C `ExecInitTableFuncScan`'s scan descriptor (`BuildDescFromLists` over the column names, types,
/// typmods and collations, the ordinality column among them), renamed by the RTE's `eref` (`ExecTypeSetColNames`):
/// columns 1..n, the scan node its own (as a VALUES scan's), so a COPY leg enters at the scan's first op (lowerv4
/// copy_row.rs `boundary`) and never runs the table function.
pub fn tablefunc_record_row<'p>(b: &mut Builder<'p, '_>, scanrelid: u32, plan: &'p Plan<'p>, func: &'p types_nodes::primnodes::TableFunc<'p>) {
    if !reads_record_row([&plan.targetlist, &plan.qual], scanrelid) {
        return;
    }
    let (typmods, colls) = (func.coltypmods.as_slice(), func.colcollations.as_slice());
    let mut cols: ArenaVec<RecordCol<'p>> = ArenaVec::new_in(b.arena); // alloc: Cold
    for (i, &typ) in func.coltypes.as_slice().iter().enumerate() {
        let ty = (typ, typmods.get(i).copied().unwrap_or(-1), colls.get(i).copied().unwrap_or(0));
        cols.push(RecordCol { expr: None, ty, resno: i as i16 + 1, name: eref_name(b, scanrelid, i) });
    }
    b.graph.record_rows.push(RecordRow { scanrelid, cols, node_id: plan.plan_node_id, named: 0, whole: 0 });
}

/// D732: a VALUES scan whose lists read its own row whole as a `record` (a COPY row mark's `wholerow{id}`): C's scan
/// descriptor (`ExecTypeFromExprList` over the first list, `ExecTypeSetColNames` from the RTE's `eref`), columns 1..n.
pub fn values_record_row<'p>(b: &mut Builder<'p, '_>, scanrelid: u32, plan: &'p Plan<'p>, lists: &'p NodeList<'p>) {
    if !reads_record_row([&plan.targetlist, &plan.qual], scanrelid) {
        return;
    }
    let Some(first) = lists.iter().next().and_then(|l| l.as_list()) else { return };
    let aliases = b.rte(scanrelid).and_then(|r| r.eref).map(|e| &e.colnames);
    let mut cols: ArenaVec<RecordCol<'p>> = ArenaVec::new_in(b.arena); // alloc: Cold
    for (i, e) in first.iter().enumerate() {
        let name = aliases.filter(|a| i < a.len()).and_then(|a| a.nth(i).as_string()).map_or("", |s| s.sval);
        cols.push(RecordCol { expr: Some(e), ty: (0, -1, 0), resno: i as i16 + 1, name });
    }
    b.graph.record_rows.push(RecordRow { scanrelid, cols, node_id: plan.plan_node_id, named: 0, whole: 0 });
}

/// D1685: a flattened UNION ALL (any members, D3466) whose row a MERGE's COPY mark carries (decompose ops/modifytable
/// `union_copy_rows`), read off its Append's output (`append`, setrefs' dummy target list: each entry a Var whose
/// varnosyn / varattnosyn name the subquery's column): the fields are the subquery's columns 1..n, named by the RTE's
/// `eref` and typed by the Vars, in field order whatever the output's (the stored record's fields are read by number),
/// `whole` the column of the whole row. Each column at most once, and the whole row, else no record row (the write
/// refuses by COPY_MARK). D3468: a column no reader above the Append takes (a MERGE reading only its join key) is the
/// dropped attribute's field (no expression, type 0: read as NULL into a cell no column maps).
pub fn union_record_row<'p>(b: &mut Builder<'p, '_>, scanrelid: u32, append: &'p Plan<'p>) {
    let n = b.rte(scanrelid).and_then(|r| r.eref).map_or(0, |e| e.colnames.len());
    // field j (the subquery's column j + 1) -> (the Append output column, its Var)
    let mut by: mcx::PgVec<'_, Option<(i16, Node<'p>)>> = mcx::PgVec::new_in(b.mcx);
    mcx::GrowIn::resize_in(&mut by, n, None);
    let mut whole = 0;
    for te in append.targetlist.iter().filter_map(|n| n.as_target_entry()) {
        let Some(v) = te.expr.as_var().filter(|v| v.varnosyn == scanrelid) else { continue };
        if v.varattnosyn == 0 && whole == 0 {
            whole = te.resno;
            continue;
        }
        match (v.varattnosyn as usize).checked_sub(1).and_then(|j| by.get_mut(j)) {
            Some(slot @ None) => *slot = Some((te.resno, te.expr)),
            _ => return,
        }
    }
    if n == 0 || whole <= 0 {
        return;
    }
    let mut cols: ArenaVec<RecordCol<'p>> = ArenaVec::new_in(b.arena); // alloc: Cold
    for (j, col) in by.into_iter().enumerate() {
        let name = eref_name(b, scanrelid, j);
        cols.push(match col {
            Some((resno, expr)) => RecordCol { expr: Some(expr), ty: (0, -1, 0), resno, name },
            None => RecordCol { expr: None, ty: (0, -1, 0), resno: 0, name },
        });
    }
    b.graph.record_rows.push(RecordRow { scanrelid, cols, node_id: append.plan_node_id, named: 0, whole });
}

/// EXPR-ODDS A: a named tuplestore scan whose lists read its own row whole as a `record` (a trigger body's
/// `new_table::text`): C `ExecEvalWholeRowVar`'s RECORD leg over `ExecInitNamedTuplestoreScan`'s slot, whose descriptor
/// is the ENR's (`ENRMetadataGetTupDesc`: a transition table's is its relation's own), dropped attributes kept, renamed
/// by the RTE's `eref` (`ExecTypeSetColNames`). Column `a` is the relation's attribute `a`; a dropped one (no attribute
/// facts) is typed 0. `named` names the relation: the lowerer copies its descriptor (lowerv4 step/expr_row_named.rs).
pub fn named_record_row<'p>(b: &mut Builder<'p, '_>, scanrelid: u32, plan: &'p Plan<'p>, relid: u32) {
    if !reads_record_row([&plan.targetlist, &plan.qual], scanrelid) {
        return;
    }
    let Some(rel) = b.facts.rel(relid) else { return };
    let mut cols: ArenaVec<RecordCol<'p>> = ArenaVec::new_in(b.arena); // alloc: Cold
    for a in 1..=rel.natts {
        let ty = b.facts.att(relid, a).map_or((0, -1, 0), |f| (f.typid, f.typmod, 0));
        cols.push(RecordCol { expr: None, ty, resno: a, name: eref_name(b, scanrelid, a as usize - 1) });
    }
    b.graph.record_rows.push(RecordRow { scanrelid, cols, node_id: plan.plan_node_id, named: relid, whole: 0 });
}
