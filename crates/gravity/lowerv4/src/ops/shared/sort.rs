//! A bounded incremental sort's stop as the families above or beside a top read it (runs/topk_rows.rs `Stop`).
use gravity_decompose::catalog::expr_type::{result_type, type_of};
use gravity_decompose::graph::Bound;
use gravity_decompose::ops::shared::sort::SortForm;
use gravity_decompose::catalog::typefacts::facts_of;
use gravity_decompose::Op;
use gravity_program::{Pipeline, Rep};

use crate::graph::Gx;

/// A pipeline the driver claims in sections (a full sort's `finishing` rule, decompose sort.rs `map`).
pub(crate) fn claim_driven(d: &Pipeline) -> bool {
    d.is_eligible() && d.driving_site >= 0
}

/// Whether the top in pipeline `k` stops at PostgreSQL's last row (V4R-179: nodeIncrementalSort.c reads up to the first
/// prefix change at or after its bound's row, and that row): a serial counted top over a presorted prefix with no WITH
/// TIES, any number of prefix keys, each a column (not a constant) of a by-value type no comparator orders
/// (rep/keylane.rs `hosted`: no tail, its word equal exactly when the values are equal) in its type's default family. A
/// test over the graph alone: where it holds, runs/topk_rows.rs's run shape gives every prefix key no tail and takes the
/// exact stop (its make_end refuses as a bug where a pull-form merge counts on it and it does not).
pub(crate) fn exact_stop(gx: &Gx<'_, '_>, k: usize) -> bool {
    let p = &gx.pipelines()[k];
    let Some(d) = p.ops.iter().find_map(|op| if let Op::Sort(d) = op { Some(d) } else { None }) else { return false };
    let pre = d.presorted as usize;
    let counted = matches!(d.form, SortForm::TopK { count, ties: false, .. } if !matches!(count, Bound::All));
    counted && !claim_driven(&p.desc) && pre > 0 && pre <= d.keys.len() &&d.keys[..pre].iter().all(|key| {
        let te = d.tlist.iter().filter_map(|n| n.as_target_entry()).find(|te| te.resno == key.resno).filter(|te| te.expr.as_const().is_none());
        let tf = te.and_then(|te| facts_of(gx.facts, type_of(te.expr).unwrap_or_else(|| result_type(te.expr))));
        !key.own_family && key.no_proc.is_none() && tf.is_some_and(|tf| tf.byval && !crate::rep::keylane::hosted(&tf))
    })
}

/// D3918: whether full sort `k`'s row holds a cell a spill pair pass (`P'`) hands on in another form: a pgrc packed lane's
/// narrow numeric (face/runs.rs `packed`: keyed by its exact word, no var tail) or a dictionary code (its order words come
/// off the code). `P'` hands each kept cell on as its datum, so such a sort takes no pair (hashjoin spill/build.rs `end_takes_pair`)
/// and its build keeps named growth, as before the pair.
pub(crate) fn unlike_pair(gx: &Gx<'_, '_>, k: usize) -> bool {
    let p = &gx.pipelines()[k];
    let Some(j) = p.ops.iter().position(|op| matches!(op, Op::Sort(_))) else { return false };
    let Some(l) = p.ops[..j].iter().rev().find_map(|op| if let Op::Project(l) = op { Some(*l) } else { None }) else { return false };
    let unlike = |r: Rep| matches!(r, Rep::Code { .. }) || matches!(r, Rep::PackedNum { facts, .. } if facts.narrow());
    l.iter().any(|n| n.as_target_entry().and_then(|te| gx.out_of(te.expr)).is_some_and(|o| unlike(o.rep)))
}
