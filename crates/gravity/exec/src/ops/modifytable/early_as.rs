//! D2605 EARLY (modifytable, regress triggers#499; lowerv4 ops/modifytable/early_as.rs): a data-modifying CTE whose end
//! is the end of the statement's own write pipeline (its CteScan that pipeline's source) queues its AFTER STATEMENT
//! events then, as PostgreSQL's ModifyTable of the CTE does when the CteScan reads past its last row: after the own rows'
//! AFTER ROW events, before the statement's own AFTER STATEMENT events. Nothing is queued between that pipeline's end
//! and the statement's, so the statement's end queues the CTE writers' events first (hosts trigger_stmt.rs `early`, then
//! `fire_as`). The lowerer declares the form as the Transition proto's EARLY_AS bit; proto.rs `Transition`'s prepare
//! (once per execution, inside the statement's query level) names the relations here.
use gravity_program::protos::Proto;
use gravity_program::WritePlan;
use types_core::Oid;

/// The Transition proto's `arg` bit (lowerv4 ops/modifytable/early_as.rs `EARLY_AS`).
const EARLY_AS: u64 = 1;

/// When `p` carries the EARLY_AS bit: the relations whose statement triggers the CTE writers bound (a routed write's
/// root), but the statement's own (PostgreSQL keeps one AFTER STATEMENT event per relation and event, the later one: it
/// stays at the statement's end), named for this query level's `fire_as`.
/// The list dies here (the hosts' `early` copies it): it is on the prepare's owner.
pub fn prepare(p: &Proto, cx: &crate::ops::Prepare<'_>) {
    if p.arg & EARLY_AS == 0 {
        return;
    }
    let program = cx.program;
    let root = |w: &WritePlan| w.route.as_ref().map_or(w.target_rel, |r| r.root) as Oid;
    let own = root(&program.write);
    let mut rels: memledger::ChargedVec<Oid> = cx.charged_vec();
    rels.with_vec(|v| v.extend(program.writes.iter().filter(|w| w.plan.cte).map(|w| root(&w.plan)).filter(|&r| r != own))); // alloc: Exec charged cx.owner
    gravity_hosts::ops::modifytable::trigger_stmt::early(&rels);
}
