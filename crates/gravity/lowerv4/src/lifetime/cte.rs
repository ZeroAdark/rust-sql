//! ctescan's lifetime decisions, which the family asks here: whether a CTE a rescan re-runs takes a form whose re-run PG's
//! rescan shows the same, and which once-run producers keep their rows whole for the statement.
//! PG's CteScan rescan rewinds its read pointer, and a CTE whose plan reads a parameter the rescan changes has its
//! tuplestore cleared and its plan re-run lazily, by the first reader that pulls past the stored rows (ExecReScanCteScan:
//! the leader's chgParam). Lowered, a CTE a rerun set re-runs is a member of that set: served eagerly, ahead of its
//! readers, when its plan is quiet and only later members of the set read it (ctescan lower.rs `eager`, D1385), or
//! re-armed before its readers' next pull (`rearms`); any other member refuses by RERUN_CTE (`rerun_refused`). A rerun set
//! inside the CTE's plan that reads a once-run producer's rows at every rerun needs those rows kept whole (`kept_whole`,
//! D2081), else ONCE_RUN_NOT_WHOLE.
use gravity_abi::breaker::{BreakerKind, Spill};
use gravity_decompose::graph::InitPlan;
use gravity_decompose::ops::shared::setop::RECURSIVE_SET;
use mcx::{CollectIn, PgVec};

use crate::frame::iface::breaker_of;
use crate::graph::Gx;
use crate::ops::ctescan::lower;

pub(crate) const RERUN_CTE: &str = "a CTE a rescan re-runs whose plan can raise, calls a VOLATILE function or locks rows, or whose store a reader other than a later pipeline of the rescanned set reads (an embedded inner: an EXISTS's; its plan reads a parameter the rescan changes; PG clears and re-runs it lazily; D1385)";
pub(crate) const ONCE_RUN_NOT_WHOLE: &str = "a CTE whose rerun set reads a once-run producer's rows the pulls do not keep whole (an owner-folded hash table, a spilling breaker; D2081)";

/// CTE `k` (InitPlan `i`) whose cte face is made: the name it refuses by when a rescan re-runs it in no form PG's rescan
/// shows the same (a member of a rerun set neither eager nor re-armed: RERUN_CTE), or when a rerun set in its plan reads
/// a once-run producer not kept whole (ONCE_RUN_NOT_WHOLE).
pub(crate) fn rerun_refused(gx: &Gx<'_, '_>, k: usize, i: &InitPlan<'_>) -> Option<&'static str> {
    if gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(k as u16))) && lower::eager(gx, k, i).is_none() && !lower::rearms(gx, k) {
        return Some(RERUN_CTE);
    }
    (reads_once_run(gx, k) == Some(false)).then_some(ONCE_RUN_NOT_WHOLE)
}

/// D2081 (fail closed): whether once-run pipeline `p`'s rows stay whole in its own slot region for the statement, so a
/// rerun set's member may read them at every rerun (the driver keeps the slot, D1431): a Hash build (a rerun set's builds
/// keep named growth), a plain aggregate, a Sort that cannot spill. Not a width-1 owner fold (HashAgg, Distinct): it pins
/// its table on the pull's nested lane and the pull's end hands the pin back, so a later pull's rerun reads a table the
/// reader's arena reused (review r1 S1: a SIGSEGV, or 5,330 for PG's 10,000); not a spilling breaker (its stream ends at
/// its last consumer's first run: 'read outside its life'); nothing else no probe showed whole (TopK, Materialize).
/// Another CTE's pipeline is kept whole when no rerun set re-runs it: it ends in that CTE's store, which its pulls append
/// to and never reset while the statement runs (spool_rec's Rec), as pipeline `k`'s own (`reads_once_run`).
pub(crate) fn kept_whole(gx: &Gx<'_, '_>, p: usize) -> bool {
    if lower::is_cte(gx, p) && !gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(p as u16))) {
        return true;
    }
    match gx.graph.pipelines.get(p).and_then(|x| breaker_of(&x.desc)) {
        Some(br) => matches!(br.kind, BreakerKind::HashBuild | BreakerKind::PlainAgg) || (br.kind == BreakerKind::Sort && br.spill == Spill::None),
        None => false,
    }
}

/// What a rerun set in CTE `k`'s plan (a nest's inner, a correlated SubPlan) reads of a once-run producer of the CTE's
/// pulled set outside the set (the D1431 shape): nothing (`None`), only producers kept whole (`Some(true)`), or one that is
/// not (`Some(false)`, D2081). A recursive union's term set is setop.recursive's to check. Pipeline `k` itself is no such
/// producer: it ends in the CTE's store, which the pulls keep (spool_rec's Rec), so a rerun set that reads the CTE (a
/// correlated SubPlan over it) is trunk's served CT-2 reader.
fn reads_once_run(gx: &Gx<'_, '_>, k: usize) -> Option<bool> {
    let body: PgVec<'_, usize> = lower::fed(gx, k).into_iter().filter(|&p| p != k).collect_in(gx.mcx);
    let mut once = None;
    for r in gx.graph.reruns.iter().filter(|r| r.plan_id != RECURSIVE_SET) {
        for p in r.pipelines.iter().flat_map(|&m| crate::ops::shared::cte_reads::reads(gx, m as usize)).filter(|&p| body.contains(&p) && !r.pipelines.contains(&(p as u16))) {
            if !kept_whole(gx, p) {
                return Some(false);
            }
            once = Some(true);
        }
    }
    once
}
