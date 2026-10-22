//! RETURNING's system columns in kind `write.returning` (write/returning.rs; cards/M4.md L5). Owner M4-RET. A helper file
//! (split from write/ret_cols.rs, M4-CLOSE B3b: V4R-338 took it past its gate): it registers nothing.
//!
//! D774: a heap target's `tableoid` as a constant (`tableoid`, face/end.rs `RetTableoid`); the other system columns handed
//! back by the host after the row op (`sys_plan`: WritePlan.sys, hosts write_sys.rs); OLD / NEW of the written row as a
//! default reference's, of an image the command never has NULL (`image`). R5 (D764): an arm of several result relations
//! reads its own target's.
use gravity_decompose::ops::shared::modifytable::{Target, WriteDesc};
use gravity_decompose::Op;
use gravity_program::{Cmd, WritePlan};
use types_nodes::primnodes::VarReturningType;

use crate::ops::modifytable::ret_cols::walk;
use crate::graph::Gx;

/// D774 (`RetTableoid`): the RETURNING projection at `(k, j)` over a write whose every row goes to its one heap target
/// (a plain table, a leaf a partitioned root was pruned to, an inheritance tree's one result relation): `tableoid` is
/// that target's OID, as `slot->tts_tableOid` after table_tuple_insert / update / the deleted row's fetch. Not an UPDATE
/// under a root that routes (`may_move`). R5 (D764): an arm of several result relations projects its own target's list,
/// whose rows are that target's, unless a row may move (V4R-349: the host hands back the destination's, `sys_plan`).
pub(crate) fn tableoid(gx: &Gx<'_, '_>, k: usize, j: usize) -> Option<(i32, types_core::Oid)> {
    use gravity_decompose::ops::shared::modifytable::TargetKind;
    let p = gx.pipelines().get(k)?;
    // D1112: op `j` the RETURNING projection, or a SubPlan step of its list before it
    let wi = crate::ops::modifytable::returning::ret_write(p, j)?;
    let Some(Op::Write(w)) = p.ops.get(wi) else { return None };
    let t = crate::ops::modifytable::returning::target_at(p, wi, w)?;
    // ROWC-UNION: under the union form a row's relation is its own (`union_form`), handed back by the host
    if union_form(gx, k, w) {
        return None;
    }
    match (w.targets.len(), t.kind) {
        (1, TargetKind::Heap) if !may_move(w) => Some((w.rti as i32, w.plan.target_rel)),
        (2.., TargetKind::Heap) if !may_move(w) => Some((t.rti as i32, t.relid)),
        _ => None,
    }
}

/// A RETURNING system column the written row carries: tableoid of a heap target (`tableoid`), else none yet.
pub(crate) fn sys_served(v: &types_nodes::primnodes::Var<'_>, w: &WriteDesc<'_>, t: &Target<'_>) -> bool {
    use gravity_decompose::ops::shared::modifytable::TargetKind;
    v.varattno == -6 && t.kind == TargetKind::Heap && !may_move(w)
}

/// An UPDATE (MERGE's too) of a leaf under a root that routes may move its row to another leaf (write/update.rs
/// `moving`, ExecCrossPartitionUpdate), whose tableoid is the destination's: the host hands it back (V4R-349: the move
/// host's inserted row, hosts write_move.rs `Mover::sys`), never the pruned leaf's or the arm's constant.
pub(crate) fn may_move(w: &WriteDesc<'_>) -> bool {
    matches!(w.cmd, Cmd::Update | Cmd::Merge) && w.route.is_some()
}

/// D774: the image a command never has (an INSERT's OLD without ON CONFLICT DO UPDATE, a DELETE's NEW): its whole row and
/// system columns are NULL (gravity_expr var.rs, `Returning::old_exists` / `new_exists`); `written`: a system column `v`
/// that is the written row's (a default reference, the NEW of an INSERT / UPDATE, the OLD of a DELETE, and the OLD tableoid
/// of an UPDATE whose row stays in its target: a row that may move is written by the move host into another leaf).
pub(crate) fn image(v: &types_nodes::primnodes::Var<'_>, w: &WriteDesc<'_>) -> (bool, bool) {
    let (old, new) = (v.varreturningtype == VarReturningType::VAR_RETURNING_OLD, v.varreturningtype == VarReturningType::VAR_RETURNING_NEW);
    match w.cmd {
        Cmd::Insert => (old && !w.on_conflict.as_ref().is_some_and(|c| c.update), !old),
        // a moved row's OLD tableoid is its source leaf's, not the destination's the host hands back (V4R-349)
        Cmd::Update => (false, !old || (v.varattno == -6 && !may_move(w))),
        Cmd::Delete => (new, !new),
        _ => (false, !(old || new)),
    }
}

/// D774: the writes whose host hands back the written row's system columns: an INSERT / UPDATE / DELETE of one target
/// (a routed INSERT's leaf included), R5 an UPDATE / DELETE arm of several (its target's host, hosts write_route.rs
/// `MultiTargetHost::sys`); V4R-501: ON CONFLICT's too (the inserted row, DO UPDATE's new version; the sys site after the
/// conflict and old sites, write/rowop_step.rs). writesubplan (gen mg-0178): a MERGE of several result relations through
/// a root (the multi-target host: an UPDATE / DELETE row's target host's, a NOT MATCHED INSERT's route leg's; merge/lower.rs
/// lays them, merge/ret.rs reads them). D3481: or a MERGE of one plain target, whose host keeps each action's row op's as
/// an INSERT's, UPDATE's or DELETE's (hosts write_sys.rs: the inserted tuple, the new version, the deleted version it
/// fetched); a MERGE through a root with one result relation keeps the Shape.
pub(crate) fn sys_site(w: &WriteDesc<'_>) -> bool {
    matches!(w.cmd, Cmd::Insert | Cmd::Update | Cmd::Delete) || w.cmd == Cmd::Merge && (w.route.is_some() && w.targets.len() > 1 || w.route.is_none() && w.targets.len() == 1)
}

/// V4R-501: OLD's system column `v` as the replaced version's (the old site's last six cells, hosts write_sys.rs
/// `SysCols::fill_old`): an UPDATE's (after an EvalPlanQual recheck the newer version it locked; R5 each arm's; a moved row's the
/// version its move deleted) or ON CONFLICT DO UPDATE's existing row (NULL for an inserted row). `Err` names the refusal:
/// xmin .. cmax of a row that may move (PostgreSQL converts a moved row's OLD through the root's virtual slot when a
/// leaf's layout differs, where they raise: D1686), MERGE's.
pub(crate) fn old_sys(v: &types_nodes::primnodes::Var<'_>, w: &WriteDesc<'_>) -> Result<(), &'static str> {
    match w.cmd {
        Cmd::Update if (-5..=-2).contains(&v.varattno) && may_move(w) => Err("RETURNING old: xmin .. cmax of a row that may move to another partition (D1686)"),
        Cmd::Update => Ok(()),
        Cmd::Insert if w.on_conflict.as_ref().is_some_and(|c| c.update) => Ok(()),
        _ => Err("RETURNING old / new: a system column of the replaced version (UPDATE's old ctid, xmin .. cmax), MERGE's or ON CONFLICT's"),
    }
}

/// V4R-501: the plan fetches the replaced version's system columns (`WritePlan::with_sys_old`) when the list reads OLD's
/// ctid .. cmax, or DO UPDATE's OLD tableoid (NULL for an inserted row); an UPDATE's OLD tableoid is the written row's or
/// R4c's constant (`image`), but under `union_old`'s form (`sys_old_plan_at`).
pub(crate) fn sys_old_plan(w: &WriteDesc<'_>) -> bool {
    sys_old_plan_with(w, false)
}

/// D3379 UNION-OLD (lockrows UNION-RET): `sys_old_plan` for the write of pipeline `k`. Under `union_old`'s form OLD's
/// tableoid is the replaced version's too: the row's own relation's, which R4c's constant (the list's target, the first)
/// is not for another relation's row.
pub(crate) fn sys_old_plan_at(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>) -> bool {
    sys_old_plan_with(w, union_old(gx, k, w))
}

/// D3379 UNION-OLD: an UPDATE of several result relations through a routing root under the union form (`union_form`):
/// every row reads the first target's list, so OLD's tableoid is the replaced version's (hosts write_sys.rs `fill_old`,
/// the row's target host's relation; a moved row's the source leaf, as PostgreSQL's ExecDelete keeps cpDeletedSlot's
/// tts_tableOid through the root's map), never R4c's constant of the first target (ret_cols.rs `images`).
pub(crate) fn union_old(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>) -> bool {
    w.cmd == Cmd::Update && may_move(w) && union_form(gx, k, w)
}

/// `sys_old_plan`; `union`: OLD's tableoid of an UPDATE that may move is the replaced version's (`union_old`).
fn sys_old_plan_with(w: &WriteDesc<'_>, union: bool) -> bool {
    let Some(list) = w.returning.filter(|_| matches!(w.cmd, Cmd::Update | Cmd::Insert)) else { return false };
    let rti = w.targets.first().filter(|_| w.targets.len() > 1).map_or(w.rti, |t| t.rti) as i32;
    let mut any = false;
    for n in list.iter() {
        let Some(te) = n.as_target_entry() else { continue };
        walk(te.expr, &mut |x| {
            let old = x.as_var().filter(|v| v.varreturningtype == VarReturningType::VAR_RETURNING_OLD && v.varattno < 0 && v.varlevelsup == 0 && v.varno == rti);
            any |= old.is_some_and(|v| old_sys(&v, w).is_ok() && (v.varattno != -6 || w.cmd == Cmd::Insert || union));
            true
        });
    }
    any
}

/// D774: the host must hand back the written row's system columns for this RETURNING list: `Some(header)` when it reads
/// one the plan cannot answer itself (every system column but a one-heap-target tableoid), `header` when xmin .. cmax.
pub(crate) fn sys_plan(w: &WriteDesc<'_>) -> Option<bool> {
    sys_plan_with(w, false)
}

/// D2734 ROWC-UNION (modifytable, gen mt-0354): a DELETE of several result relations whose rows come out of one settled union
/// of their scans (write/delete_built.rs `union_feeds`), not an arm of one target: a row's tableoid is its own relation's.
/// UNION-RET: or such an UPDATE under RETURNING (write/update_built.rs `ret_union`).
pub(crate) fn union_form(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>) -> bool {
    w.cmd == Cmd::Delete && w.targets.len() > 1 && crate::ops::modifytable::delete_built::union_feeds(gx, k, w).is_some() || crate::ops::modifytable::update_built::ret_union(gx, k, w)
}

/// ROWC-UNION: `sys_plan` for the write of pipeline `k`: under the union form (`union_form`) the host hands back a
/// tableoid too (the row's own relation's, hosts write_route.rs `MultiTargetHost::sys`), never the first target's constant.
pub(crate) fn sys_plan_at(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>) -> Option<bool> {
    sys_plan_with(w, union_form(gx, k, w))
}

/// `sys_plan`; `varies`: no tableoid is the plan's constant.
fn sys_plan_with(w: &WriteDesc<'_>, varies: bool) -> Option<bool> {
    let list = w.returning?;
    if !sys_site(w) {
        return None;
    }
    // the first target's list (under several result relations every arm's reads the same columns of its own target)
    let rti = w.targets.first().filter(|_| w.targets.len() > 1).map_or(w.rti, |t| t.rti) as i32;
    let (mut any, mut header) = (false, false);
    for n in list.iter() {
        let Some(te) = n.as_target_entry() else { continue };
        walk(te.expr, &mut |x| {
            // a default reference, or OLD / NEW of the written row (`image`)
            if let Some(v) = x.as_var().filter(|v| v.varattno < 0 && v.varlevelsup == 0 && v.varno == rti && image(v, w).1) {
                if varies || !sys_served(&v, w, &w.targets[0]) {
                    any = true;
                    header |= (-5..=-2).contains(&v.varattno);
                }
            }
            true
        });
    }
    any.then_some(header)
}

/// D774: an UPDATE's plan (write/update.rs) with the new version's system columns handed back by the host when RETURNING
/// reads them; V4R-349: a moved row's are the move host's insert through the root (ExecCrossPartitionUpdate's slot, hosts
/// write_move.rs `Mover::sys`); V4R-501: the replaced version's (`sys_old_plan`).
pub(crate) fn sys_update(w: &WriteDesc<'_>, plan: WritePlan) -> WritePlan {
    let plan = if sys_old_plan(w) { plan.with_sys_old() } else { plan };
    match sys_plan(w) {
        Some(header) => plan.with_sys(header),
        None => plan,
    }
}
