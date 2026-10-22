//! What a CTE's plan shows the user when its reader pulls it ahead of PostgreSQL's reads (nodeCtescan.c CteScanNext pulls
//! the CTE's plan one tuple at a time): the ctescan family's sibling file of effects/visible.rs.
use gravity_decompose::effects::{Effects, Licence};
use gravity_decompose::graph::InitPlan;
use gravity_decompose::Op;

use crate::graph::Gx;
use crate::ops::ctescan::lower::feeds;

/// CTE `k`'s plan (InitPlan `i`) is quiet: its own effects neither act nor raise (decompose `Effects::of_init_plan`, the
/// InitPlan's raise-free and volatility facts: F5 raise-free, no VOLATILE call), and (V4R-512) no row lock, which never
/// takes rows past the reader (PG locks only the rows the CteScan pulls). A quiet reader pulls WINDOW rows at a time;
/// D1385's eager form runs a quiet plan whole.
pub(crate) fn quiet(gx: &Gx<'_, '_>, k: usize, i: &InitPlan<'_>) -> bool {
    let own = Effects::of_init_plan(i);
    !own.may_raise && !own.acts && !feeds(gx, k, |op| matches!(op, Op::Lock(_)))
}

/// D3097: CTE `k`'s plan (InitPlan `i`) neither acts nor locks rows, and its CteScan is the statement's first fetch
/// (decompose `first_cte`, D1934: `ops::first_fetched`, reached once before any user code runs, under no LIMIT 0 or
/// one-time qual): PostgreSQL pulls it at the statement's first row, as it would the same plan inlined, so its producers
/// run whole exactly where the inlined plan's would and an error one of them may raise is the inlined plan's too (a lazy
/// build stays lazy). `cte_whole_gate` takes it beside `quiet`: a decorrelated grouped join's arithmetic may raise. The
/// plan's own pipeline runs ahead of its readers only under `fills_ahead_quietly`.
pub(crate) fn first_fetch(gx: &Gx<'_, '_>, k: usize, i: &InitPlan<'_>) -> bool {
    let own = Effects::of_init_plan(i);
    !own.acts && gx.graph.first_cte.is_some_and(|(id, _)| id == i.plan_id) && !feeds(gx, k, |op| matches!(op, Op::Lock(_)))
}

/// D3097: CTE `k`'s own pipeline may be filled whole ahead of its readers (D3040's fill): `Some` with the licence it
/// runs under, `None` when it keeps the pulled form. A quiet plan holds `effects::quiet_ahead`'s licence over its own
/// effects (it neither acts nor raises; its rows may differ run to run, as a SampleScan's). A plan that may raise fills
/// ahead only at PostgreSQL's own point (`Some(None)`, no departure to license) by the same-row rule
/// (`same_row::at_pg_point`): every reader in `readers` raises and acts nothing (`reader_quiet`), so PostgreSQL's
/// interleaved pulls meet the plan's own error first either way; each reads its source whole (`whole`: the fill runs
/// only rows PostgreSQL's readers pull, ops/ctescan/lower.rs `fills_parallel`); and no reader streams to a named
/// cursor's FETCH (`row_first`, D3450), which would return rows before PostgreSQL's error. A reader that may raise (an
/// aggregate whose argument overflows at the first row while the plan divides by zero at the second) keeps the pulled
/// form.
pub(crate) fn fills_ahead_quietly(gx: &Gx<'_, '_>, k: usize, i: &InitPlan<'_>, readers: &[usize], whole: bool, row_first: bool) -> Option<Option<Licence>> {
    if quiet(gx, k, i) {
        return Some(gravity_decompose::effects::quiet_ahead(Effects::of_init_plan(i)));
    }
    let calm = readers.iter().all(|&p| reader_quiet(gx, p));
    super::same_row::at_pg_point(&super::same_row::SameRow { whole, acts_before: !calm, raises_before: !calm, row_first }).then_some(None)
}

/// Reader pipeline `p` raises on no row: its source (the CTE's store) and ops pass `span_quiet`, and it ends in the
/// client's sink or a hash build (its keys are ops `span_quiet` reads), not a grouping or a sort whose per-row calls (an
/// aggregate's arguments and transitions, a comparator) may raise between two of the plan's rows.
fn reader_quiet(gx: &Gx<'_, '_>, p: usize) -> bool {
    let Some(x) = gx.graph.pipelines.get(p) else { return false };
    crate::effects::span_quiet(gx, x, 0, x.ops.len(), false)
        && crate::frame::iface::breaker_of(&x.desc).is_none_or(|b| matches!(b.kind, gravity_abi::breaker::BreakerKind::HashBuild))
}

/// D3336 (A2 (iii), ops/ctescan/wholefill.rs): whether reader pipeline `p` of a CTE's store reads it in no order of its
/// own: its steps are filters, gates, projections, full sorts (no presorted prefix) and hash probes or a build, and it
/// ends in a breaker (a sort, a top, a fold, a hash build). Any other reader (a merge join over the CTE's pathkeys, a
/// streamed group, a window, an incremental sort) reads the rows in the store's order, which must then be PostgreSQL's:
/// the fill's own pipeline runs serial (`CteEnd`, ops/ctescan/lower.rs).
pub(crate) fn order_free(gx: &Gx<'_, '_>, p: usize) -> bool {
    use gravity_decompose::join::JoinDesc;
    let ps = gx.pipelines();
    p < ps.len()
        && crate::frame::iface::end_breaker(gx, p).is_some()
        && ps[p].ops.iter().all(|op| match op {
            Op::Filter(_) | Op::Gate(_) | Op::Project(_) => true,
            Op::Sort(d) => d.presorted == 0,
            Op::Join(JoinDesc::Probe { .. } | JoinDesc::Build { .. }) => true,
            _ => false,
        })
}
