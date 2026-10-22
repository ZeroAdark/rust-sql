//! nest.subplan's sites (plan C, V4R-499): where a SubPlan step's words are written for its readers, and over which row.
//! `Eager`: the step evaluates it for its row. `Row` (B2c): the step lends its row and the reader evaluates it where PG's
//! interpreter reaches the node. `Reader`: a join qual's SubPlan, evaluated where the join reader reaches it, over the
//! row that reader hands its qual (the candidate pair: its inner columns in `Env::inner`), as PG evaluates `joinqual` per
//! pair (ExecQual stops at the first false conjunct; SEMI / ANTI and a unique inner stop at the first passing pair).
//! `Source`: a VALUES row's SubPlan, read by the source before any step lowers: the step's prologue lends it for the whole
//! function (`Early`), and the source's row evaluates it where ValuesNext reaches its cell, once per row.
//! Refused by name: a join-qual SubPlan read past its join. Owner subplan.
use core::cell::Cell;

use gravity_decompose::ops::shared::subplan::{SubPlanDesc, SubSite};
use gravity_decompose::{Op, PipelineNode};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{IntBinOp, IntCond, Value};
use mcx::{CollectIn, GrowIn, PgVec};

use crate::admit::refusal::{Reason, ShapeWhy};
use crate::frame::env::Env;
use crate::frame::frame::Next;
use crate::frame::row::{Carry, RowVals};
use crate::ops::subplan::per_row::{references, uses, Eval};
use crate::ops::subplan::raw;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lend {
    Eager,
    Row,
    Reader,
    Source,
}

/// What a lazy step lends its readers: its body, and its row unless it is evaluated over the reader's (`Reader`, `Source`).
#[derive(Clone, Copy)]
pub(crate) struct Lent<'a> {
    e: &'a dyn Eval,
    row: Option<&'a RowVals<'a>>,
    source: bool,
}

/// A SubPlan step's body (per_row.rs's `Sub`, hashed.rs's probes): its words `(plan_id, value, flags)` pushed for the ops
/// after it, written here (`Eager`) or lent to the readers (`Env::subplans`' `lent`).
pub(crate) fn lend<'e>(e: &dyn Eval, mode: Lend, (id, v, f): (i32, i32, i32), b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
    if mode == Lend::Source {
        return next(b, env, row); // lent by `early` since the prologue
    }
    let lent = Some(Lent { e, row: (mode == Lend::Row).then_some(row), source: false });
    let at = if mode == Lend::Eager { unless_again(e, b, env, row, false); 0 } else { &lent as *const Option<Lent<'_>> as usize };
    env.subplans.push_in((id, v, f, at));
    // D652-S1 (modifytable): an eager step's body stays in reach of a write's EvalPlanQual redo in the IR (`redo`)
    let again = Some(Lent { e, row: None, source: false });
    let eager = mode == Lend::Eager;
    if eager {
        env.redo_subs.push_in((id, &again as *const Option<Lent<'_>> as usize));
    }
    let c = next(b, env, row);
    if eager {
        env.redo_subs.pop();
    }
    env.subplans.pop();
    c
}

/// D652-S1 (modifytable): eager SubPlan `id` evaluated again over `row`, its words written, as PostgreSQL's EvalPlanQual
/// re-evaluates the plan over the newest version of the write's row (rerun/epq/epq_ir.rs's redo); false when no eager
/// step of it is in scope (a lazy or hashed one: the caller refused it at make). W6-D1112: a host-fed step (a check
/// option's SubPlan, `Eval::fed`) has nothing to redo: the host reads it at its check, after the recheck, over the newest
/// version's check slot (ExecWithCheckOptions after EvalPlanQual), and any other read evaluates it there (`eval_at`).
pub(crate) fn redo(b: &mut FunctionBuilder<'_>, env: &mut Env, id: i32, row: &RowVals) -> bool {
    let Some(&(_, at)) = env.redo_subs.iter().rev().find(|s| s.0 == id) else {
        return env.subplans.iter().rev().find(|s| s.0 == id && s.3 != 0).and_then(|s| raw::lent_at(s.3)).is_some_and(|l| l.e.fed());
    };
    let Some(lent) = raw::lent_at(at) else { return false };
    let memo = env.memo.set_aside();
    lent.e.eval(b, env, row);
    env.memo.restore(memo);
    true
}

/// D3451 LAZYSUB: lazy SubPlan `id` (a `Row` step: conditional, or a hashed probe under OR / CASE) lent again over `row`, the
/// row a write's EvalPlanQual redo holds at the step (rerun/epq/epq_ir.rs `Redo::Sub`), while `rest` lowers the redo's
/// readers after it: each evaluates it where it reaches it over the newest version's row, as EvalPlanQual's ExecQual
/// does; the step's own lent row is the version the recheck replaced. A hashed probe probes the table this execution
/// built (or builds it at that read): PostgreSQL's recheck estate builds its own under the same snapshot and command id,
/// so over the same rows. None when no such step of `id` is in scope (a `Source` one, or none).
pub(crate) fn relend<R>(b: &mut FunctionBuilder<'_>, env: &mut Env, id: i32, row: &RowVals, rest: &mut dyn FnMut(&mut FunctionBuilder<'_>, &mut Env) -> R) -> Option<R> {
    let &(_, _, _, at) = env.subplans.iter().rev().find(|s| s.0 == id && s.3 != 0)?;
    let old = raw::lent_at(at)?;
    if old.source {
        return None;
    }
    let lent = Some(Lent { e: old.e, row: old.row.map(|_| row), source: false });
    // every entry of `id` points at it while `rest` lowers (the readers look an entry up from either end), then back
    let held: PgVec<'_, (usize, usize)> = env.subplans.iter().enumerate().filter(|(_, s)| s.0 == id && s.3 != 0).map(|(i, s)| (i, s.3)).collect_in(env.mcx);
    for &(i, _) in held.iter() {
        env.subplans[i].3 = &lent as *const Option<Lent<'_>> as usize;
    }
    let out = rest(b, env);
    for &(i, a) in held.iter() {
        env.subplans[i].3 = a;
    }
    Some(out)
}

/// `SubEval`: a lazy SubPlan evaluated at its read, in the reader's block, over the lent row or else the reader's.
pub(crate) fn eval_at(b: &mut FunctionBuilder<'_>, env: &mut Env<'_, '_>, at: usize, reader: &RowVals) {
    let Some(lent) = raw::lent_at(at) else { crate::refuse!(env.node, Reason::Bug { payload: "a Source SubPlan read before its step's prologue" }) };
    let row = lent.row.unwrap_or(reader);
    // W6-D1112: a host-fed SubPlan runs at every read (the host stored its arguments; PG runs it per WCO read, EPQ's too)
    if lent.e.fed() { lent.e.eval(b, env, row) } else { unless_again(lent.e, b, env, row, lent.source) }
}

/// D3374 `SubHolds`: whether the SubPlan lent at `at` is a row-lent one that may run where PG skips it (`Eval::holds`).
pub(crate) fn holds_at(at: usize) -> bool {
    raw::lent_at(at).is_some_and(|l| !l.source && !l.e.fed() && l.e.holds())
}

/// A `Source` step's entry, lent from its prologue for the whole function (the step outlives the function's lowering).
#[derive(Default)]
pub(crate) struct Early(Cell<Option<Lent<'static>>>);

/// A step's prologue: a `Source` step's words `(plan_id, value, flags)` lent before the source reads its first row
/// (frame.rs runs the steps' prologues before the source).
pub(crate) fn prologue(e: &dyn Eval, (mode, cell): (Lend, &Early), (id, v, f): (i32, i32, i32), env: &mut Env) {
    if mode != Lend::Source {
        return;
    }
    cell.0.set(Some(raw::erase(Lent { e, row: None, source: true })));
    env.subplans.push_in((id, v, f, cell.0.as_ptr() as usize));
}

/// `eval`, unless a later expanding step's stop re-enters the row (D688): PG evaluated it once there, so its words stand.
fn unless_again(e: &dyn Eval, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, all: bool) {
    let l_read = b.create_block(&[]);
    if let Some(again) = later_set(b, env, all) {
        let l_eval = b.create_block(&[]);
        b.cond_br(again, (l_read, &[]), (l_eval, &[]));
        b.switch_to(l_eval);
    }
    // D2362/D2363: the verdict's own branches (an empty set skips its left side) do not dominate the row's later reads
    let memo = env.memo.set_aside();
    e.eval(b, env, row);
    env.memo.restore(memo);
    b.br(l_read, &[]);
    b.switch_to(l_read);
}

/// D688: a later expanding step's offset word is set (a stop inside it re-enters this row; `all`: the source's row, any
/// step's); `None` without one.
fn later_set(b: &mut FunctionBuilder<'_>, env: &Env, all: bool) -> Option<Value> {
    let mut set = None;
    for &(_, o) in env.expands.iter().filter(|e| all || env.step.is_some_and(|j| e.0 > j)) {
        let w = env.load_word(b, o);
        set = Some(set.map_or(w, |a| b.int_bin(IntBinOp::Or, a, w)));
    }
    Some(b.icmp(IntCond::Ne, set?, env.zero))
}

/// Whether the SubPlan at op `j` is read where PG's reader reaches it (a join qual, a VALUES row), so D1911's `order`
/// does not apply.
pub(crate) fn ours(d: &SubPlanDesc<'_>) -> bool {
    matches!(d.site, SubSite::JoinQual | SubSite::Values)
}

/// Where the SubPlan at op `j` of `p` is evaluated (`fed`: a write's check option reads it, W6-D1112). A join qual's: the
/// op after its run of SubPlan steps is its join, and no op past that join reads it (its arguments are compiled at the
/// step, over the rows of the join's pair).
pub(crate) fn mode(p: &PipelineNode<'_>, j: usize, d: &SubPlanDesc<'_>, fed: bool, node: i32) -> Lend {
    match d.site {
        SubSite::JoinQual => {
            let join = (j + 1..p.ops.len()).find(|&i| !matches!(p.ops[i], Op::SubPlan(_)));
            if !join.is_some_and(|i| matches!(p.ops[i], Op::Join(_)) && references(p, i, d.plan_id) == 0) {
                crate::refuse!(node, Reason::Shape { why: ShapeWhy("a join-qual SubPlan read again past its join (nest.subplan C: evaluated over its join's pair)") });
            }
            Lend::Reader
        }
        SubSite::Values => Lend::Source,
        _ if !fed && gated(p, j, d.plan_id) => Lend::Source,
        _ => {
            let lazy = fed || d.conditional || (d.effects().acts && references(p, j, d.plan_id) > 1);
            if lazy { Lend::Row } else { Lend::Eager }
        }
    }
}

/// Whether only one-time gates read the SubPlan at op `j` (a Result's constant qual, no row column in it): it is lent from
/// the step's prologue (`Source`) and evaluated in the gate's, before any row, once per evaluation of the qual, as PG's
/// Result evaluates `resconstantqual` before it pulls its outer.
fn gated(p: &PipelineNode<'_>, j: usize, plan_id: i32) -> bool {
    use gravity_decompose::ops::shared::subplan::refers_list;
    let mut gates = 0;
    let rest = p.ops.iter().skip(j + 1).all(|op| match op {
        Op::Gate(q) => {
            gates += uses(*q, plan_id);
            true
        }
        Op::Window(w) => !refers_list(w.tlist, plan_id),
        Op::Srf(x) => !refers_list(x.tlist, plan_id),
        _ => true,
    });
    // `references` counts the filters', projections', join quals' and the write's reads too: none of those
    rest && gates > 0 && references(p, j, plan_id) == gates
}
