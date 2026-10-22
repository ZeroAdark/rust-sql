//! The new tuple's cells of kind `write.update` (write/update.rs; cards/M4.md L4). Owner M4-UPDATE. A helper file (split
//! from write/update.rs): it registers nothing.
//!
//! One target attribute's cell per attribute in attno order: a SET column's cell is the projection's entry for it (the
//! plan's `updateColnos` order, resnos 1..n), every other attribute's is the scan's own cell carried in the child's
//! projection (`WriteDesc::old_cols`), a dropped attribute's is NULL. An UPDATE of several result relations carries the SET
//! cells alone (the host forms the target's new row over its old version). `new_tuple` lowers the cells to the write
//! site's `(datum, valid)` pairs.
use gravity_decompose::catalog::typefacts::{self, TypeFacts};
use gravity_decompose::ops::shared::modifytable::WriteDesc;
use gravity_decompose::Source;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, Value};
use gravity_program::WritePlan;
use mcx::{CollectIn, GrowIn, PgVec};

use crate::frame::env::Env;
use crate::frame::row::RowVals;
use crate::graph::Gx;
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::rep;

/// Where one target attribute's cell comes from, in the row the end sees (the child's projection): the SET entry or the
/// carried old cell (a resno each, with the attribute's type facts), or NULL (a dropped attribute).
pub(crate) enum Src {
    Col(i16, TypeFacts),
    Null,
}

/// Each target attribute's cell, attno order. The SET entries are the projection's resnos 1..n in `set` order; the old
/// cells are `old_cols`. A write whose child is a Result under a constant-false filter carries none (its row never comes):
/// its cells read nothing.
pub(crate) fn cells_of<'g>(gx: &Gx<'g, '_>, k: usize, w: &WriteDesc<'_>, relid: u32) -> PgVec<'g, Src> {
    let node = gx.pipelines()[k].node_id;
    let natts = gx.facts.rel(relid).map_or(0, |r| r.natts.max(0));
    let dummy = matches!(gx.pipelines()[k].source, Source::Const) && w.old_cols.is_empty();
    let mut out = mcx::reserved_in(natts as usize, gx.mcx);
    for a in 1..=natts {
        let Some(att) = gx.facts.att(relid, a) else {
            out.push_in(Src::Null);
            continue;
        };
        let resno = match w.set.and_then(|s| s.iter().position(|x| x as i16 == a)) {
            Some(i) => i as i16 + 1,
            None if dummy => {
                out.push_in(Src::Null);
                continue;
            }
            None => match w.old_cols.iter().find(|c| c.0 == a) {
                Some(c) => c.1,
                None => crate::refuse!(node, Reason::Shape { why: ShapeWhy("UPDATE: a target column the child does not carry (no old row to form the new tuple from)") }),
            },
        };
        let Some(tf) = typefacts::facts_of(gx.facts, att.typid) else {
            crate::refuse!(node, Reason::Shape { why: ShapeWhy("UPDATE: a target column type the catalog does not know") })
        };
        out.push_in(Src::Col(resno, tf));
    }
    out
}

/// An UPDATE of several result relations: the site's cells are the SET entries alone, resnos 1..n in `updateColnos` order,
/// each with the type facts of the arm's own target's attribute (arm `rti`; a partition's own attnos). Every target's list,
/// through its map to the root (`Target::to_root`; an inheritance child without one is compared as numbered), must be one
/// strictly ascending list: the host places cell `i` at the target attribute named as the root's `i`-th updated column.
/// A Result under a constant-false filter (its row never comes) reads nothing.
pub(crate) fn set_cells<'g>(gx: &Gx<'g, '_>, k: usize, w: &WriteDesc<'_>, rti: u32) -> PgVec<'g, Src> {
    let node = gx.pipelines()[k].node_id;
    let root = |t: &gravity_decompose::ops::shared::modifytable::Target<'_>| {
        t.set.map_or_else(|| mcx::PgVec::new_in(gx.mcx), |s| s.iter().map(|a| gravity_decompose::ops::modifytable::partwrite::root_attno(t, a as i16)).collect_in(gx.mcx))
    };
    let first = root(&w.targets[0]);
    // D3681: every target's columns are compared once per write (memo.rs `set_roots_ok`), not again for each of its arms
    let (key, at) = (crate::memo::key_of(&gx.graph.pipelines), w.targets.as_ptr() as usize);
    if crate::memo::read(key, |m| m.set_roots_ok.contains(&at)) != Some(true) {
        if first.iter().any(|&a| a <= 0) || first.windows(2).any(|p| p[0] >= p[1]) || w.targets.iter().skip(1).any(|t| root(t) != first) {
            crate::refuse!(node, Reason::Shape { why: ShapeWhy("UPDATE of several result relations: a target whose SET columns are not the root's, in the root's order (the host maps each SET cell by the root's attno)") });
        }
        crate::memo::write(key, |m| m.set_roots_ok.push(at)); // alloc: Program
    }
    let Some(t) = w.targets.iter().find(|t| t.rti == rti) else { crate::refuse!(node, Reason::Bug { payload: "an UPDATE arm whose target is not among the write's result relations" }) };
    if matches!(gx.pipelines()[k].source, Source::Const) {
        return first.iter().map(|_| Src::Null).collect_in(gx.mcx);
    }
    let mut out = mcx::reserved_in(first.len(), gx.mcx);
    for (i, a) in t.set.iter().flat_map(|s| s.iter()).enumerate() {
        let Some(att) = gx.facts.att(t.relid, a as i16) else {
            crate::refuse!(node, Reason::Shape { why: ShapeWhy("UPDATE: a SET column the catalog does not know") })
        };
        let Some(tf) = typefacts::facts_of(gx.facts, att.typid) else {
            crate::refuse!(node, Reason::Shape { why: ShapeWhy("UPDATE: a target column type the catalog does not know") })
        };
        out.push_in(Src::Col(i as i16 + 1, tf));
    }
    out
}

/// The new tuple's `(datum, valid)` per attribute: the cell's datum through its Rep's `datum` accessor (a lane cell's
/// by-reference datum is the page's own bytes), NULL for a dropped attribute.
pub(crate) fn new_tuple<'e>(srcs: &[Src], node: i32, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals) -> PgVec<'e, (Value, Value)> {
    let (zero, t) = (env.zero, b.bconst(true));
    let f = b.bconst(false);
    let mut cx = env.lx(b, node);
    let mut v = mcx::reserved_in(srcs.len(), env.mcx);
    for c in srcs.iter() {
        match c {
            Src::Null => v.push_in((zero, f)),
            Src::Col(resno, tf) => {
                let Some(cr) = row.col(*resno) else { crate::refuse!(node, Reason::Bug { payload: "an UPDATE cell past the child's projection" }) };
                let (d, null) = rep::datum(&mut cx, cr, tf);
                let valid = cx.b.int_bin(IntBinOp::Xor, null, t);
                v.push_in((d, valid));
            }
        }
    }
    env.memo = cx.memo;
    v
}

/// R5 (D764): UPDATE ... RETURNING over several result relations: the host hands each target's new row back through the
/// site every arm shares (hosts write_route.rs `MultiTargetHost::written`), as wide as the widest target: the SET cells,
/// then NULLs; its system columns through its target's host (D774's sys site). V4R-349: a moved row is the move host's,
/// in the arm's layout, its system columns (tableoid the destination's, never the arm's constant) the destination's.
/// V4R-350: RETURNING OLD reads each target's replaced version (a moved row's: the version its move deleted), as wide.
/// D3379 UNION-OLD: the write of pipeline `k` (ret_sys.rs `sys_old_plan_at`).
pub(crate) fn ret_width(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>, plan: WritePlan, cells: &mut PgVec<'_, Src>) -> WritePlan {
    if w.returning.is_none() {
        return plan;
    }
    let plan = if w.old_new.old { plan.with_old() } else { plan };
    let plan = if crate::ops::modifytable::ret_sys::sys_old_plan_at(gx, k, w) { plan.with_sys_old() } else { plan }; // V4R-501
    let width = w.targets.iter().map(|t| t.site.natts()).max().unwrap_or(0);
    cells.extend_in((cells.len()..width).map(|_| Src::Null));
    match crate::ops::modifytable::ret_sys::sys_plan(w) {
        Some(header) => plan.with_sys(header),
        None => plan,
    }
}
