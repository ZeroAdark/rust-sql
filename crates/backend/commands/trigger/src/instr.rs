//! EXPLAIN ANALYZE's per-trigger counts for a statement whose result relations no executor EState holds (a Gravity-served
//! write, D3381): C's `ri_TrigInstrument` arrays (InitResultRelInfo, execMain.c:1263-1274) and the three EState lists
//! ExplainPrintTriggers reads (explain.c:832-871), kept per statement level on this thread.
//!
//! A level opens at the statement's start, after its AfterTriggerBeginQuery, with its opened result relations
//! (`es_opened_result_relations`); it counts every trigger call made AT that level: the query depth it opened at and the
//! trigger depth it opened at. That is exactly the calls C brackets with the statement's own `ri_TrigInstrument`:
//! - BEFORE / INSTEAD OF row triggers and BEFORE STATEMENT triggers the statement's write fires (ExecCallTriggerFunc with
//!   its result relation's array, trigger.c:2318 etc.);
//! - AFTER events its AfterTriggerEndQuery fires (afterTriggerInvokeEvents' InstrStartNode / InstrStopNode on
//!   ExecGetTriggerResultRel's relation, trigger.c:4379 / :4599), including events an RI action's SPI write (run with
//!   fire_triggers off, so at this query level) queued for another relation: that relation is a trig-target one
//!   (`es_trig_target_relations`), first seen at its first event.
//!
//! A statement a trigger function runs is one query level deeper (its own AfterTriggerBeginQuery) or one trigger level
//! deeper (an RI action's SPI write runs inside the RI trigger's call), so its calls are not counted: C counts them, if at
//! all, in that statement's own EState. A level is taken (`take`) at the statement's finish, dropped (`drop_level`) at its
//! end, and every level a transaction or subtransaction abort leaves behind is dropped there (`at_xact_end`,
//! `at_subxact_abort`).
use types_core::instrument::Instrumentation;
use types_core::Oid;
use types_error::PgResult;
use types_rel::Relation;
use types_trigger::TriggerDesc;

/// `es_opened_result_relations` (C reports it first).
pub const TRIG_REL_OPENED: u8 = 0;
/// `es_tuple_routing_result_relations`.
pub const TRIG_REL_ROUTING: u8 = 1;
/// `es_trig_target_relations`: a relation an AFTER event of the level fired on that is neither of the others.
pub const TRIG_REL_TARGET: u8 = 2;

/// One relation's counters: C's ResultRelInfo `ri_TrigDesc` names and `ri_TrigInstrument` (one per trigger of the
/// descriptor, in its order; empty for a relation without triggers, which still counts for ExplainPrintTriggers'
/// show_relname rule).
pub struct StmtTrigRel {
    pub kind: u8,
    pub relid: Oid,
    pub relname: String,
    /// (tgname, tgoid, tgconstraint, instrumentation) per trigger of the descriptor.
    pub triggers: Vec<(String, Oid, Oid, Instrumentation)>,
}

impl StmtTrigRel {
    pub fn new(kind: u8, relid: Oid, relname: &str, desc: Option<&TriggerDesc<'_>>, options: i32) -> PgResult<Self> {
        let n = desc.map_or(0, |d| d.triggers.len());
        let mut triggers = Vec::new();
        triggers.try_reserve_exact(n).map_err(|_| mcx::oom_named("StmtTrigRel", n * core::mem::size_of::<(String, Oid, Oid, Instrumentation)>()))?;
        for t in desc.iter().flat_map(|d| d.triggers.iter()) {
            let mut i = Instrumentation::default();
            ::instrument::instr_init(&mut i, options);
            triggers.push((t.tgname.as_str().to_owned(), t.tgoid, t.tgconstraint, i));
        }
        Ok(StmtTrigRel { kind, relid, relname: relname.to_owned(), triggers })
    }
}

pub(crate) struct Level {
    token: u64,
    query_depth: i32,
    trigger_depth: i32,
    options: i32,
    rels: Vec<StmtTrigRel>,
}

use crate::queue::{STMT_LEVELS as LEVELS, STMT_LEVEL_TOKEN as NEXT_TOKEN};

/// Opens the statement's level at the current query and trigger depth, with its opened result relations (in C's
/// `es_opened_result_relations` order); its token for `take` / `drop_level`. A level left at this depth or deeper (a
/// statement that never finished) is dropped first.
pub fn begin(options: i32, opened: Vec<StmtTrigRel>) -> PgResult<u64> {
    let query_depth = crate::queue::query_depth();
    let trigger_depth = crate::exec::trigger_depth();
    let token = NEXT_TOKEN.with(|c| {
        let t = c.get();
        c.set(t + 1);
        t
    });
    LEVELS.with(|l| {
        let mut l = l.borrow_mut();
        l.retain(|x| x.query_depth < query_depth);
        l.try_reserve(1).map_err(|_| mcx::oom_named("trigger instr level", core::mem::size_of::<Level>()))?;
        l.push(Level { token, query_depth, trigger_depth, options, rels: opened });
        Ok(token)
    })
}

/// The level's relations with their counts (InstrEndLoop'd, as report_triggers reads them), the level closed; `None` when
/// it is gone (an abort dropped it).
pub fn take(token: u64) -> Option<Vec<StmtTrigRel>> {
    let mut rels = LEVELS.with(|l| {
        let mut l = l.borrow_mut();
        let i = l.iter().position(|x| x.token == token)?;
        Some(l.remove(i).rels)
    })?;
    for r in rels.iter_mut() {
        for t in r.triggers.iter_mut() {
            ::instrument::instr_end_loop(&mut t.3);
        }
    }
    Some(rels)
}

/// Closes the level if it is still open (a statement that ended without its finish).
pub fn drop_level(token: u64) {
    LEVELS.with(|l| l.borrow_mut().retain(|x| x.token != token));
}

/// AfterTriggerEndXact: no statement level outlives its transaction.
pub(crate) fn at_xact_end() {
    LEVELS.with(|l| l.borrow_mut().clear());
}

/// AfterTriggerEndSubXact on abort, the query depth restored to `query_depth`: the levels the subtransaction opened go.
pub(crate) fn at_subxact_abort(query_depth: i32) {
    LEVELS.with(|l| l.borrow_mut().retain(|x| x.query_depth <= query_depth));
}

/// Whether a call made now is the innermost open level's (its query and trigger depth).
pub(crate) fn active() -> bool {
    LEVELS.with(|l| {
        let l = l.borrow();
        l.last().is_some_and(|x| x.query_depth == crate::queue::query_depth() && x.trigger_depth == crate::exec::trigger_depth())
    })
}

/// InstrStartNode on trigger `tgoid` of `rel` in the active level (ExecGetTriggerResultRel's relation: an opened one, else
/// a trig-target one made now with the descriptor `desc`, or the relcache's when none is given); false when no level is
/// active or the trigger is not in the relation's descriptor.
pub(crate) fn start(rel: &Relation<'_>, desc: Option<&TriggerDesc<'_>>, tgoid: Oid) -> PgResult<bool> {
    if !active() {
        return Ok(false);
    }
    let relid = rel.rd_id;
    let known = LEVELS.with(|l| l.borrow().last().is_some_and(|x| x.rels.iter().any(|r| r.relid == relid)));
    if !known {
        let options = LEVELS.with(|l| l.borrow().last().map_or(0, |x| x.options));
        let fetched;
        let desc = match desc {
            Some(d) => Some(d),
            None => {
                fetched = relcache::RelationGetTriggerDesc(relid)?;
                fetched.as_deref()
            }
        };
        let r = StmtTrigRel::new(TRIG_REL_TARGET, relid, rel.name(), desc, options)?;
        LEVELS.with(|l| -> PgResult<()> {
            let mut l = l.borrow_mut();
            let x = l.last_mut().expect("an active level");
            x.rels.try_reserve(1).map_err(|_| mcx::oom_named("trigger instr relation", core::mem::size_of::<StmtTrigRel>()))?;
            x.rels.push(r);
            Ok(())
        })?;
    }
    Ok(LEVELS.with(|l| {
        let mut l = l.borrow_mut();
        let Some(i) = l.last_mut().and_then(|x| x.rels.iter_mut().find(|r| r.relid == relid)).and_then(|r| r.triggers.iter_mut().find(|t| t.1 == tgoid)) else {
            return false;
        };
        // a level a failed call left started (its statement errored and a later one at this depth reads it before the
        // abort dropped it) is restarted, never a second InstrStartNode
        if !i.3.starttime.is_zero() {
            i.3.starttime = Default::default();
        }
        ::instrument::instr_start_node(&mut i.3);
        true
    }))
}

/// InstrStopNode(1) on the trigger `start` started.
pub(crate) fn stop(relid: Oid, tgoid: Oid) {
    LEVELS.with(|l| {
        let mut l = l.borrow_mut();
        if let Some(i) = l.last_mut().and_then(|x| x.rels.iter_mut().find(|r| r.relid == relid)).and_then(|r| r.triggers.iter_mut().find(|t| t.1 == tgoid)) {
            ::instrument::instr_stop_node(&mut i.3, 1.0);
        }
    });
}

/// The after-trigger loop's sink over the active level (AfterTriggerEndQuery of a statement no EState holds).
pub(crate) struct LevelSink {
    started: Option<(Oid, Oid)>,
}

impl LevelSink {
    pub(crate) fn new() -> Self {
        LevelSink { started: None }
    }
}

impl crate::queue::AfterTriggerInstrSink for LevelSink {
    fn start(&mut self, relid: Oid, rel: &Relation<'_>, trigdesc: &TriggerDesc<'static>, tgindx: usize) -> PgResult<()> {
        self.started = None;
        let Some(t) = trigdesc.triggers.get(tgindx) else { return Ok(()) };
        debug_assert_eq!(relid, rel.rd_id);
        if start(rel, Some(trigdesc), t.tgoid)? {
            self.started = Some((relid, t.tgoid));
        }
        Ok(())
    }

    fn stop(&mut self, relid: Oid, _tgindx: usize) {
        if let Some((r, tgoid)) = self.started.take() {
            debug_assert_eq!(r, relid);
            stop(r, tgoid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_level_is_its_statements_query_and_trigger_depth() {
        crate::queue::AfterTriggerBeginQuery();
        let t = begin(0, Vec::new()).unwrap();
        assert!(active());
        {
            let _call = crate::exec::TriggerDepthGuard::enter();
            assert!(!active());
        }
        crate::queue::AfterTriggerBeginQuery();
        assert!(!active());
        crate::queue::AfterTriggerEndQuery(None, &[]).unwrap();
        assert!(active());
        assert!(take(t).is_some_and(|r| r.is_empty()));
        assert!(!active());
        crate::queue::AfterTriggerEndQuery(None, &[]).unwrap();
    }

    /// A subtransaction's abort drops the levels its statements opened; a new statement at a level's depth replaces a level
    /// a failed statement left there.
    #[test]
    fn an_abort_drops_the_levels_it_opened() {
        crate::queue::AfterTriggerBeginQuery();
        let outer = begin(0, Vec::new()).unwrap();
        crate::queue::AfterTriggerBeginQuery();
        let inner = begin(0, Vec::new()).unwrap();
        let again = begin(0, Vec::new()).unwrap();
        assert!(take(inner).is_none());
        at_subxact_abort(crate::queue::query_depth() - 1);
        assert!(take(again).is_none());
        assert!(take(outer).is_some());
        at_xact_end();
    }
}
