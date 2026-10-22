//! D1933 (W6-MERGE-NEST, M8-WRITE; design line batches/v4-m8-write-notes/d1933-w6-merge-nest-design.txt, approved with
//! conditions by COV-WRITE's design review 09-29): a MERGE over a nested loop's rows. PostgreSQL 18.6 plans a MERGE whose
//! source is a pulled-up constant (`USING (SELECT 1 AS k)`, a one-row VALUES: a Result) as a nested loop, the Result
//! outer and the target's own scan inner (a seq scan filtering the ON key as a constant, or an index scan), LEFT under a
//! NOT MATCHED [BY TARGET] action, else INNER. Its EvalPlanQual (ExecMergeMatched on TM_Updated) runs the plan again with
//! the target's scan returning the locked newest version alone: the Result's row again, the inner's filters, the join
//! qual and the actions over the new version; a version that no longer joins is null-extended (LEFT: the NOT MATCHED
//! actions over the source row) or yields nothing (INNER).
//!
//! Here (`merge`): the write's position is the target's junk ctid and a recheck names the inner's scan site (write/merge.rs
//! `LocFrom::JunkCtid`, D776's form; nest/inner.rs records the inner's site under its view index, which `merge` returns
//! as the target's "producer"). The recheck node (exec epq.rs, driver `Entry::recheck`) substitutes that site with the
//! newest version: the outer Result yields its one row, so the inner opens once and its substituted source hands that
//! version through the inner's filters (an index source re-applies its keys, IndexRecheck) and the join qual. A view
//! target's write (its INSTEAD OF triggers) never rechecks (write/rowop.rs `claim`): a nest of any served outer and inner.
//! Refused by name (the constants below), each a later row of D1933: a COPY-marked row (S1 of the review: decided by the
//! mark, never by the plan shape), another marked relation (but a REFERENCE-marked one the nest's outer scans itself:
//! mergecmd 13a, refetched at its ctid as D776's own scan; and the REFERENCE-marked inner scans of the target's own scan
//! as the outer: mergecmd MOVE (c), nest_ref.rs's chain), an outer that is not a one-row Result (or such a scan), an
//! inner that is not one heap range / index scan of the target under filters and projections, NOT MATCHED BY SOURCE
//! over a nest, and an
//! InitPlan beside a recheck in the IR whose re-run the effects licence denies (it acts) or that can raise and is read
//! under the write (D2028, `rerun_init`: the runner's recheck reads InitPlans as EvalPlanQualStart does); NOT
//! MATCHED into a target with a BEFORE ROW UPDATE / DELETE trigger serves since D2042's host fix. Both served forms refuse last a call under no
//! collation (D2046, `loose`): PostgreSQL raises 42P22 at it where the served kernels answer (DEFERRED to the bug phase).
//! A grouping beside a hash or merge join anywhere in the statement serves: a grouping over a lazily run build keeps its
//! end's state across the probe's resume (D2194 / D2147 / D2201, fixed; the guard stayed while D2325 was open).
//! D3508: or an UPDATE / DELETE, or a MERGE of WHEN MATCHED actions only, whose nest's outer is a settled UNION ALL and
//! whose inner is the target's index scan (`union_outer`): the recheck runs the union's members again.
use gravity_decompose::expr::{through_relabel, walk};
use gravity_decompose::catalog::expr_type::{collation_of, type_of};
use gravity_decompose::effects::{licence, Departure, Effects};
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::sort::indeterminate;
use gravity_decompose::ops::shared::subplan::SubLink;
use gravity_decompose::ops::shared::modifytable::{MergeDesc, Member, RowMark, WriteDesc};
use gravity_decompose::{Am, Bound, Op, PipelineNode, Source};
use gravity_program::Cmd;
use types_nodes::jointype::JoinType;
use types_nodes::plannodes::RowMarkType;
use types_nodes::list::NodeList;
use types_nodes::primnodes::{BoolExprType, MergeMatchKind, ParamKind};
use types_nodes::{Node, NodeTag};

use crate::graph::Gx;
use crate::admit::refusal::Reason;

use crate::ops::modifytable::merge::admit::shape;

/// The refusals by name (module doc).
pub(crate) const BESIDE: &str = "a write over a nested loop beside another join, a set function, a SubPlan, a LIMIT or an Append arm (D1933)";
pub(crate) const JOINTYPE: &str = "a MERGE over a nested loop of a join type other than INNER or a NOT MATCHED action's LEFT (D1933)";
pub(crate) const BY_SOURCE: &str = "MERGE: NOT MATCHED BY SOURCE over a nested loop (D1933)";
pub(crate) const MATERIAL: &str = "a write over a nested loop whose inner is a Material or another breaker (D1933 (b))";
pub(crate) const NESTS: &str = "a write over nested loops (a nest in the inner, or two nests: D1933)";
pub(crate) const INNER_OPS: &str = "a write over a nested loop whose inner runs a SubPlan, a set function, a LIMIT, a gate or a join (D1933)";
pub(crate) const COPY: &str = "a write over a nested loop reading a VALUES, subquery or function row (its COPY row at a recheck without producers: D1933 F2 / F3)";
pub(crate) const MARKED: &str = "a write over a nested loop joining the target to another marked relation (its REFERENCE row's position at the recheck: D1933 (a) / (d))";
pub(crate) const OUTER: &str = "a write over a nested loop whose outer is not a one-row Result (D1933)";
pub(crate) const INNER: &str = "a write over a nested loop whose inner is no heap range or index scan of the target (D1933)";
pub(crate) const BITMAP: &str = "a write over a nested loop whose inner is a bitmap scan of the target (its substituted row's recheck qual: D1933)";
pub(crate) const RECHECK_INITPLAN: &str = "a write's recheck beside an InitPlan that is volatile, reads a product, or can raise and is read under the write (the recheck runs it again where PostgreSQL copies or forces its param: D2028)";
pub(crate) const INDETERMINATE_COLLATION: &str = "a MERGE over a nested loop comparing under an indeterminate collation (PostgreSQL raises 42P22: D2046)";

/// Pipeline `k`'s MERGE `w` over a nested loop: `None` when its rows reach the write through no nest (merge_admit.rs's
/// chain); else the target's "producer" for write/merge.rs, the inner's view index (its scan site the recheck names), or
/// `None` for a view target; every other form refused by name.
pub(crate) fn merge(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>, m: &MergeDesc<'_>, node: i32) -> Option<Option<usize>> {
    let p = &gx.pipelines()[k];
    let pre = || p.ops.iter().take_while(|o| !matches!(o, Op::Write(_)));
    let j = pre().position(|o| matches!(o, Op::Join(JoinDesc::Nest { .. })))?;
    // D2015 NEST-ARMS: or the result relations' arms of a union the nest's inner embeds (merge/nest_arms.rs)
    if let Some(feeds) = crate::ops::modifytable::merge::nest_arms::arms(gx, k, w) {
        return Some(Some(crate::ops::modifytable::merge::nest_arms::admit(gx, (k, j), w, m, node, feeds[0])));
    }
    // G2(c) part 2 (mergecmd): the target rows out of a settled union of the targets' feeds (`union_nest`)
    if let Some(feeds) = crate::ops::modifytable::merge::route::union_targets(gx, k, w) {
        return Some(Some(union_nest(gx, (k, j), w, m, node, feeds[0])));
    }
    // the rows reach the write through filters, projections, gates and this one nest (then RETURNING's projection)
    let nests = pre().filter(|o| matches!(o, Op::Join(JoinDesc::Nest { .. }))).count();
    if nests > 1 {
        shape(node, NESTS);
    }
    // mergecmd insert-only nest: a MERGE whose only actions insert (or do nothing) never rechecks (a MATCHED row has no
    // action, a NOT MATCHED one inserts a new row), so the recheck's shapes below (the ops beside the nest, the inner,
    // the marks) do not apply; the target's junk ctid alone tells the matched rows (merge/lower.rs `positionless`)
    // D3367: or the target's side the planner's dummy relation (`dummy_side`)
    let free = positionless(w, m) || dummy_side(gx, k);
    // mergecmd 13b: the ops before a stored COPY row's entry (`own_copy_at`: a subquery's own LIMIT over its Sort, under
    // the nest's outer) are the subquery's; the recheck enters at the entry with the stored row and never runs them
    let copy = |mk: &RowMark| mk.mark == RowMarkType::ROW_MARK_COPY && !w.targets.iter().any(|t| t.rti == mk.rti);
    let entry = w.mark_descs.iter().filter(|mk| copy(mk)).filter_map(|mk| crate::rerun::epq::marks::own_copy_at(gx, k, mk.rti)).filter(|&at| at <= j).min().unwrap_or(0);
    // D1933 ACTION-SUB (regress with#239 / #241): or a SubPlan step only the actions read, lent lazily at the
    // action's read (merge/admit.rs `action_sub`, D1112): it carries nothing the nest's row needs, and the recheck steps
    // past its rerun set and re-lends it over the target's newest version (marks.rs `acts`), as EvalPlanQual re-executes it
    let act = |o: &Op<'_>| crate::ops::modifytable::merge::admit::action_sub(o, w);
    // D3508: or the Union op of a settled UNION ALL outer, first (`union_outer`)
    let union = gx.pipelines().inner_of(k, j).is_some_and(|ik| union_outer(gx, k, w, ik));
    if !free && pre().enumerate().any(|(i, o)| i != j && i >= entry && !matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_)) && !act(o) && !(union && i == 0)) {
        shape(node, BESIDE);
    }
    let Op::Join(JoinDesc::Nest { jointype, .. }) = &p.ops[j] else { crate::refuse!(node, Reason::Bug { payload: "a nest op that is not a nest" }) };
    let kind = |x: MergeMatchKind| m.actions.iter().any(|a| a.kind == x);
    // D1933 (b): the target's own scan outer over a Material inner (`material` (a)) takes BY SOURCE's LEFT join
    let bs = kind(MergeMatchKind::MERGE_WHEN_NOT_MATCHED_BY_SOURCE);
    // V-COPY: or the COPY-marked VALUES scan's (`values_fill`)
    let fed = |ik: usize| matches!(gx.pipelines()[ik].source, Source::Breaker { slot } if material_fill(gx, k, slot).is_some_and(|(_, s)| !w.targets.iter().any(|t| t.rti == s.0)) || values_fill(gx, k, slot).is_some());
    let outer_a = gx.pipelines().inner_of(k, j).is_some_and(fed) && own_scan(&p.source, w) && !w.members.has(Member::TargetView);
    // BY-SOURCE-NEST (gen mg-0144): or MOVE (c)'s chain (below) over a LEFT nest whose inner is one heap range scan
    let left_chain = || *jointype == JoinType::JOIN_LEFT && own_scan(&p.source, w) && crate::ops::modifytable::nest_ref::left_range(&p.ops[j]) && crate::ops::modifytable::nest_ref::chain(gx, k, w);
    if bs && !((outer_a || left_chain()) && *jointype == JoinType::JOIN_LEFT) {
        shape(node, BY_SOURCE);
    }
    if !(*jointype == JoinType::JOIN_INNER || *jointype == JoinType::JOIN_LEFT && (kind(MergeMatchKind::MERGE_WHEN_NOT_MATCHED_BY_TARGET) || bs)) {
        shape(node, JOINTYPE);
    }
    let Some(ik) = gx.pipelines().inner_of(k, j) else { crate::refuse!(node, Reason::Bug { payload: "a nest without its embedded inner" }) };
    // D2046 (code review round 1, S1): the last refusal of each served form, so every shape above keeps its name
    let d2046 = || if loose(gx, k, ik, w, Some(m)) { shape(node, INDETERMINATE_COLLATION) };
    if free {
        d2046();
        return Some(None);
    }
    // V4R-353: a view target's INSTEAD OF triggers are the write, which never rechecks (no ctid, no mark read)
    if w.members.has(Member::TargetView) {
        d2046();
        return Some(None);
    }
    // D2042 (fixed in the host): a BEFORE ROW prologue's lock meeting a concurrently deleted row hands the source row to
    // the NOT MATCHED actions, as PG 18.6's ExecMergeMatched does (hosts trigger_before.rs `Pre::NotMatched`)
    let ip = &gx.pipelines()[ik];
    // D1933 (b): a Material inner (`material`)
    if let Source::Breaker { slot } = ip.source {
        return Some(material(gx, (k, j, ik), w, (m, *jointype), node, slot));
    }
    // MOVE (c) (merge#377): the target's own scan the outer of an INNER nest whose inner is one heap scan of a
    // REFERENCE-marked relation under filters and projections (nest_ref.rs `chain`, an UPDATE's producerchain form; a
    // root pruned to one leaf too, merge/route.rs). PostgreSQL 18.6's EvalPlanQual reruns the plan with the target's scan
    // returning the newest version alone and the inner's scan the row its junk ctid names (EvalPlanQualFetchRowMark),
    // the inner's quals and the join qual checked again over them, then the whole action list (ExecMergeMatched); no row
    // is NOT MATCHED, which an INNER join's actions never name. Here the write's position is the driving scan's (merge/
    // lower.rs `LocFrom::Driving`) and write/rowop.rs `claim` stores the inner's REFERENCE row at its RowOf site (marks.rs
    // `sites_in` with inners), which the recheck opens once over the substituted outer row
    // BY-SOURCE-NEST: or BY SOURCE's LEFT nest (`left_chain`): a NOT MATCHED BY SOURCE row's inner row is
    // stored empty (marks.rs `inner_pos`, null_empty), so the recheck null-extends it again over the target's newest
    // version; a MATCHED row whose refetched inner row no longer joins comes null-extended, and the host's MERGE takes the
    // NOT MATCHED BY SOURCE actions for it, as ExecMergeMatched does
    if own_scan(&p.source, w) && (*jointype == JoinType::JOIN_INNER || bs && left_chain()) && crate::ops::modifytable::nest_ref::chain(gx, k, w) {
        if rerun_init(gx, gx.cte_write(k), &p.ops[..pre().count()], ik) {
            shape(node, RECHECK_INITPLAN);
        }
        d2046();
        return Some(None);
    }
    for o in ip.ops.iter() {
        match o {
            Op::Filter(_) | Op::Project(_) => {}
            // mergecmd tw-core-197: a Memoize over the inner's scan; the recheck's memo is a fresh, empty one (exec epq.rs
            // `recheck_on`, as EvalPlanQualStart's fresh Memoize), so its lookup misses and the substituted scan runs
            Op::Join(JoinDesc::Memo { .. }) => {}
            Op::Join(JoinDesc::Nest { .. }) => shape(node, NESTS),
            _ => shape(node, INNER_OPS),
        }
    }
    // S1: a COPY-marked row (a VALUES scan, a subquery or function row the planner kept: a volatile or OFFSET 0 target
    // list) is handed over whole by PG's recheck, never run again; a REFERENCE-marked one is refetched at its junk ctid.
    // mergecmd 9 (COPYROW): a COPY row the nest's outer takes (its own VALUES scan, or a subquery or function row's copy
    // entry below the nest) is served: marks.rs stores it per write (`copy_pos`) and the recheck enters the pipeline with
    // that stored row alone (values.rs / frame.rs `copy_entry` under POS_COPY), never running the source again
    // mergecmd 13a: a REFERENCE-marked relation the nest's outer scans itself (a plain source table) is refetched at its
    // junk ctid, the version the join read (EvalPlanQualFetchRowMark): marks.rs `nest_ref` stores it in the source's
    // marked-row site (D776's own form), so the recheck's outer yields that row alone, as the stored COPY row's does
    let target = |rti: u32| w.targets.iter().any(|t| t.rti == rti);
    let copied = |mk: &RowMark| mk.mark == RowMarkType::ROW_MARK_COPY && crate::rerun::epq::marks::own_copy_at(gx, k, mk.rti).is_some_and(|at| at <= j);
    // D3508: or the settled union's own COPY mark, whose members the recheck runs again (`union_outer`)
    let stored = |mk: &RowMark| copied(mk) || crate::rerun::epq::marks::nest_ref(gx, k, mk) || union && union_mark(gx, mk);
    if w.mark_descs.iter().any(|mk| !target(mk.rti) && mk.mark == RowMarkType::ROW_MARK_COPY && !stored(mk)) {
        shape(node, COPY);
    }
    if w.mark_descs.iter().any(|mk| !target(mk.rti) && !stored(mk)) {
        shape(node, MARKED);
    }
    // the recheck's outer yields at most one row, so the inner opens once over its substituted source (HeapSource and
    // BitmapSource hand the substituted row at their first open only): a Result, or the stored COPY row; or the settled
    // union's rows over an index scan inner (`union_outer`: IndexSource hands it at each open while it passes the keys)
    if !matches!(p.source, Source::Const) && !w.mark_descs.iter().any(|mk| !target(mk.rti) && stored(mk)) {
        shape(node, OUTER);
    }
    match &ip.source {
        Source::Range { am: Am::Heap, scanrelid, relid, .. } | Source::Index { am: Am::Heap, scanrelid, relid, only: false, .. } if u32::from(*scanrelid) == w.rti && *relid == w.plan.target_rel => {}
        Source::Bitmap { am: Am::Heap, scanrelid, .. } if u32::from(*scanrelid) == w.rti => shape(node, BITMAP),
        _ => shape(node, INNER),
    }
    // D2028 (`rerun_init`)
    if rerun_init(gx, gx.cte_write(k), &p.ops[..pre().count()], ik) {
        shape(node, RECHECK_INITPLAN);
    }
    d2046();
    Some(Some(ik))
}

/// D1933 (b): a MERGE over a nested loop whose inner is a Material (merge#241, publication#984). PostgreSQL 18.6
/// plans a MERGE of two small heap tables joined by a non-equality or unestimated key as a nested loop with a Materialize
/// inner over one relation's scan: (a) the target's own scan the outer and the source relation's scan under the Material,
/// or (b) the source relation's scan the outer and the target's scan under the Material. Its EvalPlanQual runs the plan
/// again in a fresh planstate tree (a fresh Materialize) with the target's scan returning the newest version alone and
/// the source's REFERENCE row refetched at its junk ctid (EvalPlanQualFetchRowMark), so the outer yields one row and the
/// Material refills once. Here the recheck reruns the Material's fill under its own empty spool (exec epq.rs, the
/// spool-scoped recheck): (a) the target's own scan substituted (`LocFrom::Driving`) and the fill at the source's stored
/// REFERENCE row (marks.rs: a producer's marked scan, D655); (b) the fill's scan of the target substituted (the producer
/// this returns: D776's `LocFrom::JunkCtid`) and the outer at the source's stored row (13a `nest_ref`, or a stored COPY
/// row). The fill: one heap range, index or bitmap heap scan under filters and projections into the Material, reading no
/// product; the inner its rows under filters and projections. (a) is INNER or BY SOURCE's LEFT join (merge#542); (b)
/// INNER or a NOT MATCHED action's LEFT join; any other mark by name; `RECHECK_INITPLAN`, `INDETERMINATE_COLLATION` and the no-widen list last.
/// D1933 (b) V-COPY (updatable_views#810 / #814: rw_view1's MERGE USING (VALUES ..), its Materialize over the
/// Values Scan kept by material V-QUIET): (a)'s fill may instead be the COPY-marked VALUES scan (`values_fill`). PostgreSQL
/// 18.6's recheck hands that Values Scan the stored row alone (EvalPlanQualFetchRowMark's copy leg), so the fresh
/// Materialize holds that one row; here the recheck reruns the fill over the row stored at its copy site (marks.rs
/// `copy_pos`, D732's VALUES producer) into the recheck's own empty spool (exec epq.rs `fresh_spools`).
fn material(gx: &Gx<'_, '_>, (k, j, ik): (usize, usize, usize), w: &WriteDesc<'_>, (m, jointype): (&MergeDesc<'_>, JoinType), node: i32, slot: u8) -> Option<usize> {
    let ps = gx.pipelines();
    let p = &ps[k];
    if !ps[ik].ops.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_))) {
        shape(node, INNER_OPS);
    }
    // V-COPY: (a)'s fill the COPY-marked VALUES scan, its relation id none
    let vfill = || values_fill(gx, k, slot).filter(|_| own_scan(&p.source, w)).map(|(q, rti)| (q, (rti, 0)));
    let Some((q, scanned)) = material_fill(gx, k, slot).or_else(vfill) else { shape(node, MATERIAL) };
    let own = |(rti, relid): (u32, u32)| rti == w.rti && relid == w.plan.target_rel;
    let target = |rti: u32| w.targets.iter().any(|t| t.rti == rti);
    let reference = |mk: &RowMark| mk.mark == RowMarkType::ROW_MARK_REFERENCE && !mk.parent && mk.ctid_col > 0 && mk.lock == gravity_program::LockKind::None;
    let values = matches!(ps[q].source, Source::Values { .. });
    let copied = |mk: &RowMark| values && mk.rti == scanned.0 && mk.mark == RowMarkType::ROW_MARK_COPY && !mk.parent && mk.ctid_col > 0;
    let built = if own_scan(&p.source, w) {
        // (a): the fill scans the one REFERENCE-marked source relation; every other relation's mark refused by name. BY
        // SOURCE's LEFT join: a null-extended row's source position is stored empty (marks.rs `null_empty`), so the
        // recheck's fill refills nothing and the row is null-extended again over the target's newest version
        let bs = m.actions.iter().any(|a| a.kind == MergeMatchKind::MERGE_WHEN_NOT_MATCHED_BY_SOURCE);
        if !(jointype == JoinType::JOIN_INNER || jointype == JoinType::JOIN_LEFT && bs) {
            shape(node, JOINTYPE);
        }
        if target(scanned.0) || !w.mark_descs.iter().any(|mk| (mk.rti, mk.relid) == scanned && reference(mk) || copied(mk)) {
            shape(node, MATERIAL);
        }
        for mk in w.mark_descs.iter().filter(|mk| !target(mk.rti) && !copied(mk)) {
            if mk.mark == RowMarkType::ROW_MARK_COPY {
                shape(node, COPY);
            }
            if (mk.rti, mk.relid) != scanned || !reference(mk) {
                shape(node, MARKED);
            }
        }
        None
    } else {
        // (b): the fill scans the target; the outer's row the stored one (`merge`'s marks and outer, over this nest)
        if !own(scanned) {
            shape(node, INNER);
        }
        let copied = |mk: &RowMark| mk.mark == RowMarkType::ROW_MARK_COPY && crate::rerun::epq::marks::own_copy_at(gx, k, mk.rti).is_some_and(|at| at <= j);
        let stored = |mk: &RowMark| copied(mk) || crate::rerun::epq::marks::nest_ref(gx, k, mk);
        if w.mark_descs.iter().any(|mk| !target(mk.rti) && mk.mark == RowMarkType::ROW_MARK_COPY && !stored(mk)) {
            shape(node, COPY);
        }
        if w.mark_descs.iter().any(|mk| !target(mk.rti) && !stored(mk)) {
            shape(node, MARKED);
        }
        if !matches!(p.source, Source::Const) && !w.mark_descs.iter().any(|mk| !target(mk.rti) && stored(mk)) {
            shape(node, OUTER);
        }
        Some(q)
    };
    let pre = p.ops.iter().take_while(|o| !matches!(o, Op::Write(_))).count();
    if rerun_init(gx, gx.cte_write(k), &p.ops[..pre], ik) {
        shape(node, RECHECK_INITPLAN);
    }
    if loose(gx, k, ik, w, Some(m)) {
        shape(node, INDETERMINATE_COLLATION);
    }
    built
}

/// D1933 (b): the pipeline filling Material slot `slot` under pipeline `k`'s nest and the `(rti, relid)` it scans, when it is
/// one heap range, index or bitmap heap scan under filters and projections into the Material, reading no product. A bitmap
/// fill's recheck hands the substituted row through its recheck quals and filter (nodeBitmapHeapscan.c
/// `BitmapHeapRecheck`), as a substituted index scan's goes through its index quals.
fn material_fill(gx: &Gx<'_, '_>, k: usize, slot: u8) -> Option<(usize, (u32, u32))> {
    let ps = gx.pipelines();
    let q = (0..ps.len()).find(|&q| q != k && ps[q].desc.produces() == Some(slot))?;
    let spool = ps[q].desc.consumes.is_empty() && matches!(ps[q].ops.split_last(), Some((Op::Join(JoinDesc::Spool), body)) if body.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_))));
    match &ps[q].source {
        Source::Range { am: Am::Heap, scanrelid, relid, .. } | Source::Index { am: Am::Heap, scanrelid, relid, only: false, .. } | Source::Bitmap { am: Am::Heap, scanrelid, relid, .. } if spool => {
            Some((q, (u32::from(*scanrelid), *relid)))
        }
        _ => None,
    }
}

/// D1933 (b) V-COPY: the pipeline filling Material slot `slot` under pipeline `k`'s nest and the range table index of
/// its VALUES scan, when it is that scan under filters and projections into the Material (material V-QUIET's eager form).
fn values_fill(gx: &Gx<'_, '_>, k: usize, slot: u8) -> Option<(usize, u32)> {
    let ps = gx.pipelines();
    let q = (0..ps.len()).find(|&q| q != k && ps[q].desc.produces() == Some(slot))?;
    let spool = ps[q].desc.consumes.is_empty() && matches!(ps[q].ops.split_last(), Some((Op::Join(JoinDesc::Spool), body)) if body.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_))));
    match &ps[q].source {
        Source::Values { scanrelid, .. } if spool => Some((q, u32::from(*scanrelid))),
        _ => None,
    }
}

/// producerchain cluster A: op `o` of pipeline `k` is an INNER nest over a Material spool one heap scan of the write's own
/// target fills (`material_fill`; rowtypes#168: Nested Loop (Function Scan, Materialize (Seq Scan target))). The spool's
/// fill is the target's one producer (merge/admit.rs `built_target`): the recheck reruns it at the target's newest version
/// (marks.rs `sites_in`), so the nest reads that one row, as EvalPlanQual's Materialize over the target's test tuple does.
pub(crate) fn target_spool(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>, o: &Op<'_>) -> bool {
    let Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER, inner, .. }) = o else { return false };
    let Source::Breaker { slot } = inner.source else { return false };
    inner.ops.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_))) && material_fill(gx, k, slot).is_some_and(|(_, s)| s == (w.rti, w.plan.target_rel))
}

/// MOVE (b): whether pipeline `k`'s first nest reads a Material that one heap range or index scan of the write's
/// own target fills (`material`'s form (b)): merge/route.rs lets a root pruned to that one leaf through as an unrouted
/// target, `merge` then admitting the nest or refusing it by name.
pub(crate) fn material_target(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>) -> bool {
    let p = &gx.pipelines()[k];
    let Some(j) = p.ops.iter().take_while(|o| !matches!(o, Op::Write(_))).position(|o| matches!(o, Op::Join(JoinDesc::Nest { .. }))) else { return false };
    let own = |s: (u32, u32)| s == (w.rti, w.plan.target_rel);
    gx.pipelines().inner_of(k, j).is_some_and(|ik| matches!(gx.pipelines()[ik].source, Source::Breaker { slot } if material_fill(gx, k, slot).is_some_and(|(_, s)| own(s))))
}

/// D1933 (b): whether `s` is the write's own heap range or index scan of its target (`material`'s form (a)).
fn own_scan(s: &Source<'_>, w: &WriteDesc<'_>) -> bool {
    matches!(s, Source::Range { am: Am::Heap, scanrelid, relid, .. } | Source::Index { am: Am::Heap, scanrelid, relid, only: false, .. } if u32::from(*scanrelid) == w.rti && *relid == w.plan.target_rel)
}

/// G2(c) part 2 (mergecmd): a MERGE whose target rows come out of a settled union of its result relations' feeds
/// (merge/route.rs `union_targets`), joined to its source by a nested loop. PostgreSQL 18.6 plans partition_prune's
/// `MERGE INTO part_abc_view pt USING (SELECT ..) q JOIN part_abc_1 pt1 ON .. ON pt.a = pt1.a` so: the Append of the
/// result relations the outer, a Material over the source relation's scan the inner. Its EvalPlanQual substitutes the
/// row's result relation's scan with the newest version and leaves the others empty (relsubs_blocked), so the Append
/// yields that version alone and the inner opens once: a fresh Material over the source's scan returning the row the join
/// read (EvalPlanQualFetchRowMark). Here the union's feeds rerun at the recheck, each empty but the row's own (merge/
/// lower.rs `rel`; the named feed at the host's newest version, D776's producer form), and the inner's producer reruns
/// at the source's stored REFERENCE row (D655). The form: the Union first, filters, gates and projections beside one INNER
/// nest (WHEN MATCHED actions only), its inner a breaker's rows under filters and projections whose producer is the one
/// REFERENCE-marked source relation's heap range or index scan under filters and projections into a Material; any other
/// mark refused by name; `RECHECK_INITPLAN`, `INDETERMINATE_COLLATION` and the no-widen list last. The target's "producer" returned: the first feed.
fn union_nest(gx: &Gx<'_, '_>, (k, j): (usize, usize), w: &WriteDesc<'_>, m: &MergeDesc<'_>, node: i32, feed: usize) -> usize {
    let ps = gx.pipelines();
    let p = &ps[k];
    let wi = p.ops.iter().position(|o| matches!(o, Op::Write(_))).unwrap_or(p.ops.len());
    if p.ops[..wi].iter().enumerate().any(|(i, o)| i != 0 && i != j && !matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_))) {
        shape(node, BESIDE);
    }
    let Op::Join(JoinDesc::Nest { jointype, .. }) = &p.ops[j] else { crate::refuse!(node, Reason::Bug { payload: "a nest op that is not a nest" }) };
    if m.actions.iter().any(|a| a.kind == MergeMatchKind::MERGE_WHEN_NOT_MATCHED_BY_SOURCE) {
        shape(node, BY_SOURCE);
    }
    if *jointype != JoinType::JOIN_INNER {
        shape(node, JOINTYPE);
    }
    let Some(ik) = ps.inner_of(k, j) else { crate::refuse!(node, Reason::Bug { payload: "a nest without its embedded inner" }) };
    let ip = &ps[ik];
    if !ip.ops.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_))) {
        shape(node, INNER_OPS);
    }
    // the inner's producer: the source relation's scan into a Material, its row the stored REFERENCE one at the recheck
    let heap = |s: &Source<'_>| match s {
        Source::Range { am: Am::Heap, scanrelid, relid, .. } | Source::Index { am: Am::Heap, scanrelid, relid, only: false, .. } => Some((u32::from(*scanrelid), *relid)),
        _ => None,
    };
    let target = |rti: u32| w.targets.iter().any(|t| t.rti == rti);
    let reference = |mk: &RowMark| mk.mark == RowMarkType::ROW_MARK_REFERENCE && !mk.parent && mk.ctid_col > 0 && mk.lock == gravity_program::LockKind::None;
    let Source::Breaker { slot } = ip.source else { shape(node, MATERIAL) };
    let fill = (0..ps.len()).find(|&q| q != k && ps[q].desc.produces() == Some(slot));
    let src = fill.and_then(|q| heap(&ps[q].source).filter(|_| ps[q].desc.consumes.is_empty()).map(|s| (q, s)));
    let spool = |q: usize| matches!(ps[q].ops.split_last(), Some((Op::Join(JoinDesc::Spool), body)) if body.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_))));
    let Some((_, (rti, relid))) = src.filter(|&(q, (rti, relid))| spool(q) && !target(rti) && w.mark_descs.iter().any(|mk| mk.rti == rti && mk.relid == relid && reference(mk))) else { shape(node, MATERIAL) };
    for mk in w.mark_descs.iter().filter(|mk| !target(mk.rti)) {
        if mk.mark == RowMarkType::ROW_MARK_COPY {
            shape(node, COPY);
        }
        if mk.rti != rti || mk.relid != relid || !reference(mk) {
            shape(node, MARKED);
        }
    }
    if rerun_init(gx, true, &p.ops[..wi], ik) {
        shape(node, RECHECK_INITPLAN);
    }
    if loose(gx, k, ik, w, Some(m)) {
        shape(node, INDETERMINATE_COLLATION);
    }
    feed
}

/// D1933B (modifytable): an UPDATE / DELETE over a nested loop's rows, `merge`'s recheck for the other writes. PostgreSQL
/// 18.6 plans `UPDATE t ... FROM (VALUES ...) v WHERE t.key = v.k` (DELETE ... USING VALUES too) as a nested loop, the
/// VALUES scan outer (ROW_MARK_COPY: its row travels whole) and the target's index or seq scan inner. Its EvalPlanQual
/// hands the VALUES row back from its copy (EvalPlanQualFetchRowMark's copy leg, never evaluated again) and the target's
/// scan the locked newest version alone, so the inner's filters and the join qual run again over that version (no row:
/// the write skips it). Here, as `merge`: the write's position is the target's junk ctid and a recheck names the inner's
/// scan site (update_built.rs `locate` over the view index this returns); the COPY row is stored per write (marks.rs
/// `copy_pos`, mergecmd 9's own VALUES entry) and the recheck enters pipeline `k` with that row alone, so the inner opens
/// once over its substituted source. `None` when the rows reach the write through no nest, or through a nest of any other
/// form (one target, no view (D3282: a partitioned root pruned to its one leaf too, w.rti that leaf: update.rs /
/// delete_built.rs write it under the root, a moved row's destinations write.route's, as their own-scan form); filters,
/// gates and projections beside one INNER nest, or producerchain G1 after
/// INNER nests over other relations' own heap scans (marks.rs `nest_joins`: each REFERENCE row stored at its inner's RowOf
/// site, nest_ref.rs's form); the inner one heap range /
/// index / TID scan of the target under filters and projections; a one-row Result outer, the COPY row `k` takes at or
/// before the nest, `k`'s own heap scan of a REFERENCE-marked relation (D1933 (a): `UPDATE t ... FROM s`, s the outer),
/// or the keyed group that de-duplicates such a relation's scan (producerchain B, nest_ref.rs `deduped_ref`; B3: or the
/// Sort a first-of-run before the nest de-duplicates, the scan's INNER probes over other relations' builds too), every
/// other relation's mark that row): the caller's `unbuilt` names those as before. Served forms refuse last D2028 and D2046
/// by name; a grouping beside a hash or merge join serves (D2194 / D2147 / D2201, fixed with D2325).
pub(crate) fn dml(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>, node: i32) -> Option<usize> {
    let ik = dml_form(gx, k, w)?;
    let p = &gx.pipelines()[k];
    let pre = p.ops.iter().take_while(|o| !matches!(o, Op::Write(_))).count();
    // D3838 ROWC-INIT: a data-modifying CTE's write arms the InitPlans at its ModifyTable for its first recheck
    // (rerun/epq/inits.rs `cte_write_armed`), which runs them again as PostgreSQL's does: no departure
    if rerun_init_armed(gx, gx.cte_write(k), &p.ops[..pre], ik, &crate::rerun::epq::inits::cte_write_armed(gx, k, pre)) {
        shape(node, RECHECK_INITPLAN);
    }
    if loose(gx, k, ik, w, None) {
        shape(node, "an UPDATE or DELETE over a nested loop comparing under an indeterminate collation (PostgreSQL raises 42P22: D2046)");
    }
    Some(ik)
}

/// D1933B: the form `dml` serves, without its refusals: the nest's inner's view index. marks.rs `sites` asks it too, so
/// only a write `dml` admits stores its outer's row for the recheck (an INSTEAD OF view's write over a nest never does).
pub(crate) fn dml_form(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>) -> Option<usize> {
    let p = &gx.pipelines()[k];
    let pre = || p.ops.iter().take_while(|o| !matches!(o, Op::Write(_)));
    // G1: the nest whose inner scans the target, after INNER nests over other relations' own heap scans
    // (marks.rs `nest_joins`, each a REFERENCE row the recheck stores at its inner's RowOf site: nest_ref.rs's form)
    let on_target = |i: usize| gx.pipelines().inner_of(k, i).is_some_and(|ik| target_scan(&gx.pipelines()[ik].source, w));
    let nests = || pre().enumerate().filter(|(_, o)| matches!(o, Op::Join(JoinDesc::Nest { .. }))).map(|(i, _)| i);
    let j = nests().find(|&i| on_target(i)).or_else(|| nests().next())?;
    // D3282 (`UPDATE pt t SET a = 3 FROM (VALUES (1), (2)) s(a) WHERE t.a = s.a AND s.a = 1`, pt list-partitioned: the
    // row moves; the plain-partition form of postgres_fdw's batch_cp_upd_test): a root pruned to its one leaf is that
    // leaf's write under the root (update.rs `row_op` / delete_built.rs: WritePlan::with_route, the moved row's
    // destinations `moving`'s), positioned at the leaf's junk ctid as any other target; the recheck substitutes the
    // leaf's scan as it does the unrouted target's
    if !matches!(w.cmd, Cmd::Update | Cmd::Delete) || w.targets.len() != 1 || w.members.has(Member::TargetView) {
        return None;
    }
    // G2: or LEFT ones (marks.rs `nest_left_joins`: a NULL row stored when null-extended, none at the recheck)
    // G4: or INNER hash probes (marks.rs `joins`: the build reruns over its relation's refetched row)
    // G6: or LEFT / ANTI ones (`left_probe`: the build reruns over the refetched row, or empty when the row
    // came null-extended or anti-joined: marks.rs `sites_in` stores a producer's NULL ctid as no row, D2048 / D1933B)
    let probes = |o: &Op<'_>| crate::rerun::epq::marks::joins(o) || left_probe(o);
    // D3705: or INNER nests over a Function Scan or CTE Scan whose row is a COPY mark's (marks.rs `nest_copy_scan`): the
    // recheck re-opens the inner over that stored row (`copied` below)
    let copy_scan = |o: &Op<'_>| matches!(w.cmd, Cmd::Update | Cmd::Delete) && crate::rerun::epq::marks::nest_copy_scan(o);
    // D3795: or nests over a merge or hash join's Material when the write rechecks in the IR through the target's nest
    // (rowop_epq.rs `held_inner`: each mark the join's sides', the nest's row held)
    let held_in_ir = std::cell::Cell::new(None);
    let held = |i: usize| {
        super::rowop_epq::held_at(gx, k, i)
            && held_in_ir.get().unwrap_or_else(|| {
                let v = super::rowop_epq::held_inner(gx, k, w);
                held_in_ir.set(Some(v));
                v
            })
    };
    // D3851 (Rails' `where.missing(:ship)` in an update_all / delete_all over a join: LEFT JOIN ships .. WHERE ships.id IS
    // NULL, PostgreSQL 18.6's Nested Loop Anti Join before or after the target's nest): or ANTI ones (marks.rs
    // `nest_anti_joins`: the rows carry the inner's ctid NULL, so the recheck re-opens the inner over no row)
    let anti = |o: &Op<'_>| crate::rerun::epq::marks::nest_anti_joins(o);
    let before = |i: usize, o: &Op<'_>| i < j && (crate::rerun::epq::marks::nest_joins(o) || crate::rerun::epq::marks::nest_left_joins(o) || anti(o) || probes(o) || copy_scan(o) || held(i)) && !on_target(i);
    // G7: or, after the target's nest, INNER / LEFT nests whose inner reads a Material spool of another
    // relation's own heap scan (`spool_fill`): the recheck reruns the fill over that relation's refetched row, or none
    // when the row came null-extended (marks.rs `sites_in`'s producer null_empty, D2048)
    let after = |i: usize, o: &Op<'_>| i > j && matches!(o, Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER | JoinType::JOIN_LEFT, inner, .. }) if spool_fill(gx, k, inner).is_some());
    // D3706: or, after it, INNER nests over a Function Scan or CTE Scan whose row is a COPY mark's (`copy_scan`, `copied`)
    let copy_after = |i: usize, o: &Op<'_>| i > j && copy_scan(o);
    // D3739 (Rails' update_all across an association: `UPDATE developers __active_record_update_alias .. FROM
    // developers JOIN developers_projects .. JOIN projects .. WHERE .. AND developers.id =
    // __active_record_update_alias.id`; SQLAlchemy's UPDATE .. FROM two relations): or, after it, INNER nests over
    // another relation's own heap scan (marks.rs `nest_joins`), PostgreSQL 18.6's Nested Loop (Nested Loop (.., the
    // target's index scan), Index Scan of a REFERENCE-marked relation): the recheck re-opens each inner over its
    // refetched row (marks.rs `nest_scan`, its RowOf site) after the target's newest version, the inner's quals tested
    // again over it, as G1's before the target's nest
    // D3851 (Rails' update_all through an inner join that also LEFT JOINs: `UPDATE treasures __active_record_update_alias
    // .. FROM treasures JOIN pets_treasures .. LEFT JOIN ships ON ships.id = treasures.ship_id WHERE .. AND treasures.id =
    // __active_record_update_alias.id`, PostgreSQL 18.6's Nested Loop Left Join (Nested Loop (.., the target's index
    // scan), Index Scan of ships)): or a LEFT one (marks.rs `nest_left_joins`), whose recheck re-opens the inner over its
    // refetched row, or none when the row came null-extended (`nest_scan`'s null_empty site), and null-extends again
    // when that row fails the inner's quals, as G2's before the target's nest; or an ANTI one (`anti`), whose rows carry
    // the inner's ctid NULL, so the recheck re-opens the inner over no row and the newest version passes again, as
    // EvalPlanQualFetchRowMark's no-row
    let ref_after = |i: usize, o: &Op<'_>| i > j && (crate::rerun::epq::marks::nest_joins(o) || crate::rerun::epq::marks::nest_left_joins(o) || anti(o)) && !on_target(i);
    // COPYNEST (tour-sortset core-0271: `UPDATE c .. WHERE id IN (SELECT .. GROUP BY .. ORDER BY .. LIMIT 10)`,
    // Nested Loop (Subquery Scan over Limit (Sort ..), the target's index scan)): or the ops below the outer's own COPY row
    // entry (merge/admit.rs `own_entry`) the copied form takes (`subquery_own`: a LIMIT among them): the recheck enters at
    // that entry with the stored row (`stored` below), never below it, as EvalPlanQual hands the Subquery Scan its row
    let entry = crate::ops::modifytable::merge::admit::own_entry(gx, k, w);
    let below = |i: usize, o: &Op<'_>| i < entry && entry <= j && crate::ops::modifytable::merge::admit::subquery_own(o);
    // B3: or, before it, the first-of-run that de-duplicates the Sort the pipeline reads (nest_ref.rs
    // `deduped_run`: the semi join's inner unique-ified by Unique over Sort)
    // D3621: or the one de-duplicating the Sort a COPY row's subquery enters below (`dedup_copy`'s sorted form)
    let copy_run = || w.mark_descs.iter().any(|mk| mk.mark == RowMarkType::ROW_MARK_COPY && dedup_copy(gx, k, mk.rti) == Some(true));
    let run = |i: usize, o: &Op<'_>| i < j && (crate::ops::modifytable::nest_ref::deduped_run(gx, k, o) || matches!(o, Op::FirstOfRun(_)) && copy_run());
    // D3508: or the Union op of a settled UNION ALL outer, first, the target's nest the only join (`union_outer`)
    let union = gx.pipelines().inner_of(k, j).is_some_and(|ik| union_outer(gx, k, w, ik)) && nests().count() == 1;
    // D3826 (Hibernate's BulkManipulationTest: `delete from MANY_TO_MANY where IN_ID in (select ID from SIMPLE_1 where
    // (select count(*) from SIMPLE_2 where SIMPLE_1.ID = SIMPLE_1_ID) = 0 ..)`, PostgreSQL 18.6's Nested Loop whose outer
    // scan filters by the correlated SubPlan, its inner the target's bitmap scan keyed by the outer row): or, before the
    // target's nest, a per-row SubPlan step at the row over its own inner: the recheck runs it again over the outer's
    // refetched row, its scans unsubstituted under the statement's snapshot (exec rerun/epq.rs `rescans`, D1478), as
    // EvalPlanQual runs the SubPlan again in its fresh state
    let sub = |i: usize, o: &Op<'_>| i < j && matches!(o, Op::SubPlan(d) if d.site == gravity_decompose::ops::shared::subplan::SubSite::Row && matches!(d.form, gravity_decompose::ops::shared::subplan::SubForm::Nested { .. }));
    if pre().enumerate().any(|(i, o)| i != j && !matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_)) && !before(i, o) && !after(i, o) && !copy_after(i, o) && !ref_after(i, o) && !below(i, o) && !run(i, o) && !sub(i, o) && !(union && i == 0)) || !matches!(&p.ops[j], Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER, .. })) {
        return None;
    }
    let ik = gx.pipelines().inner_of(k, j)?;
    let ip = &gx.pipelines()[ik];
    // D3686 (pg-boss's id lists over few rows: Nested Loop (Unique -> Sort / HashAggregate over the list, Memoize ->
    // Index Scan of the target)): or a Memoize over the target's scan, as the MERGE form's (mergecmd tw-core-197): the
    // recheck's memo is a fresh, empty one (exec epq.rs `recheck_on`, as EvalPlanQualStart's fresh Memoize), so its
    // lookup misses and the substituted scan runs over the newest version
    let ji = crate::rerun::epq::ref_nest::inner_end(ip);
    // D3601 (7) (River's completer's updated_metadata_only over its id list): or, for a data-modifying CTE's write, the
    // inner's hashed SubPlan steps whose sets are built once (rowop_epq.rs `inner_sets`, the write's IR recheck)
    // D3826 (2) (Hibernate's UPDATE .. FROM joined to the target again by ctid: `update comments set .. from comments
    // c1_0 join users u1_0 on .. where u1_0.id = $2 and comments.ctid = c1_0.ctid`, Nested Loop (Seq Scan users, Hash
    // Join (Seq Scan comments, Hash (Seq Scan comments c1_0)))): or the inner's INNER hash probes, each over a build
    // another relation's own heap scan makes (nest_ref.rs `built`, `built_ref` below): the recheck reruns that build over
    // the relation's refetched row (marks.rs `sites_in`) and opens the inner once over the target's newest version, as
    // EvalPlanQual runs the Hash Join over the two rows
    let inner_probe = |o: &Op<'_>| crate::rerun::epq::marks::joins(o) && super::nest_ref::built(gx, ik, o).is_some();
    let probed_inner = ip.ops[..ji].iter().any(inner_probe);
    let plain = ip.ops[..ji].iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_)) || inner_probe(o));
    if !plain && crate::ops::modifytable::rowop_epq::inner_sets(gx, k, ik, w).is_none() || !crate::rerun::epq::ref_nest::memo_tail(ip) {
        return None;
    }
    match &ip.source {
        Source::Range { am: Am::Heap, scanrelid, relid, .. } | Source::Index { am: Am::Heap, scanrelid, relid, only: false, .. } if u32::from(*scanrelid) == w.rti && *relid == w.plan.target_rel => {}
        // producerchain B: or a TID scan of the target keyed by the outer row (its recheck tests the newest version's
        // ctid against the list: TidRecheck)
        Source::Tid { am: Am::Heap, scanrelid, relid, form, .. } if form.name() != "current" && u32::from(*scanrelid) == w.rti && *relid == w.plan.target_rel => {}
        // G3: or a bitmap heap scan of the target keyed by the outer row (its recheck tests the newest
        // version against the recheck quals: BitmapHeapRecheck)
        Source::Bitmap { am: Am::Heap, scanrelid, relid, .. } if u32::from(*scanrelid) == w.rti && *relid == w.plan.target_rel => {}
        _ => return None,
    }
    let target = |rti: u32| w.targets.iter().any(|t| t.rti == rti);
    let stored = |mk: &RowMark| mk.mark == RowMarkType::ROW_MARK_COPY && (crate::rerun::epq::marks::own_copy_at(gx, k, mk.rti).is_some_and(|at| at <= j) || deduped_copy(gx, k, mk.rti));
    // D1933 (a): or the REFERENCE-marked relation pipeline `k` scans itself as the outer (a heap range or index scan): the
    // recheck refetches its row at the junk ctid (marks.rs `own_pos`: the scan site's marked-row position), the version
    // this statement joined, as EvalPlanQualFetchRowMark does
    // G5: or a bitmap heap scan (marks.rs `own_pos` takes any `scan_of` source)
    let own_ref = |mk: &RowMark| {
        mk.mark == RowMarkType::ROW_MARK_REFERENCE && !mk.parent && matches!(&p.source, Source::Range { am: Am::Heap, scanrelid, relid, .. } | Source::Bitmap { am: Am::Heap, scanrelid, relid, .. } | Source::Index { am: Am::Heap, scanrelid, relid, only: false, .. } if u32::from(*scanrelid) == mk.rti && *relid == mk.relid)
    };
    // G1: or one an earlier INNER nest's inner scans (`before`): its row is stored at that inner's RowOf site
    // D3739: or a later one's (`ref_after`)
    let inner_ref = |mk: &RowMark| {
        mk.mark == RowMarkType::ROW_MARK_REFERENCE && !mk.parent && mk.ctid_col > 0 && pre().enumerate().any(|(i, o)| (before(i, o) || ref_after(i, o)) && matches!(o, Op::Join(JoinDesc::Nest { inner, .. }) if heap_of(&inner.source) == Some((mk.rti, mk.relid))))
    };
    // G4: or one another pipeline's heap scan produces for such a probe's build (marks.rs `sites_in`: the
    // producer reruns over the row refetched at the junk ctid, D1249's rerun set)
    // D3826 (2): or such a probe in the target's inner (`probed_inner`)
    let probe = probed_inner || pre().enumerate().any(|(i, o)| before(i, o) && probes(o) || after(i, o));
    let built_ref = |mk: &RowMark| {
        probe && mk.mark == RowMarkType::ROW_MARK_REFERENCE && !mk.parent && mk.ctid_col > 0 && (0..gx.pipelines().len()).any(|q| q != k && heap_of(&gx.pipelines()[q].source) == Some((mk.rti, mk.relid)) && gx.pipelines()[q].desc.produces().is_some())
    };
    // producerchain B: or one the keyed group that de-duplicates a semi join's inner scans itself (nest_ref.rs)
    // D3508: or the settled union's own COPY mark (`union_outer`)
    let copied = |mk: &RowMark| mk.mark == RowMarkType::ROW_MARK_COPY && crate::rerun::epq::marks::nest_copy(gx, k, mk).is_some_and(|ik| pre().enumerate().any(|(i, o)| (before(i, o) && copy_scan(o) || copy_after(i, o)) && gx.pipelines().inner_of(k, i) == Some(ik)));
    // D3795: or, under `held_inner`, a held nest's join side's (rowop_epq.rs `nest_inner` counts them)
    let held_mark = || (0..j).any(|i| held(i));
    let one = |mk: &RowMark| stored(mk) || own_ref(mk) || inner_ref(mk) || built_ref(mk) || copied(mk) || crate::ops::modifytable::nest_ref::deduped_ref(gx, k, mk) || crate::ops::modifytable::nest_ref::deduped_copy_build(gx, k, mk) || union && union_mark(gx, mk) || held_mark();
    if !w.mark_descs.iter().filter(|mk| !target(mk.rti)).all(|mk| one(mk)) {
        return None;
    }
    if !matches!(p.source, Source::Const) && !w.mark_descs.iter().any(|mk| !target(mk.rti) && one(mk)) {
        return None;
    }
    Some(ik)
}

/// D3508 (`UPDATE t SET .. FROM (SELECT 3 AS k, 30 AS w UNION ALL SELECT 5, 50) s WHERE t.id = s.k`, DELETE ..
/// USING, a MERGE with WHEN MATCHED actions only): the nest's outer the settled union of a flattened UNION ALL subquery,
/// its Union op first (update_built.rs `copied_settled`: constant rows, scanned members), its inner `ik` the target's
/// index scan keyed by the union's column. PostgreSQL 18.6 plans it Nested Loop (Append, Index Scan). The subquery's COPY
/// mark names a relation no scan carries, so EvalPlanQual runs the union's members again, whole, and rescans the
/// target's index scan at each of their rows, handing it the newest version while that version passes the open's keys
/// (IndexRecheck); a key it fails clears it, so no later open yields it; its first joined row is the recheck's. Here the
/// recheck reruns the feeds (marks.rs `Marks::rescan`) and the host's index source keeps the substituted row across the
/// opens so (hosts index_source.rs `substituted`); a MERGE acts with the ORIGINAL source row (merge/settled.rs). A heap
/// range or bitmap inner keeps its refusal: those hosts hand the substituted row at their first open only.
pub(crate) fn union_outer(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>, ik: usize) -> bool {
    matches!(&gx.pipelines()[ik].source, Source::Index { am: Am::Heap, scanrelid, relid, only: false, .. } if u32::from(*scanrelid) == w.rti && *relid == w.plan.target_rel)
        && super::update_built::copied_settled(gx, k, w).is_some()
}

/// D3508: whether `mk` is a settled union's own COPY mark (decompose graph `copy_unions`).
fn union_mark(gx: &Gx<'_, '_>, mk: &RowMark) -> bool {
    mk.mark == RowMarkType::ROW_MARK_COPY && gx.graph.copy_unions.iter().any(|e| e.0 == mk.rti)
}

/// G7: the pipeline filling the Material spool a nest's `inner` reads (a breaker source under filters and
/// projections): `material_fill`'s one heap range or index scan into the Material.
fn spool_fill(gx: &Gx<'_, '_>, k: usize, inner: &PipelineNode<'_>) -> Option<usize> {
    let Source::Breaker { slot } = inner.source else { return None };
    inner.ops.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_))).then(|| material_fill(gx, k, slot)).flatten().map(|(q, _)| q)
}

/// G6: a LEFT or ANTI hash probe before the target's nest (`UPDATE t ... FROM a LEFT JOIN b ..`:
/// PostgreSQL's Nested Loop (Hash Left Join (a, b), t); `DELETE .. USING a LEFT JOIN b .. WHERE b.k IS NULL`: Hash Anti
/// Join). EvalPlanQual refetches a's and b's rows at their junk ctids (b's NULL when the row came null-extended or
/// anti-joined) and reruns the join over them: the build holds b's refetched row or none, so the probe hands on the one
/// row it handed on before, as PostgreSQL's recheck plan does over EvalPlanQualFetchRowMark's rows.
fn left_probe(o: &Op<'_>) -> bool {
    matches!(o, Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_LEFT | JoinType::JOIN_ANTI, .. }))
}

/// G1: whether `s` is the write's target's own heap range / index / TID / bitmap scan (`dml_form`'s inner forms).
fn target_scan(s: &Source<'_>, w: &WriteDesc<'_>) -> bool {
    match s {
        Source::Range { am: Am::Heap, scanrelid, relid, .. } | Source::Index { am: Am::Heap, scanrelid, relid, only: false, .. } => u32::from(*scanrelid) == w.rti && *relid == w.plan.target_rel,
        Source::Tid { am: Am::Heap, scanrelid, relid, form, .. } => form.name() != "current" && u32::from(*scanrelid) == w.rti && *relid == w.plan.target_rel,
        Source::Bitmap { am: Am::Heap, scanrelid, relid, .. } => u32::from(*scanrelid) == w.rti && *relid == w.plan.target_rel,
        _ => false,
    }
}

/// G1: a heap range, index (not index-only) or bitmap scan source's (scanrelid, relid).
fn heap_of(s: &Source<'_>) -> Option<(u32, u32)> {
    match s {
        Source::Range { am: Am::Heap, scanrelid, relid, .. } | Source::Bitmap { am: Am::Heap, scanrelid, relid, .. } | Source::Index { am: Am::Heap, scanrelid, relid, only: false, .. } => Some((u32::from(*scanrelid), *relid)),
        _ => None,
    }
}

/// G1: `dml_form` admits pipeline `k`'s write through more than one nest (its earlier INNER nests' REFERENCE
/// rows positioned at their inners' RowOf sites: write/rowop.rs passes marks.rs `sites_in` its inners flag by it).
pub(crate) fn dml_chain(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>) -> bool {
    let p = &gx.pipelines()[k];
    dml_form(gx, k, w).is_some() && p.ops.iter().take_while(|o| !matches!(o, Op::Write(_))).filter(|o| matches!(o, Op::Join(JoinDesc::Nest { .. }))).count() > 1
}

/// D3605 (Rails' delete_all / update_all over a two-level association: `DELETE FROM comments WHERE comments.id IN
/// (SELECT comments.id FROM comments JOIN posts .. JOIN authors .. WHERE ..)`, a Hash Semi Join whose Hash holds Nested
/// Loops over the posts and comments indexes): an UPDATE / DELETE whose own pipeline `k` holds no nest, one of whose
/// REFERENCE marks is the scan of a nest inner of a producer the recheck reruns (marks.rs `nest_scan`'s producer form:
/// an INNER nest over a heap scan under filters and projections, one row per substituted row before it). EvalPlanQual
/// refetches that row at its junk ctid and reruns the build's join over it; the recheck reopens the inner over the row
/// stored at its RowOf site (write/rowop.rs passes marks.rs `sites_in` its inners flag by this).
pub(crate) fn producer_inners(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>) -> bool {
    let ps = gx.pipelines();
    let p = &ps[k];
    let own = p.ops.iter().take_while(|o| !matches!(o, Op::Write(_))).any(|o| matches!(o, Op::Join(JoinDesc::Nest { .. })));
    let producer = |ik: usize| ik >= ps.len() && ps.inners()[ik - ps.len()].0 .0 != k;
    let inner = |m: &RowMark| m.mark == RowMarkType::ROW_MARK_REFERENCE && !m.parent && m.rti != w.rti && crate::rerun::epq::marks::nest_scan(gx, k, m).is_some_and(producer);
    matches!(w.cmd, Cmd::Update | Cmd::Delete) && w.targets.len() == 1 && !own && w.mark_descs.iter().any(inner)
}

/// IN-LIMIT (`x IN (SELECT .. LIMIT n)`, PostgreSQL's semi join unique-ified as the nest's outer): pipeline `k` reads
/// the keyed group that de-duplicates the subquery, and that group's producer takes `rti`'s COPY row (its copy entry,
/// the SubqueryScan's first op) with only filters, gates and projections past it. A write has no grouping of its own,
/// so a group over its join tree's subquery is that unique-ification: its cells are the passengers decompose carries
/// (facts/demand/readers.rs: the subquery's whole row as `keyed_row`'s `any_value`, D514's `min`), and one it does not carry
/// still refuses by name ('a column the row does not carry'). The recheck's rerun of the producer with the stored row
/// alone (exec `rerun_producer`; the subquery's own pipelines `recheck_skip`) makes the one group the row joined, as
/// EvalPlanQual hands the SubqueryScan its stored row.
fn deduped_copy(gx: &Gx<'_, '_>, k: usize, rti: u32) -> bool {
    dedup_copy(gx, k, rti).is_some()
}

/// `deduped_copy`'s form: Some(false) for the keyed group, D3621 (pg-boss's batch complete, `id IN (SELECT
/// UNNEST($2::uuid[]))` over two or more ids: Unique -> Sort) Some(true) for a Sort, the producer's last op, whose runs
/// one first-of-run before `k`'s write de-duplicates: the recheck's rerun sorts the stored row alone, one run.
fn dedup_copy(gx: &Gx<'_, '_>, k: usize, rti: u32) -> Option<bool> {
    let ps = gx.pipelines();
    let Source::Breaker { slot } = ps[k].source else { return None };
    let q = (0..k).rev().find(|&q| ps[q].desc.produces() == Some(slot))?;
    let at = crate::rerun::epq::marks::own_copy_at(gx, q, rti)?;
    let keyed = gx.graph.groups.iter().any(|g| g.producer == gravity_program::LifeId(q as u16) && g.slot == slot && !g.keys.is_empty() && g.set.is_none() && g.two_pass.is_none());
    let runs = ps[k].ops.iter().take_while(|o| !matches!(o, Op::Write(_))).filter(|o| matches!(o, Op::FirstOfRun(_))).count();
    let sorted = !keyed && runs == 1 && matches!(ps[q].ops.last(), Some(Op::Sort(_)));
    let body = if sorted { &ps[q].ops[..ps[q].ops.len() - 1] } else { &ps[q].ops[..] };
    ((keyed || sorted) && body.iter().skip(at).all(|o| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_)))).then_some(sorted)
}

/// D2028: the recheck node runs an InitPlan again (a producer rerun, or its on-demand cell) where PostgreSQL's
/// EvalPlanQualStart copies its param, forcing it first when the plan under the write reads it and it is pending (a
/// conditional read the first run skipped): a departure of kind More, licensed by the effects home alone (mergecmd
/// landing 7). The InitPlan's own facts are F5's over its plan and every CTE, SubPlan and breaker it reads (decompose
/// rerun.rs `of_subplan`), so a product it reads (a CTE's spool, a Sort inside it) re-runs exactly as stably: code that
/// acts is denied (a VOLATILE call would run again per recheck); code that may raise is licensed with a recheck, which
/// this path does not run, so one read under the write still refuses (PostgreSQL forces it at EvalPlanQualStart, the
/// rerun at its read) unless every row the write can recheck made it first (`forced`: nothing pending to force), and
/// one read by the write alone (an RLS check, a WHEN qual or SET: lazy in both) is served. `pre` is pipeline `k`'s ops
/// before its write (an UPDATE's SET projection among them), `ik` the nest's inner (D1933B: `merge` and `dml` share it).
/// That rule stays for a write that rechecks in the IR (`ir`: a data-modifying CTE's, D2739; merge/nest_arms.rs's
/// union outer). The runner's recheck (exec epq.rs, the forms here outside a data-modifying CTE) reads each InitPlan as
/// EvalPlanQualStart does: an evaluated one's cell, never run again (its members `recheck_skip`, frame/program.rs); a
/// pending one the recheck plan reads, forced once in the statement's run before the recheck (D2112 phase B,
/// `INIT_RECHECK`); one attached below the write, armed afresh for the recheck (D2673, `INIT_EPQ_FRESH`); one only the
/// write's own lists read, run lazily in the statement's cell. There only an InitPlan that is no graph pipeline refuses.
pub(crate) fn rerun_init<'p>(gx: &Gx<'_, 'p>, ir: bool, pre: &[Op<'p>], ik: usize) -> bool {
    rerun_init_armed(gx, ir, pre, ik, &[])
}

/// `rerun_init`, but for the InitPlans `armed` (indexes in the graph's `init_plans`, D3838: a data-modifying CTE's
/// write's, which its first recheck runs again as EvalPlanQualStart's fresh ModifyTable does) none departs.
pub(crate) fn rerun_init_armed<'p>(gx: &Gx<'_, 'p>, ir: bool, pre: &[Op<'p>], ik: usize, armed: &[usize]) -> bool {
    let ip = &gx.pipelines()[ik];
    let path: [&[Op<'p>]; 2] = [pre, &ip.ops[..]];
    let under = |i: &gravity_decompose::InitPlan<'_>| reads(&path, &ip.source, |x| x == i.param || i.params.iter().any(|y| y == x));
    let made = |i: &gravity_decompose::InitPlan<'_>| forced(gx, &path, &ip.source, |x| x == i.param || i.params.iter().any(|y| y == x));
    let rerun = |i: &gravity_decompose::InitPlan<'_>| {
        gx.pipelines().get(i.pipeline as usize).is_none()
            || ir && licence(Effects::of_init_plan(i), Departure::More).map_or(true, |l| l.recheck() && under(i) && !made(i))
    };
    gx.graph.init_plans.iter().enumerate().any(|(x, i)| !matches!(i.link, SubLink::Cte) && !armed.contains(&x) && rerun(i))
}

/// mergecmd insert-only nest: an insert-only MERGE (merge/nm.rs `insert_only`) that inserts, into one unrouted heap
/// target with its junk ctid: no row it writes has a position or a recheck. (DO NOTHING alone writes no row and keeps
/// the served nest form.) mergecmd G2(c): or through a partitioned root (V4R-498's routed INSERT, `nm::routed`), over
/// any count of result relations the planner kept for the join: ExecMergeMatched returns before any lock when no
/// MATCHED action exists, so no leaf's row is ever located or rechecked either.
pub(crate) fn positionless(w: &WriteDesc<'_>, m: &MergeDesc<'_>) -> bool {
    let inserts = m.actions.iter().any(|a| a.kind == MergeMatchKind::MERGE_WHEN_NOT_MATCHED_BY_TARGET && a.cmd.is_some());
    let target = w.targets.len() == 1 && w.route.is_none() || routed_insert(w);
    crate::ops::modifytable::merge::nm::insert_only(m) && inserts && target && !w.members.has(Member::TargetView) && w.targets[0].junk_col > 0
}

/// D3367: pipeline `k`'s rows come through a LEFT nest whose inner is the planner's dummy relation (a MERGE whose join
/// condition folds to false or NULL beside a NOT MATCHED action: the source LEFT JOIN the target's dummy side). Its row
/// never comes, so every row is null-extended, NOT MATCHED: no MATCHED action runs, no row is located or rechecked, and
/// the target's junk ctid (NULL in every row) tells the NOT MATCHED rows as an insert-only MERGE's does.
pub(crate) fn dummy_side(gx: &Gx<'_, '_>, k: usize) -> bool {
    let left = |o: &Op<'_>| matches!(o, Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_LEFT, .. }));
    let ops = &gx.pipelines()[k].ops;
    ops.iter().take_while(|o| !matches!(o, Op::Write(_))).enumerate().any(|(j, o)| left(o) && gx.pipelines().inner_of(k, j).is_some_and(|ik| crate::ops::shared::dummy::dummy(gx, ik, usize::MAX)))
}

/// G2(c): V4R-498's form, an insert-only MERGE routed through a partitioned root (member Route alone: never an
/// inheritance root's, merge/route.rs INHERIT-ROOT, nor a key-updating one), every result relation the root's leaf.
fn routed_insert(w: &WriteDesc<'_>) -> bool {
    let partitioned = w.members.has(Member::Route) && !w.members.has(Member::Targets) && !w.members.has(Member::CrossPartition);
    partitioned && crate::ops::modifytable::merge::nm::routed(w).is_some() && w.targets.iter().all(|t| t.kind == gravity_decompose::ops::shared::modifytable::TargetKind::Heap && t.junk_col == w.targets[0].junk_col)
}

/// D2046: a call under no collation (sort::indeterminate, D1292's test: its input collation InvalidOid over an argument
/// of a collatable type, or one with a collation) anywhere the statement evaluates: every pipeline's (and embedded
/// inner's) filters, gates, projections, LIMITs, set-returning function steps (mergecmd D2046-SRF), join keys and quals,
/// nest params, source keys, rechecks and VALUES rows,
/// the aggregates, and the MERGE's actions, RETURNING and checks. A node the walk does not know counts (but an identity
/// column's NextValueExpr, a sequence's next value: no input collation, no arguments, mergecmd D2046-ID; an aggregate's
/// SortGroupClause, `seq_only`), and so does an
/// op of pipeline `k` or of the nest's inner `ik` it does not read (another pipeline's sort / group / window / set-op
/// keys refuse by D1292 at the decode; its producer's rows are walked where they are made). A Gate's list is read qual by
/// qual, and an Append feed, arm or union with no pruning evaluates nothing (its arms' rows are walked in their own
/// pipelines; mergecmd insert-only nest: an ON false target's dummy Result, a UNION ALL source), nor does a DISTINCT ON
/// first-of-run (its keys D1292's at the decode).
pub(crate) fn loose(gx: &Gx<'_, '_>, k: usize, ik: usize, w: &WriteDesc<'_>, m: Option<&MergeDesc<'_>>) -> bool {
    let bad = |c: u32, a: Node<'_>| c == 0 && type_of(a).map_or_else(|| collation_of(a) != 0, |t| indeterminate(gx.facts, t, 0));
    let any = |c: u32, l: &NodeList<'_>| l.iter().any(|a| bad(c, a));
    let one = |n: Node<'_>| {
        let mut hit = false;
        // a RETURNING list's ReturningExpr (a view's or a virtual generated column's OLD / NEW expression) opened to
        // its expression, as gravity_expr compiles it
        let known = crate::ops::modifytable::ret_cols::walk(n, &mut |x| {
            hit |= match x.node_tag() {
                NodeTag::T_OpExpr => x.as_op_expr().is_some_and(|o| any(o.inputcollid, &o.args)),
                NodeTag::T_DistinctExpr => x.as_distinct_expr().is_some_and(|o| any(o.inputcollid, &o.args)),
                NodeTag::T_NullIfExpr => x.as_null_if_expr().is_some_and(|o| any(o.inputcollid, &o.args)),
                NodeTag::T_FuncExpr => x.as_func_expr().is_some_and(|f| any(f.inputcollid, &f.args)),
                NodeTag::T_ScalarArrayOpExpr => x.as_scalar_array_op_expr().is_some_and(|s| any(s.inputcollid, &s.args)),
                NodeTag::T_MinMaxExpr => x.as_min_max_expr().is_some_and(|s| any(s.inputcollid, &s.args)),
                NodeTag::T_Aggref => x.as_aggref().is_some_and(|a| any(a.inputcollid, &a.args)),
                NodeTag::T_WindowFunc => x.as_window_func().is_some_and(|f| any(f.inputcollid, &f.args)),
                NodeTag::T_RowCompareExpr => x.as_row_compare_expr().is_some_and(|r| r.inputcollids.iter().zip(r.largs.iter().zip(r.rargs.iter())).any(|(c, (a, b))| bad(c, a) || bad(c, b))),
                _ => false,
            };
            !hit
        });
        hit || !(known || seq_only(n))
    };
    let list = |l: &NodeList<'_>| l.iter().any(one);
    let arm = |o: &Op<'_>, strict: bool| match o {
        Op::Filter(n) => one(*n),
        Op::Gate(n) => n.as_list().map_or_else(|| one(*n), list), // a Result's resconstantqual: an implicit-AND list
        Op::Project(l) => list(l),
        _ if crate::graph::append_without_exprs(o) => false,
        // G2(c) part 2: a union's (or an arm's) partition pruning is PostgreSQL's own verdict, once per execution
        // (exec ops/append/prune.rs over ExecDoInitialPruning / the exec steps), never a served kernel's comparison; a
        // MergeAppend's keys (RunsMergeappend) stay strict
        _ if crate::graph::step_kind(o) == crate::face::kinds::KindId::SrcAppend => false,
        Op::Join(JoinDesc::Nest { qual, params, .. }) => list(qual) || crate::frame::iface::nest_param_vals(params, &one),
        Op::Join(JoinDesc::Probe { keys, qual, .. }) => list(keys) || list(qual),
        Op::Join(JoinDesc::Merge { clauses, qual, .. }) => list(clauses) || list(qual),
        Op::Join(JoinDesc::Build { keys, .. } | JoinDesc::Memo { keys, .. }) => list(keys),
        // D3800: a drain hands its spool's rows and its table's unmatched records on, comparing nothing (its probe's keys
        // and quals are the probe pipeline's, walked there)
        Op::Join(JoinDesc::Drain { .. }) => false,
        // D2046-DON: DISTINCT ON's first-of-run (a kept subquery's, below its copy entry) compares its keys only,
        // and those refuse by D1292 at the decode (decompose agg `Shape::key`, before the op is pushed)
        Op::FirstOfRun(_) => false,
        // D2046-SRF: a set-returning function step (a FROM function's or a ProjectSet's) evaluates its calls and
        // its target list, read as any projection's
        Op::Srf(d) => d.functions.iter().any(|f| one(f.node)) || list(d.tlist),
        // a WindowAgg's step (below a COPY entry, merge/admit.rs `subquery_own`) compares its partition and peer keys under
        // their own collations only (an indeterminate one refuses by D1292 at the decode, decompose ops/window.rs) and
        // evaluates its functions' arguments and FILTER and its run condition; its qual and projection follow as a Filter
        // and a Project, read above
        Op::Window(d) => d.functions.iter().any(|f| one(f.node)) || list(d.run_condition),
        // D1933 ACTION-SUB: a SubPlan step only a MERGE action reads (merge/admit.rs `action_sub`) evaluates its
        // test and arguments (its plan's pipelines and nested inner are walked as any other)
        Op::SubPlan(d) if crate::ops::modifytable::merge::admit::action_sub(o, w) => d.test.is_some_and(one) || list(d.args),
        // D3601 (7): a hashed SubPlan step (`inner_sets`) evaluates its arguments and compares through its test's
        // operators under the test's collations (ExecHashSubPlan's table: subplan tab_collations)
        Op::SubPlan(d) if matches!(d.form, gravity_decompose::ops::shared::subplan::SubForm::Hashed { .. }) => d.test.is_some_and(one) || list(d.args),
        // D3826: a per-row SubPlan step at the row (`dml_form`'s, before the target's nest) evaluates its test and
        // arguments; its plan's pipelines and nested inner are walked as any other
        Op::SubPlan(d) if d.site == gravity_decompose::ops::shared::subplan::SubSite::Row && matches!(d.form, gravity_decompose::ops::shared::subplan::SubForm::Nested { .. }) => d.test.is_some_and(one) || list(d.args),
        _ => strict,
    };
    // a LIMIT evaluates its count and offset expressions
    let op = |o: &Op<'_>, strict: bool| match crate::frame::iface::limit_bounds(o) {
        Some((count, offset)) => [count, offset].into_iter().any(|b| matches!(b, Bound::Expr(n) if one(*n))),
        None => arm(o, strict),
    };
    let src = |s: &Source<'_>| match s {
        Source::Index { keys, recheck, .. } => keys.iter().any(|x| one(x.value)) || list(recheck),
        Source::Bitmap { recheck, .. } | Source::Tid { quals: recheck, .. } => list(recheck),
        Source::Values { lists, .. } => lists.iter().filter_map(|l| l.as_list()).flat_map(|l| l.iter()).any(one),
        _ => false,
    };
    let pipes = gx.pipelines();
    let inners = pipes.inners();
    let mut all = pipes.iter().enumerate().chain(inners.iter().enumerate().map(|(i, x)| (pipes.len() + i, x.1)));
    let pre = pipes[k].ops.iter().take_while(|o| !matches!(o, Op::Write(_))).count();
    all.any(|(q, p)| src(&p.source) || p.ops[..if q == k { pre } else { p.ops.len() }].iter().any(|o| op(o, q == k || q == ik)))
        || gx.graph.groups.iter().chain(gx.graph.streams.iter()).flat_map(|g| g.aggs.iter()).any(|d| one(d.node))
        || m.is_some_and(|m| m.actions.iter().any(|a| a.quals.is_some_and(list) || list(a.tlist)))
        || w.returning.is_some_and(list)
        || w.checks.iter().any(|c| c.qual.as_list().map_or_else(|| one(c.qual), list)) // a check's qual: an implicit-AND list
}

/// D2046-ID: whether every node of `n` the walk does not know is a NextValueExpr (an identity column's next value
/// in a MERGE INSERT action, a leaf with no input collation; merge/lower.rs's action check asks the same) or a
/// SortGroupClause (an ordered or DISTINCT aggregate's clause: a leaf naming one of the aggregate's arguments, compared
/// under the Aggref's own inputcollid, which `loose` reads at the Aggref; mergecmd landing 7's witness).
pub(crate) fn seq_only(n: Node<'_>) -> bool {
    crate::stack_check();
    if matches!(n.node_tag(), NodeTag::T_NextValueExpr | NodeTag::T_SortGroupClause) {
        return true;
    }
    let mut ok = true;
    let known = gravity_decompose::expr::kids(n, &mut |c| ok &= seq_only(c));
    known && ok
}

/// A PARAM_EXEC `hit` names in `ops` (Filter / Gate / Project, a nest's qual) or `src`'s index keys (a node the walk does
/// not know counts as a read).
fn reads(ops: &[&[Op<'_>]], src: &Source<'_>, hit: impl Fn(i32) -> bool) -> bool {
    let one = |n: Node<'_>| {
        let mut found = false;
        let known = walk(n, &mut |x| {
            found |= x.as_param().is_some_and(|q| q.paramkind == ParamKind::PARAM_EXEC && hit(q.paramid));
            !found
        });
        found || !known
    };
    let list = |l: &NodeList<'_>| l.iter().any(one);
    let keys = match src {
        Source::Index { keys, recheck, .. } => keys.iter().any(|x| one(x.value)) || list(recheck),
        _ => false,
    };
    keys || ops.iter().flat_map(|o| o.iter()).any(|o| match o {
        Op::Filter(n) | Op::Gate(n) => one(*n),
        Op::Project(l) => list(l),
        Op::Join(JoinDesc::Nest { qual, params, .. }) => list(qual) || crate::frame::iface::nest_param_vals(params, &one),
        _ => true,
    })
}

/// D2028: a PARAM_EXEC `hit` names that every row the write can recheck (a matched one) made before the write saw it: an
/// index key of `src`, or a direct argument (through relabels) of a strict call that is a top-level conjunct of a filter
/// or gate in `ops` or of the nest's qual (true only with every argument evaluated and not null). PostgreSQL's
/// EvalPlanQualStart then finds the param set and copies it, and the recheck reads the same Init cell.
fn forced<'p>(gx: &Gx<'_, '_>, ops: &[&[Op<'p>]], src: &Source<'p>, hit: impl Fn(i32) -> bool) -> bool {
    let param = |n: Node<'p>| {
        let mut n = n;
        while let Some(a) = through_relabel(n) {
            n = a;
        }
        n.as_param().is_some_and(|q| q.paramkind == ParamKind::PARAM_EXEC && hit(q.paramid))
    };
    fn conjunct<'p>(gx: &Gx<'_, '_>, n: Node<'p>, param: &dyn Fn(Node<'p>) -> bool) -> bool {
        crate::stack_check();
        if let Some(b) = n.as_bool_expr() {
            return b.boolop == BoolExprType::AND_EXPR && b.args.iter().any(|a| conjunct(gx, a, param));
        }
        let call = n.as_op_expr().map(|o| (o.opfuncid, &o.args)).or_else(|| n.as_func_expr().map(|f| (f.funcid, &f.args)));
        call.is_some_and(|(f, args)| gx.facts.func(f).is_some_and(|p| p.strict) && args.iter().any(|a| param(a)))
    }
    let keys = matches!(src, Source::Index { keys, .. } if keys.iter().any(|k| param(k.value)));
    keys || ops.iter().flat_map(|o| o.iter()).any(|o| match o {
        Op::Filter(n) | Op::Gate(n) => conjunct(gx, *n, &param),
        Op::Join(JoinDesc::Nest { qual, .. }) => qual.iter().any(|q| conjunct(gx, q, &param)),
        _ => false,
    })
}
