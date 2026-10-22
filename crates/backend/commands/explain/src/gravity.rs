//! pgrust-only: the `Gravity` block of a served statement's EXPLAIN ANALYZE (docs/gravity/entry.md §4), printed after the plan tree from the run's record (the query_desc_gravity_report seam). Times
//! print only under TIMING, as PostgreSQL's per-node times do; every count is the run's own.
use types_core::instrument::GravityReport;
use types_error::PgResult;
use types_nodes::plannodes::PlannedStmt;
use types_nodes::Node;

use crate::format::{append, ExplainCloseGroup, ExplainIndentText, ExplainOpenGroup, ExplainPropertyBool, ExplainPropertyFloat, ExplainPropertyList, ExplainPropertyText, ExplainPropertyUInteger};
use crate::state::{ExplainState, EXPLAIN_FORMAT_TEXT};

fn ms(ns: u64) -> f64 {
    ns as f64 / 1e6
}

/// The block, when the statement was served (`es.qd` live, ANALYZE).
pub fn ExplainPrintGravity(es: &mut ExplainState<'_>) -> PgResult<()> {
    if !es.analyze || es.qd.is_null() {
        return Ok(());
    }
    let Some(r) = execmain_seams::query_desc_gravity_report::call(es.qd) else { return Ok(()) };
    let pushed = pushed(&r, es)?;
    if es.format == EXPLAIN_FORMAT_TEXT {
        text(&r, &pushed, es)?;
    } else {
        structured(&r, &pushed, es)?;
    }
    Ok(())
}

/// PGRCSERVE M0: the conjuncts the rows road's sites push into their table AM's scans (`GravityReport::rows_pushed`),
/// each deparsed over its SeqScan as the node's `Filter:` prints it (the relation's name prefixed when the statement
/// reads more than one).
fn pushed(r: &GravityReport, es: &ExplainState<'_>) -> PgResult<Vec<String>> {
    let mut out = Vec::new();
    let (Some(pstmt), Some(cxt)) = (es.pstmt, es.deparse_cxt.as_ref()) else { return Ok(out) };
    for &(id, i) in &r.rows_pushed {
        let Some(node) = find_node(pstmt, id) else { continue };
        let Some(q) = node.as_seq_scan().and_then(|s| s.scan.plan.qual.iter().nth(i as usize)) else { continue };
        ruleutils::set_deparse_context_plan(cxt, node, Vec::new());
        out.push(ruleutils::deparse_expression(es.str.allocator(), q, cxt, es.rtable_size > 1 || es.verbose, false)?);
    }
    Ok(out)
}

/// The plan node numbered `id` (the main tree, then each subplan).
fn find_node<'mcx>(pstmt: &'mcx PlannedStmt<'mcx>, id: i32) -> Option<Node<'mcx>> {
    fn under<'mcx>(n: Option<Node<'mcx>>, id: i32) -> Option<Node<'mcx>> {
        let n = n?;
        let p = n.as_plan()?;
        if p.plan_node_id == id {
            return Some(n);
        }
        let kids = [n.as_bitmap_and().map(|a| &a.bitmapplans), n.as_bitmap_or().map(|o| &o.bitmapplans), n.as_append().map(|a| &a.appendplans), n.as_merge_append().map(|m| &m.mergeplans)];
        under(p.lefttree, id).or_else(|| under(p.righttree, id)).or_else(|| kids.into_iter().flatten().find_map(|l| l.iter().find_map(|c| under(Some(c), id)))).or_else(|| n.as_subquery_scan().and_then(|s| under(s.subplan, id)))
    }
    under(pstmt.planTree, id).or_else(|| pstmt.subplans.iter().find_map(|s| under(s, id)))
}

fn text(r: &GravityReport, pushed: &[String], es: &mut ExplainState<'_>) -> PgResult<()> {
    ExplainIndentText(es)?;
    append!(es, "Gravity: {} road", r.road)?;
    if r.width > 0 {
        append!(es, ", width {}", r.width)?;
    }
    append!(es, ", {} pipelines, {} sites", r.program_pipelines, r.sites)?;
    if r.sections > 0 {
        append!(es, ", {} sections, {} claims", r.sections, r.claims)?;
    }
    append!(es, "\n")?;
    es.indent += 1;
    for p in &r.pipelines {
        ExplainIndentText(es)?;
        append!(es, "p{}  {}", p.id, p.arm)?;
        if p.width > 0 {
            append!(es, " W{}", p.width)?;
        }
        if let Some(v) = p.verdict() {
            append!(es, " ({})", v)?;
        }
        if es.timing {
            append!(es, "  {:.3} ms", ms(p.ns))?;
        }
        if p.claims > 0 {
            append!(es, "  claims {}", p.claims)?;
        }
        append!(es, "\n")?;
    }
    if es.timing {
        ExplainIndentText(es)?;
        append!(es, "Run: {:.3} ms", ms(r.run_ns))?;
        if !r.pipelines.is_empty() {
            append!(es, ", leader alone {:.3} ms", ms(r.lead_ns))?;
        }
        append!(es, "\n")?;
    }
    if r.granules.iter().any(|&g| g > 0) || r.opens > 0 {
        ExplainIndentText(es)?;
        append!(es, "Granules: {} read, {} skipped, {} answered; {} part opens\n", r.granules[0], r.granules[1], r.granules[2], r.opens)?;
    }
    if r.spilled > 0 {
        ExplainIndentText(es)?;
        append!(es, "Spill: {} bytes\n", r.spilled)?;
    }
    if r.rows_road > 0 {
        ExplainIndentText(es)?;
        append!(es, "Rows road: {} {}\n", r.rows_road, if r.rows_road == 1 { "site" } else { "sites" })?;
        if !pushed.is_empty() {
            ExplainIndentText(es)?;
            append!(es, "Rows road pushed: {}\n", pushed.join(", "))?;
        }
    }
    ExplainIndentText(es)?;
    append!(es, "Program: {}", if r.memo { "plan memo" } else { "admitted" })?;
    if r.shape != "graph" && !r.shape.is_empty() {
        append!(es, ", shape {}", r.shape)?;
    }
    if es.timing {
        append!(es, ", {:.3} ms", ms(r.admit_ns))?;
    }
    append!(es, "\n")?;
    es.indent -= 1;
    Ok(())
}

fn structured(r: &GravityReport, pushed: &[String], es: &mut ExplainState<'_>) -> PgResult<()> {
    ExplainOpenGroup("Gravity", Some("Gravity"), true, es)?;
    ExplainPropertyText("Road", r.road, es)?;
    ExplainPropertyUInteger("Width", None, r.width as u64, es)?;
    ExplainPropertyUInteger("Program Pipelines", None, r.program_pipelines as u64, es)?;
    ExplainPropertyUInteger("Sites", None, r.sites as u64, es)?;
    ExplainPropertyUInteger("Sections", None, r.sections as u64, es)?;
    ExplainPropertyUInteger("Claims", None, r.claims, es)?;
    if es.timing {
        ExplainPropertyFloat("Run Time", Some("ms"), ms(r.run_ns), 3, es)?;
        ExplainPropertyFloat("Leader Time", Some("ms"), ms(r.lead_ns), 3, es)?;
    }
    ExplainOpenGroup("Pipelines", Some("Pipelines"), false, es)?;
    for p in &r.pipelines {
        ExplainOpenGroup("Pipeline", None, true, es)?;
        ExplainPropertyUInteger("Id", None, p.id as u64, es)?;
        ExplainPropertyText("Arm", p.arm, es)?;
        ExplainPropertyUInteger("Width", None, p.width as u64, es)?;
        ExplainPropertyText("Verdict", p.verdict().as_deref().unwrap_or("Eligible"), es)?;
        if es.timing {
            ExplainPropertyFloat("Time", Some("ms"), ms(p.ns), 3, es)?;
        }
        ExplainPropertyUInteger("Claims", None, p.claims, es)?;
        ExplainCloseGroup("Pipeline", None, true, es)?;
    }
    ExplainCloseGroup("Pipelines", Some("Pipelines"), false, es)?;
    ExplainPropertyUInteger("Granules Read", None, r.granules[0], es)?;
    ExplainPropertyUInteger("Granules Skipped", None, r.granules[1], es)?;
    ExplainPropertyUInteger("Granules Answered", None, r.granules[2], es)?;
    ExplainPropertyUInteger("Part Opens", None, r.opens, es)?;
    ExplainPropertyUInteger("Spill Bytes", None, r.spilled, es)?;
    if r.rows_road > 0 {
        ExplainPropertyUInteger("Rows Road Sites", None, r.rows_road as u64, es)?;
        ExplainPropertyList("Rows Road Pushed", pushed, es)?;
    }
    ExplainPropertyBool("Plan Memo", r.memo, es)?;
    ExplainPropertyText("Shape", r.shape, es)?;
    if es.timing {
        ExplainPropertyFloat("Admission Time", Some("ms"), ms(r.admit_ns), 3, es)?;
    }
    ExplainCloseGroup("Gravity", Some("Gravity"), true, es)
}
