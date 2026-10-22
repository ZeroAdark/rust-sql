//! D2633: the columns of each CTE its CteScans read (plan facts): a CTE's store keeps only those (lowerv4
//! ops/ctescan/lower.rs `kept`, through `PipelineGraph::cte_reads`), and a scan at the root of the CTE's plan carries only
//! the lanes they and its quals need (facts::demand `entries_read`). PostgreSQL's tuplestore holds the whole CTE row and each
//! CteScan deforms what its readers ask of the slot (nodeCtescan.c), so this is a cost choice, never an answer change.
use gravity_program::ArenaVec;
use types_nodes::plannodes::{CteScan, Plan};

use crate::expr;
use crate::walk::Builder;

/// D2633: the columns of CTE `plan_id` its CteScans read, 1-based and ascending (`cte_cols`); `None` = every column. The
/// lowerer keeps only these in the CTE's store (lowerv4 ops/ctescan/lower.rs `kept`), and a scan at the root of the CTE's
/// plan carries only the columns they and its quals need (`cte_root`): PostgreSQL's tuplestore holds the whole CTE row,
/// each CteScan deforming what its readers ask of the slot (a cost choice, never an answer change).
#[derive(Clone, Copy, Debug)]
pub struct CteRead<'p> {
    pub plan_id: i32,
    pub cols: Option<&'p [i16]>,
}

/// D2633: the union of `cte_read` over every CteScan of CTE `plan_id` in the statement (its tree, every InitPlan, SubPlan
/// and CTE: `rerun::each_plan_node`); `None` when one reads every column or no CteScan reads the CTE.
pub fn cte_cols<'p>(b: &mut Builder<'p, '_>, plan_id: i32) -> Option<&'p [i16]> {
    let mut scans: ArenaVec<&'p CteScan<'p>> = ArenaVec::new_in(b.arena); // alloc: Cold
    crate::rerun::each_plan_node(b.stmt, &mut |n| {
        if let Some(cs) = n.as_cte_scan().filter(|c| c.ctePlanId == plan_id) {
            scans.push(cs);
        }
    });
    if scans.is_empty() {
        return None;
    }
    let mut cols: ArenaVec<i16> = ArenaVec::new_in(b.arena); // alloc: Cold
    for cs in scans.iter().copied() {
        for &a in cte_read(b, cs)? {
            if !cols.contains(&a) {
                cols.push(a);
            }
        }
    }
    cols.sort_unstable();
    #[cfg(debug_assertions)]
    if let Some(root) = (plan_id as usize).checked_sub(1).and_then(|n| b.stmt.subplans.nth(n)).and_then(|n| n.as_plan()) {
        for t in root.targetlist.iter().filter_map(|n| n.as_target_entry()).filter(|t| !cols.contains(&t.resno)) {
            super::census::log(b, root, t.resno, super::Read::No, super::census::Site::Store);
        }
    }
    Some(cols.leak())
}

/// D2633 (facts::demand `entries_read`): when `plan` is the root of a CTE's plan whose CteScans read only some columns,
/// which of its target entries they read (by resno: a CTE's column is its root's entry).
pub fn cte_root<'p>(b: &Builder<'p, '_>, plan: &Plan<'p>) -> Option<ArenaVec<bool>> {
    let root = |id: i32| (id as usize).checked_sub(1).filter(|&n| n < b.stmt.subplans.len()).and_then(|n| b.stmt.subplans.nth(n)).and_then(|n| n.as_plan());
    let cols = b.graph.cte_reads.iter().find(|r| root(r.plan_id).is_some_and(|p| core::ptr::eq(p, plan)))?.cols?;
    let mut hit = ArenaVec::new_in(b.arena); // alloc: Cold
    for t in plan.targetlist.iter() {
        hit.push(t.as_target_entry().is_none_or(|t| cols.contains(&t.resno)));
    }
    Some(hit)
}

/// D2633: the CTE columns CteScan `cs` reads: its quals' columns and those of the target entries its parent reads
/// (facts::demand `read_entries`: PG hands a CteScan under a join or an aggregate its physical target list, every
/// column, so a bare column entry the parent never reads is no read). `None` (every column) for a whole-row or system
/// column, and for an expression `expr::walk` does not know (fail closed).
fn cte_read<'p>(b: &mut Builder<'p, '_>, cs: &'p CteScan<'p>) -> Option<&'p [i16]> {
    let (plan, varno) = (&cs.scan.plan, cs.scan.scanrelid as i32);
    let entries = crate::facts::demand::read_entries(b, plan, varno);
    let mut cols: ArenaVec<i16> = ArenaVec::new_in(b.arena); // alloc: Cold
    for n in entries.iter().copied().chain(plan.qual.iter()) {
        let mut ok = true;
        let known = expr::walk(n, &mut |x| {
            if let Some(v) = x.as_var().filter(|v| v.varno == varno && v.varlevelsup == 0) {
                match v.varattno {
                    a if a <= 0 => ok = false,
                    a if !cols.contains(&a) => cols.push(a),
                    _ => {}
                }
            }
            ok
        });
        if !(known && ok) {
            return None;
        }
    }
    cols.sort_unstable();
    Some(cols.leak())
}
