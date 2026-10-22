//! D655 Row C (V4R-534's redo-in-place EvalPlanQual taught MERGE): a MERGE in a
//! data-modifying CTE rechecks in the IR (write/rowop_epq.rs `in_ir` asks `admits`; the runner's recheck would send the
//! rechecked row to the client, marks.rs EMIT_ONLY), as ExecMergeMatched's TM_Updated leg does. The host locks the newest
//! version (table_tuple_lock, FIND_LAST_VERSION) and answers RECHECK; every MATCHED action's RECHECK enters the redo loop
//! (`Redo::lower`, rowop_step.rs `Arm::MergeEpq`), which puts that version in the write site (WRITE_EPQ_ROW), rechecks the
//! join over it (EvalPlanQual) and on a pass runs the WHEN MATCHED list again from its first action over the newest version
//! (lmerge_matched; a RECHECK there loops, a NOT_MATCHED falls through to the NOT MATCHED actions as on the first try). A
//! version that no longer joins is ExecMergeMatched's NULL-ctid row: the NOT MATCHED actions over the source row (the
//! join's NULL-extended row, ExecMergeNotMatched), or with none nothing (an inner join's empty EvalPlanQual).
//! Six forms (`admits`), each with a MATCHED UPDATE or DELETE and a BY SOURCE action only under PROBED or BUILT-RIGHT,
//! one heap target, no route or view, no action SubPlan:
//! - OWN (held-out tw-core-182: `WITH m AS (MERGE INTO kv t USING (VALUES ('key6', 6)) s(k, v) ON t.k = s.k WHEN MATCHED
//!   THEN UPDATE .. RETURNING ..) SELECT * FROM m`, the VALUES row folded into the scan's qual): the target's own heap
//!   scan under filters, gates and projections, no NOT MATCHED action. The redo is the scan's recheck quals and the ops
//!   below the write over the newest version (epq_ir.rs `epq_of_subs`), the position the newest version's.
//! - BUILT (D776's form; gen mg-0025 group: `WITH m AS (MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN DELETE WHEN
//!   NOT MATCHED THEN INSERT .. RETURNING ..) ..`, a Hash Left Join whose build side scans the target): another relation's
//!   heap scan (merge#304's second MERGE: or a CTE's rows, `USING m` under a COPY mark, `cte_source`: EvalPlanQual hands
//!   the stored row in and never runs the CTE again) probes the target's build by one INNER (no NOT MATCHED action) or LEFT hash probe, no join qual past the
//!   hash clauses and none run again per candidate; the build a plain range scan of the target under plain-Var projections
//!   (no filter: its redo never fails); the write pipeline's ops quiet, no filter after the probe, a projection
//!   after it reading the build side by plain Vars; no mark but the target's and the source's. EvalPlanQual refetches the
//!   source row (the version the join read, which the step's row holds) and reruns the hash join over the newest version:
//!   the redo images the newest version's build keys against the record the probe matched (epq_ir.rs `BuildKeys`), and
//!   the actions read the newest version's cells for the target's carried columns and its ctid as the position.
//! - DRAIN (A4-MATEND's spooled Hash Right Join, hashjoin/drain/lower.rs `merge_drained_target`; the gen mg-0025 group
//!   once earlier rolled-back writes grew the target: the source hashed, the target probing): the target's own range scan
//!   (no recheck qual) under gates and projections probes the source's build by one RIGHT probe (D3379: a quiet join qual
//!   whose source columns the step's row carries rechecks after the hash clauses, as they do); the write pipeline's ops
//!   past the Drain op quiet gates and one quiet projection; no mark but the target's and the
//!   source's (a heap scan of another relation). EvalPlanQual reruns the join over the newest version and the source row
//!   the join read: the redo is the probe pipeline's ops below the probe over the newest version (`epq_of`, a redo that
//!   never fails), the hash clauses over it joined to the source columns the step's row carries (each INNER_VAR column a
//!   clause reads, carried as a plain copy), then the projection's columns that read the target's side again over that
//!   joined row (the junk ctid among them: the newest version's position); a clause that fails is the NULL-extended
//!   source row (the NOT MATCHED actions).
//! - SORTED (merge#304's first MERGE: `WITH m AS (MERGE INTO sq_target t USING sq_source s ON tid = sid ..) ..`, a Merge
//!   Left Join under default statistics): DRAIN's mirror over a merge join (decompose join.rs `map_merge_join`): the
//!   source's rows (another relation's heap scan under quiet ops, sorted) drive one INNER (no NOT MATCHED action) or LEFT
//!   merge join (D3379: a quiet join qual rechecks after the merge clauses, as DRAIN's) over the target's sorted rows (its
//!   own plain range scan under plain-Var projections, no filter); the write pipeline's ops quiet, past the join quiet
//!   gates and one quiet projection; no mark but the target's and the source's. The redo is the target's producer's ops over the newest version (`epq_of`, the sorted inner's
//!   columns), the merge clauses over it joined to the source columns the step's row carries (each OUTER_VAR column a
//!   target-side column reads, carried as a plain copy; a clause's column it does not carry, a MATCHED-only MERGE whose
//!   actions never read the source key, is the merge join's own outer row's, D3379 `held_source`), then the projection's
//!   target-side columns again.
//! - PROBED (mergecmd-rowc-merge s2bs: `WITH m AS (MERGE INTO rmt t USING rms s ON t.id = s.id WHEN MATCHED THEN UPDATE ..
//!   WHEN NOT MATCHED BY SOURCE THEN UPDATE .. RETURNING ..) ..`, a Hash Left Join the target's scan probes): DRAIN's
//!   mirror with the probe in the write pipeline itself: the target's own plain range scan under quiet gates and
//!   projections probes another relation's build (a heap scan) by one INNER (MATCHED actions alone) or LEFT (BY SOURCE
//!   actions, no NOT MATCHED one) hash probe with no join qual; past it quiet gates and one quiet projection; no mark but
//!   the target's and the source's. The redo is the ops below the probe over the newest version (`epq_of`), joined to
//!   the source columns the step's row carries, and the projection's target-side columns again (`remade`). INNER: a
//!   hash clause that fails ends the row (the empty EvalPlanQual). LEFT: the join condition (ri_MergeJoinCondition)
//!   over the newest version and the step's own source columns decides: true runs the MATCHED actions again, else the
//!   BY SOURCE ones, as ExecMergeMatched's recheck does. A BY SOURCE action's own RECHECK runs no EvalPlanQual (its
//!   TM_Updated leg only locks the newest version): its loop (`by_source`) runs the BY SOURCE actions again over the
//!   newest version, the step's NULL source columns standing.
//! - BUILT-RIGHT (mergecmd-rowc-merge s2bs: the same MERGE over a Hash Right Join whose build scans the target, the
//!   source probing: a spooled Hash Right Join, hashjoin/drain): BY SOURCE actions, no NOT MATCHED one; the source a heap
//!   scan under quiet ops ending in the RIGHT probe with no join qual, the target's build its own plain range scan under
//!   plain-Var projections, the drain pipeline quiet past its Drain op; no mark but the target's and the source's. The
//!   redo takes the newest version's cells for the target's carried columns and its ctid (BUILT's), and the join
//!   condition over them and the step's own source columns decides as under PROBED; a drained (BY SOURCE) row's own
//!   RECHECK runs the BY SOURCE actions again over the newest version.
//! BEFORE ROW: a target that fires BEFORE ROW triggers rechecks the same way. Its action's prologue
//! (ExecBRUpdateTriggers / ExecBRDeleteTriggers with is_merge_update / is_merge_delete) locks the newest version
//! (GetTupleForTrigger, FIND_LAST_VERSION, no EvalPlanQual of its own) and hands TM_Updated to ExecMergeMatched before any
//! trigger runs; the host's prologue lock is that (hosts trigger_before.rs `lock_old`: a traversed lock answers RECHECK
//! with the newest version locked, TM_Deleted NOT_MATCHED). The redo then runs as above, and the action's second prologue
//! takes the held lock (TM_Ok, not traversed) and fires the triggers once, OLD the newest version.
//! Every other MATCHED-acting MERGE in a data-modifying CTE keeps ROWC_MERGE's refusal. D1687 (`beside`): a writing CTE's
//! MERGE one of the forms admits may sit beside another write plan (rowop.rs `two_plans_served`): it stops for no
//! runner's EvalPlanQual.
use gravity_abi::ctx::site_off;
use gravity_abi::program::WRITE_EPQ_ROW;
use gravity_abi::site::site;
use gravity_abi::status::{unsupported, UNSUPPORTED_SHAPE, WRITE_DONE};
use gravity_decompose::join::JoinDesc;
use gravity_decompose::catalog::typefacts::TypeFacts;
use gravity_decompose::ops::shared::modifytable::{Member, WriteDesc};
use gravity_decompose::{Am, Op, PipelineNode, Source};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, IntBinOp, IntCond, Type};
use gravity_program::{Cmd, ExprOut};
use mcx::{CollectIn, GrowIn, Mcx, PgBox, PgVec};
use types_nodes::jointype::JoinType;
use types_nodes::primnodes::{MergeMatchKind, INNER_VAR, OUTER_VAR};
use types_nodes::Node;

use crate::admit::refusal::Reason;
use crate::cell::{Cell, Nullable, Row};
use crate::face::source::Locator;
use crate::face::step::ExprFace;
use crate::frame::env::Env;
use crate::frame::frame::Next;
use crate::frame::px::{ExId, Px};
use crate::frame::row::{Carry, RowVals};
use crate::frame::site::CTID_AT;
use crate::graph::Gx;
use crate::rerun::epq::epq_ir::{self, BuildKeys, Epq};

use super::lower::MergeEnd;

/// SORTED (`held_source`): a source column the merge join's outer row does not hold (never: the clause read it there).
const HELD: crate::admit::refusal::ShapeWhy = crate::admit::refusal::ShapeWhy("a writing CTE's MERGE recheck over a source column its merge join's outer row does not hold (D3379)");

/// The redo of a MERGE that rechecks in the IR (module doc).
pub(crate) struct Redo<'g> {
    epq: Epq<'g>,
    form: Form<'g>,
    /// The target's attributes by attno: the write site's cells.
    atts: PgVec<'g, Option<ExprOut>>,
}

enum Form<'g> {
    Own,
    /// The build side's keys, each carried target column `(attno, resno)` and the target's junk ctid (its resno, the tid
    /// type's facts).
    Built { keys: BuildKeys<'g>, cols: PgVec<'g, (i16, i16)>, tid: (i16, TypeFacts) },
    /// DRAIN, SORTED or PROBED (`Drained::side`).
    Drain(PgBox<'g, Drained<'g>>),
    /// BUILT-RIGHT: the target's carried columns `(attno, resno)` and its junk ctid (its resno, the tid type's facts).
    BuiltRight { cols: PgVec<'g, (i16, i16)>, tid: (i16, TypeFacts) },
    /// D2015 UNION-OUTER (merge/nest_arms.rs `first`): the first arm's row decides, the step's row acts (epq_ir.rs
    /// `FirstOuter`).
    /// The target's junk ctid (its resno, the tid type's facts) beside it.
    First(PgBox<'g, epq_ir::FirstOuter<'g>>, (i16, TypeFacts)),
}

/// DRAIN / SORTED (module doc): the join's hash or merge clauses at its node; the write pipeline's projection's columns
/// that read the target's side (each its column, expression and form) at that projection's node; the source columns the
/// step's row carries as plain copies `(attno, resno)` on the source's `side` (DRAIN's INNER_VAR, SORTED's OUTER_VAR).
struct Drained<'g> {
    ex: &'static ExprFace,
    probe: i32,
    clauses: PgVec<'g, ExId>,
    at: i32,
    proj: PgVec<'g, (usize, ExId, ExprOut)>,
    carried: PgVec<'g, (i16, i16)>,
    /// SORTED: each source column a merge clause or the join qual reads that `carried` lacks (`held_source`).
    held: PgVec<'g, i16>,
    side: i32,
    /// PROBED's LEFT probe (BY SOURCE actions): the join condition over the remade row decides, not the hash clauses.
    bs: bool,
    /// PROBED: the target is the write's driving scan, so the actions write at the newest version's locator (as OWN's).
    own: bool,
}

/// Whether MERGE `w` (write op `j` of pipeline `k`, a data-modifying CTE's) is one of the six forms (module doc): the
/// expressions its recheck evaluates (write/rowop_epq.rs `rowc_tail` reads them), else none.
pub(crate) fn admits<'g, 'p>(gx: &Gx<'g, 'p>, k: usize, j: usize, w: &WriteDesc<'_>) -> Option<PgVec<'g, Node<'p>>> {
    let m = w.merge.as_ref()?;
    let other = [Member::Route, Member::Targets, Member::CrossPartition, Member::TargetView, Member::WriteSubPlan].into_iter().any(|x| w.members.has(x));
    let acts = m.actions.iter().any(|a| a.kind == MergeMatchKind::MERGE_WHEN_MATCHED && matches!(a.cmd, Some(Cmd::Update | Cmd::Delete)));
    // BY SOURCE: PROBED alone (a LEFT probe with an action that acts; a DO NOTHING one alone plans no outer join)
    let by = |c: bool| m.actions.iter().any(|a| a.kind == MergeMatchKind::MERGE_WHEN_NOT_MATCHED_BY_SOURCE && (c || a.cmd.is_some()));
    let bs = by(false);
    // unknown trigger facts keep ROWC_MERGE's name (rowop_epq.rs `rowc_tail` would name them ROWC_BEFORE_ROW); a BEFORE
    // ROW trigger rechecks here (module doc, BEFORE ROW)
    let t = gx.facts.trig_ext(w.plan.target_rel);
    if other || !acts || !t.known || w.targets.len() != 1 || w.route.is_some() || w.mark_descs.iter().any(|x| x.parent) {
        return None;
    }
    let nm = m.actions.iter().any(|a| a.kind == MergeMatchKind::MERGE_WHEN_NOT_MATCHED_BY_TARGET);
    let p = &gx.pipelines()[k];
    let sub = |x: &[Node<'_>]| x.iter().any(|&e| gravity_decompose::expr::has_subplan(e));
    if own(p, w) {
        if !w.mark_descs.iter().all(|x| x.rti == w.rti) {
            return probed(gx, k, j, w, (nm, bs)).map(|(x, _)| x).filter(|x| !sub(x));
        }
        let x = crate::ops::modifytable::rowop_epq::below_in(gx.mcx, p, j)?;
        return (!nm && !by(true) && !sub(&x)).then_some(x);
    }
    if by(true) {
        return built_right(gx, k, j, w, (nm, bs)).map(|(x, _)| x).filter(|x| !sub(x));
    }
    if let Some((x, _)) = drain(gx, k, j, w).or_else(|| sorted(gx, k, j, w, nm)) {
        return (!sub(&x)).then_some(x);
    }
    built(gx, k, j, w, nm).map(|(x, _)| x).filter(|x| !sub(x))
}

/// D1687 beside D655 Row C (merge#304: two data-modifying CTEs' MERGEs, the second reading the first's rows): whether
/// pipeline `k`'s write `w` is a writing CTE's MERGE that rechecks in its own IR (`admits`, write/rowop_epq.rs `in_ir`).
/// It stops for no runner's EvalPlanQual, so it may sit beside another write plan, as such an UPDATE / DELETE does
/// (rowop.rs `two_plans_served`).
pub(crate) fn beside(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>) -> bool {
    let Some(j) = gx.pipelines()[k].ops.iter().position(|o| matches!(o, Op::Write(_))) else { return false };
    w.cmd == Cmd::Merge && w.checks.is_empty() && w.on_conflict.is_none() && gx.cte_write(k) && admits(gx, k, j, w).is_some() && crate::ops::modifytable::rowop_epq::in_ir(gx, k)
}

/// The `side` (OUTER_VAR / INNER_VAR) columns `x` reads by attno, or none past a node the walk cannot see through.
fn reads<'g>(x: Node<'_>, side: i32, mcx: Mcx<'g>) -> Option<PgVec<'g, i16>> {
    let mut out = PgVec::new_in(mcx);
    let known = gravity_decompose::expr::walk(x, &mut |n| {
        if let Some(v) = n.as_var().filter(|v| v.varno == side && v.varlevelsup == 0) {
            out.push_in(v.varattno);
        }
        true
    });
    known.then_some(out)
}

/// The step's row's plain source copies: each projected `side` (INNER_VAR / OUTER_VAR) column `(attno, resno)`.
fn carried<'g>(l: &types_nodes::list::NodeList<'_>, side: i32, mcx: Mcx<'g>) -> PgVec<'g, (i16, i16)> {
    let plain = |e: Node<'_>| e.as_target_entry().and_then(|te| te.expr.as_var().filter(|v| v.varno == side && v.varlevelsup == 0 && v.varattno > 0).map(|v| (v.varattno, te.resno)));
    l.iter().filter_map(plain).collect_in(mcx)
}

/// DRAIN's form (module doc): the expressions its recheck evaluates and (the probe pipeline, its probe op, the write
/// pipeline's projection op).
fn drain<'g, 'p>(gx: &Gx<'g, 'p>, k: usize, j: usize, w: &WriteDesc<'_>) -> Option<(PgVec<'g, Node<'p>>, (usize, usize, usize))> {
    let ps = gx.pipelines();
    let p = &ps[k];
    let kp = gx.merge_drained_target(k)?;
    let jd = p.ops[..j].iter().position(|o| matches!(o, Op::Join(JoinDesc::Drain { jointype: JoinType::JOIN_RIGHT, .. })))?;
    let pk = &ps[kp];
    let range = matches!(&pk.source, Source::Range { am: Am::Heap, scanrelid, relid, .. } if u32::from(*scanrelid) == w.rti && *relid == w.plan.target_rel);
    let (Some(Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_RIGHT, slot, clauses, qual, catalog: false, .. })), before) = pk.ops.split_last().map_or((None, &[][..]), |(l, r)| (Some(l), r)) else { return None };
    let quiet = |x: Node<'_>| !crate::effects::volatile(gx, x);
    let below = |o: &Op<'_>| match o {
        Op::Gate(x) => quiet(*x),
        Op::Project(l) => l.iter().all(quiet),
        _ => false,
    };
    if !range || !qual.iter().all(quiet) || !before.iter().all(below) {
        return None;
    }
    let src = gx.life_of(kp, *slot)?.0 as usize;
    let (srti, srel) = match &ps[src].source {
        Source::Range { am: Am::Heap, scanrelid, relid, .. } | Source::Index { am: Am::Heap, scanrelid, relid, .. } | Source::Bitmap { am: Am::Heap, scanrelid, relid, .. } => (u32::from(*scanrelid), *relid),
        _ => return None,
    };
    let mut proj = None;
    for (i, o) in p.ops[jd + 1..j].iter().enumerate() {
        match o {
            Op::Gate(x) if quiet(*x) => {}
            Op::Project(l) if proj.is_none() && l.iter().all(quiet) => proj = Some((jd + 1 + i, *l)),
            _ => return None,
        }
    }
    let (jproj, l) = proj?;
    let inner = carried(l, INNER_VAR, gx.mcx);
    let have = |x: Node<'_>| reads(x, INNER_VAR, gx.mcx).is_some_and(|v| v.iter().all(|a| inner.iter().any(|c| c.0 == *a)));
    let target_side = |x: Node<'_>| reads(x, OUTER_VAR, gx.mcx).is_none_or(|v| !v.is_empty());
    let cols = l.iter().filter_map(|e| e.as_target_entry()).all(|te| !target_side(te.expr) || have(te.expr));
    let marks = w.mark_descs.iter().all(|x| x.rti == w.rti || x.rti == srti && x.relid == srel);
    // D3379 (mergecmd_rowc_merge :61): a join qual past the hash clauses rechecks as they do, over the same joined row
    if srti == w.rti || !clauses.iter().chain(qual.iter()).all(have) || !cols || !marks || w.targets[0].junk_col <= 0 {
        return None;
    }
    let mut out: PgVec<'g, Node<'p>> = PgVec::new_in(gx.mcx);
    for o in before.iter() {
        if let Op::Project(x) = o {
            out.extend_in(x.iter());
        }
    }
    out.extend_in(clauses.iter().chain(qual.iter()).chain(l.iter()));
    Some((out, (kp, pk.ops.len() - 1, jproj)))
}

/// PROBED's form (module doc): the expressions its recheck evaluates and (its probe op, the write pipeline's projection
/// op).
fn probed<'g, 'p>(gx: &Gx<'g, 'p>, k: usize, j: usize, w: &WriteDesc<'_>, (nm, bs): (bool, bool)) -> Option<(PgVec<'g, Node<'p>>, (usize, usize))> {
    let ps = gx.pipelines();
    let p = &ps[k];
    let jp = p.ops[..j].iter().position(|o| matches!(o, Op::Join(_)))?;
    let Op::Join(JoinDesc::Probe { jointype, slot, clauses, qual, catalog: false, .. }) = &p.ops[jp] else { return None };
    let jt = if bs { *jointype == JoinType::JOIN_LEFT } else { *jointype == JoinType::JOIN_INNER };
    let quiet = |x: Node<'_>| !crate::effects::volatile(gx, x);
    let below = |o: &Op<'_>| match o {
        Op::Gate(x) => quiet(*x),
        Op::Project(l) => l.iter().all(quiet),
        _ => false,
    };
    if nm || !jt || !qual.is_nil() || !p.ops[..jp].iter().all(below) {
        return None;
    }
    let src = gx.life_of(k, *slot)?.0 as usize;
    let (srti, srel) = heap_of(&ps[src].source)?;
    let mut proj = None;
    for (i, o) in p.ops[jp + 1..j].iter().enumerate() {
        match o {
            Op::Gate(x) if quiet(*x) => {}
            Op::Project(l) if proj.is_none() && l.iter().all(quiet) => proj = Some((jp + 1 + i, *l)),
            _ => return None,
        }
    }
    let (jproj, l) = proj?;
    let inner = carried(l, INNER_VAR, gx.mcx);
    let have = |x: Node<'_>| reads(x, INNER_VAR, gx.mcx).is_some_and(|v| v.iter().all(|a| inner.iter().any(|c| c.0 == *a)));
    let target_side = |x: Node<'_>| reads(x, OUTER_VAR, gx.mcx).is_none_or(|v| !v.is_empty());
    let cols = l.iter().filter_map(|e| e.as_target_entry()).all(|te| !target_side(te.expr) || have(te.expr));
    let marks = w.mark_descs.iter().all(|x| x.rti == w.rti || x.rti == srti && x.relid == srel);
    // INNER: the redo tests the hash clauses over the source keys the step's row carries; LEFT: the join condition, whose
    // source columns are the plan slot's (setrefs puts them in it)
    if srti == w.rti || !bs && !clauses.iter().all(have) || !cols || !marks || w.targets[0].junk_col <= 0 {
        return None;
    }
    let mut out: PgVec<'g, Node<'p>> = PgVec::new_in(gx.mcx);
    for o in p.ops[..jp].iter() {
        if let Op::Project(x) = o {
            out.extend_in(x.iter());
        }
    }
    out.extend_in(clauses.iter().chain(l.iter()));
    // the BY SOURCE split's join condition (the write op's own description, of the plan's lifetime)
    if let Some(Op::Write(wd)) = p.ops.get(j) {
        out.extend_in(wd.merge.iter().flat_map(|m| m.join_cond.iter()).flat_map(|c| c.iter()));
    }
    Some((out, (jp, jproj)))
}

/// BUILT-RIGHT's form (module doc): the expressions its recheck evaluates and the target's build pipeline.
fn built_right<'g, 'p>(gx: &Gx<'g, 'p>, k: usize, j: usize, w: &WriteDesc<'_>, (nm, bs): (bool, bool)) -> Option<(PgVec<'g, Node<'p>>, usize)> {
    let ps = gx.pipelines();
    let p = &ps[k];
    let jd = p.ops[..j].iter().position(|o| matches!(o, Op::Join(JoinDesc::Drain { jointype: JoinType::JOIN_RIGHT, .. })))?;
    let Op::Join(JoinDesc::Drain { matches, .. }) = &p.ops[jd] else { return None };
    let quiet = |x: Node<'_>| !crate::effects::volatile(gx, x);
    let calm = |o: &Op<'_>| match o {
        Op::Gate(x) | Op::Filter(x) => quiet(*x),
        Op::Project(l) => l.iter().all(quiet),
        _ => false,
    };
    if nm || !bs || jd != 0 || !p.ops[jd + 1..j].iter().all(calm) {
        return None;
    }
    // the probe: another relation's heap scan under quiet ops, one RIGHT probe of the target's build, no join qual
    let kp = gx.life_of(k, *matches)?.0 as usize;
    let pk = &ps[kp];
    let (srti, srel) = heap_of(&pk.source)?;
    let (Some(Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_RIGHT, slot, qual, catalog: false, .. })), before) = pk.ops.split_last().map_or((None, &[][..]), |(l, r)| (Some(l), r)) else { return None };
    if !qual.is_nil() || !before.iter().all(calm) {
        return None;
    }
    // the build: the target's own plain range scan under plain-Var projections (no filter: its redo never fails)
    let q = gx.life_of(kp, *slot)?.0 as usize;
    let pq = &ps[q];
    let target = matches!(&pq.source, Source::Range { am: Am::Heap, scanrelid, relid, .. } if u32::from(*scanrelid) == w.rti && *relid == w.plan.target_rel);
    let plain = |l: &types_nodes::list::NodeList<'_>| l.iter().all(|e| e.as_target_entry().is_some_and(|te| te.expr.as_var().is_some_and(|v| v.varlevelsup == 0)));
    let (Some(Op::Join(JoinDesc::Build { .. })), rest) = pq.ops.split_last().map_or((None, &[][..]), |(l, r)| (Some(l), r)) else { return None };
    let marks = w.mark_descs.iter().all(|x| x.rti == w.rti || x.rti == srti && x.relid == srel);
    if pq.desc.produces() != Some(*slot) || !target || !rest.iter().all(|o| matches!(o, Op::Project(l) if plain(*l))) || srti == w.rti || !marks || w.targets[0].junk_col <= 0 {
        return None;
    }
    let mut out: PgVec<'g, Node<'p>> = PgVec::new_in(gx.mcx);
    for o in rest.iter().chain(p.ops[jd + 1..j].iter()) {
        if let Op::Project(x) = o {
            out.extend_in(x.iter());
        }
    }
    if let Some(Op::Write(wd)) = p.ops.get(j) {
        out.extend_in(wd.merge.iter().flat_map(|m| m.join_cond.iter()).flat_map(|c| c.iter()));
    }
    Some((out, q))
}

/// SORTED's form (module doc): the expressions its recheck evaluates and (the merge join op, the target's sorted
/// producer, the write pipeline's projection op).
fn sorted<'g, 'p>(gx: &Gx<'g, 'p>, k: usize, j: usize, w: &WriteDesc<'_>, nm: bool) -> Option<(PgVec<'g, Node<'p>>, (usize, usize, usize))> {
    let ps = gx.pipelines();
    let p = &ps[k];
    let Source::Breaker { slot: from } = p.source else { return None };
    let jm = p.ops[..j].iter().position(|o| matches!(o, Op::Join(_)))?;
    let Op::Join(JoinDesc::Merge { slot, jointype, clauses, qual, .. }) = &p.ops[jm] else { return None };
    let jt = *jointype == JoinType::JOIN_INNER && !nm || *jointype == JoinType::JOIN_LEFT && nm;
    let quiet = |x: Node<'_>| !crate::effects::volatile(gx, x);
    let calm = |o: &Op<'_>| match o {
        Op::Gate(x) | Op::Filter(x) => quiet(*x),
        Op::Project(l) => l.iter().all(quiet),
        _ => false,
    };
    // a side's sorted rows: the pipeline that fills slot `s` through its last op, a Sort (decompose sort.rs `map`; the merge
    // join reads that breaker itself, `spool_run`), and its ops before the Sort
    let sorted_by = |s: u8| gx.life_of(k, s).map(|l| l.0 as usize).filter(|&q| matches!(ps[q].ops.last(), Some(Op::Sort(_)))).map(|q| (q, &ps[q].ops[..ps[q].ops.len() - 1]));
    if !jt || !qual.iter().all(quiet) || !p.ops[..jm].iter().all(calm) {
        return None;
    }
    // the source: another relation's heap scan under quiet ops, sorted
    let (src, sops) = sorted_by(from)?;
    let (srti, srel) = heap_of(&ps[src].source)?;
    // the target: its own plain range scan under plain-Var projections, sorted (no filter: its redo never fails)
    let (q, tops) = sorted_by(*slot)?;
    let target = matches!(&ps[q].source, Source::Range { am: Am::Heap, scanrelid, relid, .. } if u32::from(*scanrelid) == w.rti && *relid == w.plan.target_rel);
    let plain = |l: &types_nodes::list::NodeList<'_>| l.iter().all(|e| e.as_target_entry().is_some_and(|te| te.expr.as_var().is_some_and(|v| v.varlevelsup == 0)));
    if !sops.iter().all(calm) || !target || !tops.iter().all(|o| matches!(o, Op::Project(l) if plain(*l))) {
        return None;
    }
    let mut proj = None;
    for (i, o) in p.ops[jm + 1..j].iter().enumerate() {
        match o {
            Op::Gate(x) if quiet(*x) => {}
            Op::Project(l) if proj.is_none() && l.iter().all(quiet) => proj = Some((jm + 1 + i, *l)),
            _ => return None,
        }
    }
    let (jproj, l) = proj?;
    let outer = carried(l, OUTER_VAR, gx.mcx);
    let have = |x: Node<'_>| reads(x, OUTER_VAR, gx.mcx).is_some_and(|v| v.iter().all(|a| outer.iter().any(|c| c.0 == *a)));
    let target_side = |x: Node<'_>| reads(x, INNER_VAR, gx.mcx).is_none_or(|v| !v.is_empty());
    let cols = l.iter().filter_map(|e| e.as_target_entry()).all(|te| !target_side(te.expr) || have(te.expr));
    let marks = w.mark_descs.iter().all(|x| x.rti == w.rti || x.rti == srti && x.relid == srel);
    // D3379 (mergecmd_rowc_cte :47): a join qual past the merge clauses rechecks as they do, over the same joined row;
    // (:48) a source column the projection does not carry is the merge join's held outer row's (`held_source`)
    let known = |x: Node<'_>| reads(x, OUTER_VAR, gx.mcx).is_some_and(|v| v.iter().all(|&a| a > 0));
    if srti == w.rti || !clauses.iter().chain(qual.iter()).all(known) || !cols || !marks || w.targets[0].junk_col <= 0 {
        return None;
    }
    let mut out: PgVec<'g, Node<'p>> = PgVec::new_in(gx.mcx);
    for o in tops.iter() {
        if let Op::Project(x) = o {
            out.extend_in(x.iter());
        }
    }
    out.extend_in(clauses.iter().chain(qual.iter()).chain(l.iter()));
    Some((out, (jm, q, jproj)))
}

/// A heap scan source's (range-table index, relation).
fn heap_of(s: &Source<'_>) -> Option<(u32, u32)> {
    match s {
        Source::Range { am: Am::Heap, scanrelid, relid, .. } | Source::Index { am: Am::Heap, scanrelid, relid, .. } | Source::Bitmap { am: Am::Heap, scanrelid, relid, .. } => Some((u32::from(*scanrelid), *relid)),
        _ => None,
    }
}

/// BUILT's CTE source (module doc): pipeline `k` the CteScan's emit over a CTE's rows (decompose subplan.rs
/// `map_cte_scan`: a breaker source a CTE's pipeline fills, whose COPY row enters at op 0), its range-table index.
fn cte_source(gx: &Gx<'_, '_>, k: usize) -> Option<u32> {
    let Source::Breaker { slot } = gx.pipelines()[k].source else { return None };
    let cte = gx.life_of(k, slot).is_some_and(|l| gx.cte_write(l.0 as usize));
    let row = gx.graph.record_rows.iter().find(|r| r.node_id >= 0 && crate::rerun::epq::copy_row::boundary(gx, k, r) == Some(0));
    row.filter(|_| cte).map(|r| r.scanrelid)
}

/// OWN's scan: the target's own heap scan (a TID scan refuses in the redo by name, D1672).
fn own(p: &PipelineNode<'_>, w: &WriteDesc<'_>) -> bool {
    match &p.source {
        Source::Range { am, scanrelid, relid, .. } | Source::Index { am, scanrelid, relid, .. } | Source::Bitmap { am, scanrelid, relid, .. } => *am == Am::Heap && *relid == w.plan.target_rel && u32::from(*scanrelid) == w.rti,
        _ => false,
    }
}

/// BUILT's form (module doc): the expressions its recheck evaluates and (the probe op, the target's build pipeline).
fn built<'g, 'p>(gx: &Gx<'g, 'p>, k: usize, j: usize, w: &WriteDesc<'_>, nm: bool) -> Option<(PgVec<'g, Node<'p>>, (usize, usize))> {
    let ps = gx.pipelines();
    let p = &ps[k];
    // the source's mark: a heap scan's (its relation), a CTE's COPY one (relid none)
    let (srti, srel) = match heap_of(&p.source) {
        Some(x) => (x.0, Some(x.1)),
        None => (cte_source(gx, k)?, None),
    };
    let mut joins = p.ops[..j].iter().enumerate().filter(|(_, o)| matches!(o, Op::Join(_)));
    let (Some((jp, Op::Join(JoinDesc::Probe { jointype, slot, clauses, qual, .. }))), None) = (joins.next(), joins.next()) else { return None };
    let jt = *jointype == JoinType::JOIN_INNER && !nm || *jointype == JoinType::JOIN_LEFT && nm;
    if srti == w.rti || !jt || !qual.is_nil() || !epq_ir::images_decide(gx, clauses) {
        return None;
    }
    let q = crate::ops::modifytable::merge::admit::built_target(gx, k, w)?;
    let pq = &ps[q];
    let target = matches!(&pq.source, Source::Range { am: Am::Heap, scanrelid, relid, .. } if u32::from(*scanrelid) == w.rti && *relid == w.plan.target_rel);
    let (Some(Op::Join(JoinDesc::Build { keys, .. })), rest) = pq.ops.split_last().map_or((None, &[][..]), |(l, r)| (Some(l), r)) else { return None };
    let plain = |l: &types_nodes::list::NodeList<'_>| l.iter().all(|e| e.as_target_entry().is_some_and(|te| te.expr.as_var().is_some_and(|v| v.varlevelsup == 0)));
    if pq.desc.produces() != Some(*slot) || !target || !rest.iter().all(|o| matches!(o, Op::Project(l) if plain(*l))) {
        return None;
    }
    let quiet = |x: Node<'_>| !crate::effects::volatile(gx, x);
    let inner = |x: Node<'_>| {
        let mut hit = false;
        let known = gravity_decompose::expr::walk(x, &mut |n| {
            hit = hit || n.as_var().is_some_and(|v| v.varno == INNER_VAR);
            !hit
        });
        hit || !known
    };
    let entry = |e: Node<'_>| e.as_target_entry().is_some_and(|te| te.expr.as_var().is_some() || !inner(te.expr));
    let mut out: PgVec<'g, Node<'p>> = PgVec::new_in(gx.mcx);
    let mut projected = false;
    for (i, o) in p.ops[..j].iter().enumerate() {
        match o {
            _ if i == jp => {}
            Op::Gate(x) if quiet(*x) => {}
            Op::Filter(x) if i < jp && quiet(*x) => out.extend_in(crate::ops::modifytable::rowop_epq::conjuncts(*x)),
            Op::Project(l) if l.iter().all(|e| quiet(e) && (i < jp || entry(e))) => {
                projected = projected || i > jp;
                out.extend_in(l.iter());
            }
            _ => return None,
        }
    }
    let copy = types_nodes::plannodes::RowMarkType::ROW_MARK_COPY;
    let marks = w.mark_descs.iter().all(|x| x.rti == w.rti || x.rti == srti && srel.map_or(x.mark == copy, |r| x.relid == r));
    if !projected || !marks || w.targets[0].junk_col <= 0 {
        return None;
    }
    out.extend_in(clauses.iter().chain(keys.iter()));
    for o in rest.iter() {
        if let Op::Project(l) = o {
            out.extend_in(l.iter());
        }
    }
    Some((out, (jp, q)))
}

/// The redo of write op `j` of pipeline `k` (MERGE `w`, plan node `node`, the statement's expression face `ex`) that the
/// claim admitted (`admits`).
pub(crate) fn of<'g>(px: &mut Px<'g, '_>, (k, j): (usize, usize), w: &WriteDesc<'_>, (node, ex): (i32, &'static ExprFace)) -> Redo<'g> {
    let gx = px.gx;
    let relid = w.plan.target_rel;
    let nm = w.merge.as_ref().is_some_and(|m| m.actions.iter().any(|a| a.kind == MergeMatchKind::MERGE_WHEN_NOT_MATCHED_BY_TARGET));
    let atts = epq_ir::atts_iter(gx, relid).collect_in(px.mcx());
    // D2015 UNION-OUTER: the statement's own MERGE through a root (write/rowop_epq.rs `in_ir`, not `admits`)
    if let Some((jn, ik, f0)) = crate::ops::modifytable::merge::nest_arms::first(gx, k, w) {
        let Some(feeds) = crate::ops::modifytable::merge::nest_arms::arms(gx, k, w) else { crate::refuse!(node, Reason::Bug { payload: "a union-outer MERGE without its arms" }) };
        let relids: PgVec<'_, u32> = w.targets.iter().map(|t| t.relid).collect_in(px.mcx());
        let Some(of) = gravity_decompose::catalog::typefacts::facts_of(gx.facts, types_core::catalog::OIDOID) else { crate::refuse!(node, Reason::Bug { payload: "the oid type the catalog does not know" }) };
        let Some(tid) = crate::ops::modifytable::merge::nm::junk_tid(gx, w, node) else { crate::refuse!(node, Reason::Bug { payload: "a union-outer MERGE target without its ctid junk column" }) };
        let first = epq_ir::first_outer(px, (k, j), (jn, ik), f0, (&feeds[..], &relids[..]), (w.tableoid_col, of), node, epq_ir::ROWC_SYS);
        return Redo { epq: epq_ir::none(node, px.mcx()), form: Form::First(mcx::box_in(first, px.mcx()), tid), atts };
    }
    let bs = w.merge.as_ref().is_some_and(|m| m.actions.iter().any(|a| a.kind == MergeMatchKind::MERGE_WHEN_NOT_MATCHED_BY_SOURCE && a.cmd.is_some()));
    if own(&gx.pipelines()[k], w) && !w.mark_descs.iter().all(|x| x.rti == w.rti) {
        let Some((_, (jp, jproj))) = probed(gx, k, j, w, (nm, bs)) else { crate::refuse!(node, Reason::Bug { payload: "a rechecking MERGE over its own scan and another relation's mark outside PROBED" }) };
        let epq = epq_ir::epq_of(px, k, jp, node, relid, epq_ir::ROWC_SYS);
        let mut d = drained(px, (k, jproj), (k, jp), INNER_VAR, (node, ex));
        (d.bs, d.own) = (bs, true);
        return Redo { epq, form: Form::Drain(mcx::box_in(d, px.mcx())), atts };
    }
    if let Some((_, q)) = built_right(gx, k, j, w, (nm, bs)) {
        let jb = gx.pipelines()[q].ops.len().saturating_sub(1);
        let epq = epq_ir::epq_of(px, q, jb, node, relid, epq_ir::ROWC_SYS);
        let cols = w.merge.as_ref().map_or_else(|| PgVec::new_in(px.mcx()), |m| mcx::to_vec_in(&m.target_cols[..], px.mcx()));
        let Some(tid) = crate::ops::modifytable::merge::nm::junk_tid(gx, w, node) else { crate::refuse!(node, Reason::Bug { payload: "a hashed MERGE target without its ctid junk column" }) };
        return Redo { epq, form: Form::BuiltRight { cols, tid }, atts };
    }
    if let Some((_, (kp, jp, jproj))) = drain(gx, k, j, w) {
        let epq = epq_ir::epq_of(px, kp, jp, node, relid, epq_ir::ROWC_SYS);
        let d = drained(px, (k, jproj), (kp, jp), INNER_VAR, (node, ex));
        return Redo { epq, form: Form::Drain(mcx::box_in(d, px.mcx())), atts };
    }
    if let Some((_, (jm, q, jproj))) = sorted(gx, k, j, w, nm) {
        let epq = epq_ir::epq_of(px, q, gx.pipelines()[q].ops.len() - 1, node, relid, epq_ir::ROWC_SYS);
        let d = drained(px, (k, jproj), (k, jm), OUTER_VAR, (node, ex));
        return Redo { epq, form: Form::Drain(mcx::box_in(d, px.mcx())), atts };
    }
    match built(gx, k, j, w, nm) {
        Some((_, (jp, q))) => {
            let jb = gx.pipelines()[q].ops.len().saturating_sub(1);
            let epq = epq_ir::epq_of(px, q, jb, node, relid, epq_ir::ROWC_SYS);
            let keys = BuildKeys::of(px, q, (k, jp));
            let cols = w.merge.as_ref().map_or_else(|| PgVec::new_in(px.mcx()), |m| mcx::to_vec_in(&m.target_cols[..], px.mcx()));
            let Some(tid) = crate::ops::modifytable::merge::nm::junk_tid(gx, w, node) else { crate::refuse!(node, Reason::Bug { payload: "a hashed MERGE target without its ctid junk column" }) };
            Redo { epq, form: Form::Built { keys, cols, tid }, atts }
        }
        None => Redo { epq: epq_ir::epq_of_subs(px, k, j, node, relid, epq_ir::ROWC_SYS), form: Form::Own, atts },
    }
}

/// DRAIN's / SORTED's compiled parts (`Drained`) for projection op `jproj` of write pipeline `k` and join op `jp` of
/// pipeline `kp` (DRAIN's probe, SORTED's merge join), the source on `side`.
fn drained<'g>(px: &mut Px<'g, '_>, (k, jproj): (usize, usize), (kp, jp): (usize, usize), side: i32, (node, ex): (i32, &'static ExprFace)) -> Drained<'g> {
    let (gx, mcx) = (px.gx, px.mcx());
    let (p, pk) = (&gx.pipelines()[k], &gx.pipelines()[kp]);
    let (Op::Join(JoinDesc::Probe { clauses, qual, .. } | JoinDesc::Merge { clauses, qual, .. }), Op::Project(l)) = (&pk.ops[jp], &p.ops[jproj]) else { crate::refuse!(node, Reason::Bug { payload: "a drained MERGE redo without its join or projection" }) };
    let probe = crate::rerun::inner::op_node(pk, jp);
    let carry = carried(l, side, mcx);
    let mut held: PgVec<'g, i16> = PgVec::new_in(mcx);
    if side == OUTER_VAR {
        for a in clauses.iter().chain(qual.iter()).filter_map(|q| reads(q, side, gx.mcx)).flatten() {
            if !carry.iter().any(|c| c.0 == a) && !held.contains(&a) {
                held.push_in(a);
            }
        }
    }
    // D3379: the join qual (DRAIN / SORTED only: PROBED's has none) after the hash or merge clauses, over the same row
    let clauses = clauses.iter().chain(qual.iter()).map(|q| ex.0.compile(px, q, None, probe)).collect_in(mcx);
    let at = crate::rerun::inner::op_node(p, jproj);
    let target = if side == INNER_VAR { OUTER_VAR } else { INNER_VAR };
    let mut proj = PgVec::new_in(mcx);
    for te in l.iter().filter_map(|e| e.as_target_entry()) {
        if reads(te.expr, target, gx.mcx).is_some_and(|v| v.is_empty()) {
            continue; // the source's side: the step's row's value stands
        }
        let Some(out) = gx.out_of(te.expr) else { crate::refuse!(node, Reason::Shape { why: epq_ir::ROWC_SYS }) };
        proj.push_in(((te.resno - 1) as usize, ex.0.compile(px, te.expr, None, at), out));
    }
    Drained { ex, probe, clauses, at, proj, carried: carry, held, side, bs: false, own: false }
}

/// The redo loop's header: a declared one (the backend takes only those as loops, tpde view.rs).
pub(crate) fn header(b: &mut FunctionBuilder<'_>) -> Block {
    let l = b.create_block(&[]);
    b.mark_loop_header(l);
    l
}

impl Redo<'_> {
    /// The redo loop at `l_redo` (module doc) over the step's row `at` (its actions' row `arm`); its rows join `l_join`.
    /// PROBED's BY SOURCE actions recheck at their own loop `l_bs` (`by_source`).
    pub(super) fn lower<'e>(&self, me: &MergeEnd<'_>, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, (at, arm, l_redo, l_bs, l_join): (&RowVals, &RowVals, Block, Option<Block>, Block), next: &mut Option<Next<'_, 'e>>) {
        b.switch_to(l_redo);
        let site = newest(me, b, env, at);
        let types: PgVec<'_, Type> = at.carry.params().iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
        let c = match &self.form {
            // the write's row over the newest version, its position the newest version's (the driving locator's)
            Form::Own => self.epq.lower(b, env, at, site, me.node, &mut |b, env, r| {
                let cols = me.atts.iter().map(|a| a.as_ref().filter(|a| a.resno > 0).and_then(|a| r.cols.get(a.resno as usize - 1).copied().flatten())).collect_in(env.mcx);
                let arm2 = RowVals { row: Row { cells: mcx::to_vec_in(&r.row.cells[..], env.mcx) }, vals: mcx::to_vec_in(&r.vals[..], env.mcx), cols, carry: r.carry.copy_in(env.mcx), next: r.next, at: r.at };
                let loc = env.loc.replace(newest_loc(b, env, site));
                let inner = env.inner.replace(mcx::to_vec_in(&r.cols[..], env.mcx));
                let l_j = b.create_block(&types);
                me.acts(b, env, &me.actions, (r, &arm2, None, l_j), next, Some(l_redo));
                b.switch_to(l_j);
                (env.loc, env.inner) = (loc, inner);
                Carry::from_params(b.block_params(l_j), env.mcx)
            }),
            // the build side's row over the newest version: a match runs the actions over the newest cells, a miss the NOT
            // MATCHED actions over the source row (none: nothing)
            Form::Built { keys, cols, tid } => self.epq.lower(b, env, at, site, me.node, &mut |b, env, rb| {
                let (l_m, l_nm, l_j) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&types));
                let inner = env.inner.as_deref().map(|v| mcx::to_vec_in(v, env.mcx));
                if keys.reaches(env) {
                    let hit = keys.matched(b, env, me.node, rb, l_nm);
                    b.cond_br(hit, (l_m, &[]), (l_nm, &[]));
                    b.switch_to(l_m);
                    env.memo.clear();
                    let newest = epq_ir::site_row(b, env, site, &self.atts, at);
                    let (at2, arm2) = rebuilt(me, at, &newest, cols, tid.0, env.mcx);
                    env.inner = Some(mcx::to_vec_in(&at2.cols[..], env.mcx));
                    me.acts(b, env, &me.actions, (&at2, &arm2, None, l_j), next, Some(l_redo));
                } else {
                    // a LEFT probe's NULL-extended row (hashjoin probe_fill.rs `miss`, where the probe holds no record image):
                    // its junk ctid is NULL, so no MATCHED action reaches the redo; a row with one (none) stops by name
                    let Some((_, valid)) = crate::ops::modifytable::merge::nm::junk(b, env, at, tid, me.node) else { crate::refuse!(me.node, Reason::Bug { payload: "a hashed MERGE target without its ctid junk column" }) };
                    let l_bad = b.create_block(&[]);
                    b.mark_cold(l_bad);
                    b.cond_br(valid, (l_bad, &[]), (l_nm, &[]));
                    b.switch_to(l_bad);
                    let (x, uns) = (env.x, b.iconst(Type::I64, unsupported(UNSUPPORTED_SHAPE)));
                    crate::ops::modifytable::rowop::stop_at(b, &x, at, uns);
                }
                b.switch_to(l_nm);
                env.inner = Some(mcx::to_vec_in(&at.cols[..], env.mcx));
                env.memo.clear();
                let c = match me.nm.is_empty() {
                    true => at.carry.copy_in(env.mcx),
                    false => crate::ops::modifytable::merge::nm::chain(b, env, (&me.nm, me.ex, me.node, me.atts.len(), me.mt), (at, arm), &*me.op, me.ret.as_deref(), next),
                };
                b.br(l_j, &c.params());
                b.switch_to(l_j);
                env.inner = inner;
                Carry::from_params(b.block_params(l_j), env.mcx)
            }),
            // the newest version's probe (SORTED: sorted inner) row joined to the source columns the step's row carries: the
            // hash (merge) clauses pass runs the actions over the projection's target columns made again, a failure the NOT
            // MATCHED actions
            Form::Drain(d) => self.epq.lower(b, env, at, site, me.node, &mut |b, env, r| {
                let (l_m, l_nm, l_j) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&types));
                let (mut row2, map) = match d.side {
                    INNER_VAR => with_source(r, at, &d.carried, env.mcx),
                    _ => with_target(r, at, &d.carried, env.mcx),
                };
                held_source(env, &mut row2, d, me.node);
                let inner = env.inner.replace(map);
                env.memo.clear();
                // PROBED's LEFT probe: no clause test, the join condition below decides (a failed one is BY SOURCE)
                for &q in d.clauses.iter().filter(|_| !d.bs) {
                    let (pass, l_on) = (d.ex.0.pred(b, env, d.probe, &row2, q), b.create_block(&[]));
                    b.cond_br(pass, (l_on, &[]), (l_nm, &[]));
                    b.switch_to(l_on);
                }
                b.br(l_m, &[]);
                b.switch_to(l_m);
                let (at2, arm2) = remade(me, b, env, d, at, &row2);
                let loc = if d.own { env.loc.replace(newest_loc(b, env, site)) } else { env.loc };
                // BY SOURCE (ExecMergeMatched's recheck): the join condition over the newest version and the step's own
                // source columns (its plan slot) true runs the MATCHED actions again, else the BY SOURCE ones
                let l_split = if d.bs { crate::ops::modifytable::merge::nm::split(b, env, (&me.cond, me.ex, me.node), &arm2) } else { None };
                me.acts(b, env, &me.actions, (&at2, &arm2, None, l_j), next, Some(l_redo));
                if let Some(l) = l_split {
                    b.switch_to(l);
                    env.inner = Some(mcx::to_vec_in(&at2.cols[..], env.mcx));
                    env.memo.clear();
                    let Some(lb) = l_bs else { crate::refuse!(me.node, Reason::Bug { payload: "a BY SOURCE MERGE redo without its BY SOURCE loop" }) };
                    me.acts(b, env, &me.bs, (&at2, &arm2, None, l_j), next, Some(lb));
                }
                env.loc = loc;
                b.switch_to(l_nm);
                env.inner = Some(mcx::to_vec_in(&at.cols[..], env.mcx));
                env.memo.clear();
                let c = match me.nm.is_empty() {
                    true => at.carry.copy_in(env.mcx),
                    false => crate::ops::modifytable::merge::nm::chain(b, env, (&me.nm, me.ex, me.node, me.atts.len(), me.mt), (at, arm), &*me.op, me.ret.as_deref(), next),
                };
                b.br(l_j, &c.params());
                b.switch_to(l_j);
                env.inner = inner;
                Carry::from_params(b.block_params(l_j), env.mcx)
            }),
            // the newest version's cells for the target's carried columns and its ctid: the join condition over them and the
            // step's own source columns (the plan slot) runs the MATCHED actions again, else the BY SOURCE ones
            Form::BuiltRight { cols, tid } => self.epq.lower(b, env, at, site, me.node, &mut |b, env, _| {
                let l_j = b.create_block(&types);
                let inner = env.inner.as_deref().map(|v| mcx::to_vec_in(v, env.mcx));
                env.memo.clear();
                let newest = epq_ir::site_row(b, env, site, &self.atts, at);
                let (at2, arm2) = rebuilt(me, at, &newest, cols, tid.0, env.mcx);
                env.inner = Some(mcx::to_vec_in(&at2.cols[..], env.mcx));
                env.memo.clear();
                let l_split = crate::ops::modifytable::merge::nm::split(b, env, (&me.cond, me.ex, me.node), &arm2);
                me.acts(b, env, &me.actions, (&at2, &arm2, None, l_j), next, Some(l_redo));
                if let Some(l) = l_split {
                    b.switch_to(l);
                    env.inner = Some(mcx::to_vec_in(&at2.cols[..], env.mcx));
                    env.memo.clear();
                    let Some(lb) = l_bs else { crate::refuse!(me.node, Reason::Bug { payload: "a BY SOURCE MERGE redo without its BY SOURCE loop" }) };
                    me.acts(b, env, &me.bs, (&at2, &arm2, None, l_j), next, Some(lb));
                }
                b.switch_to(l_j);
                env.inner = inner;
                Carry::from_params(b.block_params(l_j), env.mcx)
            }),
            // the first arm's row joined to the newest version: a pass runs the actions over the write's row made again over
            // it (the step's own source columns), a failure the NOT MATCHED actions over the step's row
            Form::First(f, tid) => {
                let (l_nm, l_j) = (b.create_block(&types), b.create_block(&types));
                let inner = env.inner.as_deref().map(|v| mcx::to_vec_in(v, env.mcx));
                if f.reaches(env) {
                    let c = f.lower(b, env, at, (site, me.node), l_nm, &mut |b, env, r| {
                        let cols = me.atts.iter().map(|a| a.as_ref().filter(|a| a.resno > 0).and_then(|a| r.cols.get(a.resno as usize - 1).copied().flatten())).collect_in(env.mcx);
                        let arm2 = RowVals { row: Row { cells: mcx::to_vec_in(&r.row.cells[..], env.mcx) }, vals: mcx::to_vec_in(&r.vals[..], env.mcx), cols, carry: r.carry.copy_in(env.mcx), next: r.next, at: r.at };
                        let inner = env.inner.replace(mcx::to_vec_in(&r.cols[..], env.mcx));
                        let l_m = b.create_block(&types);
                        me.acts(b, env, &me.actions, (r, &arm2, None, l_m), next, Some(l_redo));
                        b.switch_to(l_m);
                        env.inner = inner;
                        Carry::from_params(b.block_params(l_m), env.mcx)
                    });
                    b.br(l_j, &c.params());
                } else {
                    // the nest's NULL-extended row (its outer row not held for the redo): its junk ctid is NULL, so no
                    // MATCHED action reaches the redo; a row with one (none) stops by name
                    let Some((_, valid)) = crate::ops::modifytable::merge::nm::junk(b, env, at, tid, me.node) else { crate::refuse!(me.node, Reason::Bug { payload: "a union-outer MERGE target without its ctid junk column" }) };
                    let l_bad = b.create_block(&[]);
                    b.mark_cold(l_bad);
                    b.cond_br(valid, (l_bad, &[]), (l_nm, &at.carry.params()));
                    b.switch_to(l_bad);
                    let (x, uns) = (env.x, b.iconst(Type::I64, unsupported(UNSUPPORTED_SHAPE)));
                    crate::ops::modifytable::rowop::stop_at(b, &x, at, uns);
                }
                b.switch_to(l_nm);
                env.inner = Some(mcx::to_vec_in(&at.cols[..], env.mcx));
                env.memo.clear();
                let c = match me.nm.is_empty() {
                    true => at.carry.copy_in(env.mcx),
                    false => crate::ops::modifytable::merge::nm::chain(b, env, (&me.nm, me.ex, me.node, me.atts.len(), me.mt), (at, arm), &*me.op, me.ret.as_deref(), next),
                };
                b.br(l_j, &c.params());
                b.switch_to(l_j);
                env.inner = inner;
                Carry::from_params(b.block_params(l_j), env.mcx)
            }
        };
        env.memo.clear();
        b.br(l_join, &c.params());
        if let Some(l) = l_bs {
            self.by_source(me, b, env, (at, l, l_join), next);
        }
    }

    /// PROBED's BY SOURCE loop at `l_bs`: a BY SOURCE action's RECHECK (ExecMergeMatched's TM_Updated leg for an action
    /// that was not MATCHED) runs no EvalPlanQual; the BY SOURCE actions run again from the first over the newest version,
    /// the step's source columns (NULL) standing.
    fn by_source<'e>(&self, me: &MergeEnd<'_>, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, (at, l_bs, l_join): (&RowVals, Block, Block), next: &mut Option<Next<'_, 'e>>) {
        b.switch_to(l_bs);
        let site = newest(me, b, env, at);
        let types: PgVec<'_, Type> = at.carry.params().iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
        let c = self.epq.lower(b, env, at, site, me.node, &mut |b, env, r| {
            let l_j = b.create_block(&types);
            let inner = env.inner.as_deref().map(|v| mcx::to_vec_in(v, env.mcx));
            env.memo.clear();
            // PROBED: the projection's target-side columns again over the newest version (the actions write at its locator);
            // BUILT-RIGHT: its cells for the carried columns and its ctid
            let ((at2, arm2), loc) = match &self.form {
                Form::Drain(d) => {
                    let (row2, map) = with_source(r, at, &d.carried, env.mcx);
                    env.inner = Some(map);
                    env.memo.clear();
                    (remade(me, b, env, d, at, &row2), env.loc.replace(newest_loc(b, env, site)))
                }
                Form::BuiltRight { cols, tid } => {
                    let newest = epq_ir::site_row(b, env, site, &self.atts, at);
                    let made = rebuilt(me, at, &newest, cols, tid.0, env.mcx);
                    env.inner = Some(mcx::to_vec_in(&made.0.cols[..], env.mcx));
                    env.memo.clear();
                    (made, env.loc)
                }
                _ => crate::refuse!(me.node, Reason::Bug { payload: "a BY SOURCE MERGE redo outside PROBED and BUILT-RIGHT" }),
            };
            me.acts(b, env, &me.bs, (&at2, &arm2, None, l_j), next, Some(l_bs));
            b.switch_to(l_j);
            (env.loc, env.inner) = (loc, inner);
            Carry::from_params(b.block_params(l_j), env.mcx)
        });
        env.memo.clear();
        b.br(l_join, &c.params());
    }
}

/// A redo loop's head: the host puts the version its RECHECK locked in the write site (WRITE_EPQ_ROW), its error or an
/// odd status stops the row; the write site.
fn newest(me: &MergeEnd<'_>, b: &mut FunctionBuilder<'_>, env: &mut Env, at: &RowVals) -> usize {
    crate::frame::cancel::check(b, env.ctx, env.x.l_cancel);
    let site = me.op.site();
    let st = crate::ops::modifytable::rowop::host_op(b, env, site, WRITE_EPQ_ROW);
    let (dn, l_bad, l_on) = (b.iconst(Type::I64, WRITE_DONE), b.create_block(&[]), b.create_block(&[]));
    b.mark_cold(l_bad);
    let ok = b.icmp(IntCond::Eq, st, dn);
    b.cond_br(ok, (l_on, &[]), (l_bad, &[]));
    b.switch_to(l_bad);
    let (neg, uns) = (b.icmp(IntCond::Slt, st, env.zero), b.iconst(Type::I64, unsupported(UNSUPPORTED_SHAPE)));
    let err = b.select(neg, st, uns);
    let x = env.x;
    crate::ops::modifytable::rowop::stop_at(b, &x, at, err);
    b.switch_to(l_on);
    env.memo.clear();
    site
}

/// DRAIN / SORTED / PROBED: step row `at` with the projection's target-side columns made again over joined row `row2`
/// (`Drained::proj`), and its actions' row; `Env::inner` the remade row's columns.
fn remade<'x, 'e: 'x>(me: &MergeEnd<'_>, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, d: &Drained, at: &RowVals<'x>, row2: &RowVals) -> (RowVals<'x>, RowVals<'x>) {
    env.memo.clear();
    let t = b.bconst(true);
    let mut at2 = RowVals { row: Row { cells: mcx::to_vec_in(&at.row.cells[..], env.mcx) }, vals: mcx::to_vec_in(&at.vals[..], env.mcx), cols: mcx::to_vec_in(&at.cols[..], env.mcx), carry: at.carry.copy_in(env.mcx), next: at.next, at: at.at };
    for &(i, x, out) in d.proj.iter() {
        let (v, valid) = d.ex.0.emit(b, env, d.at, row2, x);
        let c = at2.row.cells.len() as u16;
        at2.row.cells.push_in(Cell::of_expr(out, c, c, Nullable::Maybe));
        at2.vals.push_in((v, Some(b.int_bin(IntBinOp::Xor, valid, t))));
        if i >= at2.cols.len() {
            at2.cols.resize_in(i + 1, None);
        }
        at2.cols[i] = Some(c);
    }
    let acols = me.atts.iter().map(|a| a.as_ref().filter(|a| a.resno > 0).and_then(|a| at2.cols.get(a.resno as usize - 1).copied().flatten())).collect_in(env.mcx);
    let arm2 = RowVals { row: Row { cells: mcx::to_vec_in(&at2.row.cells[..], env.mcx) }, vals: mcx::to_vec_in(&at2.vals[..], env.mcx), cols: acols, carry: at2.carry.copy_in(env.mcx), next: at2.next, at: at2.at };
    env.inner = Some(mcx::to_vec_in(&at2.cols[..], env.mcx));
    env.memo.clear();
    (at2, arm2)
}

/// DRAIN: probe row `r` joined to step row `at`'s source copies (`inner`: INNER_VAR attno, resno), the joined row and its
/// INNER_VAR map (`Env::inner`); a carried cell lives for the row (nest.loop's joined row, whose inner cells do too).
fn with_source<'r, 'm>(r: &RowVals<'r>, at: &RowVals, inner: &[(i16, i16)], mcx: Mcx<'m>) -> (RowVals<'r>, PgVec<'m, Option<u16>>) {
    let j = joined(r, at, mcx::to_vec_in(&r.cols[..], *r.vals.allocator()));
    let mut map = mcx::pg_vec![in mcx; None; inner.iter().map(|x| x.0 as usize).max().unwrap_or(0)];
    source_map(r, at, inner, &mut map);
    (j, map)
}

/// `with_source`'s joined row, its columns `cols`: probe row `r`'s cells, then step row `at`'s.
fn joined<'x>(r: &RowVals<'x>, at: &RowVals, cols: PgVec<'x, Option<u16>>) -> RowVals<'x> {
    use gravity_program::Until;
    let m = *r.vals.allocator();
    let mut j = RowVals { row: Row { cells: mcx::to_vec_in(&r.row.cells[..], m) }, vals: mcx::to_vec_in(&r.vals[..], m), cols, carry: at.carry.copy_in(m), next: r.next, at: r.at };
    for (c, &v) in at.row.cells.iter().zip(at.vals.iter()) {
        let mut c = *c;
        if matches!(c.life.until, Until::Window | Until::Part | Until::Life) {
            c.life.until = Until::Row;
        }
        j.row.cells.push_in(c);
        j.vals.push_in(v);
    }
    j
}

/// `with_source`'s INNER_VAR map: attno `a` is the joined row's cell for `at`'s column `resno`.
fn source_map(r: &RowVals, at: &RowVals, inner: &[(i16, i16)], map: &mut [Option<u16>]) {
    let base = r.row.cells.len();
    for &(a, resno) in inner.iter() {
        let cell = usize::try_from(resno).ok().and_then(|x| x.checked_sub(1)).and_then(|i| at.cols.get(i).copied().flatten());
        map[a as usize - 1] = cell.map(|k| (base + k as usize) as u16);
    }
}

/// SORTED (D3379, mergecmd_rowc_cte :48): each source column `Drained::held` names, from the merge join's outer row (its
/// node's `Env::redo_outers` entry: the source row the join read, the version EvalPlanQualFetchRowMark refetches under
/// SnapshotAny) past joined row `j`'s cells, as `j`'s OUTER_VAR column.
fn held_source(env: &Env, j: &mut RowVals, d: &Drained, node: i32) {
    use gravity_program::Until;
    if d.held.is_empty() {
        return;
    }
    let Some((cells, vals, cols, _)) = env.redo_outers.iter().rev().find(|x| x.0 == d.probe).map(|x| &x.1) else { crate::refuse!(node, Reason::Shape { why: HELD }) };
    for &a in d.held.iter() {
        let i = a as usize - 1;
        let Some(k) = cols.get(i).copied().flatten().map(usize::from).filter(|&k| k < cells.len() && k < vals.len()) else { crate::refuse!(node, Reason::Shape { why: HELD }) };
        let mut c = cells[k];
        if matches!(c.life.until, Until::Window | Until::Part | Until::Life) {
            c.life.until = Until::Row;
        }
        let at = j.row.cells.len() as u16;
        j.row.cells.push_in(c);
        j.vals.push_in(vals[k]);
        if i >= j.cols.len() {
            j.cols.resize_in(i + 1, None);
        }
        j.cols[i] = Some(at);
    }
}

/// SORTED: the newest version's row `r` (the sorted inner's columns, the joined row's INNER_VAR map) joined to step row
/// `at`'s source copies (`outer`: OUTER_VAR attno, resno) as the joined row's columns.
fn with_target<'x, 'm: 'x>(r: &RowVals<'x>, at: &RowVals, outer: &[(i16, i16)], mcx: Mcx<'m>) -> (RowVals<'x>, PgVec<'m, Option<u16>>) {
    let mut map = mcx::pg_vec![in mcx; None::<u16>; outer.iter().map(|x| x.0 as usize).max().unwrap_or(0)];
    source_map(r, at, outer, &mut map);
    let inner = mcx::to_vec_in(&r.cols[..], mcx);
    (joined(r, at, map), inner)
}

/// BUILT: the step's row `at` with the newest version's cells (`newest`, the write site's row by attno and its ctid) after
/// its own, each carried target column `(attno, resno)` and the junk ctid (`tid`) read from them; and the actions' row
/// (each carried attribute by attno, as merge/lower.rs `run` lays it).
fn rebuilt<'x, 'm: 'x>(me: &MergeEnd<'_>, at: &RowVals<'x>, newest: &RowVals, cols: &[(i16, i16)], tid: i16, mcx: Mcx<'m>) -> (RowVals<'x>, RowVals<'x>) {
    let base = at.row.cells.len();
    let mut r = RowVals { row: Row { cells: mcx::to_vec_in(&at.row.cells[..], mcx) }, vals: mcx::to_vec_in(&at.vals[..], mcx), cols: mcx::to_vec_in(&at.cols[..], mcx), carry: at.carry.copy_in(mcx), next: at.next, at: at.at };
    r.row.cells.extend_in(newest.row.cells.iter().copied());
    r.vals.extend_in(newest.vals.iter().copied());
    let of = |a: i16| usize::try_from(a).ok().and_then(|a| a.checked_sub(1)).and_then(|i| newest.cols.get(i).copied().flatten()).map(|c| (base + c as usize) as u16);
    let mut set = |resno: i16, cell: Option<u16>| {
        if let (Some(i), Some(c)) = (usize::try_from(resno).ok().and_then(|x| x.checked_sub(1)), cell) {
            if i >= r.cols.len() {
                r.cols.resize_in(i + 1, None);
            }
            r.cols[i] = Some(c);
        }
    };
    for &(attno, resno) in cols.iter() {
        set(resno, of(attno));
    }
    set(tid, of(CTID_AT));
    let acols = me.atts.iter().enumerate().map(|(i, a)| a.as_ref().filter(|a| a.resno > 0).and_then(|_| of(i as i16 + 1))).collect_in(mcx);
    let arm = RowVals { row: Row { cells: mcx::to_vec_in(&r.row.cells[..], mcx) }, vals: mcx::to_vec_in(&r.vals[..], mcx), cols: acols, carry: r.carry.copy_in(mcx), next: r.next, at: r.at };
    (r, arm)
}

/// The write site's position (WRITE_EPQ_ROW stored the newest version's) as a heap locator, OWN's driving position.
fn newest_loc(b: &mut FunctionBuilder<'_>, env: &Env, at: usize) -> Locator {
    let s = site_off(at) as i32;
    let (w0, blk) = (b.load(Type::I32, env.ctx, s + site::POS_FLAGS as i32), b.load(Type::I32, env.ctx, s + site::POS_BLOCK as i32));
    let sixteen = b.iconst(Type::I32, 16);
    let off = b.int_bin(IntBinOp::Ushr, w0, sixteen);
    Locator { block: b.conv(ConvOp::Zext, Type::I64, blk), offset: b.conv(ConvOp::Zext, Type::I64, off), pinned: None, rel: None }
}
