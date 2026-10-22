//! Road `merge.markable` (H19, R11-L02): the merge's inner as a sealed run read by position, so PG's mark / restore of the
//! inner tuple is a word (merge/mergejoin.rs `W_MARK` / `W_IP`) and no tuple is copied (PG's `ExecMarkPos` /
//! `ExecRestrPos`). This file holds what merge.join's step builds its compare and its joined rows from: the clauses' compare
//! calls (PG's `MJExamineQuals`: support function 1 of each clause's btree family for the operator's input types, under its
//! collation), the joined row and LEFT / ANTI's unmatched one, and the named Shape an inner side that may act gets
//! (D1236). Owner M11-MERGE. Budget 160 lines.
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Type, Value};
use gravity_program::Until;
use mcx::{CollectIn, GrowIn, Mcx, PgVec};
use types_nodes::list::NodeList;
use types_nodes::primnodes::FuncExpr;
use types_nodes::Node;

use crate::cell::{At, Cell, CellHome, CellLife, Nullable};
use crate::frame::env::Env;
use crate::frame::frame::Next;
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::graph::Gx;
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::ops::Table;

/// D1236's quiet pipeline reading is the effects home's (effects/visible/merge.rs); fed_first.rs names it here.
pub(crate) use crate::effects::visible::merge::pipe_quiet;

/// The road's row: merge.join's run reader (runs/slot_run.rs over face/runs.rs `SlotRunMech`) reads the inner by position,
/// its mark and restore two words; the step finds this row to say the road is built.
pub(crate) struct Markable;
crate::strategies! {
    /// Markable run cursor (mark / restore = position).
    MERGE_MARKABLE = 113, "merge.markable", Merge;
}

pub(crate) const ROWS: Table = Table { strategies: &[(MERGE_MARKABLE, &Markable)], ..crate::ops::NONE };

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

/// Per merge clause `outer op inner`: its outer argument, its inner argument and `cmp_proc(outer, inner)` returning int4,
/// the call's nodes in a context the program owns (`Px::program_cx`).
pub(crate) fn compares<'g, 'p>(px: &mut Px<'g, 'p>, node: i32, clauses: &'p NodeList<'p>, families: &[u32], collations: &[u32]) -> PgVec<'g, (Node<'p>, Node<'p>, Node<'p>)> {
    if families.len() < clauses.len() || collations.len() < clauses.len() {
        shape(node, "merge families shorter than the clauses");
    }
    let mcx = px.mcx();
    let m = px.program_cx("merge.join compares");
    let mut out: PgVec<'g, (Node<'p>, Node<'p>, Node<'p>)> = mcx::reserved_in(clauses.len(), mcx);
    for (i, c) in clauses.iter().enumerate() {
        let Some(op) = c.as_op_expr().filter(|o| o.args.len() == 2) else { shape(node, "a merge clause that is not a binary operator") };
        let (o, n) = (op.args.nth(0), op.args.nth(1));
        let Ok((lt, rt)) = px.gx.catalog.op_input_types(op.opno) else { shape(node, "a merge clause operator the catalog does not know") };
        let cmp = px.gx.catalog.btree_cmp_proc(families[i], lt, rt).ok().filter(|&f| f != 0).unwrap_or_else(|| shape(node, "a merge clause whose btree family has no compare for its types"));
        let call = NodeList::make1(m, o).and_then(|mut l| l.lappend(m, n).map(|_| l)).and_then(|args| Node::mk(m, FuncExpr { funcid: cmp, funcresulttype: 23, inputcollid: collations[i], args, ..Default::default() }));
        let Ok(call) = call else { shape(node, "a merge compare out of memory") };
        out.push_in((o, n, call));
    }
    out
}

/// The outer row joined to inner cells `(cells, vals, cols)`: the outer's cells then the inner's, each inner cell's life at
/// most the row, and the map of the inner's columns into it (`env.inner`). The row resumes at the outer row.
pub(crate) fn join_rows<'r, 'm>(outer: &RowVals<'r>, inner: (&[Cell], &[(Value, Option<Value>)], &[Option<u16>]), carry: &Carry, mcx: Mcx<'m>) -> (RowVals<'r>, PgVec<'m, Option<u16>>) {
    let base = outer.row.cells.len();
    let mut r = RowVals { row: outer.row.clone(), vals: outer.vals.clone(), cols: outer.cols.clone(), carry: carry.copy_in(*outer.vals.allocator()), next: outer.at, at: outer.at };
    for (c, &v) in inner.0.iter().zip(inner.1.iter()) {
        let mut c = *c;
        if matches!(c.life.until, Until::Window | Until::Part | Until::Life) {
            c.life.until = Until::Row;
        }
        r.row.cells.push_in(c);
        r.vals.push_in(v);
    }
    let map = inner.2.iter().map(|c| c.map(|k| (base + k as usize) as u16)).collect_in(mcx);
    (r, map)
}

/// The outer row handed on unmatched, every inner cell NULL in the shape the first joined row carried (`cells`: its cells,
/// value types and column map): LEFT's, and ANTI's too (D3905: ExecInitMergeJoin makes mj_NullInnerTupleSlot for JOIN_ANTI
/// as for JOIN_LEFT, so a target list naming an inner column reads NULL). An ANTI with no joined row hands it on as it came.
pub(crate) fn fill_row<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, c: &Carry, next: Next<'_, 'e>, anti: bool, (node, cells): (i32, Option<&InnerShape<'_>>)) -> Carry<'e> {
    env.memo.clear();
    let Some((cells, types, cols)) = cells else {
        if anti {
            return next(b, env, &RowVals { row: row.row.clone(), vals: row.vals.clone(), cols: row.cols.clone(), carry: c.copy_in(env.mcx), next: row.at, at: row.at });
        }
        shape(node, "a LEFT merge join whose inner hands on no row (its NULL-extended row's shape)")
    };
    let (cells, vals) = null_cells(b, env, &cells[..], &types[..]);
    let (r2, map) = join_rows(row, (&cells[..], &vals[..], &cols[..]), c, env.mcx);
    let saved = env.inner.replace(map);
    let c2 = next(b, env, &r2);
    env.inner = saved;
    c2
}

/// Cells `cells` (value types `types`) all NULL, each a value the row owns: an unmatched side's (LEFT's inner, RIGHT's outer).
pub(crate) fn null_cells<'e>(b: &mut FunctionBuilder<'_>, env: &Env<'e, '_>, cells: &[Cell], types: &[Type]) -> (PgVec<'e, Cell>, PgVec<'e, (Value, Option<Value>)>) {
    let t = b.bconst(true);
    let vals: PgVec<'e, (Value, Option<Value>)> = cells.iter().zip(types.iter()).map(|(c, &ty)| match c.at {
        At::Lane(_) => (env.zero, Some(t)),
        _ => (b.const_raw(ty, 0), Some(t)),
    }).collect_in(env.mcx);
    let cells: PgVec<'e, Cell> = cells.iter().map(|&c| Cell { life: CellLife { until: Until::Row, home: CellHome::FrameSlot }, at: match c.at { At::Lane(l) => At::Value(l.attno), a => a }, null: Nullable::Maybe, ..c }).collect_in(env.mcx);
    (cells, vals)
}

/// The inner's cells as the first joined row carried them: cells, value types, column map.
pub(crate) type InnerShape<'g> = (PgVec<'g, Cell>, PgVec<'g, Type>, PgVec<'g, Option<u16>>);
