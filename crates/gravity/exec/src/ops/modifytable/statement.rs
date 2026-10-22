//! M4's per-statement write protocol for v4 programs (M4-plan.md §3.11, cards/M4.md L2). The statement bracket a v4
//! write runs in is the runner's and the hosts' today: `GetCurrentCommandId(true)` (`output_cid.rs`), the AFTER-trigger
//! query bracket (`after_query.rs`, `runner/execute.rs` / `drive.rs`), BEFORE / AFTER STATEMENT triggers
//! (`hosts/src/trigger_stmt.rs`), RowExclusiveLock per target and `CheckValidResultRel` (`hosts/src/write.rs` bind). The
//! admit / bind arms of the INSERT / UPDATE kinds come with their readers (L0-a's row-op face, L4); the four runtime
//! protocols v4 declares (`ProtoKind::{Literals, Route, Transition}`) are m4_proto.rs's (Decision 15).
//! Created by M4 (owner M4-HOSTS).
use gravity_program::protos::{ProtoKind, V4Descs};
use types_nodes::plannodes::PlannedStmt;

use crate::ops::Protocol;
use crate::admit::{Admit, Bind, Refused};


pub const ADMIT: &[Admit] = &[];
pub const BIND: &[Bind] = &[];
pub const PROTOCOLS: &[&dyn Protocol] = &[&crate::ops::modifytable::proto::Literals, &crate::ops::modifytable::proto::Route, &crate::ops::modifytable::proto::Transition];

/// R2b (V4R-205): the M4 protocols' admission over this plan, at every admission of the statement (admit.rs `bind`): a
/// declared write.literals whose ValuesScan's cells that are not a Const are not the program's (NEW-29: their
/// `literals_mark`) is a named refusal (`proto::admit_literals`).
pub fn admit<A: gravity_program::Allocator + Clone>(v4: Option<&V4Descs<A>>, pstmt: &'static PlannedStmt<'static>) -> Result<(), Refused> {
    let Some(d) = v4 else { return Ok(()) };
    d.protos.iter().filter(|p| p.kind == ProtoKind::Literals).try_for_each(|p| crate::ops::modifytable::proto::admit_literals(p, crate::ops::modifytable::proto::roots(pstmt)))
}

/// Before the graph admission, as ExecInitModifyTable raises it before any row.
pub fn admit_ams(pstmt: &PlannedStmt<'_>) -> types_error::PgResult<()> {
    if pstmt.commandType == types_nodes::nodes_enums::CmdType::CMD_SELECT && !pstmt.hasModifyingCTE {
        return Ok(());
    }
    for mt in pstmt.planTree.into_iter().chain(pstmt.subplans.iter().flatten()).filter_map(|n| n.as_modify_table()) {
        for rti in mt.resultRelations.iter() {
            let rte = (rti as usize).checked_sub(1).filter(|&i| i < pstmt.rtable.len()).and_then(|i| pstmt.rtable.nth(i).as_range_tbl_entry());
            if let Some(rte) = rte {
                gravity_hosts::ops::modifytable::write::admit_target(rte.relid, mt.operation, mt.onConflictAction != 0)?;
            }
        }
    }
    Ok(())
}

/// H-f (V4R-239): a MERGE's subcommands (`ExecInitMerge`'s `mt_merge_subcommands`): the INSERT / UPDATE / DELETE any
/// result relation's `mergeActionLists` holds (DO NOTHING is none), for the write host's statement triggers, its
/// transition capture and a view target's per-action INSTEAD OF hosts. Read per admission, as the modified columns are.
pub fn merge_ops(pstmt: &'static PlannedStmt<'static>) -> gravity_hosts::ops::modifytable::trigger_stmt::MergeOps {
    pstmt.planTree.and_then(|n| n.as_modify_table()).map_or(gravity_hosts::ops::modifytable::trigger_stmt::MergeOps::NONE, subcommands)
}

/// One ModifyTable's MERGE subcommands (`ExecInitMerge`'s `mt_merge_subcommands`): the INSERT / UPDATE / DELETE its
/// `mergeActionLists` holds (DO NOTHING is none). The statement's own (`merge_ops`), or a writing CTE's (`beside.rs`).
pub fn subcommands(mt: &types_nodes::plannodes::ModifyTable<'_>) -> gravity_hosts::ops::modifytable::trigger_stmt::MergeOps {
    use types_nodes::nodes_enums::CmdType;
    let mut ops = gravity_hosts::ops::modifytable::trigger_stmt::MergeOps::NONE;
    for a in mt.mergeActionLists.iter().filter_map(|l| l.as_list()).flat_map(|l| l.iter()).filter_map(|a| a.as_merge_action()) {
        match a.commandType {
            CmdType::CMD_INSERT => ops.insert = true,
            CmdType::CMD_UPDATE => ops.update = true,
            CmdType::CMD_DELETE => ops.delete = true,
            _ => {}
        }
    }
    ops
}
