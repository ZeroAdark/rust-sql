//! D2015 NEST-ARMS (held-out parts-0083): a MERGE through a partitioned root
//! over a nested loop whose inner is the Append of its result relations' scans. PostgreSQL 18.6 plans `MERGE INTO root t
//! USING (VALUES ..) s ON t.key = s.a ..` over a few source rows as a Nested Loop (Left beside a NOT MATCHED action, else
//! Inner): the source the outer, the Append of one parameterized scan per leaf the inner, every leaf a result relation
//! (Merge on the root). Decompose embeds that Append as a union the nest's inner reads first, each arm a feed (M8-NEST S2b
//! / S2c: rerun/inner_arms.rs makes every arm's source again inside the inner at each re-open, numbering each arm's site
//! as its feed's pipeline site). A row's result relation is its junk tableoid and its position its junk ctid
//! (ExecLookupResultRelByOid). EvalPlanQual substitutes that relation's scan with the newest version and blocks every
//! other result relation's (EvalPlanQualStart's relsubs_blocked), so the re-run inner yields that version alone, through
//! its arm's keys (IndexRecheck) and the join qual, over the source row the recheck hands the outer.
//!
//! Here the union form serves it (merge/route.rs `union_targets` takes the arms as its feeds; the routed NOT MATCHED INSERT
//! beside, `insert_beside`, and RETURNING over the union form as G2(c)'s): merge/lower.rs `rel` stores every feed site's
//! position empty and names the row's own feed site; the recheck reruns pipeline `k`, whose outer yields its one row (a
//! Result's, or the COPY-marked VALUES row stored whole: D732, nest_write.rs's COPYROW), so the inner opens once and each
//! arm's source hands its substituted row, or none, at that open. `admit` is nest_write.rs `merge`'s check of the form:
//! the nest INNER, or LEFT under a NOT MATCHED [BY TARGET] action; filters, gates and projections beside it; the inner the
//! union under filters and projections; the outer a one-row Result or a stored mark's row; every other relation's mark
//! stored; `RECHECK_INITPLAN`, `INDETERMINATE_COLLATION` and the no-widen list last, as the one-target form. A union outer
//! (held-out parts-0083: a flattened UNION ALL of constant rows) rechecks in the IR instead (`first`), else refuses by name
//! (`UNION_OUTER`). rerun/inner_arms_index.rs's write rule asks `covers` (an index arm of this form is rechecked through its
//! keys). Helper file of write.merge: no rows of its own.
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::modifytable::{MergeDesc, RowMark, WriteDesc};
use gravity_decompose::{Am, Op, PipelineNode, Source};
use types_nodes::jointype::JoinType;
use types_nodes::plannodes::RowMarkType;
use types_nodes::primnodes::MergeMatchKind;

use crate::admit::refusal::Reason;
use crate::graph::Gx;
use crate::ops::modifytable::merge::admit::shape;
use crate::ops::modifytable::nest_write::{BESIDE, BY_SOURCE, COPY, INDETERMINATE_COLLATION, INNER_OPS, JOINTYPE, MARKED, OUTER, RECHECK_INITPLAN};

/// The form's outer a union read first that `first` does not take: PostgreSQL's EvalPlanQual hands no scan that union's
/// COPY mark and evaluates its arms again, then takes the first joined row.
pub(crate) const UNION_OUTER: &str = "a MERGE through a root over a nested loop of its result relations' arms whose outer is a union (EvalPlanQual runs its arms again: D2015)";

/// The feeds, by target index (`targets` order), of the union a nest's inner in pipeline `k` embeds when they are MERGE
/// `w`'s result relations' arms (module doc), else `None`: several heap result relations named by a junk tableoid, one
/// nest (INNER or LEFT) before the write, its inner the union's emit (the Union op first, then filters and projections), each feed one result relation's heap range or index scan under filters and projections into the
/// union's slot.
pub(crate) fn arms<'g>(gx: &Gx<'g, '_>, k: usize, w: &WriteDesc<'_>) -> Option<mcx::PgVec<'g, usize>> {
    use crate::rerun::union_arms::{feed_of, union_of};
    let ps = gx.pipelines();
    let p = ps.get(k)?;
    if w.merge.is_none() || w.targets.len() < 2 || w.tableoid_col <= 0 {
        return None;
    }
    let wi = p.ops.iter().position(|o| matches!(o, Op::Write(_)))?;
    let mut nests = p.ops[..wi].iter().enumerate().filter(|(_, o)| matches!(o, Op::Join(JoinDesc::Nest { .. })));
    let (Some((j, Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER | JoinType::JOIN_LEFT, .. }))), None) = (nests.next(), nests.next()) else { return None };
    let ik = ps.inner_of(k, j)?;
    let ip = &ps[ik];
    let Source::Breaker { slot } = ip.source else { return None };
    let simple = |ops: &[Op<'_>]| ops.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_)));
    // (a run-time pruning by the nest's parameters prunes again at the recheck's one open: rerun/inner_arms.rs `nest_prune`)
    let first = ip.ops.first().is_some_and(|o| crate::graph::step_kind(o) == crate::face::kinds::KindId::SrcAppend && !crate::graph::is_arm(o));
    if union_of(ip).map(|u| u.0) != Some(slot) || !first || !simple(&ip.ops[1..]) {
        return None;
    }
    let (emit, _, feeds) = crate::pipes::inner_union(gx, ik)?;
    if emit != ik || feeds.len() != w.targets.len() {
        return None;
    }
    let mut by = mcx::pg_vec![in gx.mcx; usize::MAX; w.targets.len()];
    for &f in feeds.iter() {
        let q = &ps[f];
        let (rti, relid) = match &q.source {
            Source::Range { am: Am::Heap, scanrelid, relid, .. } | Source::Index { am: Am::Heap, scanrelid, relid, only: false, .. } => (u32::from(*scanrelid), *relid),
            _ => return None,
        };
        let (last, body) = q.ops.split_last()?;
        if feed_of(q) != Some(slot) || matches!(last, Op::Filter(_) | Op::Project(_)) || !q.desc.consumes.is_empty() || !simple(body) {
            return None;
        }
        let i = w.targets.iter().position(|t| t.rti == rti && t.relid == relid)?;
        if by[i] != usize::MAX {
            return None;
        }
        by[i] = f;
    }
    Some(by)
}

/// nest_write.rs `merge`'s check of the form (module doc) for pipeline `k`'s MERGE `w` (`m`) over its nest op `j`; the
/// target's "producer" returned: `feed`, the first result relation's arm.
pub(crate) fn admit(gx: &Gx<'_, '_>, (k, j): (usize, usize), w: &WriteDesc<'_>, m: &MergeDesc<'_>, node: i32, feed: usize) -> usize {
    use crate::ops::modifytable::nest_write::{loose, rerun_init};
    use crate::rerun::epq::marks::{nest_ref, own_copy_at};
    let ps = gx.pipelines();
    let p = &ps[k];
    let wi = p.ops.iter().position(|o| matches!(o, Op::Write(_))).unwrap_or(p.ops.len());
    // a union outer rechecks in the IR (`first`), its marks never stored
    let union_outer = j > 0 && p.ops.first().is_some_and(|o| crate::graph::step_kind(o) == crate::face::kinds::KindId::SrcAppend && !crate::graph::is_arm(o));
    if union_outer && first(gx, k, w).is_none() {
        shape(node, UNION_OUTER);
    }
    if p.ops[..wi].iter().enumerate().any(|(i, o)| i != j && !(union_outer && i == 0) && !matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_))) {
        shape(node, BESIDE);
    }
    let Op::Join(JoinDesc::Nest { jointype, .. }) = &p.ops[j] else { crate::refuse!(node, Reason::Bug { payload: "a nest op that is not a nest" }) };
    let kind = |x: MergeMatchKind| m.actions.iter().any(|a| a.kind == x);
    if kind(MergeMatchKind::MERGE_WHEN_NOT_MATCHED_BY_SOURCE) {
        shape(node, BY_SOURCE);
    }
    if !(*jointype == JoinType::JOIN_INNER || *jointype == JoinType::JOIN_LEFT && kind(MergeMatchKind::MERGE_WHEN_NOT_MATCHED_BY_TARGET)) {
        shape(node, JOINTYPE);
    }
    let Some(ik) = ps.inner_of(k, j) else { crate::refuse!(node, Reason::Bug { payload: "a nest without its embedded inner" }) };
    if !ps[ik].ops.iter().skip(1).all(|o| matches!(o, Op::Filter(_) | Op::Project(_))) {
        shape(node, INNER_OPS);
    }
    // the outer's row at the recheck: a COPY-marked row the nest's outer takes (its own VALUES scan, or a subquery's copy
    // entry before the nest) stored whole, or a REFERENCE-marked relation it scans itself refetched (nest_write.rs `merge`'s
    // COPYROW / 13a); every other relation's mark refuses, as there
    let target = |rti: u32| w.targets.iter().any(|t| t.rti == rti);
    let copied = |mk: &RowMark| mk.mark == RowMarkType::ROW_MARK_COPY && own_copy_at(gx, k, mk.rti).is_some_and(|at| at <= j);
    let stored = |mk: &RowMark| copied(mk) || nest_ref(gx, k, mk);
    if !union_outer && w.mark_descs.iter().any(|mk| !target(mk.rti) && mk.mark == RowMarkType::ROW_MARK_COPY && !stored(mk)) {
        shape(node, COPY);
    }
    if !union_outer && w.mark_descs.iter().any(|mk| !target(mk.rti) && !stored(mk)) {
        shape(node, MARKED);
    }
    // the recheck's outer yields at most one row, so the inner (and each arm) opens once over its substituted source
    if !union_outer && !matches!(p.source, Source::Const) && !w.mark_descs.iter().any(|mk| !target(mk.rti) && stored(mk)) {
        shape(node, OUTER);
    }
    if rerun_init(gx, true, &p.ops[..wi], ik) {
        shape(node, RECHECK_INITPLAN);
    }
    if loose(gx, k, ik, w, Some(m)) {
        shape(node, INDETERMINATE_COLLATION);
    }
    feed
}

/// D2015 UNION-OUTER (held-out parts-0083: `MERGE INTO sales s USING (SELECT 900010 AS id, date '2023-03-03' AS d UNION ALL
/// SELECT 1, date '2022-12-08') v ON s.id = v.id AND s.sold_on = v.d WHEN MATCHED THEN UPDATE .. WHEN NOT MATCHED THEN
/// INSERT ..`): the form whose outer is a union read first rechecks in the IR (write/rowop_epq.rs `in_ir`, merge/epq.rs
/// `Form::First`, rerun/epq/epq_ir.rs `FirstOuter`): PostgreSQL's ExecMergeMatched decides MATCHED by EvalPlanQual's first
/// row, the union's first arm joined to the newest version, and runs the actions over the stopped row's source columns
/// (planSlot). Its nest op, the nest's inner and the outer union's first feed, when: the arms form (`arms`); the nest LEFT
/// under a NOT MATCHED [BY TARGET] action and no BY SOURCE one; pipeline `k` the union's unpruned marker, then only
/// projections and gates around the nest up to the write (a filter could drop the first arm's row, and the first rerun row
/// would be another arm's); the union's first feed one row (a Result: a constant source under projections); no mark but
/// the union's COPY mark; every result relation's trigger facts known, none with a BEFORE ROW trigger (merge/epq.rs
/// `admits`'s rule); no cross-partition UPDATE, view or action SubPlan; no SubPlan and no InitPlan read in what the redo
/// evaluates (EvalPlanQualBegin forces a pending InitPlan: rowop_epq.rs `rowc_tail`'s rule). Else none.
pub(crate) fn first(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>) -> Option<(usize, usize, usize)> {
    use gravity_decompose::ops::shared::modifytable::Member;
    let m = w.merge.as_ref()?;
    let feeds = arms(gx, k, w)?;
    let ps = gx.pipelines();
    let p = &ps[k];
    let wi = p.ops.iter().position(|o| matches!(o, Op::Write(_)))?;
    if !p.ops.first().is_some_and(crate::rerun::union_arms::union_op) {
        return None;
    }
    let jn = p.ops[..wi].iter().position(|o| matches!(o, Op::Join(JoinDesc::Nest { .. })))?;
    let Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_LEFT, params, qual, .. }) = &p.ops[jn] else { return None };
    let kind = |x: MergeMatchKind| m.actions.iter().any(|a| a.kind == x);
    if !kind(MergeMatchKind::MERGE_WHEN_NOT_MATCHED_BY_TARGET) || kind(MergeMatchKind::MERGE_WHEN_NOT_MATCHED_BY_SOURCE) {
        return None;
    }
    if !p.ops[1..wi].iter().enumerate().all(|(i, o)| i + 1 == jn || matches!(o, Op::Project(_) | Op::Gate(_))) {
        return None;
    }
    let outer = gx.union_feeds(k);
    let &f0 = outer.first()?;
    let q0 = &ps[f0];
    let (_, body) = q0.ops.split_last()?;
    if !matches!(q0.source, Source::Const) || body.is_empty() || !body.iter().all(|o| matches!(o, Op::Project(_))) || !q0.desc.consumes.is_empty() {
        return None;
    }
    let ik = ps.inner_of(k, jn)?;
    let copy = |rti: u32| w.copy_arms.iter().any(|e| e.0 == rti);
    if w.mark_descs.iter().any(|mk| mk.parent || mk.mark != RowMarkType::ROW_MARK_COPY || !copy(mk.rti)) {
        return None;
    }
    if [Member::CrossPartition, Member::TargetView, Member::WriteSubPlan].into_iter().any(|x| w.members.has(x)) {
        return None;
    }
    if w.targets.iter().any(|t| {
        let f = gx.facts.trig_ext(t.relid);
        !f.known || f.before_row != 0
    }) {
        return None;
    }
    // what the redo evaluates: the first feed's projections, the write pipeline's ops around the nest, the nest's
    // parameters (each one's value, nest_write.rs `vals`) and quals, each arm's recheck quals and ops, the inner
    // union's emit ops
    let init = |id: i32| crate::rerun::params::producer(gx, id) == Some(crate::face::kinds::KindId::NestInitplan);
    let loud = |e: types_nodes::Node<'_>| gravity_decompose::expr::has_subplan(e) || crate::ops::modifytable::rowop_epq::reads(e, &init);
    if crate::frame::iface::nest_param_vals(params, &loud) {
        return None;
    }
    for o in body.iter().chain(p.ops[1..wi].iter()).chain(ps[ik].ops.iter().skip(1)) {
        match o {
            Op::Project(l) => {
                if l.iter().any(|e| loud(e)) {
                    return None;
                }
            }
            Op::Filter(x) | Op::Gate(x) => {
                if loud(*x) {
                    return None;
                }
            }
            _ => {}
        }
    }
    if qual.iter().any(|e| loud(e)) {
        return None;
    }
    for &f in feeds.iter() {
        let row = crate::ops::modifytable::rowop_epq::below(&ps[f], ps[f].ops.len().saturating_sub(1), gx.mcx)?;
        if row.into_iter().any(|e| loud(e)) {
            return None;
        }
    }
    Some((jn, ik, f0))
}

/// rerun/inner_arms_index.rs's write rule: whether every write or row lock of the statement is a MERGE of this form
/// (`arms`) whose feeds include arm pipeline `arm` (its index arm rechecked through its keys at the substituted open), or
/// an UPDATE / DELETE of several result relations over the same nest form (write/update_built.rs `nest_union`, regress
/// subselect#137: EvalPlanQual substitutes the row's own leaf's arm and blocks the others the same way; a bitmap arm
/// rechecks its bitmapqualorig), its outer no union (`copied_union`'s outer yields several rows at the recheck, so the
/// inner would open again past its substituted row).
pub(crate) fn covers(gx: &Gx<'_, '_>, arm: &PipelineNode<'_>) -> bool {
    let ps = gx.pipelines();
    let at = arm as *const PipelineNode<'_> as *const u8;
    let Some(a) = (0..ps.len()).find(|&q| std::ptr::eq(&ps[q] as *const PipelineNode<'_> as *const u8, at)) else { return false };
    let mut writers = (0..ps.len()).filter(|&q| crate::rerun::inner_arms_index::writes(&ps[q]));
    let (Some(k), None) = (writers.next(), writers.next()) else { return false };
    let mut ws = ps[k].ops.iter().filter_map(|o| if let Op::Write(w) = o { Some(w) } else { None });
    let (Some(w), None) = (ws.next(), ws.next()) else { return false };
    use crate::ops::modifytable::update_built::{copied_union, nest_union};
    let feeds = if w.merge.is_some() { arms(gx, k, w) } else { nest_union(gx, k, w).filter(|_| !copied_union(gx, k, w)) };
    ps[k].ops.iter().all(|o| !matches!(o, Op::Lock(_))) && feeds.is_some_and(|f| f.contains(&a))
}
