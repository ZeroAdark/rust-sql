//! MERGE's half of the write map: a target's WHEN clauses, the MERGE desc and the target columns it carries.
use gravity_program::{ArenaVec, Cmd};
use types_nodes::nodes_enums::CmdType;
use types_nodes::plannodes::{ModifyTable, Plan};
use types_nodes::primnodes::{VarReturningType, INNER_VAR, OUTER_VAR};
use types_nodes::Node;

use super::{Carried, MergeAction, MergeActions, MergeDesc, Member, RouteDesc, Target, TargetKind};
use crate::admit::Refusal;
pub(super) use crate::ops::shared::gate::never;
use crate::ledger::Kind;
use crate::walk::Builder;

/// One target's WHEN clauses off `mergeActionLists[i]` / `mergeJoinConditions[i]`.
pub(super) fn merge_actions<'p>(b: &Builder<'p, '_>, mt: &'p ModifyTable<'p>, i: usize, k: Kind, id: i32) -> Result<MergeActions<'p>, Refusal> {
    let list = if mt.mergeActionLists.len() > i { mt.mergeActionLists.nth(i).as_list() } else { None };
    let list = list.ok_or(Refusal::shape(k, id, "MERGE without its action list"))?;
    let join_cond = if mt.mergeJoinConditions.len() > i { mt.mergeJoinConditions.nth(i).as_list().filter(|l| !l.is_nil()) } else { None };
    let mut actions = ArenaVec::with_capacity_in(list.len(), b.arena);
    for a in list.iter() {
        let a = a.as_merge_action().ok_or(Refusal::shape(k, id, "a mergeActionLists entry that is not a MergeAction"))?;
        let cmd = match a.commandType {
            CmdType::CMD_INSERT => Some(Cmd::Insert),
            CmdType::CMD_UPDATE => Some(Cmd::Update),
            CmdType::CMD_DELETE => Some(Cmd::Delete),
            CmdType::CMD_NOTHING => None,
            _ => return Err(Refusal::shape(k, id, "an unknown MERGE action command")),
        };
        if a.qual.is_some_and(|q| q.as_list().is_none()) {
            return Err(Refusal::shape(k, id, "a MERGE action qual that is not a list"));
        }
        let quals = a.qual.and_then(|q| q.as_list()).filter(|l| !l.is_nil());
        let set = if cmd == Some(Cmd::Update) { Some(&a.updateColnos) } else { None };
        actions.push(MergeAction { kind: a.matchKind, cmd, quals, tlist: &a.targetList, set });
    }
    Ok(MergeActions { join_cond, actions })
}

/// The MERGE desc: `targets[0]`'s actions on the frozen face, and the carry noted for the child walk —
/// the target's attributes every target's actions, join condition and RETURNING read (each target's Vars
/// mapped to the routing root's attnos; every attribute when a row is formed — an UPDATE or RETURNING).
pub(super) fn merge_desc<'p>(b: &mut Builder<'p, '_>, mt: &'p ModifyTable<'p>, targets: &[Target<'p>], route: Option<RouteDesc>, returning_action: bool, c: &mut Carried, k: Kind, id: i32) -> Result<MergeDesc<'p>, Refusal> {
    if b.alloc.is_none() {
        return Err(Refusal::shape(k, id, "MERGE: no node allocator (decompose_in)"));
    }
    let (root_rti, root_relid) = route.map_or((targets[0].rti, targets[0].relid), |r| (r.root_rti, r.root_relid));
    let root = b.relation(root_rti, k, id)?;
    let mut atts = crate::ops::shared::scan::Atts::new(b.arena);
    let mut whole = false;
    for t in targets.iter() {
        let m = t.merge.as_ref().expect("a MERGE target's actions");
        whole |= t.returning.is_some();
        let mut refs: ArenaVec<Node<'p>> = ArenaVec::new_in(b.arena);
        refs.extend(m.join_cond.iter().flat_map(|l| l.iter()));
        for a in m.actions.iter() {
            whole |= a.cmd == Some(Cmd::Update);
            refs.extend(a.quals.iter().flat_map(|l| l.iter()));
            refs.extend(a.tlist.iter());
        }
        for &n in refs.iter() {
            whole |= target_refs(b, n, root_rti, t.rti, &mut atts, c, false, k, id)?;
        }
        // D3481: one plain target's RETURNING system columns (lowerv4 ret_sys.rs `sys_site`)
        let one = route.is_none() && targets.len() == 1;
        for n in t.returning.iter().flat_map(|l| l.iter()) {
            whole |= target_refs(b, n, root_rti, t.rti, &mut atts, c, one, k, id)?;
        }
    }
    if whole {
        for a in 1..=root.natts.max(0) {
            if b.facts.att(root.relid, a).is_some() && !atts.add(a) {
                return Err(Refusal::shape(k, id, "more than 4096 lanes (the sanity limit)"));
            }
        }
    }
    let mut attnos = ArenaVec::with_capacity_in(atts.as_slice().len(), b.arena);
    // a view target's old row is its wholerow image (ExecMergeMatched's oldtuple), no scan of the view carries columns
    if targets[0].kind != TargetKind::View {
        attnos.extend(atts.as_slice().iter().copied());
    }
    let rowid_from = mt.plan.lefttree.and_then(|c| c.as_plan()).and_then(rowid_from).unwrap_or(-1);
    b.carry = Some(crate::ops::shared::carry::Carry { update: false, rti: root_rti, relid: root_relid, attnos, by_node: ArenaVec::new_in(b.arena), rowid_from });
    let first = merge_actions(b, mt, 0, k, id)?;
    Ok(MergeDesc { join_cond: first.join_cond, actions: first.actions, rowid_col: 0, target_cols: ArenaVec::new_in(b.arena), returning_action })
}

/// What an expression reads of MERGE target `rti`, by attribute number (mapped to the routing root's):
/// a column is carried, the whole row carries every column (returns true), `ctid` is the rowid column,
/// `tableoid` the target's constant; the other system columns are not carried (sources.rs' lanes are the scan's).
/// D3481: in one plain target's RETURNING (`ret`) a plain system column reads the slot the action's row op left
/// (ExecProcessReturning: the new version, the deleted version, the inserted tuple), which write.returning lays, so it
/// carries nothing.
#[allow(clippy::too_many_arguments)]
fn target_refs(b: &Builder<'_, '_>, n: Node<'_>, root: u32, rti: u32, atts: &mut crate::ops::shared::scan::Atts, c: &mut Carried, ret: bool, k: Kind, id: i32) -> Result<bool, Refusal> {
    let (mut whole, mut err) = (false, None);
    crate::expr::walk(n, &mut |x| {
        if let Some(v) = x.as_var().filter(|v| v.varno == rti as i32 && v.varlevelsup == 0) {
            let r = match v.varattno {
                0 => {
                    whole = true;
                    c.add(Member::WholeRow);
                    Ok(())
                }
                crate::ops::shared::scan::SELF_ITEM_POINTER => Ok(()),
                -6 => Ok(()),
                a if a < 0 && ret && v.varreturningtype == VarReturningType::VAR_RETURNING_DEFAULT => Ok(()),
                a if a < 0 => Err(Refusal::shape(k, id, "a system column of the MERGE target (xmin / cmin / xmax / cmax: no system-column lane)")),
                a => match crate::ops::modifytable::carry::root_attno(b, root, rti, a) {
                    None => Err(Refusal::shape(k, id, "a MERGE target column the routing root does not have")),
                    Some(ra) if atts.add(ra) => Ok(()),
                    Some(_) => Err(Refusal::shape(k, id, "more than 4096 lanes (the sanity limit)")),
                },
            };
            err = r.err();
        }
        err.is_none()
    });
    err.map_or(Ok(whole), Err)
}

/// After the child: the target's ctid column and its carried attributes in the child's projection. A view target
/// (`view_row` > 0): its wholerow junk column is the row id, NULL = not matched (ExecModifyTable's view arm), and no
/// attribute is carried.
pub(super) fn merge_carried<'p>(b: &mut Builder<'p, '_>, plan: &'p Plan<'p>, mut m: MergeDesc<'p>, view_row: i16, k: Kind, id: i32) -> Result<MergeDesc<'p>, Refusal> {
    let carry = b.carry.take().expect("set by merge_desc");
    let child = plan.lefttree.and_then(|c| c.as_plan()).ok_or(Refusal::shape(k, id, "MERGE without a child"))?;
    if view_row > 0 {
        m.rowid_col = view_row;
        return Ok(m);
    }
    // the planner's dummy relation as the whole input (a join condition that folds to false or NULL: `ON false`, `ON t.id =
    // s.id` over a one-row source whose key is NULL): an input-less Result under a one-time gate that never passes. Its row
    // never comes, so it carries no target column, as `update_carried` takes an UPDATE's (lowerv4 shared/dummy.rs serves
    // the write as one that meets no row)
    let dummy = child.lefttree.is_none() && plan.lefttree.and_then(|c| c.as_result()).and_then(|r| r.resconstantqual).is_some_and(never);
    let rowid = child.targetlist.iter().filter_map(|e| e.as_target_entry()).find(|te| te.resjunk && te.resname == Some("ctid"));
    match rowid {
        Some(rowid) => m.rowid_col = rowid.resno,
        None if dummy => {}
        None => return Err(Refusal::shape(k, id, "MERGE without the target's ctid junk column")),
    }
    let resnos = carry.by_node.iter().find(|(n, _)| *n == child.plan_node_id).map(|(_, r)| r);
    match resnos {
        Some(r) if r.len() == carry.attnos.len() => {
            for (a, res) in carry.attnos.iter().zip(r.iter()) {
                m.target_cols.push((*a, *res));
            }
        }
        _ if carry.attnos.is_empty() || dummy => {}
        _ => return Err(Refusal::shape(k, id, "MERGE: the target's columns did not reach the write op")),
    }
    Ok(m)
}

/// D3367: the plan node whose own target list computes the MERGE child's junk ctid (the planner's row identity of the
/// target), followed down through each node that passes it on (a join's, a Hash's, a Sort's OUTER_VAR / INNER_VAR
/// reference into its child's target list). `None`: no ctid junk column, or a reference past a child's list.
fn rowid_from(child: &Plan<'_>) -> Option<i32> {
    let mut plan = child;
    let mut te = child.targetlist.iter().filter_map(|e| e.as_target_entry()).find(|te| te.resjunk && te.resname == Some("ctid"))?;
    loop {
        let next = match te.expr.as_var() {
            Some(v) if v.varno == OUTER_VAR => plan.lefttree,
            Some(v) if v.varno == INNER_VAR => plan.righttree,
            _ => return Some(plan.plan_node_id),
        };
        let at = usize::try_from(te.expr.as_var()?.varattno).ok()?.checked_sub(1)?;
        plan = next?.as_plan()?;
        te = (at < plan.targetlist.len()).then(|| plan.targetlist.nth(at).as_target_entry()).flatten()?;
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use types_nodes::list::NodeList;
    use types_nodes::plannodes::{Join, NestLoop, Result as ResultPlan};

    /// D3367: the junk ctid's producer is followed through a join's reference to the node whose own list computes it
    /// (the dummy Result inner of `source LEFT JOIN target ON false`), or is the child itself (the dummy as the whole
    /// input); none without the column. The dummy's gate never passes.
    #[test]
    fn rowid_producer() {
        let ctx = mcx::MemoryContext::new("merge-test");
        let m = ctx.mcx();
        let te = |e, resno, name, junk| Node::mk_target_entry(m, e, resno, name, junk).unwrap();
        let tid_null = Node::mk_const(m, 27, -1, 0, 6, datum::Datum::from_i64(0), true, false).unwrap();
        let gate = Node::mk_const(m, 16, -1, 0, 1, datum::Datum::from_bool(false), false, true).unwrap();
        assert!(never(gate));
        let list = |cells: &[_]| NodeList::from_slice(m, cells).unwrap();
        let dummy = Node::mk(m, ResultPlan { plan: Plan { plan_node_id: 3, targetlist: list(&[te(tid_null, 1, None, false)]), ..Default::default() }, resconstantqual: Some(gate) }).unwrap();
        let outer = Node::mk(m, ResultPlan { plan: Plan { plan_node_id: 2, ..Default::default() }, resconstantqual: None }).unwrap();
        let ctid = Node::mk_var(m, INNER_VAR, 1, 27, -1, 0, 0).unwrap();
        let nest = NestLoop { join: Join { plan: Plan { plan_node_id: 1, targetlist: list(&[te(ctid, 1, Some("ctid"), true)]), lefttree: Some(outer), righttree: Some(dummy), ..Default::default() }, ..Default::default() }, ..Default::default() };
        assert_eq!(rowid_from(&nest.join.plan), Some(3), "the dummy inner computes the null-extended ctid");
        let own = Node::mk_var(m, 1, -1, 27, -1, 0, 0).unwrap();
        let whole = Plan { plan_node_id: 4, targetlist: list(&[te(own, 1, Some("ctid"), true)]), ..Default::default() };
        assert_eq!(rowid_from(&whole), Some(4), "the whole input computes it");
        let past = Node::mk_var(m, OUTER_VAR, 1, 27, -1, 0, 0).unwrap();
        let none = Plan { plan_node_id: 5, targetlist: list(&[te(past, 1, Some("ctid"), true)]), ..Default::default() };
        assert_eq!(rowid_from(&none), None, "a reference into a child the node does not have");
        assert_eq!(rowid_from(&Plan { plan_node_id: 6, ..Default::default() }), None, "no ctid junk column");
    }
}
