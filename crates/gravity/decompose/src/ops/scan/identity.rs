//! PGRCSERVE M3 (L1), the compile rule of row identity: the relations whose rows a statement names by their identity. A
//! relation of another table AM among them reads its AM's rows (hosts am_rows.rs `Roads`, the rows road), whose `tts_tid`
//! is the AM's own TID for the row (pgrcolumnar: the row's stable ID), so the program's `ctid` and the locator a write, a
//! lock or EvalPlanQual takes are that TID; that road's scan asks the AM to stamp it only for such a relation (`Need`).
use nodes_core::NodeWalker;
use types_error::PgResult;
use types_nodes::nodes_enums::CmdType;
use types_nodes::parsenodes::RTEKind;
use types_nodes::plannodes::{Plan, PlannedStmt, RowMarkType};
use types_nodes::Node;

use crate::ops::shared::scan::SELF_ITEM_POINTER;

/// Why a statement names a relation's rows by identity, the strongest first (one relation, one need: the first found of
/// the strongest).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Need {
    /// A result relation of UPDATE, DELETE or MERGE: the write fetches, locks and replaces each row by its TID.
    Dml,
    /// A locking row mark (FOR UPDATE / NO KEY UPDATE / SHARE / KEY SHARE): the lock host locks by TID.
    Lock,
    /// A REFERENCE row mark (a joined relation of a write or a lock): EvalPlanQual refetches the row by its TID.
    Epq,
    /// A TID or TID range scan, or a scan that reads `ctid` (its target list or qual: a cursor's identity copy too).
    Ctid,
}

/// The relations (pg_class OIDs, without repeats) whose rows `stmt` names by identity, each with its strongest `Need`: an
/// UPDATE, DELETE or MERGE result relation, a row mark's relation other than a COPY mark's, a TID or TID range scan's
/// relation (its AM fetches each TID, or refuses the range walk), and the relation of a scan whose target list or qual
/// reads `ctid`.
pub fn relids<'p>(stmt: &'p PlannedStmt<'p>) -> Vec<(u32, Need)> {
    let mut rtis: Vec<(u32, Need)> = Vec::new(); // alloc: Program
    for m in stmt.rowMarks.iter().filter_map(|n| n.as_plan_row_mark()) {
        let need = match m.markType {
            RowMarkType::ROW_MARK_COPY => continue,
            RowMarkType::ROW_MARK_REFERENCE => Need::Epq,
            _ => Need::Lock,
        };
        rtis.push((m.rti, need)); // alloc: Program
    }
    crate::rerun::each_plan_node(stmt, &mut |n| {
        if let Some(mt) = n.as_modify_table() {
            if matches!(mt.operation, CmdType::CMD_UPDATE | CmdType::CMD_DELETE | CmdType::CMD_MERGE) {
                rtis.extend(mt.resultRelations.iter().map(|r| (r as u32, Need::Dml))); // alloc: Program
            }
            return;
        }
        let Some((scanrelid, plan, tid)) = scan_of(n) else { return };
        if tid || reads_ctid(plan, scanrelid) {
            rtis.push((scanrelid, Need::Ctid)); // alloc: Program
        }
    });
    let mut out: Vec<(u32, Need)> = Vec::new(); // alloc: Program
    for (rti, need) in rtis {
        let rte = (rti as usize).checked_sub(1).filter(|&i| i < stmt.rtable.len()).and_then(|i| stmt.rtable.nth(i).as_range_tbl_entry());
        let Some(rte) = rte.filter(|r| r.rtekind == RTEKind::RTE_RELATION && r.relid != 0) else { continue };
        match out.iter_mut().find(|x| x.0 == rte.relid) {
            Some(x) => x.1 = x.1.min(need),
            None => out.push((rte.relid, need)), // alloc: Program
        }
    }
    out
}

/// A relation scan node's range-table index and plan, and whether it is a TID or TID range scan.
fn scan_of<'p>(n: Node<'p>) -> Option<(u32, &'p Plan<'p>, bool)> {
    if let Some(s) = n.as_seq_scan() {
        return Some((s.scan.scanrelid, &s.scan.plan, false));
    }
    if let Some(s) = n.as_index_scan() {
        return Some((s.scan.scanrelid, &s.scan.plan, false));
    }
    if let Some(s) = n.as_index_only_scan() {
        return Some((s.scan.scanrelid, &s.scan.plan, false));
    }
    if let Some(s) = n.as_bitmap_heap_scan() {
        return Some((s.scan.scanrelid, &s.scan.plan, false));
    }
    if let Some(s) = n.as_sample_scan() {
        return Some((s.scan.scanrelid, &s.scan.plan, false));
    }
    if let Some(s) = n.as_tid_scan() {
        return Some((s.scan.scanrelid, &s.scan.plan, true));
    }
    n.as_tid_range_scan().map(|s| (s.scan.scanrelid, &s.scan.plan, true))
}

/// Whether the scan's target list or qual reads its own relation's `ctid` (a Var of `scanrelid`, this query level).
fn reads_ctid(plan: &Plan<'_>, scanrelid: u32) -> bool {
    struct Find(i32);
    impl<'m> NodeWalker<'m> for Find {
        fn visit(&mut self, n: Node<'m>) -> PgResult<bool> {
            if let Some(v) = n.as_var() {
                return Ok(v.varno == self.0 && v.varlevelsup == 0 && v.varattno == SELF_ITEM_POINTER);
            }
            nodes_core::expression_tree_walker(n, self)
        }
    }
    let mut f = Find(scanrelid as i32);
    // a walk that fails reads true (fail closed: the rows road serves a relation that reads no ctid too)
    plan.targetlist.iter().chain(plan.qual.iter()).any(|e| f.visit(e).unwrap_or(true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use types_nodes::list::{NodeList, OptNodeList};
    use types_nodes::parsenodes::RangeTblEntry;
    use types_nodes::plannodes::{PlanRowMark, Scan, SeqScan, TidScan};
    use types_nodes::primnodes::{TargetEntry, Var};

    /// Range table [16400 .. 16404]: a FOR UPDATE mark on 1 (whose scan also reads its ctid: the stronger need stays), a
    /// REFERENCE mark on 2, a COPY mark on 4, a TID scan of 3 in a subplan, a plain scan of 5 reading no ctid of its own.
    #[test]
    fn writes_marks_tid_scans_and_ctid_reads_name_rows_by_identity() {
        let ctx = mcx::MemoryContext::new("identity-test");
        let m = ctx.mcx();
        let rte = |relid| Node::mk(m, RangeTblEntry { rtekind: RTEKind::RTE_RELATION, relid, relkind: b'r', ..Default::default() }).unwrap();
        let rtable = NodeList::from_slice(m, &[rte(16400), rte(16401), rte(16402), rte(16403), rte(16404)]).unwrap();
        let mark = |rti, kind| Node::mk(m, PlanRowMark { rti, prti: rti, markType: kind, ..Default::default() }).unwrap();
        let marks = NodeList::from_slice(m, &[mark(1, RowMarkType::ROW_MARK_EXCLUSIVE), mark(2, RowMarkType::ROW_MARK_REFERENCE), mark(4, RowMarkType::ROW_MARK_COPY)]).unwrap();
        let ctid_of = |varno| {
            let v = Node::mk(m, Var { varno, varattno: SELF_ITEM_POINTER, ..Var::default() }).unwrap();
            NodeList::from_slice(m, &[Node::mk(m, TargetEntry { expr: v, resno: 1, resname: None, ressortgroupref: 0, resorigtbl: 0, resorigcol: 0, resjunk: false }).unwrap()]).unwrap()
        };
        let scan = |rti: u32, tl| Scan { plan: Plan { plan_node_id: rti as i32, targetlist: tl, ..Default::default() }, scanrelid: rti };
        let outer = Node::mk(m, SeqScan { scan: scan(1, ctid_of(1)), ..Default::default() }).unwrap();
        let tid = Node::mk(m, TidScan { scan: scan(3, NodeList::nil()), ..Default::default() }).unwrap();
        let other = Node::mk(m, SeqScan { scan: scan(5, ctid_of(4)), ..Default::default() }).unwrap();
        let subplans = OptNodeList::from_slice(m, &[Some(tid), Some(other)]).unwrap();
        let stmt = PlannedStmt { planTree: Some(outer), rtable, rowMarks: marks, subplans, ..Default::default() };
        // the statement leaks for its own lifetime (a test's few bytes)
        assert_eq!(relids(Box::leak(Box::new(stmt))), vec![(16400, Need::Lock), (16401, Need::Epq), (16402, Need::Ctid)]); // alloc: Cold
    }
}
