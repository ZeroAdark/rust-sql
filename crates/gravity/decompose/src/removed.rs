//! Plan node families the server does not run: a Foreign Scan and a Custom Scan, and with them every write into a foreign
//! table through a plan (an INSERT into one has no Foreign Scan: ModifyTable writes it through its FDW). exec's admission
//! raises 0A000 for the first one `find` names (exec admit/removed.rs), before the plan is decomposed: never a refusal,
//! never a fallback. The plan's nodes decide, not its range table: a foreign partition the planner pruned is in no node.
use types_nodes::nodes_enums::CmdType;
use types_nodes::parsenodes::{RTEKind, RangeTblEntry};
use types_nodes::plannodes::PlannedStmt;
use types_nodes::{Node, NodeTag};

/// What `find` names, with the foreign table it reads or writes (`relid` 0: none known, a join pushed down to the server
/// with no foreign table in the range table).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Removed {
    /// A Foreign Scan that reads (`writes`: a direct modify, UPDATE / DELETE pushed down) foreign table `relid`.
    ForeignScan { relid: u32, writes: bool },
    /// A result relation that is a foreign table: the statement writes it through its FDW.
    ForeignTarget { relid: u32 },
    /// A Custom Scan.
    CustomScan,
}

/// The first Foreign Scan or Custom Scan of the statement (its plan tree, then each subplan's, parents first), else its
/// first result relation that is a foreign table.
pub fn find<'p>(stmt: &'p PlannedStmt<'p>) -> Option<Removed> {
    let mut found = None;
    crate::rerun::each_plan_node(stmt, &mut |n| {
        if found.is_none() {
            found = node(stmt, n);
        }
    });
    found.or_else(|| stmt.resultRelations.iter().find_map(|rti| rte(stmt, rti).filter(|r| foreign(r))).map(|r| Removed::ForeignTarget { relid: r.relid }))
}

fn node<'p>(stmt: &'p PlannedStmt<'p>, n: Node<'p>) -> Option<Removed> {
    match n.node_tag() {
        NodeTag::T_ForeignScan => {
            let f = n.as_foreign_scan()?;
            // a join pushed down to the server scans no one relation (scanrelid 0): the range table's first foreign table
            let rel = rte(stmt, f.scan.scanrelid as i32).filter(|r| foreign(r)).or_else(|| stmt.rtable.iter().filter_map(|x| x.as_range_tbl_entry()).find(|r| foreign(r)));
            Some(Removed::ForeignScan { relid: rel.map_or(0, |r| r.relid), writes: f.operation != CmdType::CMD_SELECT })
        }
        NodeTag::T_CustomScan => Some(Removed::CustomScan),
        _ => None,
    }
}

fn rte<'p>(stmt: &'p PlannedStmt<'p>, rti: i32) -> Option<&'p RangeTblEntry<'p>> {
    let i = usize::try_from(rti).ok()?.checked_sub(1)?;
    (i < stmt.rtable.len()).then(|| stmt.rtable.nth(i).as_range_tbl_entry()).flatten()
}

/// pg_class.h `RELKIND_FOREIGN_TABLE`.
const RELKIND_FOREIGN_TABLE: u8 = b'f';

fn foreign(r: &RangeTblEntry<'_>) -> bool {
    r.rtekind == RTEKind::RTE_RELATION && r.relkind == RELKIND_FOREIGN_TABLE
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcx::Mcx;
    use types_nodes::list::{IntList, NodeList, OptNodeList};
    use types_nodes::plannodes::{ForeignScan, Plan, Scan, SeqScan};

    /// A statement over range table [heap 16400, foreign 16401]: `plan`, `subplans`, result relations `results`.
    fn stmt<'a>(m: Mcx<'a>, plan: Node<'a>, subplans: &[Node<'a>], results: &[i32]) -> PlannedStmt<'a> {
        let heap = RangeTblEntry { rtekind: RTEKind::RTE_RELATION, relid: 16400, relkind: b'r', ..Default::default() };
        let ft = RangeTblEntry { rtekind: RTEKind::RTE_RELATION, relid: 16401, relkind: RELKIND_FOREIGN_TABLE, ..Default::default() };
        let rtable = NodeList::from_slice(m, &[Node::mk(m, heap).unwrap(), Node::mk(m, ft).unwrap()]).unwrap();
        let cmd = if results.is_empty() { CmdType::CMD_SELECT } else { CmdType::CMD_INSERT };
        PlannedStmt { planTree: Some(plan), rtable, subplans: OptNodeList::from_slice(m, &subplans.iter().map(|&n| Some(n)).collect::<Vec<_>>()).unwrap(), resultRelations: IntList::from_slice(m, results).unwrap(), commandType: cmd, ..Default::default() }
    }

    /// `find` over `s`, borrowed for its own lifetime (the statement leaks: a test's few bytes).
    fn run(s: PlannedStmt<'_>) -> Option<Removed> {
        find(Box::leak(Box::new(s))) // alloc: Cold
    }

    fn scan<'a>(id: i32, scanrelid: u32) -> Scan<'a> {
        Scan { plan: Plan { plan_node_id: id, ..Default::default() }, scanrelid }
    }

    /// A Foreign Scan in the plan tree or a subplan's, and a foreign result relation, are named; heap scans over a range
    /// table that holds a foreign table (a leaf the planner pruned) are not.
    #[test]
    fn foreign_scans_and_foreign_targets() {
        let ctx = mcx::MemoryContext::new("removed-test");
        let m = ctx.mcx();
        let seq = || Node::mk(m, SeqScan { scan: scan(1, 1), ..Default::default() }).unwrap();
        let fs = |op| Node::mk(m, ForeignScan { scan: scan(2, 2), operation: op, ..Default::default() }).unwrap();
        assert_eq!(run(stmt(m, seq(), &[], &[])), None, "a heap scan beside a pruned foreign leaf");
        assert_eq!(run(stmt(m, fs(CmdType::CMD_SELECT), &[], &[])), Some(Removed::ForeignScan { relid: 16401, writes: false }));
        assert_eq!(run(stmt(m, seq(), &[fs(CmdType::CMD_UPDATE)], &[])), Some(Removed::ForeignScan { relid: 16401, writes: true }), "a subplan's tree");
        assert_eq!(run(stmt(m, seq(), &[], &[2])), Some(Removed::ForeignTarget { relid: 16401 }), "an INSERT into a foreign table");
        assert_eq!(run(stmt(m, seq(), &[], &[1])), None, "an INSERT into a heap table");
    }
}
