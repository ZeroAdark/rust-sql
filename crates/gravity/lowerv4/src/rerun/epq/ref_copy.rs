//! D3569 REFCOPY, the row lock over a nest whose inner is a COPY-marked subquery's row (Odoo's cron acquisition,
//! ir.cron `_acquire_one_job`: `WITH last_cron_progress AS (SELECT .. FROM ir_cron_progress WHERE cron_id = %s ORDER BY
//! id DESC LIMIT 1) SELECT * FROM ir_cron LEFT JOIN last_cron_progress lcp ON lcp.cron_id = ir_cron.id WHERE .. ORDER
//! BY priority FOR NO KEY UPDATE OF ir_cron SKIP LOCKED`). PostgreSQL inlines the CTE and plans LockRows -> Sort ->
//! Nested Loop Left Join (the locked relation's scan; Subquery Scan lcp -> Limit -> Sort -> scan), lcp COPY-marked: its
//! whole row is a junk column of every row the Sort holds, NULL on a null-extended one. EvalPlanQualFetchRowMark's copy
//! leg hands the Subquery Scan that stored row (none for a NULL one): its quals and projection run again over it, the
//! subquery below it never; the nest joins the newest version of the locked row to it, null-extending a LEFT nest's row
//! when there is none or the join qual fails. The runner's recheck takes a COPY row in a nest inner for a write only
//! (marks.rs `nest_copy`); here the recheck runs in the IR (epq_ir.rs `Fetch::copy`): the stored record's fields as the
//! Subquery Scan's input (copy_row.rs `image_row`), its ops from its entry redone, then the join. Served: lower.rs
//! `ref_copy`, 15b's lock over a Sort of the locked mark's own heap scan, with or without a LIMIT / OFFSET above, whose
//! producer holds one such nest among its filters, gates and projections.
use gravity_decompose::catalog::typefacts;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::modifytable::{LockDesc, RowMark};
use gravity_decompose::{Am, Op, PipelineNode, Source};
use gravity_program::{ExprLife, ExprOut, LockKind, Rep};
use mcx::{CollectIn, GrowIn, PgVec};
use types_nodes::jointype::JoinType;
use types_nodes::plannodes::RowMarkType;
use types_nodes::primnodes::OUTER_VAR;
use types_nodes::Node;

use crate::ops::lockrows::plan::Census;
use crate::graph::Gx;

/// A fetch's relation when the fetch is a COPY mark's stored row (epq_ir.rs `redo_of`): no relation is refetched.
pub(crate) const COPY_ROW: u32 = 0;

/// The lock's two marks when they are REFCOPY's: the locking one on the plan's relation and a COPY one on another range
/// table entry, neither a parent's, both with their junk column in the row.
pub(crate) fn marks<'d>(d: &'d LockDesc<'_>) -> Option<(&'d RowMark, &'d RowMark)> {
    if d.marks.len() != 2 || d.family().is_some() || d.family_top().is_some() {
        return None;
    }
    let m = d.marks.iter().find(|m| m.lock != LockKind::None)?;
    let c = d.marks.iter().find(|m| m.lock == LockKind::None)?;
    let ok = !m.parent && m.ctid_col > 0 && m.relid == d.plan.target_rel && !c.parent && c.ctid_col > 0 && c.mark == RowMarkType::ROW_MARK_COPY && c.rti != m.rti;
    ok.then_some((m, c))
}

/// Op `i` of pipeline `q` when it is an INNER or LEFT nest with no parameters whose embedded inner enters COPY mark
/// `c`'s subquery row (a planned Subquery Scan's, copy_row.rs `boundary`), then runs filters and a projection to its
/// end (`ref_nest::inner_end`), handing them on as they come after it: Some((the inner's `Pipes` index, its entry, its
/// end)).
pub(crate) fn inner(gx: &Gx<'_, '_>, q: usize, i: usize, c: &RowMark) -> Option<(usize, usize, usize)> {
    inner_of(gx, q, i, c.rti)
}

/// `inner` for the COPY row of range table index `rti` (D3592: rerun/inner.rs gives such an inner its copy entry).
pub(crate) fn inner_of(gx: &Gx<'_, '_>, q: usize, i: usize, rti: u32) -> Option<(usize, usize, usize)> {
    let ps = gx.pipelines();
    let Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER | JoinType::JOIN_LEFT, params, .. }) = ps[q].ops.get(i)? else { return None };
    if !params.is_nil() {
        return None;
    }
    let x = ps.inner_of(q, i)?;
    let r = gx.graph.record_rows.iter().find(|r| r.scanrelid == rti && r.whole == 0 && r.node_id >= 0)?;
    let at = crate::rerun::epq::copy_row::boundary(gx, x, r)?;
    let s = &ps[x];
    let end = crate::rerun::epq::ref_nest::inner_end(s);
    let part = s.ops.get(at..end)?;
    let ok = part.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_))) && matches!(part.last(), Some(Op::Project(_))) && s.ops[end..].iter().skip(1).all(pass_through);
    ok.then_some((x, at, end))
}

/// Pipeline `s`'s columns at op `end`, the projection before it (each target entry's column, resno - 1, in the stored
/// form of its SQL type): what a null-extended row's inner side holds (epq_ir.rs `Fetch::copy`); none without that
/// projection or when a type is unknown.
pub(crate) fn outs<'g>(gx: &Gx<'g, '_>, s: &PipelineNode<'_>, end: usize) -> Option<PgVec<'g, (usize, ExprOut)>> {
    let Some(Op::Project(l)) = end.checked_sub(1).and_then(|e| s.ops.get(e)) else { return None };
    let mut out = PgVec::new_in(gx.mcx);
    for te in l.iter().filter_map(|n| n.as_target_entry()) {
        let typid = gravity_decompose::catalog::expr_type::type_of(te.expr)?;
        let tf = typefacts::facts_of(gx.facts, typid)?;
        out.push_in(((te.resno - 1) as usize, ExprOut { rep: Rep::of_storage(tf.typlen, tf.byval), typid, life: ExprLife::Row }));
    }
    Some(out)
}

/// A projection handing on the row below it as it came (ref_nest.rs's): a Memoize's.
fn pass_through(o: &Op<'_>) -> bool {
    match o {
        Op::Project(l) => l.iter().all(|n| n.as_target_entry().is_some_and(|te| te.expr.as_var().is_some_and(|v| v.varno == OUTER_VAR && v.varlevelsup == 0 && i32::from(v.varattno) == i32::from(te.resno)))),
        _ => false,
    }
}

/// D3607 COPYPROBE (River <= 0.10's completer claim: `job_to_update AS (SELECT river_job.id,
/// job_to_finalized_at.finalized_at FROM river_job, job_to_finalized_at WHERE river_job.id = job_to_finalized_at.id AND
/// river_job.state = 'running' FOR UPDATE)`, job_to_finalized_at a CTE of unnest columns; D3601's claim too): a lock
/// with no LIMIT / OFFSET inside a CTE that writes nothing (every InitPlan a CTE), its one locking mark on the
/// pipeline's own heap scan, over INNER hash probes whose builds are COPY-marked rows (a CTE's store, an inlined
/// subquery's rows: `copy_build`), one COPY mark each and no other mark, only quiet filters, gates and projections
/// beside them and projections above the lock. PostgreSQL's EvalPlanQual joins the newest version of the locked row to
/// each COPY mark's stored row (EvalPlanQualFetchRowMark's copy leg; the CTE or subquery never runs again): the redo
/// matches the newest version against the record each probe handed on (epq_ir.rs `Redo::Probe`), then the join quals.
/// The locked mark's junk ctid column, else none.
pub(crate) fn probe_form(gx: &Gx<'_, '_>, k: usize, j: usize, d: &LockDesc<'_>) -> Option<i16> {
    if d.family().is_some() || d.family_top().is_some() || d.marks.iter().any(|m| m.parent) {
        return None;
    }
    let mut locking = d.marks.iter().filter(|m| m.lock != LockKind::None);
    let (Some(m), None) = (locking.next(), locking.next()) else { return None };
    let copies: PgVec<'_, &RowMark> = d.marks.iter().filter(|c| c.lock == LockKind::None).collect_in(gx.mcx);
    if copies.is_empty() || copies.iter().any(|c| c.mark != RowMarkType::ROW_MARK_COPY) || m.ctid_col <= 0 || m.relid != d.plan.target_rel {
        return None;
    }
    let ps = gx.pipelines();
    let p = &ps[k];
    let own = match &p.source {
        Source::Range { am: Am::Heap, scanrelid, relid, .. } | Source::Index { am: Am::Heap, scanrelid, relid, .. } | Source::Bitmap { am: Am::Heap, scanrelid, relid, .. } => u32::from(*scanrelid) == m.rti && *relid == m.relid,
        _ => false,
    };
    let rerun = gx.graph.reruns.iter().any(|r| r.pipelines.iter().any(|&x| usize::from(x) == k));
    let cte = gx.cte_write(k) && p.desc.produces().is_some() && Census(gx).ctes_only();
    if !own || rerun || !cte || !p.ops[j + 1..].iter().all(|o| matches!(o, Op::Project(_))) {
        return None;
    }
    let loud = |x: Node<'_>| crate::effects::volatile(gx, x) || gravity_decompose::expr::has_subplan(x);
    let mut used: PgVec<'_, usize> = PgVec::new_in(gx.mcx);
    for o in p.ops[..j].iter() {
        match o {
            Op::Filter(x) | Op::Gate(x) if !loud(*x) => {}
            Op::Project(l) if !l.iter().any(loud) => {}
            Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_INNER, slot, keys, clauses, qual, .. }) => {
                if keys.iter().chain(clauses.iter()).chain(qual.iter()).any(loud) || crate::ops::hashjoin::keys::confirms(gx, clauses) {
                    return None;
                }
                let c = copy_build(gx, k, *slot, &copies[..])?;
                if used.contains(&c) {
                    return None;
                }
                used.push_in(c);
            }
            _ => return None,
        }
    }
    (used.len() == copies.len()).then_some(m.ctid_col)
}

/// D3607: which of COPY marks `copies` slot `slot`'s build (the pipeline before `k` that fills it, nest_outer.rs
/// `producer`) holds the rows of: its stored row entering that pipeline at `at` (copy_row.rs `boundary`), only quiet
/// filters and projections from there to its build op, its keys quiet, no write or lock in it; else none.
fn copy_build(gx: &Gx<'_, '_>, k: usize, slot: u8, copies: &[&RowMark]) -> Option<usize> {
    let ps = gx.pipelines();
    let q = crate::ops::modifytable::nest_outer::producer(gx, k, slot)?;
    let (Some(Op::Join(JoinDesc::Build { keys, .. })), rest) = ps[q].ops.split_last().map_or((None, &[][..]), |(l, r)| (Some(l), r)) else { return None };
    let loud = |x: Node<'_>| crate::effects::volatile(gx, x) || gravity_decompose::expr::has_subplan(x);
    if keys.iter().any(loud) || rest.iter().any(|o| matches!(o, Op::Write(_) | Op::Lock(_))) {
        return None;
    }
    let quiet = |o: &Op<'_>| match o {
        Op::Filter(x) => !loud(*x),
        Op::Project(l) => !l.iter().any(loud),
        _ => false,
    };
    copies.iter().position(|c| {
        let at = gx.graph.record_rows.iter().find(|r| r.scanrelid == c.rti).and_then(|r| crate::rerun::epq::copy_row::boundary(gx, q, r));
        at.is_some_and(|at| at <= rest.len() && rest[at..].iter().all(quiet))
    })
}
