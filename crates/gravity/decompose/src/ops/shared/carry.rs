//! A write target's columns carried from its scan to the write op: which target attributes ride along (`Carry`, set by
//! the MERGE / UPDATE mapping) and how a scan of the target, or of a partition / inheritance child under it, adds them to
//! its lane set (`atts_for`). A home, not a family: it imports no family.

use gravity_program::ArenaVec;

use crate::ops::shared::scan::Atts;
use crate::walk::Builder;

pub struct Carry {
    /// An UPDATE's old row (else MERGE's columns): only the refusal texts differ.
    pub update: bool,
    pub rti: u32,
    pub relid: u32,
    pub attnos: ArenaVec<i16>,
    /// Per mapped plan node on the path: the carried columns' resnos in its projection, in `attnos` order.
    pub by_node: ArenaVec<(i32, ArenaVec<i16>)>,
    /// D3367: the plan node that computes a MERGE's junk ctid (the target's side of its join), when it is not a scan of
    /// the target: the planner's dummy relation carries the columns as NULLs (modifytable/carry.rs `dummy_target`); -1
    /// none (an UPDATE's carry too).
    pub rowid_from: i32,
}

/// The target's attribute `attno` as seen by `rti`: the target itself, or a partition / inheritance child
/// under it (the statement's `AppendRelInfo`s, chained for a sub-partition; `parent_colnos` renumbers).
/// `None`: `rti` is not the target nor under it.
pub(crate) fn child_attno(b: &Builder<'_, '_>, target: u32, rti: u32, attno: i16) -> Option<i16> {
    crate::walk::stack_check(); // D3748
    if rti == target {
        return Some(attno);
    }
    let ari = b.stmt.appendRelations.iter().filter_map(|n| n.as_append_rel_info()).find(|a| a.child_relid == rti)?;
    let pa = child_attno(b, target, ari.parent_relid, attno)?;
    ari.parent_colnos.iter().position(|&p| p == pa).map(|j| j as i16 + 1)
}

/// `rti` is the target or a partition / inheritance child under it.
pub(crate) fn under_target(b: &Builder<'_, '_>, target: u32, rti: u32) -> bool {
    crate::walk::stack_check(); // D3748
    rti == target || b.stmt.appendRelations.iter().filter_map(|n| n.as_append_rel_info()).any(|a| a.child_relid == rti && under_target(b, target, a.parent_relid))
}

/// The target's attributes into a scan's lane set when the scan is the target or a child of it.
pub fn atts_for(b: &Builder<'_, '_>, scanrelid: u32, atts: &mut Atts) -> bool {
    match &b.carry {
        Some(c) if under_target(b, c.rti, scanrelid) => c.attnos.iter().all(|&a| child_attno(b, c.rti, scanrelid, a).is_none_or(|ca| atts.add(ca))),
        _ => true,
    }
}
