//! (the refusal 'a write target without a single producer chain', the biggest M3 group of the modifytable
//! family): an UPDATE / DELETE whose target's own heap scan is the outer of INNER nested loops, each inner one heap scan
//! (range, index, bitmap) of a REFERENCE-marked relation under filters and projections, parameterized or not, with no
//! Material between (rerun/epq/marks.rs `nest_joins`). PostgreSQL 18.6 plans `UPDATE t ... FROM s WHERE <t's own quals>
//! AND <s's own quals>` so when s's side is small or its join is a filter no hash or merge join takes (a rule's
//! `WHERE f1 = OLD.f1` over the target's own relation, a self join, `UPDATE t ... FROM s WHERE s.id = t.id` over s's
//! primary key: an index scan parameterized by the outer row). ModifyTable takes its subplan's rows in order, each with
//! the target's junk ctid (the row to write) and s's junk ctid (`ctid{rowmarkId}`, preprocess_targetlist); a target row
//! joined twice is written once (the second sees TM_SelfModified and is skipped). Its EvalPlanQual reruns the plan with
//! the target's scan returning the locked newest version alone and s's scan returning the row its junk ctid names under
//! SnapshotAny (EvalPlanQualFetchRowMark: the version the join read), s's keys and filters checked again over it
//! (IndexRecheck / BitmapHeapRecheck, its quals, the nest's parameters now the newest version's), then the join qual
//! and the projection.
//!
//! Here: the rows are the target's own scan rows joined by those nests, so the write is the own-rows write and its marks
//! are lockrows NEST's (write/rowop.rs `claim` asks marks.rs `sites_in` with inners): each inner's REFERENCE row is stored
//! in a RowOf site of the inner's scan site before the host call, and the runner's recheck (exec epq.rs, driver
//! `Entry::recheck`) substitutes the target's scan with the newest version and each inner with its stored row, which it
//! opens once (hosts `next_substituted`: keys and recheck quals) as PostgreSQL rescans the inner over its test tuple.
//! Admitted (`chain`) only where every relation the write marks besides its targets is such an inner's (writesubplan A2: or
//! the COPY row of a subquery such an inner runs, marks.rs `nest_copy`; another COPY row, a relation scanned elsewhere keep
//! `unbuilt`'s name; a parent's mark carries no row), one result relation (an inheritance
//! tree's one leaf too), and every op before the write a filter, gate, projection, SubPlan step, a join marks.rs
//! `write_joins` names or such a nest.
//! ANTINEST (tour-subq subq-0106, claims shop-0597 / writes-0042): or an ANTI nest over such an inner
//! (`DELETE FROM c WHERE NOT EXISTS (SELECT 1 FROM o WHERE o.cid = c.id)`: Nested Loop Anti Join, o's index scan
//! parameterized by c's row). Its rows carry o's junk ctid NULL, so EvalPlanQualFetchRowMark gives o no test tuple and
//! PostgreSQL's recheck passes the anti join over the target's newest version (its own quals checked again); here the
//! inner's RowOf site holds an empty position (marks.rs `inner_pos`, null_empty) and the re-open yields no row.
//! SEMINEST (regress matview#206, privileges#812, write_parallel#16: REFRESH MATERIALIZED VIEW CONCURRENTLY's
//! `DELETE FROM mv WHERE ctid = ANY (SELECT diff.tid FROM diff WHERE ..)`, which PostgreSQL 18.6 plans as Nested Loop Semi
//! Join of the matview's scan and diff's once the matview's stats are set, as under pg_regress's autovacuum): or a SEMI
//! nest over such an inner. Its row carries the first matching inner row's junk ctid (the semi join's output), so the
//! inner's RowOf site holds that row and the recheck's re-open yields it alone, the join qual checked again over the
//! target's newest version, as EvalPlanQual's semi join over the refetched row.
//! FAMNEST (rowsecurity#338 #339): or an INNER nest whose inner is the settled union of one REFERENCE-marked
//! parent's children (`family_inner`): PostgreSQL 18.6 plans `UPDATE t ... FROM parent WHERE <t's quals> AND <parent's
//! quals>` as Nested Loop (t's scan, Append of the children's scans), and its EvalPlanQual fetches a child's row only where
//! the row's tableoid names that child (EvalPlanQualFetchRowMark), so the inner's re-open yields that refetched row alone,
//! the child's quals checked again over it. marks.rs `sites_in` keeps each child's row in a RowOf site of its arm's scan
//! site, gated on the family's tableoid column (lockrows REFPARENT's gate).
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::modifytable::{RowMark, WriteDesc};
use gravity_decompose::{Am, Op, Source};
use gravity_program::{Cmd, LockKind};
use mcx::{CollectIn, GrowIn, PgVec};
use types_nodes::jointype::JoinType;
use types_nodes::plannodes::RowMarkType;

use crate::graph::Gx;

/// A heap range, index (not index-only) or bitmap scan source's (scanrelid, relid), as marks.rs `scan_of` takes them.
fn heap(s: &Source<'_>) -> Option<(u32, u32)> {
    match s {
        Source::Range { am: Am::Heap, scanrelid, relid, .. } | Source::Bitmap { am: Am::Heap, scanrelid, relid, .. } | Source::Index { am: Am::Heap, scanrelid, relid, only: false, .. } => {
            Some((u32::from(*scanrelid), *relid))
        }
        _ => None,
    }
}

/// Whether pipeline `k`'s UPDATE / DELETE `w` (its source the target's own heap scan) reaches its write through at least
/// one such nest and only the ops the module doc names, every non-target mark a REFERENCE row one of the nests' inners
/// scans (or a probe's build scans). write/update.rs and write/delete.rs admit the rows by it, write/rowop.rs `claim`
/// positions the marks by it. mergecmd MOVE (c): a MERGE's too (write/nest_write.rs `merge` admits its INNER nest by it:
/// PostgreSQL 18.6's EvalPlanQual reruns the join over the target's newest version and the inners' refetched rows, then
/// the whole action list, ExecMergeMatched).
pub(crate) fn chain(gx: &Gx<'_, '_>, k: usize, w: &WriteDesc<'_>) -> bool {
    let p = &gx.pipelines()[k];
    // one result relation (an inheritance tree's one leaf too: V4R-214's route, w.rti its leaf), scanned by the pipeline
    if !matches!(w.cmd, Cmd::Update | Cmd::Delete | Cmd::Merge) || w.targets.len() != 1 || heap(&p.source) != Some((w.rti, w.plan.target_rel)) {
        return false;
    }
    let pre = || p.ops.iter().take_while(|o| !matches!(o, Op::Write(_)));
    // VALUES-INNER (updatable_views#822 / #826): or an INNER nest whose inner is a VALUES scan (marks.rs
    // `nest_values`), its row a COPY mark's the recheck hands the re-open (`nest_copy` below)
    // ANTINEST: or an UPDATE's / DELETE's ANTI nest over such an inner (marks.rs `nest_anti_joins`)
    let anti = |o: &Op<'_>| matches!(w.cmd, Cmd::Update | Cmd::Delete) && crate::rerun::epq::marks::nest_anti_joins(o);
    // SEMINEST: or an UPDATE's / DELETE's SEMI nest over such an inner (marks.rs `nest_semi_joins`): its row
    // carries the first matching inner row's ctid, the recheck re-opens the inner over that row alone
    // D3739: or a SEMI nest whose inner nests INNER over other heap scans itself (marks.rs `nest_semi_deep`)
    let semi = |o: &Op<'_>| matches!(w.cmd, Cmd::Update | Cmd::Delete) && (crate::rerun::epq::marks::nest_semi_joins(o) || crate::rerun::epq::marks::nest_semi_deep(o));
    // BY-SOURCE-NEST (gen mg-0144): or, for a MERGE, a LEFT nest whose inner
    // is one heap range scan (`left_range`): PostgreSQL 18.6's NOT MATCHED BY SOURCE plan with the target's own scan the
    // outer; a null-extended row stores the inner's empty row (marks.rs `inner_pos`'s null_empty), so the recheck
    // null-extends again over the target's newest version (EvalPlanQualFetchRowMark's no-row for a relation inside an
    // outer join). An index, bitmap or TID inner keeps the refusal (D1684: the recheck's empty row position there)
    // COPY-SCAN: or an UPDATE's / DELETE's INNER nest over a Function Scan or CTE Scan (marks.rs `nest_copy_scan`), its
    // row a COPY mark's the recheck hands the re-open, as VALUES-INNER's
    let scan = |o: &Op<'_>| matches!(w.cmd, Cmd::Update | Cmd::Delete) && crate::rerun::epq::marks::nest_copy_scan(o);
    // D3688: or an UPDATE's / DELETE's INNER nest whose inner holds one Memoize over such a scan (marks.rs `nest_memo_joins`)
    let memo = |o: &Op<'_>| matches!(w.cmd, Cmd::Update | Cmd::Delete) && crate::rerun::epq::marks::nest_memo_joins(o);
    let nest = |o: &Op<'_>| crate::rerun::epq::marks::nest_joins(o) || crate::rerun::epq::marks::nest_values(o) || scan(o) || anti(o) || semi(o) || memo(o) || w.cmd == Cmd::Merge && left_range(o);
    let simple = |o: &Op<'_>| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_) | Op::SubPlan(_)) || crate::rerun::epq::marks::write_joins(o);
    // FAMNEST: or an INNER nest over the union of a REFERENCE parent's children (`family_inner`)
    let fams: PgVec<'_, (usize, PgVec<'_, (u32, u32)>)> = pre().enumerate().filter_map(|(j, _)| family_inner(gx, k, j, w).map(|f| (j, f))).collect_in(gx.mcx);
    let fam = |j: usize| fams.iter().any(|f| f.0 == j);
    if !pre().enumerate().any(|(j, o)| nest(o) || fam(j)) || !pre().enumerate().all(|(j, o)| simple(o) || nest(o) || fam(j)) {
        return false;
    }
    // the relations the nests' inners scan; writesubplan A2: an inner running a COPY-marked subquery row's SubqueryScan
    // (a view's OFFSET 0 subquery a rule's OLD reads) carries that row, never a mark of the relation below it
    let target = |rti: u32| w.targets.iter().any(|t| t.rti == rti);
    let copied: PgVec<'_, usize> = w.mark_descs.iter().filter(|m| !target(m.rti) && !m.parent && m.mark == RowMarkType::ROW_MARK_COPY && m.ctid_col > 0).filter_map(|m| crate::rerun::epq::marks::nest_copy(gx, k, m)).collect_in(gx.mcx);
    // VALUES-INNER: each VALUES inner is such a COPY row's (a recheck yields that stored row alone); COPY-SCAN: each
    // Function Scan or CTE Scan inner too
    if pre().enumerate().any(|(j, o)| (crate::rerun::epq::marks::nest_values(o) || scan(o)) && gx.pipelines().inner_of(k, j).is_none_or(|ik| !copied.contains(&ik))) {
        return false;
    }
    let mut inners: PgVec<'_, (u32, u32)> = pre().enumerate().filter(|&(j, _)| gx.pipelines().inner_of(k, j).is_none_or(|ik| !copied.contains(&ik))).filter_map(|(_, o)| match o {
        Op::Join(JoinDesc::Nest { inner, .. }) if nest(o) => heap(&inner.source),
        _ => None,
    }).collect_in(gx.mcx);
    // D3739: and the heap scans a `nest_semi_deep` inner's own nests scan
    let deep = pre().filter(|o| crate::rerun::epq::marks::nest_semi_deep(o)).flat_map(|o| match o {
        Op::Join(JoinDesc::Nest { inner, .. }) => Some(inner.ops.iter()),
        _ => None,
    }).flatten().filter_map(|x| match x {
        Op::Join(JoinDesc::Nest { inner: i2, .. }) => heap(&i2.source),
        _ => None,
    });
    inners.extend_in(deep);
    inners.extend_in(fams.into_iter().flat_map(|f| f.1));
    // a probe's build (`write_joins`) is a producer scanning its own marked relation: marks.rs `sites` positions it
    let produced = |rti: u32, relid: u32| (0..gx.pipelines().len()).any(|q| q != k && heap(&gx.pipelines()[q].source) == Some((rti, relid)));
    // a parent's mark carries no row (its child's does, marks.rs `sites` skips it): PG 18 keeps a relation that once had
    // inheritance children a parent of itself alone (expand_inherited_rtentry), so its one child mark is the inner's
    w.mark_descs.iter().filter(|m| !target(m.rti) && !m.parent).all(|m| {
        let copy = m.mark == RowMarkType::ROW_MARK_COPY && m.ctid_col > 0 && crate::rerun::epq::marks::nest_copy(gx, k, m).is_some();
        copy || m.mark == RowMarkType::ROW_MARK_REFERENCE && m.ctid_col > 0 && (inners.contains(&(m.rti, m.relid)) || produced(m.rti, m.relid))
    }) && inners.iter().all(|&(rti, relid)| w.mark_descs.iter().any(|m| m.rti == rti && m.relid == relid && !m.parent && m.mark == RowMarkType::ROW_MARK_REFERENCE))
}

/// BY-SOURCE-NEST: a LEFT nested loop (marks.rs `nest_left_joins`) whose inner is one heap range scan.
pub(crate) fn left_range(o: &Op<'_>) -> bool {
    crate::rerun::epq::marks::nest_left_joins(o) && matches!(o, Op::Join(JoinDesc::Nest { inner, .. }) if matches!(inner.source, Source::Range { am: Am::Heap, .. }))
}

/// FAMNEST (module doc): op `j` of UPDATE / DELETE `w`'s pipeline `k` is an INNER nest whose inner is the emit
/// of a settled union (pipes.rs `union_in_inner`, no run-time pruning), its ops after the Union filters and projections, and
/// every feed one heap scan of a distinct REFERENCE-marked child of the write's family under filters and projections: the
/// children's (rti, relid). Else `None`.
fn family_inner<'g>(gx: &Gx<'g, '_>, k: usize, j: usize, w: &WriteDesc<'_>) -> Option<PgVec<'g, (u32, u32)>> {
    let ps = gx.pipelines();
    if !matches!(w.cmd, Cmd::Update | Cmd::Delete) || !matches!(ps[k].ops.get(j), Some(Op::Join(JoinDesc::Nest { jointype: JoinType::JOIN_INNER, .. }))) {
        return None;
    }
    let ik = ps.inner_of(k, j)?;
    let (e, _, feeds) = crate::pipes::union_in_inner(gx, ik)?;
    let simple = |ops: &[Op<'_>]| ops.iter().all(|o| matches!(o, Op::Filter(_) | Op::Project(_)));
    if e != ik || feeds.is_empty() || !simple(&ps[ik].ops[1..]) || crate::rerun::union_arms::union_of(&ps[ik]).is_none_or(|u| u.1) {
        return None;
    }
    let child = |rti: u32, relid: u32| w.mark_descs.iter().any(|m| m.rti == rti && m.relid == relid && !m.parent && m.prti != m.rti && m.mark == RowMarkType::ROW_MARK_REFERENCE && m.lock == LockKind::None && m.ctid_col > 0);
    let mut out: PgVec<'g, (u32, u32)> = mcx::reserved_in(feeds.len(), gx.mcx);
    for &f in feeds.iter() {
        let q = &ps[f];
        let (last, body) = q.ops.split_last()?;
        let scan = heap(&q.source).filter(|&(rti, relid)| child(rti, relid) && !out.contains(&(rti, relid)))?;
        if !matches!(last, Op::Append(_)) || crate::rerun::union_arms::feed_of(q).is_none() || !q.desc.consumes.is_empty() || !simple(body) {
            return None;
        }
        out.push_in(scan);
    }
    Some(out)
}

/// FAMNEST: whether pipeline `q` feeds the union a `family_inner` nest of write pipeline `k` reads (marks.rs
/// `sites_in` keeps that child's row in a RowOf site of the arm's scan site: the arm scans on while the inner reads).
pub(crate) fn family_feed(gx: &Gx<'_, '_>, k: usize, q: usize) -> bool {
    let ps = gx.pipelines();
    let Some(Op::Write(w)) = ps[k].ops.iter().find(|o| matches!(o, Op::Write(_))) else { return false };
    let Some((e, _, feeds)) = crate::pipes::union_in_inner(gx, q) else { return false };
    let inners = ps.inners();
    feeds.contains(&q) && e >= ps.len() && inners.get(e - ps.len()).is_some_and(|x| x.0 .0 == k && family_inner(gx, k, x.0 .1, w).is_some())
}

/// producerchain B (`UPDATE / DELETE t ... WHERE t.k IN (SELECT s.k FROM s ...)`, REFRESH MATERIALIZED VIEW
/// CONCURRENTLY's `DELETE FROM mv WHERE ctid = ANY (SELECT diff.tid FROM diff ...)`): PostgreSQL 18.6 de-duplicates the
/// semi join's inner (a HashAggregate over s's scan, its `s.ctid` junk a non-key column of one member) as the nested
/// loop's outer, the target's scan inner (write/nest_write.rs `dml_form`, D1933B). Whether `m` is s's REFERENCE mark:
/// pipeline `k` reads the keyed group, and that group's producer is s's own heap scan with only filters, gates and
/// projections (decompose demand/readers.rs `marked_junk` carries the ctid as an `any_value` cell). EvalPlanQual refetches
/// s's row at that ctid (the version the statement joined) and the HashAggregate over that row alone yields the one
/// group the row joined; here marks.rs `sites` positions the mark at the producer's scan site and the recheck reruns
/// the producer with that row substituted, then the group's reader.
/// B2 (orm-0108, tw-core-107: `.. WHERE t.id IN (SELECT s.id FROM s JOIN r ON ..)`): or the producer joins
/// further REFERENCE-marked relations by INNER nested loops over their own heap scans (marks.rs `nest_joins`), `m` the
/// relation of its scan or of one such inner: EvalPlanQual refetches every one of those rows and the HashAggregate over
/// their join yields the one group again. Here the recheck reruns the producer with its scan's row substituted and each
/// such inner scanning again under the statement's snapshot (marks.rs `sites_in`, `deduped_inner`): the inner finds the
/// version the statement joined, the one EvalPlanQual refetches, and the group exists exactly when that join holds (the
/// group's key the substituted row's, the only cell the write's recheck reads).
/// B3 (orm-0110: the same semi join's inner a Hash Join of s and r, unique-ified by Unique over a Sort on
/// the key): or the producer's INNER hash probes build from r's own heap scan (`built`), `m` that scan's relation too; or
/// the group a Sort whose runs pipeline `k`'s first-of-run de-duplicates (`deduped_run`). EvalPlanQual refetches s's and
/// r's rows and the Sort over their join yields the one row again; here marks.rs `sites` positions each mark at its
/// producer's scan site (the build's, the probing producer's) and the recheck reruns both with those rows substituted,
/// then pipeline `k` from the Sort (the recheck node's own first-of-run words: the row passes).
pub(crate) fn deduped_ref(gx: &Gx<'_, '_>, k: usize, m: &RowMark) -> bool {
    let marked = m.mark == RowMarkType::ROW_MARK_REFERENCE && m.ctid_col > 0 && m.lock == LockKind::None;
    if let Some(arms) = deduped_arms(gx, k) {
        let ps = gx.pipelines();
        return marked && (m.parent || m.prti != m.rti && arms.iter().any(|&q| heap(&ps[q].source) == Some((m.rti, m.relid))));
    }
    let Some(q) = deduped_producer(gx, k) else { return false };
    let ps = gx.pipelines();
    let inner = |o: &Op<'_>| matches!(o, Op::Join(JoinDesc::Nest { inner, .. }) if crate::rerun::epq::marks::nest_joins(o) && heap(&inner.source) == Some((m.rti, m.relid)));
    let build = |o: &Op<'_>| built(gx, q, o).is_some_and(|b| heap(&ps[b].source) == Some((m.rti, m.relid)));
    let mine = heap(&ps[q].source) == Some((m.rti, m.relid)) || ps[q].ops.iter().any(|o| inner(o) || build(o));
    m.mark == RowMarkType::ROW_MARK_REFERENCE && !m.parent && m.ctid_col > 0 && m.lock == LockKind::None && mine
}

/// D3704 (`DELETE FROM t WHERE EXISTS (SELECT 1 FROM p ..)`, p an inheritance parent: Nested Loop (HashAggregate over the
/// Append of p's and its children's scans, the target's scan)): the arms of the streamed union whose keyed group pipeline
/// `k` reads, each one heap scan under filters, gates and projections closing into the group, the last its producer.
/// EvalPlanQual refetches the child row the row's tableoid names (the others give none) and the HashAggregate over it
/// yields the one group; here marks.rs `sites` positions each child's mark at its arm's scan site, gated on the family's
/// tableoid column, and the recheck reruns the arms, then pipeline `k` from the group. A parent's mark carries no row.
fn deduped_arms<'g>(gx: &Gx<'g, '_>, k: usize) -> Option<PgVec<'g, usize>> {
    use gravity_decompose::ops::shared::append::AppendDesc;
    let ps = gx.pipelines();
    let Source::Breaker { slot } = ps[k].source else { return None };
    let last = (0..k).rev().find(|&q| ps[q].desc.produces() == Some(slot))?;
    let arm = |q: usize| ps[q].ops.iter().find_map(|o| match o { Op::Append(AppendDesc::Arm { i, n, .. }) => Some((*i, *n)), _ => None });
    let (i, n) = arm(last)?;
    let n_arms = usize::from(n);
    if n_arms < 2 || usize::from(i) + 1 != n_arms || n_arms > last + 1 {
        return None;
    }
    let arms: PgVec<'g, usize> = (last + 1 - n_arms..=last).collect_in(gx.mcx);
    let keyed = gx.graph.groups.iter().any(|g| g.producer == gravity_program::LifeId(last as u16) && g.slot == slot && !g.keys.is_empty() && g.set.is_none() && g.two_pass.is_none());
    let quiet = |q: usize| {
        let p = &ps[q];
        p.desc.produces() == Some(slot) && p.desc.consumes.is_empty() && p.union.is_none() && heap(&p.source).is_some() && p.ops.iter().all(|o| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_) | Op::Append(AppendDesc::Arm { .. })))
    };
    (keyed && arms.iter().enumerate().all(|(x, &q)| quiet(q) && arm(q).is_some_and(|a| usize::from(a.0) == x && a.1 == n))).then_some(arms)
}

/// producerchain B / B2: the producer of the keyed group pipeline `k` reads (its source slot): a heap scan under filters,
/// gates, projections and INNER `nest_joins` nests, grouped by keys alone (no grouping set, one pass). B3: or INNER probes
/// (`built`); or the Sort `deduped_run` names, its producer's last op.
fn deduped_producer(gx: &Gx<'_, '_>, k: usize) -> Option<usize> {
    let ps = gx.pipelines();
    let Source::Breaker { slot } = ps[k].source else { return None };
    let q = (0..k).rev().find(|&q| ps[q].desc.produces() == Some(slot))?;
    let keyed = gx.graph.groups.iter().any(|g| g.producer == gravity_program::LifeId(q as u16) && g.slot == slot && !g.keys.is_empty() && g.set.is_none() && g.two_pass.is_none());
    let run = ps[k].ops.iter().take_while(|o| !matches!(o, Op::Write(_))).any(|o| matches!(o, Op::FirstOfRun(_)));
    let (body, sorted) = match ps[q].ops.split_last() {
        Some((Op::Sort(_), body)) if run && !keyed => (body, true),
        _ => (&ps[q].ops[..], false),
    };
    let simple = body.iter().all(|o| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_)) || crate::rerun::epq::marks::nest_joins(o) || built(gx, q, o).is_some());
    ((keyed || sorted) && simple && heap(&ps[q].source).is_some()).then_some(q)
}

/// B3: the producer of the build an INNER hash probe `o` of pipeline `q` reads, when it is one heap scan under filters,
/// gates and projections (its last op the build), else none. D3737 (Django's delete() over a LEFT join with a filter on
/// the joined relation: Nested Loop (HashAggregate over a Hash Left Join, the target's index scan)): or a LEFT one's,
/// its relation's row refetched or, null-extended, none (marks.rs `sites_in`), so the join null-extends again.
pub(crate) fn built(gx: &Gx<'_, '_>, q: usize, o: &Op<'_>) -> Option<usize> {
    let Op::Join(JoinDesc::Probe { slot, .. }) = o else { return None };
    let b = gx.life_of(q, *slot).map(|l| usize::from(l.0)).filter(|&b| b < gx.pipelines().len())?;
    let pb = &gx.pipelines()[b];
    let body = pb.ops.split_last().filter(|(last, _)| matches!(last, Op::Join(JoinDesc::Build { .. }))).map(|(_, body)| body)?;
    let left = matches!(o, Op::Join(JoinDesc::Probe { jointype: JoinType::JOIN_LEFT, catalog: false, marks: false, .. }));
    let plain = |ops: &[Op<'_>]| ops.iter().all(|o| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_)));
    ((crate::rerun::epq::marks::joins(o) || left) && (heap(&pb.source).is_some() && plain(body) || copy_build(gx, b).is_some())).then_some(b)
}

/// D3826 (1) (Hibernate's `DELETE FROM mm WHERE in_id IN (SELECT s1.id FROM s1 WHERE (SELECT count(*) FROM s2 WHERE
/// s1.id = s2.s1_id) = 0)`: pgrust's planner decorrelates the count into a Hash Left Join of s1 and a Subquery Scan over
/// its grouped s2, gravity_decorrelated, COPY-marked): the range table index of the COPY mark whose stored row enters
/// build pipeline `b` (copy_row.rs `boundary`: the Subquery Scan's first op), with only filters, gates and projections
/// past that entry up to the build, else none. EvalPlanQual hands the Subquery Scan that stored row (none for a
/// null-extended one, copy_row.rs `none`) and never runs the subquery; the recheck reruns `b` from its entry over the
/// stored row alone (marks.rs `copy_pos`; the subquery's producers below the entry never rerun), so the probe finds that
/// row, or none and null-extends again.
pub(crate) fn copy_build(gx: &Gx<'_, '_>, b: usize) -> Option<u32> {
    let ps = gx.pipelines();
    let pb = (b < ps.len()).then(|| &ps[b])?;
    let body = pb.ops.split_last().filter(|(last, _)| matches!(last, Op::Join(JoinDesc::Build { .. }))).map(|(_, body)| body)?;
    gx.graph.record_rows.iter().filter(|r| crate::rerun::epq::marks::copied(gx, r.scanrelid)).find_map(|r| {
        let at = crate::rerun::epq::copy_row::boundary(gx, b, r)?;
        body.iter().skip(at).all(|o| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_))).then_some(r.scanrelid)
    })
}

/// D3826 (1): whether `m` is the COPY mark whose stored row enters a build (`copy_build`) that the producer of the keyed
/// group pipeline `k` reads (`deduped_producer`) probes: the recheck reruns that build over the stored row, then the
/// producer over its scan's refetched row (`deduped_ref`), then the group.
pub(crate) fn deduped_copy_build(gx: &Gx<'_, '_>, k: usize, m: &RowMark) -> bool {
    let Some(q) = deduped_producer(gx, k) else { return false };
    let ps = gx.pipelines();
    m.mark == RowMarkType::ROW_MARK_COPY && ps[q].ops.iter().any(|o| built(gx, q, o).is_some_and(|b| copy_build(gx, b) == Some(m.rti)))
}

/// B3: whether op `o` of pipeline `k` is the first-of-run that de-duplicates the Sort `k` reads, its
/// producer `deduped_producer`'s (write/nest_write.rs `dml_form` takes it before the target's nest).
pub(crate) fn deduped_run(gx: &Gx<'_, '_>, k: usize, o: &Op<'_>) -> bool {
    matches!(o, Op::FirstOfRun(_)) && deduped_producer(gx, k).is_some_and(|q| matches!(gx.pipelines()[q].ops.last(), Some(Op::Sort(_))))
}

/// B2: the producer `q` of the keyed group write pipeline `k` reads and the embedded inner (its `Pipes`
/// index) of one of `q`'s `nest_joins` nests that scans mark `m`'s relation (`deduped_ref`), else none.
pub(crate) fn deduped_inner(gx: &Gx<'_, '_>, k: usize, m: &RowMark) -> Option<(usize, usize)> {
    let q = deduped_producer(gx, k)?;
    let ps = gx.pipelines();
    if !matches!(ps[k].ops.iter().find(|o| matches!(o, Op::Write(_))), Some(Op::Write(w)) if crate::ops::modifytable::nest_write::dml_form(gx, k, w).is_some()) {
        return None;
    }
    let inners = ps.inners();
    let i = inners.iter().position(|x| x.0 .0 == q && heap(&x.1.source) == Some((m.rti, m.relid)) && crate::rerun::epq::marks::nest_joins(&ps[q].ops[x.0 .1]))?;
    Some((q, ps.len() + i))
}
