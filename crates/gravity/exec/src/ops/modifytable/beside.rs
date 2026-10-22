//! D1687: a writer beside the program's `write` (a writing CTE's, one of `Program.writes`) reads its own check options
//! and, a MERGE, its own subcommands. The admission's check list holds every write's (each check option tagged with its
//! write's relation and CTE flag at the template, admit `check_template`): the program write keeps the ones of its own
//! write, each writer beside it gets the ones of its write (hosts `set_beside`, read by `derive_writer_beside`), and a
//! MERGE beside it the subcommands of its own ModifyTable (ExecInitMerge's `mt_merge_subcommands`: its statement
//! triggers, a view target's hosts). A program with no writer beside its write, or none with a check option or a MERGE,
//! keeps the one list as before.
//! D3348: each writing CTE's modified columns too, by relation and command (`modified_cols_by_rel`: its own ModifyTable's,
//! never the statement's own write's), which a write over several result relations forms its new rows from. D3623: and by
//! that ModifyTable's plan node (`WriteAt::node`), which a writer beside finds its own by first.

use std::rc::Rc;

use gravity_decompose::ops::shared::modifytable::WriteDesc;
use gravity_decompose::ops::shared::subplan::SubLink;
use gravity_decompose::PipelineGraph;
use gravity_hosts::ops::modifytable::trigger_stmt::MergeOps;
use gravity_hosts::ops::modifytable::write_check::CheckSpec;
use gravity_hosts::HostRegistry;
use gravity_program::{Cmd, Program, WritePlan};
use types_nodes::nodes_enums::CmdType;
use types_nodes::plannodes::PlannedStmt;

use crate::admit::{CheckTpl, Refused};

/// A write's relation: a route's root, else its target (hosts `derive_writer_beside` finds a writer's facts by it).
pub(crate) fn rel(p: &WritePlan) -> u32 {
    p.route.as_ref().map_or(p.target_rel, |r| r.root)
}

/// A write desc's relation as its lowered plan names it: a routed write's plan is routed through the desc's root (lowerv4
/// `WritePlan::with_route`, which the desc's own plan never carries), else its target.
pub(crate) fn desc_rel(d: &WriteDesc<'_>) -> u32 {
    d.route.as_ref().map_or(d.plan.target_rel, |r| r.root_relid)
}

/// Pipeline `k` is a data-modifying CTE's (lowerv4 `Gx::cte_write`, which sets its write plan's `cte`; a WriteDesc's own
/// plan never carries it): a check option is matched to its write plan by its relation and this.
pub(crate) fn cte_write(graph: &PipelineGraph<'_>, k: usize) -> bool {
    graph.init_plans.iter().any(|i| i.pipeline as usize == k && matches!(i.link, SubLink::Cte))
}

/// This admission's check options (`checks`, bound from `tpl` in its order) and MERGE subcommands onto the program write
/// and each writer beside it. Fail closed: a check option of no single write plan, and two writers beside of one relation
/// whose facts would mix (a writer beside's are found by its relation), refuse by name.
pub(crate) fn set(reg: &mut HostRegistry, program: &Program, pstmt: &'static PlannedStmt<'static>, tpl: &[CheckTpl], checks: Option<Vec<CheckSpec>>) -> Result<(), Refused> {
    // D3348: a writer beside finds its modified columns by its relation and command (`modified_cols_by_rel`, each
    // writing CTE's own): fail closed when two writing CTEs of one relation and command update different columns.
    // D3623: unless every writer beside names its own ModifyTable (`WriteAt::node`), which it finds its columns by
    if !program.writes.is_empty() && !by_node(program, pstmt) && modified_mixed(pstmt) {
        return crate::admit::refuse("hosts", "two writing CTEs of one relation and command whose updated columns differ (D3348)");
    }
    let merges = program.writes.iter().any(|w| w.plan.cmd == Cmd::Merge);
    if program.writes.is_empty() || checks.is_none() && !merges {
        if let Some(c) = checks {
            reg.set_checks(c);
        }
        return Ok(());
    }
    let plans: Vec<&WritePlan> = core::iter::once(&program.write).chain(program.writes.iter().map(|w| &w.plan)).collect(); // alloc: Program
    let mut own: Vec<Vec<CheckSpec>> = plans.iter().map(|_| Vec::new()).collect(); // alloc: Program
    for (t, c) in tpl.iter().zip(checks.into_iter().flatten()) {
        // fail closed: a check option whose write is not exactly one of the program's write plans names no writer
        let mut of = plans.iter().enumerate().filter(|(_, p)| (rel(p), p.cte) == (t.rel, t.cte)).map(|(i, _)| i);
        let (Some(i), None) = (of.next(), of.next()) else { return crate::admit::refuse("hosts", "a check option of no single write plan beside another write (D1687)") };
        own[i].push(c); // alloc: Program
    }
    // a writer beside finds its facts by its relation (hosts `derive_writer_beside`): fail closed when two writers beside
    // of one relation would read each other's check options or subcommands. D3472: the program's own write is not one of
    // them: it reads the program's list (`set_checks`) and never the writers' (`set_beside`), and each writer beside has
    // its own entry there, so a writer beside of the program write's relation finds its own facts, never the write's
    let shared = |i: usize| plans.iter().enumerate().skip(1).any(|(j, p)| j != i && rel(p) == rel(plans[i]));
    if (1..plans.len()).any(|i| (!own[i].is_empty() || plans[i].cmd == Cmd::Merge) && shared(i)) {
        return crate::admit::refuse("hosts", "two write plans of one relation beside each other, one with a check option or a MERGE (D1687)");
    }
    let mut own = own.into_iter();
    if let Some(c) = own.next().filter(|c| !c.is_empty()) {
        reg.set_checks(c);
    }
    let facts = program.writes.iter().zip(own).map(|(w, c)| {
        let merge = if w.plan.cmd == Cmd::Merge { merge_ops(pstmt, rel(&w.plan)) } else { MergeOps::NONE };
        (rel(&w.plan), (!c.is_empty()).then(|| Rc::from(c)), merge) // alloc: Program
    });
    reg.set_beside(facts.collect()); // alloc: Program
    Ok(())
}

/// The subcommands of the one writing CTE's MERGE into `rel` (its root or a result relation), `statement::subcommands`
/// of its ModifyTable; none when no such ModifyTable or several.
fn merge_ops(pstmt: &'static PlannedStmt<'static>, rel: u32) -> MergeOps {
    let relid = |rti: u32| crate::shapes::rte(pstmt, rti).map(|r| r.relid);
    let of = |m: &&types_nodes::plannodes::ModifyTable<'static>| m.rootRelation != 0 && relid(m.rootRelation) == Some(rel) || m.resultRelations.iter().any(|r| relid(r as u32) == Some(rel));
    let mut mts = pstmt.subplans.iter().flatten().filter_map(|n| n.as_modify_table()).filter(|m| m.operation == CmdType::CMD_MERGE && !m.canSetTag).filter(of);
    let (Some(mt), None) = (mts.next(), mts.next()) else { return MergeOps::NONE };
    super::statement::subcommands(mt)
}

/// D1687: each writing CTE's `insertedCols` / `updatedCols` (its own ModifyTable's, `canSetTag` false: the first of its
/// result relations that carries a `perminfoindex`, a routed write's root), by its relation and command, which a writer
/// beside the program's write finds them by (hosts `derive_writer_beside`). D3348: the statement's own ModifyTable stays out
/// (its write reads `modified_cols`): it may write the same relation with other SET columns, and a write over several result
/// relations forms each new row from these columns (hosts write_route.rs `MultiTargetHost`). Two writing CTEs of one
/// relation and command whose updated columns differ refuse (`modified_mixed`), unless each writer names its ModifyTable's
/// plan node (D3623: the third field, which the writer finds its own by first, `by_node`).
pub fn modified_cols_by_rel(pstmt: &'static PlannedStmt<'static>) -> Vec<(u32, Cmd, i32, gravity_hosts::ops::modifytable::write_constraints::ModifiedCols)> {
    let of = |rti: i32| {
        let rte = crate::shapes::rte(pstmt, rti as u32)?;
        let pi = pstmt.permInfos.nth(rte.perminfoindex.checked_sub(1)? as usize).as_rte_permission_info()?;
        Some((rte.relid, (&pi.insertedCols, &pi.updatedCols)))
    };
    let cmd = |op: CmdType| match op {
        CmdType::CMD_INSERT => Some(Cmd::Insert),
        CmdType::CMD_UPDATE => Some(Cmd::Update),
        CmdType::CMD_DELETE => Some(Cmd::Delete),
        CmdType::CMD_MERGE => Some(Cmd::Merge),
        _ => None,
    };
    let own = |m: &types_nodes::plannodes::ModifyTable<'static>| {
        let (rel, cols) = m.resultRelations.iter().chain((m.rootRelation != 0).then_some(m.rootRelation as i32)).find_map(of)?;
        Some((rel, cmd(m.operation)?, m.plan.plan_node_id, cols))
    };
    pstmt.subplans.iter().flatten().filter_map(|n| n.as_modify_table()).filter(|m| !m.canSetTag).filter_map(own).collect() // alloc: Plan
}

/// D3348: two writing CTEs of one relation and command whose `updatedCols` differ: a writer beside, found by its relation
/// and command, would read the other's SET columns (`modified_cols_by_rel`). `insertedCols` alone (two INSERTs of one
/// relation) names no column a write forms, only ExecConstraints' DETAIL, as before; an INSERT and an UPDATE of one
/// relation (regress with.sql's `WITH RECURSIVE t AS (INSERT INTO y ...), t2 AS (UPDATE y ...)`) each find their own.
pub fn modified_mixed(pstmt: &'static PlannedStmt<'static>) -> bool {
    let by = modified_cols_by_rel(pstmt);
    by.iter().enumerate().any(|(i, (r, k, _, c))| by[i + 1..].iter().any(|(s, l, _, d)| s == r && l == k && c.1 != d.1))
}

/// D3623: every writer beside the program's write names a writing CTE's ModifyTable by its plan node (`WriteAt::node`,
/// one entry of `modified_cols_by_rel`), so each finds its own modified columns whatever the others of its relation update;
/// and the program's write, when it is a writing CTE's, is one ModifyTable too (`program_write_cols`).
fn by_node(program: &Program, pstmt: &'static PlannedStmt<'static>) -> bool {
    let by = modified_cols_by_rel(pstmt);
    program.writes.iter().all(|w| w.node != 0 && by.iter().filter(|e| e.2 == w.node).count() == 1) && (!program.write.cte || program_write_cols(program, pstmt).is_some())
}

/// D3623: the modified columns of the program's own write when it is a writing CTE's (a SELECT over writing CTEs, whose
/// statement has no ModifyTable of its own for `admit::modified_cols` to read): the one writing CTE's ModifyTable of its
/// relation and command that no writer beside names (`WriteAt::node`); `None` for any other write, or not exactly one.
pub fn program_write_cols(program: &Program, pstmt: &'static PlannedStmt<'static>) -> Option<gravity_hosts::ops::modifytable::write_constraints::ModifiedCols> {
    if !program.write.cte {
        return None;
    }
    let by = modified_cols_by_rel(pstmt);
    let rel = rel(&program.write);
    let mut own = by.iter().filter(|e| e.0 == rel && e.1 == program.write.cmd && !program.writes.iter().any(|w| w.node == e.2));
    let (Some(e), None) = (own.next(), own.next()) else { return None };
    Some(e.3)
}
