//! Gravity width costing's planner side (D3164, per-pipeline measured costing): each
//! level's state before its paths are made (`level_setup`), the comparison of a statement's alternative forms
//! (`cheaper`), and the trace of the chosen path's predicted pipelines (`trace`). The model is pathnode gwcost.rs.
//!
//! `pgrust.gravity_width_costing` (bool, default off): on, a level whose Gravity width is above one compares paths by their
//! width costs. Off (and at width 0 or 1, at a nested level, for a write, a lock, SERIALIZABLE, a temporary relation or a
//! CTE scan) every comparison is PostgreSQL's, bit for bit. `pgrust.gravity_width_costing_trace` (bool, default off): on, the planner
//! LOGs the chosen path's predicted pipelines, for the census's drift check against the lowered program's descriptors.

use types_error::PgResult;
use types_nodes::nodes_enums::CmdType;
use types_pathnodes::PathId;

use crate::run::PlannerRun;

const FLOAT4OID: u32 = 700;
const FLOAT8OID: u32 = 701;

/// A boolean `pgrust.*` placeholder GUC: on for on / true / yes / 1 (any case, untrimmed: the knob's words), off when never
/// set (or no GUC boot: a unit-test binary). Read as a knob: per statement, not per name lookup.
fn guc_on(knob: guc_seams::Knob) -> bool {
    if !guc_seams::knob_read::is_installed() {
        return false;
    }
    matches!(guc_seams::knob_read::call(knob), guc_seams::KnobRead::Word(_))
}

pub(crate) fn level_setup(run: &mut PlannerRun<'_>) -> PgResult<()> {
    let parse = run.parse();
    let select = parse.commandType == CmdType::CMD_SELECT && parse.rowMarks.is_nil() && !parse.hasModifyingCTE;
    let mut width = 0u16;
    if guc_on(guc_seams::Knob::GravityWidthCosting) {
        let w = crate::gravity_paths::gravity_width(run);
        let serializable = xact_seams::isolation_is_serializable::is_installed() && xact_seams::isolation_is_serializable::call();
        if w > 1 && select && !serializable && !any_temp(run)? && !any_cte(run) {
            width = w.min(u16::MAX as u32) as u16;
        }
    }
    let free_aggs = free_aggs(run)?;
    let parse = run.parse();
    let end_hint = if parse.hasAggs || !parse.groupClause.is_nil() || !parse.groupingSets.is_nil() || !parse.distinctClause.is_nil() {
        1
    } else if !parse.sortClause.is_nil() || parse.hasWindowFuncs {
        2
    } else {
        0
    };
    let g = &mut run.root.gravity_width_cost;
    g.width = width;
    g.free_aggs = free_aggs;
    g.morsel = select;
    g.end_hint = end_hint;
    g.memo.borrow_mut().clear();
    if width > 1 {
        fires::fire("planner.width_cost.on"); // shapecheck fire signal: a level compared width costs
    }
    Ok(())
}

fn any_cte(run: &PlannerRun<'_>) -> bool {
    let planned = |r: &types_pathnodes::PlannerInfo<'_>| r.cte_plan_ids.iter().any(|&id| id >= 0);
    planned(&run.root) || run.suspended_roots.iter().any(|s| planned(&s.root)) || run.root.simple_rel_array.iter().flatten().any(|&rel| run.root.rel(rel).rtekind == types_pathnodes::RTE_CTE)
}

fn any_temp(run: &PlannerRun<'_>) -> PgResult<bool> {
    for &rel in run.root.simple_rel_array.iter().flatten() {
        let r = run.root.rel(rel);
        if r.rtekind != types_pathnodes::RTE_RELATION || r.relid == 0 {
            continue;
        }
        let rte = run.rte(r.relid as usize);
        if rte.relid != 0 && lsyscache::get_rel_persistence(rte.relid)? == b't' as i8 {
            return Ok(true);
        }
    }
    Ok(false)
}

fn free_aggs(run: &PlannerRun<'_>) -> PgResult<bool> {
    for &info in run.root.agginfos.iter() {
        for &a in run.root.agg_info(info).aggrefs.iter() {
            let Some(r) = run.root.expr_node(a).as_aggref() else { return Ok(false) };
            if !r.aggorder.is_nil() || !r.aggdistinct.is_nil() || !r.aggdirectargs.is_nil() || r.aggtype == FLOAT4OID || r.aggtype == FLOAT8OID {
                return Ok(false);
            }
            if !crate::gravity_paths::order_insensitive_agg(r.aggfnoid)? {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// Whether path `a` of run `ra` is cheaper than path `b` of run `rb` (two forms of one statement: decorrelated, unnested, a
/// grouping-sets union) at `fraction`: `compare_fractional_path_costs` over each run's width costs (PostgreSQL's where the
/// run does not compare them).
pub(crate) fn cheaper(ra: &PlannerRun<'_>, a: PathId, rb: &PlannerRun<'_>, b: PathId, fraction: f64) -> bool {
    if !crate::pathnode::gwcost::active(ra) && !crate::pathnode::gwcost::active(rb) {
        return crate::pathnode::compare_fractional_path_costs(ra.root.path(a).base(), rb.root.path(b).base(), fraction) < 0;
    }
    let (pa, pb) = (ra.root.path(a).base(), rb.root.path(b).base());
    if pa.disabled_nodes != pb.disabled_nodes {
        return pa.disabled_nodes < pb.disabled_nodes;
    }
    let ((sa, ta), (sb, tb)) = (crate::pathnode::gwcost::costs(ra, a), crate::pathnode::gwcost::costs(rb, b));
    if fraction <= 0.0 || fraction >= 1.0 {
        return ta < tb || (ta == tb && sa < sb);
    }
    let ca = fp_contract::fmadd(fraction, ta - sa, sa);
    let cb = fp_contract::fmadd(fraction, tb - sb, sb);
    ca < cb
}

pub(crate) fn trace(run: &PlannerRun<'_>, best: PathId) -> PgResult<()> {
    if !guc_on(guc_seams::Knob::GravityWidthCostingTrace) {
        return Ok(());
    }
    let w = run.root.gravity_width_cost.width;
    let pg = run.root.path(best).base().total_cost;
    let (pipes, g) = if w > 1 { crate::pathnode::gwcost::trace(run, best) } else { (Vec::new(), pg) }; // alloc: Cold
    let mut text = format!("gravity: width-cost W={w} pg={pg:.2} g={g:.2} pipes="); // alloc: Cold
    for p in pipes.iter() {
        let verdict = match p.serial {
            None => "E".to_string(), // alloc: Cold
            Some(r) => format!("S:{r:?}"), // alloc: Cold
        };
        text.push_str(&format!("{}:{}:{:?}:{}:{:.2}:{:.2};", p.oid, verdict, p.end, p.w, p.cost, p.s)); // alloc: Cold
    }
    elog_seams::ereport_msg::call(types_error::LOG, text, None)
}
