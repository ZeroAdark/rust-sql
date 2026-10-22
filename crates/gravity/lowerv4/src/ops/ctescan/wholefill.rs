//! D3336 (A2 (iii)), road `cte.wholefill` (default off): a pulled CTE that every reader reads whole, filled whole ahead of
//! its readers though its plan may raise, and its sorting readers reading the sealed store by record claims.
//!
//! D3040's fill (lower.rs `fills_parallel`) runs a CTE's plan whole in program order at its pipelines' widths into an
//! owned spool, sealed once (D4628's seal under `spool.seal`), never pulled. A plan that may raise takes it today only at
//! PostgreSQL's own point (`effects::visible::ctescan::fills_ahead_quietly`): its CteScan the statement's first fetch and
//! every reader raising nothing (a sink or a hash build over quiet ops), since PostgreSQL's CteScan pulls the plan one row
//! per miss, interleaving the plan's rows with its readers' work, and the first error of the two is the statement's. A
//! reader that sorts (a filter's division, a comparator) or a CTE that is not the first fetch keeps the pull, which runs
//! the whole plan nested in the first reader's call on the leader.
//!
//! Under this road such a plan is filled as D3040's when it neither acts nor locks (no VOLATILE call by the InitPlan's
//! effects, no FOR UPDATE, the statement acting nowhere), every reader reads its source whole (no LIMIT bound, no gate,
//! no named cursor's stream, no LIMIT step: lower.rs `fills_alone`, `stops_early`), and the CTE's own pipeline runs
//! whole before any row can leave the statement (`Px::ahead_of_rows`: every reader comes before the statement's first
//! emitting pipeline, none in a rerun set or run on demand), its plan calling no user code (`acting_side`, D3587's
//! hidden calls). The fill then holds v5 errorhold's licence (`Px::run_ahead`, `Departure::Ahead`): an error raised
//! before a row left the statement, the plan's or a reader's, re-runs the statement in plan order (exec
//! `runner::recheck`), where every licence is denied, the CTE is pulled as before and PostgreSQL's first error (or
//! none, when no reader ever pulls the plan) is the statement's. op 14's held-at-len form (rerun/spool/form.rs `held`)
//! cannot answer here: the owned store's records lie in owner order (D4628), not PostgreSQL's pull order, so no record
//! count marks where PostgreSQL's reader meets the plan's error. A reader whose read stops where its merge join's other
//! input ends (a merge join over the CTE's pathkeys) is exact the same way: an error the fill met past that point
//! re-runs the statement in plan order. A reader that reads the store in its order (effects::visible::ctescan
//! `order_free` false) gets PostgreSQL's order: the fill's own pipeline then runs serial.
//!
//! The readers (lower.rs `make_source`): a reader whose end is a full sort (`runs.sort`, `ops/sort/lower.rs
//! wide_row_end`) or a merge join's input sort (`face/slot_run.rs emit_fill`'s exclusions), through filters, gates and
//! projections, whose sort no reader folds by its ties' order and which makes no visible call
//! (`effects::visible::wide_visible` / `calls_visible`), reads the sealed store by record claims (`cte.claims`'s
//! `nest.spool.chunks`) as the order-free readers do: each claim fills its own run, sealed at its section end, and the
//! sort's reader merges the runs (`Px::wide`, as over a wide group emit). The merge joins reading those runs stay merges.
//! A top-k end stays serial (R-KEEP rnd-3914: a top's serial run under claims). Owner M8-CTE.
use gravity_abi::breaker::BreakerKind;
use gravity_decompose::effects::Effects;
use gravity_decompose::graph::InitPlan;
use gravity_decompose::ops::shared::subplan::SubLink;
use gravity_decompose::Op;

use crate::face::kinds::KindId;
use crate::frame::px::Px;
use crate::graph::Gx;

crate::strategies! {
    /// D3336 (A2 (iii)): a CTE every reader reads whole, filled ahead under v5 errorhold's licence though its plan may
    /// raise, and its sorting readers reading the sealed store by record claims.
    CTE_WHOLEFILL = 161, "cte.wholefill", Cte; // a3-strategy-bits.tsv row 161
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(CTE_WHOLEFILL, &())], ..crate::ops::NONE };

/// The Shape of a whole fill whose licence the lowering was refused after `licensable` granted it (the two read the
/// same facts: a refusal by name, never a fill without its licence).
pub(crate) const UNLICENSED: &str = "a CTE filled whole ahead under cte.wholefill whose run-ahead licence was denied (D3336 A2 (iii))";

/// The road on for this statement (default off: road_off.rs `DEFAULT_OFF`).
pub(crate) fn road_on(px: &Px<'_, '_>) -> bool {
    px.strategies.has(CTE_WHOLEFILL) && crate::face::source::on(crate::admit::refusal::Variant::Strategy(CTE_WHOLEFILL))
}

fn init_plan<'g, 'p>(gx: &Gx<'g, 'p>, k: usize) -> Option<&'g InitPlan<'p>> {
    gx.graph.init_plans.iter().find(|i| i.pipeline as usize == k && matches!(i.link, SubLink::Cte))
}

/// Whether CTE `k`'s plan, which may raise, may run whole ahead of its readers under v5 errorhold's licence: the road is
/// on, the plan may raise but neither acts (no VOLATILE call by the InitPlan's effects) nor locks rows, the statement
/// acts nowhere and the lowering is not the plan-order run's (`denying`), its pipeline runs whole before any row can leave
/// the statement (`Px::ahead_of_rows`) and neither it nor what feeds it calls user code (`Px::run_ahead`'s tests, read
/// here without granting). A quiet plan is D3040's own (`false`).
pub(crate) fn licensable(px: &Px<'_, '_>, k: usize) -> bool {
    let gx = px.gx;
    if !road_on(px) {
        return false;
    }
    let Some(i) = init_plan(gx, k) else { return false };
    let own = Effects::of_init_plan(i);
    let lock = || crate::ops::ctescan::lower::feeds(gx, k, |op| matches!(op, Op::Lock(_)));
    let hides = || crate::effects::hides_user_code(gx, k) && crate::effects::statement_user_code(gx, px.arena);
    own.may_raise
        && !own.acts
        && !lock()
        && !gx.graph.acts
        && !gravity_decompose::effects::denying()
        && px.ahead_of_rows(k)
        && !crate::effects::visible::hashjoin::acting_user_fn(gx, k)
        && !hides()
}

/// Whether reader pipeline `p` of a CTE's store may stop its read at a count (a LIMIT step, `frame::iface::limit_bounds`):
/// PostgreSQL then pulls the plan only as far as the count takes it, and the whole fill would run the rest for nothing (the
/// ruling's eligibility: such a reader keeps the pull). An embedded inner (past the graph's pipelines) always does.
pub(crate) fn stops_early(gx: &Gx<'_, '_>, p: usize) -> bool {
    let ps = gx.pipelines();
    p >= ps.len() || ps[p].ops.iter().any(|op| crate::frame::iface::limit_bounds(op).is_some())
}

/// The licence of CTE `k`'s whole fill (`licensable` held when the fill was chosen): its pipeline runs ahead under
/// `Departure::Ahead` (`Px::run_ahead`: the recheck that re-runs the statement in plan order on an error before a row
/// left it), held on what feeds it too. Refused by name (`UNLICENSED`) if the grant is denied.
pub(crate) fn license(px: &mut Px<'_, '_>, k: usize, node: i32) {
    if px.run_ahead(k, Effects::RAISES).is_none() {
        crate::refuse!(node, crate::admit::refusal::Reason::Shape { why: crate::admit::refusal::ShapeWhy(UNLICENSED) });
    }
    fires::fire("lower.cte.wholefill"); // shapecheck fire signal (D3336 A2 (iii)): a CTE that may raise filled whole ahead
}

/// Whether reader `k` of a sealed store (lower.rs `make_source`, its other tests passed) reads it by record claims into a
/// sort's runs: the road is on, its ops are filters, gates and projections, and its end is a full row sort over no
/// presorted prefix and no union's arms (`wide_row_end`), its run holding rows (`run_holds_rows`), whose run no reader
/// folds by its ties' order and whose pipeline makes no visible call or first-row error (`wide_visible`), or a merge join's
/// input sort its claims may fill (`slot_run::emit_fill`: not in a rerun set, an input that acts nowhere, the merge's roads
/// on) making no visible call.
pub(crate) fn sort_reader(px: &Px<'_, '_>, k: usize) -> bool {
    let gx = px.gx;
    if !road_on(px) || crate::frame::iface::end_breaker(gx, k) != Some(BreakerKind::Sort) {
        return false;
    }
    let plain = gx.pipelines()[k].ops.iter().all(|op| matches!(op, Op::Filter(_) | Op::Gate(_) | Op::Project(_) | Op::Sort(_)));
    let full = gx.end_need(k) == KindId::RunsSort && crate::frame::iface::run_holds_rows(gx, k) && crate::frame::iface::wide_row_end(gx, k) && crate::effects::visible::wide_visible(gx, k).is_none();
    let merge = gx.end_need(k) == KindId::MergeJoin && crate::face::slot_run::emit_fill(px, k) && crate::effects::visible::calls_visible(gx, k).is_none();
    plain && (full || merge)
}
