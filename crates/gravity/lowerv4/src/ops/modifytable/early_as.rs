//! D2605 EARLY (modifytable, regress triggers#499: `WITH ins (a) AS (INSERT INTO parted2_stmt_trig VALUES (1), (2)
//! RETURNING a) INSERT INTO parted_stmt_trig SELECT a FROM ins RETURNING tableoid::regclass, a`): PostgreSQL queues a
//! data-modifying CTE's AFTER STATEMENT events when its ModifyTable ends (fireASTriggers once its last ExecModifyTable call
//! returns no row), which is when the CteScan reading it reaches its end, and the statement's own when the statement's
//! ModifyTable ends, later. The served writers bind in PostgreSQL's BEFORE STATEMENT order (the statement's own, then the
//! CTE's: driver node/mod.rs), but hosts trigger_stmt.rs queues every AFTER STATEMENT event at the statement's end in
//! that order, the statement's own first, so rowop.rs `trigger_order_kept` refuses by D2605's name.
//!
//! `declare` serves the form in which the CTE's end is the end of the statement's own write pipeline: the CTE's product
//! is read only by the CteScan that is that pipeline's source, and the pipeline only filters and projects its rows into
//! the write (no join, gate, limit, union, sort, set function or sub-plan op, so no own row is written after the CTE's
//! end and the pipeline never stops before it). It declares `ProtoKind::Transition` on that pipeline with the EARLY_AS
//! bit: exec ops/modifytable/early_as.rs names the CTE writers' relations and hosts trigger_stmt.rs `fire_as` queues
//! their AFTER STATEMENT events first at the statement's end (nothing is queued between the pipeline's end and the
//! statement's), after every own row's AFTER ROW events and before the statement's own AFTER STATEMENT events.
//!
//! Fail closed (`shape` is `None`, and D2605's refusal stays): exactly one CTE write, with RETURNING, and one write
//! pipeline outside every CTE; every InitPlan a CTE's link; the CTE's product read by that pipeline alone, which reads
//! nothing else; the CTE's targets (every result relation, its route's root and the leaves as admitted) fire no
//! row-level trigger of its command (BEFORE / AFTER / INSTEAD OF, internal or deferrable ones too), with known facts, so
//! its rows queue no event beside the statement's own rows'.

use gravity_decompose::ops::modifytable::trig_facts::{cmd_bits, TrigExt};
use gravity_decompose::ops::shared::modifytable::WriteDesc;
use gravity_decompose::{Op, Source};
use gravity_program::protos::{Proto, ProtoKind};
use gravity_program::Cmd;
use mcx::{CollectIn, PgVec};

use crate::frame::px::Px;
use crate::graph::Gx;

/// The Transition proto's `arg` bit (exec ops/modifytable/early_as.rs `EARLY_AS`): its pipeline is the statement's own
/// write pipeline, at whose end the CTE writers' AFTER STATEMENT events queue.
const EARLY_AS: u64 = 1;

/// The statement's own write pipeline, when the CTE's end is that pipeline's (module doc); `None` otherwise.
pub(crate) fn shape(gx: &Gx<'_, '_>) -> Option<usize> {
    let ps = gx.pipelines();
    let n = ps.len();
    let write = |k: usize| ps[k].ops.iter().find_map(|o| if let Op::Write(w) = o { Some(w) } else { None }).filter(|w| w.cmd != Cmd::Select);
    if !gx.graph.init_plans.iter().all(|i| gx.cte_write(i.pipeline as usize)) {
        return None;
    }
    let ctes: PgVec<'_, usize> = (0..n).filter(|&k| gx.cte_write(k) && write(k).is_some()).collect_in(gx.mcx);
    let own: PgVec<'_, usize> = (0..n).filter(|&k| !gx.cte_write(k) && write(k).is_some()).collect_in(gx.mcx);
    let (&[c], &[r]) = (&ctes[..], &own[..]) else { return None };
    let cw = write(c)?;
    if cw.returning.is_none() || ps[c].ops.iter().any(crate::graph::is_arm) || ps[r].union.is_some() {
        return None;
    }
    // the CTE's product: read by the own write pipeline alone, as its source, which reads nothing else
    let s = ps[c].desc.produces()?;
    let readers = (0..n).filter(|&m| m != c && ps[m].desc.consumes.contains(&s));
    if !readers.eq(core::iter::once(r)) || !matches!(ps[r].source, Source::Breaker { slot } if slot == s) || ps[r].desc.consumes.iter().any(|&x| x != s) {
        return None;
    }
    // every own row written before the CTE's end, and none after it
    let streams = ps[r].ops.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_) | Op::Write(_)));
    let one_write = ps[r].ops.iter().filter(|o| matches!(o, Op::Write(_))).count() == 1;
    (streams && one_write && rowless(gx, cw)).then_some(r)
}

/// Write `w`'s targets fire no row-level trigger of its command, with known facts (module doc).
fn rowless(gx: &Gx<'_, '_>, w: &WriteDesc<'_>) -> bool {
    let bits = cmd_bits(w.cmd);
    let quiet = |t: TrigExt| t.known && (t.before_row | t.after_row | t.instead_row | t.internal | t.deferrable) & bits == 0;
    let rels = core::iter::once(w.plan.target_rel).chain(w.targets.iter().map(|t| t.relid)).chain(w.route.as_ref().map(|r| r.root_relid));
    let leaves = w.route.is_none() || (w.leaves_known && w.leaves.iter().all(|l| quiet(l.trig)));
    leaves && rels.map(|r| gx.facts.trig_ext(r)).all(quiet)
}

/// rowop.rs, where `trigger_order_kept` refuses (a writing CTE's claim beside the statement's own write): when the CTE's
/// end is the own write pipeline's, its EARLY_AS Transition proto declared (once) and `true`; else `false` (D2605's
/// refusal stays).
pub(crate) fn declare(px: &mut Px<'_, '_>) -> bool {
    let Some(r) = shape(px.gx) else { return false };
    if !px.protos.iter().any(|q| q.kind == ProtoKind::Transition && q.arg & EARLY_AS != 0) {
        px.declare_proto(Proto::new(ProtoKind::Transition, r as u16).arg(EARLY_AS));
    }
    true
}
