//! D3506 SETLOCK (graphile-worker 0.16.6's get_job: `WITH j AS (SELECT .. FROM jobs WHERE .. AND (jobs.job_queue_id IS
//! NULL OR jobs.job_queue_id IN (SELECT id FROM job_queues WHERE is_available = true FOR UPDATE SKIP LOCKED)) ORDER BY
//! .. LIMIT 1 FOR UPDATE SKIP LOCKED) ..`): a LockRows in the plan of a hashed SubPlan. PostgreSQL builds the set at
//! the first probe (`buildSubPlanHash`): the SubPlan's LockRows -> scan run to their end, each row locked (a SKIP
//! LOCKED one held by another skipped, a newer version rechecked by that LockRows' own EvalPlanQual over its scan's
//! quals) before the probing row reaches its own lock. The set is never rebuilt in the statement's estate (no
//! parameter). A recheck of the probing pipeline's lock (EvalPlanQualStart's fresh SubPlan states) builds a set of its
//! own at the redo's first probe, locking again: our own locks come back TM_Ok, a row another session released since is
//! locked now, one it still holds is skipped; that set lives until the lock's EvalPlanQual ends and the statement's set
//! is never touched. Served (`form`): the lock over its one locking mark's own heap scan (range / index / bitmap), only
//! filters, gates and projections below it (the last the scan's projection carrying the mark's junk ctid), only
//! projections between it and the set's build op, the pipeline's last; the pipeline reads no product and no rerun set
//! holds it; the set is probed by one one-word hashed SubPlan step in the part the statement's other lock (`claim`)
//! rechecks (`claim_part`: below that lock, or below the Sort that lock reads), beside an UPDATE / DELETE that lock
//! pairs with (lockrows plan.rs `Plan::write_pair`). The lock is a `Program.locks` entry of its own after the claims'
//! (`mark`; ops/lockrows/lower.rs `set_lock`), rechecked in the IR over its own scan (a product-form lock: epq_ir.rs
//! `epq_of`); the build runs LAZY (its plan acts). The claim's recheck in the IR evaluates its probes again
//! (`probes_sets`; over a Sort, the producer's probes made again for it, `redo_probes`) against a set of its own
//! (subplan hashed.rs `twin_handle`, exec subplan hash.rs `SubHashEpq`): built at the redo's first probe by running the
//! set's members again into it, the lock host live, and kept for the execution (later rechecks reuse it, as
//! EvalPlanQualBegin resets an EPQ state whose hashed SubPlan's parameters did not change).
use gravity_decompose::ops::shared::subplan::SubForm;
use gravity_decompose::{Am, Op, PipelineNode, Source};
use gravity_program::program::LockSite;
use gravity_program::LockKind;
use mcx::{CollectIn, GrowIn, PgVec};

use crate::graph::Gx;

/// The served form's lock at op `j` of graph pipeline `k`: its locking mark as a `Program.locks` entry, else none.
pub(crate) fn form(gx: &Gx<'_, '_>, k: usize, j: usize) -> Option<LockSite> {
    let ps = gx.pipelines();
    let p = ps.get(k).filter(|_| k < ps.len())?;
    let Some(Op::Lock(d)) = p.ops.get(j) else { return None };
    let b = p.ops.len().checked_sub(1)?;
    let Some(Op::SubPlan(bd)) = p.ops.get(b) else { return None };
    let slot = p.desc.produces()?;
    if !matches!(bd.form, SubForm::Build) || !p.desc.consumes.is_empty() || crate::lifetime::subplan::rebuilt(gx, k) {
        return None;
    }
    let (Some(m), None) = (d.marks.first(), d.marks.get(1)) else { return None };
    if d.family().is_some() || m.lock == LockKind::None || m.parent || m.ctid_col <= 0 || m.relid != d.plan.target_rel || !own_scan(p, m.rti, m.relid) {
        return None;
    }
    let plain = |o: &Op<'_>| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_));
    if !p.ops[..j].iter().all(plain) || !p.ops[j + 1..b].iter().all(|o| matches!(o, Op::Project(_))) {
        return None;
    }
    // the scan's projection, last below the lock, carries the mark's junk ctid
    let Some(Op::Project(l)) = p.ops[..j].last() else { return None };
    let ctid = l.iter().filter_map(|x| x.as_target_entry()).any(|te| te.resno == m.ctid_col && te.expr.as_var().is_some_and(|v| v.varattno == -1 && v.varlevelsup == 0 && i64::from(v.varno) == i64::from(m.rti)));
    // one hashed probe of the set, in the claim's part (`claim_part`), beside the statement's UPDATE / DELETE
    let mut probes = ps.iter().enumerate().flat_map(|(q, x)| x.ops.iter().enumerate().filter(move |(_, o)| matches!(o, Op::SubPlan(s) if matches!(s.form, SubForm::Hashed { slot: t } if t == slot))).map(move |(i, _)| (q, i)));
    let (Some((q, i)), None) = (probes.next(), probes.next()) else { return None };
    let part = claim_part(gx)?;
    let write = ps.iter().any(|x| x.ops.iter().any(|o| matches!(o, Op::Write(w) if matches!(w.cmd, gravity_program::Cmd::Update | gravity_program::Cmd::Delete))));
    (ctid && write && q != k && part.iter().any(|&(c, jc)| q == c && i < jc)).then_some(LockSite { rti: m.rti, relid: m.relid, lock: m.lock, wait: m.wait })
}

/// The ops a recheck of the statement's other lock (`claim`) redoes: its pipeline's ops below it, and when that
/// pipeline reads a Sort's emit, the Sort's producer's ops before its sort build (Limit -> LockRows -> Sort -> scan,
/// ops/lockrows/plan.rs `breaker_limit`): `(pipeline, op end)` each.
fn claim_part<'g>(gx: &Gx<'g, '_>) -> Option<PgVec<'g, (usize, usize)>> {
    let ps = gx.pipelines();
    let (c, jc) = claim(gx)?;
    let mut out = mcx::pg_vec![in gx.mcx; (c, jc)];
    if let Source::Breaker { slot } = ps[c].source {
        if let Some(q) = (0..c).rev().find(|&x| ps[x].desc.produces() == Some(slot)) {
            if let Some(jq) = ps[q].ops.iter().position(|o| matches!(o, Op::Sort(_))) {
                out.push_in((q, jq));
            }
        }
    }
    Some(out)
}

/// The hashed probes of the `form` locks' sets among pipeline `q`'s ops before `jq` (a claim's Sort producer), each
/// made again for its lock's EvalPlanQual redo (subplan hashed.rs `redo_probe`), by SubPlan id.
pub(crate) fn redo_probes<'g>(px: &mut crate::frame::px::Px<'g, '_>, q: usize, jq: usize, node: i32) -> PgVec<'g, (i32, mcx::PgBox<'g, dyn crate::face::step::StepMech + 'g>)> {
    let gx = px.gx;
    let mcx = px.mcx();
    let sets = set_slots(gx);
    let at: PgVec<'_, usize> = (0..jq).filter(|&i| set_probe(&gx.pipelines()[q].ops[i], &sets)).collect_in(gx.mcx);
    at.into_iter().map(|i| crate::ops::subplan::hashed::redo_probe(px, q, i, node)).collect_in(mcx)
}

/// The slots the `form` locks' sets are built into.
pub(crate) fn set_slots<'g>(gx: &Gx<'g, '_>) -> PgVec<'g, u8> {
    let ps = gx.pipelines();
    locks(gx).iter().filter_map(|x| ps[x.0].desc.produces()).collect_in(gx.mcx)
}

/// Op `o` is a hashed SubPlan probe of a `form` lock's set (slots `sets`, `set_slots`).
pub(crate) fn set_probe(o: &Op<'_>, sets: &[u8]) -> bool {
    matches!(o, Op::SubPlan(s) if matches!(s.form, SubForm::Hashed { slot } if sets.contains(&slot)))
}

/// The statement's other lock (beside every `form` lock): `(pipeline, op)` of the one LockRows no hashed SubPlan's plan
/// holds, else none.
pub(crate) fn claim(gx: &Gx<'_, '_>) -> Option<(usize, usize)> {
    let ps = gx.pipelines();
    let at = |q: usize| ps[q].ops.iter().enumerate().filter(|(_, o)| matches!(o, Op::Lock(_))).map(move |(i, _)| (q, i));
    let mut locks = (0..ps.len()).filter(|&q| !builds_set(&ps[q])).flat_map(at);
    let (Some(c), None) = (locks.next(), locks.next()) else { return None };
    ps.inners().iter().all(|x| !x.1.ops.iter().any(|o| matches!(o, Op::Lock(_)))).then_some(c)
}

/// Pipeline `p` ends in a hashed SubPlan's set build.
fn builds_set(p: &PipelineNode<'_>) -> bool {
    matches!(p.ops.last(), Some(Op::SubPlan(s)) if matches!(s.form, SubForm::Build))
}

/// Every `form` lock of the statement, in pipeline order: `(pipeline, op, site)`.
pub(crate) fn locks<'g>(gx: &Gx<'g, '_>) -> PgVec<'g, (usize, usize, LockSite)> {
    let ps = gx.pipelines();
    let mut out = PgVec::new_in(gx.mcx);
    for (k, p) in ps.iter().enumerate() {
        for (j, o) in p.ops.iter().enumerate() {
            if matches!(o, Op::Lock(_)) {
                if let Some(s) = form(gx, k, j) {
                    out.push_in((k, j, s));
                }
            }
        }
    }
    out
}

/// The `Program.locks` mark (1-based) of the `form` lock at op `j` of pipeline `k`: after the claim's entries
/// (`claims`).
pub(crate) fn mark(gx: &Gx<'_, '_>, k: usize, j: usize, claims: usize) -> Option<usize> {
    locks(gx).iter().position(|x| x.0 == k && x.1 == j).map(|i| claims + i + 1)
}

/// The set build pipeline `kb` holds a `form` lock: its probes' recheck reads a set of its own (`twin`).
pub(crate) fn acting_set(gx: &Gx<'_, '_>, kb: usize) -> bool {
    locks(gx).iter().any(|x| x.0 == kb)
}

/// D3678 (ops/modifytable/rowop_epq.rs D652-H): pipeline `kb` builds a hashed SubPlan's set LAZY (its plan acts:
/// subplan hashed.rs `lazy_build`) and reads no product, so a recheck can build a twin of it by running the pipeline
/// again (exec subplan hash.rs `SubHashEpq`).
pub(crate) fn twin_set(gx: &Gx<'_, '_>, kb: usize) -> bool {
    let p = &gx.pipelines()[kb];
    let lazy = matches!(p.ops.last(), Some(Op::SubPlan(d)) if matches!(d.form, SubForm::Build) && crate::ops::subplan::hashed::lazy_build(d));
    lazy && p.desc.consumes.is_empty() && !crate::lifetime::subplan::rebuilt(gx, kb)
}

/// D3678: pipeline `k`'s write rechecks in the IR over a twin of the set pipeline `kb` builds (`twin_set`; rowop_epq.rs
/// `write_twins`): EvalPlanQualStart's fresh SubPlan states build the set again for the recheck.
pub(crate) fn write_twin(gx: &Gx<'_, '_>, k: usize, kb: usize) -> bool {
    twin_set(gx, kb) && crate::ops::modifytable::rowop_epq::write_twins(gx, k)
}

/// Pipeline `p`'s source is relation `relid`'s own heap scan at range table index `rti` (range, bitmap or index, not
/// ONLY).
fn own_scan(p: &PipelineNode<'_>, rti: u32, relid: u32) -> bool {
    match &p.source {
        Source::Range { am: Am::Heap, scanrelid, relid: r, .. } | Source::Bitmap { am: Am::Heap, scanrelid, relid: r, .. } | Source::Index { am: Am::Heap, scanrelid, relid: r, only: false, .. } => u32::from(*scanrelid) == rti && *r == relid,
        _ => false,
    }
}

/// The claim's lock at op `j` of pipeline `k`: every SubPlan step below it a hashed probe of a `form` lock's set, at
/// least one. Its recheck in the IR evaluates them again over the newest version (epq_ir.rs `Redo::Sub`) against the
/// sets of its own (`twin`), as the LockRows' fresh EvalPlanQual estate builds them.
pub(crate) fn probes_sets(gx: &Gx<'_, '_>, k: usize, j: usize) -> bool {
    let sets = set_slots(gx);
    let mut subs = gx.pipelines()[k].ops[..j].iter().filter(|o| matches!(o, Op::SubPlan(_))).peekable();
    subs.peek().is_some() && subs.all(|o| set_probe(o, &sets))
}
