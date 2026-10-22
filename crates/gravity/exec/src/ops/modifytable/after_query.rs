use gravity_program::Cmd;
use types_core::Oid;
use types_error::PgResult;
use types_nodes::nodes_enums::CmdType;
use types_nodes::plannodes::PlannedStmt;

/// `standard_ExecutorStart` / `standard_ExecutorFinish`'s AFTER trigger query bracket for a served write (a SELECT
/// queues no events); `end` runs inside the statement's snapshot.
pub fn begin(cmd: Cmd) {
    if cmd != Cmd::Select {
        eoxact_registered();
        turn_registered();
        trigger::AfterTriggerBeginQuery();
        gravity_hosts::ops::modifytable::trigger_stmt::begin();
    }
}

/// D2772 POST (regress triggers#500): the driver's turn hook (driver entry/post.rs `POST_TURN`), registered once per
/// process. In a program with post pipelines (an unread data-modifying CTE's, lowerv4 ops/modifytable/post.rs) the driver
/// turns after the statement's own pipelines: this queues the AFTER STATEMENT events of every writer bound so far (the
/// statement's own; the CTE's writer is not bound yet), as PostgreSQL's ExecModifyTable queues them when the statement's
/// ModifyTable ends, before ExecPostprocessPlan runs the CTE's. The CTE writer then binds (its BEFORE STATEMENT triggers)
/// and its AFTER STATEMENT events queue at the statement's end (`finish`).
fn turn_registered() {
    if gravity_driver::entry::POST_TURN.get().is_none() {
        let _ = gravity_driver::entry::POST_TURN.set(turn);
    }
}

/// D2605: the driver's statement-entry hooks (driver node/ends.rs `STMT_ENDS`), registered once per process at startup:
/// a lazy writer's bind tags the statement entries it made (hosts `trigger_stmt::tag_since`), and each ModifyTable's end
/// queues its own entries' AFTER STATEMENT events (`fire_as_tagged`; 0: the statement's own writers', at the main plan's end).
pub(crate) fn stmt_ends_registered() {
    use gravity_hosts::ops::modifytable::trigger_stmt as ts;
    if gravity_driver::STMT_ENDS.get().is_none() {
        let _ = gravity_driver::STMT_ENDS.set(gravity_driver::StmtEnds { mark: ts::pending_mark, tag: ts::tag_since, end: stmt_end });
    }
}

fn stmt_end(tag: u32) -> gravity_driver::Result<()> {
    gravity_hosts::ops::modifytable::trigger_stmt::fire_as_tagged(tag).map_err(gravity_hosts::stash)
}

fn turn() -> gravity_driver::Result<()> {
    gravity_hosts::ops::modifytable::trigger_stmt::fire_as().map_err(gravity_hosts::stash)
}

/// D832: the served writes' statement-trigger state (hosts `trigger_stmt` PENDING) is dropped at every transaction end,
/// in AbortTransaction's / CommitTransaction's callbacks while its contexts still live (a backend that exits inside a
/// transaction aborts it first); registered once per backend thread (the xact callback registry is per thread; the
/// once-flag lives in `trigger_stmt`'s thread-local block beside PENDING, no new TLS).
fn eoxact_registered() {
    if gravity_hosts::ops::modifytable::trigger_stmt::arm_eoxact() {
        xact::RegisterXactCallback(eoxact, datum::Datum::null());
    }
}

fn eoxact(event: types_core::xact::XactEvent, _arg: datum::Datum) -> PgResult<()> {
    use types_core::xact::XactEvent::*;
    match event {
        XACT_EVENT_COMMIT | XACT_EVENT_PARALLEL_COMMIT | XACT_EVENT_ABORT | XACT_EVENT_PARALLEL_ABORT | XACT_EVENT_PREPARE => gravity_hosts::ops::modifytable::trigger_stmt::at_eoxact(),
        XACT_EVENT_PRE_COMMIT | XACT_EVENT_PARALLEL_PRE_COMMIT | XACT_EVENT_PRE_PREPARE => {}
    }
    Ok(())
}

pub fn end(cmd: Cmd) -> PgResult<()> {
    finish(cmd, false)
}

/// `skip_queue` (EXEC_FLAG_SKIP_TRIGGERS, an RI cascade's SPI): the AS events queue under the enclosing query's level, which fires them.
/// D2830: `AfterTriggerEndQuery` takes the level's root-less targets with their bind-time descriptors (the estate's result
/// relations in C's ExecGetTriggerResultRel); the targets filed at the level while its events fired go after it.
pub fn finish(cmd: Cmd, skip_queue: bool) -> PgResult<()> {
    // the statement's pgrc read counts land before anything here can fail
    crate::runner::pgrc_statement_end();
    if cmd != Cmd::Select {
        // D3293: the leaves written through their table AM finish its bulk insert first (their rows stand before any
        // AFTER trigger reads them)
        gravity_hosts::ops::modifytable::trigger_stmt::finish_am()?;
        gravity_hosts::ops::modifytable::trigger_stmt::fire_as()?;
        if !skip_queue {
            let d = trigger::query_depth();
            let rels = gravity_hosts::ops::modifytable::trigger_stmt::take_targets();
            let r = trigger::AfterTriggerEndQuery(None, &rels);
            gravity_hosts::ops::modifytable::trigger_stmt::drop_targets_from(d);
            r?;
        }
    }
    Ok(())
}

/// EXPLAIN ANALYZE of a write: what PostgreSQL's per-trigger lines (ExplainPrintTriggers over the result relations'
/// `ri_TrigInstrument`, D3381) need from the served run.
pub enum TriggerReport {
    /// No result relation fires a trigger: PostgreSQL prints no trigger line.
    None,
    /// One result relation, not partitioned: the statement level's counters (trigger crate `instr`) over it as the one
    /// opened result relation; an AFTER event on another relation (an RI action's SPI write) adds it as a trig-target one,
    /// as ExecGetTriggerResultRel does.
    Level(Vec<trigger::instr::StmtTrigRel>),
    /// Several result relations, a partitioned one or a SELECT's data-modifying CTE, with triggers: PostgreSQL's lines name
    /// the relations it opened or routed to, in its order, which the served write does not track (the reason).
    Refuse(&'static str),
}

/// The write's trigger report under `instrument_options` (the result relations as ExecInitModifyTable opens them).
pub fn trigger_report(pstmt: &PlannedStmt<'_>, instrument_options: i32) -> PgResult<TriggerReport> {
    if pstmt.resultRelations.iter().next().is_none() || !relcache_seams::relation_get_trigger_desc::is_installed() {
        return Ok(TriggerReport::None);
    }
    let mut first = None;
    let mut count = 0usize;
    let mut fires = false;
    for rti in pstmt.resultRelations.iter() {
        let Some(rte) = pstmt.rtable.iter().nth(rti as usize - 1).and_then(|n| n.as_range_tbl_entry()) else { continue };
        if rte.relid == 0 {
            continue;
        }
        let desc = relcache_seams::relation_get_trigger_desc::call(rte.relid as Oid)?;
        let partitioned = lsyscache::relation::get_rel_relkind(rte.relid as Oid)? as u8 == types_rel::RELKIND_PARTITIONED_TABLE;
        fires |= desc.is_some();
        if count == 0 {
            first = Some((rte.relid as Oid, desc, partitioned));
        }
        count += 1;
    }
    if !fires {
        return Ok(TriggerReport::None);
    }
    if pstmt.commandType == CmdType::CMD_SELECT {
        return Ok(TriggerReport::Refuse("entry: EXPLAIN ANALYZE of a SELECT whose data-modifying CTE fires triggers (its trigger lines)"));
    }
    let one = if count == 1 { first } else { None };
    match one {
        Some((relid, desc, false)) => {
            let ctx = gravity_driver::mem::root::Root::new("GravityTriggerReport"); // alloc: Cold
            let name = lsyscache::relation::get_rel_name(ctx.mcx(), relid)?.map(|n| n.to_string()).unwrap_or_default(); // alloc: Cold
            let rel = trigger::instr::StmtTrigRel::new(trigger::instr::TRIG_REL_OPENED, relid, &name, desc.as_deref(), instrument_options)?;
            Ok(TriggerReport::Level(vec![rel])) // alloc: Cold
        }
        Some(_) => Ok(TriggerReport::Refuse("entry: EXPLAIN ANALYZE of a write with triggers into a partitioned table (its routed relations' trigger lines)")),
        None => Ok(TriggerReport::Refuse("entry: EXPLAIN ANALYZE of a write with triggers over several result relations (their trigger lines in PostgreSQL's order)")),
    }
}
