//! A MERGE target's columns carried to the write op (decompose.md §2.12): the target scan
//! publishes the attributes the actions read and every projection on the path to the ModifyTable
//! passes them on as synthesized junk columns. An UPDATE's old row travels the same way (write.rs
//! `update_carry`, V4R-UPD: the attributes its SET list leaves, so the write never refetches the row).

use gravity_program::ArenaVec;
use types_nodes::list::NodeList;
use types_nodes::primnodes::{INNER_VAR, OUTER_VAR};
use types_nodes::Node;

use crate::graph::Op;
use crate::ledger::Kind;
use crate::admit::Refusal;
use crate::ops::shared::carry::{child_attno, under_target, Carry};
use crate::walk::{Builder, Open};

/// A refusal's text: MERGE's as it always read (census keys hold it), an UPDATE's own.
macro_rules! why {
    ($update:expr, $s:literal) => {
        if $update { concat!("UPDATE", $s) } else { concat!("MERGE", $s) }
    };
}

/// The target's attribute number for `attno` of `rti` (a child under the target; `parent_colnos` is the map).
pub fn root_attno(b: &Builder<'_, '_>, target: u32, rti: u32, attno: i16) -> Option<i16> {
    crate::walk::stack_check(); // D3748
    if rti == target {
        return Some(attno);
    }
    let ari = b.stmt.appendRelations.iter().filter_map(|n| n.as_append_rel_info()).find(|a| a.child_relid == rti)?;
    let pa = ari.parent_colnos.get(attno.checked_sub(1)? as usize).copied().filter(|&p| p > 0)?;
    root_attno(b, target, ari.parent_relid, pa)
}

/// An Append's / MergeAppend's children: their projections agree positionally, so the carried
/// columns sit at one set of resnos (checked) that the Append passes through.
fn append_resnos<'a>(c: &'a Carry, node: Node<'_>) -> Result<Option<&'a ArenaVec<i16>>, &'static str> {
    let kids = if let Some(a) = node.as_append() {
        &a.appendplans
    } else if let Some(m) = node.as_merge_append() {
        &m.mergeplans
    } else {
        return Ok(None);
    };
    let mut found: Option<&'a ArenaVec<i16>> = None;
    for k in kids.iter() {
        match (found, resnos_of(c, Some(k))) {
            (None, r) => found = r,
            (Some(f), Some(r)) if f.as_slice() == r.as_slice() => {}
            (Some(_), _) => return Err(why!(c.update, ": the target's columns at different positions under an Append")),
        }
    }
    Ok(found)
}

fn resnos_of<'a>(c: &'a Carry, node: Option<Node<'_>>) -> Option<&'a ArenaVec<i16>> {
    // a Hash is never mapped on its own (`map_hash_join` walks its child): its child's positions are its
    let node = node?;
    let node = node.as_hash().and_then(|h| h.plan.lefttree).unwrap_or(node);
    let id = node.as_plan()?.plan_node_id;
    c.by_node.iter().find(|(n, _)| *n == id).map(|(_, r)| r)
}

fn scanrelid(node: Node<'_>) -> Option<u32> {
    if let Some(s) = node.as_seq_scan() {
        return Some(s.scan.scanrelid);
    }
    if let Some(s) = node.as_index_scan() {
        return Some(s.scan.scanrelid);
    }
    if let Some(s) = node.as_bitmap_heap_scan() {
        return Some(s.scan.scanrelid);
    }
    if let Some(s) = node.as_tid_scan() {
        return Some(s.scan.scanrelid);
    }
    if let Some(s) = node.as_tid_range_scan() {
        return Some(s.scan.scanrelid);
    }
    node.as_sample_scan().map(|s| s.scan.scanrelid)
}

/// The mapped node's projection extended with the carried columns (a Hash passes its child's through).
pub fn after<'p>(b: &mut Builder<'p, '_>, node: Node<'p>, k: Kind, mut open: Open<'p>) -> Result<Open<'p>, Refusal> {
    let Some(c) = b.carry.as_ref() else { return Ok(open) };
    let Some(plan) = node.as_plan() else { return Ok(open) };
    let id = plan.plan_node_id;
    let upd = c.update;
    let scan = scanrelid(node).filter(|&r| under_target(b, c.rti, r));
    let null = scan.is_none() && dummy_target(c, node);
    let (varno, relid, from): (i32, u32, ArenaVec<i16>) = if null {
        (0, c.relid, c.attnos.iter().copied().collect_in(b.arena))
    } else if let Some(r) = scan {
        let mut from = ArenaVec::with_capacity_in(c.attnos.len(), b.arena);
        for &a in c.attnos.iter() {
            from.push(child_attno(b, c.rti, r, a).ok_or(Refusal::shape(k, id, why!(upd, ": a target column the partition does not have")))?);
        }
        (r as i32, b.rte(r).map_or(c.relid, |e| e.relid), from)
    } else if let Some(resnos) = append_resnos(c, node).map_err(|r| Refusal::shape(k, id, r))? {
        (OUTER_VAR, c.relid, resnos.iter().copied().collect_in(b.arena))
    } else {
        match (resnos_of(c, plan.lefttree), resnos_of(c, plan.righttree)) {
            (None, None) => return Ok(open),
            (Some(l), None) => (OUTER_VAR, c.relid, l.iter().copied().collect_in(b.arena)),
            (None, Some(r)) => (INNER_VAR, c.relid, r.iter().copied().collect_in(b.arena)),
            (Some(_), Some(_)) => return Err(Refusal::shape(k, id, why!(upd, ": the target's columns carried on both sides of a join"))),
        }
    };
    let (tlist, at) = match open.ops.iter().enumerate().rev().find_map(|(i, o)| if let Op::Project(t) = o { Some((*t, i)) } else { None }) {
        Some((t, i)) if core::ptr::eq(t, &plan.targetlist) => (t, i),
        _ if scan.is_none() && !matches!(k, Kind::NestLoop | Kind::HashJoin | Kind::MergeJoin) => {
            b.carry.as_mut().unwrap().by_node.push((id, from));
            return Ok(open);
        }
        _ => return Err(Refusal::shape(k, id, why!(upd, ": the target's columns cannot be carried through this node's projection"))),
    };
    let alloc = b.alloc.ok_or(Refusal::shape(k, id, why!(upd, ": no node allocator (decompose_in)")))?;
    let mut cells: ArenaVec<Node<'p>> = ArenaVec::with_capacity_in(tlist.len() + from.len(), b.arena);
    cells.extend(tlist.iter());
    let mut resnos = ArenaVec::with_capacity_in(from.len(), b.arena);
    for (i, &col) in from.iter().enumerate() {
        // a carried column is the target's attribute `attnos[i]` (a child's own attno on its scan): its type is the facts'
        let (trel, tatt) = if scan.is_some() { (relid, col) } else { (c.relid, c.attnos[i]) };
        let att = b.facts.att(trel, tatt).ok_or(Refusal::shape(k, id, if upd { "no attribute facts for an UPDATE target column" } else { "no attribute facts for a MERGE target column" }))?;
        let resno = (tlist.len() + i + 1) as i16;
        let var = if null {
            Node::mk_const(alloc, att.typid, att.typmod, 0, i32::from(att.typlen), datum::Datum::from_i64(0), true, att.byval)
        } else {
            Node::mk_var(alloc, varno, col, att.typid, att.typmod, 0, 0)
        };
        let var = var.map_err(|_| Refusal::shape(k, id, why!(upd, ": out of memory synthesizing a carried column")))?;
        cells.push(Node::mk_target_entry(alloc, var, resno, None, true).map_err(|_| Refusal::shape(k, id, why!(upd, ": out of memory synthesizing a carried column")))?);
        resnos.push(resno);
    }
    let list = NodeList::from_slice(alloc, &cells).and_then(|l| mcx::alloc_leak_in(alloc, l)).map_err(|_| Refusal::shape(k, id, why!(upd, ": out of memory synthesizing a carried projection")))?;
    open.ops[at] = Op::Project(list);
    b.carry.as_mut().unwrap().by_node.push((id, resnos));
    Ok(open)
}

/// D3367: the planner's dummy relation as the target's side of a MERGE (a join condition that folds to false or NULL:
/// the whole input, or beside a NOT MATCHED action the source LEFT JOIN an input-less Result under a one-time gate that
/// never passes, the node computing the junk ctid: `Carry.rowid_from`). Its row never comes, so it carries the target's
/// columns as NULL constants of their types: every joined row is the source's, null-extended, NOT MATCHED.
fn dummy_target(c: &Carry, node: Node<'_>) -> bool {
    node.as_result().is_some_and(|r| r.plan.plan_node_id == c.rowid_from && r.plan.lefttree.is_none() && r.resconstantqual.is_some_and(super::merge::never))
}

trait CollectIn<T> {
    fn collect_in(self, a: gravity_program::ArenaRef) -> ArenaVec<T>;
}

impl<T, I: Iterator<Item = T>> CollectIn<T> for I {
    fn collect_in(self, a: gravity_program::ArenaRef) -> ArenaVec<T> {
        let mut v = ArenaVec::new_in(a);
        v.extend(self);
        v
    }
}
