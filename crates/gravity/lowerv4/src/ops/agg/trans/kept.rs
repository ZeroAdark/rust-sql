//! A plain fold's kept columns (D345, V4R-434). Hooks / levers: -. Budget 160 lines. Owner M1-FOLD (M6-AGG-RUNS wrote
//! it for D345, main 09-28).
//!
//! A GroupAggregate whose every key the planner dropped as redundant (`WHERE k = 'x' GROUP BY k`: decompose's
//! `empty_no_row`) outputs its pinned columns from the group's first input row, PG's representative tuple. Decompose keys
//! the ones whose every row carries one image (`pinned_word`: runs.group); the rest (a record, a text under a
//! nondeterministic collation, a numeric's dscale: equal is not one image, so a key would split the group) stay a Plain
//! group with `pinned` set. The plain end keeps each of them in an `AggState` slot as the hosted bracket's kept column
//! (hosts v4/m6_agg_kept.rs: aggno `naggs + j`, the first call's value kept, NULL too; exec's agg.arena collects them by
//! decompose's `pinned_cols` walk, so `j` is `pinned`'s order), and the drain reads them back as the row's columns.
//! Kept columns make the arena serial (the program runs width 1, exec v4/m6.rs), so the first row is the scan's first,
//! as PG's. A host-sorted DISTINCT or ORDER BY the plain end folds itself (D1210) keeps its state in the same arena; an
//! aggregate a kept group hands to another end (those ends keep no first row) refuses by name. Registers nothing;
//! plain.rs reads it.
use gravity_abi::breaker::{RecordLayout, SlotKind};
use gravity_decompose::ops::shared::agg::{GroupDecl, GroupForm};
use gravity_decompose::graph::Op;
use gravity_decompose::catalog::typefacts;
use gravity_abi::status::raise;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, HostFlags, IntCond, Type, Value};
use gravity_kernels::ops::agg::trans::hosted::{m6_agg_final, m6_agg_trans};
use gravity_program::rep::ExprOut;
use mcx::{CollectIn, GrowIn, Mcx, PgVec};
use types_nodes::primnodes::OUTER_VAR;

use crate::ops::agg::trans::args::{with_args, Arg};
use crate::cell::{Cell, Nullable, Row};
use crate::frame::env::Env;
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::admit::refusal::{Reason, ShapeWhy};

/// Kept column `j` of a group: its input column (the Agg's OUTER_VAR attno), aggno and `AggState` slot offset.
#[derive(Clone, Copy)]
pub(crate) struct Kept {
    pub col: i16,
    pub aggno: i32,
    pub off: i32,
}

/// D345: the pinned columns plain group `g` keeps from its first row (none unless every key was dropped and decompose
/// keyed none of them).
pub(crate) fn cols<'g>(g: &'g GroupDecl<'_>) -> &'g [i16] {
    if g.form == GroupForm::Plain && g.empty_no_row { &g.pinned } else { &[] }
}

/// The `AggState` slots `fold/hosted.rs hosted_slots` appends for `g`'s kept columns, before the hosted states.
pub(crate) fn count(g: &GroupDecl<'_>) -> usize {
    cols(g).len()
}

/// Group `g`'s kept columns in layout `l` (the first `AggState` slots, `hosted_slots`): exec's arena numbers the Agg's
/// aggregates first (`aggrefs`, one per aggno), the kept columns after them. A host-sorted DISTINCT or ORDER BY the plain
/// end folds itself (D1210) is a hosted state of the same arena beside them; a kept group whose aggregate another end
/// folds refuses by name at the hand-off (`refuse_handoff`: that end keeps no first row).
pub(crate) fn of<'m>(g: &GroupDecl<'_>, l: &RecordLayout, _node: i32, mcx: Mcx<'m>) -> PgVec<'m, Kept> {
    let cs = cols(g);
    let naggs = g.aggs.iter().map(|a| a.aggno as i32 + 1).max().unwrap_or(0);
    let first = (0..l.nslots()).find(|&s| l.slot_kind(s) == SlotKind::AggState).unwrap_or(0);
    cs.iter().enumerate().map(|(j, &col)| Kept { col, aggno: naggs + j as i32, off: l.slot_off(first + j) as i32 }).collect_in(mcx)
}

/// A kept group handed to another end (a DISTINCT past the plain end's host-sorted fold, an ORDER BY beside a rider
/// that reads the row order: plain.rs's hand-off) refuses by name: that end keeps no first row.
pub(crate) fn refuse_handoff(g: &GroupDecl<'_>, node: i32) {
    if !cols(g).is_empty() {
        crate::refuse!(node, Reason::Shape { why: ShapeWhy("D345: a kept pinned column beside a DISTINCT or ordered aggregate another end folds (it keeps no first row)") });
    }
}

/// A kept group's first row is PostgreSQL's representative: its Agg's first input row. Where PostgreSQL presorted an
/// aggregate's input (an Aggref `aggpresorted`: a Sort under the Agg), that row is the Sort's first, so the plain end must
/// read that Sort's drain in its order (`regroup::fed_in_order`, D1596's `fed`); a group that does not refuses by name.
/// No presorted aggregate: the Agg reads its input as it comes, and the kept arena (serial) reads the same first row.
pub(crate) fn refuse_unfed(g: &GroupDecl<'_>, fed: bool, node: i32) {
    if !cols(g).is_empty() && gravity_decompose::facts::order::presorted(g) && !fed {
        crate::refuse!(node, Reason::Shape { why: ShapeWhy("D345: a kept pinned column beside a presorted aggregate whose Sort's first row the end does not read") });
    }
}

/// One row: each kept column's datum into the host, which keeps the group's first (hosts `keep_first`).
pub(crate) fn step(b: &mut FunctionBuilder<'_>, env: &mut Env, node: i32, row: &RowVals, rec: Value, kept: &[Kept]) {
    let Some(word) = env.agg_arena.filter(|_| !kept.is_empty()) else { return };
    for k in kept {
        with_args(b, env, node, row, &[Arg::Col(k.col)], |b, env, args| {
            let c = args[0];
            let tf = typefacts::row_of(c.cell.typid).copied().unwrap_or(typefacts::opaque(c.cell.typid, c.cell.typid, -1, false, false));
            let mut cx = env.lx(b, node);
            let v = crate::rep::datum(&mut cx, c, &tf);
            env.memo = cx.memo;
            let p = b.ptr_add(rec, env.zero, 1, k.off);
            trans(b, env, word, k.aggno, p, v);
        });
    }
}

/// The drain's kept columns: each with its type, read off the Var the drain's projection or HAVING reads it by.
pub(crate) fn drain_of<'g>(px: &Px<'g, '_>, k: usize, g: &GroupDecl<'_>, l: &RecordLayout, node: i32) -> PgVec<'g, (Kept, ExprOut)> {
    let gx = px.gx;
    let mut out = PgVec::new_in(px.mcx());
    for kc in of(g, l, node, px.mcx()) {
        let mut var = None;
        for op in gx.pipelines()[k].ops.iter() {
            let roots: PgVec<'_, types_nodes::Node<'_>> = match op {
                Op::Project(list) => list.iter().collect_in(px.mcx()),
                Op::Filter(q) | Op::Gate(q) => mcx::pg_vec![in px.mcx(); *q],
                _ => PgVec::new_in(px.mcx()),
            };
            for r in roots {
                gravity_decompose::expr::walk(r, &mut |x| {
                    if x.as_var().is_some_and(|v| v.varno == OUTER_VAR && v.varlevelsup == 0 && v.varattno == kc.col) {
                        var = var.or(Some(x));
                    }
                    x.as_aggref().is_none()
                });
            }
        }
        let Some(v) = var else { crate::refuse!(node, Reason::Bug { payload: "a kept column the drain does not read (D345)" }) };
        out.push_in((kc, crate::step::project::out_of(gx, v, node)));
    }
    out
}

/// The drain's one row: each kept column's final (the host's first-row value, NULL over no row) as its column.
pub(crate) fn drain_row<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, rec: Value, kept: &[(Kept, ExprOut)], (carry, next, at): (Carry<'e>, Value, Value)) -> RowVals<'e> {
    let ncols = kept.iter().map(|(k, _)| k.col as usize).max().unwrap_or(0);
    let mcx = env.mcx;
    let mut row = RowVals { row: Row::new_in(mcx), vals: PgVec::new_in(mcx), cols: mcx::pg_vec![in mcx; None; ncols], carry, next, at };
    let Some(word) = env.agg_arena.filter(|_| !kept.is_empty()) else { return row };
    for &(k, out) in kept {
        let p = b.ptr_add(rec, env.zero, 1, k.off);
        let (v, null) = fin(b, env, word, k.aggno, p);
        row.cols[k.col as usize - 1] = Some(row.row.cells.len() as u16);
        row.row.cells.push_in(Cell::of_expr(out, 0, 0, Nullable::Maybe));
        row.vals.push_in((v, Some(null)));
    }
    row
}

/// Kept column `aggno`'s transition over its state at `p`: face/aggcall.rs `step`'s call with one input (an M6 face this
/// M1 file cannot name, registry/mod.rs); the host keeps the first (hosts `keep_first`).
fn trans(b: &mut FunctionBuilder<'_>, env: &mut Env, word: u16, aggno: i32, p: Value, (v, null): (Value, Value)) {
    let (vs, ns) = (b.declare_stack_slot(8, 8), b.declare_stack_slot(1, 8));
    let (vs, ns) = (b.stack_addr(vs), b.stack_addr(ns));
    b.store(vs, 0, v);
    let nb = b.conv(ConvOp::Zext, Type::I8, null);
    b.store(ns, 0, nb);
    let (h, a) = (b.load(Type::I64, env.params, 8 * word as i32), b.iconst(Type::I64, aggno as i64));
    let flags = HostFlags { may_error: true, touches_memory: true, ..HostFlags::default() };
    let f = b.declare_host_once("m6_agg_trans", &[Type::Ptr, Type::I64, Type::I64, Type::Ptr, Type::Ptr, Type::Ptr], Some(Type::I64), m6_agg_trans as *const () as usize, flags);
    let st = b.call_host(f, &[env.ctx, h, a, p, vs, ns], Some(Type::I64)).expect("m6_agg_trans returns its status");
    raise_on(b, env, st);
}

/// Kept column `aggno`'s value (datum, NULL flag): face/aggcall.rs `finish`'s call (hosts `kept_final`).
fn fin(b: &mut FunctionBuilder<'_>, env: &mut Env, word: u16, aggno: i32, p: Value) -> (Value, Value) {
    let out = b.declare_stack_slot(16, 8);
    let out = b.stack_addr(out);
    let (h, a) = (b.load(Type::I64, env.params, 8 * word as i32), b.iconst(Type::I64, aggno as i64));
    let flags = HostFlags { may_error: true, touches_memory: true, ..HostFlags::default() };
    let f = b.declare_host_once("m6_agg_final", &[Type::Ptr, Type::I64, Type::I64, Type::Ptr, Type::Ptr], Some(Type::I64), m6_agg_final as *const () as usize, flags);
    let st = b.call_host(f, &[env.ctx, h, a, p, out], Some(Type::I64)).expect("m6_agg_final returns its status");
    raise_on(b, env, st);
    let (v, nb) = (b.load(Type::I64, out, 0), b.load(Type::I64, out, 8));
    (v, b.icmp(IntCond::Ne, nb, env.zero))
}

/// A failed call (its error stashed) leaves through `HOST`.
fn raise_on(b: &mut FunctionBuilder<'_>, env: &mut Env, st: Value) {
    let (bad, ok, e) = (b.icmp(IntCond::Ne, st, env.zero), b.create_block(&[]), b.iconst(Type::I64, raise::bit(raise::HOST) as i64));
    b.cond_br(bad, (env.l_raise, &[e]), (ok, &[]));
    b.switch_to(ok);
}
