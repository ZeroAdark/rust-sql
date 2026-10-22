//! W5 multilock Row A (V4R-524), review round 1's S1 (D2033's mechanism on this row's forms): the runner's EvalPlanQual
//! reruns every producer before the stopped pipeline first, each whole, its error the statement's at once (driver
//! `Entry::rerun`, exec runner `rerun_producer`: no lazy build, no held error, no skip after an empty build). PostgreSQL's
//! EvalPlanQual runs a Hash only when its join asks for it (nodeHashjoin.c HJ_BUILD_HASHTABLE): an outer-first join
//! builds at its outer's first test row, never when that row fails its quals, and a build-first join runs its outer
//! only over a non-empty build. So each producer PG may not run with the lock's plan must be quiet: nothing its rerun
//! runs may raise or call a user or VOLATILE function (D449's rule: the effects home's `span_quiet`). A COPY mark's
//! producer reruns from its copy entry over the stored row, and the producers below that entry never rerun (D732-rest).
//! lockrows REFFAM: a producer over a REFERENCE-marked relation's own range scan alone reruns over the row version the
//! statement read, which passed the same expressions then: it need only call no user or VOLATILE function
//! (effects `span_calls_free`).
//! Review round 2: S3 (D2050's mechanism): PG keeps the recheck plan tree for the statement and ExecReScanHashJoin keeps
//! hj_OuterNotEmpty, so from the second recheck on a cost-path outer-first join whose outer had a row builds first and,
//! over an empty build, runs nothing of its outer side: where the build may be empty (it reads a locked relation, or is
//! not quiet) the outer side before it must be quiet (BUILD_FIRST) and nothing below it runs always. S2 (D2048's
//! mechanism): a producer holding an outer join refuses (NULL_SIDE). Serve-back: D2033's / D2050's / D2048's fixes (the
//! recheck keeps PG's build order and a NULL mark empties its site). Owner M8-MEMO-CTE.
use gravity_decompose::effects::Licence;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::modifytable::RowMark;
use gravity_decompose::{Op, Source};
use gravity_program::LockKind;
use mcx::{CollectIn, GrowIn, PgVec};
use types_nodes::jointype::JoinType;
use types_nodes::plannodes::RowMarkType;

use crate::graph::Gx;
use crate::admit::refusal::{Reason, ShapeWhy};

const RERUN_FIRST: ShapeWhy = ShapeWhy("a row lock whose recheck reruns first a producer PG's EvalPlanQual may not run, one that may raise or call a function (D2033)");
const BUILD_FIRST: ShapeWhy = ShapeWhy("a row lock whose later rechecks PG may run build-first over a build that may be empty, skipping an outer side that may raise or call a function (D2050)");
/// A producer's FULL / ANTI / RIGHT join, or its LEFT join beside a COPY mark: marks.rs `sites` empties a REFERENCE mark's
/// site for a NULL ctid (D2048), which a LEFT join's null-extended row alone is known to need.
const NULL_SIDE: ShapeWhy = ShapeWhy("a row lock whose recheck reruns a producer holding an outer join (a null-extended row's NULL mark, D2048)");

/// Refuses lock pipeline `k` (marks `marks`) by NULL_SIDE, RERUN_FIRST or BUILD_FIRST unless every producer its recheck
/// reruns is quiet or one PG runs whenever the lock's plan runs, and every outer side PG's later rechecks may skip is quiet.
/// `nulls`: NULL_SIDE is asked (marks.rs's form). lockrows L1-RERUN: lower.rs's L1 lock over a join passes false, its
/// producer's null-extended row served by D2048's fix (marks.rs `sites` V4R-495; witness lockrows-9-d2048-null-mark).
/// `lazy` (`epq_lazy`, and `first_held`'s): the lazy group of `k`'s epq probe, which the driver's recheck runs at that
/// probe's first row, as PG's outer-first join builds, or first where PG's later recheck builds first (driver
/// `Entry::rerun`, D2033), and the first probe's build it keeps in the statement run's order: never a producer PG may not
/// run. Both forms (marks.rs's, lower.rs's L1) ask it. The recheck's run ahead of PostgreSQL's demand comes back with
/// its licence: a quiet constant fill it reruns before PG's fill at the nest's first inner fetch (D3653, `fill_licence`);
/// `None` when it reruns none.
pub(crate) fn admit_lazy(gx: &Gx<'_, '_>, k: usize, node: i32, marks: &[RowMark], (nulls, init): (bool, &[usize]), lazy: &[usize]) -> Option<Licence> {
    match verdict_held(gx, k, marks, Road::Lock { nulls, init }, lazy) {
        Ok(ahead) => ahead,
        Err(why) => crate::refuse!(node, Reason::Shape { why }),
    }
}

/// The recheck `verdict` answers: a row lock's (`nulls`: NULL_SIDE is asked, marks.rs's form), or (C4, R83) a write's,
/// an UPDATE / DELETE / MERGE over its result relations `targets` (range table indexes), whose marks are every other
/// relation's (`arena`: the lowering's, for the effects home's plan walk), and `init` its InitPlans' on-demand members
/// (rerun/epq/marks.rs `init_members`), which its recheck never reruns: it reads each InitPlan as EvalPlanQualStart does,
/// an evaluated one's value copied and a pending one forced once in the statement's products (D2028).
#[derive(Clone, Copy)]
pub(crate) enum Road<'a> {
    /// D4154 INITMULTI: `init` the InitPlans' members the M4 pair's B (marks.rs's runner recheck) never reruns, as the
    /// write road's; empty for any other lock.
    Lock { nulls: bool, init: &'a [usize] },
    Write { targets: &'a [u32], arena: gravity_program::ArenaRef, init: &'a [usize] },
}

/// `admit_lazy`'s and the write road's (rerun/epq/write_road.rs `admit`) answer: the refusal (NULL_SIDE, RERUN_FIRST or BUILD_FIRST) recheck pipeline `k`
/// takes on `road`, `None` when its recheck runs what PG's EvalPlanQual runs.
pub(crate) fn verdict(gx: &Gx<'_, '_>, k: usize, marks: &[RowMark], road: Road<'_>, lazy: &[usize]) -> Option<ShapeWhy> {
    verdict_held(gx, k, marks, road, lazy).err()
}

/// `verdict`, with the licence of the quiet constant fill the recheck reruns ahead (`fill_licence`) when it passes.
fn verdict_held(gx: &Gx<'_, '_>, k: usize, marks: &[RowMark], road: Road<'_>, lazy: &[usize]) -> Result<Option<Licence>, ShapeWhy> {
    let ps = gx.pipelines();
    let producer = |c: usize, slot: u8| (0..c).rev().find(|&p| ps[p].desc.produces() == Some(slot));
    // D732-rest: where a COPY mark's stored row enters its producer (a VALUES scan emits it), and the producers below
    let (mut from, mut copied, mut never): (PgVec<'_, usize>, PgVec<'_, bool>, PgVec<'_, usize>) = (mcx::pg_vec![in gx.mcx; 0usize; ps.len()], mcx::pg_vec![in gx.mcx; false; ps.len()], PgVec::new_in(gx.mcx));
    for m in marks.iter().filter(|m| m.mark == RowMarkType::ROW_MARK_COPY) {
        if let Some(q) = (0..ps.len()).find(|&q| matches!(ps[q].source, Source::Values { scanrelid, .. } if u32::from(scanrelid) == m.rti)) {
            copied[q] = true;
            continue;
        }
        let rec = gx.graph.record_rows.iter().find(|r| r.scanrelid == m.rti);
        if let Some((q, at)) = rec.and_then(|r| (0..ps.len()).find_map(|q| crate::rerun::epq::copy_row::boundary(gx, q, r).map(|at| (q, at)))) {
            (from[q], copied[q]) = (at, true);
            never.extend_in(crate::rerun::epq::copy_row::upstream(gx, q, at));
        } else if let Some((x, at)) = rec.and_then(|r| crate::rerun::epq::copy_row::inner_entry(gx, r)) {
            // D3592: an embedded nest inner's entry (rerun/epq/marks.rs `producer_copy`): the producers below it never
            // rerun
            never.extend_in(crate::rerun::epq::copy_row::upstream(gx, x, at));
        }
    }
    // D3276 (DRAINED LOCK): a drained CTE's pipelines run after the emit and never at a recheck (rerun/epq/marks.rs `drained`);
    // D3379 LOCKPOST: a post CTE's too (`after_emit`)
    never.extend_in(gx.after_emit());
    // C4 (R83, D2050's write road): a write's recheck reruns its producers over the rows the statement read. Every relation
    // but its result relations is REFERENCE- or COPY-marked (EvalPlanQualFetchRowMark refetches the very version the
    // statement read, or hands its stored row), so a producer that reads no result relation, through what it consumes
    // (`fixed`), yields its rows again through the same expressions, which raised nothing then: run where PostgreSQL's
    // EvalPlanQual does not run it, it adds calls alone, and a VOLATILE call may run more often than PG runs it, never
    // fewer times than an evaluation needs. A result relation enters the join tree once. Where the stopped pipeline's
    // source reads it (`src_reads`), PG's EvalPlanQual reads it first, every producer that reads it is that source's, and
    // the rerun runs the fixed ones beside them (D4127: a VOLATILE call that raises by its own state, not its row, may then
    // raise where PG never reaches its producer; the held-error licence is its fix). Otherwise, when no fixed producer acts (the effects home's plan walk: no
    // VOLATILE call, none the catalog cannot answer, so its rows are the same), nor an op of the stopped pipeline ahead of
    // its first read of a result relation (`ahead`), every other input yields its row again and PG's EvalPlanQual reaches
    // every producer that reads a result relation, which the rerun runs first over the same rows, raising what PG raises
    // there and nothing before it. NULL_SIDE is D2048's
    // (the write's marks give a null-extended row's site no row: rerun/epq/marks.rs `sites`), as lower.rs's L1 lock's.
    // Otherwise the write takes the row lock's answer with its result relations as the locked ones.
    let (nulls, write) = match road {
        Road::Lock { nulls, init } => {
            never.extend_in(init.iter().copied());
            (nulls, None)
        }
        Road::Write { targets, arena, init } => {
            never.extend_in(init.iter().copied());
            (false, Some((targets, arena)))
        }
    };
    if let Some((targets, arena)) = write {
        let target = |q: usize| crate::ops::lockrows::marks::own(&ps[q].source).is_some_and(|r| targets.contains(&r));
        let reads = |x: usize| {
            let mut t = mcx::pg_vec![in gx.mcx; x];
            let mut i = 0;
            while let Some(&q) = t.get(i) {
                if target(q) {
                    return true;
                }
                for &s in ps[q].desc.consumes.iter() {
                    if let Some(p) = producer(q, s).filter(|p| !t.contains(p)) {
                        t.push_in(p);
                    }
                }
                i += 1;
            }
            false
        };
        let acts = |q: usize| {
            let node = gx.graph.stmt.zip(ps[q].node_ops.last()).and_then(|(s, n)| gravity_decompose::lifetime::node_of(s, n.node_id).map(|(n, _)| (s, n)));
            node.is_none_or(|(s, n)| gravity_decompose::effects::of_plan(&gravity_decompose::effects::Cx::new(s, gx.facts, arena), n).acts)
        };
        let fixed_calm = (0..k).filter(|&q| ps[q].desc.produces().is_some() && !never.contains(&q) && !reads(q)).all(|q| !acts(q));
        // the stopped pipeline's ops ahead of its first read of a result relation, its source reading none: those before
        // its first join, SubPlan or set-returning step, each a filter, gate, projection or Append marker (an arm's, a
        // feed's, or a settled union's reader, which hands its feeds' rows on as they come). They run over the fixed row
        // the statement read, through the same expressions: a VOLATILE call among them runs again where PG's runs (D4127:
        // one whose answer follows its own state, not its row, may stop PG before the result relation's producers, which
        // the rerun has run first)
        let src_reads = target(k) || matches!(ps[k].source, Source::Breaker { slot } if producer(k, slot).is_some_and(reads));
        let ahead = {
            let first = ps[k].ops.iter().position(|o| matches!(o, Op::Join(_) | Op::SubPlan(_) | Op::Srf(_))).unwrap_or(ps[k].ops.len());
            ps[k].ops[from[k].min(first)..first].iter().all(|o| match o {
                Op::Filter(_) | Op::Gate(_) | Op::Project(_) => true,
                Op::Append(gravity_decompose::ops::shared::append::AppendDesc::Arm { .. } | gravity_decompose::ops::shared::append::AppendDesc::Feed { .. } | gravity_decompose::ops::shared::append::AppendDesc::Union { .. }) => true,
                _ => false,
            })
        };
        if src_reads || fixed_calm && ahead {
            return Ok(None);
        }
    }
    let live = |q: usize| &ps[q].ops[from[q].min(ps[q].ops.len())..];
    // S2: every other pipeline is a producer (marks.rs D652); a LEFT / FULL / ANTI / RIGHT join in one the recheck reruns.
    // D2048: not a LEFT join when every mark but the locking ones is a REFERENCE mark: a null-extended row's NULL ctid
    // gives its site no row at the recheck whichever pipeline null-extended it (rerun/epq/marks.rs `sites`, null_empty),
    // as EvalPlanQualFetchRowMark returns none, so the rerun join null-extends that row again and its nullable side
    // runs nothing (a COPY mark's NULL row keeps the refusal). D3379 RDRAIN: nor then the RIGHT probe whose spooled drain is
    // the lock's own pipeline (hashjoin drain/spool_admit.rs `free_lock`: its outer the nullable side, a drained build row
    // carrying the outer's NULL ctid, so the rerun probe has no outer row and the rerun drain null-extends it again)
    let refs = marks.iter().all(|m| m.lock != LockKind::None || m.mark == RowMarkType::ROW_MARK_REFERENCE);
    let rdrain = |q: usize| refs && gx.free_lock(k, k).is_some() && gx.spooled_drain(q).is_some_and(|(d, _)| d == k);
    let outer = |q: usize, o: &Op<'_>| matches!(o, Op::Join(JoinDesc::Probe { jointype, .. } | JoinDesc::Merge { jointype, .. } | JoinDesc::Nest { jointype, .. } | JoinDesc::Drain { jointype, .. }) if !matches!(*jointype, JoinType::JOIN_INNER | JoinType::JOIN_SEMI | JoinType::JOIN_RIGHT_SEMI) && !(refs && *jointype == JoinType::JOIN_LEFT) && !(*jointype == JoinType::JOIN_RIGHT && rdrain(q)));
    // D3592: nor a LEFT nest whose inner enters a COPY mark's stored row (rerun/epq/marks.rs `producer_copy`): a
    // null-extended row's NULL whole row is the empty copy (D1685), the re-open's no row, so the rerun nest
    // null-extends it again, as EvalPlanQualFetchRowMark's copy leg hands the Subquery Scan no row
    let copy_left = |q: usize, i: usize| marks.iter().any(|m| m.mark == RowMarkType::ROW_MARK_COPY && crate::rerun::epq::ref_copy::inner(gx, q, i, m).is_some());
    let at = |q: usize| from[q].min(ps[q].ops.len());
    if nulls && (0..ps.len()).any(|q| q != k && !never.contains(&q) && live(q).iter().enumerate().any(|(i, o)| outer(q, o) && !copy_left(q, at(q) + i))) {
        return Err(NULL_SIDE);
    }
    // S3: a build may be empty at a recheck when a pipeline its rows come from (it, and the producers of what those
    // consume) reads a locked relation (its newest version may fail a qual or carry a NULL key) or is not quiet
    let locked: PgVec<'_, u32> = match write {
        Some((targets, _)) => mcx::to_vec_in(targets, gx.mcx),
        None => marks.iter().filter(|m| m.lock != LockKind::None).map(|m| m.rti).collect_in(gx.mcx),
    };
    // lockrows REFFAM: a producer over a REFERENCE-marked relation's own scan alone (no locked relation, nothing consumed)
    // reruns over the row version the statement already read (EvalPlanQualFetchRowMark's), through the same expressions
    // that row passed then: it raises nothing anew, so only a user or VOLATILE call counts (effects `span_calls_free`)
    let refs: PgVec<'_, u32> = marks.iter().filter(|m| m.lock == LockKind::None && m.mark == RowMarkType::ROW_MARK_REFERENCE && !m.parent).map(|m| m.rti).collect_in(gx.mcx);
    let same_row = |x: usize| from[x] == 0 && ps[x].desc.consumes.is_empty() && crate::ops::lockrows::marks::own(&ps[x].source).is_some_and(|r| refs.contains(&r)) && crate::effects::span_calls_free(gx, &ps[x]);
    // lockrows REFFAM: or a producer reading only a REFERENCE family's settled union (its every feed a `same_row` one)
    // a settled union reader's feeds: those after the slot's previous reader (a slot is reused after its life: portals#244's
    // locked family's union and the REFERENCE family's union share slot 0), never every producer of the slot
    let union_feeds = |x: usize| match (&ps[x].source, crate::rerun::union_arms::union_reader(&ps[x])) {
        (Source::Breaker { slot }, true) => crate::rerun::union_join_arms::feeds(gx, x, *slot),
        _ => Vec::new(),
    };
    let union_row = |x: usize| {
        let feeds = union_feeds(x);
        from[x] == 0 && ps[x].desc.consumes.len() == 1 && !feeds.is_empty() && feeds.iter().all(|&p| same_row(p)) && crate::effects::span_calls_free(gx, &ps[x])
    };
    let empty = |b: usize| {
        let mut t = mcx::pg_vec![in gx.mcx; b];
        let mut i = 0;
        while let Some(&q) = t.get(i) {
            // lockrows REFFAM: a REFERENCE family's same rows (`same_row` / `union_row`) fill the build again
            if (crate::ops::lockrows::marks::own(&ps[q].source).is_some_and(|r| locked.contains(&r)) || !crate::effects::span_quiet(gx, &ps[q], from[q], usize::MAX, copied[q])) && !same_row(q) && !union_row(q) {
                return true;
            }
            for &s in ps[q].desc.consumes.iter() {
                if let Some(p) = producer(q, s).filter(|p| !t.contains(p) && !never.contains(p)) {
                    t.push_in(p);
                }
            }
            i += 1;
        }
        false
    };
    // an outer-first probe PG's later rechecks may run build-first (not FILL_OUTER: S2 left INNER / SEMI / RIGHT SEMI)
    let flips = |q: usize, o: &Op<'_>| matches!(o, Op::Join(JoinDesc::Probe { slot, outer_first: true, .. }) if producer(q, *slot).is_some_and(|b| empty(b)));
    // the producers PG runs whenever a pipeline that always runs does, from `k`. Its hash joins from the last probe down:
    // a probe runs while every later one is outer-first and no later one flips (over an empty build a build-first join
    // runs nothing of its outer side); a build-first probe builds first; an outer-first one builds at its outer's first
    // row, which always comes when the pipeline's one test row (a seq scan's, a keyless index scan's: IndexRecheck has no
    // qual to fail; or a COPY entry's stored row) reaches it
    // through pure projections alone. A gate may run nothing; a breaker source's producer is the plan's subtree below the
    // scan, run unless a probe cuts it off.
    // D3269 (a): producer `p` hands the one test row its source yields (a range scan's, a keyless index scan's, a COPY
    // entry's) on through every op to its sink: quiet projections, a sort, an Append arm's marker, and quiet filters
    // where it reads no locked relation (the REFERENCE row the statement read passed them then)
    let yields = |p: usize| {
        let fixed = !crate::ops::lockrows::marks::own(&ps[p].source).is_some_and(|r| locked.contains(&r));
        let one = from[p] > 0 || matches!(&ps[p].source, Source::Range { .. }) || matches!(&ps[p].source, Source::Index { keys, recheck, .. } if keys.is_empty() && recheck.iter().next().is_none());
        one && live(p).iter().all(|o| match o {
            Op::Project(l) => l.iter().all(|n| crate::effects::expr_quiet(gx, n)),
            Op::Filter(n) => fixed && crate::effects::expr_quiet(gx, *n),
            Op::Sort(_) => true,
            o => crate::rerun::union_arms::feed_op(o),
        })
    };
    let mut always = mcx::pg_vec![in gx.mcx; false; ps.len()];
    let mut todo = mcx::pg_vec![in gx.mcx; k];
    while let Some(q) = todo.pop() {
        let ops = live(q);
        if ops.iter().any(|o| matches!(o, Op::Gate(_))) {
            continue;
        }
        let mut runs: PgVec<'_, usize> = PgVec::new_in(gx.mcx);
        // lockrows REFFAM: or a REFERENCE family's settled union whose feeds give the same rows (`same_row`)
        let ref_union = from[q] == 0 && { let f = union_feeds(q); !f.is_empty() && f.iter().all(|&p| same_row(p)) };
        // portals#244 (FOR UPDATE of both aliases): or a locked family's settled union whose every feed is a plain range scan
        // of a locked child (projections and Append markers, no filter): the recheck's one active child (`Marks::gate`)
        // yields its test tuple, as PG's EvalPlanQual scan of that child does, so the union yields exactly that one row
        let plain_feed = |f: usize| from[f] == 0 && ps[f].desc.consumes.is_empty() && matches!(ps[f].source, Source::Range { .. })
            && crate::ops::lockrows::marks::own(&ps[f].source).is_some_and(|r| locked.contains(&r))
            && ps[f].ops.iter().all(|o| matches!(o, Op::Project(_)) || crate::rerun::union_arms::feed_op(o));
        let lock_union = from[q] == 0 && { let f = union_feeds(q); !f.is_empty() && f.iter().all(|&p| plain_feed(p)) };
        let one = from[q] > 0 || ref_union || lock_union || matches!(&ps[q].source, Source::Range { .. }) || matches!(&ps[q].source, Source::Index { keys, recheck, .. } if keys.is_empty() && recheck.iter().next().is_none());
        // lockrows L1-RERUN: a pipeline reading no locked relation reruns over the REFERENCE / COPY row the join read,
        // which passed its filters then: a pure filter passes it again
        let fixed = !crate::ops::lockrows::marks::own(&ps[q].source).is_some_and(|r| locked.contains(&r));
        let reaches = |o: &Op<'_>| matches!(o, Op::Project(l) if l.iter().all(|n| crate::effects::expr_quiet(gx, n))) || fixed && matches!(o, Op::Filter(n) if crate::effects::expr_quiet(gx, *n)) || (ref_union || lock_union) && crate::rerun::union_arms::union_op(o);
        // D3269 (a): or a merge join's sorted outer whose every feed hands its one test row on to its sort (`yields`)
        let sorted = from[q] == 0 && matches!(&ps[q].source, Source::Breaker { slot } if !crate::rerun::union_arms::union_reader(&ps[q]) && {
            let mut feeds = (0..q).filter(|&p| ps[p].desc.produces() == Some(*slot)).peekable();
            feeds.peek().is_some() && feeds.all(yields)
        });
        let mut cut = false;
        for (i, o) in ops.iter().enumerate().rev() {
            // D3269 (a): a merge join runs its inner (the sorted run's producer) once its outer's first row comes
            // (nodeMergejoin.c EXEC_MJ_INITIALIZE_OUTER, then _INNER), as an outer-first probe builds at it
            if let Op::Join(JoinDesc::Merge { slot, .. }) = o {
                if (one || sorted) && ops[..i].iter().all(reaches) {
                    runs.extend_in(producer(q, *slot));
                }
                continue;
            }
            let Op::Join(JoinDesc::Probe { slot, outer_first, .. }) = o else { continue };
            if !outer_first || one && ops[..i].iter().all(reaches) {
                runs.extend_in(producer(q, *slot));
            }
            // lockrows REFFAM: a build-first probe over a build that cannot be empty (`empty`) runs its outer side
            if !outer_first && producer(q, *slot).is_none_or(|b| empty(b)) || flips(q, o) {
                cut = true;
                break;
            }
        }
        // lockrows D2604: every producer of the breaker's slot (an Append's arms into the Sort: PG's EvalPlanQual runs each
        // child, its test tuple or none)
        if let (Source::Breaker { slot }, 0, false) = (&ps[q].source, from[q], cut) {
            match crate::rerun::union_arms::union_reader(&ps[q]) {
                // a settled union's reader: its own feeds (`union_feeds`), not an earlier life's producers of the slot
                true => runs.extend_in(union_feeds(q)),
                false => runs.extend_in((0..q).filter(|&p| ps[p].desc.produces() == Some(*slot))),
            }
        }
        for b in runs {
            if !always[b] {
                always[b] = true;
                todo.push_in(b);
            }
        }
    }
    // D3379 SUBSET: a correlated SubPlan's rerun set member is never rerun whole (exec runner `rerun_producer` steps it
    // past): its SubPlan step reruns the set where PG's EvalPlanQual evaluates the SubPlan (rerun/epq/marks.rs `sub_sets`)
    let sub_member = |x: usize| gx.graph.reruns.iter().any(|r| r.plan_id != 0 && r.pipelines.iter().any(|&m| usize::from(m) == x));
    // D3568 HASHSET: nor a hashed SubPlan's set build, which the recheck steps past (rerun/epq/marks.rs
    // `hashed_skipped`): its probe reads the set as built, or builds it there at its first probe, as PG's EvalPlanQual
    // builds its own
    let rerun = |x: usize| ps[x].desc.produces().is_some() && !never.contains(&x) && !sub_member(x) && !crate::rerun::epq::marks::hashed_skipped(gx, x);
    // portals#244: a union feed (a range scan, quiet projections, its Append marker) is quiet: the marker evaluates nothing
    // (effects `span_quiet` knows no Append op)
    let quiet_feed = |x: usize| from[x] == 0 && ps[x].desc.consumes.is_empty() && matches!(ps[x].source, Source::Range { .. })
        && ps[x].ops.iter().all(|o| matches!(o, Op::Project(l) if l.iter().all(|n| crate::effects::expr_quiet(gx, n))) || crate::rerun::union_arms::feed_op(o));
    // lockrows J182: a pulled plan Material's fill runs nothing itself (rerun/spool/form.rs `spool_pull`): its reader, a nest
    // inner, pulls the child row by row only when the nest reaches it, as PostgreSQL's Material pulls its child
    let pulled = |x: usize| crate::pipes::spool_pull(gx, x).is_ok();
    // D3653: nor nest.spool's quiet child, a plan Material's fill of constants (effects/visible/material.rs `quiet`; a VALUES
    // list, the COPY mark's entry, emits its stored row there): run ahead of PostgreSQL's fill at the nest's first inner
    // fetch, it raises and calls nothing (effects `span_quiet` knows no Spool op), under `fill_licence`'s licence
    let licensed = std::cell::Cell::new(None);
    let quiet_fill = |x: usize| from[x] == 0 && fill_licence(gx, x).inspect(|l| licensed.set(Some(*l))).is_some();
    if (0..k).any(|x| rerun(x) && !always[x] && !lazy.contains(&x) && !same_row(x) && !union_row(x) && !quiet_feed(x) && !pulled(x) && !quiet_fill(x) && !crate::effects::span_quiet(gx, &ps[x], from[x], usize::MAX, copied[x])) {
        return Err(RERUN_FIRST);
    }
    // S3: in every pipeline the recheck runs, the source and the ops before a probe that flips. D2674: not the lock
    // pipeline's topmost probe over a build the recheck reruns (rerun/epq/marks.rs `epq_probe`): its call records PG's
    // hj_OuterNotEmpty and the driver skips a later recheck's call over its empty build, as PG's build-first join returns
    let exact = |q: usize, i: usize| {
        let b = match &ps[q].ops[i] {
            Op::Join(JoinDesc::Probe { slot, .. }) => producer(q, *slot),
            _ => None,
        };
        q == k && crate::rerun::epq::marks::epq_probe(gx, k) == Some(i) && b.is_some_and(|b| rerun(b))
    };
    let skipped = |q: usize| (from[q]..ps[q].ops.len()).any(|i| flips(q, &ps[q].ops[i]) && !exact(q, i) && !crate::effects::span_quiet(gx, &ps[q], from[q], i, copied[q]));
    if (0..=k).any(|q| (q == k || q < k && rerun(q)) && skipped(q)) {
        return Err(BUILD_FIRST);
    }
    Ok(licensed.get())
}

/// D3653: the licence of a recheck's re-run of nest.spool's quiet child, a plan Material's fill of constants
/// (effects/visible/material.rs `quiet`; a VALUES list, the COPY mark's entry), ahead of PostgreSQL's fill at the nest's
/// first inner fetch: it acts, raises and reads nothing, the same rows at every run (`effects::unobservable`).
/// `None`: another fill.
fn fill_licence(gx: &Gx<'_, '_>, x: usize) -> Option<Licence> {
    crate::effects::visible::material::quiet(gx, x).then(|| gravity_decompose::effects::unobservable(gravity_decompose::effects::Effects::NONE)).flatten()
}

/// D2033 step 2: the build PG may not run of lock pipeline `k`'s FIRST probe below its epq probe (rerun/epq/marks.rs
/// `epq_probe`): that probe's lazy group (D824), or its op 14 build whose error is held for the probe's first row
/// (hashjoin build_class.rs `defers`) with no producer fed into it. Empty for any other shape.
/// The probe is an outer-first INNER / SEMI hash probe. Below it run only quiet projections over `k`'s own scan of a marked
/// relation, a plain heap range scan, so every recheck's outer side is exactly the test row and the probe builds
/// whenever its join is asked for a row. PG's build order at that join (its own hj_OuterNotEmpty) shows nothing, and no
/// word records it. The probes above it are outer-first and the topmost is the epq probe, whose build is quiet or in its
/// own lazy group (`epq`), so the order of the two builds shows nothing either. The epq probe's order (D2050 / D2674)
/// decides whether the call runs at all. The recheck keeps the build in the statement run's order (`recheck_hold`):
/// lazy, it waits for the probe's first row; held, its error waits for it.
pub(crate) fn first_held<'g>(px: &crate::frame::px::Px<'g, '_>, k: usize, marks: &[RowMark], epq: &[usize]) -> PgVec<'g, usize> {
    let gx = px.gx;
    let mcx = px.mcx();
    let ps = gx.pipelines();
    let p = &ps[k];
    let Some(top) = crate::rerun::epq::marks::epq_probe(gx, k) else { return PgVec::new_in(mcx) };
    let probes: PgVec<'_, usize> = p.ops.iter().enumerate().filter(|(_, o)| matches!(o, Op::Join(_))).map(|(j, _)| j).collect_in(mcx);
    let Some(&j) = probes.first().filter(|&&j| j != top) else { return PgVec::new_in(mcx) };
    let outer_first = |i: usize| matches!(&p.ops[i], Op::Join(JoinDesc::Probe { outer_first: true, jointype: JoinType::JOIN_INNER | JoinType::JOIN_SEMI, catalog: false, .. }));
    let marked = crate::ops::lockrows::marks::own(&p.source).is_some_and(|r| marks.iter().any(|m| m.rti == r));
    let below = p.ops[..j].iter().all(|o| matches!(o, Op::Project(l) if l.iter().all(|n| crate::effects::expr_quiet(gx, n))));
    if !probes.iter().all(|&i| outer_first(i)) || !matches!(p.source, Source::Range { .. }) || !marked || !below {
        return PgVec::new_in(mcx);
    }
    let build = |i: usize| match &p.ops[i] {
        Op::Join(JoinDesc::Probe { slot, .. }) => gx.life_of(k, *slot).map(|l| l.0 as usize),
        _ => None,
    };
    let (Some(t), Some(b)) = (build(top), build(j)) else { return PgVec::new_in(mcx) };
    if !epq.contains(&t) && !crate::effects::span_quiet(gx, &ps[t], 0, usize::MAX, false) {
        return PgVec::new_in(mcx);
    }
    let lazy: PgVec<'_, usize> = px.lazy.iter().filter(|&&(_, q)| q == k).map(|&(x, _)| x).collect_in(mcx);
    if lazy.contains(&b) {
        let mut group = mcx::pg_vec![in mcx; b];
        let mut i = 0;
        while let Some(&m) = group.get(i) {
            for &s in ps[m].desc.consumes.iter() {
                if let Some(q) = (0..m).rev().find(|&q| ps[q].desc.produces() == Some(s)).filter(|q| lazy.contains(q) && !group.contains(q)) {
                    group.push_in(q);
                }
            }
            i += 1;
        }
        return group;
    }
    let held = crate::rerun::epq::marks::held_build(gx, b, k, j);
    match held && ps[b].desc.consumes.is_empty() {
        true => mcx::pg_vec![in mcx; b],
        false => PgVec::new_in(mcx),
    }
}

/// D2033 step 3: the op 14 build (hashjoin build_class.rs `defers`, no producer fed into it) of lock pipeline `k`'s epq
/// probe (rerun/epq/marks.rs `epq_probe`) when that probe is the pipeline's only join and its build is not lazy (`lazy`
/// holds the epq probe's lazy group, which the driver orders itself). The probe is an outer-first INNER / SEMI hash
/// probe over `k`'s own scan of a marked relation, a plain heap range scan, under quiet filters and projections. While a
/// statement's rechecks have not set that join's hj_OuterNotEmpty, PG's recheck fetches the test row first and builds only
/// if it passes the filters; once one set it, PG builds first whatever the outer gives (D2050 / D2674). The recheck holds
/// the build's error for the probe's first row until then and raises it at once after (`recheck_hold_epq`).
pub(crate) fn epq_held(px: &crate::frame::px::Px<'_, '_>, k: usize, marks: &[RowMark], lazy: &[usize]) -> Option<usize> {
    let gx = px.gx;
    let ps = gx.pipelines();
    let p = &ps[k];
    let j = crate::rerun::epq::marks::epq_probe(gx, k)?;
    let mut joins = p.ops.iter().enumerate().filter(|(_, o)| matches!(o, Op::Join(_)));
    let (Some((i, Op::Join(JoinDesc::Probe { slot, outer_first: true, jointype: JoinType::JOIN_INNER | JoinType::JOIN_SEMI, catalog: false, .. }))), None) = (joins.next(), joins.next()) else { return None };
    let marked = crate::ops::lockrows::marks::own(&p.source).is_some_and(|r| marks.iter().any(|m| m.rti == r));
    let quiet = |o: &Op<'_>| matches!(o, Op::Project(l) if l.iter().all(|n| crate::effects::expr_quiet(gx, n))) || matches!(o, Op::Filter(n) if crate::effects::expr_quiet(gx, *n));
    if i != j || !matches!(p.source, Source::Range { .. }) || !marked || !p.ops[..j].iter().all(quiet) {
        return None;
    }
    let b = gx.life_of(k, *slot).map(|l| l.0 as usize)?;
    (!lazy.contains(&b) && ps[b].desc.consumes.is_empty() && crate::rerun::epq::marks::held_build(gx, b, k, j)).then_some(b)
}

/// D2033: the lazy group (D824, `Px::lazy`) of graph pipeline `k`'s epq probe (rerun/epq/marks.rs `epq_probe`, the topmost
/// outer-first INNER / SEMI probe, whose hj_OuterNotEmpty D2674 records): its build and the producers fed into it. Empty when
/// that probe's build is not lazy. A lazy build at a lower probe is not in it: PG decides its order by that join's own flag,
/// which no word records, so RERUN_FIRST still asks it, but for the first probe's of `first_held` (D2033 step 2).
pub(crate) fn epq_lazy<'g>(px: &crate::frame::px::Px<'g, '_>, k: usize) -> PgVec<'g, usize> {
    let gx = px.gx;
    let mcx = px.mcx();
    let Some(j) = crate::rerun::epq::marks::epq_probe(gx, k) else { return PgVec::new_in(mcx) };
    let Op::Join(JoinDesc::Probe { slot, .. }) = &gx.pipelines()[k].ops[j] else { return PgVec::new_in(mcx) };
    let Some(b) = gx.life_of(k, *slot).map(|l| l.0 as usize) else { return PgVec::new_in(mcx) };
    let lazy: PgVec<'_, usize> = px.lazy.iter().filter(|&&(_, q)| q == k).map(|&(x, _)| x).collect_in(mcx);
    if !lazy.contains(&b) {
        return PgVec::new_in(mcx);
    }
    let ps = gx.pipelines();
    let mut group = mcx::pg_vec![in mcx; b];
    let mut i = 0;
    while let Some(&m) = group.get(i) {
        for &s in ps[m].desc.consumes.iter() {
            if let Some(p) = (0..m).rev().find(|&p| ps[p].desc.produces() == Some(s)).filter(|p| lazy.contains(p) && !group.contains(p)) {
                group.push_in(p);
            }
        }
        i += 1;
    }
    group
}
