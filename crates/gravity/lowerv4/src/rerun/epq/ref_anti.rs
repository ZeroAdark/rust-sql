//! REFANTI, the row lock over an anti join (D3561 procrastinate's fetch_job, D3562): a claim skipping the rows another
//! relation names, `SELECT .. FROM t WHERE .. AND NOT EXISTS (SELECT 1 FROM l WHERE l.task_id = t.id) ORDER BY .. LIMIT 5
//! FOR UPDATE [OF t] SKIP LOCKED`. PostgreSQL plans the anti join below LockRows (Nested Loop Anti Join over l's
//! parameterized scan, or Hash Anti Join over a Hash of it, under a Sort when the claim's order is not an index's), l
//! REFERENCE-marked once the sublink is pulled up; its junk ctid is NULL on every row above the join (ExecNestLoop and
//! ExecHashJoin project the outer row with the null inner slot). EvalPlanQualFetchRowMark gives no test tuple for a
//! NULL ctid, so in the recheck l's scan is empty and the newest version of t passes the anti join: NOT EXISTS is not
//! rechecked, only t's own quals and the filters around the join. The runner's recheck would send the row past the
//! Limit (LIMIT_ABOVE) and opens no anti join; here the recheck runs in the IR: its redo hands the row on past the
//! join with the inner's columns NULL (epq_ir.rs `Redo::Anti`), no inner opened. Served (`form`): the statement's one
//! lock pipeline over the locked mark's own heap scan, filters, gates and projections around one ANTI join whose inner
//! reads the REFERENCE mark's own heap scan alone (`inner`), projections and LIMIT / OFFSET above the lock, beside
//! InitPlans that lock and write nothing (their pipelines counted out); and ops/lockrows/plan.rs `breaker_limit`'s Sort forms
//! (15b, CTESORT) whose producer holds such a join. `form`'s CTE variant takes procrastinate's claim CTE in its index
//! order, beside the UPDATE that reads its store whole.
use gravity_decompose::catalog::typefacts;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::modifytable::{LockDesc, RowMark};
use gravity_decompose::{Am, Op, PipelineNode, Source};
use gravity_program::{ExprLife, ExprOut, LockKind, Rep};
use types_nodes::jointype::JoinType;
use types_nodes::plannodes::RowMarkType;
use types_nodes::primnodes::OUTER_VAR;
use mcx::{GrowIn, PgVec};

use crate::ops::lockrows::plan::Census;
use crate::graph::Gx;

/// The lock's two marks when they are REFANTI's: the locking one on the plan's relation, and a REFERENCE one on another
/// range table entry, neither a parent's, both with their junk ctid in the row.
pub(crate) fn marks<'d>(d: &'d LockDesc<'_>) -> Option<(&'d RowMark, &'d RowMark)> {
    if d.marks.len() != 2 || d.family().is_some() || d.family_top().is_some() {
        return None;
    }
    let m = d.marks.iter().find(|m| m.lock != LockKind::None)?;
    let r = d.marks.iter().find(|m| m.lock == LockKind::None)?;
    let ok = !m.parent && m.ctid_col > 0 && m.relid == d.plan.target_rel && !r.parent && r.ctid_col > 0 && r.mark == RowMarkType::ROW_MARK_REFERENCE && r.rti != m.rti;
    ok.then_some((m, r))
}

/// The part of the ANTI join at op `i` of pipeline `k` its inner reads: the pipeline (the nest's embedded inner, or the
/// graph pipeline a Materialize's spool or a hash build is: Some(its index)) and the end of its part (its Memoize, spool or
/// build op). Else none.
fn part<'g, 'p>(gx: &'g Gx<'_, 'p>, k: usize, i: usize) -> Option<(&'g PipelineNode<'p>, Option<usize>, usize)> {
    let ps = gx.pipelines();
    let producer = |slot: u8| gx.life_of(k, slot).map(|l| l.0 as usize).filter(|&x| x < k);
    let (s, q) = match ps[k].ops.get(i)? {
        Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_ANTI, inner, .. }) => match inner.source {
            Source::Breaker { slot } => {
                let f = producer(slot)?;
                if !inner.ops.iter().all(pass_through) || !matches!(ps[f].ops.last(), Some(Op::Join(JoinDesc::Spool))) {
                    return None;
                }
                (&ps[f], Some(f))
            }
            _ => (&**inner, None),
        },
        Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_ANTI, slot, catalog: false, .. }) => {
            let f = producer(*slot)?;
            if !matches!(ps[f].ops.last(), Some(Op::Join(JoinDesc::Build { .. }))) {
                return None;
            }
            (&ps[f], Some(f))
        }
        _ => return None,
    };
    let end = s.ops.iter().position(|o| matches!(o, Op::Join(JoinDesc::Memo { .. } | JoinDesc::Spool | JoinDesc::Build { .. }))).unwrap_or(s.ops.len());
    Some((s, q, end))
}

/// Whether op `i` of pipeline `k` is an ANTI join whose inner reads REFERENCE mark `r`'s own heap scan alone: its part
/// (`part`) that scan, then filters and a projection, then a Memoize passed through (an embedded inner) or the spool or
/// build op, a graph pipeline reading nothing and no InitPlan's. Some(that graph pipeline; none for an embedded inner).
pub(crate) fn inner(gx: &Gx<'_, '_>, k: usize, i: usize, r: &RowMark) -> Option<Option<usize>> {
    let (s, q, end) = part(gx, k, i)?;
    let below = s.ops[..end].iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_))) && matches!(s.ops[..end].last(), Some(Op::Project(_)));
    let alone = q.is_none_or(|f| gx.pipelines()[f].desc.consumes.is_empty() && !Census(gx).init_member(f));
    if heap(&s.source) != Some((r.rti, r.relid)) || !below || !alone || !s.ops[end..].iter().skip(1).all(pass_through) {
        return None;
    }
    Some(q)
}

/// The ANTI join at op `i` of pipeline `k`: its inner's columns (the projection ending its part, `part`: each target
/// entry's column, resno - 1, in the stored form of its SQL type), the columns a NULL-extended row's inner side holds
/// (epq_ir.rs `Redo::Anti`); none without that projection or when a type is unknown.
pub(crate) fn cols<'g>(gx: &Gx<'g, '_>, k: usize, i: usize) -> Option<PgVec<'g, (usize, ExprOut)>> {
    let (s, _, end) = part(gx, k, i)?;
    let Some(Op::Project(l)) = end.checked_sub(1).and_then(|e| s.ops.get(e)) else { return None };
    let mut out = PgVec::new_in(gx.mcx);
    for te in l.iter().filter_map(|n| n.as_target_entry()) {
        let typid = gravity_decompose::catalog::expr_type::type_of(te.expr)?;
        let tf = typefacts::facts_of(gx.facts, typid)?;
        out.push_in(((te.resno - 1) as usize, ExprOut { rep: Rep::of_storage(tf.typlen, tf.byval), typid, life: ExprLife::Row }));
    }
    Some(out)
}

/// The lock at op `j` of pipeline `k` in REFANTI's plain form (module doc), else none; `limit`: whether an op is a LIMIT /
/// OFFSET. D3561 (`cte`): or the lock pipeline a CTE's store (procrastinate's claim over its index order, `WITH candidate
/// AS (SELECT .. NOT EXISTS (..) ORDER BY priority DESC, id LIMIT 1 FOR UPDATE OF jobs SKIP LOCKED) UPDATE .. FROM
/// candidate ..`), every InitPlan a CTE; ops/lockrows/plan.rs `Plan::form` then asks `write_pair` for the write reading the store whole
/// (CTELOCK's form). Some((the locked mark's junk ctid column, whether the join is a hash probe)).
pub(crate) fn form(gx: &Gx<'_, '_>, k: usize, j: usize, d: &LockDesc<'_>, limit: fn(&Op<'_>) -> bool, cte: bool) -> Option<(i16, bool)> {
    let ps = gx.pipelines();
    let p = &ps[k];
    let n = match cte {
        true => (gx.cte_write(k) && Census(gx).ctes_only()).then_some(None)?,
        false => Some(Census(gx).inits(k, k)?),
    };
    if !cte && !Census(gx).whole(k + 1) || p.desc.produces().is_some() != cte || !p.ops[j + 1..].iter().all(|o| matches!(o, Op::Project(_)) || limit(o)) {
        return None;
    }
    let (m, r) = marks(d)?;
    if heap(&p.source) != Some((m.rti, m.relid)) {
        return None;
    }
    let plain = |o: &Op<'_>| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_));
    let ja = p.ops[..j].iter().position(|o| !plain(o))?;
    if !p.ops[ja + 1..j].iter().all(plain) {
        return None;
    }
    // the statement's pipelines: this one, the inner's graph pipeline (a hash build's, or a Materialize's producer: the one
    // slot this pipeline reads), and the InitPlans'
    let q = inner(gx, k, ja, r)?;
    if n.is_some_and(|n| !Census(gx).whole(1 + n + usize::from(q.is_some()))) || p.desc.consumes.len() != usize::from(q.is_some()) {
        return None;
    }
    Some((m.ctid_col, matches!(p.ops[ja], Op::Join(JoinDesc::Probe { .. }))))
}

/// A heap scan's (range, bitmap, index) (scanrelid, relid).
fn heap(s: &Source<'_>) -> Option<(u32, u32)> {
    match s {
        Source::Range { am: Am::Heap, scanrelid, relid, .. } | Source::Bitmap { am: Am::Heap, scanrelid, relid, .. } | Source::Index { am: Am::Heap, scanrelid, relid, only: false, .. } => Some((u32::from(*scanrelid), *relid)),
        _ => None,
    }
}

/// A projection handing on the row below it as it came (ref_nest.rs's): a Memoize's or a Materialize emit's.
fn pass_through(o: &Op<'_>) -> bool {
    match o {
        Op::Project(l) => l.iter().all(|n| n.as_target_entry().is_some_and(|te| te.expr.as_var().is_some_and(|v| v.varno == OUTER_VAR && v.varlevelsup == 0 && i32::from(v.varattno) == i32::from(te.resno)))),
        _ => false,
    }
}
